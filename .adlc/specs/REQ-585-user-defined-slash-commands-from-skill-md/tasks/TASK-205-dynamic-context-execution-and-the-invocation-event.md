---
id: TASK-205
title: "Ask once, run in order, say what did not run — and report the invocation as a typed event"
status: draft
parent: REQ-585
created: 2026-08-20
updated: 2026-08-20
dependencies: [TASK-201, TASK-204]
---

## Description

The consent-and-commands half of BR-6, plus BR-12's observability. Slots into
the ordering TASK-204 established, between Stage A and Stage B.

## Files to Create/Modify

- `crates/tetond/src/runtime.rs` — the consent call, `run_all`, the fold, the `SkillInvoked` publish, Stage B
- `crates/tetond/tests/skill_turn.rs` — the level matrix, the timeout/failure legs, the frame legs

## Acceptance Criteria

- [ ] At `guarded` and `edits`: one `authorize_skill` call per invocation, listing every command verbatim. Declining leaves ``[dynamic context not run: `<cmd>` — declined]`` in every slot and the turn still runs (AC-8).
- [ ] At `plan`: commands are not run and the placeholders name the level. At `full`: they run with no prompt (AC-9).
- [ ] Commands run sequentially in document order with the session root as cwd, through TASK-198's `run_bounded`. A timeout yields a timed-out placeholder; a non-zero exit yields a failed placeholder; the invocation still produces its turn (AC-10).
- [ ] Output enters inside `frame_untrusted_builtin("skill:<name>", …)`, and a planted `<|im_start|>` / `User:` / `<tool-result>` in a command's stdout reaches the frame neutralized. Removing any one guard fails a test (AC-8, AC-12).
- [ ] Dynamic output carries `Unknown` provenance, exactly as `shell` output does — so on a boundary-configured machine an invocation that ran any command pins its turn local (BR-7, AC-11b).
- [ ] Stage B: if the folded expansion now exceeds the budget, refuse with the message that says the dynamic output pushed it over (BR-8d).
- [ ] `Event::SkillInvoked` is published with `name`, `source`, `path_display`, `body_bytes`, `ignored_keys` and per-command `outcomes`. Asserted against the value the **daemon emitted**, never a hand-built literal (LESSON-544).
- [ ] `Tool::refine` is never called on this path — no model call happens at expansion time (BR-4).
- [ ] Mutation table: asking per command instead of per invocation, skipping the frame, running commands out of order, and dropping the `Unknown` provenance each fail a named test.

## Technical Notes

- One consent, many commands. A prompt storm is REQ-560 BR-2's named anti-pattern, and the whole point of `subject.commands` being a list.
- The permission `await` releases the turn to the event loop. Re-read nothing across it that the gate already snapshotted — `decide` snapshots the level at the top for exactly this reason (BR-7 of REQ-560).
