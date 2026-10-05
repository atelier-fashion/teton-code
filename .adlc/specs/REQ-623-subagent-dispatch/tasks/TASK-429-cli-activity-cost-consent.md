---
id: TASK-429
title: "CLI: children on the activity line, per-child /cost rows, child-labelled consent prompts"
status: draft
parent: REQ-623
created: 2026-10-05
updated: 2026-10-05
dependencies: ["TASK-421", "TASK-424"]
repo: teton-code
---

## Description

BR-13's client half. The REQ-621 activity line lists running children by name with their
elapsed time; `/cost` renders nested child rows (name, route, status, cost) under the parent
turn; a `permission_request` carrying `child_id` renders the child's name in the prompt so
the user knows which child is asking. An old daemon's stream (no ids) renders exactly as
today.

## Files to Create/Modify

- `crates/teton/src/activity.rs` — child tracking from `agent_child_started`/`finished`; render
- `crates/teton/src/cost_ui.rs` — nested rows
- `crates/teton/src/client.rs` — route the agent events; consent label
- `crates/teton/src/activity.rs` — tests: two children shown, one finishes, line updates; no-children stream renders unchanged

## Acceptance Criteria

- [ ] Activity line shows `children: audit-1 12s, audit-2 12s` style while two run and drops a finished one
- [ ] `/cost` shows the parent total and one indented row per child
- [ ] A consent prompt from a child is labelled with its name; one from the parent is unchanged
- [ ] Snapshot/golden tests for the three renders; `cargo test -p teton` green

## Verification

| rule | kind | artifact | benign_path |
|------|------|----------|-------------|
| BR-13 | test-case | `crates/teton/src/activity.rs::tests::children_render_and_clear` | yes |
| AC-12 | test-case | `crates/teton/src/cost_ui.rs::tests::child_rows_nest_under_parent` | no |
| AC-7 | test-case | `crates/teton/src/client.rs::tests::consent_prompt_names_the_child` | yes |

## Technical Notes

- REQ-622 made input client-owned during a turn; the activity line must not reflow the editor row. Follow the TASK-419/420 patterns in `activity.rs`.
- Keep rendering width-bounded: truncate the children list with `+N` past the terminal width.
