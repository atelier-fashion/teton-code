---
id: REQ-590
title: "Derive the local tier's context budget from the engine's real window"
status: draft
deployable: true
created: 2026-08-24
updated: 2026-08-25
component: "daemon/router"
domain: "routing"
stack: ["rust", "daemon", "llama.cpp"]
concerns: ["routing", "latency", "cost", "reliability"]
tags: ["context-budget", "local-engine", "n_ctx", "oq-3", "generation-reservation"]
---

## Description

**This REQ discharges REQ-586's recorded OQ-3.**

REQ-586 made every route's context budget derive from that route's window facts — except the
local tier, which short-circuits in `derive` (`harness/budget.rs`) to a fixed pair of
`(LOCAL_BUDGET_TOKENS 4,096 words, LOCAL_BUDGET_BYTES 32,768 B)` regardless of what engine is
loaded or how large its window is.

REQ-589 hit the consequence in the field: `/analyze` refused at **4,097 words against 4,096**, on
a machine whose engine had a 16,384-token window loaded. REQ-589 gave that user an exit — an
offer to proceed — and deliberately did not touch the budget. This REQ is the budget.

### What the local tier gets wrong, precisely

There are **two** defects, and they point in opposite directions.

**The word half is far too small.** 4,096 whitespace words against a 16,384-token window. At the
codebase's own remote safety ratio (3 BPE tokens per 2 words), 4,096 words claim at most ~6,144
tokens — leaving roughly 10,000 tokens of the engine's window unused. A remote route with a
declared 16,384-token window would derive ~10,240 words for the same engine.

**The byte half has no generation reservation, and every neighbour it meets does.** This is the
defect REQ-586's OQ-3 gestured at, and its reasoning needs correcting before it is built on:

> OQ-3 recorded that `LOCAL_BUDGET_BYTES` is `LOCAL_ENGINE_N_CTX × 2 B/token` — the whole window.
> **It is not.** It is `LOCAL_BUDGET_TOKENS × APPROX_BYTES_PER_TOKEN` = 4,096 words × 8 B/word.
> The two derivations coincide at 32,768 by arithmetic accident.

The *conclusion* survives the corrected premise, by a different route. Converted at
`DUTY_REQUEST_BYTES_PER_TOKEN` — the 2 B/token BPE floor this codebase uses everywhere a byte
budget must be safe against dense content — 32,768 bytes is **16,384 tokens: the entire window,
with nothing left for the reply.**

Three things in this daemon disagree with that, all reachable from the same engine:

| Site | Rule |
|---|---|
| `teton-inference/src/engine.rs:103` `over_window` | refuses any prompt where `prompt_tokens > n_ctx − max_tokens` |
| `egress/redact.rs:133` `REDACT_PROMPT_BUDGET_BYTES` | `(LOCAL_ENGINE_N_CTX − 1,024) × 2 B/token`, citing LESSON-446 |
| `harness/budget.rs:118` `LOCAL_BUDGET_BYTES` | window × 2 B/token, **reservation zero** |

So the harness's local budget permits, at its own worst-case bridge, a prompt the engine will
refuse — and the refusal is correct but arrives wearing the wrong label. `harness/turn_loop.rs:390`
states the budget "keeps a full assembled prompt within the local engine's 16,384-token window
with headroom": true of typical prose at ~4 B/token, false at the 2 B/token floor the same file
names two paragraphs earlier as the reason bytes are bounded at all.

**This has not bitten because dense content is rare, not because the bound is right.** That is
LESSON-446's own failure shape — a budget, a threshold and a window that all say 4,096 in
different currencies agree on nothing — and the original `/analyze` refusal was at 4,097 *words*
against a 4,096-*word* budget.

### What this REQ does

Give the local tier a budget derived from the engine's window and a real generation reservation,
instead of a pair that ignores both.

**It is not a one-line change, and an adversarial pass on the first draft of this spec is why
this section is worded carefully.** Three things the naive version gets wrong:

- The local budget is **not only a ceiling.** It drives `under_pressure` (compaction fires at
  70% of it) and `digest_thresholds` (when a tool result gets condensed). Raising it changes what
  goes *into* prompts, not just what is refused — so REQ-586 OQ-3's cost concern is live, and the
  size of the raise is a product decision with behavioural blast radius. See D-3.
