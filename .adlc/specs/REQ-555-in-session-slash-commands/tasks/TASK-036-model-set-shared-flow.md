---
id: TASK-036
title: "/model set <name> via a single shared validate→confirm→set flow"
status: draft
parent: REQ-555
created: 2026-08-04
updated: 2026-08-04
dependencies: ["TASK-034"]
repo: teton-code
---

## Description

Extract `run_model_set`'s validate→confirm→set core into one shared helper
that takes an already-open `Connection` + `UiContext`, then add the
`/model set <name>` handler as a second call site. The REQ-547 BR-3
above-RAM-floor warning + second confirmation must have exactly one
implementation (spec BR-4b, architecture D-3 — a parallel copy of this flow
is how REQ-547's consent bypass shipped; LESSON-441).

## Files to Create/Modify

- `crates/teton/src/main.rs` — refactor `run_model_set` into a thin
  connection-opening shell around the extracted helper; behavior of
  `teton model set` byte-identical (existing e2e/unit tests unmodified).
- `crates/teton/src/slash.rs` — `/model set <name>` table row + handler
  calling the shared helper on the session connection; `assume_yes` wired
  from the session's `--yes` flag (Permissions table: `--yes` is the
  explicit unattended stand-in for the second confirmation).
- `crates/teton/src/model_ui.rs` (only if the helper lands here rather than
  main.rs — implementer's choice; ONE home, two call sites).

## Acceptance Criteria

- [ ] One shared function contains catalog-name validation (unknown name →
      error listing available names), the BR-3 above-floor warning +
      `confirm_above_ram_floor` second confirmation, and the `model/set`
      call with `confirmed_above_ram_floor` — called by BOTH `teton model
      set` and `/model set` (BR-4b)
- [ ] `/model set` three legs pinned by scripted tests: valid name changes
      selection; unknown name lists catalog names; above-floor warns and
      only proceeds after confirmation — declining leaves selection
      unchanged and says so (AC-3b)
- [ ] Declining the confirmation consumes the dialogue prompter (plain, not
      framed) and the entry loop continues (LESSON-470: default-no dialogue)
- [ ] `teton model set` subcommand behavior unchanged (existing tests pass
      unmodified)
- [ ] `cargo test -p teton` green; fmt + clippy clean

## Technical Notes

- Current flow (feature-tracer): `model/list` → find entry → `above_floor =
  entry.ram_floor_bytes > list.probe.total_ram_bytes` → interactive
  `model_ui::confirm_above_ram_floor(name, floor, ram, surface, prompter)`
  or `--yes` notice → `ModelSetParams { name, confirmed_above_ram_floor }`.
  Preserve the exact message strings — the refactor is a move, not a
  rewrite.
- The in-session confirmation uses `ctx.prompter` (the session's plain
  dialogue prompter), NEVER the framed entry prompter (spec Permissions;
  REQ-549 BR-5 posture).
- Success line notes "the daemon installs the weights if they are missing"
  exactly as the subcommand does today.
- Mutation-check the confirmation leg: force the helper to skip
  `confirm_above_ram_floor` and assert the test goes red (LESSON-441/464 —
  a new guard needs its own known-bad).
