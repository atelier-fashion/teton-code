# REQ-603 — Architecture

## Approach

Extract the session-lifecycle surface from `crates/tetond/src/runtime/mod.rs`
into `crates/tetond/src/runtime/session.rs` as a second `impl DaemonRuntime`
block, following the pattern `turn.rs` established (REQ-599 ADR-3, REQ-600).
Bodies are byte-identical; nothing is restructured.

**Baseline, re-measured at this REQ's own base `e3013c6`** (`origin/main` at
branch time): `runtime/mod.rs` is **7,420 production lines** of 23,853 total,
under the counting rule *everything above the first column-0 `#[cfg(test)]`*
— the rule `crates/tetond/tests/runtime_module_map.rs::production_counts()`
enforces (`lines().position(|l| l.starts_with("#[cfg(test)]"))`). The corrected
spec baseline and the machine-checked rule agree; the stale 10,306 figure
REQ-599 closed on is not used anywhere in this REQ.

## Key Decisions

### ADR-1: The slice was located by reading the impl structure, not by ids

AC-1 asks for the method, not just the answer. REQ-599's ADR-1 measured that
rationale ids do not locate seams, and LESSON-593 corrected that to "a weak
*positive* signal only". So the derivation here is structural, and reproducible:

1. Cut `mod.rs` at the first column-0 `#[cfg(test)]` (line 7421) to get the
   production corpus.
2. Enumerate every method of the `impl DaemonRuntime` block at 2153–5807 with
   its line span.
3. For each method, record which `self.<field>` it touches.
4. Cluster on *what the method serves*, then check the clustering against
   adjacency in the file.

Step 4's result is the finding: the session-lifecycle methods are **already
contiguous**, occupying lines **3178–3587** — one unbroken run between
`record_health` (ends 3176) and `mcp_egress`'s doc (begins 3589). A seam that is
already a contiguous run is the strongest structural evidence available that it
is one subsystem, and no id search produced it.

The set, and what each serves:

| item | lines | serves |
|---|---:|---|
| `clear_session` | 3178–3253 | `session/clear` |
| `jail_root` | 3255–3265 | root fallback, shared by create / cd / every turn |
| `session_root_for` | 3267–3281 | the one root derivation (REQ-583 ADR-1) |
| `set_session_cwd` | 3283–3484 | `session/set_cwd` |
| `store_session_skills` | 3486–3503, 3510–3549 | the one skill-registry derivation, shared by `session/create` and `set_cwd` — **identified as part of the slice, but it did not move; see ADR-4** |
| `drop_grants_expiring_on_root_change` | 3551–3587 | `set_session_cwd`'s grant-shedding half |

**`projects()` (3504–3508) stays in `mod.rs`**, and its own doc comment is the
reason: *"Held here rather than beside the session registry because it is a fact
about the **machine**, not about any session."* It sits inside the run only by
file layout. Excluding it is the one place this slice is not simply "the
contiguous block".

### ADR-2: The Assumption is confirmed for production and refined for tests

The requirement's single Assumption — *"the slice is still coherent as a unit …
this REQ must confirm it before committing to a module, and say so if it turns
out the lifecycle code is genuinely entangled rather than merely large"* — was
tested rather than inherited. It resolves in two halves.

**Production: coherent.** Measured, not asserted:

- the six items are contiguous (above); five of them moved, and the sixth was
  blocked by a guard-baseline problem rather than by entanglement (ADR-4);
- the run's **only** dependency on a private `mod.rs` item is
  `refused_claim_error`, which `turn.rs:477` already reaches cross-module, so it
  needs no visibility change at all;
- no item in the run needs its visibility widened (ADR-3).

**Tests: genuinely entangled, and the entanglement is principled.** The ten
session-lifecycle tests live in `mod tests::conversation_carry` and **nine of
them drive a real prompt turn** (`carry_runtime` + `prompt` + `Scripted`) before
clearing a conversation or moving a root — because a cleared conversation and a
moved root only mean anything once a turn has built one.

That is not accidental co-location. `conversation_carry`'s own module header
states its membership rule:

> *"Every test in this module calls `DaemonRuntime::run_prompt_turn` — the real
> entry point `session/prompt` reaches — against a scripted local engine, and
> asserts on the context that engine was handed."*

The nine satisfy that rule on their own merits; they are conversation-carry
tests whose *verb* happens to be `clear_session` or `set_session_cwd`. Moving
them would require lifting ~230 lines of turn-path fixture (`Scripted`,
`RecordingEngine`, `RecordedPrompts`, `local_config`, `carry_runtime`,
`carry_runtime_recording_duties`, `one_session*`, `prompt`,
`await_permission_request`) into `testsupport.rs` to serve two homes, and would
leave `conversation_carry` describing a rule it no longer obeys.

**The tenth test does not meet that rule** — `the_session_root_is_probed_from_
the_cwd_or_the_daemon_fallback` never calls `run_prompt_turn`; it calls
`session_root_for` and `jail_root` directly. It moves with its subject. It is
also *forced* to: `jail_root` stays private to `session.rs`, and a parent module
cannot see a child's private items, so leaving that test behind would not
compile. The compiler agrees with the classification, which is the check worth
having.

