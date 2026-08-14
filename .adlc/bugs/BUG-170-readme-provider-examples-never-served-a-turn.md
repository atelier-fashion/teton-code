---
id: BUG-170
title: "The README's provider examples were never a turn anything could serve — a base-URL endpoint and an endpoint-less anthropic registration"
status: resolved
severity: high
created: 2026-08-14
updated: 2026-08-14
component: "docs/readme"
domain: "providers"
stack: ["rust", "docs", "daemon"]
tags: ["provider-setup", "endpoint", "readme", "req-577", "bug-160", "bug-165", "lesson-512", "documentation-drift"]
---

## Description

The README's "Hooking up an external model" block has shipped two
`teton provider add` commands since BUG-160's fix (PR #72). Neither of them
produces a working provider, and the two fail in different ways:

```bash
teton provider add opus --kind anthropic --model claude-opus-5
teton provider add kimi --kind openai-compatible \
  --endpoint https://api.moonshot.ai/v1 --model kimi-k3
```

1. **The `anthropic` line is refused** — after the user's API key is already in
   the OS keychain. `Config::validate` requires an endpoint for every
   `ProviderKind::is_remote()` kind, and `Anthropic` is one
   (`teton-core/src/entities.rs:36`). `provider add` reads the secret *before*
   it calls `config/set` (`teton/src/main.rs:1341`), and `config/set` validates
   the candidate config before persisting (`tetond/src/runtime.rs:2188`). So
   the sequence is: prompt for the key, store the key, register, refuse. The
   user is left with a credential in their keychain, no provider, and the
   message `provider 'opus' is a remote provider and must set an 'endpoint'` —
   which contradicts the command they were just told to run.

2. **The `kimi` line registers and then 404s on its first turn.** Teton's
   `--endpoint` is the **absolute request URL**, POSTed verbatim:
   `OpenAiCompatAdapter::build_request` sets `url: self.config.endpoint.clone()`
   (`teton-providers/src/openai_compat.rs:139`) and nothing anywhere joins a
   path onto it. `https://api.moonshot.ai/v1` is Moonshot's *`base_url`* — the
   value an OpenAI SDK appends `/chat/completions` to. Registration succeeds,
   `teton provider list` looks healthy, `teton doctor` reports nothing wrong,
   and the failure arrives one step away from its cause on the first routed
   turn.

REQ-577 then copied both errors into new surfaces at greater volume: the typed
recipe catalog gave five vendors their base URLs and gave Anthropic
`endpoint: None`, and the bundled guide, the `providers` doc topic and the
README all repeated it. The blast radius grew from two README lines to the
resident system prompt of every session.

## Reproduction Steps

1. `teton provider add opus --kind anthropic --model claude-opus-5`, entering
   any key at the echo-off prompt.
2. Observe the refusal, and that `security find-generic-password -s teton -a
   opus` now returns an entry.
3. `teton provider add kimi --kind openai-compatible --endpoint
   https://api.moonshot.ai/v1 --model kimi-k3`; `teton policy set-tier think
   kimi`; then ask any question that routes to `think`.

## Expected Behavior

A command printed in Teton's own README registers a provider that serves a
turn. `--endpoint` values are the URLs the vendor's own `curl` example posts
to, and every remote kind — `anthropic` included — carries one.

## Actual Behavior

1. `provider 'opus' is a remote provider and must set an 'endpoint'`, raised
   after the credential is stored.
2. A registered `kimi` provider whose every call 404s.

## Environment

