# REQ-618 — Architecture

## The shape of the problem

Three separate mechanisms let the 2026-09-04 session lose the user's ask, and
they fail independently:

1. `ContextManager::truncate_to_budget` drops `blocks[0]` in a loop. The oldest
   block in a turn is the user's prompt — or, on a `/skill` turn, the expansion
   that *is* the prompt. Nothing distinguishes it from a tool result.
2. `compact_if_pressured` hands the whole conversation to the `compact` duty and
   accepts any answer that parses, shrinks, and fits. `CompactOffer::droppable`
   protects exactly one block: the newest. The user's prompt is block 1 of the
   offer and is the block a summarizer will reach for first.
3. Neither publishes anything. `CompactionOutcome` is returned to the turn loop,
   which prints `reason` to *stderr* on a degrade and otherwise says nothing.
   `context_pressure` fires from `truncate_to_budget`'s report; a *successful*
   compaction produces no report and no event at all.

So the fix is three changes at three seams, plus one refusal ahead of them.

## ADR-618-1: The anchor is a field on `ContextBlock`, and it is re-stated, never carried

`Anchor` is a new `Copy` enum in `harness/context.rs` with **three** variants —
`None`, `UserAsk`, `SkillBody` — and a new public field `anchor: Anchor` on
`ContextBlock`. It has no `Default`, so every one of the 36 construction sites
in the tree must state it; that is the repo's own rule for "every call states X"
(conventions, *A required field with no `Default`*), and it turns "the new push
path forgot to anchor" into a compile error rather than a review catch.

**Why not `repo_context` and `system`, which the spec's entity table lists.**
Both would be variants that can never be assigned and can never change an
outcome. `TETON.md` is not a block: REQ-612 ADR-2 put it in the *system prompt*
with a manager-level `system_sources` set, precisely so it would not enter the
oldest-first drop order. And `truncate_to_budget` removes `self.blocks[0]` — it
never touches `self.system`. The system prompt and the repository notes inside
it are therefore already un-droppable **by construction**, which is a stronger
guarantee than an anchor flag. Shipping two variants nothing can assign would be
the "documented guarantee that is false" the architecture doc warns about one
screen up. This is the answer to **OQ-1**: yes, the repo-context block is
protected — it always was, and it needs no anchor to be.

**Why re-stated rather than carried.** The anchor of a block is a fact about
*this turn's* relationship to it — "the current ask", "the previous ask" — not a
property of the text. A stored flag that survived a carry unchanged would leave
a three-prompts-ago block anchored forever. So `ContextManager::restate_anchors`
is a single pure walk over `self.blocks` that assigns every anchor from scratch,
and it runs at each seam that seeds or re-shapes a manager:

| seam | why it owes a re-statement |
|---|---|
| `CarriedTurn::begin` (after `replay` + `push_user_from`) | the replayed blocks carry the *previous* turn's anchors; the new prompt block is now the current ask and the old one demotes to previous |
| `ContextManager::push_user_from` / `push_user` | a second prompt block inside one manager (fixtures, the `/skill` seed) moves the ask |
| the model-invoked skill admit site in `turn_loop` | the expansion block becomes `SkillBody` for this turn |

This is LESSON-501's rule and the `system_sources` precedent (REQ-612 ADR-2)
applied to a second fact: *re-asserted at every writing seam, never trusted
across one*.

**The assignment rule** (`restate_anchors`):

- A **prompt block** is `role == BlockRole::User && matches!(provenance, Provenance::User{..})`.
  A tool result is `BlockRole::Tool` and a model turn is `BlockRole::Assistant`,
  so neither can ever be mistaken for one. The **newest two** prompt blocks get
  `UserAsk` (BR-1: the current turn's and the previous turn's); everything older
  gets `None` (BR-8: the anchor lapses one turn later).
- A block already carrying `SkillBody` keeps it **only** if it is newer than the
  newest prompt block — i.e. it belongs to the turn in progress. On the next
  prompt it is older than that block and demotes to `None` (BR-2's second
  sentence, by construction rather than by a timer).
- Everything else is `None`.

