---
id: BUG-202
title: "A provider credential over a cleartext endpoint is refused for [web] but only warned about for providers"
status: resolved
severity: medium
created: 2026-08-28
updated: 2026-08-28
component: "daemon/config"
domain: "privacy"
stack: ["rust", "daemon"]
concerns: ["security", "privacy"]
tags: ["cleartext", "auth-ref", "config-validate", "http", "asymmetry", "credential-exposure", "req-563"]
---

## Description

`Config::validate()` refuses a `[web]` configuration that pairs a
`search_key_ref` with a cleartext `http://` endpoint — `ConfigError::WebSearchKeyOverCleartextEndpoint`,
a hard, fail-closed validation error that gates daemon startup.

The identical hazard for a **provider** is not refused. The provider validation
loop checks `is_recognized_auth_ref` and that a remote provider has an endpoint,
and nothing else. A config pairing `endpoint = "http://api.example.com"` with
`auth_ref = "keychain://teton/x"` loads cleanly, and every turn then puts an
`x-api-key` or `Bearer` header on the open wire.

The predicate needed to close this already exists, is already `pub`, and is
already used by the web rule: `teton_core::is_cleartext_to_a_remote_host`.

## Why the existing mitigation does not cover it

There *is* a cleartext check on the provider side — but only inside the guided
`provider add` flow, and only as a **warning**. That path is one of three ways a
provider record comes into being:

1. `provider add` (guided) — warns.
2. A hand-edited `config.toml` — no check at all.
3. A migrated or vendored config from an older schema — no check at all.

So the protection is attached to a UI flow rather than to the config document,
which is exactly the placement `Config::validate()` exists to correct. This is
the same shape as REQ-563's reasoning for the `[web]` rule, applied
inconsistently to its sibling.

## Reproduction Steps

1. Hand-edit `config.toml` to declare a remote provider with
   `endpoint = "http://api.example.com"` and any valid `auth_ref`
   (`keychain://…` or `env:…`).
2. Start the daemon. It starts — no refusal, no warning.
3. Run any turn routed to that provider.
4. Observe the credential header transmitted over cleartext HTTP.

For the contrast, perform the same edit against `[web]`'s `endpoint` +
`search_key_ref`: the daemon refuses to start.

## Expected Behavior

`Config::validate()` returns a `ConfigError` — structurally parallel to
`WebSearchKeyOverCleartextEndpoint` — when a provider record pairs an `auth_ref`
with an endpoint for which `is_cleartext_to_a_remote_host` is true. The daemon
does not start, and the message names the offending provider id and the remedy.

`http://localhost`, `http://127.0.0.1`, and other loopback / non-remote hosts
must remain permitted — **verified**: `is_cleartext_to_a_remote_host` returns
`split_http_scheme(url).is_some_and(|(cleartext, _)| cleartext) && !url_host(url).is_some_and(is_loopback_host)`,
where `is_loopback_host` covers `localhost` and all of `127.0.0.0/8`, so the predicate already encodes that
distinction, and local OpenAI-compatible servers (Ollama, llama.cpp) depend on
it. This is the reason the predicate is named "to a remote host" rather than
"is http".

## Actual Behavior

The config loads. The credential is transmitted in cleartext on every turn. The
only signal is a warning the user sees exclusively if they happened to create
the provider through the guided flow.

## Environment

- Platform: all
- Version: present as of `main` @ fb7446f (v0.1.26)

## Root Cause

**Partly confirmed by the predicate's own doc comment.** `is_cleartext_to_a_remote_host`
says it was made **public since REQ-578** specifically "so `teton provider add` can warn
before a key is typed into an `http://` registration using this rule rather than a copy
of it." So the provider case was consciously considered — and consciously implemented as
a *warning in the setup flow* rather than a refusal in the validator. This is a placement
decision to revisit, not an oversight to patch.

The rest, to be confirmed during the fix: the `[web]` cleartext rule was added
with REQ-563 as part of that REQ's own scope, and the provider-side equivalent
was implemented in the guided-setup UI (REQ-579) rather than in the config
document's validator. Neither REQ owned "credentials over cleartext" as a
cross-cutting rule, so the two halves landed at different layers with different
strengths.

This is the config validity-vs-usability boundary described in
`.adlc/context/conventions.md`: a raw credential bound to a cleartext endpoint
is a **structural** error — the record is coherent but unsafe by construction —
which puts it on the fail-closed `Config::validate()` side, alongside the `[web]`
rule, not in a non-fatal usability pass.

