---
id: REQ-604
title: "The turn-ordering fixture covers a plain turn only — capture the skill and consent scenarios REQ-599 AC-6 named"
status: complete
deployable: false
created: 2026-08-31
updated: 2026-09-01
component: "daemon/runtime"
domain: "testing"
stack: ["rust", "daemon"]
concerns: ["reliability", "maintainability"]
tags: ["golden-fixture", "event-ordering", "req-599-followup", "lesson-569", "lesson-591"]
---

## Description

REQ-599 AC-6 claimed an event-sequence fixture recorded before the split replays
identically after it, "for a turn that exercises: skill expansion, a routing
decision, a consent prompt, and a successful dispatch".

The fixture — `crates/tetond/tests/fixtures/req598_turn_event_order.txt` —
records a plain typed prompt turn:

```
route_decided
session_titled
route_decided
session_update
prefix_cache
```

No skill expansion. No consent prompt. The criterion was ticked on the fixture
holding across all seven steps, which it did, for the scenario it contains.
REQ-602 TASK-306 narrowed AC-6 to what shipped and filed this REQ for the rest.

**Why this could not simply be fixed at the time.** The fixture's whole value is
its provenance: captured at `17c39ec`, before any `TurnContext` existed.
Regenerating it at today's tip would produce a golden file computed by the
subject it checks, which is not an oracle (LESSON-569, and the fixture's own
header says so). So the missing scenarios cannot be added by recording them now.

**Why it is nonetheless doable.** `17c39ec` is still in history and still
carries the `carry_runtime` and `prompt` helpers. A capture harness can be built
at that commit and the sequences recorded there, which is a genuine pre-split
recording rather than a re-derivation.

**Why it is worth the cost.** Ordering on this path has already failed once, and
failed in the way that is hardest to catch: LESSON-591's detached-naming race
passed 40/40 locally and on `ubuntu-latest`, and went red only on
`macos-latest`. The two uncovered scenarios both add events to the same
sequence. The behaviours are covered — skill-turn routing in
`crates/tetond/tests/skill_turn.rs`, consent isolation in
`crates/tetond/tests/skill_consent_matrix.rs` — but neither pins *where* their
events fall in a turn.

## Acceptance Criteria

- [x] AC-1: Two sequences are captured **at `17c39ec`**, not at tip: one turn that
      expands a skill and dispatches, one that raises a consent prompt, is
      answered, and dispatches. The capture commit is recorded in each
      fixture's header, as the existing fixture records its own.
- [x] AC-2: Each new fixture replays identically against the current tree.
- [x] AC-3 **(N/A — the condition did not arise; see note below)**: **If one does not replay, the disposition is decided on evidence and
      recorded — never by regenerating.** These scenarios have never been
      pinned, so a sequence that changed between `17c39ec` and tip is a real
      possibility and not automatically a bug. Exactly two outcomes are
      permitted, and each names what it rests on:
  - **Regression.** The ordering was load-bearing and something moved it. Fix
    the code; the fixture stands as captured.
  - **Intended change.** A REQ between `17c39ec` and tip deliberately changed
    the sequence. Name that REQ and the criterion that authorised it, record the
    delta in the fixture header beside its provenance, and pin the new sequence
    as *captured sequence plus stated delta* — never by re-recording at tip,
    which is the oracle problem LESSON-569 names and the reason this REQ exists
    at all.
  - **Default when neither can be shown: regression.** An unexplained delta is
    not evidence of intent, and "it must have been deliberate" is the reasoning
    that ticked REQ-599's AC-6 in the first place.
- [x] AC-4: Detached events are excluded **by discriminator, not by position** —
      LESSON-591: `session_titled` and the title duty's own `route_decided` are
      both published from inside a `tokio::spawn` and their arrival order is a
      race. Any new detached event these scenarios introduce is identified the
      same way.
- [x] AC-5: Non-vacuity: each fixture asserts a positive count of the events it exists
      to pin, so a filter that ate everything cannot pass (the existing test's
      "exactly ONE route decision survives" assertion is the model).
- [x] AC-6: A transposition of two adjacent distinct events still fails, per scenario
      — the normalizer must not have been widened into an excuse.
- [x] AC-7: Suite green, grepped for `FAILED`; clippy and `fmt --check` clean.

## Outcome — merged as `4b1d22c` (PR #258)

**Both captured sequences replayed against the current tree on the first run,
unmodified.** Neither fixture was regenerated at any point.

### AC-3 is N/A, not met

AC-3 governs what to do *if* a captured sequence does not replay. No sequence
failed to replay, so its trigger never fired and its machinery was never
exercised. A criterion whose condition did not arise is not a criterion met, and
recording it as met would be the same move that ticked REQ-599's AC-6 — claiming
evidence for something never tested. It is marked N/A deliberately.

What that costs: the disposition protocol in ADR-7 is **unexercised**. It was
written before the replay was run, precisely so the decision could not be made
after seeing a red test, but it has never been executed. The next REQ that
inherits it should treat it as a design, not as a proven procedure.

### What the green result does establish

The four refactors between `17c39ec` and tip — REQ-598 (`TurnContext`), REQ-599
(decomposing `runtime.rs`), REQ-600 (the eight-stage split) and REQ-602
(post-split cleanup) — each claimed to preserve behaviour. Each claim was
previously evidenced on the **plain typed turn alone**, because that is the only
scenario the REQ-598 fixture contains. Skill-expansion and consent orderings now
carry pre-split evidence too. REQ-603's session-lifecycle extraction, which
landed between this branch's Phase 7 and its merge, is covered as well: the
fixtures were re-run against merged `main` and are green.

### Verification (re-run from an isolated path after the scratchpad finding below)

Against merged `main` = `4b1d22c`, post-REQ-603:

- `cargo test --workspace --no-fail-fast` — exit 0, **0** occurrences of
  `FAILED`, 74 targets, **4,078** passed.
- `cargo clippy --workspace --all-targets` — clean under `deny`; no new
  `#[allow(...)]`.
- `cargo fmt --check` — clean.
- The four `req604_event_order` tests — 4 passed.
- CI on the rebased head `b279cc9` — 7/7 green, including
  `fmt · clippy · test (macos-latest)`, the runner LESSON-591's race went red on.

### Evidence-integrity note (disclosed, not buried)

The `/sprint` scratchpad is session-specific but **not agent-specific**, so the
three concurrent runners shared one directory. This runner wrote workspace test
output to generic names (`suite.txt`, `suite2.txt`, `suite3.txt`) and clobbered
REQ-606's results file, which is how REQ-606 came to report "56 passed" from a
run that was not its own.

REQ-604's own figures were checked rather than assumed after the fact: each file
contained four `req604_event_order` hits, zero REQ-606 references, and only
`.worktrees/REQ-604` paths, and the shared `CARGO_TARGET_DIR` had built for this
worktree only. The numbers were nonetheless **re-derived from an isolated path**
on merged `main`, and came back identical (4,078 / 0 `FAILED` / 74 targets). The
authoritative evidence — CI — was never in the scratchpad at all.

Captured as LESSON-610. This runner caused the collision; it did not suffer it.

## Assumptions

- ~~`17c39ec` still builds.~~ **Verified.** `cargo build --tests -p tetond` is
  clean at that commit and its 1,932 lib tests run, so the fallback outcome
  ("record that the scenarios cannot be captured and say what covers them
  instead") was not needed.

## Out of Scope

- Regenerating the existing fixture. It is correct for its scenario and its
  provenance is the reason it is worth anything.
- Changing turn-path behaviour.

## External Dependencies

None.
