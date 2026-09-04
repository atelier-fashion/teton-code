---
id: TASK-389
title: "Docs: the offer and setting in the `context` topic, README, the dogfood runbook, and architecture patterns"
status: complete
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

- [x] AC-12: `every_topic_serves_its_whole_bundled_body` green with the grown topic and its
      byte ceiling checked first (13,071 bytes against the 50,000-byte `MAX_TOPIC_BYTES`); the
      README rows are found by `cli_rows.rs` — the `/context init [--force]` row parses to
      `context init` and is the one row `slash::COMMANDS` does not yet carry, which TASK-387
      adds. That single assertion is red on purpose until it lands; every other row resolves.
- [x] The runbook leg exists with prompt, rubric and `OUTSTANDING`.
- [x] `TOPIC_INDEX` byte-identical — no topic was added or renamed.

## Verification

| rule | kind | artifact | benign_path |
|------|------|----------|-------------|
| BR-10 | structural-check | `crates/tetond/src/harness/tools/docs.rs::every_topic_serves_its_whole_bundled_body` over `docs/context.md`; `crates/teton/src/cli_rows.rs` README cross-check | no |
| AC-12 | structural-check | `crates/tetond/src/harness/tools/docs.rs::every_topic_serves_its_whole_bundled_body`; `crates/teton/src/cli_rows.rs` README cross-check | no |

## Technical Notes

AC-13 is a by-hand dogfood and carries no obligation row on purpose; the runbook cell is where
its result lands and `/wrapup` reads it.

## Implementation Notes

Written against the landed tier-0/1 commits (TASK-379 through TASK-385) for every fact the
surfaces state: the `[context] generate` values and the `ask` default, the ten
`GenerationOutcome` wire words, `ContextAction::Init { force }`, the `repo_context:generate:<root>`
key and its level table, the two evidence tables with their 16 KiB / 4 KiB ceilings and the
100,000-entry / 10-second walk budget, `Category::Draft` bound to `Think` with
`/policy set-category draft <tier>`, the `generated_header` golden and mode `0644`, and the four
failure stages. TASK-386 had **not** landed at commit time, so the state names, the first-turn
seam and the `Init` refusal are written to the spec's BR-1/BR-8 and ADR-1/ADR-6 wording; the
CLI surfaces TASK-387 owns (the `/context init` row, `teton context generate <mode>`, the banner
clause, the two doctor advisories in `doctor.md`) are documented ahead of that commit by design —
the README check is the coordination.
