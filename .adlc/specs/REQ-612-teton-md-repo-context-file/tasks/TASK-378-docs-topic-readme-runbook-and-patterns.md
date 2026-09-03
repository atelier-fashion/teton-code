---
id: TASK-378
title: "Docs: the `context` topic section, README rows, doctor topic, the dogfood runbook, and architecture patterns"
status: draft
parent: REQ-612
repo: teton-code
created: 2026-09-03
updated: 2026-09-03
dependencies: [TASK-375]
---

## Description

BR-7's stated cost shape and AC-12, plus the AC-13 dogfood leg recorded where by-hand
verification lives. No new `teton_docs` topic (ADR-8): the `context` topic gains a section and
loses its stale "25 tool iterations" figure. Runs after the ceiling task so the numbers it
states are the shipped ones.

## Files to Create/Modify

- `crates/tetond/src/harness/docs/context.md` — a "Repository notes" section: the file names
  and precedence, the cap, truncation, the switches, the reload rule, the boundary rule, and
  the cost shape (`max_turns` 12 local / 40 strong × the block; up to a quarter of the local
  byte budget; the overhead consequence for redact-scanning routes). Fix "25" to name
  `max_turns`. State the MEASURED redact consequence from TASK-375's ledger entry, not the
  spec's prediction: overhead 23 KiB, chunk cap 4, scannable bound up to 184,265, scan calls
  up to 5. Also update `docs/manual-verification.md:2214`, which still quotes the pre-REQ
  capability sentence (TASK-375 flagged it).
- `crates/tetond/src/harness/docs/doctor.md` — the two advisories.
- `README.md` — `/context [on|off]` in the session-command table; a short "TETON.md" paragraph
  under the session section.
- `docs/manual-verification.md` — the AC-13 leg: this repository with a `TETON.md` describing
  the crate layout; first prompt "where does the system prompt get built?"; count `glob`/`grep`
  calls with and without the file on the local tier; record the result and date.
- `.adlc/context/architecture.md` — two Key Patterns entries: *a system-prompt block that comes
  from a file carries the file's identity at the manager* (ADR-2) and *repository text in the
  system prompt is the last region and is framed as description* (ADR-1/ADR-4); a note on the
  `REDACT_BODY_OVERHEAD_BYTES` ledger entry.

## Acceptance Criteria

- [ ] AC-12: `every_topic_serves_its_whole_bundled_body` green with the grown topic; the README
      row is found by `cli_rows.rs`; the overhead consequence is stated.
- [ ] The runbook leg exists with the exact prompt and the counting method; the result cell is
      `OUTSTANDING` until run by hand.
- [ ] No topic-index change (`TOPIC_INDEX` byte-identical).

## Verification

| rule | kind | artifact | benign_path |
|------|------|----------|-------------|
| BR-7 | structural-check | `crates/tetond/src/harness/tools/docs.rs::every_topic_serves_its_whole_bundled_body` over `docs/context.md`; `crates/teton/src/cli_rows.rs` README cross-check | no |
| AC-12 | structural-check | `crates/tetond/src/harness/tools/docs.rs::every_topic_serves_its_whole_bundled_body`; `crates/teton/src/cli_rows.rs` README cross-check | no |

## Technical Notes

AC-13 is a by-hand dogfood and carries no obligation row on purpose: `dogfood` is not an
obligation kind (it reports no executed-work count). The runbook cell is where its result is
recorded, and `/wrapup` reads it. The `context` topic has a byte ceiling of its own — check it
before writing the section (LESSON-543's habit).
