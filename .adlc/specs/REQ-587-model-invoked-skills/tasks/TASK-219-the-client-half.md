---
id: TASK-219
title: "The client: a `(model-only)` mark, an acknowledgment prompt, and who invoked it"
status: draft
parent: REQ-587
created: 2026-08-20
updated: 2026-08-20
dependencies: [TASK-210, TASK-212]
---

## Description

BR-3's `/help` mark, BR-4's acknowledgment prompt and its pipe rule, BR-9's echo
line.

## Files to Create/Modify

- `crates/teton/src/slash.rs` — `skill_row`'s `(model-only)` mark; `classify`'s hint for a model-only name
- `crates/teton/src/session_ui.rs` — the acknowledgment prompt, `consent_gate`'s new subject, `skill_echo_line`'s "invoked by the model", `SessionGrants` expiry

## Acceptance Criteria

- [ ] `/help` marks a model-only skill in the source parenthetical, from `SkillView`'s flags. `shadow_reason` is two-valued today and BR-3 has three states — take whatever TASK-212 decided; do not invent a second predicate here.
- [ ] `consent_gate` and `render_consent_subject` are **exhaustive matches** over `PermissionSubject`, by design — the new variant is a compile error at both until handled, which is the guard working.
- [ ] The pipe rule extends to the acknowledgment: no terminal ⇒ refuse **without reading stdin**, returning `Refused { reason }`, never `Cancelled`. The negative pin is the one that matters — assert the next stdin line arrives as a prompt line.
- [ ] `SessionGrants` expires the acknowledgment key on an own-session root move, using TASK-210's shared predicate, at the same moment the daemon does (ASSUME-017).
- [ ] `skill_echo_line` says the model invoked it, in the **shipped spellings**: `format_bytes` (so `KiB`) and both counts when they differ (`3 dynamic commands, 1 run`).
- [ ] The client still parses **no** permission key — the source scan asserting it stays green with the new subject.
- [ ] Mutation: rendering the mark from a re-derived predicate, and treating the new subject as answerable on a pipe, each fail a named test.

## Technical Notes

- `/help`'s row assertions are exact-text: the fixtures at `cli_e2e.rs` and `slash.rs` pin whole rows, so a mark inside the parenthetical moves several goldens. Widen them.
