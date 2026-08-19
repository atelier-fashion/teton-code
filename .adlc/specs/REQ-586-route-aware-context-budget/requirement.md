---
id: REQ-586
title: "A turn's context budget follows its route — remote tiers get the provider's window, bounded by what the redact scan can cover, and nothing is clamped in silence"
status: draft
deployable: true
created: 2026-08-19
updated: 2026-08-19
component: "daemon/harness"
domain: "harness"
stack: ["rust", "daemon", "llm-providers"]
concerns: ["cost", "privacy", "reliability", "developer-experience"]
tags: ["context-budget", "context-window", "max_context", "truncate_to_budget", "compaction", "summarization", "redact", "chunking", "routing", "route", "prompt-size", "silent-truncation", "over-window", "egress", "capability-profile", "harness-config", "carry", "skills", "automation", "adlc"]
---

## Description

Every turn the harness runs — local or remote — is assembled under **one**
context budget: `HarnessConfig::default()` sets `context_budget_tokens =
4_096` (whitespace-separated words, the estimator's unit) and
`context_budget_bytes = 32_768`, the system prompt is charged against it, and
`HarnessConfig::from_harness_profile` — the constructor every remote route
goes through — copies those two numbers from the default. The provider's
declared window (`capabilities.max_context`, 200,000 for Anthropic by default,
`128000` when a user writes it into an OpenAI-compatible record, `0` =
unknown otherwise) never reaches the harness. So a turn routed to a
200k-token frontier model is assembled in a window sized for the local
engine, and when a prompt does not fit, `ContextManager::truncate_to_budget`
drops the oldest blocks and then **middle-elides the last block in place** —
with no event, no notice, and nothing in `/verbose`: the model answers a
prompt the user did not send and nobody is told.

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
three places that have nothing to do with skills: the conversation carry
(REQ-567 BR-4: the budget spans the session and compaction, not failure, is
the response to pressure), the `compact` duty's soft threshold and
`truncate_to_budget`'s hard backstop (REQ-561 BR-4/BR-4a), and the redact
scan's total cap, whose chunk arithmetic is **derived from**
`context_budget_bytes` (`REDACT_TOTAL_CAP_CHUNKS`: 2 × (32,768 + 10,240) ÷
27,070 → 4 chunks → `REDACT_INPUT_MAX_BYTES` = 108,280, the largest payload the
redactor will scan at all). Raise the budget without re-deriving that, and a
long remote turn on a machine that opted into the scan is blocked as
"unscannable" — the exact collision REQ-562/REQ-577 closed.

What this REQ does: **a turn's context budget derives from the route that
serves it.** A remote route's budget comes from its provider's window minus
the turn's generation reservation and a stated safety ratio for the
estimator (whitespace words undercount subword tokens); an unknown window
(`max_context = 0`) keeps today's default, and says so where the user can see
it; the local tier keeps its engine-derived budget (unchanged here, OQ-3).
When the redact scan applies to the route's egress, the budget is further
bounded by what the scan can cover, and that bound is **named** rather than
discovered as a blocked turn. The `compact` soft threshold scales with the
budget so compaction stays "early, not at the wall". And no turn is clamped
in silence any more: dropping blocks or eliding a block is an event and a
line, on every tier — which is the surface REQ-585 BR-8's refusal for
oversized skill turns stands on.

Why this is a cost feature and not only a capacity one: the product promise
is that the user *chooses* what a frontier model sees. A bigger budget is
more input tokens per remote turn, and the user made that choice when they
wrote the window into the provider record; what this REQ owes them is that
the budget in effect is visible (`/verbose`, `/doctor`, the refusal text),
that a provider with an unknown window is not silently stuck at 4,096, and
that an optional per-provider cap exists for the user who wants a 200k model
held to 40k of context. The cost meter already prices every token that
leaves; this REQ does not change attribution, it changes how many tokens
there can be.

## System Model

_Shapes are illustrative; the constraints are the requirement._

### Entities

| Entity | Field | Type | Constraints |
|--------|-------|------|-------------|
| ProviderCapabilities (existing) | max_context | u32 tokens | `0` = unknown; settable today only by editing `[providers.<id>.capabilities]`; Anthropic adapter default 200,000; OpenAI-compatible default 0 |
| ProviderCapabilities | context_budget_cap | u32 tokens? (new, optional) | a user ceiling below the window; when set, the effective budget is `min(window-derived, cap)` |
| RouteBudget (new, derived per turn) | budget_tokens, budget_bytes | usize | the `HarnessConfig` pair for this turn; derived from the route's provider window (remote) or unchanged (local / unknown) |
| RouteBudget | generation_reservation | u32 tokens | the turn's `max_tokens`; subtracted from the window before the safety ratio |
| RouteBudget | safety_ratio | fixed constant | the words→tokens undercount allowance the estimator needs (stated, pinned, measured against the corpus) |
| RouteBudget | bound | `window` / `default_unknown` / `redact_scan` / `user_cap` / `local_engine` | **which constraint bound the budget** — the one fact `/verbose`, `/doctor` and a refusal name |
| RouteBudget | soft_threshold_tokens | usize | the `compact` duty's trigger, a fixed fraction of `budget_tokens` (today 1,500 of 4,096 ≈ 37%) |
| ContextPressure (new event payload) | kind | `blocks_dropped` / `block_elided` / `compacted` | what the gate did |
| ContextPressure | dropped_blocks, elided_bytes, budget_tokens, bound | usize, usize, usize, enum | enough for a client to render one honest line |

### Events

| Event | Trigger | Payload |
|-------|---------|---------|
| `context_pressure` (new, additive) | `truncate_to_budget` dropped ≥1 block or elided a block in place; or the `compact` duty ran | kind, dropped_blocks, elided_bytes, budget_tokens, bound, session_id |

Older clients ignore an unknown event (the REQ-573 additive rule); the CLI
renders it as one line (`context: 3 older blocks dropped to fit the 4,096-word
budget (bound: local engine)`) and `/verbose` adds the numbers.

### Permissions

| Action | Roles Allowed |
|--------|---------------|
| set `max_context` / `context_budget_cap` on a provider | the user, through the same `config/set` gate every provider-record write meets (presence-attested where the build has it — REQ-576 BR-10(b)); `/provider add` and `/provider setup` may collect a window (OQ-1) |
| a remote turn sending up to the derived budget | automatic, once the user has declared the window — the declaration is the consent; egress, boundaries and taint are untouched |
| the model changing its own budget | never |

## Business Rules

- [ ] BR-1: **The budget is a property of the route, derived per turn.** The
  `HarnessConfig` a turn runs under carries a `(budget_tokens, budget_bytes)`
  pair derived from the route's provider: for a remote provider with a
  declared window, from that window; for a remote provider with an unknown
  window (`max_context = 0`), today's default pair, unchanged; for the local
  tier, today's default pair, unchanged (OQ-3). `CarriedTurn::begin` seeds
  the turn's `ContextManager` from that pair exactly as it does today, so the
  conversation carry, compaction and the prefix cache see a budget, not a
  new mechanism (informed by REQ-567 BR-4, REQ-561 BR-4a).
- [ ] BR-2: **A remote budget never exceeds what the provider will accept.**
  The derivation is `budget_tokens = (max_context − generation_reservation) /
  safety_ratio` with `budget_bytes` scaled by the same `APPROX_BYTES_PER_TOKEN`
  rule as today, where the safety ratio is a pinned constant chosen so that
  the estimator's whitespace-word count, which undercounts subword tokens —
  more so for code, JSON and paths than for prose — cannot assemble a prompt
  the window rejects. The ratio is measured, not guessed: an AC pins it
  against a corpus of real turns (prose, code, tool results) tokenized with a
  reference tokenizer. A provider "context length exceeded" response is a
  **typed outcome** named as such (not a generic turn error), and it does not
  retry in a loop (informed by REQ-581's typed-outcome rule).
- [ ] BR-3: **An unknown window is stated, not silently defaulted.** A remote
  provider with `max_context = 0` runs under the default budget, and the fact
  is visible: `/doctor` and `/provider list` name the window as unknown and
  say the budget is defaulted, `/verbose` names the bound on every turn, and
  the BR-7 refusal text says "set `capabilities.max_context` for `<id>`".
  The shipped vendor recipes (REQ-577) carry the window for their example
  models so `/provider setup` records one; `/provider add` and `config/set`
  accept one (OQ-1 on the flag shape). No window is ever *guessed* from a
  model name outside the recipes (informed by LESSON-496 — a capability the
  user enabled must not be withheld without a voice; REQ-577).
- [ ] BR-4: **When the redact scan applies to the route, the budget is
  bounded by what the scan can cover — and the bound is named.** A route
  whose egress runs the model redact scan (`[privacy] redact = true`, or the
  search tier's hard-coupled scan, REQ-563 BR-14) cannot assemble a body the
  redactor refuses as unscannable (`REDACT_INPUT_MAX_BYTES`, fail-closed →
  Block). The effective budget on such a route is therefore
  `min(window-derived, scannable)`, where `scannable` is derived from the
  same constants the chunk cap is — stated in the spec's arithmetic, pinned
  by a test beside `REDACT_TOTAL_CAP_CHUNKS`, never a second hand-copied
  number (LESSON-456). With today's constants that is roughly 44 KB of
  context: an opted-in machine gains little until the chunk cap is revisited,
  and that is said in `/verbose` (`bound: redact scan`) and in the docs rather
  than met as a blocked turn. Raising the chunk cap is not this REQ (OQ-2)
  (informed by REQ-563 BR-2/BR-14, REQ-567).
- [ ] BR-5: **An optional per-provider cap holds a big window to a smaller
  budget.** `[providers.<id>.capabilities] context_budget_cap` (name
  illustrative), when set, bounds the derived budget; it is the cost knob for
  a user who wants a 200k model with 40k of context, and it is the only new
  config field. Absent, the window the user declared is the cap — declaring
  the window is the consent to spend it.
- [ ] BR-6: **The `compact` soft threshold scales with the budget.**
  `summarize_threshold_tokens` is derived as the same fraction of the budget
  it is today (1,500 of 4,096 ≈ 37%), so compaction stays "early, not at the
  wall" on a 100k budget exactly as on a 4k one, and `truncate_to_budget`
  stays the unconditional hard backstop at 100% (REQ-561 BR-4a is unchanged
  in kind). A `compact` duty that is itself routed remote is subject to the
  same egress rules as today; this REQ changes how much it summarizes, not
  where (informed by REQ-561 BR-4/BR-4a, REQ-567 BR-4).
- [ ] BR-7: **Nothing is clamped in silence, on any tier.** When
  `truncate_to_budget` drops blocks or elides a block in place, the daemon
  emits a `context_pressure` event and the CLI renders one line naming what
  happened, the budget and its bound; `/verbose` shows the numbers. An
  in-place elision of the **newest user block** — the case where the model
  would answer a prompt the user did not send — is additionally reported in
  the turn's own output as a notice, not only as an event. This is the
  surface REQ-585 BR-8 builds its skill-turn refusal on; for typed prompts the
  elision still happens (changing that is OQ-4) but it is never silent
  (informed by REQ-567 BR-4 "never silently", REQ-561 BR-4, LESSON-543).
- [ ] BR-8: **The bound is one fact with one source.** Which constraint bound
  the budget — `window`, `default_unknown`, `redact_scan`, `user_cap`,
  `local_engine` — is computed once per turn and is what `/verbose`,
  `/doctor`, the `context_pressure` event and every refusal text read; no
  surface re-derives it (LESSON-456: one classifier per fact).
- [ ] BR-9: **Cost attribution is unchanged and the budget is visible.**
  Every token that leaves is priced as today; `/cost` rows are unchanged in
  shape. `/verbose` prints the budget in effect and its bound once per turn;
  the status line is unchanged (OQ-5).
- [ ] BR-10: **A route change mid-session is a budget change, and the carry
  survives it.** A session whose retained conversation was assembled under a
  100k budget and whose next turn routes to a 4k tier replays the retained
  blocks and `truncate_to_budget` drops the oldest to fit — exactly today's
  rule — with a `context_pressure` event saying so; the reverse direction
  simply has more room. The prefix cache on the local tier is unaffected
  (REQ-564/REQ-567 BR-7: carry correctness is independent of KV state)
  (informed by REQ-567 BR-4/BR-7).
- [ ] BR-11: **The redact chunk arithmetic is re-derived where it lives, not
  restated.** Any change this REQ makes to `context_budget_bytes` as a
  *default* (none intended) or to the scannable bound is written into the
  `REDACT_TOTAL_CAP_CHUNKS` derivation comment and its test, the way REQ-577
  and BUG-181 did; the margin test `the_total_cap_clears_the_harness_context_budget_with_margin`
  keeps measuring the **default** budget (the local shape), and a second
  assertion pins that the scannable bound and the cap agree (informed by
  LESSON-456).

## Acceptance Criteria

- [ ] AC-1: A route to a provider with `max_context = 128000` and a 1,024
  generation reservation yields a `HarnessConfig` whose `budget_tokens` is
  `(128000 − 1024) / safety_ratio` and `budget_bytes` the scaled pair; a
  route to a provider with `max_context = 0` yields today's `(4096, 32768)`;
  the local route yields today's pair. (unit, `router.rs`; BR-1)
- [ ] AC-2: With a 128k provider bound to the `think` tier, a prompt of
  20,000 words is assembled whole — no blocks dropped, no elision — and
  reaches the provider in one request; the same prompt on the local tier is
  clamped with a `context_pressure` event. (daemon unit + remote-loop
  fixture; BR-1, BR-7)
- [ ] AC-3: The safety ratio is pinned by a test that tokenizes a fixture
  corpus (prose, Rust source, JSON tool results, path-heavy shell output)
  with a reference tokenizer and asserts `tokens ≤ words × safety_ratio` for
  every sample; a provider `context_length_exceeded`-class response surfaces
  as a typed outcome naming the window and the assembled size, with no retry.
  (unit + remote-loop fixture; BR-2)
- [ ] AC-4: `/doctor` and `/provider list` say `window: unknown — context
  budget defaulted (set capabilities.max_context)` for a provider with
  `max_context = 0`, and `window: 128k` otherwise; `/verbose` prints `context
  budget: N words (bound: default_unknown)` on a turn routed there. (`cli_e2e`;
  BR-3, BR-8, BR-9)
- [ ] AC-5: Every shipped vendor recipe carries a window for its example
  model, `/provider setup` writes it into the record, and a contract test
  enumerates the recipes and asserts no window is zero; `/provider add`
  accepts it (flag per OQ-1); `config/set` accepts
  `capabilities.max_context`. (unit + `cli_e2e`; BR-3)
- [ ] AC-6: With `[privacy] redact = true` and a 128k provider, the budget in
  effect is the scannable bound, `/verbose` says `bound: redact_scan`, and a
  40,000-word prompt is compacted/clamped to fit **and then scanned
  successfully** — no turn on that route is ever blocked as "unscannable"
  because of its size; a test that removes the bound makes such a turn block.
  (egress-capture + daemon unit; BR-4)
- [ ] AC-7: The scannable bound is computed from the same constants as
  `REDACT_TOTAL_CAP_CHUNKS`; a test asserts that a body at the bound plus the
  overhead assumption fits under `REDACT_INPUT_MAX_BYTES` with the 2× margin,
  and that changing either constant alone fails it. (unit; BR-4, BR-11)
- [ ] AC-8: `context_budget_cap = 40000` on a 200k provider bounds the
  budget to the cap and `/verbose` says `bound: user_cap`; absent, the window
  binds. (unit + `cli_e2e`; BR-5)
- [ ] AC-9: On a 100k budget the `compact` duty fires at the same fraction it
  fires at on 4k (≈37%), and `truncate_to_budget` still fires at 100%; the
  REQ-561 fallback (failed compaction → deterministic truncation) is
  unchanged and pinned. (unit; BR-6)
- [ ] AC-10: A prompt that forces `truncate_to_budget` to drop three blocks
  emits one `context_pressure { kind: blocks_dropped, dropped_blocks: 3,
  budget_tokens, bound }`; a single oversized user block that is middle-elided
  emits `{ kind: block_elided, elided_bytes }` **and** the turn output carries
  a one-line notice; the CLI renders each as one line; removing either
  emission fails its test. (daemon unit + `cli_e2e`; BR-7)
- [ ] AC-11: A session carries a 30,000-word conversation assembled on a
  128k route; the next turn routes local; the retained blocks replay, the
  oldest are dropped to fit with a `context_pressure` event, the turn
  completes, and the session's retained conversation afterwards is what the
  local turn kept (REQ-567 BR-6's atomic commit). (integration, `carry.rs`;
  BR-10)
- [ ] AC-12: The `bound` value is computed in exactly one function and every
  surface that prints it calls that function; a mutation that changes the
  function's answer changes all of them in one test. (unit; BR-8)
- [ ] AC-13: `cargo test --workspace --no-fail-fast` green; the two
  prompt-margin tests still measure the default (local) shape and stay green
  without moving the overhead ceiling; the redact arithmetic comment and its
  test are updated together. (BR-11)
- [ ] AC-14: **Dogfood, by hand, recorded in `docs/manual-verification.md`:**
  with the user's Kimi provider given `max_context = 128000`, a 6,000-word
  pasted prompt on the `build` tier reaches Kimi whole (verify in `/verbose`
  and the cost row's input tokens); `/doctor` shows the window; with `redact
  = true` the same prompt shows `bound: redact_scan` and completes; and once
  REQ-585 lands, `/proceed REQ-xxx` expands rather than being refused for
  size. (manual)

## External Dependencies

- None new. A reference tokenizer for AC-3 is a **dev-dependency** (or a
  checked-in token-count fixture produced once), not a runtime one — the
  harness keeps its whitespace estimator.
- Sequencing: REQ-585 depends on this REQ (its BR-8 and success bar); this
  REQ depends on nothing in flight. REQ-584 (spec PR #185) is unrelated.

## Assumptions

- `HarnessConfig` is built per route in `router.rs` (`from_harness_profile`
  for remote, `default()` for local) and the `ContextManager` is seeded per
  turn in `CarriedTurn::begin`; so a per-route budget is a change to the
  derivation, not to the carry or the loop.
- The estimator is whitespace-word count (`estimated_tokens`) with
  `APPROX_BYTES_PER_TOKEN = 8` for the byte pair; the safety ratio is the
  only new constant and is pinned by AC-3. A ratio around 1.5 is the working
  assumption for mixed prose/code; AC-3 decides.
- `max_context` is already a config field (`[providers.<id>.capabilities]`)
  and round-trips through `config_doc`; nothing today sets it except a hand
  edit, the Anthropic adapter's 200,000 default, and tests. The user's Kimi
  record on the dogfood machine almost certainly has it unset — AC-14 sets
  it.
- `[privacy] redact` defaults to `false`, so BR-4's bound binds only
  opted-in machines; the scannable bound with today's constants is ≈ 44 KB
  (`REDACT_INPUT_MAX_BYTES / 2 − REDACT_BODY_OVERHEAD_BYTES`), about 1.3×
  today's budget — stated so nobody expects the scan and a 128k context to
  coexist until OQ-2 is taken up.
- Truncation is invisible today: `was_truncated()` is read by one test and
  no event or notice exists for dropping or eliding (`ContextCleared` is the
  only context event, and it is for `/clear` and `/cd`).
- The local tier's budget is left at today's default on purpose; the
  engine's window (16,384 tokens per the docs-tool comment) versus the
  4,096-word budget is a separate question (OQ-3) with prefix-cache and
  prompt-processing latency trade-offs REQ-564 measured.
- REQ id allocated with remote verification (`ADLC_ALLOC_DEGRADED=0`,
  2026-08-19).

## Open Questions

- [ ] OQ-1: **How does a user declare a window?** `/provider add
  --max-context <n>` and a `/provider setup` question (with the recipe's
  value as the default) are the obvious shapes; `config/set
  providers.<id>.capabilities.max_context` already works in principle.
  *Lean:* recipes carry it, `/provider setup` records it silently from the
  recipe, `/provider add` takes a flag, and `/doctor` nags when it is zero.
- [ ] OQ-2: **Should the redact chunk cap scale so an opted-in machine can
  use a big window?** More chunks = more local model calls per send
  (latency; the scan as a whole is bounded at one `DUTY_DEADLINE`, so past a
  point more chunks just time out and block). *Lean:* not in this REQ — name
  the bound, measure the chunk-count distribution in the runbook, and spec a
  scan-latency budget separately if dogfood wants it.
- [ ] OQ-3: **Derive the local budget from the engine's `n_ctx`?** *Lean:*
  not here — REQ-564's prefix-cache work and the local prompt-processing
  cost make that a measured decision; keep the default and revisit.
- [ ] OQ-4: **Refuse or elide a typed oversized prompt?** This REQ makes the
  elision loud; REQ-585 refuses for skill turns. *Lean:* keep eliding typed
  prompts (a pasted log should not fail the turn) and revisit if the notice
  proves annoying.
- [ ] OQ-5: **Status line?** Showing the budget on the status line is noise
  for most turns. *Lean:* `/verbose` only; the bound appears on the status
  line only when it is `redact_scan` or `default_unknown` (the two surprising
  cases). Confirm.

## Out of Scope

- Changing the local tier's budget or the prefix-cache behaviour (OQ-3).
- Raising `REDACT_TOTAL_CAP_CHUNKS` / a scan-latency budget (OQ-2).
- Refusing (rather than loudly eliding) typed oversized prompts (OQ-4).
- Prompt caching / cached-input pricing on remote providers; any change to
  cost attribution or pricing.
- A new estimator (tokenizer in the daemon); the whitespace estimator stays,
  the safety ratio covers it.
- REQ-585 itself (skills) — this REQ only makes the budget it needs true.

## Deferred

- Scan-latency budget / chunk-cap scaling for opted-in machines (OQ-2).
- Engine-derived local budget (OQ-3).
- `docs/manual-verification.md` REQ-586 runbook (AC-14) — needs a release
  and the user's Kimi record updated.

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
read verbatim as load-bearing. The budget, estimator, carry-seeding,
`max_context`, redact-default and truncation-visibility facts in Assumptions
were verified against the code on 2026-08-19 — `turn_loop.rs` (`HarnessConfig`),
`carry.rs` (`CarriedTurn::begin`), `context.rs` (`truncate_to_budget`,
`estimated_tokens`, `APPROX_BYTES_PER_TOKEN`), `teton-providers/capability.rs`,
`teton-core/config.rs` (`PrivacyConfig::redact`), `egress/redact.rs`
(`REDACT_TOTAL_CAP_CHUNKS` derivation).)
