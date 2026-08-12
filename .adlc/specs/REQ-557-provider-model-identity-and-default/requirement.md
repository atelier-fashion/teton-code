---
id: REQ-557
title: "Provider model identity and an explicit default provider"
status: complete
deployable: true
created: 2026-08-05
updated: 2026-08-12
component: "daemon/router"
domain: "providers"
stack: ["rust", "daemon", "cli", "llm-providers"]
concerns: ["cost", "routing", "extensibility"]
tags: ["provider-registration", "model-selection", "default-provider", "price-table", "byom"]
---

## Description

REQ-544's charter promise is a legible routing policy — *"architecture goes to
Opus, implementation goes to DeepSeek"*. That sentence is **not expressible in
the product today**, because a provider has no model.

`teton provider add` takes `id`, `--kind`, and `--endpoint`. The `ModelProvider`
entity (REQ-544 System Model) carries `id`, `kind`, `endpoint`, `auth_ref`, and
`capabilities` — no model. The model string the router reports comes from
`billing_model()` (`crates/tetond/src/runtime.rs:2772`), which searches the
**price table** for the first entry whose `provider_id` matches and returns its
`model`, falling back to the provider id itself when nothing matches. A billing
artifact is standing in for a routing fact.

Three consequences follow:

1. **One provider is one model.** Routing policy keys on `provider_id`, so
   "Opus for architecture, Sonnet for implementation" would need two providers —
   but registering `anthropic` twice is registering the same id twice. The user
   can express *which vendor* handles a phase, never *which model*.
2. **The model name is a lookup, not a declaration.** A provider whose id is
   absent from the price table reports its own id as its model
   (`map_or_else(|| provider_id.to_owned(), …)`) — the same
   fallback-identifier-standing-for-absence shape that produced BUG-146, where
   `build_router` substituted the literal `"local"` for a provider that existed
   nowhere and the session announced a route to it. LESSON-456 names the rule
   this violates: *a fallback identifier is not "none" — keep the `Option`.*
3. **The default remote provider is whichever one is first in the array.**
   `build_router` (`crates/tetond/src/runtime.rs:2680`) selects it with
   `.find(|p| p.kind.is_remote())`. There is no `default` setting; the answer to
   "where does an unrouted turn go?" is config file ordering, which no surface
   displays and no command sets.

This REQ makes the model a first-class, declared property of a provider and
makes the default an explicit choice. It is the **blocking prerequisite** for
REQ-558 (routing categories) and REQ-559 (reasoning effort): a category cannot
bind to a model that cannot be named, and an effort level cannot be clamped
against a model whose identity is inferred from a price table.

Scope is deliberately narrow — this REQ adds the field, the CLI surface, the
default setting, and the migration. It does not change the routing axis (REQ-558)
and adds no new request parameters (REQ-559).

## System Model

### Entities

| Entity | Field | Type | Constraints |
|--------|-------|------|-------------|
| ModelProvider | id | string | required, unique — unchanged |
| ModelProvider | kind | enum(local, openai-compatible, anthropic, custom) | required — unchanged |
| ModelProvider | endpoint | string (URL) | required for remote kinds — unchanged |
| ModelProvider | **model** | string | **new**; required for remote kinds — enforced by a *non-fatal usability pass*, not by the deserializer and not by `validate()` (BR-7). The exact model identifier sent on the wire (e.g. `claude-opus-5`, `deepseek-chat`). Never inferred, never defaulted to the provider id |
| ModelProvider | auth_ref | string | unchanged; two providers sharing a vendor MAY share one `auth_ref` |
| ModelProvider | capabilities | object | unchanged |
| Config | **default_provider** | Option\<string\> | **new**; FK → ModelProvider.id. `None` is a real state meaning "no default configured", never a literal placeholder |
| PriceTableEntry | model | string | **key changes**: priced by `model`, not by `provider_id` — two providers on the same model price identically |

