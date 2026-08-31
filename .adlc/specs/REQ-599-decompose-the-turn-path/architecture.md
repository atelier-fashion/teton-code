# REQ-599 — Architecture

## Approach

Decompose `runtime.rs` (14,183 production lines) into topic modules, in a
sequence of independently-green commits, preserving behavior and traceability.

**The requirement asked for one thing to be settled before anything else: is the
rationale-id clustering a usable proxy for the real seams?** It is not, and
measuring that is the first result below. The goal survives; the method does
not, and ADR-2 replaces it.

## Key Decisions

### ADR-1: The central bet is REFUTED by measurement

> **CORRECTED 2026-08-31 after adversarial review — read this before the ADR
> below.** The measurement is reproducible and the parser is sound, but the
> *statistic* is not. Span was computed as `max − min`, which has zero breakdown
> resistance: one outlying annotation forces the "scattered" verdict. Re-measured
> with the smallest window holding 70% of an id's items, **5 of 19 cluster, not
> 1** — and trimming a single extreme item per id moves the max-span count from 1
> to 5 as well.
>
> The decisive counterexample is **REQ-581**: max-span 3,515, filed below as
> "loose, not a seam". Its 70%-window is **219 lines** holding 4 of 5 items, and
> that window is `ProbeAnswer` / `probe_outcome` / `to_protocol_health` /
> `stream_probe` — exactly the set that became `runtime/provider.rs`. The id
> predicted the module; the metric could not see it. Note that the "Findings for
> the deferred work" section below records `provider` as "measured as scattered
> across 10,366 lines and was skipped for that reason" — on that seam the
> discarded proxy beat the census that replaced it.
>
> **What survives:** the requirement's *literal rule* — "where they interleave
> across a proposed boundary, the boundary is wrong" — really is fatal, because
> in a cross-cutting file every boundary has interleaving ids and the rule
> condemns all of them. That is enough to reject the rule as a **decision
> procedure**.
>
> **What does not survive:** the generalisation that ids "cannot locate a seam."
> They are a weak *positive* signal — usable to propose candidate boundaries,
> not to reject them. ADR-2's structural method remains the right way to decide;
> it was not, as this ADR claimed, the only source of signal available.
>
> See LESSON-593, rewritten with the correction.


The requirement's Assumptions say:

> The documentation's REQ/ADR/LESSON ids are a usable proxy for the real seams.
> Where a stage's rationale ids cluster cleanly, that is a boundary; where they
> interleave across a proposed boundary, the boundary is wrong. **This
> assumption is the REQ's central bet and should be validated early in
> `/architect`.**

Validated, and it does not hold. Every production item in `runtime.rs` was
parsed with its attached doc block, and each REQ id's item positions measured.

**File level — 19 REQ ids appearing on 3+ items:**

| verdict | count | meaning |
|---|---:|---|
| CLUSTERED (span < 2,500 lines) | **1** | REQ-554, 3 items, span 127 |
| loose (span < 7,000) | 5 | |
| **SCATTERED across the file** (span ≥ 7,000) | **13** | e.g. REQ-589 spans 12,222 lines; REQ-544 spans 12,776; REQ-561 spans 13,547 |

**Function level — inside `run_prompt_turn`'s 1,084 lines, 13 REQ ids appearing
twice or more:**

| verdict | count |
|---|---:|
| LOCAL to one stage (span < 20% of the body) | **4** |
| two regions | 4 |
| **spans the whole function** | **5** |

Taken literally the requirement's rule — "where they interleave across a
proposed boundary, the boundary is wrong" — would condemn *every* possible
boundary, making the REQ unimplementable. That is the tell that the rule is
wrong rather than the file.

**Why it was a reasonable bet and still failed.** The ids are real and dense.
But a REQ id marks *a change*, and changes to a turn path are overwhelmingly
**cross-cutting**: REQ-589 (the over-budget offer) touches budget checks,
dispatch, the commit path and the failure arms; REQ-585 (skills) touches
expansion, consent, provenance and refit. An id records which decision a line
serves, not which subsystem it belongs to. Those coincide only for a REQ that
introduced a self-contained subsystem — which is the one clustered case,
REQ-554's chat template.

The traceability ids remain valuable and BR-2 stands: they must survive the
move, and REQ-598 already shipped the sweep that enforces it. They are an
**asset to preserve**, not a **signal to navigate by**.

### ADR-2: Seams come from the type/impl structure, not the commentary

What the same census does show, unambiguously:

| | |
|---|---|
| production types defined in the file | **43** |
| `impl` blocks / distinct targets | 28 / 23 |
| `impl DaemonRuntime` blocks | 3, at lines 2638, 9781, 9787 |
| **the god-impl** | lines **2638–9781**, ~**7,143 lines in one `impl` block** |

