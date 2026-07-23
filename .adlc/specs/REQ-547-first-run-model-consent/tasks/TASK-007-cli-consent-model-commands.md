---
id: TASK-007
title: "CLI: consent prompt, override UI, model commands"
status: draft
parent: REQ-547
created: 2026-07-21
updated: 2026-07-21
dependencies: [TASK-001, TASK-004]
---

## Description

The user-facing half. Wires the CLI's already-built-but-unwired `confirm_model`
to the new proposal event, adds override selection, and ships `teton model`.

## Files to Create/Modify

- `crates/teton/src/firstrun.rs` — render `model_selection_proposed`: detected RAM, free disk, GPU class, chosen band, the plain-language reason, the proposed model with size + RAM floor, and the selectable alternatives (BR-2)
- `crates/teton/src/model_ui.rs` — new: alternative-selection prompt; an above-RAM-floor pick warns explicitly and requires a **second** confirmation (BR-3)
- `crates/teton/src/main.rs` — `--yes`/auto-accept flag (BR-5); `teton model list|set|status` (AC-9)
- `crates/teton/src/client.rs` — send `model/confirm`; handle `model/*` responses

## Acceptance Criteria

- [ ] The prompt shows every BR-2 element — asserted against a scripted proposal event via the existing rendering trait, not eyeballed
- [ ] Choosing an alternative sends `choose{name}`; an above-floor choice is only sent after the second confirmation, and declining the warning aborts without sending (AC-3)
- [ ] `--yes` completes with no prompt (AC-5); absent it, an interactive client always prompts
- [ ] `teton model list` shows the catalog with per-entry fit for this machine and marks the current selection; `set` changes it; `status` reports install state (AC-9)
- [ ] Tests: rendering assertions over scripted events, the double-confirm path, clap parse tests for the new flag/subcommands

## Technical Notes

`confirm_model` already exists in `firstrun.rs` from REQ-544 but was never wired
(there was no protocol hook). Wire it rather than rewriting. Keep rendering
behind the existing `Surface` trait so the future TUI swap stays cheap. The
install path may be shown by `model status` (local render) but must never ride a
protocol event (BR-11).
