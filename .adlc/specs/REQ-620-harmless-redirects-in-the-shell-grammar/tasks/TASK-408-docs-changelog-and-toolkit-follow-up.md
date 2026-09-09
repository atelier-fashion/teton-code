---
id: TASK-408
title: "Docs: ADR-614-1 amendment carried to the architecture index, CHANGELOG entry, and the toolkit conventions follow-up"
status: draft
parent: REQ-620
created: 2026-09-09
updated: 2026-09-09
dependencies: ["TASK-406", "TASK-407"]
repo: teton-code
---

## Description

Record what shipped where readers look for it: the project architecture index, the
changelog's upgrade notes (the prompt-margin raise changes every redact-scanning route's
budget), and a filed follow-up for adlc-toolkit's `conventions.md`, whose preamble grammar
paragraph (BUG-220) says pipes into readers and `2>/dev/null` pin — true until this REQ,
false after.

## Files to Create/Modify

- `.adlc/context/architecture.md` — the bold lead-in under ADR-614-1 (text in `architecture.md` of this REQ, "Proposed additions")
- `CHANGELOG.md` — `## [Unreleased]` → Added: the grammar widening and the contract; Changed: `REDACT_BODY_OVERHEAD_BYTES` 23 → 24 KiB and what that does to redact-scanning routes
- `docs/manual-verification.md` — a row for the cleared-command check (the 2026-09-09 command, minus the `~/bin` probe, on a remote route)
- `.adlc/specs/REQ-620-harmless-redirects-in-the-shell-grammar/requirement.md` — Deferred section if any AC was descoped

## Acceptance Criteria

- [ ] The architecture index's ADR-614-1 names the three amendments and the date
- [ ] The changelog entry states the upgrade consequence of the margin raise in one sentence
- [ ] A toolkit issue or BUG is filed (`adlc_alloc_id bug` in the toolkit repo) to loosen `conventions.md`'s "no `| head`, no `2>/dev/null`" rule to "on Teton Code ≥ the release carrying REQ-620"; its id is recorded in this task
- [ ] No source file changed by this task

## Verification

| rule | kind | artifact | benign_path |
|------|------|----------|-------------|
| AC-9 | structural-check | `cargo test -p tetond --lib shell_provenance` (the toolkit preamble table stays green) | no |

## Technical Notes

- House style for ADR extensions: a bold lead-in with the REQ id and date inside the
  parent ADR block, not a new ADR number in the index.
- The toolkit follow-up is filed, not done, from this repo: `~/.claude/skills` symlinks to
  the toolkit checkout, and its conventions file is that repo's to change.
