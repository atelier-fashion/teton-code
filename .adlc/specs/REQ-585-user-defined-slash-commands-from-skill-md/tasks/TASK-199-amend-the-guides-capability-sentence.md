---
id: TASK-199
title: "Amend BUG-181's capability sentence so it is true again, inside the resident prompt's headroom"
status: complete
parent: REQ-585
created: 2026-08-20
updated: 2026-08-20
dependencies: [TASK-197]
---

## Description

BUG-181 put one sentence in the bundled guide: *"Teton loads nothing from
`.claude/` or `~/.claude` (no skills, commands, CLAUDE.md, agents or hooks);
the session's commands are exactly those `/help` lists, and only the user runs
them."* REQ-585 makes the first clause false. BR-9 amends it: skills and
commands from those places **are** loaded and listed by `/help`; `CLAUDE.md`,
agents and hooks still are not; the model still cannot invoke any command.

The pinning test was written to fail loudly on exactly this edit. Update it.

## Files to Create/Modify

- `crates/tetond/src/harness/self_config.md` — line 4
- `crates/tetond/src/harness/turn_loop.rs` — `the_system_prompt_states_what_the_session_can_run_and_from_where` (`:3371`)
- `docs/manual-verification.md` — the headroom table (`:2113-2134`), re-measured

## Acceptance Criteria

- [ ] The amended sentence keeps every pinned property: **one** guide line contains `/help`; it names both `.claude/` and `~/.claude`; it contains "only the user runs"; the `asking`-line count is still 1; no `teton …` shell form; the line's byte offset is before `"\n1. "`; it is resident in `build_system_prompt` for **both** `HarnessConfig::default()` and `::for_strong_model()`.
- [ ] The `"loads nothing from"` assertion (`turn_loop.rs:3396`) is **re-worded with the sentence, not deleted**. Its doc comment (`:3363-3369`) names REQ-585 and says so; deleting it passes CI and silently removes the guard.
- [ ] Both prompt-margin tests stay green **without moving a ceiling**: `redact.rs:2212 the_total_cap_clears_the_harness_context_budget_with_margin` (the tighter, opted-out shape) and `harness/tools/web.rs:2273 the_web_tool_docs_clear_the_outbound_body_overhead`. `REDACT_BODY_OVERHEAD_BYTES` is **not** raised again — BUG-181 already bought 1 KiB, and the recorded post-REQ-586 headroom is 868 B on the tighter shape.
- [ ] `docs/manual-verification.md`'s headroom table is re-measured and rewritten with the post-amendment figures, and its note that REQ-587 reads this number before writing a resident sentence is kept current.
- [ ] The skill roster is **not** added to the guide (OQ-2). AC-15 words this as "not in the bundled guide" precisely so REQ-587's tool-description roster does not fail it later — do not tighten that wording.
- [ ] Mutation: reverting the sentence to BUG-181's wording fails the updated assertion.

## Technical Notes

- Draft the sentence to a byte budget first, then edit. The margin tests report the exact spend; measure with `cargo test -p tetond the_total_cap_clears` rather than estimating.
- One sentence. The prohibition against a second line containing "ask" is a real constraint of the guide, not a style note.
- **Sequenced behind TASK-197 on purpose.** TASK-197 changes `CarriedTurn::begin`'s signature across six production and three test call sites. Parallel implementers share one worktree (LESSON-541), so a concurrent `tetond` task would see a workspace that does not compile through no fault of its own. TASK-197 is not a functional dependency — it is a compile-stability one, and it is the only such edge in this REQ.
