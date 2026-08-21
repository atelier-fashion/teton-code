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
- [ ] `consent_gate` and `render_consent_subject` are **exhaustive matches** over `PermissionSubject`, by design — the new variant was a compile error at both until handled, which is the guard working. **That pressure is already spent (2026-08-21):** TASK-210 had to restore workspace compilation, so both arms exist today as fail-closed placeholders — `consent_gate` returns `ConsentGate::RefuseUnrecognized` (chosen over the `SkillDynamicContext` row deliberately: `render_consent_subject` cannot draw this subject yet, and that function's own doc forbids a silently blank prompt, so the fail-closed arm costs a skill invocation while the permissive one would put an unrendered question to a human), and `render_consent_subject` renders nothing, grouped with `Unrecognized`. Both carry a `TASK-219 OWNS THIS ARM AND MUST REPLACE IT` block comment. **The compiler will not remind you — grep for that marker.** Shipping either placeholder means BR-4's acknowledgment can never be granted.
- [ ] **Four `#[cfg(test)]` fixtures also carry placeholder values** added on 2026-08-21 to restore `--all-targets`: `client.rs:~2157` and `slash.rs:~2881` (`model_invocable: false, user_invocable: true`), `session_ui.rs:~7205` and `~:7517` (`invoked_by: events::InvokedBy::User`). They are the REQ-585 world exactly, which is correct for the tests that exist — but a `(model-only)` mark cannot be asserted from a fixture whose `model_invocable` is hard-`false`. Vary them.
- [ ] The pipe rule extends to the acknowledgment: no terminal ⇒ refuse **without reading stdin**, returning `Refused { reason }`, never `Cancelled`. The negative pin is the one that matters — assert the next stdin line arrives as a prompt line.
- [ ] `SessionGrants` expires the acknowledgment key on an own-session root move, using TASK-210's shared predicate, at the same moment the daemon does (ASSUME-017).
- [ ] `skill_echo_line` says the model invoked it, in the **shipped spellings**: `format_bytes` (so `KiB`) and both counts when they differ (`3 dynamic commands, 1 run`).
- [ ] The client still parses **no** permission key — the source scan asserting it stays green with the new subject.
- [ ] Mutation: rendering the mark from a re-derived predicate, and treating the new subject as answerable on a pipe, each fail a named test.

## Technical Notes

- `/help`'s row assertions are exact-text: the fixtures at `cli_e2e.rs` and `slash.rs` pin whole rows, so a mark inside the parenthetical moves several goldens. Widen them.
