---
id: TASK-378
title: "Docs: the `context` topic section, README rows, doctor topic, the dogfood runbook, and architecture patterns"
status: complete
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

- [x] AC-12: `every_topic_serves_its_whole_bundled_body` green with the grown topic (the
      `context` topic was 4,074 bytes against the 4,096-byte `MAX_TOPIC_BYTES`, paid for by
      trimming the same file; `doctor` was 4,068).

      **Amended by the product owner, 2026-09-03.** That "paid for by trimming the same file"
      cost four true facts out of `context.md` — the base64 class neither budget guard covers,
      the wire's snake_case bound spellings, `/provider setup`'s window recording and the
      inert-cap advisory, the 256,000-token big-window notice, and the "a context the gate could
      not fit says so once per turn" line — plus prose trims in `doctor.md`. The owner rejected
      that trade: `MAX_TOPIC_BYTES` is raised 4,096 → **50,000** (a topic may say everything it
      knows) and every one of those facts is restored beside the new "Repository notes"
      section. `context.md` is now 5,684 bytes and `doctor.md` 4,217, both far inside the new
      ceiling. The ceiling's old justification — that it sat under the harness's digest
      threshold, so a docs read was never condensed — is deliberately abandoned; what bounds a
      docs read now is the `digest` duty itself, which applies to `teton_docs` because its
      outcome is `ResultDisposition::Data` (pinned, both halves, by
      `the_topic_ceiling_is_bounded_by_the_digest_duty`, which replaces
      `the_topic_ceiling_stays_under_the_summarize_threshold`).

      The `/context [on|off]` row is in the README's
      session-command table in the `/transcript` row's shape; the overhead consequence is stated
      in the topic's `[privacy] redact` section, measured (23 KiB, chunk cap 4, bound risen to
      184,265, up to 5 scan calls).
- [x] The runbook leg exists with the exact prompt and the counting method; the result cell is
      `OUTSTANDING` until run by hand.
- [x] No topic-index change (`TOPIC_INDEX` byte-identical — `docs.rs` is untouched).

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