`ModelProvider` is defined in `crates/teton-core/src/entities.rs:74` (not
`config.rs`, which holds the `Config` that owns a `Vec<ModelProvider>` and the
validation pass at `config.rs:269`).

For the local kind, `model` is the selected catalog entry and remains owned by
the REQ-547 consent flow — it is read, never set by `teton provider add`.

### Events

| Event | Trigger | Payload |
|-------|---------|---------|
| route_decided | unchanged trigger | gains `model` as a **declared** field sourced from `ModelProvider.model` rather than derived from the price table |
| cost_recorded | unchanged trigger | `CostRecord.model` unchanged in shape; its provenance becomes the provider's declared model |

No new events and no new RPCs. `config/get`'s `ProviderConfig` projection gains
`model`; `provider/add` gains the parameter.

### Permissions

| Action | Roles Allowed |
|--------|---------------|
| Set `ModelProvider.model` | the user only, via `teton provider add --model` or a config-file edit — never inferable from model output or file content (REQ-544 permission posture) |
| Set `default_provider` | the user only, same posture |
| Read the effective provider→model mapping | any attached client (`teton provider list`, `config/get`) |

## Business Rules

- [x] BR-1: `ModelProvider` carries a declared `model` string. For every remote
      kind it is **required** at registration; `teton provider add` without
      `--model` fails with a message naming the flag, and never invents a value.
      The provider id MUST NOT be usable as a stand-in for the model — the
      `map_or_else(|| provider_id.to_owned(), …)` fallback in `billing_model()`
      is deleted, not relocated. (informed by LESSON-456, BUG-146)
- [x] BR-2: A provider's `model` is the string sent to the provider's API. The
      router reads it from the provider record; nothing derives a model
      identifier from the price table, from the provider id, or from the
      endpoint. Pricing is a **consumer** of the model string, never its source.
- [x] BR-3: Multiple providers MAY share a `kind`, `endpoint`, and `auth_ref`
      while differing in `model` and `id` — this is the shape that makes
      "Opus for design, Sonnet for build" expressible, and it is the point of
      the REQ. Registering two providers with the same `id` remains an error.
- [x] BR-4: `default_provider` is an explicit, user-set config key.
      `build_router`'s positional selection is removed — **both halves of it**.
      Today `default_provider` falls back to `local_provider` when no remote is
      registered, and `local_provider` itself falls back to the literal string
      `"local"` (`runtime.rs:2675-2684`), so an unconfigured install routes every
      turn to a provider id registered nowhere. That doubled
      fallback-identifier-standing-for-absence is precisely BUG-146's root cause
      #1. When no default is configured the value is `None` — a real absence
      carried in the type, which the router surfaces as a nameable "no default
      provider configured" condition, never as a synthesized id.
      (informed by LESSON-456, BUG-146)
- [x] BR-5: An unroutable turn caused by a missing or dangling
      `default_provider` reports **that** cause, classified in the same branch
      that chooses the sentence, and reuses `unserved_turn_error`'s existing
      precedence rather than adding a second classifier for the same machine
      state. (informed by BUG-146, BUG-152, LESSON-456)
- [x] BR-6: A `default_provider` naming an unregistered id is rejected at config
      load with a message naming the id and the registered ids — it must not
      become a route that fails later, further from the cause.
