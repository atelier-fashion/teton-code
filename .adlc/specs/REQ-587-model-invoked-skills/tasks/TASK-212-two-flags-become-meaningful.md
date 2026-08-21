---
id: TASK-212
title: "Two frontmatter keys stop being inert, and a skill gains a third state"
status: draft
parent: REQ-587
created: 2026-08-20
updated: 2026-08-20
dependencies: [TASK-210, TASK-214]
---

## Description

BR-3. `disable-model-invocation` and `user-invocable` leave `ignored_keys` and
become typed flags, which gives a skill three states rather than two:
dispatchable, model-only, and listed-but-neither.

## Files to Create/Modify

- `crates/tetond/src/skills/frontmatter.rs` — two arms in the `match key`, boolean coercion, the diagnostic for a non-literal
- `crates/tetond/src/skills/mod.rs` — `Skill.model_invocable` / `.user_invocable`; `is_dispatchable` and `shadow_reason` gain the third state
- `crates/tetond/src/skills/discovery.rs` — carry both through `assemble`
- `crates/tetond/src/skills/expand.rs` — **the second `Skill { … }` literal** (`~:409`, a test fixture). `Skill` has no `Default` and neither literal uses `..`, so adding fields breaks both; this one lives in TASK-214's file, which is why that task is a dependency rather than a neighbour
- `crates/tetond/src/server.rs` — `skills_list_result` (`~:4439`) maps the flags onto `SkillView`; without this step the wire fields stay `false` and TASK-219's `(model-only)` mark is inert with nothing red

## Acceptance Criteria

- [ ] Both keys parse as boolean **literals**; anything else takes the **safe** value and is named in the diagnostics rather than silently ignored. There is no boolean coercion anywhere in this module today, so this is new code and the safe value must be stated per flag: an unparseable `disable-model-invocation` means *not* model-invocable; an unparseable `user-invocable` means *still* user-invocable.
- [ ] They leave `ignored_keys`. Exactly one surface renders that list (`session_ui.rs`'s `/verbose` line) and four tests assert it — update them, do not widen the list.
- [ ] **Two named resolvers, per ADR-12** — `dispatchable_by_user(name)` and `invocable_by_model(name)`. `is_dispatchable()` keeps its meaning (`shadowed.is_none()`) and **neither flag is folded into it**. Folding `user_invocable` in there is the arm that silently kills BR-3's model-only state: `/delta` refuses (AC-12 green), the roster still lists it (AC-1 green), and the model's call returns `unknown_skill` with no AC red.
- [ ] Assert both directions on one fixture: a `user-invocable: false` skill refuses from `/name` **and** resolves for the model.
- [ ] Both flags reach `SkillView` (TASK-210's fields) so `/help` can mark `(model-only)`.
- [ ] Mutation: dropping either flag from `assemble`, and leaving `dispatchable` unchanged, each fail a named test.

## Technical Notes

- `frontmatter::parse` is total: malformed skips the file whole. A bad *value* on a known key is not malformed — the file still registers, with the safe value and a diagnostic. Say which in the module doc.
- `shadow_reason` returns `Option<String>` — two-valued. Three states need either a third return or a separate predicate; decide it here rather than in the client (TASK-219 consumes whatever this task decides).
