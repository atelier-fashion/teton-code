---
id: TASK-389
title: "Docs: the offer and setting in the `context` topic, README, the dogfood runbook, and architecture patterns"
status: draft
parent: REQ-613
repo: teton-code
created: 2026-09-03
updated: 2026-09-03
dependencies: [TASK-386]
---

## Description

BR-10's documentation half and AC-12, plus the AC-13 dogfood leg. The `context` topic documents
the offer, `generate`, both `init` doors and the unattended sentence so a model asked "why is
there a TETON.md I didn't write?" answers from a resident fact; the README's `[context]`
paragraph names `generate`; the runbook carries the quality leg; the architecture context gains
the two patterns.

## Files to Create/Modify

- `crates/tetond/src/harness/docs/context.md` — the generation section (offer, setting, doors,
  unattended posture, cost shape, the `always` breadth stated plainly).
- `README.md` — `/context init [--force]` row; the `[context] generate` paragraph.
- `docs/manual-verification.md` — the AC-13 leg with the exact prompt, the reading rubric, and
  an `OUTSTANDING` result cell.
- `.adlc/context/architecture.md` — patterns: *a repository-touching act with no human typing a
  name gets its own gate entry point keyed by the durable root* (ADR-2); *a once-per-repository
  model call defaults to the deep-reasoning tier* (ADR-4).

## Acceptance Criteria

- [ ] AC-12: `every_topic_serves_its_whole_bundled_body` green with the grown topic and its
      byte ceiling checked first; the README rows are found by `cli_rows.rs`.
- [ ] The runbook leg exists with prompt, rubric and `OUTSTANDING`.
- [ ] `TOPIC_INDEX` byte-identical.

## Verification

| rule | kind | artifact | benign_path |
|------|------|----------|-------------|
| BR-10 | structural-check | `crates/tetond/src/harness/tools/docs.rs::every_topic_serves_its_whole_bundled_body` over `docs/context.md`; `crates/teton/src/cli_rows.rs` README cross-check | no |
| AC-12 | structural-check | `crates/tetond/src/harness/tools/docs.rs::every_topic_serves_its_whole_bundled_body`; `crates/teton/src/cli_rows.rs` README cross-check | no |

## Technical Notes

AC-13 is a by-hand dogfood and carries no obligation row on purpose; the runbook cell is where
its result lands and `/wrapup` reads it.