So AC-5 is met by its second clause for nine tests and its first for one, and
the module header records the measured reason rather than a gesture.

### ADR-3: Visibility — a corpus change, not a widening

`crates/tetond/tests/runtime_visibility.rs` excludes `mod.rs` and scans every
other file under `runtime/`. Its `submodules()` doc already names this REQ:

> *"REQ-600 extracts `runtime/turn.rs` and REQ-603 extracts a session module,
> and against a frozen list this ratchet would stay green while the crate-wide
> surface grew without bound."*

So moving items into `session.rs` brings **already-existing** visibilities into
the scanned corpus. Not one qualifier is widened by this REQ:

| item | before | after | why not narrower |
|---|---|---|---|
| `clear_session` | `pub` | `pub` | `server.rs`, `harness/tools/mod.rs` |
| `session_root_for` | `pub` | `pub` | `server.rs`, plus 6 integration tests linking the lib from outside the crate |
| `set_session_cwd` | `pub` | `pub` | `server.rs`, `harness/tools/mod.rs`, `projects/mod.rs` |
| `store_session_skills` | `pub(crate)` | `pub(crate)` | stayed in `mod.rs` (ADR-4), so it never entered this scan's corpus |
| `jail_root` | private | private | only `session_root_for` and the moved test |
| `drop_grants_expiring_on_root_change` | private | private | only `set_session_cwd` |

The ratchet updates are therefore bookkeeping of the corpus, and are commented
in exactly the frame REQ-600 used for `turn.rs::run_prompt_turn`:

- `PUBLIC` gains three `session.rs::…` entries; `PUBLIC_DECLARATIONS` 14 → 17.
- `CRATE_WIDE` is **unchanged at four**. It was to gain `store_session_skills`;
  since that item stayed in `mod.rs` — which this scan excludes — the pinned
  crate-wide surface never moved.

**One finding recorded rather than acted on.** The demote-and-build derivation
showed `clear_session` compiles clean at `pub(crate)`: every caller
(`server.rs`, `harness/tools/mod.rs`) is in-crate, and no integration test
reaches it. It stays `pub`. Narrowing an API inside a relocation is exactly
LESSON-595, and whether the `session/*` surface should be uniformly
`pub(crate)` is a real question that deserves to be asked on its own.

AC-4's ratchet is **not** loosened: nothing becomes `pub(crate)` that was
narrower, and nothing becomes `pub` that was narrower. The claim is established
by demoting and building (`cargo check --workspace --all-targets`, reading
`E0603`), never by grepping for the name — LESSON-596, and the method
`runtime_visibility.rs` documents as the only one that has been right.

### ADR-4: The doc re-attachment was attempted, refused by a guard, and reverted

Lines 3486–3503 are `store_session_skills`'s doc comment — *"Derive
`session_id`'s skill registry … the one derivation"* — but they are currently
attached to `projects()`, whose own one-line doc sits at 3504 as the last line
of the same block. `store_session_skills` has no doc at all.

The plan was to take 3486–3503 with `store_session_skills` and leave 3504 with
`projects()`, correcting the misattachment on the way out.

**That was tried and it does not work, for a reason worth recording.**
`traceability_sweep.rs`'s arm 2 — "if an id annotated item X at the base and X
still exists, the id must still annotate X" — went red:

```
AC-3   left `projects` (still on 146 other item(s))
ADR-1  left `projects` (still on 161 other item(s))
BR-1   left `projects` (still on 274 other item(s))
REQ-585 left `projects` (still on 178 other item(s))
```

The wedge is **already present at the sweep's baseline commit `17c39ec`**
(verified with `git show 17c39ec:crates/tetond/src/runtime.rs`). So the guard
built to catch "an item wedged between a doc comment and the item it explains"
records this particular wedge as ground truth, and any correction reads to it as
rationale moving *off* its item. The guard cannot see the defect because the
defect predates it.

The two available moves were both bad: take the doc and turn arm 2 red, or take
the function without its doc and leave an 18-line comment in `mod.rs`
describing a function in `session.rs` — the LESSON-594 shape, in the direction
the sweep cannot see.

So `store_session_skills` **stayed**, and the slice is five items. Untangling
this means changing what the sweep's baseline is permitted to assert, which is a
change to a guard and belongs in a commit where it can be reviewed as one — not
buried in a relocation, which is the same rule REQ-603 applies to behaviour.
Filed as a follow-up.

**The same run caught a genuine error of mine.** `scratch_root` was cut out from
under a plain-`//` `REQ-583` banner, leaving the rationale behind. That is
LESSON-594, and `turn.rs`'s header records the identical mistake being made once
before. Corrected by carrying the banner into `testsupport.rs`. The guard earned
its keep twice in one run.

Verification for AC-3 is `git diff origin/main..HEAD | grep '^[-+].*///'`
reviewed by hand, plus `runtime_doc_paths.rs` and `traceability_sweep.rs`
staying green.

