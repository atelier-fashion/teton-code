---
id: TASK-007
title: "CLI: consent prompt, override UI, model commands"
status: complete
parent: REQ-547
created: 2026-07-21
updated: 2026-07-23
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

- [x] The prompt shows every BR-2 element — asserted against a scripted proposal event via the existing rendering trait, not eyeballed
- [x] Choosing an alternative sends `choose{name}`; an above-floor choice is only sent after the second confirmation, and declining the warning aborts without sending (AC-3)
- [x] `--yes` completes with no prompt (AC-5); absent it, an interactive client always prompts
- [x] `teton model list` shows the catalog with per-entry fit for this machine and marks the current selection; `set` changes it; `status` reports install state (AC-9)
- [x] Tests: rendering assertions over scripted events, the double-confirm path, clap parse tests for the new flag/subcommands

## Technical Notes

`confirm_model` already exists in `firstrun.rs` from REQ-544 but was never wired
(there was no protocol hook). Wire it rather than rewriting. Keep rendering
behind the existing `Surface` trait so the future TUI swap stays cheap. The
install path may be shown by `model status` (local render) but must never ride a
protocol event (BR-11).

## Implementation Notes (2026-07-23)

- `confirm_model` was **wired, not rewritten**: it is the proposal's own
  accept/reject question inside `model_ui::resolve_proposal`, and its
  `#[cfg_attr(not(test), allow(dead_code))]` is gone. A "no" there opens the
  override menu rather than declining the local tier, so backing out of the
  default can never be misread as declining local inference.
- **Two paths to the same prompt.** Live: `session_ui::render_event` returns the
  new `EventOutcome::ModelProposal` and the owning client renders + answers it.
  Late attach: the proposal event is broadcast once and never replayed
  (TASK-004), so `Connection::answer_outstanding_model_proposal` reads
  `model/status.pending_request_id` at session start and, when one is
  outstanding, renders the machine and catalog from `model/list` and answers by
  that id. `SessionState::claim_model_proposal` claims a `request_id` once, so a
  client that meets the same proposal both ways prompts only once.
- `model/list` cannot name the daemon's proposed entry, so the late-attach path
  offers "[a]ccept as offered" rather than guessing a name it would then
  mis-render — the same double-confirm gates its numbered choices.
- Backing out of the BR-3 warning **sends nothing at all** and does not re-open
  the menu; EOF and `q` likewise leave the proposal open (BR-1 remote-only)
  rather than being read as a decline, which BR-4 would persist.
- `teton model set` applies the same warning + second confirmation client-side
  (`--yes` supplies it non-interactively); the daemon refuses the change
  independently, so this is the legible half of a protocol guard, not the guard.
- `teton model status` derives the weights path from the daemon state directory
  (`<socket dir>/models/<name>.gguf`, mirroring tetond's `WEIGHTS_DIR`) because
  `InstallStateView` carries no path (BR-11).