**BR-3 falls out of this.** Nothing in a block's *text* is an input to
`restate_anchors` — it reads `role` and `provenance` and position, all of which
the harness sets at push time. A tool result whose body contains the literal
string `anchor: user_ask` is `BlockRole::Tool`, so it lands on the `None` arm
(AC-6). To keep that true against a future push path, a source-scanning check in
the suppression-ratchet style asserts that no `anchor:` field initializer
**outside `harness/context.rs`** names anything but `Anchor::None`: the anchor is
assigned by the manager or it is not assigned. A *region* check rather than a
count, per LESSON-568 — relocating a call keeps a count identical.

**On a typed `/skill`, the ask and the body are one block.** `CarriedTurn::begin`
seeds the expansion *as* the prompt (`push_user_from(prompt, sources, unknown)`,
`runtime/turn.rs` — "one block either way"), so that block is `UserAsk` and is
already un-droppable. `SkillBody` names the case where the two are genuinely
different blocks: a model-invoked `skill` expansion, which
`turn_loop` pushes as a **tool result** (`push_tool_result_prov`). BR-2 is
satisfied on both paths; only one of them needs the second variant.

## ADR-618-2: The anchor set binds three consumers, and each was already free to break it

- **`truncate_to_budget`** stops dropping at the newest non-anchor block instead
  of at `blocks.len() > 1`. It drops the oldest **non-anchored** block each
  iteration. If only anchors remain and the context still does not fit, it stops
  and reports `over_budget` exactly as it does today — the refusal is BR-1's, and
  it is raised by the caller (ADR-618-4), not here. This method must keep
  returning rather than refusing: it runs from `Drop` in `CarriedTurn::commit_now`,
  where there is nothing to refuse to.
- **`CompactOffer::droppable`** is not enough on its own: it is a single
  *count* of leading blocks, and the anchors are not a prefix. `compact_offer`
  therefore renders anchored blocks with a note in the same shape as
  `PROTECTED_BLOCK_NOTE`, and `read_compaction` is given an explicit
  **protected-index set** beside the droppable count. An answer naming a
  protected index is rejected whole — a degraded compaction, not a partial one,
  which is REQ-561 BR-4 unchanged.
- **`compaction_summary`** is unaffected: it already inherits provenance from
  every block it was *shown*, anchors included.

The in-place clamp is the subtle one. `truncate_to_budget` middle-elides the
**last** block when it alone busts the byte budget, and on a `/skill` turn the
last block can be the anchored expansion. BR-1 forbids summarizing or dropping
an anchor; a middle-elision is a lossy edit of the same kind. So the clamp is
skipped when the last block is anchored, and the context is left over budget
with `over_budget: true` — which is precisely what ADR-618-4's refusal reads.

## ADR-618-3: `provenance_class` is derived from what the manager provably knows

The spec's `CompactionRecord.dropped_blocks` names classes `rooted` /
`boundary` / `unknown`. Two of those are **REQ-614's** vocabulary and neither
exists in the tree: `ToolProvenance` is `Sources(BTreeSet<ProvenanceId>) |
Unknown`, and the boundary verdict is computed at *egress* from
`Config::effective_boundaries`. `ContextManager` holds no `Config` and no
`BoundaryMatcher`, and giving it one to fill in a report field would push an
egress concern into the context manager for no enforcement value.

So `ProvenanceClass` ships with three variants derived from `Provenance` alone:

| variant | assigned when |
|---|---|
| `Unknown` | `ToolProvenance::Unknown`, or `Provenance::User { unknown: true }` |
| `Rooted` | a non-empty `ProvenanceId` set — an identity was minted, which is what "resolved under the session root" means |
| `None` | no file provenance: typed prompt text, model turns |

`boundary` is **not** a variant. A privacy reader's question — "did this summary
derive from a protected file?" — is answered at the choke point by the session
taint, which is where the matcher lives. When REQ-614 lands its per-result
verdict on the block, `Boundary` becomes assignable and is added then; adding it
now would ship a variant that can never be assigned. This is W-1's resolution:
REQ-618 does not depend on REQ-614 and does not pretend to REQ-614's precision.

## ADR-618-4: BR-1's refusal is raised at the gate, not inside the manager