- [x] BR-7: **Migration is one-time and loud.** An existing config whose
      providers lack `model` is migrated by resolving each provider's model
      through today's price-table lookup exactly once and writing the result
      back as a declared field. Any provider the lookup cannot resolve is
      reported by id with the `teton provider add --model` remedy, and the
      daemon starts with that provider unusable rather than silently routing to
      a provider-id-shaped model string. Migration never runs twice and never
      guesses.

      **The field must be reachable by the migration — at two layers.** First,
      `ModelProvider` is a serde struct; a `model` declared as a bare required
      `String` makes every pre-REQ config fail to *deserialize*, and a config
      that cannot be opened cannot be migrated. The field therefore lands
      deserializable-as-absent (`Option`/`#[serde(default)]`).

      Second — and this is the layer an earlier draft of this rule got wrong —
      required-ness must **not** live in `Config::validate()` either.
      `Config::load` validates internally and the daemon converts a load error
      into a refusal to start, so a validation-level requirement would still
      block a pre-REQ config from starting long enough to migrate, and would
      make a *single* unresolvable provider prevent startup entirely — the
      opposite of this rule's own "the daemon starts with that provider
      unusable". Required-ness is therefore enforced by a **non-fatal usability
      pass** that reports offending providers by id and marks them unroutable,
      leaving `validate()`'s fail-closed startup posture for genuinely
      structural errors (duplicate ids, raw keys, a **dangling**
      `default_provider` — which is invalid rather than merely incomplete,
      because it names something that does not exist).
- [x] BR-8: `default_provider` is **not** a permission to bypass BR-1 of
      REQ-544. Boundary enforcement, session taint pinning, and egress recording
      are unchanged by this REQ; a default provider is a routing convenience,
      not an egress decision. (informed by LESSON-432)
- [x] BR-9: **Every model Teton calls is tracked, with its cost.** The cost
      surface accounts for models — not providers — so: two providers declaring
      the same model are priced identically from one source of price truth; a
      model with no price is reported as **unpriced and named**, never as a `$0`
      record (REQ-544 BR-2 forbids displaying unattributed spend as actual); and
      the set of models actually in use is enumerable from the cost surface, so
      an unpriced model is *discoverable* rather than merely flagged — a user who
      registers a model the table doesn't know can see which one to add. The
      internal keying that delivers this is `/architect`'s decision; this rule
      constrains the observable, not the data structure.

## Acceptance Criteria

- [ ] AC-1 **[MANUAL GATE — not CI-enforceable; NOT RUN]**:
      `teton provider add opus --kind anthropic --model claude-opus-5` and
      `teton provider add sonnet --kind anthropic --model claude-sonnet-5`
      both succeed; `teton provider list` shows two providers with distinct
      models and the same kind. Registering a third with `id: opus` fails.

      **Unticked deliberately — do not tick without a sign-off block in
      `docs/manual-verification.md`.** It was ticked in error during the
      2026-08-11 wrapup by a sweep that flipped every box rather than checking
      each against evidence; the runbook had already recorded this leg as
      verified *"at the strength it was actually verified — which is not at
      all."* Following REQ-547 AC-13's precedent, which stayed unticked until its
      sign-off was filled and countersigned.

      **The gap is narrow, and the unticked box should not be read as "none of
      this works."** Automated and passing: the same two-provider registration
      over the `config/set` RPC the CLI drives, including the `config/get`
      round-trip (`tetond/tests/e2e/ac_matrix.rs`); the duplicate-id refusal,
      this AC's third clause, through the real CLI binary
      (`teton/tests/cli_e2e.rs`); and `provider list`'s rendering of the declared
      model. **Not** automated: the keychain write and the CLI's own success
      rendering for a *remote* kind via `provider add` — because
      `run_provider_add` hardcodes `default_keychain()` rather than accepting an
      injectable backend, so the subprocess e2e harness cannot substitute the
      existing `MockKeychain`. That is plumbing, not a structural impossibility.
      A ~2-minute manual procedure to close it is in the runbook.
- [x] AC-2: `teton provider add x --kind anthropic` (no `--model`) exits
      non-zero with a message naming `--model`, and registers nothing.
- [x] AC-3: A turn routed to a provider emits `route_decided` whose `model`
      equals that provider's declared `model`, asserted against a provider whose
      id appears **nowhere** in the price table — proving the value is declared,
      not looked up. This test fails against today's binary.
- [x] AC-4: With no `default_provider` set and no category policy matching, a
      turn fails with a message naming the missing default and the
      `teton provider` remedy — not a route to a synthesized provider id. A unit
      test asserts the router's default is `None`, not a string. (informed by
      BUG-146)
