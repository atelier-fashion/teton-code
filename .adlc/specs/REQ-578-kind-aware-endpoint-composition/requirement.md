---
id: REQ-578
title: "Kind-aware endpoint composition at the provider registration seam"
status: complete
deployable: true
created: 2026-08-15
updated: 2026-08-15
component: "daemon/config"
domain: "providers"
stack: ["rust", "daemon", "cli", "llm-providers"]
concerns: ["developer-experience", "reliability", "extensibility"]
tags: ["endpoint-composition", "provider-add", "provider-recipes", "base-url", "endpoint-semantics", "config-validation", "onboarding"]
---

## Description

Make the natural thing users type correct. Teton's `--endpoint` is the
absolute request URL the adapter POSTs verbatim — a deliberate, freshly
test-pinned contract (REQ-577, LESSON-523) — but every vendor documents a
*base* URL, every OpenAI-compatible SDK takes a *base* URL, and the product's
own README shipped base URLs for months (BUG-170). REQ-577 fixed the
documented facts; this REQ fixes the trap itself: a user who pastes
`https://api.moonshot.ai/v1` today registers cleanly and 404s on their first
turn, one step removed from the cause.

The fix is **composition at the registration seam, never at request time**:
`teton provider add` accepts either form, composes the kind's canonical
request path when the given URL clearly lacks one, and **persists the
absolute request URL**. Everything downstream — `Config::validate`, the
adapters' verbatim POST, egress origin-binding, the REQ-577 seam tests — is
untouched, because the stored value never changes shape. The Anthropic kind
additionally gets a registration-time default endpoint (the missing-endpoint
rejection currently fires *after* the user's key has been read; the default
plus BR-5's reordering removes that failure mode entirely).

Deliberately forgiving where it must be: self-hosted gateways and proxies
serve chat completions at arbitrary paths, so an explicit non-canonical path
is trusted verbatim — composition only fills in what is unambiguously
missing, and the CLI always says what it stored.

This was scoped out of REQ-577 at the Phase-5 halt (option (b)) and
commissioned as its own REQ.

## System Model

### Entities

No new entities and no schema change. `ProviderConfig.endpoint` remains the
stored absolute request URL (`Option<String>`); this REQ changes only *how
the registration surface derives it* from user input. The per-kind canonical
request paths (openai-compatible → `/chat/completions` terminal path;
anthropic → `/v1/messages`; anthropic default origin
`https://api.anthropic.com`) are product-owned facts with one typed source,
adjacent to the REQ-577 recipe catalog's seam test.

### Events

No new event types. The CLI's registration flow gains one echo line when the
stored endpoint differs from what the user typed (BR-4); it rides the
existing output surface, not a new event.

### Permissions

| Action | Roles Allowed |
|--------|---------------|
| register a provider (with composition) | the user via `teton provider add` — unchanged; config mutation stays presence-gated per REQ-576 |

## Business Rules

- [ ] BR-1: `provider add` accepts a vendor base URL or an absolute request
  URL for remote kinds; what is **persisted is always the absolute request
  URL**. Composition happens at the registration seam only, never at request
  time — the adapter's verbatim-POST contract and its REQ-577 pin test are
  untouched and unmodified. (informed by LESSON-523)
- [ ] BR-2: composition is class-based and conservative, per kind:
  (a) input already ends with the kind's canonical request path → stored
  verbatim; (b) input has no path, a bare `/`, or a bare `/v1`(`/`) → the
  canonical path is appended; (c) input has any other explicit path → stored
  verbatim (custom gateways and proxies stay first-class, never "corrected").
  The per-kind canonical paths are facts verified against vendor docs at
  implementation time and golden-pinned beside the recipe catalog's seam
  test. (informed by LESSON-523, LESSON-512)
- [ ] BR-3: `--kind anthropic` with no `--endpoint` defaults to the official
  Messages URL, **written explicitly into config at registration time** — no
  runtime or invisible default; the stored config remains the declared
  identity, and the add path can no longer reach `Config::validate`'s
  missing-endpoint rejection.
- [ ] BR-4: whenever the stored endpoint differs from what the user typed
  (composition applied, or the Anthropic default filled in), the CLI echoes
  the stored value at registration time — the user learns exactly what will
  be called, at the moment it is decided, not from a downstream 404.
  (informed by LESSON-456, BUG-146)
- [ ] BR-5: no credential is read until the registration is structurally
  complete — kind, model, and post-composition endpoint all present and
  well-formed. The echo-off key prompt is the last step before the RPC.
  (informed by BUG-170; complements the in-flight keychain-cleanup chip)
- [ ] BR-6: `Config::validate` gains **no new fatal class** — a hand-edited
  endpoint with a custom path remains structurally valid exactly as today
  (LESSON-506's validity-vs-usability split). Instead, `teton doctor` gains
  an advisory check: a remote provider whose endpoint matches BR-2 class (b)
  shapes (bare origin or bare `/v1`) is flagged with the exact full form to
  use — advisory wording, never a failing status.
- [ ] BR-7: strict idempotence for existing users: every already-valid
  config and every previously documented full-URL command behaves
  byte-identically. REQ-577's recipes, prose gates, and catalog tests are
  unchanged by this REQ.
- [ ] BR-8: the composition rules are tested non-vacuously: a unit table
  covering every (kind × BR-2 class) cell, plus at least one end-to-end
  registration that executes the composed result through the real
  `config/set` validation path — and a mutation check proving the
  composition's removal fails the base-URL acceptance test. (informed by
  LESSON-519, LESSON-520, LESSON-523's execute-don't-read rule)

## Acceptance Criteria

- [ ] AC-1: `teton provider add kimi --kind openai-compatible --endpoint
  https://api.moonshot.ai/v1 --model kimi-k3` persists
  `https://api.moonshot.ai/v1/chat/completions`, echoes the stored form, and
  the resulting config passes `Config::validate` (e2e through the real
  registration path).
- [ ] AC-2: the same command with the full request URL persists it
  byte-identically with no echo note (idempotence).
- [ ] AC-3: `teton provider add claude --kind anthropic --model
  claude-opus-5` with no `--endpoint` persists the official Messages URL,
  echoes it, validates cleanly — and the key prompt appears only after the
  endpoint is determined (BR-5 ordering observable in the flow).
- [ ] AC-4: an explicit custom path (e.g.
  `https://gw.example.com/llm/proxy`) is stored verbatim for both remote
  kinds — no composition, no warning at registration.
- [ ] AC-5: `teton doctor` flags a hand-edited bare-`/v1` remote endpoint
  with the exact full form, as an advisory that does not change doctor's
  exit status; a custom-path endpoint is not flagged.
- [ ] AC-6: the REQ-577 adapter verbatim-POST pin, recipe seam test, and all
  prose gates pass **unmodified** — this REQ's diff does not touch those
  test files.
- [ ] AC-7: mutation check recorded: with composition removed, AC-1's test
  fails on the missing path (not on some adjacent assertion).

## External Dependencies

- Vendor documentation consulted at implementation time to verify the
  per-kind canonical request paths (the both-halves rule — LESSON-523). No
  runtime dependency.

## Assumptions

- Canonical request paths are stable at release cadence (they survived the
  REQ-577 round-2 re-verification unchanged even as doc hosts moved).
- ~~The in-flight chip "Clean up keychain entry on rejected provider add"
  (task_956eff31) edits the same registration flow's rejection branch; BR-5
  edits its pre-prompt ordering. A rebase of whichever lands second is
  expected.~~ **Resolved 2026-08-15 before implementation began:** the chip
  landed first as BUG-171 (PR #146, commit 4e16d14) and this REQ's branch is
  cut from a main that already carries it. BR-5's prevention layers on top
  of the landed take-back fallback; no rebase needed. Implementation must
  read the post-BUG-171 shape of the rejection branch, not the pre-chip one
  described in older review notes.
- Composition needs exactly two kind rules today (`openai-compatible`,
  `anthropic`); the typed source makes a third kind's rule a compile-time
  addition, not a convention.

## Open Questions

- [x] OQ-1: Which surfaces compose? **Resolved (adopted at /proceed launch,
  2026-08-15, per the draft's recommendation):** the shared
  registration-building path the CLI uses (so any future client of that
  path — the VS Code extension's add-provider flow — inherits it), while a
  raw `config/set` carrying an explicit `ProviderConfig` stays verbatim: it
  is the programmatic/power-user seam, and silently rewriting its payload
  would violate the durable-document posture (REQ-574).

## Out of Scope

- Rewriting REQ-577's recipes, guide, or README to prefer base URLs — the
  full request URL remains the canonical documented form (LESSON-523's
  honest teaching); composition is forgiveness, not the new convention.
- Keychain cleanup on rejected registration (in-flight chip task_956eff31).
- Normalizing or migrating hand-edited configs (BR-6 is advisory only).
- New provider kinds, or per-vendor path variants beyond the two kinds'
  canonical paths.
- Request-time URL rewriting of any form.

## Retrieved Context

- LESSON-523 (lesson, score 9): A named example is verified against both halves of its contract — the vendor's and the product's own
- LESSON-524 (lesson, score 6): Exposure is not callability — a capability asserted present must be asserted usable at every permission level
- LESSON-515 (lesson, score 6): A feature-gated target is invisible to every refactor
- LESSON-518 (lesson, score 6): A blocking gate's reader-loop freedom is not inherited from the await-based reader-loop tests
- LESSON-519 (lesson, score 6): An 'assert by inspection, not from the error' AC needs the real artifact — add a refusing test seam to reach it
- LESSON-520 (lesson, score 6): A gate that fires before deserialization makes an invalid-payload test vacuous — use a persistable payload + a refuse/accept pair
- BUG-167 (bug, score 6): The llama-gated template smoke no longer compiles
- LESSON-510 (lesson, score 6): A harness that checked a binary exists has not checked it is the one under test
- LESSON-496 (lesson, score 6): "Cut first under pressure" means "never available" when the limit equals the count
- LESSON-481 (lesson, score 6): A gate that hides a feature from users also hides it from the test suite — split the logic out from under the gate
- LESSON-456 (lesson, score 6): A `_`-discarded error is a silent downgrade — the daemon knew exactly why, and told the user something else
- BUG-146 (bug, score 6): First prompt after install fails with a message blaming the local engine for a config/timing problem
- LESSON-506 (lesson, score 5): A fail-closed load gate runs before the migration meant to satisfy it
- LESSON-495 (lesson, score 5): A remembered grant answers every question its key matches — so the key must encode the whole question
- BUG-152 (bug, score 5): A prompt typed while the local tier is still loading is reported as an error, not as a wait