`ContextManager::anchor_bytes()` sums the rendered cost of anchored blocks plus
the system prompt. A new `ContextManager::anchors_fit()` compares it against both
budgets. The turn loop checks it **after** the compact-then-truncate gate and
**before** `ctx.prepare(hook)` — the last point at which "nothing is sent to the
model" (AC-2) is still true. On a failure it publishes
`turn_refused_anchors_exceed_budget { anchor_bytes, budget_bytes, anchor_kinds }`
and ends the turn with the same typed-refusal shape REQ-586 BR-2 uses for
`ContextLengthExceeded`, so the retry/fallback/degrade machinery does not act on
it (conventions, *A typed outcome needs both halves*).

It is deliberately not inside `truncate_to_budget`: that method runs from `Drop`
during `CarriedTurn::commit_now`, and a refusal raised there has no caller and no
turn to end.

## ADR-618-5: `context_compacted` is published by the call site, from a record the manager returns

`CompactionOutcome` gains a `record: Option<CompactionRecord>` — `Some` on an
applied compaction, and `Some` with `fallback: true` on the mechanical
fall-through. The manager builds it (it is the only thing that knows the byte
totals and the dropped blocks' provenance) and does **not** publish it: it holds
no `SessionEvents` handle, which is the same split `PressureReport` already
takes (LESSON-501). The two `compact_if_pressured` call sites publish.

The mechanical-fallback record (BR-5, AC-5) is produced by `truncate_to_budget`
rather than by the duty path, because that is what actually stands in when the
duty fails — the `PressureReport` gains the byte totals it needs and the turn
loop composes the `fallback: true` record from it. One record per compaction on
both paths; `fallback` is what tells them apart.

**AC-4's identity, defined so the assertion cannot be vacuous:**

- `summarized_bytes` — the `text.len()` sum of the blocks the duty's answer
  replaced with a summary.
- `dropped_bytes` — the `text.len()` sum of blocks removed with no replacement
  (all of them, on the mechanical path).
- `kept_bytes` — the `text.len()` sum of the blocks that survived, measured
  **before** the summary block is inserted.
- The identity is `kept + dropped + summarized == pre-compaction Σ text.len()`.
  The summary block's own bytes are in **none** of the three: it did not exist
  before the compaction, so counting it would break the identity it is supposed
  to close. `anchor_bytes` is reported separately and is a **subset** of
  `kept_bytes`, which is what makes `anchor_bytes <= kept_bytes` a real assertion
  rather than an arithmetic tautology.

## ADR-618-6: `SkillFitVerdict::fits_without_room` extends the existing fit path

`harness/budget.rs` already owns `skill_fit` / `skill_append_fit`, both of which
return `SkillFit::{Fits, TooLarge}` from one `ContextManager::would_*_fit`
measurement. A third verdict is added at that seam rather than at a new one:

```
fits              body_bytes <= room_fraction × budget_bytes  and the Fit fits
fits_without_room body_bytes >  room_fraction × budget_bytes  and the Fit fits
over_budget       the Fit does not fit                        (today's TooLarge)
```

`ROOM_FRACTION_PERCENT: usize = 25` is a pinned constant, expressed in percent
so the arithmetic is integer (the codebase has no float constants in this path).
The refusal composes through the **same** `skill_refusal` composer, with a new
`SkillSentence` arm, so the three sentences cannot quote different numbers for
one measurement — REQ-589 BR-5's rule, extended by one arm rather than forked.
`skill_refused_no_room` is published and the offer goes through
`offer_or_refuse_over_budget`, which already asks BR-3's question and already
carries `proceed once` (AC-3).

**W-4's resolution — what "two anchored bodies in one turn" means.** BR-2 says
the second expansion is refused "with the arithmetic (this is the case BR-4
governs)", and BR-4 is the fraction rule. The refusal is therefore *arithmetic*,
not a bare "one body per turn" cap: the second expansion is measured with every
**already-anchored** body in the turn counted as unavoidable overhead, so the
test is `Σ anchored_body_bytes + candidate_bytes > room_fraction × budget_bytes`.
Two 10 % bodies are both admitted; two 20 % bodies refuse the second. A flat cap
would have been a behaviour change REQ-587's per-turn invocation cap does not
ask for, and would refuse pairs of small skills that fit fine.

