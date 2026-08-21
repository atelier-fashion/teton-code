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

- [ ] `/help` marks a model-only skill in the source parenthetical, from `SkillView`'s flags. **TASK-212 decided it (2026-08-21) — take this, do not invent a second predicate.** `shadow_reason` stays two-valued and shadowing-only, because folding model-only into it would render "model-only" inside a `shadowed by …` sentence at every surface. The three-valued answer is `UserDispatch { Allowed, Shadowed(ShadowedBy), ModelOnly }` via `Skill::user_dispatch()`, and it fixes the precedence: **shadowing wins over model-only**. On the wire the two facts ride verbatim and separately — `shadowed` and `user_invocable` — so compose them in that order.
- [ ] **BR-3's third state has no mark yet.** A row both flags deny — listed, invocable by nobody — is representable and reaches the wire (pinned by `a_row_both_flags_deny_is_listed_and_invocable_by_nobody` and `both_invocation_flags_reach_the_client`), but nothing *renders* the combination as its own mark. `/help` is the only surface that can. Give it one, or state why the `(model-only)` mark is the right answer for a row the model cannot invoke either.
- [ ] `consent_gate` and `render_consent_subject` are **exhaustive matches** over `PermissionSubject`, by design — the new variant was a compile error at both until handled, which is the guard working. **That pressure is already spent (2026-08-21):** TASK-210 had to restore workspace compilation, so both arms exist today as fail-closed placeholders — `consent_gate` returns `ConsentGate::RefuseUnrecognized` (chosen over the `SkillDynamicContext` row deliberately: `render_consent_subject` cannot draw this subject yet, and that function's own doc forbids a silently blank prompt, so the fail-closed arm costs a skill invocation while the permissive one would put an unrendered question to a human), and `render_consent_subject` renders nothing, grouped with `Unrecognized`. Both carry a `TASK-219 OWNS THIS ARM AND MUST REPLACE IT` block comment. **The compiler will not remind you — grep for that marker.** Shipping either placeholder means BR-4's acknowledgment can never be granted.
- [ ] **Four `#[cfg(test)]` fixtures also carry placeholder values** added on 2026-08-21 to restore `--all-targets`: `client.rs:~2157` and `slash.rs:~2881` (`model_invocable: false, user_invocable: true`), `session_ui.rs:~7205` and `~:7517` (`invoked_by: events::InvokedBy::User`). They are the REQ-585 world exactly, which is correct for the tests that exist — but a `(model-only)` mark cannot be asserted from a fixture whose `model_invocable` is hard-`false`. Vary them.
- [ ] The pipe rule extends to the acknowledgment: no terminal ⇒ refuse **without reading stdin**, returning `Refused { reason }`, never `Cancelled`. The negative pin is the one that matters — assert the next stdin line arrives as a prompt line.
- [ ] `SessionGrants` expires the acknowledgment key on an own-session root move, using TASK-210's shared predicate, at the same moment the daemon does (ASSUME-017). **The two stores disagree in the tree right now**: `SessionGrants::forget_project_skills` (`session_ui.rs:~97`) retains on `is_project_skill_key`, so it keeps the acknowledgment across a `/cd` while the daemon — since TASK-215 — drops it. That is REQ-585's finding 2 reproduced on the one key ASSUME-017 was written for: the client would auto-answer the new root's question from the old root's grant, with no human shown anything. Switch it to `expires_on_session_root_change` and assert both stores at the same moment.
- [ ] `skill_echo_line` says the model invoked it, in the **shipped spellings**: `format_bytes` (so `KiB`) and both counts when they differ (`3 dynamic commands, 1 run`). **Done as of 2026-08-21 except three facts no event carries** — BR-9's shadowing clause in the echo line, and `/verbose`'s flags, shadowing fact and turn count. `SkillInvoked` has none of them and `render_event` sees only `SessionState`, so the client cannot derive them. TASK-217 adds the wire fields; **this task resumes after it** to render them. Until then BR-9 and AC-10 are partly unmet, and the tests are green anyway.
- [ ] The client still parses **no** permission key — the source scan asserting it stays green with the new subject.
- [ ] Mutation: rendering the mark from a re-derived predicate, and treating the new subject as answerable on a pipe, each fail a named test.

## Technical Notes

- `/help`'s row assertions are exact-text: the fixtures at `cli_e2e.rs` and `slash.rs` pin whole rows, so a mark inside the parenthetical moves several goldens. Widen them.