- `derive` **cannot simply stop short-circuiting.** `HarnessConfig::default()` calls
  `derive(BudgetInputs::local())` (`turn_loop.rs:493`), and `generation_reservation()` calls
  `HarnessConfig::default()` (`budget.rs:614`). The short-circuit is the only thing keeping that
  cycle open. See BR-2 and BR-3.
- Lowering the byte half is **a user-visible regression**, not a free correction — accepted, and
  absorbed by REQ-589's offer. See BR-7 and D-4.

## System Model

### Entities

| Entity | Field | Type | Constraints |
|--------|-------|------|-------------|
| `LOCAL_ENGINE_N_CTX` | — | u32 | existing (`runtime.rs:11999`); the window the daemon loads with and the one the local derivation reads |
| *(new constant)* | — | u32 | the local generation reservation; D-2. One home, reachable **without** constructing a `HarnessConfig` (BR-3) |
| `BudgetInputs` | `window`, `reservation` | u32 | already exist; `local()` currently hardcodes both to 0 |
| `RouteBudget` | `(words, bytes)` | (usize, usize) | shape unchanged |
| `BudgetBound::LocalEngine` | — | variant | retained; meaning narrows from "a fixed pair" to "derived from the local engine's window" |

*No new field on `EngineLoadReport`, and no change to the `Engine` trait — D-1 reversed. This
also removes the problem that the `ScriptedFileEngine` install path (`runtime.rs:11818`) never
builds an `EngineLoadReport` at all, so a plumbed window would have had no source on the only
local engine a default-feature build can have.*

### Events

| Event | Trigger | Payload |
|-------|---------|---------|
| `route.budget` (existing) | a route decision stamps a budget | unchanged fields; `LocalEngine` now reports a derived pair |

## Business Rules

- [ ] **BR-1: The local tier's budget is derived from the engine's window and a real generation
  reservation**, through the same arithmetic every other route uses — `(window − reservation)`,
  then the 3/2 words rule and the 2 B/token bytes rule.

- [ ] **BR-2: `derive` keeps a local branch; it does not lose one.** `HarnessConfig::default()`
  calls `derive(BudgetInputs::local())`, so simply deleting the `is_local` arm drops that call
  into the `window == 0` path and flips its bound `LocalEngine → DefaultUnknown` — which then
  renders the `capabilities.max_context` remedy BR-6 forbids. The local branch must carry the
  window and reservation into the shared arithmetic while keeping its own bound.

- [ ] **BR-3: The local reservation is a constant, not `generation_reservation()`.**
  `generation_reservation()` reads `HarnessConfig::default()`, which calls `derive`. Sourcing the
  local reservation from it closes a cycle — `derive → generation_reservation →
  HarnessConfig::default → derive` — a stack overflow in the most-constructed value in the crate.
  The reservation has one home, and that home is reachable without constructing a `HarnessConfig`.

- [ ] **BR-4: The local window is `LOCAL_ENGINE_N_CTX`, the value the daemon loads with.**
  Not plumbed from the loaded engine. See D-1 — the clamp that would have justified plumbing does
  not exist. This is the rule `egress/redact.rs:133` already follows against the same engine.

- [ ] **BR-5: The no-better-fact default keeps its constants and its other callers.**
  `LOCAL_BUDGET_TOKENS` / `LOCAL_BUDGET_BYTES` stay **the one home** (LESSON-456) of the pair a
  route with no window runs under: a remote provider declaring `max_context = 0`, and the
  unresolvable route. REQ-586 AC-1 stays true. **But see BR-2** — `HarnessConfig::default()` is
  not simply "a caller of the constants"; it is a caller of `derive`, and this REQ must say what
  bound and pair it ends up with rather than assuming it is untouched.

- [ ] **BR-6: The bound stays `LocalEngine`, and offers no `capabilities.max_context` remedy.**
  There is no provider declaration to go and edit for this route; a surface implying otherwise
  sends the user to change something that does not exist.

