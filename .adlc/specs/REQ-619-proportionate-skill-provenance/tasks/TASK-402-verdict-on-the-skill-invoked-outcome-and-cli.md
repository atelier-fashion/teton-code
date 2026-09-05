---
id: TASK-402
title: "The verdict rides `skill_invoked` additively, and the CLI renders a non-rooted reason under /verbose"
status: draft
parent: REQ-619
created: 2026-09-05
updated: 2026-09-05
dependencies: [TASK-399]
---

## Description

BR-7 (ADR-619-5). `DynamicOutcomeView` gains `reach: Option<Reach>`
(`rooted` / `boundary_touch` / `unknown`) and `reach_reason: Option<String>`,
both `serde(default)`. `outcome_view` fills them from the `PreambleRun`
verdict. The CLI's `skill_invoked` renderer prints `reach: <kind> — <reason>`
per command under `/verbose` when the reach is not `rooted`. The transcript
format document lists the two fields.

## Files to Create/Modify

- `crates/teton-protocol/src/events.rs` — `Reach` enum, two fields on `DynamicOutcomeView`, wire tests (round-trip, absent-means-`None`, the REQ-588 forward-compatible vocabulary test extended)
- `crates/tetond/src/skills/dynamic.rs` — `outcome_view(command, run, door)` fills the fields
- `crates/teton/src/session_ui.rs` — render under `/verbose`; a test that a `rooted` outcome prints no reach line and an `unknown` one prints the reason
- `docs/transcript-format.md` — the two fields under `skill_invoked`
- `crates/tetond/src/harness/docs/*.md` — the skills topic, if it describes what `skill_invoked` carries

## Acceptance Criteria

- [ ] A `skill_invoked` record serialised by this daemon carries `reach` and `reach_reason` per command; one serialised without them deserialises with both `None`
- [ ] The reason is the classifier's static sentence; a test asserts the wire value equals `Verdict::reason` and contains no substituted command text
- [ ] `/verbose` prints one reach line per non-rooted command; no line for a rooted one
- [ ] `cargo test -p teton-protocol -p teton -p tetond` green

## Verification

| rule | kind | artifact | benign_path |
|------|------|----------|-------------|
| BR-7 | test-case | `crates/teton-protocol/src/events.rs::tests::a_skill_outcome_carries_its_reach_additively` | yes |
| BR-7 | test-case | `crates/tetond/src/skills/dynamic.rs::tests::outcome_view_carries_the_verdict_and_nothing_of_the_output` | yes |
| BR-7 | test-case | `crates/teton/src/session_ui.rs::tests::a_non_rooted_preamble_prints_its_reason_under_verbose_and_a_rooted_one_prints_nothing` | yes |
| BR-8 | test-case | `crates/teton/src/session_ui.rs::tests::a_non_rooted_preamble_prints_its_reason_under_verbose_and_a_rooted_one_prints_nothing` | yes |

## Technical Notes

- Additive only (REQ-588's vocabulary rule): older clients ignore unknown fields; older daemons omit them. Do not touch `DynamicOutcome`.
- The CLI renders the `session_pinned` line already (BUG-214); this task adds only the per-command reason so a user can see *which* preamble pinned.