## Investigation — the decision, and why it was needed

A first candidate was written, tested, and **reverted** because it would have
broken a legitimate topology. Option **(D)** was then chosen and shipped. The
superseded candidate is kept at `.adlc/bugs/BUG-202-candidate-fix.patch` for the
record.

### What the candidate did

Mirrored the `[web]` rule exactly — `ConfigError::AuthRefOverCleartextEndpoint`
returned from `Config::validate()` when a provider pairs an `auth_ref` with an
endpoint for which `is_cleartext_to_a_remote_host` is true.

**It worked, and the mutation test confirmed it worked**: deleting the guard
turned 3 new tests red while
`a_search_key_beside_a_cleartext_remote_endpoint_is_refused` — the `[web]`
sibling — stayed **green**. That green is the proof the two are separate
enforcement points and the web test never covered this path.

### Why it was reverted

`crates/tetond/src/runtime.rs`'s
`a_cleartext_remote_endpoint_warns_and_a_loopback_one_does_not` failed. It is
**not a stale test** — it deliberately asserts the guided flow *warns* for a
cleartext remote endpoint and names the host the key would travel to.

Reading it surfaced what the bug report missed: **`is_cleartext_to_a_remote_host`
exempts only loopback.** So the refusal also rejects
`http://10.0.1.50:8000/v1/chat/completions` — a self-hosted vLLM or Ollama box
on a LAN with an auth token, over a trusted network. That is a legitimate and
likely topology for this project's stated audience (cost-conscious developers
running their own models), and the refusal would break it at daemon startup.

This reframes the asymmetry. It may be **deliberate and correct**: every `[web]`
search backend is public SaaS, where cleartext is almost certainly a mistake.
Provider endpoints are not — they legitimately include private hosts.

### The undisputed part

Whatever is decided about refuse-vs-warn, one thing is a defect under every
reading: the cleartext warning lives in `provider_setup_warnings`, reachable
**only from the guided `provider add` flow**. A hand-edited config and a
migrated config get **no signal at all**. That gap is real and worth closing
independently.

### Options (needs a decision)

- **(A) Refuse only for publicly-routable hosts.** Closes the real hazard
  (credential crossing the public internet in the clear), preserves LAN setups.
  Blocked on an unreliable sub-problem: classifying DNS names —
  `models.corp.example.com` is somebody's internal host and cannot be told from
  a public one.
- **(B) Refuse, loopback-only exemption.** Ships the candidate patch as-is.
  Maximum consistency with `[web]`; breaks LAN-over-http users.
- **(C) Keep the warning; close only the gap.** Surface the existing warning for
  hand-edited and migrated configs via the non-fatal usability pass, per the
  project's own validity-vs-usability convention. Non-breaking; leaves a
  credential reaching a public host in cleartext with only a warning.
- **(D — CHOSEN, shipped) Refuse by default with an explicit per-provider opt-out**
  (`allow_cleartext = true`). Secure by default for every config path including
  hand-edited ones, no DNS heuristic, and every legitimate setup stays possible
  behind one greppable, auditable line. Costs a user-facing config schema field,
  which is why it was not added unilaterally.

### Also verified during investigation (no action needed)

- **The sweep closes at two enforcement points.** `[web]` (`search_key_ref` +
  `search_endpoint`, enforced) and `[[providers]]` (`auth_ref` + `endpoint`, not
  enforced). `McpTransport::Http` carries an endpoint but **no credential**, so
  it is not a third site.
- **`compose_endpoint` never rewrites a scheme** — it only appends a canonical
  path — so the stored endpoint's scheme is what reaches the wire, and checking
  it at validation time is sound.
- **BUG-171's fix already covers the new refusal path.** A
  `PROVIDER_SETUP_INVALID` error is not `METHOD_NOT_FOUND`, so it lands in
  `report_registration_outcome`'s catch-all `Err` arm
  (`crates/teton/src/main.rs`), which runs `PriorKey::undo` and reports the
  keychain cleanup. A refused registration would not strand the typed key.
- **The guided flow already routes validator errors to the user** —
  `runtime.rs` maps `validate()` failures to `PROVIDER_SETUP_INVALID` carrying
  the validator's own sentence, so any refusal chosen here surfaces correctly
  without further wiring.

## Resolution