- [x] AC-5: A config naming `default_provider = "ghost"` with no such provider
      is rejected at load, naming `ghost` and listing the registered ids.
- [x] AC-6: Migration: a config written by the pre-REQ binary (providers with no
      `model`, one resolvable through the price table and one not) loads with
      the resolvable provider migrated to a declared model, the unresolvable one
      reported by id and marked unusable, and the migration recorded so a second
      start does not re-run it. Both legs in one test.
- [x] AC-7: Cost: two providers declaring the same model produce CostRecords
      priced identically from one source of price truth; a provider declaring a
      model absent from that source produces a record flagged unpriced rather
      than `usd: 0`.
- [x] AC-7b: `teton cost` enumerates every model the session actually called,
      including unpriced ones, naming each unpriced model — a user can read off
      which model needs a price without inspecting config or logs. (BR-9)
- [x] AC-8: Mutation check — restoring the provider-id fallback in
      `billing_model()`, or restoring the positional default-provider `.find`,
      each makes at least one test red. (informed by LESSON-441)

## External Dependencies

- None. No new crates, no new RPCs, no provider API surface change. The price
  table, keychain `auth_ref` resolution, and `config/get` projection all exist.

## Assumptions

- **Verified, not assumed.** `ModelPrice` (`crates/tetond/src/cost/prices.rs:38`)
  already carries both `provider_id` and `model`, so the price data needed for
  BR-9 exists today. Re-orienting the lookup around `model` **is** a struct
  change, not merely a lookup change — but it is **not a user-data migration**:
  the table is `PriceTable::bundled()`, embedded in the binary and never read
  from disk (`runtime.rs:482`, `:549`). Nothing a user owns has to change shape.
- No shipped config in the wild predates the price table such that BR-7's
  one-time migration has nothing to resolve against. Teton is pre-alpha and
  distribution began at REQ-548; the migration's unresolvable branch covers the
  case regardless.
- **Verified, not assumed.** `auth_ref` sharing across two providers of the same
  vendor works today, so BR-3's core case needs no new credential mechanism:
  `provider_transport` (`crates/tetond/src/runtime.rs:2596-2625`) binds a
  resolved credential to the provider's **endpoint origin**, not to its id. Two
  providers with the same `endpoint` and the same `auth_ref` both resolve and
  both attach the header. The one constraint that carries over: an `auth_ref`
  provider whose endpoint does not parse to a network origin is rejected as a
  credential error, and that check is per-provider, so each of the two must
  declare the endpoint.
- REQ-558 and REQ-559 are sequenced after this REQ and may assume a declared
  `ModelProvider.model` and a real `Option<default_provider>`.
- id allocated with remote verification (no degradation warning from the
  allocator).

## Open Questions

All four were settled by the implementation; dispositions verified against `main`
at wrapup (2026-08-11) rather than asserted from the tasks.

- [x] OQ-1 — **accept-and-fail-late.** `teton provider add` takes the model
      string as given; no vendor `models/list` call at registration. Registration
      therefore needs neither network nor a working credential. On the
      `teton doctor` half the spec leaned on: `doctor` exists and renders the
      provider table, which now carries `model` via TASK-046's projection — so a
      *missing* model is visible there. What it does **not** do is validate a
      model string against the vendor or flag a typo, and the unusable-provider
      report lands at daemon startup (ADR-E) rather than in `doctor`. See
      Deferred.
- [x] OQ-2 — **config-file only in v1.** No `teton provider default <id>`
      subcommand exists (verified: `ProviderAction` has no `Default` variant).
      Consistent with REQ-555's out-of-scope posture for `/provider`.
- [x] OQ-3 — **no implicit default.** `build_router` reads
      `config.default_provider` directly with no `.find` over remote providers
      (`runtime.rs:5782`), so registering a second provider cannot silently
      change routing. This is ADR-D and it is the whole point of BR-4.
