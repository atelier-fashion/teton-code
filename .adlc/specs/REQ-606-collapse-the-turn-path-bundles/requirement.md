---
id: REQ-606
title: "Collapse the turn-path parameter bundles that carry no invariant"
status: complete
deployable: false
created: 2026-09-01
updated: 2026-09-01
component: "daemon/session"
domain: "refactoring"
stack: ["rust", "daemon"]
concerns: ["maintainability"]
tags: ["refactor", "parameter-bundle", "req-600-followup", "turn-path"]
---

## Description

REQ-600 decomposed `run_prompt_turn` into eight stages and introduced **fourteen**
parameter bundles to keep their signatures under clippy's limit without adding
`too_many_arguments` suppressions — which `suppression_ratchet.rs` refuses by
design ("a new suppression is a new unnamed parameter cluster; name it instead").

**The set, enumerated.** Review reported "thirteen"; the diff of `9ec2a17`
against `9232fac` introduces fourteen. The set is listed here rather than left
as a count, because AC-1's deliverable is a classification and a classification
of an unnamed set cannot be checked:

| # | Type | Module |
|---|------|--------|
| 1 | `ClaimedTurn` | `runtime/turn.rs` |
| 2 | `AssembledHarness` | `runtime/turn.rs` |
| 3 | `AttemptInputs<'a>` | `runtime/turn.rs` |
| 4 | `AttemptState` | `runtime/turn.rs` |
| 5 | `ResolvedRoute` | `runtime/turn.rs` |
| 6 | `PreparedAttempts` | `runtime/turn.rs` |
| 7 | `SessionFacts<'a>` | `runtime/turn.rs` |
| 8 | `TurnRequest<'a>` | `runtime/turn.rs` |
| 9 | `ExpansionInputs<'a>` | `runtime/turn.rs` |
| 10 | `TurnProducts` | `runtime/turn.rs` |
| 11 | `LoopContext<'a>` | `harness/turn_loop.rs` |
| 12 | `ToolCallSite<'a>` | `harness/turn_loop.rs` |
| 13 | `ModelReply<'a>` | `harness/turn_loop.rs` |
| 14 | `TurnLatches` | `harness/turn_loop.rs` |

`SkillToolDocs` is deliberately excluded: it is `pub(crate)`, it carries bundled
documentation rather than a call's parameters, and it is not a signature-width
device. If `/architect` judges any of the fourteen out on the same grounds, it
says so and why — the set shrinks on a stated rule, never on a recount.

Naming them was right. Fourteen is more than the job needs. Review judged that
roughly five earn a name and the rest are transport:

- **`PreparedAttempts`** is constructed on the last line of
  `prepare_the_attempts` and destructured on the first line of its only call
  site. It exists because Rust returns one value, and carries no invariant.
- **`TurnProducts`** is named as an output but is an *input* bundle, built from
  four loose locals at the call site and destructured on the callee's first line.
- **`ToolCallSite`** is a borrowed re-projection of `ModelReply`: four of its
  five fields come straight from it.
- **`SessionFacts`** and **`TurnRequest`** together re-spell six values that
  `TurnContext` already carries, so a reader must know which spelling is in force
  at which line. REQ-600 ADR-3 gives a real reason — they exist before the pivot,
  where no `TurnContext` can — so this one may well be correct as it stands.

`route` appears as a field in four bundles, `probed` in three, `turn_id` in three.

## Acceptance Criteria

