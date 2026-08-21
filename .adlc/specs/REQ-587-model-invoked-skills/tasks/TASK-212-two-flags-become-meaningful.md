---
id: TASK-212
title: "Two frontmatter keys stop being inert, and a skill gains a third state"
status: draft
parent: REQ-587
created: 2026-08-20
updated: 2026-08-20
dependencies: []
---

## Description

BR-3. `disable-model-invocation` and `user-invocable` leave `ignored_keys` and
become typed flags, which gives a skill three states rather than two:
dispatchable, model-only, and listed-but-neither.

## Files to Create/Modify

- `crates/tetond/src/skills/frontmatter.rs` — two arms in the `match key`, boolean coercion, the diagnostic for a non-literal
- `crates/tetond/src/skills/mod.rs` — `Skill.model_invocable` / `.user_invocable`; `is_dispatchable` and `shadow_reason` gain the third state
- `crates/tetond/src/skills/discovery.rs` — carry both through `assemble`

## Acceptance Criteria

- [ ] Both keys parse as boolean **literals**; anything else takes the **safe** value and is named in the diagnostics rather than silently ignored. There is no boolean coercion anywhere in this module today, so this is new code and the safe value must be stated per flag: an unparseable `disable-model-invocation` means *not* model-invocable; an unparseable `user-invocable` means *still* user-invocable.
- [ ] They leave `ignored_keys`. Exactly one surface renders that list (`session_ui.rs`'s `/verbose` line) and four tests assert it — update them, do not widen the list.
- [ ] **`user-invocable: false` must actually stop `/name` dispatching.** `SkillRegistry::dispatchable` filters only on `is_dispatchable()` today, i.e. `shadowed.is_none()`, so the flag is inert unless that predicate or `accept_invocation` gains the branch. Assert the `/name` refusal, not just the flag's presence.
- [ ] Both flags reach `SkillView` (TASK-210's fields) so `/help` can mark `(model-only)`.
- [ ] Mutation: dropping either flag from `assemble`, and leaving `dispatchable` unchanged, each fail a named test.

## Technical Notes

- `frontmatter::parse` is total: malformed skips the file whole. A bad *value* on a known key is not malformed — the file still registers, with the safe value and a diagnostic. Say which in the module doc.
- `shadow_reason` returns `Option<String>` — two-valued. Three states need either a third return or a separate predicate; decide it here rather than in the client (TASK-219 consumes whatever this task decides).