So the file is two structurally different halves:

1. **~2,600 lines before the god-impl** and ~4,400 after: 43 types with their
   own impls, plus free functions. These are largely self-contained and move
   almost mechanically.
2. **~7,143 lines of a single `impl DaemonRuntime`**, whose methods are ordered
   by *when they were written*, not by topic. This is the actual god module and
   the whole difficulty.

The one functional grouping that is already cohesive by position is **duty
routing** — 9 functions spanning 2,641 lines (`digest_route`, `triage_route`,
`shell_route`, `title_route`, `compact_route`, `resolve_duty`,
`build_duty_route`, `spawn_title_session`, `redact_route`). Every other grouping
sampled spans 6,000–12,000 lines. Duty routing is therefore the **first**
extraction: it is the seam the code already has, and REQ-598 has just given it a
named parameter bundle (`DutyContext`) so the extracted functions do not arrive
with ten arguments.

### ADR-3: An inherent impl may be split across modules of the same crate

The enabling fact, and the reason this is tractable at all: Rust requires an
inherent `impl` to live in the **crate** that defines the type, not the module.
`crates/tetond/src/runtime/duty.rs` can therefore hold
`impl DaemonRuntime { fn digest_route(…) }` while `runtime/mod.rs` keeps the
struct definition.

This means the god-impl can be sliced **without introducing traits, without
newtypes, and without changing a single call site** — callers still write
`self.digest_route(dctx)`. That is what makes a behavior-preserving split
plausible and keeps ADR-4's "no new abstractions" honest.

Visibility: methods currently private to the file become `pub(crate)` or
`pub(super)` where a sibling module needs them. That widening is real and is the
main semantic cost of the split; it is bounded by keeping the modules under one
`runtime/` parent so `pub(super)` suffices for most.

### ADR-4: Extraction order — types first, then god-impl slices, cheapest seam first

Each step is its own commit, independently green (BR-9):