- [ ] **BR-7: No turn that serves today is newly refused.** Both halves are enforced
  conjunctively. Lowering the byte half 32,768 → 30,720 newly refuses byte-dense local content
  in that 2,048-byte band — minified JSON, base64, path-heavy build logs, the classes
  `budget.rs:55-62` names as measured. For an ordinary turn that is a new elision; for a skill
  turn it is a **new over-budget offer on content that has never raised one**.
  **D-4 overrides this rule deliberately**: the byte half falls, the regression is accepted, and
  REQ-589's offer is the surface that absorbs it. The rule stays stated because an override is
  only honest if the thing overridden is written down — and because AC-7 must pin the chosen
  behaviour rather than the improving direction only.

- [ ] **BR-8 (floor): `MIN_BUDGET_*` may never raise the local pair above what the engine holds.**
  The floor's "only ever raises" property is safe for a remote route with a declared window and is
  not safe against a hard engine limit. **This rule is presently latent** — at
  `LOCAL_ENGINE_N_CTX = 16,384` the floor never bites — and becomes live only if that constant
  falls. It is stated so that whoever lowers it inherits the rule rather than the bug. Note the
  remedy is *not* obviously "clamp to the window": at a 4,096-token engine the derived byte half
  is 6,144, which `budget.rs:126-133` documents as **below the smallest prompt the harness can
  produce**. That case is OQ-2, and BR-8 does not pretend to resolve it.

- [ ] **BR-9: `COMPACT_OUTPUT_MAX_BYTES` tracks the local byte budget.** It is defined as
  `LOCAL_BUDGET_BYTES` (`compact.rs:134`) and its doc states the invariant it holds: a repair may
  not return more than the budget it is repairing to. **D-4 breaks that invariant** — 32,768 would
  exceed the new 30,720 budget, so a compaction landing in the gap is rejected and the turn
  degrades to oldest-first eviction, on exactly the route that most needed the model's judgement.
  This REQ must re-point the constant at the budget it repairs to; leaving it is not an option
  the decision authorizes.

- [ ] **BR-10: The word half must not lose its slack silently.** Today 4,096 words claim at most
  ~6,144 tokens against 15,360 usable — 2.5× headroom. The derived pair sets
  `words × 3/2 = usable` **exactly**, by construction, so any content denser than 1.5 real tokens
  per whitespace word overruns the engine at full budget. `budget.rs:205-212` measures Rust at
  1.69. The byte guard covers dense-and-heavy content; it does not cover **token-dense but
  byte-light** content (whitespace-separated single-character tokens, numeric columns), which
  passes the byte guard and overruns anyway. **D-3 accepts this**: no explicit margin is added,
  `context_length_exceeded` becomes an ordinary local outcome, and REQ-589's offer plus the
  engine's typed refusal are the intended catches. AC-9 measures how wide the uncovered gap
  actually is, with a real tokenizer — the one thing that would turn this accepted risk into a
  known quantity.

- [ ] **BR-11: A larger local budget does not degrade a local turn below REQ-544's BR-8 latency
  duty** (`router.rs:405` — named by REQ id because this document has its own BR-8).

- [ ] **BR-12: The change is observable.** A user can see which window their local budget came
  from and what was reserved.

## Acceptance Criteria

- [ ] AC-1: With the local window and reservation, the local route's pair equals what the remote
  path yields for a declared window of the same size. One formula, tested from both sides.
- [ ] AC-2 (BR-2): `HarnessConfig::default().budget.bound` is still `LocalEngine`, and its pair is
  still `(4096, 32768)` **or** the value D-3 settles on — asserted explicitly, because today's
  pin (`turn_loop.rs:465-468`) compares only the numbers and would pass while the bound flipped.
- [ ] AC-3 (BR-3): A test constructs `HarnessConfig::default()` and `derive` on the local path.
  Passing is not enough — a cycle is a stack overflow, so this AC is satisfied by the test
  *existing and terminating*, and by a note at the reservation's home saying why it is not
  `generation_reservation()`.
- [ ] AC-4 (BR-1/BR-4): `budget_bytes / DUTY_REQUEST_BYTES_PER_TOKEN ≤ LOCAL_ENGINE_N_CTX −
  reservation`, as a property over the derivation. **This assertion fails against today's
  constants**, which is the point.
