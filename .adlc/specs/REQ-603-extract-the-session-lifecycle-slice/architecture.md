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
| `store_session_skills` | 3486–3503, 3510–3549 | the one skill-registry derivation, shared by `session/create` and `set_cwd` |
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

- the six items are contiguous (above);
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
| `store_session_skills` | `pub(crate)` | `pub(crate)` | `server.rs`, `projects/mod.rs`, `projects/scan.rs` — all outside `runtime/`, which is AC-4's stated carve-out |
| `jail_root` | private | private | only `session_root_for` and the moved test |
| `drop_grants_expiring_on_root_change` | private | private | only `set_session_cwd` |

The ratchet updates are therefore bookkeeping of the corpus, and are commented
in exactly the frame REQ-600 used for `turn.rs::run_prompt_turn`:

- `PUBLIC` gains three `session.rs::…` entries; `PUBLIC_DECLARATIONS` 14 → 17.
- `CRATE_WIDE` gains `store_session_skills`, taking the pinned surface 4 → 5.
  The test named for "four" is renamed and the module-doc table updated in the
  same commit, so the file does not assert one number and read another.

AC-4's ratchet is **not** loosened: nothing becomes `pub(crate)` that was
narrower, and nothing becomes `pub` that was narrower. The claim is established
by demoting and building (`cargo check --workspace --all-targets`, reading
`E0603`), never by grepping for the name — LESSON-596, and the method
`runtime_visibility.rs` documents as the only one that has been right.

### ADR-4: One doc block is re-attached, and that is stated rather than silent

Lines 3486–3503 are `store_session_skills`'s doc comment — *"Derive
`session_id`'s skill registry … the one derivation"* — but they are currently
attached to `projects()`, whose own one-line doc sits at 3504 as the last line
of the same block. `store_session_skills` has no doc at all.

The move takes 3486–3503 with `store_session_skills` and leaves 3504 with
`projects()`. This corrects a pre-existing misattachment rather than preserving
it, so it is called out here, in the module header, and in the commit message:
LESSON-599's rule is that a relocation's prose is the one part the compiler and
the whole suite are structurally incapable of checking, so a prose change inside
a relocation gets named or it gets missed.

Verification for AC-3 is `git diff origin/main..HEAD | grep '^[-+].*///'`
reviewed by hand, plus `runtime_doc_paths.rs` staying green.

### ADR-5: Nothing REQ-600 owns is touched

`run_prompt_turn`'s control flow is out of scope (REQ-600, and the requirement's
Out of Scope). No file under `harness/` is touched, and `turn.rs` is not
modified — which also keeps this REQ's rebase surface clear of REQ-606, which is
collapsing `turn.rs`'s parameter bundles concurrently.

The one file shared with the concurrent REQs is the module-map table in
`.adlc/specs/REQ-599-decompose-the-turn-path/architecture.md`. The edit there is
deliberately two lines — one new row, one refreshed count — so REQ-604's and
REQ-606's rebases onto it are mechanical.

## Expected outcome

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