Option **(D)**. `Config::validate()` now refuses a provider that pairs an
`auth_ref` with a cleartext `http://` endpoint on a non-loopback host — the
`[web]` rule's provider half, using the same `is_cleartext_to_a_remote_host`
predicate rather than a second spelling of it (LESSON-494).

Unlike the `[web]` rule, this one is **escapable**. `ModelProvider` gains
`allow_cleartext: bool` (default `false`, `skip_serializing_if` so a config that
never opted in carries no line). The default is secure for every config path —
including the hand-edited and migrated ones that had no check at all — while a
self-hosted model server on a trusted LAN stays possible behind one explicit,
greppable, auditable line. No DNS heuristic is involved, because none is
reliable: nothing distinguishes `models.corp.example.com` from
`models.example.com`.

The refusal names the provider, the host the credential would travel to, the
`https://`/loopback remedies, **and its own escape hatch** — a refusal that does
not name its way out is a dead end.

Two behaviours worth stating because they were deliberate:

- **The refusal lands at preview**, before a key is typed. That is the property
  worth keeping from the warning it replaces.
- **The opt-out silences the refusal, never the warning.** Somebody who told the
  daemon they trust their LAN is still told what travels where.

`apply_update` preserves `allow_cleartext` across a re-registration rather than
defaulting it, exactly as it preserves the capability profile (BUG-155).
Without that, an unrelated `--model` change would silently clear the opt-out and
refuse a config that had been working.

### Verification

- `cargo test --workspace --no-fail-fast`: **3,997 passed, 0 failed, 1 ignored**
  across 69 targets; output grepped for `FAILED` (0), per conventions.md.
- `cargo clippy --workspace --all-targets` clean; `cargo fmt --check` clean.
- **Mutation 1 (run):** disabling the guard turns **4** tests red — and
  `a_search_key_beside_a_cleartext_remote_endpoint_is_refused`, the `[web]`
  sibling, stays **green**. That green is the evidence these are two enforcement
  points of one invariant and the web test never covered this path.
- **Mutation 2 (run):** replacing the `apply_update` preservation lookup with
  `false` turns the re-registration test red — BUG-155's failure mode
  reproduced on the new field.
- Falsification is built into the opt-out test: every endpoint it permits with
  the flag on is asserted **refused** with the flag off, so it tests the flag
  rather than the endpoints.

## Deployment

- Merged to `main` as `0b8c1c7` via https://github.com/atelier-fashion/teton-code/pull/226 on 2026-08-28.
- No service deploy applies: Teton Code is a CLI + daemon that ships via tagged
  release, not a promoted revision. **Not yet in a tagged release** — the newest
  tag is v0.1.26 (2026-08-27), so this fix reaches users with the next one.

## Lessons

- `.adlc/knowledge/lessons/LESSON-578-a-rule-on-a-ui-flow-guards-one-door.md` —
  a rule attached to a UI flow guards one of the doors the record can come in
  through; and before mirroring a sibling rule, check whether the sibling'''s
  domain assumptions hold.

## Follow-up (not in this fix)

`teton provider add` has no `--allow-cleartext` flag, so registering a LAN
provider through the guided flow requires hand-editing `config.toml` once. The
preview refusal names the field, so the path is discoverable, but a flag would
close it. Filed as a note rather than folded in — it is a protocol and CLI
surface change, not part of the security fix.

## Files Changed

- `crates/teton-core/src/entities.rs` — `ModelProvider::allow_cleartext` and its `is_false` skip predicate.
- `crates/teton-core/src/config.rs` — `ConfigError::AuthRefOverCleartextEndpoint` (carrying provider id and host); the guard in `validate`'s provider loop; five tests.
- `crates/tetond/src/runtime.rs` — preserve `allow_cleartext` across re-registration in `apply_update`; rewrite the cleartext preview test for refusal; add the seeded-opt-out test.
- `crates/tetond/src/provider_recipes.rs` — fixture field.

## Fix Notes

- The refusal message must be composed at the surface that renders it and must
  name the provider id and the remedy — not a bare "invalid config"
  (conventions.md, LESSON-557).
- The regression test must be a **mutation test**: deleting the new refusal makes
  it fail. A test that merely asserts the new error exists would have passed
  before the fix too.
- Cover the loopback allowance explicitly (`http://localhost:11434`), or the fix
  will break every local Ollama user — that case is the reason this cannot be a
  blanket `http://` ban.
- Check whether `provider add`'s warning path and the new validator can share one
  predicate **and** one sentence, so the two surfaces cannot come to disagree
  (the REQ-588 BR-2 pattern).