- [ ] AC-5 (BR-5): `max_context = 0` still yields `(4096, 32768)` (REQ-586 AC-1, unchanged), on a
  fixture that is *not* the local route — so it cannot pass by accident if the local pair happens
  to match.
- [ ] AC-6 (BR-6): The `LocalEngine` bound names the engine window as its source and offers no
  `max_context` remedy. Paired against a remote `Window` bound, which does.
- [ ] AC-7 (BR-7): A local turn of byte-dense content sized in the band between the new and old
  byte budgets. **Whichever way D-4 goes, this AC pins the chosen behaviour** — it serves, or it
  raises exactly one over-budget offer and no elision. A test that only pins the improving
  direction is what let this regression go unnoticed in the first draft.
- [ ] AC-8 (BR-9): `COMPACT_OUTPUT_MAX_BYTES ≤` the local byte budget, asserted as a relation
  between the two rather than as two literals.
- [ ] AC-9 (BR-10): A token-dense, byte-light corpus sample at full word budget, tokenized by a
  **real** tokenizer, not `approx_tokens`. Either it fits, or the test records that it does not
  and names `context_length_exceeded` as the intended outcome. An assertion written in
  whitespace-words cannot see this — it would pass identically at 1.2 or 2.0 tokens per word.
- [ ] AC-10 (BR-11): Two measurements on the reference machine, recorded as numbers: **(a)** wall
  clock to prefill a full-budget local prompt against the same at today's budget; **(b)** the
  REQ-544 BR-8 duty (`min_tokens_per_sec: 5.0`, `benchmark.rs:43`) re-run with a full-budget
  context resident — **pass = the duty still passes**. (b) reuses a threshold this project already
  chose. Note the gap: that duty measures *generation* on a *short* prompt, so as it stands it
  can see neither prefill cost nor generation under a large resident context.
- [ ] AC-11 (BR-11/D-3): The compaction trigger's new value, measured on a real multi-turn local
  session: how many turns accumulate before `under_pressure` fires, before and after. This is the
  number D-3 needs and the one REQ-586 OQ-3 actually asked for.
- [ ] AC-12: The `/analyze` case that motivated REQ-589 — 4,097 words on the local tier — is not
  refused and raises no over-budget offer. The field report, turned into a test.
- [ ] AC-13: `cargo audit` clean; full suite green; no new clippy warnings.
- [ ] AC-14: A dogfood leg in `docs/manual-verification.md` — a large local turn by hand,
  confirming the reported budget and that the turn serves. **REQ-589 AC-15's runbook was never
  written**, which is why this REQ still has no field data; this AC is not satisfied by intending
  to run it.

## Decisions

- **D-1 — REVERSED after adversarial review. Use the `LOCAL_ENGINE_N_CTX` constant; do not plumb
  the engine's window.** The first draft argued for plumbing on the grounds that llama.cpp may
  clamp `n_ctx` below the request, making the engine's own `over_window` check pass a prompt the
  real context cannot hold — a `GGML_ASSERT` and a dead daemon. **That is false.**
  `llama-cpp-sys-2-0.1.151/llama.cpp/src/llama-context.cpp:77` (vendored, not in this repo) takes the requested value verbatim; exceeding the trained window is a
  `LLAMA_LOG_WARN` (:251), not a clamp; `GGML_PAD` (:215) rounds **up**; and the only downward
  adjustment (:220-229) is gated on `n_seq_max > 1`, which defaults to 1. So realised `n_ctx` ≥
  requested, and at 16,384 (already a multiple of 256) they are equal. With the hazard gone the
  plumbing had no remaining justification, and reusing the constant — as `egress/redact.rs:133`
  already does against this same engine — is correct and far smaller.
  *Recorded rather than quietly deleted because the refuted claim is the interesting part: the
  accessors `LlamaContext::n_ctx()` and `LlamaModel::n_ctx_train()` do exist, which is what the
  first draft checked. Their existence says nothing about whether a clamp exists. The draft
  validated the wrong proposition.*

