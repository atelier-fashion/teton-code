---
id: TASK-034
title: "Slash input classifier, dispatch table, and client-local commands (/help, /verbose, /quit, // escape)"
status: draft
parent: REQ-555
created: 2026-08-04
updated: 2026-08-04
dependencies: []
repo: teton-code
---

## Description

Create `crates/teton/src/slash.rs`: the three-bucket input classifier
(command / escaped prompt / plain prompt), the single command table that both
dispatches and generates `/help`, and the three client-local commands
(`/help`, `/verbose`, `/quit`). Wire it into `run_session`'s entry loop, and
switch the turn-ended line's gate from the `verbose` fn parameter to
`ctx.state.verbose` (architecture D-5) so `/verbose` governs both notice
surfaces.

## Files to Create/Modify

- `crates/teton/src/slash.rs` — new module: `Input` classifier (`classify`),
  command table `[(name, summary, handler)]`, `CommandOutcome::{Continue,
  Quit}`, handlers for /help (renders from the table + one escape-hatch
  footer line), /verbose (toggles `state.verbose`, echoes "verbose on/off"),
  /quit (returns `Quit`), unknown-command hint naming /help (BR-2), trailing-
  argument rejection for arg-less commands. Unit tests: classification
  totality in both directions (BR-8 — every table entry reachable; non-`/`
  passthrough byte-identical; `//` collapses exactly the leading pair),
  /help lists every table row plus the escape footer (AC-1, AC-7b half),
  unknown command dispatches no RPC (AC-6).
- `crates/teton/src/main.rs` — `mod slash;` declaration; entry-loop branch:
  classify after `input.trim()`, run commands (continue/break), send
  escaped/plain prompts as today; turn-ended gate reads `ctx.state.verbose`
  (D-5). Keep `--verbose` as the initializer only.

## Acceptance Criteria

- [ ] `classify` is pure and total: command / escaped-prompt / plain-prompt,
      with empty input skipped before classification exactly as today (BR-8)
- [ ] `//text` reaches the prompt path with exactly one leading `/` removed;
      slashes elsewhere untouched (BR-1b)
- [ ] `/help` output is generated from the dispatch table (BR-7) and includes
      the escape-hatch footer line
- [ ] `/verbose` toggles BOTH the route-notice gate and the turn-ended line
      live (spec AC-4 groundwork; D-5)
- [ ] `/quit` returns through the same post-loop path as Ctrl-D — session-end
      cost summary renders, no `process::exit` (BR-6)
- [ ] Unknown `/foo` prints one actionable hint naming /help; no RPC issued;
      loop continues (BR-2, AC-6)
- [ ] Both directions of the classification invariant are pinned by tests
      (LESSON-479 — name the direction in each test comment)
- [ ] `cargo test -p teton` green; fmt + clippy clean

## Technical Notes

- Interception point: `run_session` entry loop in main.rs, after
  `let text = input.trim()`, before `PromptTurnParams` (feature-tracer:
  lines ~381–391).
- Handlers take `(&mut Connection, &mut UiContext, args: &str)` and return
  `anyhow::Result<CommandOutcome>`; this task's three commands don't use the
  connection but the signature is shared so TASK-035/036 slot in.
- `/model set` takes an argument; the table must support subcommand-style
  names (`model set`) or arg parsing — design the table so TASK-035/036 add
  rows without changing the classifier.
- Output only via `ctx.surface` (`LineKind::Info` for help/verbose echo,
  `LineKind::Error` for unknown-command) — no direct stdout (BR-9).
- Do not mutate `state.grants` / `state.model_seen` from handlers.