- Platform: macOS (Darwin 25.6.0), Apple Silicon
- Version: present since 0.1.13 (PR #72); found on `main` at 0.1.15 (`4569311`)
  and in the REQ-577 branch that inherited it

## Root Cause

**The examples were verified against the vendors and never against Teton's own
`--endpoint` contract.** Both halves of that sentence matter.

The vendor facts were right. `https://api.moonshot.ai/v1` really is Moonshot's
documented `base_url`, and Anthropic's adapter really does know the Messages
API protocol. What nobody checked was the *seam*: what Teton does with the
string a user passes to `--endpoint`. It POSTs it, unchanged. A `base_url` is
by definition not a request URL — it is a prefix a client library completes —
so a value that is correct on the vendor's page is wrong in this flag, and
correct-looking.

The Anthropic half has the same shape one layer up. "The `anthropic` kind knows
its own address" is a reasonable belief about an adapter that speaks one
vendor's protocol, and it is false here: `build_provider` constructs
`AnthropicAdapter::new(id, endpoint)` from the config entry exactly like every
other kind (`tetond/src/runtime.rs:6701`). The belief was never confronted with
`Config::validate`, which is the code that decides.

Why CI was green throughout: REQ-577 built four gates over these values — a
verbatim golden, a kind/endpoint invariant, and bidirectional prose gates for
the guide, the README and the doc topic — and **every one of them compared the
catalog to a copy of itself**. The invariant test even restated the rule by
hand (`matches!(kind, OpenaiCompatible)`) rather than reading
`ProviderKind::is_remote()`, so it agreed with the catalog's mistake. Six
copies of a wrong fact, mechanically checked to be identical. That is the
LESSON-512 failure in its exact form: a named example is a test vector, and
these vectors were never *run* — not through validation, and not through the
request builder.

## Resolution

Fixed on the REQ-577 branch (PR #144), phase 5.

- **The catalog ships full request URLs.** Every endpoint re-verified against
  the vendor's current `curl` example on 2026-08-14 (round 2), including the
  three vendors whose documentation *hosts* had moved since round 1:
  `https://api.openai.com/v1/chat/completions`,
  `https://api.moonshot.ai/v1/chat/completions`,
  `https://api.deepseek.com/chat/completions` (the one path with no `/v1`; the
  `/v1` form is no longer documented at all, though it still routes),
  `http://localhost:11434/v1/chat/completions`,
  `https://api.x.ai/v1/chat/completions`, and — new — Anthropic's
  `https://api.anthropic.com/v1/messages`.
- **The invariant reads the daemon's own predicate.**
  `an_endpoint_is_present_exactly_when_the_kind_needs_one` keys off
  `ProviderKind::is_remote()` instead of a hand-written match, so it can no
  longer agree with a mistake by sharing it.
- **The regression guard is a seam test**, not a seventh copy:
  `provider_recipes::tests::every_recipe_is_a_registration_the_daemon_accepts_and_an_adapter_can_post`
  assembles each recipe into the `ModelProvider` that `provider add` builds,
  requires `Config::validate()` to accept it, and requires the endpoint path to
  be the one that recipe's adapter actually requests (`/chat/completions` for
  the OpenAI shape, `/v1/messages` for Anthropic). Mutation-checked against the
  exact round-1 data: restoring `endpoint: None` for Anthropic fails with the
  validation error and the note that the key is stored first; restoring
  `https://api.deepseek.com` fails with the verbatim-POST contract stated in
  the message.
- **The prose gates now check facts paired**, not as sets. The guide's recipe
  line is split per vendor so an endpoint's example model must be in its own
  segment, and the README block is read one `provider add` at a time with
  `(kind, endpoint, model)` required to be a combination some single recipe
  ships — the check that would have caught this directly, rather than by way of
  the stale model id that happened to accompany it.
- All four prose surfaces updated: the README block, the bundled guide's
  resident recipe line, the `providers` doc topic, and each of their
  "no `--endpoint` for anthropic" phrasings, which were teaching the defect.

## Deployment

n/a — OSS repo, no staging/production pipeline; the fix ships with REQ-577 in
the next tagged release (post-0.1.15).

## Files Changed

- `crates/tetond/src/provider_recipes.rs` — full request URLs, `is_remote()`
  invariant, the seam test, re-verification comments
- `crates/tetond/src/harness/self_config.md` — resident recipe line
- `crates/tetond/src/harness/docs/providers.md` — recipes and the base-URL
  warning
- `README.md` — the two commands in the walkthrough
- `crates/tetond/tests/web_setup_contracts.rs` — per-vendor and per-command
  pairing
- `.adlc/bugs/BUG-170-readme-provider-examples-never-served-a-turn.md` — this
  file