- **D-2 — the reservation is 1,024 tokens, and its home is a new constant.** Not
  `REDACT_DUTY.max_tokens()`, which the first draft named: that value is `16 findings × 128 B / 2`
  — an *output contract* for redaction findings that equals 1,024 by coincidence, with unrelated
  meaning. Not `generation_reservation()` either, which would close the BR-3 cycle. A constant
  with its own home, cited by both the local derivation and its test.

- **D-3 — DECIDED (owner, 2026-08-25): take the full window.** The local pair derives from
  `LOCAL_ENGINE_N_CTX − 1,024` with no additional margin:

  | | today | decided |
  |---|---|---|
  | words | 4,096 | **10,240** |
  | bytes | 32,768 | **30,720** |
  | compaction fires at | 2,867 w / 22,937 B | 7,168 w / 21,504 B |
  | digest folds raw above | 1,500 w / 12,000 B | 3,750 w / 11,250 B |

  The compaction and digest movements are **accepted consequences, not oversights** — the draft's
  error was dismissing them as impossible, and they are recorded here so the architecture phase
  treats them as designed behaviour. A local session now holds ~2.5× more conversation before
  anything is forgotten. AC-10 and AC-11 measure what that costs; they do not gate the decision.

  *Note the digest byte threshold moves **down** (12,000 → 11,250) while the word threshold moves
  up. That is the ratio arithmetic, not a mistake: `digest_thresholds` scales each half by its own
  constant, and the byte half of the budget fell.*

- **D-4 — DECIDED by D-3: the byte half falls to 30,720.** Not a separate choice — 30,720 *is*
  the full-window derivation. This resolves BR-7 and BR-9 as follows, and both need work in
  architecture rather than merely being noted:

  - **BR-7's regression is accepted and is now catchable.** Byte-dense local content in the
    2,048-byte band between 30,720 and 32,768 is newly over budget. REQ-589 shipped the
    over-budget offer immediately before this REQ, so the outcome is *an offer to proceed*, not a
    hard refusal — the regression lands on the one surface built to absorb it. AC-7 pins that:
    exactly one offer, no silent elision.
  - **BR-9 must be fixed, not just observed.** `COMPACT_OUTPUT_MAX_BYTES = LOCAL_BUDGET_BYTES`
    (32,768) would exceed the 30,720 budget it repairs to, so a compaction landing in the gap is
    rejected and the turn degrades to oldest-first eviction. It has to track the local budget.
  - **BR-10's slack is gone, deliberately.** `10,240 × 3/2 = 15,360 = usable`, exactly zero
    margin, where today there is 2.5×. Content denser than 1.5 real tokens per whitespace word
    overruns at full budget; `budget.rs:205` measures Rust at 1.69. The catches are the byte
    guard (for dense-and-heavy content), the engine's typed `context_length_exceeded`, and
    REQ-589's offer. AC-9 measures the gap the byte guard does *not* cover — token-dense but
    byte-light content — and records the outcome rather than assuming it.

## External Dependencies

- **None.** D-1's reversal removed the only one. The derivation reads `LOCAL_ENGINE_N_CTX`, a
  constant in this workspace, so nothing here depends on the non-default `llama` feature or on
  any llama-cpp-2 accessor — and the default build, which links only the `Engine` trait,
  `MockEngine` and `ScriptedFileEngine`, is unaffected.

  *A future llama.cpp bump that introduces a downward clamp would invalidate D-1. That is a
  hypothetical, and a REQ cannot justify present plumbing against one — but if it happens, D-1's
  record above is the thing to re-read.*

## Assumptions

- **A-1: REFUTED — see D-1.** The first draft assumed llama.cpp might clamp `n_ctx` below the
  request. It does not. Kept as a record because the refutation is what reversed D-1, and because
  the draft's *check* (do the accessors exist?) validated a different proposition than the one it
  needed (does a clamp exist?).

- **A-2: The 3/2 words-per-token safety ratio holds for the local engine.** Both are BPE and the
  ratio is a worst case — but BR-10 records that the derived pair leaves it **zero** slack, where
  today it has 2.5×, and `budget.rs:205-212` measures Rust at 1.69 tokens/word. This assumption is
  load-bearing in a way it was not before, and AC-9 is its detector. *The first draft nominated
  AC-12 (the `/analyze` case) as the detector; that was wrong — AC-12 measures in whitespace
  words and would pass identically at any real tokenizer density.*