- [x] OQ-4 — **not mirrored.** The local provider's `model` stays absent from
      this surface; the REQ-547 consent flow remains the single owner of the
      local model selection (TASK-046 pinned this explicitly). Avoids the
      second-source drift LESSON-456 warns about.

## Deferred

Recorded at wrapup (2026-08-11); the verification gap added 2026-08-12. The
first two were not descoped mid-flight — both are consequences of OQ
dispositions made during architecture.

- **A `teton doctor` check for model-string validity** (OQ-1). `doctor` renders
  the provider table including `model`, so an *absent* model is visible, but
  nothing catches a *wrong* one — a typo'd model string is discovered at first
  use, as accept-and-fail-late intends. Worth a follow-up only if typo'd models
  turn out to be a real support cost.
- **`teton provider default <id>`** (OQ-2). Setting the default requires a
  config-file edit in v1. If REQ-560's permission/status work makes routing state
  more visible, a subcommand becomes the obvious companion.
- **A verification gap, not deferred work: AC-1's CLI success path is NOT RUN.**
  `teton provider add --model` for a *remote* kind writes to the keychain, and
  `run_provider_add` hardcodes `default_keychain()` instead of taking an
  injectable backend — so the subprocess e2e harness cannot substitute the
  `MockKeychain` that already exists and is already used one frame down. An
  automated success leg would write real entries into the developer's login
  keychain and can prompt mid-suite. The closing move is an env-gated backend
  override, which is a security-sensitive seam and deserves its own design. Until
  then AC-1 stays unticked and the ~2-minute manual procedure lives in
  `docs/manual-verification.md`.

## Out of Scope

- The routing axis itself — categories, tiers, and the freeform heuristic stay
  exactly as they are (REQ-558).
- Reasoning effort, thinking parameters, and any new request-body field
  (REQ-559).
- Permission levels and CLI status line (REQ-560).
- Per-provider model *lists* or in-session model switching for remote providers
  (`/model set` remains local-tier only, REQ-555 OQ-2).
- Automatic model discovery from a provider's `models` endpoint.
- Any change to keychain storage, egress enforcement, or the cost meter's
  rendering beyond BR-9's unpriced state.

## Retrieved Context

- REQ-544 (spec, score 10): Teton Code — hybrid local/remote AI coding agent with workflow-aware model routing
- LESSON-456 (lesson, score 5): A `_`-discarded error is a silent downgrade — the daemon knew exactly why, and told the user something else
- BUG-146 (bug, score 5): First prompt after install fails with a message blaming the local engine for a config/timing problem
- REQ-555 (spec, score 4): In-session slash commands for the teton interactive CLI
- REQ-547 (spec, score 3): First-run local model consent
- LESSON-482 (lesson, score 3): A prompt that enumerates a turn's legal endings must name every one
- BUG-152 (bug, score 3): A prompt typed while the local tier is still loading is reported as an error, not as a wait
- LESSON-445 (lesson, score 2): Side effects of a minutes-long operation must be staged, then committed only after re-checking authority
- LESSON-443 (lesson, score 2): A guard keyed on a feature's absence disables itself when the feature lands
- LESSON-432 (lesson, score 2): Provenance must derive from what a tool touches, not from an argument name
- LESSON-481 (lesson, score 2): A gate that hides a feature from users also hides it from the test suite
- REQ-556 (spec, score 2): Live model-loading progress in the interactive session
- BUG-153 (bug, score 2): /exit is not a command
- REQ-554 (spec, score 1): Local tier renders prompts through the model's native chat template
- REQ-549 (spec, score 1): Daemon process identity and interactive startup UX

Note: the retrieval contract's spec-status filter (`approved|in-progress|deployed`)
matches zero specs in this project — every spec carries `status: complete`, which
was treated as the local spelling of `deployed`, consistent with the precedent
recorded in REQ-555 and REQ-556. The Step-1.6 delegated body-read timed out
(SIGTERM at 120s); the documented fallback path ran and the top-15 bodies were
read directly.
