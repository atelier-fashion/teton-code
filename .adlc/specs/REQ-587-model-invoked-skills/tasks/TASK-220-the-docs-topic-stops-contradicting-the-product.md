---
id: TASK-220
title: "The bundled topic says the model cannot invoke a skill — in the binary that hands it a `skill` tool"
status: complete
parent: REQ-587
created: 2026-08-20
updated: 2026-08-20
dependencies: []
---

## Description

AC-16. `teton_docs`'s `skills` topic is what the model reads when it asks what
skills are. It currently contradicts this REQ in four places. That is BUG-181's
defect with the sign flipped, on the one surface REQ-577 shipped so the model
would stop guessing.

## Files to Create/Modify

- `crates/tetond/src/harness/docs/skills.md` — the four passages
- `crates/tetond/src/harness/tools/docs.rs` — needle assertions on the topic's content

## Acceptance Criteria

- [x] All four amended: *"The model cannot invoke a skill: name it and let the user type it"*; *"every other key … is inert"* (BR-3 makes two meaningful); *"stalls at its first 'invoke the skill' step"* (this REQ is what unstalls it); and the `## Provenance` paragraph, which is the pre-BR-10 rule. Two more are quietly false and worth the same pass: "one model with **five tools**" and "`/name <rest>` is exactly one user-role prompt turn", which is now silent on the second caller.
- [x] **Still under `MAX_TOPIC_BYTES`.** The topic is **4,087 of 4,096** — nine bytes. The amendment buys its room by **cutting**, not by moving the ceiling and not by splitting into a seventh topic, which would cost `DESCRIPTION` a name out of its remaining 18 characters.
  **Current figure (2026-08-21, after TASK-216's conditional-registration clause and the
  AC-16 safe-reading correction): 4,092 of 4,096 — four bytes.** Both later amendments paid
  by cutting; the ceiling never moved and `every_bundled_topic_is_under_the_ceiling` still
  guards it. The next amendment to this topic must cut first — four bytes is one word.
- [x] **Needle assertions, because the byte check is the easy half.** The existing sweep asserts only `len > 500 && <= 4096` — nothing asserts content, so all four sentences could survive with CI green. Add assertions that the topic no longer denies model invocation, no longer calls the two flags inert, no longer says a skill-invoking skill stalls, and states BR-10's two provenance rules.
- [x] Mutation: restoring any one of the four sentences fails a named needle.

## Technical Notes

- Nine bytes is the constraint that shapes the edit: write the replacements first, measure, then cut elsewhere in the topic to pay for them. `every_bundled_topic_is_under_the_ceiling`'s failure message says explicitly not to raise the ceiling or delete the assertion.
