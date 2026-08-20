---
id: REQ-586
title: "A turn's context budget follows its route — remote tiers get the provider's window, bounded by what the redact scan can cover, and nothing is clamped in silence"
status: approved
deployable: true
created: 2026-08-19
updated: 2026-08-19
component: "daemon/harness"
domain: "harness"
stack: ["rust", "daemon", "llm-providers", "json-rpc"]
concerns: ["cost", "privacy", "reliability", "developer-experience"]
tags: ["context-budget", "context-window", "max_context", "truncate_to_budget", "compaction", "digest", "summarization", "redact", "chunking", "routing", "route", "reroute", "fallback", "prompt-size", "silent-truncation", "over-window", "egress", "capability-profile", "harness-config", "carry", "provider-config", "skills", "automation", "adlc"]
---

## Description

Every turn the harness runs — local or remote — is assembled under **one**
context budget: `HarnessConfig::default()` sets `context_budget_tokens =
4_096` (whitespace-separated words, the estimator's unit) and
`context_budget_bytes = 32_768` (the dense-content guard, sized at ≈2 bytes
per BPE token so it tracks the local engine's 16,384-token window), the
system prompt is charged against it, and `HarnessConfig::from_harness_profile`
— the constructor *every* route goes through, local and remote alike —
copies those two numbers from the default. The provider's declared window
(`capabilities.max_context`) never reaches the harness. And that field is
almost always **unknown**: the config entity defaults it to `0`, the
Anthropic adapter's 200,000 constant is overridden by whatever the config
record carries (i.e. `0` unless a user hand-wrote a
`[providers.<id>.capabilities]` table), nothing in `/provider add`,
`/provider setup`, `config/set` or the vendor recipes sets it, and nothing in
`/doctor` or `/provider list` shows it — the wire `ProviderConfig` has no
capabilities field at all. So a turn routed to a 200k-token frontier model
is assembled in a window sized for the local engine, and when a prompt does
not fit, `ContextManager::truncate_to_budget` drops the oldest blocks and
then **middle-elides the last block in place**. The model is told (a
`[earlier conversation truncated…]` note and an elision marker land in the
prompt — the marker hard-codes "local context window", which would be a lie
on a remote route); the **user** is told nothing: no event, no line, nothing
in `/verbose`. The model answers a prompt the user did not send.

That was invisible while prompts were short. REQ-585 made it a wall: the
ADLC toolkit's skills are prompt templates of 400 to 7,200 words, and
measured against the budget with the ~850-word system prompt and the
599-word ethos include every one of them inlines, **seven of the seventeen
cannot fit on any tier** — `/spec`, `/manifest`, `/analyze`,
`/template-drift`, `/wrapup`, `/sprint`, `/proceed` — including on a Kimi or
Claude route with a window thirty to fifty times larger than the budget. The
product owner's answer to REQ-585's OQ-0 was unambiguous: *the big skills are
the point; we need them for automation.* This REQ is the unblocker, and it is
its own REQ rather than a BR of REQ-585 because the budget is load-bearing in
places that have nothing to do with skills: the conversation carry (REQ-567
BR-4: the budget spans the session and compaction, not failure, is the
response to pressure), the `compact` duty's soft threshold
(`COMPACT_PRESSURE_PERCENT` = 70% of either budget — already proportional)
and `truncate_to_budget`'s hard backstop (REQ-561 BR-4/BR-4a), the `digest`
duty's per-result threshold (`summarize_threshold_tokens` = 1,500 words),
the mid-turn **reroute** paths (a privacy block re-routes the same turn to
the local pin; a provider failure fails over to the fallback route — both
keep the `ContextManager` the first route seeded), and the redact scan's
total cap, whose chunk arithmetic is **derived from** `context_budget_bytes`
(`REDACT_TOTAL_CAP_CHUNKS`: 2 × (32,768 + 10,240) ÷ 27,070 → 4 chunks →
`REDACT_INPUT_MAX_BYTES` = 108,280, the largest payload the redactor will scan
at all). Raise the budget without re-deriving that, and a long remote turn on
a machine that opted into the scan is blocked as "unscannable" — the exact
collision REQ-562/REQ-577 closed.

What this REQ does: **a turn's context budget derives from the route that
serves it — per route *attempt*.** A remote route's budget comes from its
provider's window minus the turn's generation reservation and a stated
safety allowance for the estimator (whitespace words undercount subword
tokens; the byte guard gets its own window-derived floor); an unknown window
keeps today's default and **says so** where the user can see it, and the
user gains a way to declare one that is not a hand edit — an additive
protocol change; the local tier keeps its engine-derived budget (OQ-3). When
the route is rerouted mid-turn — privacy block to the local pin, provider
failure to the fallback — the budget is re-derived from the new route and
the context is re-fitted, loudly, before the next model call. When the
redact scan applies to the route's egress, the budget is further bounded by
what the scan can cover, and that bound is **named** rather than discovered
as a blocked turn. Compaction keeps firing at 70% of whatever the budget is;
the `digest` threshold scales with it so a 128k route is not condensing a
1,500-word file through a local model call nobody asked for. And no turn is
clamped in silence any more: dropping blocks or eliding a block is an event
and a line, on every tier, and the in-prompt marker names the right window —
which is the surface REQ-585 BR-8's refusal for oversized skill turns stands
on.

Why this is a cost feature and not only a capacity one: **this budget is the
only per-turn input-token bound Teton has.** There is no spend cap, no
`[cost]` table, and a `Native`-tier route runs up to 25 loop iterations (40
on `for_strong_model`), each re-sending the whole assembled context. Today
the worst case is ≈4k words × 25 per prompt; after this REQ, on a 128k
window, it is ≈85k words × 25 — with only the optional per-provider cap
(BR-5) between the user and that number. The product promise is that the
user *chooses* what a frontier model sees: declaring the window is that
choice, and what this REQ owes them in return is that the budget in effect
is visible on every turn, that the worst case is stated in the docs and the
runbook, that the cap is one line of config, and that a provider with an
unknown window is never silently stuck at 4,096. The cost meter already
prices every token that leaves; this REQ does not change attribution, it
changes how many tokens there can be.

## System Model

_Shapes are illustrative; the constraints are the requirement._

### Entities

| Entity | Field | Type | Constraints |
|--------|-------|------|-------------|
| ProviderCapabilities (existing, config) | max_context | u32 tokens | `0` = unknown; today settable only by hand-editing `[providers.<id>.capabilities]`; the adapters' own defaults are overridden by this value, so every record without the table is *unknown* — Anthropic included |
| ProviderCapabilities | context_budget_cap | u32 tokens? (new, optional) | a user ceiling below the window; effective budget = `min(window-derived, cap)` |
| ProviderConfig (wire, `teton-protocol`) | capabilities / max_context | (new, additive) | today the wire record has no capabilities field and `ConfigUpdate` has no variant for one — so `/provider add`, `/provider setup`, `config/set`, `/doctor`, `/provider list` can neither set nor show a window without an additive protocol change (REQ-573's rule: older peers ignore the field / degrade) |
| RouteBudget (new, derived per route attempt) | budget_tokens, budget_bytes | usize | the `HarnessConfig` pair for this attempt; derived from the route's provider window (declared remote) or unchanged (local / unknown); re-derived on reroute or fallback |
| RouteBudget | generation_reservation | u32 tokens | the attempt's `max_tokens`; subtracted from the window first |
| RouteBudget | safety_ratio (words), bytes_per_token_floor (bytes) | pinned constants | the two estimator allowances — words undercount subword tokens; bytes guard dense content — each pinned by AC-3 against a corpus |
| RouteBudget | bound | `window` / `default_unknown` / `redact_scan` / `user_cap` / `local_engine` | **which constraint bound the budget** — computed at route time, one source, read by every surface |
| RouteBudget | compact_at, digest_threshold | usize | `COMPACT_PRESSURE_PERCENT` (70%) of `budget_tokens` (unchanged rule); the `digest` per-result threshold as the same fraction of the route budget it is today (1,500/4,096) |
| ContextPressure (new event payload) | kind | `blocks_dropped` / `block_elided` / `refit_on_reroute` | what the gate did and why |
| ContextPressure | dropped_blocks, elided_bytes, budget_tokens, bound | usize, usize, usize, enum | enough for a client to render one honest line |

### Events

| Event | Trigger | Payload |
|-------|---------|---------|
| `route_decided` (existing, additive fields) | every route attempt, as today | + `budget_tokens`, `bound` — the carrier for the per-turn `/verbose` line and BR-8's one-source rule (the bound is computed where the route is) |
| `context_pressure` (new, additive) | `truncate_to_budget` dropped ≥1 block, elided a block in place, or re-fitted the context after a reroute/fallback | kind, dropped_blocks, elided_bytes, budget_tokens, bound, session_id |

Older clients ignore an unknown event or field (the REQ-573 additive rule);
the CLI renders `context_pressure` as one line (`context: 3 older blocks
dropped to fit the 4,096-word budget (bound: local engine)`) and `/verbose`
adds the numbers and the per-turn budget line from `route_decided`.

### Permissions

| Action | Roles Allowed |
|--------|---------------|
| set `max_context` / `context_budget_cap` on a provider | the user, through the same `config/set` gate every provider-record write meets (presence-attested where the build has it — REQ-576 BR-10(b)), once the wire carries the field; `/provider add` and `/provider setup` may collect a window (OQ-1) |
| a remote turn sending up to the derived budget | automatic, once the user has declared the window — the declaration is the consent; egress, boundaries and taint are untouched |
| the model changing its own budget | never |

## Business Rules

- [ ] BR-1: **The budget is a property of the route attempt, derived per
  attempt.** The `HarnessConfig` an attempt runs under carries a
  `(budget_tokens, budget_bytes)` pair derived from the route's provider: for
  a remote provider with a declared window, from that window; for a remote
  provider with an unknown window (`max_context = 0`), today's default pair;
  for the local tier, today's default pair (OQ-3). `CarriedTurn::begin` seeds
  the turn's `ContextManager` from the first attempt's pair as it does today
  — and when the turn is **rerouted mid-turn** (a privacy block to the local
  pin, a provider failure to the fallback route), the budget is re-derived
  from the new route and the context is re-fitted by `truncate_to_budget`
  **before the next model call**, with a `context_pressure { kind:
  refit_on_reroute }` event. The third mid-turn re-config path — a
  **degrade** of the *same* provider to the reduced harness profile
  (`PrimaryDegraded`, after a malformed tool call) — keeps the provider's
  derived budget: the profile changes, the window does not, and no refit or
  `default_unknown` bound appears for a provider whose window is declared. Today every route's budget is equal so the stale
  seed is invisible; after this REQ an 80k-word context assembled for a 128k
  route and rerouted local must compact-and-fit, never reach the local engine
  as an over-window error or a smaller fallback provider as a 400 (informed
  by REQ-567 BR-4, REQ-561 BR-4a).
- [ ] BR-2: **A remote budget is sized so the window is not exceeded for
  the content classes the corpus measures — and the backstop is typed.**
  `budget_tokens = (max_context − generation_reservation) / safety_ratio`,
  and `budget_bytes` is derived **from the window with its own pinned
  bytes-per-token floor** (≈2 B/token, today's dense-content rule), *not* as
  8 × `budget_tokens` — scaling the word budget by 8 would let a minified
  JSON or base64 tool result pass both guards and be rejected by the
  provider. Both constants are pinned by AC-3 against a corpus (prose, Rust,
  JSON tool results, path-heavy shell output) tokenized with a reference
  tokenizer, asserting the **combined** estimate — `max(words × ratio, bytes
  / floor) ≥ tokens` — covers every sample; no single ratio covers
  path-heavy output on its own, which is why both guards exist. A provider
  "context length exceeded" response is a **typed outcome** naming the window
  and the assembled size; it does not retry in a loop and it does **not**
  degrade the provider's health or trigger failover as a generic client
  error does today (informed by REQ-581's typed-outcome rule).
- [ ] BR-3: **An unknown window is stated, not silently defaulted — and
  declaring one is not a hand edit.** A provider with `max_context = 0` runs
  under the default budget, and the fact is visible: `/doctor` and `/provider
  list` name the window as unknown and say the budget is defaulted,
  `/verbose` names the bound on every turn, and the BR-7/REQ-585 refusal text
  says "set `capabilities.max_context` for `<id>`". Because the wire
  `ProviderConfig` carries no capabilities today, this requires an
  **additive protocol change** (a capabilities field on the wire record, or
  a `ConfigUpdate` variant), after which the shipped vendor recipes (REQ-577)
  carry the window for their example models so `/provider setup` records one,
  `/provider add` accepts one, `config/set` accepts one, and `/doctor` /
  `/provider list` show it. No window is ever *guessed* from a model name
  outside the recipes (informed by LESSON-496 — a capability the user enabled
  must not be withheld without a voice; REQ-577; REQ-573's additive rule).
- [ ] BR-4: **When `[privacy] redact = true`, the budget is bounded by what
  the scan can cover — and the bound is named.** The model redact scan runs
  on the **whole outbound request body** — system prompt + context + JSON
  escaping — only when `[privacy] redact = true` (default `false`; the search
  tier's scan covers lookup egress, not the turn body, and does not bound
  it). A scanned route cannot assemble a body the redactor refuses as
  unscannable (`REDACT_INPUT_MAX_BYTES`, fail-closed → Block). The effective
  budget on such a route is therefore `min(window-derived, scannable)`, where
  `scannable_bytes = (REDACT_INPUT_MAX_BYTES − system-prompt overhead) /
  escaping factor` — a **floor** (the overhead term already folds the
  default budget's escaping, so the bound is conservative by a few KB), and
  **cap minus overhead, not cap with the 2× margin inverted**: the margin in the cap's derivation exists so a budget bumped
  elsewhere cannot drift past the cap, and a bound *derived from* the cap
  cannot drift by construction. With today's constants that is ≈ 89 KB of
  context (≈ 2.7× today's budget; a body at the cap is four chunk-widths —
  up to five chunks with the overlap, `REDACT_MAX_CHUNKS` — inside the
  envelope REQ-562 measured); it admits every ADLC skill. The overhead
  term is today a `#[cfg(test)]` assumption (`REDACT_BODY_OVERHEAD_BYTES`) and
  must be promoted to, or replaced by, a production constant defined as
  "what the body carries beyond the assembled context" that the bound reads.
  The scan is byte-denominated, so `redact_scan` bounds `budget_bytes` (the
  word component stays window-derived and the byte guard binds), which is
  what makes the bound classification unambiguous. `/verbose` says `bound:
  redact_scan`; raising the chunk cap is not this REQ
  (OQ-2) (informed by REQ-563 BR-2, REQ-567, LESSON-456).
- [ ] BR-5: **An optional per-provider cap holds a big window to a smaller
  budget.** `[providers.<id>.capabilities] context_budget_cap` (name
  illustrative), when set, bounds the derived budget; it is the one cost knob
  this REQ adds and the only new config field besides the wire carrier. Absent,
  the window the user declared is the cap — declaring the window is the
  consent to spend it. Whether recipes should carry a recommended cap is
  OQ-6.
- [ ] BR-6: **Compaction keeps its proportion; the `digest` threshold gains
  one.** The `compact` duty already fires at `COMPACT_PRESSURE_PERCENT` (70%)
  of either budget and `truncate_to_budget` stays the unconditional backstop
  at 100% (REQ-561 BR-4a is unchanged in kind and in number). The `digest`
  duty's per-result threshold (`summarize_threshold_tokens`, 1,500 words
  today) becomes the same fraction of the route budget it is today
  (1,500/4,096 ≈ 37%) — **and its byte twin becomes the same fraction of
  `budget_bytes`** (12,000/32,768, the same 36.6%), never words × 8, so a
  dense (minified JSON, base64) tool result on a big route is still digested
  rather than folded raw at the edge of the byte budget — so a 128k route
  does not condense a 1,500-word `read` result through a local model call it
  has ample room to carry raw (a fidelity loss the user did not ask for),
  while the local tier's rule is byte-identical to today. And because the
  `compact` duty's own prompt renders every block (up to 1,024 bytes each)
  with no total bound, a big-route conversation at 70% pressure can exceed
  the *local* engine's window when `compact` is on its default local binding
  and degrade to the REQ-561 deterministic drop every time: the duty's prompt
  therefore **stays bounded to its route's engine window as the conversation
  grows** (e.g. a bounded window of the oldest blocks — the answer is block
  numbers, so a partial offer still compacts), so "compaction keeps its
  proportion" is true in practice and not only in the threshold. A `compact` or `digest` duty that is itself routed
  remote is subject to the same egress rules as today (informed by REQ-561
  BR-4/BR-4a, REQ-567 BR-4).
- [ ] BR-7: **Nothing is clamped in silence, on any tier — and what the
  model is told names the right window.** When `truncate_to_budget` drops
  blocks, elides a block in place, or re-fits after a reroute, the daemon
  emits `context_pressure` and the CLI renders one line naming what happened,
  the budget and its bound; `/verbose` shows the numbers. An in-place elision
  of the **newest user block** — the case where the model would answer a
  prompt the user did not send — is additionally reported in the turn's own
  output as a notice, not only as an event. The in-prompt truncation note and
  elision marker the model already sees stop hard-coding "local context
  window" and name the route's. This is the surface REQ-585 BR-8 builds its
  skill-turn refusal on; for typed prompts the elision still happens
  (changing that is OQ-4) but it is never silent (informed by REQ-567 BR-4
  "never silently", REQ-561 BR-4, LESSON-543).
- [ ] BR-8: **The bound is one fact with one source.** Which constraint
  bound the budget — `window`, `default_unknown`, `redact_scan`, `user_cap`,
  `local_engine` — is computed once per route attempt, where the route is
  decided (it rides `route_decided`), classified from the routing table's
  local provider id rather than from which constructor built the config
  (every route goes through `from_harness_profile`; `default()` is the
  unresolvable-route case), and is what `/verbose`, `/doctor`,
  `context_pressure` and every refusal text read; no surface re-derives it
  (LESSON-456: one classifier per fact).
- [ ] BR-9: **Cost attribution is unchanged; the budget — the only
  per-turn input-token bound — is visible.** Every token that leaves is
  priced as today; `/cost` rows are unchanged in shape. `/verbose` prints the
  budget in effect and its bound once per turn (from `route_decided`); the
  status line is unchanged. The docs and the REQ-586 runbook state the
  worst case — budget × loop iterations per prompt — and point at BR-5's cap;
  a spend cap is Deferred, not pretended.
- [ ] BR-10: **A route change between turns is a budget change, and the
  carry survives it.** A session whose retained conversation was assembled
  under a 100k budget and whose next turn routes to a 4k tier replays the
  retained blocks and `truncate_to_budget` drops the oldest to fit — exactly
  today's rule — with a `context_pressure` event saying so; the reverse
  direction simply has more room. The prefix cache on the local tier is
  unaffected (REQ-564/REQ-567 BR-7: carry correctness is independent of KV
  state). BR-1 covers the *within*-turn case (informed by REQ-567
  BR-4/BR-7).
- [ ] BR-11: **The redact chunk arithmetic is re-derived where it lives, not
  restated.** The scannable bound is computed from the same constants as
  `REDACT_TOTAL_CAP_CHUNKS` (never a second hand-copied number); the margin
  test `the_total_cap_clears_the_harness_context_budget_with_margin` keeps
  measuring the **default** budget (the local shape), and a second assertion
  pins that a body at the scannable bound plus the overhead and escaping
  terms fits under `REDACT_INPUT_MAX_BYTES` — changing either constant alone
  fails it (informed by LESSON-456).

## Acceptance Criteria

- [x] AC-1: A route to a provider with `max_context = 128000` and a 1,024
  generation reservation yields a `HarnessConfig` whose `budget_tokens` is
  `(128000 − 1024) / safety_ratio` and whose `budget_bytes` is
  `(128000 − 1024) × bytes_per_token_floor`; a route to a provider with
  `max_context = 0` yields today's `(4096, 32768)`; the local route (the
  routing table's local provider id, through `from_harness_profile`) yields
  today's pair; the unresolvable-route `default()` is unchanged. (unit,
  `router.rs`; BR-1, BR-2)
  *(`router.rs::the_route_budget_is_derived_from_the_routes_own_window` —
  the 128k pair, `max_context = 0`, the local tier through the routing table
  and the unresolvable `None` route in one test; arithmetic table
  `budget.rs::derivation_table`; one-source pin
  `budget.rs::harness_config_default_reads_this_module`.)*
- [x] AC-2: With a 128k provider bound to the `think` tier, a prompt of
  20,000 words is assembled whole — no blocks dropped, no elision — and
  reaches the provider in one request; the same prompt on the local tier is
  clamped with a `context_pressure` event. (daemon unit + remote-loop
  fixture; BR-1, BR-7)
  *(`remote_loop.rs::a_128k_route_assembles_a_20000_word_prompt_whole_and_the_default_pair_clamps_it`
  — one request, no blocks dropped, no elision on the remote leg; the same
  prompt clamped with a `context_pressure` on the default pair.)*
- [x] AC-3: The two estimator constants are pinned by a test that tokenizes
  a fixture corpus (prose, Rust source, minified JSON tool results,
  path-heavy shell output, base64) with a reference tokenizer (dev-dependency
  or a committed token-count fixture) and asserts `max(words × safety_ratio,
  bytes / bytes_per_token_floor) ≥ tokens` for every sample **except the
  documented gap**: random base64 tokenizes at ≈1.45 B/token under
  `o200k_base` (measured, TASK-183), below the 2 B/token floor; the floor is
  kept at 2 (lowering it to cover base64 would cost every prose/code prompt
  ≈25% of its remote budget), base64 is recorded as `KNOWN_UNCOVERED` in the
  test — asserted in both directions — and the typed context-length outcome
  is its backstop; a provider
  `context_length_exceeded`-class response surfaces as a typed outcome
  naming the window and the assembled size, with no retry, no failover and
  no change to the provider's health. (unit + remote-loop fixture; BR-2)
  *(`token_corpus.rs::combined_estimate_covers_every_sample_outside_the_documented_gap`
  and
  `token_corpus.rs::words_guard_alone_covers_prose_but_not_dense_content` —
  the corpus claim and `KNOWN_UNCOVERED` asserted in both directions,
  against constants the suite now reads from `harness::budget` and
  `harness::duty` rather than restating (TASK-192). Typed backstop:
  `conformance.rs::each_adapter_maps_its_vendor_spelling_to_context_length_exceeded`,
  `remote_loop.rs::a_context_length_refusal_ends_the_turn_typed_after_one_request`,
  `runtime.rs::a_context_length_refusal_changes_no_health_and_degrades_nothing`.)*
- [x] AC-4: `route_decided` carries `budget_tokens`, `budget_bytes` and
  `bound`; under `/verbose` the route line ends `· budget {n} words / {k} KB
  (bound: {b})` with the bound in words (`window`, `unknown window`, `redact
  scan`, `user cap`, `local engine`), and is byte-identical to the pre-REQ
  line when any field is absent; `/doctor` and `/provider list` say `window:
  unknown — context budget defaulted (set capabilities.max_context)` for a
  provider with `max_context = 0`, `window: 128k` (or `1m`) otherwise,
  `(local engine)` for a local row, and `window: not reported` against an
  older daemon — over the additive wire field. (`cli_e2e` + protocol contract
  test; the older-daemon case is a `render_config` unit test, since a shipped
  daemon always populates the field; BR-3, BR-8, BR-9)
  *(`cli_e2e.rs::every_provider_row_names_its_window_on_both_surfaces` and
  `cli_e2e.rs::doctor_advises_on_an_undeclared_window_and_an_inert_cap_and_stays_green`;
  the route line (and the byte-identical pre-REQ line when a field is
  absent) in
  `session_ui.rs::a_route_line_carries_the_budget_when_the_daemon_states_one`;
  the older-daemon `window: not reported` in
  `main.rs::every_row_says_what_is_known_about_its_context_window`; the wire
  contract in
  `events.rs::route_decided_budget_fields_are_additive_in_both_directions`.)*
- [x] AC-5: The wire `ProviderConfig` (or a `ConfigUpdate` variant) carries
  `max_context` and `context_budget_cap` additively — an older client's
  request without the field is accepted and preserves the stored value; every
  shipped vendor recipe carries a window for its example model and a
  contract test asserts none is zero; `/provider setup` writes it;
  `/provider add` accepts it (flag per OQ-1); `config/set` accepts it through
  the same write gate as every provider-record write. (unit + `cli_e2e`;
  BR-3. Harness note: no e2e completes a *remote* registration — every remote
  kind reads a credential through the developer's real login keychain — so
  the flag→payload seam is pinned by unit test and the end-to-end write is
  pinned with a local provider plus a config-declared window on both listing
  surfaces.)
  *(additive wire —
  `methods.rs::a_provider_record_without_the_window_fields_still_deserializes`,
  `methods.rs::a_client_predating_the_window_fields_still_reads_a_provider_that_carries_them`;
  preservation —
  `runtime.rs::re_registration_merges_the_window_fields_field_wise`,
  `config_preservation.rs::a_field_less_registration_preserves_the_stored_window_and_a_declared_one_writes_it`;
  recipes — `provider_recipes.rs::no_recipe_ships_an_unknown_window`; setup
  and add —
  `provider_setup_flow.rs::a_fresh_setup_writes_the_recipe_window_into_the_capabilities_table`,
  `main.rs::the_declared_window_reaches_the_registration_payload_and_an_undeclared_one_stays_none`,
  `cli_e2e.rs::provider_add_records_a_declared_window_in_the_daemons_config`.)*
- [x] AC-6: With `[privacy] redact = true` and a 128k provider, the budget
  in effect is the scannable bound, `/verbose` says `bound: redact_scan`, and
  a 40,000-word prompt is compacted/clamped to fit **and then scanned
  successfully** — no turn on that route is ever blocked as "unscannable"
  because of its size; a test that removes the bound makes such a turn
  block; with `redact = false` and `[web] tier = search` the bound is
  `window`, not `redact_scan`. (egress-capture + daemon unit; BR-4)
  *(`redact_egress.rs::a_redact_scanned_128k_route_assembles_a_body_the_scan_reads_whole_and_forwards`
  — a body at the bound scanned and forwarded, the same body refused
  unscanned once the bound is removed, and (added by TASK-192, whose
  mutation (i) the two original legs survived) the same pair demanded of
  `Router::budget_for`, so the flag is asserted to reach the derivation and
  not only the test.)*
- [x] AC-7: The scannable bound is computed from the same constants as
  `REDACT_TOTAL_CAP_CHUNKS` with the overhead and escaping terms modelled; a
  test asserts a body at the bound fits under `REDACT_INPUT_MAX_BYTES` and
  that the overhead term the bound reads is a production constant (not
  `#[cfg(test)]`); the default-budget margin test is unchanged and green.
  (unit; BR-4, BR-11)
  *(`redact.rs::the_scannable_bound_plus_overhead_and_escaping_fits_under_the_cap`
  — the bound modelled from the same constants;
  `redact.rs::the_total_cap_clears_the_harness_context_budget_with_margin`
  unchanged and green.)*
  **What this test does and does not pin (re-worded in verify).** It pins the
  *shape* of the derivation — that the bound is `(cap − overhead) × divisor /
  (divisor + 1)` and not a copied literal, and that a dropped escaping term or
  an inverted bound is red. It does **not** fail when a single input moves:
  the bound is an expression over those constants, so it re-derives with them
  and every inequality here holds by construction. A reviewer proved it —
  moving `REDACT_BODY_OVERHEAD_BYTES` from 10 to 12 KiB leaves the package
  green. The inputs are pinned by the margin test
  (`the_total_cap_clears_the_harness_context_budget_with_margin`) instead, and
  the "production constant, not `#[cfg(test)]`" clause is a compile-time
  consequence of reading the constant rather than an assertion. **The overhead
  is pinned only from below** — the margin test asserts the real prompt spends
  *less* than it, and nothing asserts a ceiling until the overhead is large
  enough to break `cap >= 2 × body` (≈21 KiB). Growing it inside that range
  silently shrinks every scanned route's byte budget with nothing red. A
  ceiling on it is the follow-up, not a change made here.
- [x] AC-8: `context_budget_cap = 40000` on a 200k provider bounds the
  budget to the cap and `/verbose` says `bound: user_cap`; absent, the window
  binds. (unit + `cli_e2e`; BR-5)
  *(`budget.rs::derivation_table` — the cap as a window ceiling, inert at or
  above the window;
  `cli_e2e.rs::a_cap_below_the_window_is_the_bound_a_verbose_turn_names` and
  `session_ui.rs::a_route_line_carries_the_budget_when_the_daemon_states_one`
  for `bound: user cap`.)*
- [x] AC-9: On a 100k budget the `compact` duty fires at
  `COMPACT_PRESSURE_PERCENT` (70%) exactly as on 4k, and `truncate_to_budget`
  still fires at 100%; the REQ-561 fallback (failed compaction →
  deterministic truncation) is unchanged and pinned; the `digest` threshold
  on the local tier is byte-identical to today's 1,500 and on a 128k route a
  3,000-word tool result enters context raw while a 240 KB minified tool
  result is digested; a 200-block conversation on a 128k route compacts
  through the local `compact` binding (the duty's prompt fits the local
  window) rather than degrading to the deterministic drop. (unit; BR-6)
  *(`compact.rs::a_hundred_k_budget_is_pressured_at_the_same_percent_as_a_four_k_one`;
  thresholds in
  `budget.rs::digest_thresholds_on_the_default_route_are_todays`,
  `budget.rs::digest_thresholds_scale_with_the_pair_under_the_ceiling`,
  `context.rs::the_default_routes_digest_thresholds_are_byte_identical_to_today`
  and
  `context.rs::a_dense_result_is_digested_on_a_128k_route_while_prose_is_not`;
  the 200-block fold in
  `compact.rs::a_two_hundred_block_conversation_still_fits_the_duty_prompt`
  and end to end in
  `context_pressure.rs::a_two_hundred_block_conversation_on_a_big_route_compacts_through_the_local_binding`;
  the REQ-561 fallback still pinned by
  `context.rs::engine_failure_falls_back_to_bounded_mechanical_truncation`.)*
- [x] AC-10: A prompt that forces `truncate_to_budget` to drop three blocks
  emits one `context_pressure { kind: blocks_dropped, dropped_blocks: 3,
  budget_tokens, bound }`; a single oversized user block that is middle-elided
  emits `{ kind: block_elided, elided_bytes }` **and** the turn output carries
  a one-line notice; the in-prompt elision marker on a remote route names
  that route's window, not "local"; the CLI renders each as one line;
  removing either emission fails its test. (daemon unit + `cli_e2e`; BR-7)
  *(`context.rs::a_gate_that_drops_three_blocks_reports_three_blocks`,
  `context.rs::an_oversized_newest_user_block_reports_the_bytes_it_lost`,
  `context.rs::an_oversized_tool_result_is_elided_without_claiming_the_user_was`,
  `context.rs::the_elision_marker_names_the_routes_own_window`; emissions
  end to end in
  `context_pressure.rs::three_dropped_blocks_are_one_event_naming_all_three`
  and
  `context_pressure.rs::an_elided_newest_user_message_is_an_event_and_a_notice_in_the_turns_output`;
  one CLI line per shape in
  `session_ui.rs::a_context_pressure_event_is_never_silent` and
  `session_ui.rs::each_pressure_shape_names_the_budget_and_its_bound`.)*
- [x] AC-11: A session carries a 30,000-word conversation assembled on a
  128k route; the next turn routes local; the retained blocks replay, the
  oldest are dropped to fit with a `context_pressure` event, the turn
  completes, and the session's retained conversation afterwards is what the
  local turn kept (REQ-567 BR-6's atomic commit). (integration, `carry.rs`;
  BR-10)
  *(`conversation_carry.rs::a_conversation_assembled_on_a_128k_route_survives_a_local_turns_smaller_budget`
  — the replay, the drop with its event, the completed turn, and the
  retained conversation afterwards.)*
- [x] AC-12: The `bound` value is computed in exactly one function at route
  time and every surface that prints it reads the same value; a mutation
  that changes the function's answer changes all of them in one test. (unit;
  BR-8)
  *(`router.rs::the_budget_the_event_reports_is_the_budget_the_turn_runs_under`
  — one derivation at route time, read by the event, the harness and the
  marker; TASK-192's mutation (a) confirms a change to `derive` moves every
  surface together.)*
- [x] AC-13: `cargo test --workspace --no-fail-fast` green; the two
  prompt-margin tests still measure the default (local) shape and stay green
  without moving the overhead ceiling; the redact arithmetic comment and its
  test are updated together. (BR-11)
  *(TASK-192's sweep — `cargo fmt --all --check`, `cargo clippy --workspace
  --all-targets -- -D warnings` and `cargo test --workspace --no-fail-fast`
  all green: 3,159 passed / 0 failed / 1 ignored across 59 targets, no
  `FAILED` in the log (LESSON-533).
  `redact.rs::the_total_cap_clears_the_harness_context_budget_with_margin`
  and `web.rs::the_web_tool_docs_clear_the_outbound_body_overhead` measure
  the default (local) shape and pass with `REDACT_BODY_OVERHEAD_BYTES`
  unmoved at 10 KiB.)*
- [ ] AC-14: **Dogfood, by hand, recorded in `docs/manual-verification.md`:**
  with the user's Kimi provider given `max_context = 128000` (through the new
  surface, not a hand edit), a 6,000-word pasted prompt on the `build` tier
  reaches Kimi whole (verify in `/verbose` and the cost row's input tokens);
  `/doctor` shows the window; with `redact = true` the same prompt shows
  `bound: redact_scan` and completes; the runbook records the worst-case
  per-prompt input at that budget; and once REQ-585 lands, `/proceed REQ-xxx`
  expands rather than being refused for size. (manual; BR-3, BR-4, BR-9)
- [x] AC-15: **Mid-turn reroute re-fits.** On a 128k route, a turn with a
  60,000-word context hits a privacy block and is rerouted to the local pin:
  the context is re-fitted to the local budget with `context_pressure { kind:
  refit_on_reroute }` **before** the next model call and the turn completes —
  no over-window error; the same for a provider failure failing over to a
  fallback provider with a smaller declared window — no 400; and a
  `MalformedToolCall` on a 128k route continues under the reduced profile
  with the same `budget_tokens` and `bound: window`, with no
  `refit_on_reroute`. (remote-loop fixture; BR-1)
  *(`privacy_fixes.rs::a_128k_turn_blocked_by_privacy_is_refitted_before_the_local_pin_serves_it`
  and `ac_matrix.rs::ac7_degraded_provider_falls_back_and_completes` — each
  red when its own reroute arm skips the refit (TASK-192 mutation (c)); unit
  pins
  `runtime.rs::a_reroute_to_a_smaller_window_refits_the_context_and_publishes_it`
  and
  `runtime.rs::a_degrade_that_keeps_the_window_refits_nothing_and_says_nothing`;
  the silent-degrade half in
  `router.rs::a_degrade_keeps_the_failed_providers_budget`.)*

## External Dependencies

- One new **dev-dependency** (a reference tokenizer) or a committed
  token-count fixture for AC-3; the harness keeps its whitespace estimator at
  runtime.
- An **additive protocol change**: the wire `ProviderConfig` (or
  `ConfigUpdate`) gains `max_context` / `context_budget_cap` so clients can
  set and show a window (BR-3, AC-4/AC-5) — REQ-573's additive rule, older
  peers degrade.
- Sequencing: REQ-585 depends on this REQ (its BR-8 and success bar); this
  REQ depends on nothing in flight. REQ-584 (spec PR #185) is unrelated.

## Assumptions

- `HarnessConfig` is built per route in `router.rs` — every resolvable route,
  local included, through `from_harness_profile`; `default()` only for an
  unresolvable route — and the `ContextManager` is seeded per turn in
  `CarriedTurn::begin` from the first attempt's config; the `ContextManager`
  has no budget setter after construction (only the builder), which is why
  BR-1 names the reroute paths explicitly.
- The estimator is whitespace-word count (`estimated_tokens`) with
  `APPROX_BYTES_PER_TOKEN = 8` for the default byte pair, whose doc describes
  the byte budget as the dense-content guard at ≈2 B/BPE-token against the
  16,384-token local window; BR-2 keeps that role for the byte guard on
  remote routes. A word ratio around 1.5 and a byte floor around 2 are the
  working assumptions; AC-3 decides both.
- `max_context` is a config field (`[providers.<id>.capabilities]`) that
  round-trips through `config_doc`; the adapters' constants (Anthropic
  200,000; OpenAI-compatible 0) are overridden by the config value at
  construction, and the config entity defaults to 0 — so on the dogfood
  machine *both* the Kimi record and any Anthropic record are unknown today.
  Nothing in the CLI, the recipes or the RPCs sets or shows it; the wire
  `ProviderConfig` has no capabilities field, and `RegisterProvider`
  deliberately preserves/defaults capabilities ("not settable over this
  RPC").
- `[privacy] redact` defaults to `false`; the provider-body scan runs only
  when it is `true` and covers the whole outbound request body; the search
  tier's scan is a separate slot over lookup egress. The scannable bound with
  today's constants is ≈ 89 KB (cap minus overhead, escaping modelled) — ≈
  2.7× today's budget, up to five chunks at the cap — which admits every
  ADLC skill; `REDACT_BODY_OVERHEAD_BYTES` is `#[cfg(test)]` today.
- Truncation is user-invisible today: `was_truncated()` is read only by
  tests (several, across `carry.rs`, `sessions.rs`, `turn_loop.rs`,
  `context.rs`), and `ContextCleared` (for `/clear` and `/cd`) is the only
  context event; REQ-561's "for the event surface" compaction outcome never
  shipped as an event. The model-facing note and marker exist and the marker
  names the local window unconditionally.
- Compaction already fires at `COMPACT_PRESSURE_PERCENT` = 70% of either
  budget; `summarize_threshold_tokens` = 1,500 is the `digest` duty's
  per-tool-result threshold (REQ-558), used only at the tool-result fold.
- There is no spend cap or `[cost]` table; the loop iteration cap is 25
  (`Native`) / 40 (`for_strong_model`) / fewer on degraded profiles; the
  context budget is the only per-turn input-token bound.
- The local tier's budget is left at today's default on purpose; the
  engine's window (`LOCAL_ENGINE_N_CTX` = 16,384) versus the 4,096-word
  budget is a separate question (OQ-3) with prefix-cache and
  prompt-processing latency trade-offs REQ-564 measured.
- REQ id allocated with remote verification (`ADLC_ALLOC_DEGRADED=0`,
  2026-08-19).

## Open Questions

- [x] OQ-1: **How does a user declare a window?** `/provider add
  --max-context <n>` and a `/provider setup` question (with the recipe's
  value as the default) are the obvious shapes; `config/set` cannot reach
  capabilities today (no wire field, no `ConfigUpdate` variant) and gains the
  ability only through the additive change in BR-3. *Lean:* recipes carry
  it, `/provider setup` records it from the recipe, `/provider add` takes a
  flag, `config/set` accepts the new field, and `/doctor` nags when it is
  zero.
  **Resolved as the lean, all four surfaces:** `provider add --max-context
  <n> [--context-budget-cap <n>]`, `/provider setup` recording the recipe's
  window when the chosen model is that recipe's example model, and
  `config/set` carrying both keys through the same gate every provider-record
  write meets. `/doctor` advises on a window of zero and on a cap that cannot
  bind (AC-4).
- [ ] OQ-2: **Should the redact chunk cap scale so an opted-in machine can
  use a big window past ≈89 KB?** More chunks = more local model calls per
  send (latency; the scan as a whole is bounded at one `DUTY_DEADLINE`, so
  past a point more chunks just time out and block). *Lean:* not in this REQ
  — name the bound, measure the chunk-count distribution in the runbook, and
  spec a scan-latency budget separately if dogfood wants it.
- [ ] OQ-3: **Derive the local budget from the engine's `n_ctx`?** *Lean:*
  not here — REQ-564's prefix-cache work and the local prompt-processing
  cost make that a measured decision; keep the default and revisit.
  **One fact for whoever measures it:** the local pair is the only pair that
  does not subtract the generation reservation before applying the 2 B/token
  floor. `LOCAL_BUDGET_BYTES` is 32,768, which is `LOCAL_ENGINE_N_CTX`
  (16,384) × 2 exactly — the *whole* engine window, with no room left for the
  reply — where a remote pair takes the reservation off the window first
  (BR-2). The word half (4,096) is well under it, so nothing overflows today;
  the point is that the byte half is not conservative in the way the remote
  halves are, and a measured decision should start from that rather than
  from the assumption that the two derivations agree.
- [ ] OQ-4: **Refuse or elide a typed oversized prompt?** This REQ makes the
  elision loud; REQ-585 refuses for skill turns. *Lean:* keep eliding typed
  prompts (a pasted log should not fail the turn) and revisit if the notice
  proves annoying.
- [ ] OQ-5: **Status line?** *Lean (resolved toward BR-9):* unchanged;
  `/verbose` only. The two surprising bounds (`redact_scan`,
  `default_unknown`) are already named in `/doctor` and in every refusal.
- [ ] OQ-6: **Should recipes carry a recommended `context_budget_cap`** (a
  cost default below the window, e.g. 64k) so a 200k model does not spend
  200k per iteration by default? *Lean:* no — the window the user declared
  is the consent, and a default cap is a surprise in the other direction;
  state the worst case in the docs, make the cap one line, and spec a spend
  cap (Deferred) if dogfood bills say so.
- [x] OQ-7: **Should the `digest` threshold have an absolute ceiling on big
  routes** (e.g. never carry a >20k-word single tool result raw, even on
  200k)? *Lean:* yes — an **absolute** word/byte ceiling chosen against the
  AC-3 corpus (never more than N words raw on any route), on top of BR-6's
  proportional rule; decide N in `/architect` with the corpus.
  **Resolved as the lean.** `DIGEST_ABSOLUTE_CEILING_TOKENS` = 20,000 words
  and `DIGEST_ABSOLUTE_CEILING_BYTES` = 160 KiB, applied with `.min()` on top
  of the proportional rule, so a 1m-token route's threshold stops climbing
  where a raw fold stops being worth its place in the window.

## Deferred (found in implementation)

- **`ContextPressureKind` is not forward-compatible.** It is a plain
  snake_case serde enum, so a client that does not know a kind refuses the
  frame rather than degrading — a *lost* pressure line, not a mis-rendered
  one. No released client is affected: `context_pressure` and every kind it
  carries (including `did_not_fit`) ship in the same release, and a client
  predating the event drops the whole envelope at `classify` as it always
  did. It matters the day a *sixth* kind is added to a shipped enum. The
  honest fix is a custom `Deserialize` with an `Unknown(String)` catch-all
  (the shape `Event` itself would want too); pinned meanwhile by
  `a_context_that_did_not_fit_has_its_own_kind_and_degrades_both_ways`
  (TASK-194).

- **A clamped newest-user block can be dropped by the same turn's exit gate.**
  Observed in TASK-193's AC-10 fixture: the in-place clamp fills the byte
  budget exactly, so appending the model's reply makes the exit gate drop the
  now-oldest block — which is the user's own message. The answer is retained
  without the question. This is existing REQ-561/REQ-567 behaviour, and this
  REQ makes it **honest** (two `context_pressure` events, the second naming
  the drop) rather than silent, which is BR-7's whole claim; making it not
  *happen* — reserving room for the reply before clamping, or refusing the
  turn as an oversized skill turn is refused — is a follow-up REQ, not this
  one.

## Out of Scope

- Changing the local tier's budget or the prefix-cache behaviour (OQ-3).
- Raising `REDACT_TOTAL_CAP_CHUNKS` / a scan-latency budget (OQ-2).
- Refusing (rather than loudly eliding) typed oversized prompts (OQ-4).
- A spend cap, prompt caching / cached-input pricing on remote providers,
  or any change to cost attribution or pricing (Deferred).
- A new runtime estimator (tokenizer in the daemon); the whitespace
  estimator stays, the two pinned allowances cover it.
- REQ-585 itself (skills) — this REQ only makes the budget it needs true.

## Deferred

- A per-session or per-turn **spend cap** (the cost guard this REQ names
  as missing; OQ-6).
- Scan-latency budget / chunk-cap scaling for opted-in machines (OQ-2).
- Engine-derived local budget (OQ-3).
- `docs/manual-verification.md` REQ-586 runbook (AC-14) — needs a release
  and the user's Kimi record updated through the new surface.

## Validation

`/validate` ran 2026-08-19 on the first draft: 0 Blockers, 8 Warnings, 6
Info; all applied in this revision — BR-1/AC-15 per-attempt budget with
mid-turn reroute re-fit (F-1); BR-6/AC-9 corrected to `COMPACT_PRESSURE_PERCENT`
with the `digest` threshold as the new knob and OQ-7 (F-2); BR-4/AC-6 drop
the search-tier claim and state the whole-body scan (F-3); Description/BR-3/
AC-4/AC-5/External Dependencies state that every record is unknown today
(Anthropic included) and that setting/showing a window is an additive
protocol change (F-4); BR-2/AC-3 derive the byte budget from the window with
its own floor, assert the combined estimate, and keep the typed outcome from
degrading provider health (F-5); Description/BR-9/OQ-6/Deferred state that
this budget is the only per-turn input-token bound and quantify the worst
case (F-6); BR-4/AC-7 choose cap-minus-overhead (≈89 KB) with escaping
modelled and the overhead constant promoted (F-7); Events/AC-4 ride
`route_decided` for the per-turn line (F-8); BR-7/AC-10 name the right window
in the in-prompt marker (F-9); BR-8/AC-1 classify `local_engine` from the
routing table (F-10); Assumptions corrected on `was_truncated` readers
(F-11); External Dependencies name the dev-dependency (F-12); OQ-5 resolved
toward BR-9 (F-13); REQ-585 consistency confirmed (F-14).

`/proceed` Phase 1 re-validation (2026-08-19): APPROVED; 3 Warnings + 5 Info
applied before `/architect` — the `Degrade` re-config arm keeps the provider's
budget (BR-1/AC-15); the `digest` byte twin scales and the `compact` duty's
own prompt stays bounded to its route's window (BR-6/AC-9); five chunks at
the cap, the ≈89 KB bound is a floor and byte-denominated (BR-4); AC-14 BR
tags; OQ-7 lean is an absolute ceiling.

## Retrieved Context

- REQ-567 (spec, score 14): Cross-prompt conversation carry in interactive sessions
- REQ-583 (spec, score 13): Session-root awareness and bounded discovery — the agent knows where it is, the user is told when it is nowhere, and a search cannot become a disk crawl
- LESSON-532 (lesson, score 11): Presence in context is not instruction-following — a small model transfers data, not directives
- REQ-563 (spec, score 11): Opt-in web lookup through the egress choke point
- REQ-561 (spec, score 10): Wire the four unreached categories: triage, shell, title, compact
- LESSON-543 (lesson, score 9): A model answers 'can you do X?' from whatever is in front of it — every class of question a user asks about the product needs its own resident fact
- BUG-181 (bug, score 9): The model affirms capabilities Teton does not have
- BUG-176 (bug, score 9): The shipped guide told users to put a live API key on the command line
- LESSON-495 (lesson, score 9): A remembered grant answers every question its key matches — so the key must encode the whole question
- LESSON-496 (lesson, score 9): "Cut first under pressure" means "never available" when the limit equals the count
- LESSON-539 (lesson, score 8): Claim first, then re-read — session state snapshotted before the turn claim is stale by construction
- LESSON-540 (lesson, score 8): A fixture that names "the first listed entry" or writes stdin after spawn is a platform test
- BUG-180 (bug, score 8): A remote provider's text-form tool call ends the turn silently
- LESSON-524 (lesson, score 8): Exposure is not callability — a capability asserted present must be asserted usable at every permission level
- LESSON-515 (lesson, score 8): A feature-gated target is invisible to every refactor

(Spec filter admitted `status: complete`, this repo's terminal status. The
delegate's body-read returned 8 of 15 blocks; REQ-561, LESSON-496, LESSON-539,
LESSON-540, LESSON-524, LESSON-515 and BUG-180 were read directly or were
already in this session's context. REQ-567 BR-4 and REQ-561 BR-4/BR-4a were
read verbatim as load-bearing. The code facts in Assumptions were verified on
2026-08-19 — `turn_loop.rs` (`HarnessConfig`, `summarize_threshold_tokens`),
`router.rs` (`from_harness_profile` on every route), `carry.rs`
(`CarriedTurn::begin`), `runtime.rs` (reroute/fallback paths, adapter
capabilities from config, `RegisterProvider`), `context.rs`
(`truncate_to_budget`, `estimated_tokens`, `APPROX_BYTES_PER_TOKEN`, the
model-facing marker), `compact.rs` (`COMPACT_PRESSURE_PERCENT`),
`teton-protocol/methods.rs` (wire `ProviderConfig`), `teton-core/config.rs`
(`PrivacyConfig::redact`), `egress/mod.rs` (scan slots) and
`egress/redact.rs` (`REDACT_TOTAL_CAP_CHUNKS` derivation, `#[cfg(test)]`
overhead) — partly by the `/validate` pass and spot-checked here.)