### ADR-5: Nothing REQ-600 owns is touched

`run_prompt_turn`'s control flow is out of scope (REQ-600, and the requirement's
Out of Scope). No file under `harness/` is touched, and `turn.rs` is not
modified — which also keeps this REQ's rebase surface clear of REQ-606, which is
collapsing `turn.rs`'s parameter bundles concurrently.

The one file shared with the concurrent REQs is the module-map table in
`.adlc/specs/REQ-599-decompose-the-turn-path/architecture.md`. The edit there is
deliberately two lines — one new row, one refreshed count — so REQ-604's and
REQ-606's rebases onto it are mechanical.

## Outcome, as measured

**AC-6, with the counting rule.** Under *everything above the first column-0
`#[cfg(test)]`* — the rule `runtime_module_map.rs::production_counts()`
enforces:

| file | before (`e3013c6`) | after |
|---|---:|---:|
| `runtime/mod.rs` | 7,420 | **7,075** |
| `runtime/session.rs` | — | 478 |
| `runtime/testsupport.rs` | 87 | 125 |

`mod.rs` loses **345** production lines (4.6%). It remains **more than seven
times** the architecture doc's 1,000-line target, so REQ-599's AC-1 stays
**NOT MET** — restated, not lowered.

REQ-599 ADR-4 estimated this slice at ~900 lines. Re-measured, the production
surface that moved is 335 non-blank lines; with the module header, the moved
test and the lifted helper the module is 478. The estimate was never
re-derived at the time, which is why the requirement asked for a measurement
rather than trust (LESSON-593).

**ADR-4 was overturned by the guard it predicted.** The plan was to re-attach
the `store_session_skills` doc block on the way out. `traceability_sweep.rs`'s
arm 2 went red: `AC-3`, `ADR-1`, `BR-1` and `REQ-585` "left `projects`". The
baseline commit `17c39ec` already contains the wedge, so the guard records the
misattachment as ground truth and any correction reads as rationale moving off
its item. `store_session_skills` therefore stayed in `mod.rs` and the slice is
five items, not six. Filed as a follow-up; see `session.rs`'s header.

The same run caught a real mistake of mine: `scratch_root` was cut out from
under a plain-`//` `REQ-583` banner. That is LESSON-594, and `turn.rs`'s header
records the identical error being made once before. Corrected by carrying the
banner.

**Verification run**

- `cargo test --workspace --no-fail-fast` — **4,074 passed, 0 failed**, 74
  targets; output grepped for `FAILED` (0 occurrences). A summed count is a
  floor, not a total (LESSON-533), so the grep is the claim.
- `cargo clippy --workspace --all-targets` — clean. `cargo fmt --all --check` — clean.
- `runtime_module_map`, `runtime_doc_paths`, `runtime_visibility`,
  `traceability_sweep` — all green.
- **Bodies byte-identical**: the five moved spans extracted from
  `origin/main:runtime/mod.rs` and compared against `session.rs`'s `impl` block
  — 335 non-blank lines, exact match. The moved test likewise.
- **One adaptation, not a body change**: `scratch_root`'s local import became
  `use std::sync::atomic::{AtomicU64, Ordering};` because `Ordering` reached it
  through `use super::*` in its old home and does not in `testsupport.rs`.

**Mutations run against the map guard** (LESSON-598 — a guard that has stopped
covering its subject looks exactly like one that passes; adding a module is
precisely the structural change that can break contact):

| mutation | observed |
|---|---|
| `session.rs` row count 478 → 200 | `session.rs: map says 200, tree has 478 (58% off)` |
| delete the `session.rs` row | `these modules exist but the architecture doc's map does not mention them … ["session.rs"]` |

Both reverted; the suite is green as committed. The guard is in contact with
the new module by observation, not by assumption.

## Expected outcome (as planned, before measurement)

`mod.rs` loses ~405 production lines (the 3178–3587 run less `projects()`'s 5),
landing near **7,015**. That is a 5.5% reduction and leaves `mod.rs` seven times
over the architecture doc's 1,000-line target, so REQ-599's AC-1 stays **NOT
MET** — restated, not lowered. The exact after-figure is measured and reported
in TASK-318 rather than predicted here.

ADR-4 of REQ-599 estimated this slice at ~900 lines. The re-measured production
figure is ~405; with the ten tests and their fixtures the subsystem is ~1,700
lines of file. The estimate was not re-derived at the time, which is the whole
reason the requirement asked for a re-measurement rather than trust.

## Risks

- **The nine tests that stay** describe methods in another module. Mitigated by
  the module header naming them and the reason, per AC-5 / REQ-599 BR-7 — but it
  is a real residue and is recorded as such, not as a clean outcome.
- **`CRATE_WIDE` 4 → 5** weakens REQ-602's headline number. It is a corpus
  change rather than a widening, but a reader skimming the ratchet sees a
  larger surface; the comment must carry the distinction or the next reader
  will draw the wrong conclusion.