- **A-3: No user has hand-set a local context budget expecting 4,096.** `context_budget_cap` only
  lowers, and only on remote routes.

- **A-4: The reference machine's measurements generalize.** AC-10 and AC-11 are taken on one
  machine; the local tier is hardware-adaptive by design. A raise that is comfortable on an
  M-series Mac may not be on the slowest machine the probe admits.

## Open Questions

- [ ] OQ-1: Should the local budget re-derive when the engine is swapped mid-session, or is
  stamping it at route-decision time enough?
- [ ] OQ-2: If `LOCAL_ENGINE_N_CTX` ever falls, the derived byte half can land *below* the
  harness's own 5,979-byte system prompt plus the 1 KiB truncation floor — a tier that cannot
  serve anything. Is the answer a proportionally tiny budget, or should the tier decline to serve
  and say why? BR-8 keeps this honest; it does not resolve it.
- [ ] OQ-3: **CLOSED** — D-3 and D-4 decided by the owner, 2026-08-25: the full window. AC-10 and
  AC-11 still measure the cost, but they report rather than gate. REQ-589 AC-15's runbook remains
  unwritten and is the natural place to take AC-11's reading.

### Resolved from the placeholder

- ~~OQ-1 (remote formula or a local rule?)~~ → **the remote formula, over the constant window.**
  See BR-1/BR-4 and D-1 (which reversed on how the window is obtained, not on the formula).
- ~~OQ-2 (prefix-cache cost?)~~ → **NOT resolved; the draft's dismissal was wrong.** The budget
  drives compaction and digest thresholds, so it is not merely a ceiling. Now D-3, open.
- ~~OQ-3 (does `HarnessConfig::default()` follow?)~~ → **not the question it looked like.** It
  calls `derive`, so it is affected structurally whatever the constants do. See BR-2 and AC-2.
- ~~OQ-4 (what measurement settles it?)~~ → **AC-10 and AC-11**, the second being the one
  REQ-586 OQ-3 actually asked for.

## Out of Scope

- Anything REQ-589 covers — the over-budget offer, its consent flow, the durable remedy.
- Changing `LOCAL_ENGINE_N_CTX` itself, or how large a window the daemon requests.
- The redact path's own budget. It is already derived correctly and is cited here as the
  precedent, not as a target.
- Hardware-adaptive selection of a *different* model, and any change to `LOCAL_ENGINE_N_CTX`
  itself. BR-8 states the rule such a change would inherit; making it is REQ-547's business.
- Changing `COMPACT_PRESSURE_PERCENT` or the digest threshold *ratios*. D-3 decides how far the
  budget moves; it does not re-tune the fractions that read it.

## Provenance of this document

The first complete draft was written 2026-08-25 and immediately attacked by an adversarial pass,
which broke five of its ten business rules. Two Criticals were confirmed against source and are
recorded above rather than silently fixed, because both are instructive: **D-1** rested on a
llama.cpp clamp that does not exist, and **BR-2/BR-3** on not noticing that
`HarnessConfig::default()` calls `derive` while `generation_reservation()` calls
`HarnessConfig::default()`. The draft also asserted the budget was "a ceiling, not a target" and
used that to dismiss REQ-586's deferred cost question; `under_pressure` and `digest_thresholds`
both read the budget, so the dismissal was wrong and that question is now D-3.

Anyone revisiting this spec should assume the same treatment is warranted again.

## Retrieved Context

Primary sources, each read rather than recalled: `harness/budget.rs` (`derive`, the constants and
their doc comments), `egress/redact.rs:118-175` (the worked derivation and LESSON-446's citation),
`teton-inference/src/engine.rs:100-115` (`over_window`) and `:1010` (`with_n_ctx`),
`harness/turn_loop.rs:380-398` (the byte-budget doc), `harness/duty.rs:445`
(`DUTY_REQUEST_BYTES_PER_TOKEN`). Lessons: LESSON-446 (token budgets must share a currency — the
same failure family), LESSON-456 (one home per fact — why BR-5 keeps the constants),
LESSON-447 (fallbacks must preserve the guarded invariant — bears on D-1's fallback option).
Field report: REQ-589's motivating refusal.