## ADR-618-7: BR-6's summary line names blocks, because blocks are what the manager counts

The spec's line reads *"[summary of `<n>` earlier blocks, `<bytes>` bytes, from
turns `<a>`–`<b>`; the user's prompts are kept verbatim below]"*. `ContextBlock`
carries no turn ordinal and `ContextManager` holds no turn counter, so `<a>`–`<b>`
is not a fact this daemon has (W-2). Two ways to get one: add a second required
field and a counter that must survive the carry, or say what is true.

The line ships as:

```
[summary of <n> earlier blocks, <bytes> bytes; the user's prompts are kept verbatim below]
```

The turn range is dropped rather than faked. A block index is not a turn number,
and printing one under the other's name in the one sentence whose job is to tell
the model what it is reading would be exactly the untruth LESSON-570 is about. If
turn ordinals become worth having, they are their own change with their own carry
seam. The clause the rule actually turns on — *the user's prompts are kept
verbatim below* — ships intact, and it is now **true by construction** rather
than aspirational, because ADR-618-1 is what keeps them there.

The line is harness-authored and rides **outside** the untrusted envelope, in the
slot the existing `[earlier conversation compacted — n blocks elided]` notice
already occupies (which it replaces). That is BR-6's point: the model must be
able to tell a summary from a tool result.

## ADR-618-8: AC-8's fixture is a reconstruction, and says so

Session `sess-23aczryx…` is not a repository artifact — the tree holds no
`.jsonl` fixtures and transcripts are written to a directory tools are *forbidden*
to read (REQ-611 ADR-7). AC-8 is therefore satisfied by a fixture that
**reconstructs** the transcript's shape at the figures the REQ names — the 21,162-token
budget, a 25 KB skill body, twenty-six tool results, the `/analyze` prompt line
and *"where are the results?"* — and the test's doc comment states that it
reconstructs rather than replays. LESSON-519's rule is that an
assert-by-inspection AC needs the real artifact; the honest reading here is that
the *claim* AC-8 makes (both prompts survive verbatim into the fourth prompt's
request body) is checkable against a reconstruction, and that nobody should later
read the test as evidence about the original file.

## Blast radius

| file | change |
|---|---|
| `crates/tetond/src/harness/context.rs` | `Anchor`, `ProvenanceClass`, `CompactionRecord`; `ContextBlock.anchor`; `restate_anchors`, `anchor_bytes`, `anchors_fit`; anchor-aware `truncate_to_budget`; `CompactionOutcome.record`; BR-6 summary line |
| `crates/tetond/src/harness/compact.rs` | anchored-block note in `compact_offer`; protected-index set in `read_compaction` |
| `crates/tetond/src/harness/budget.rs` | `ROOM_FRACTION_PERCENT`, `SkillFitVerdict`, the `NoRoom` sentence arm |
| `crates/tetond/src/harness/turn_loop.rs` | BR-1 refusal gate; `context_compacted` publish; `SkillBody` anchor at the model-expansion admit site; BR-2 arithmetic |
| `crates/tetond/src/carry.rs` | `restate_anchors` at `begin`; anchored `ContextBlock` literals |
| `crates/tetond/src/runtime/turn.rs`, `runtime/duty.rs` | `skill_refused_no_room` offer routing; `context_compacted` publish at the second call site |
| `crates/teton-protocol/src/events.rs` | `ContextCompacted`, `SkillRefusedNoRoom`, `TurnRefusedAnchorsExceedBudget`; `ContextPressure.anchors_intact` |
| `crates/teton/src/session_ui.rs` | render arms for the three new events |
| `crates/tetond/src/{sessions,egress/provenance,repo_context/render,harness/tools/mcp}.rs` | `anchor: Anchor::None` at existing `ContextBlock` literals |

## What is deliberately not done

- No change to the `compact` duty's route, its thresholds, or the digest
  thresholds (spec's Out of Scope; REQ-616 BR-8 owns the scaling).
- No window or budget is raised (REQ-616).
- No idle compaction (OQ-2).
- No `boundary` provenance class (ADR-618-3).
- No turn ordinal on `ContextBlock` (ADR-618-7).