| # | step | ~lines | risk |
|---|---|---:|---|
| 1 | `runtime/` module skeleton; `runtime.rs` -> `runtime/mod.rs` | 0 | none — pure rename |
| 2 | self-contained types + their impls -> `runtime/types.rs` (or per-topic) | ~2,000 | low |
| 3 | **duty routing** slice -> `runtime/duty.rs` (ADR-2's real seam) | ~1,200 | low |
| 4 | provider setup / registration / migration -> `runtime/provider.rs` | ~1,800 | medium |
| 5 | model consent + engine activation -> `runtime/consent.rs` | ~1,200 | medium |
| 6 | MCP egress + redaction wiring -> `runtime/egress.rs` | ~600 | low |
| 7 | session lifecycle -> `runtime/session.rs` | ~900 | medium |
| ~~8~~ | ~~`run_prompt_turn` -> a stage sequence~~ | ~1,100 | **DEFERRED to its own REQ** |

#### Reconciled with what shipped (REQ-602 TASK-306, 2026-08-31)

The table above is the **plan**. It was never reconciled with the branch, and
five of the modules it names do not exist: `types`, `consent`, `egress`,
`session`, and `turn`. Read as a map it is wrong in every row but two — which
is the failure mode `runtime_module_map.rs` exists to prevent for the *other*
map in this document, and which this one was not subject to.

What the seven commits actually produced:

| # | planned | shipped | commit |
|---|---|---|---|
| 1 | `runtime.rs` -> `runtime/mod.rs` | same | `f1f77b8` |
| 2 | `types.rs` | `config_document.rs` | `577b568` |
| 3 | `duty.rs` | `duty.rs` | `9ee1303` |
| 4 | `provider.rs` | `engine.rs` | `7813b95` |
| 5 | `consent.rs` | `views.rs` | `fd85489` |
| 6 | `egress.rs` | `taint.rs` | `f64d99b` |
| 7 | `session.rs` | `provider.rs` | `56f3777` |
| ~~8~~ | ~~`turn.rs`~~ | deferred to REQ-600 | — |

`testsupport.rs` shipped as well and appears in no plan row: it was extracted to
hold helpers two modules had come to share.

Only **two** of the seven proposed names shipped at all: `duty.rs` and
`provider.rs`. `types`, `consent`, `egress`, `session` and `turn` never existed
as modules — `consent` ended up inside `engine.rs` and `egress` inside
`taint.rs`, as *content* rather than as modules. And the two that did survive
are paired with the wrong steps: `provider` was planned for step 4 and shipped
at step 7. The seams were chosen from the impl
structure as ADR-2 said they would be, and that reordered the work; nobody went
back to say so. Recording the drift is the point: a plan that silently becomes a
map is the more expensive of the two failures, because the map is what a reader
trusts.

#### The session-lifecycle slice shipped nothing

Planned step 7 was `session lifecycle -> runtime/session.rs`, ~900 lines. No
commit in the sequence extracts it, and until now nothing recorded that. It is
**deferred, not dropped**, and filed as **REQ-603** so the deferral has a
tracked home rather than living in a paragraph. The reason it did not ship is
the honest one: the seven steps were chosen cheapest-seam-first from the impl
structure, session lifecycle was the most entangled of them, and the REQ ran out
of steps before it ran out of seams. `mod.rs` is still 10,306 production lines
(AC-1, NOT MET), so the slice has lost none of its value.

**Step 8 is deferred to its own REQ** (decided 2026-08-30). Steps 1–7 relocate
code; step 8 restructures control flow on the path every prompt runs through.
Landing them together would bury the one genuine behavior risk inside a diff
dominated by relocation. This REQ therefore delivers the moves, leaving
`runtime/turn.rs` small enough that the deferred restructure can be reviewed as
a change rather than as a diff.

`#[cfg(test)]` bodies move with their code (BR-7). Because tests are 61% of the
file, each step's diff is dominated by test relocation — which is why the
per-step line counts above are production-only and the actual commits will be
several times larger.

### ADR-5: AC-4's traceability check cannot derive an "owning module" from ids

AC-4 asks the check to fail "on an id that moved to an unexpected module". ADR-1
removes the basis for computing *expected*: ids do not belong to modules, they
cross them.

What replaces it, and it is strictly stronger than a count:

- **Re-attachment** — an id that annotated item `X` before the split still
  annotates `X` after, wherever `X` now lives. This is item-scoped, so it needs
  no module map, and it is exactly the arm that catches a comment left behind or
  captured by a neighbour. **REQ-598 already shipped this** in
  `crates/tetond/tests/traceability_sweep.rs`; this REQ re-points its `BASE`
  constant and `TOUCHED` list at the split.
- **Disappearance** — workspace-scoped, unchanged.
- **Vacuity floor** — unchanged, and re-measured after each step.

So AC-4 is satisfied by *extending* an existing checked property rather than
inventing a module map to assert against. AC-12 (the architecture doc names
every module and a test asserts each exists) keeps the map honest without
pretending it can be derived.

## OQ-1 answered — the module map, as built

Measured on the finished branch. Production lines exclude in-file
`#[cfg(test)]` bodies.

| module | production | holds |
|---|---:|---|
| `mod.rs` | 10,306 | `DaemonRuntime`, the ~6,540-line god-impl, the turn path, and everything not yet sliced |
| `engine.rs` | 1,091 | probe, installer, engine loaders, `EngineSlot`, `StagedEngines` |
| `config_document.rs` | 888 | rendering and persisting the config document |
| `duty.rs` | 632 | the five `*_route` resolvers, `resolve_duty`, `spawn_title_session`, `RedactionGateImpl` |
| `taint.rs` | 535 | `SessionTaint`, the lookup seam, `TaintingPrivacySink` |
| `views.rs` | 501 | `config/get`'s snapshot and the web-setup views |
| `provider.rs` | 410 | transport, credentials, connection probe |
| `testsupport.rs` | 42 | scratch-dir helpers shared by the tree's tests |

`runtime.rs` was **14,183** production lines at `fedcab1`. `mod.rs` is now
**10,306** — a reduction of 3,877 (27%), with 4,057 lines living in seven
modules that can be read on their own.

### AC-1's target is NOT met, and this records why

This document set the target at "no module above 2,000 production lines, and
`mod.rs` under 1,000". `mod.rs` is 10,306. The target was written assuming all
eight steps; step 8 was deferred by decision on 2026-08-30, and it is the step
that reaches the god-impl.

The arithmetic is the explanation: `impl DaemonRuntime` is still ~6,540
production lines. Steps 2, 4, 5, 6 and 7 moved **top-level** items — types,
free functions, constants — and only step 3 took methods out of the impl itself.
Six of the seven steps could not have reduced it. Reaching 2,000 requires
slicing the god-impl along the turn path, which is exactly what the deferred
step is.

Restating rather than lowering the number: a target moved to match the result
stops being a target.

## Risks

- **Visibility widening** is the real semantic cost (ADR-3). Every method the
  split makes `pub(crate)` is surface that did not exist before.
- **Step 8 is a behavior risk**, not a move. It should be reviewed as a change,
  not as a relocation, and its own commit should be small enough to read.
- **The 61% test fraction** makes every diff look enormous. Reviewers must be
  told which hunks are relocation and which are edits, or step 8's real change
  will be invisible inside step 8's noise.