- [x] AC-1: Each of the fourteen named in the Description's table is
      classified: **carries an invariant** (keep), **transport** (collapse), or
      **deliberate duplication with a stated reason** (keep, and say the reason
      in the type's doc). The classification is the deliverable; the count that
      results is not a target.
- [x] AC-2: No `#[allow(clippy::too_many_arguments)]` is added.
      `suppression_ratchet.rs` stays green at its recorded figure, or the figure
      moves deliberately with what collapsed named.
- [x] AC-3: `run_prompt_turn`'s body stays under 200 lines (REQ-600 AC-1) and
      `run_session_turn_with_pressure_policy` stays at brace depth 5 or below
      (REQ-600 AC-3), both under the rules those ACs state.
- [~] AC-4 **— two of three; invariant 1 NOT pinned, see Verification.** Behaviour unchanged: the REQ-598 event fixture replays unregenerated, and
      **each of REQ-600 BR-3's three testable ordering invariants — 1, 3 and 5 —
      still fails on its inversion, re-run rather than re-asserted.** REQ-600
      shipped a guard that silently stopped covering its subject when code
      moved, and only re-running the mutation found it.
  - **Why three and not five.** REQ-600's own verification records **its** AC-4
    as *four of five*, and the two that are not pinned by inversion cannot be: invariant
    2's ordering is enforced by the compiler (`accept_invocation` takes the gate
    as a parameter, so the test pins the adjacent property that the gate is
    constructed exactly once inside the memoizing `permission_gate_for`), and
    invariant 4 "has no inversion test either and cannot have one on this path"
    — there is no presence gate to park in, and its substitute pins that no
    blocking wait is introduced. An AC that demands five inversions cannot be
    met, and an unmeetable AC is the shape that gets ticked without checking.
  - **What covers 2 and 4 instead.** Their substitutes are re-run under the same
    rule: the gate-construction count for invariant 2, the no-blocking-wait
    assertion for invariant 4. If this REQ's collapse changes a signature such
    that invariant 2's ordering stops being compiler-enforced, that is a finding
    to record, not a substitution to make quietly.
- [x] AC-5: Suite green, grepped for `FAILED`; clippy 0 under `deny`; fmt clean.

## Verification (TASK-005)

`cargo test --workspace --no-fail-fast`: **4,074 passed, 0 failed**, output
grepped for `FAILED` — **0 occurrences**, `EXIT=0`, 74 targets.
`cargo clippy --workspace --all-targets -- -D warnings`: **0**.
`cargo fmt --all --check`: clean.

Every figure below states its rule and was re-derived at this REQ's tree, not
carried over from REQ-600's record.

