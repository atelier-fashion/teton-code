---
id: TASK-035
title: "/cost and /model handlers reusing the existing renderers"
status: draft
parent: REQ-555
created: 2026-08-04
updated: 2026-08-04
dependencies: ["TASK-034"]
repo: teton-code
---

## Description

Add the `/cost` and `/model` command handlers. `/cost` renders the daemon's
`cost/query` report through the exact code path `teton cost` uses
(`query_and_render_cost`). `/model` prints one `LineKind::Info` line naming
the currently selected model and its state, derived from the same
`model/status` response `teton model status` renders in full (spec BR-4,
architecture D-6).

## Files to Create/Modify

- `crates/teton/src/slash.rs` — `/cost` and `/model` table rows + handlers
  calling the shared functions on the session connection with the session's
  own `UiContext` (D-4). Unit tests via `RecordingSurface`.
- `crates/teton/src/main.rs` — make `query_and_render_cost` callable from
  `slash.rs` (`pub(crate)` or move; keep ONE implementation used by both
  `run_cost` and `/cost` — spec BR-4/AC-2).
- `crates/teton/src/model_ui.rs` — new `render_current_model_line(
  &ModelStatusResult, &mut dyn Surface)`: e.g. `model: qwen3-coder-30b-a3b
  (user_override) — ready`; explicit renderings for no-decision-yet,
  declined local tier (AC-3), and install states (absent/partial/verified/
  corrupt → human words). Unit tests beside the existing `model_ui` tests
  using `model_ui::testing` fixtures.

## Acceptance Criteria

- [ ] `/cost` and `teton cost` execute the SAME rendering function — asserted
      structurally (one function, two call sites), not by string comparison
      (AC-2)
- [ ] `/model` prints exactly one Info line from `ModelStatusResult`; the
      declined-local-tier case says so rather than printing nothing (AC-3)
- [ ] Neither handler issues a `prompt/turn` RPC (BR-1) — pinned by test
- [ ] No new protocol methods or daemon changes (BR-3)
- [ ] `cargo test -p teton` green; fmt + clippy clean

## Technical Notes

- `CostQueryParams::default()` and `ModelStatusParams::default()` are
  stateless; both are safe mid-session on the open `Connection` (the call
  pumps events through the same ctx — integration-explorer confirmed stray
  responses are ignored by id routing).
- `render_current_model_line` consumes the SAME response type as
  `render_status` — never a second query, never a cached copy (BR-4).
- Selection source label: reuse `firstrun::source_label` for the `(source)`
  suffix so spellings can't drift.
- Handle the `METHOD_NOT_FOUND` daemon-too-old arm the way the subcommands
  do (Notice line), and RPC errors as `LineKind::Error` — never a panic, the
  loop must continue.