| AC | status | evidence |
|---|---|---|
| AC-1 | met | All fourteen classified, each verdict and its rule written into the type's own doc. **Twelve keep, two collapse** — the opposite weighting to review's "roughly five earn a name", and decided by arithmetic rather than taste (see below). `PreparedAttempts` deleted; `ToolCallSite` 5 fields → 3. `route`/`probed`/`turn_id` duplication carries its stated reason in `AttemptInputs`' doc. |
| AC-2 | met | **No suppression added or removed** — `git diff origin/main..HEAD` over `crates/**/*.rs` matches zero `allow(clippy` lines in either direction. `suppression_ratchet.rs` green at its recorded figure (3 tests). |
| AC-3 | met | `run_prompt_turn` body span **188 → 185** lines (signature line through closing brace, REQ-600 AC-1's rule), against 200. `run_session_turn_with_pressure_policy` brace depth **5 → 5**, unchanged. **Instrument note:** this REQ's counter reads the baseline at 5 where REQ-600 recorded 4 — an off-by-one in whether the body's own brace counts as depth 1. Both figures come from one instrument applied to both trees, so the *delta* is sound and it is zero; the absolute figure is reported under this REQ's rule and passes either way. |
| AC-4 | **two of three** | Invariants **3** and **5** re-run and observed red. Substitutes for **2** and **4** re-run and observed red. **Invariant 1 is NOT pinned — see the finding below.** REQ-598 event fixture replays **unregenerated** (`git diff origin/main..HEAD -- crates/tetond/tests/` is empty). |
| AC-5 | met | Figures at the head of this section. |

### Mutations, run on the changed tree and observed

Every one applied to **this REQ's** tree and reverted, per AC-4's "re-run rather
than re-asserted". The first attempt at the invariant-3 mutation is included
because it is the failure mode AC-4 exists to catch: a bad line index meant the
edit never applied, and the guard passed — a green that proved nothing. It was
caught by asserting on the patch, not by reading the result.

| # | instrument | mutation | observed |
|---|---|---|---|
| 1 | inversion | generic remote arm moved ahead of the typed spend-ceiling arm | **GREEN — suite unchanged at 4,074** |
| 1 | deletion (decisive) | the spend-ceiling arm deleted outright | **GREEN — suite unchanged at 4,074** |
| 2 | substitute | a second `PermissionGate::with_level` construction | RED: "constructed 2 time(s)" |
| 3 | inversion | claim and registry re-read swapped | RED: "claim at byte 2365 and the re-read at byte 1404" |
| 3 | (first attempt) | index off by one — patch never applied | GREEN, and meaningless. Recorded. |
| 4 | substitute | `fs::read_to_string` added inside `run_the_allowed_tool` | RED: "1x `fs::read_to_string(`" — and it caught it in the one function this REQ changed |
| 5 | inversion | the hold's rebind de-shadowed, so the context carries the pre-hold router | RED: "the context is carrying the pre-hold router" |

### Re-run again after REQ-603 merged, per `architecture.md` ADR-2

ADR-2 said the mutation evidence belongs to the tree that merges, because
REQ-603 relocates session-lifecycle code out of `runtime/mod.rs` — where four of
the five guards live and read the turn path by source scan. REQ-603 merged
(`7fe035c`) while this REQ was in Phase 7, touching four `crates/` files
including `runtime/mod.rs`. So the whole set was re-run on the rebased tree.

All five guards stayed in `runtime/mod.rs`. **That was not taken as evidence** —
LESSON-598's whole point is that a guard which has stopped covering its subject
looks exactly like one that passes — so every mutation was applied again:

| # | mutation | observed post-603 |
|---|---|---|
| 3 | claim and re-read swapped | RED — identical message, "claim at byte 2365, re-read at 1404" |
| 4 | `fs::read_to_string` in `run_the_allowed_tool` | RED — "1x `fs::read_to_string(`" |
| 2 | second `PermissionGate::with_level` | RED — "constructed 2 time(s)" |
| 5 | hold's rebind de-shadowed | RED — "carrying the pre-hold router" |
| 1 | spend-ceiling arm deleted outright | **GREEN — 4,074, unchanged** |

Suite 4,074 / 0 and both AC-3 figures (185 lines, depth 5) are unchanged across
the rebase. **REQ-604 had not merged at this point**, so a final re-run is still
owed once it does — that is the orchestrator's rebase, and this table is the
method for it.

### And again after REQ-604 merged — the final rebase

REQ-604 (`4b1d22c`) merged last in the cluster, adding a ~593-line test module
to `crates/tetond/src/runtime/mod.rs` and two new fixtures. That is the file
four of the five guards live in, so the table was run a **third** time, on the
tree that actually merges.

The rebase was clean — this REQ never edits `mod.rs`. The guards kept their
names and their file but **moved line numbers**, which is exactly the condition
LESSON-598 says cannot be read off the source. So each mutation asserted that
its patch had *changed the file* before the guard was run at all — the first
attempt at the invariant-3 mutation, recorded above, passed a guard precisely
because a bad index meant nothing was patched.

| # | mutation | patch proof | observed |
|---|---|---|---|
| 3 | claim and re-read swapped | +7/-7; re-read line 554 now precedes claim line 569 | RED — "claim at byte 2365, re-read at 1404" |
| 5 | hold's rebind de-shadowed | +2/-2; `_rebound_router` present | RED — "carrying the pre-hold router" |
| 2 | second `PermissionGate::with_level` | `with_level` count 1 → 2 | RED — "constructed 2 time(s)" |
| 4 | `fs::read_to_string` in `run_the_allowed_tool` | `fs::read_to_string` count 0 → 1 | RED — named `turn_loop.rs`, the one file this REQ changes |
| 1 | spend-ceiling arm deleted outright | 1,475 bytes / 25 lines removed; `is_spend_ceiling_reached` count 1 → 0 | **GREEN — 4,078 / 0** |

Suite **4,078 passed, 0 failed** (REQ-604 added four), `EXIT=0`, `FAILED`
grepped — 0 occurrences. AC-3 unchanged across both rebases: **185 lines**,
brace depth **5**. The REQ-598 fixture is still unregenerated — REQ-604 added
`req604_*` fixtures beside it and did not touch `req598_turn_event_order.txt`.

**Invariant 1 is unchanged by either predecessor.** It was green before REQ-603,
green after it, and green after REQ-604 — which is the strongest form of the
finding: three independent trees, same result, and the deletion is provably
applied each time.

### Rule A was verified against clippy, not assumed

The classification rests entirely on clippy's `too_many_arguments` threshold
being 7, so that number was measured rather than recalled. `ExpansionInputs` —
the **narrowest margin in the set**, and therefore the one that would expose an
off-by-one — was collapsed for real, its call site updated, and clippy run:

```
error: this function has too many arguments (8/7)
    = help: to override `-D clippy::all` add `#[allow(clippy::too_many_arguments)]`
```

Clippy names the suppression AC-2 forbids as the only escape, which is the whole
of Rule A in one message. The probe was reverted. Had the threshold been 8, five
rows of the Rule A table would have changed verdict, so this was the single
load-bearing assumption in the classification.

### Finding — invariant 1 is not pinned, and REQ-600's table overstated it

REQ-600's architecture recorded invariant 1 ("typed-outcome arms before the
generic remote arm") as **PINNED — 3 tests**, and REQ-600's AC-4 counted it
among the three that hold. On this tree it does not hold, and the check is not
close: **deleting the spend-ceiling arm entirely leaves all 4,074 tests green.**

That deletion is precisely the defect REQ-588 BR-3 names — "without this branch a
budget stop would fall through to *provider failed unrecoverably* — a sentence
that is wrong about the cause, silent about the money, and names no remedy."

Two things make the reading precise:

- **The ordering only ever had teeth in one place.** `ContextLengthExceeded` and
  `LocalContextLengthExceeded` are their own `HarnessError` variants, not
  refinements of `Remote(_)`, so their position relative to the generic remote
  arm cannot change behaviour. The only overlapping pair is the spend-ceiling
  guard against `Remote(perr) if st.attempts < 2` — and nothing drives a ceiling
  stop through the loop on a first attempt.
- **The three credited tests pin the choke point, not the arm.** The ceiling
  refusal is composed in `egress/mod.rs` and is covered there. The turn-path arm
  is a second, distinct site, and it is uncovered.

**Not a regression from this REQ.** The match arms are byte-identical to
`origin/main` — verified by diff, not by inspection — and no test changed.

**Recorded NOT MET rather than fixed here, deliberately.** Closing it needs a
fixture that drives `ProviderError::SpendCeilingReached` through `run_attempts`,
which exists nowhere today: the variant is only ever produced from
`TransportError::SpendCeiling` at the egress choke point. That is new
failure-path coverage, not a refactor, and writing it in a hurry inside a REQ
whose own AC-4 is about vacuous guards is how LESSON-569 happens again. **It
needs its own REQ.**

## Wrapup (Phase 8b)

**Merged as `d8871b8`** — "REQ-606: Collapse the turn-path parameter bundles that
carry no invariant (#257)" — squashed onto `main` from `dfd6ff4`, the exact head
CI run `33458140668` was green on (all 7 checks, both `macos-latest` and
`ubuntu-latest`). Third and last of the overlap cluster `{603, 604, 606}`; the
orchestrator owned the merge and this runner never held it.

### The result, in one line

Fourteen bundles classified. **Twelve earn their name, two did not** — the
inverse of the "roughly five earn a name" the REQ was filed on, and decided by
arithmetic rather than taste. `PreparedAttempts` deleted, `ToolCallSite`
narrowed 5 fields → 3, `TurnRequest` renamed `PromptRequest` to stop it silently
shadowing `teton_providers::TurnRequest`.

### AC-4 invariant 1 — NOT MET, and the evidence is stronger than a single run

Recorded NOT MET, deliberately, and **not** opportunistically fixed while the
code was open.

The finding is not one observation. Deleting the spend-ceiling arm from
`run_attempts` outright leaves the suite **green on three independent trees**:

| tree | suite | invariant 1 |
|---|---|---|
| pre-cluster (`391091a`) | 4,074 / 0 | GREEN |
| post-REQ-603 (`7fe035c`) | 4,074 / 0 | GREEN |
| post-REQ-604 (`4b1d22c`, the merged tree) | 4,078 / 0 | GREEN |

**And the deletion was proven applied each time**, not assumed: on the final tree
it removed 1,475 bytes / 25 lines and took `is_spend_ceiling_reached` from 1
occurrence to 0 before the suite was run. That assertion-on-the-patch exists
because the first attempt at the invariant-3 mutation silently failed to apply
and its guard passed — a green that proved nothing, which is precisely the
failure mode AC-4 exists to catch.

Three trees, three proven-applied deletions, one result. That is the evidence the
follow-up REQ starts from, and it is what makes "REQ-600's table recorded this as
PINNED — 3 tests" a correctable record rather than a disagreement. Those three
tests pin the ceiling refusal composed at the **egress choke point**
(`egress/mod.rs`); the turn-path arm is a second, distinct site and is uncovered.
The ordering only ever had teeth there — `ContextLengthExceeded` is its own
`HarnessError` variant, not a refinement of `Remote(_)`, so its position relative
to the generic remote arm cannot change behaviour.

**Why it was not fixed here.** Closing it needs a fixture driving
`ProviderError::SpendCeilingReached` through `run_attempts` on a first attempt.
That variant is only ever produced from `TransportError::SpendCeiling` at the
egress choke point, so the fixture is new failure-path coverage — not a refactor
— and writing it in a hurry inside a REQ whose own AC-4 is about vacuous guards
is how LESSON-569 repeats. It needs its own REQ.

### Knowledge captured

| id | kind | subject |
|---|---|---|
| ASSUME-031 | invalidated | the bundles can be collapsed without exceeding the argument limit |
| ASSUME-032 | validated | AC-3's body-length budget survives the collapse |
| LESSON-611 | lesson | a clobbered results file reads as a real run (reader's side of LESSON-610) |

LESSON-611 landed inside the REQ-606 merge itself rather than in this wrapup,
because the collision was found during Phase 4 and the id was measured by hand
at that point. Cross-linked to REQ-604's LESSON-610 here.

### A process fact worth writing down

**A `/sprint` pipeline-runner never invokes the delegate.** `/proceed`'s Phase 5
skips the `adlc-read` pre-pass outright in subagent mode — "subagents cannot
reliably reach a parent's shell env" — so the delegation *gate* is never
consulted and the delegate's configured state is irrelevant to a sprinted REQ
either way. This is stated in `/proceed`'s SKILL.md but is easy to miss when
reasoning about a runner from the outside: a change to `delegate-gate.sh` (such
as `e31ee93`, which made an explicit `delegate.enabled: false` outrank a legacy
key) changes nothing for any REQ running under `/sprint`. It only affects a
solo `/proceed` in the main conversation.

## Assumptions

- **[ASSUME-031 — INVALIDATED]** The bundles can be collapsed without pushing any
  signature back over the argument limit. If one cannot, that is a finding to
  record — it would mean the cluster is real and the bundle earns its name after
  all. *Twelve of the fourteen could not. The failure branch was the deliverable.*
- **[ASSUME-032 — VALIDATED]** **The same applies to AC-3's body-length budget,
  which is tighter than it looks.** *188 → 185 against a limit of 200.* `run_prompt_turn` is at **188** lines against AC-3's 200 — twelve
  lines of headroom, re-derived at this REQ's base rather than taken from
  REQ-600's record. Collapsing an *input* bundle moves its fields back to the
  call site, and for the input bundles that call site is `run_prompt_turn`'s
  body. If a collapse that is right on the classification cannot be had without
  pushing the body over 200, that is the same kind of finding as the argument
  limit: record it, and keep the bundle. AC-1's classification is the
  deliverable; neither the resulting count nor the resulting line count is a
  target to be hit by weakening the other criterion.

## Out of Scope

- Further decomposition of the turn path. REQ-603 re-measures this impl for the
  session-lifecycle slice and can absorb anything structural.

## External Dependencies

- None.
