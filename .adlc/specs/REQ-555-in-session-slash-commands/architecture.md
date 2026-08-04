# REQ-555 — Architecture: In-session slash commands

## Approach

A single new client-side module, `crates/teton/src/slash.rs`, owns input
classification and command dispatch for the interactive session. `run_session`'s
entry loop gains exactly one new branch: after `input.trim()`, the line is
classified into one of three buckets (BR-8) — **command** (leading `/`),
**escaped prompt** (leading `//`, collapsed to one `/`), or **plain prompt**
(everything else, byte-identical to today). Commands never construct
`PromptTurnParams`; escaped and plain prompts always do.

No daemon, protocol, or event changes of any kind. Data-bearing commands are
new call sites of existing RPCs on the session's already-open `Connection`
(`cost/query`, `model/status`, `model/list`, `model/set`), reusing the
renderers the subcommands already use (`cost_ui::render_report_view`,
`model_ui` helpers). This satisfies the thin-client/surface-parity rule
(REQ-544 BR-4) by construction.

Exploration confirmed (feature-tracer, architecture-mapper,
integration-explorer, 2026-08-04):

- The interception point is `crates/teton/src/main.rs` `run_session`, after
  `let text = input.trim()` and before `PromptTurnParams` construction.
- `query_and_render_cost` (main.rs) is already connection-reusing and
  stateless — callable mid-session as-is once visible to the handler.
- `model_ui::render_status`, `render_list`, and `confirm_above_ram_floor` are
  `pub`; `run_model_set`'s validate→confirm→set flow opens its own connection
  and must be factored into a shared helper (see D-3).
- Test seams exist for every layer: `RecordingSurface`, `ScriptedPrompter`
  (unit), `TestDaemon::run_cli_with_stdin` (e2e, piped stdin).

## Key Decisions

### D-1: One module owns classification and dispatch (`slash.rs`)

The classifier and the command table live in one new module with one public
entry point consumed by `run_session`. The table is a single static array of
`(name, summary, handler)` rows; `/help` renders from the same array the
dispatcher matches against, making BR-7 (help/dispatch cannot drift)
structural rather than asserted. Rationale: LESSON-475/LESSON-479 — detection
sets and the code they guard drift unless one artifact feeds both.

### D-2: Classification returns a control-flow value, not a side effect

`classify(input) -> Input::{Command(name, args), Prompt(text)}` is a pure
function (unit-testable without a daemon), and command handlers return
`CommandOutcome::{Continue, Quit}`. `/quit` breaks the entry loop by returning
`Quit` — flowing out of `run_session` through the **same** post-loop path
Ctrl-D takes (session-end cost summary, exit code), never `std::process::exit`
(BR-6). The escape hatch is part of `classify`: a leading `//` yields
`Prompt(<input with exactly one leading '/' removed>)` (BR-1b).

### D-3: `/model set` shares one flow function with `teton model set`

`run_model_set`'s validate→confirm→set core moves to a helper
(`model_flow::apply_model_set(name, assume_yes, conn, ctx)`-shaped) that takes
an already-open `Connection` + `UiContext`. The subcommand keeps its
open-a-connection shell and calls the helper; the `/model set` handler calls
the same helper on the session connection. The REQ-547 BR-3 above-floor
second confirmation therefore has exactly one implementation (spec BR-4b;
LESSON-441 — a parallel copy of a confirmation flow is how REQ-547's consent
bypass shipped).

### D-4: Command RPCs run under the session's own `UiContext`

Handlers reuse `run_session`'s existing ctx (`answer_permissions: true`,
`answer_model_proposals: true`) rather than a passive one: the interactive
session is the client that owns permission/proposal prompts, and an event
arriving during a command's RPC pump must behave exactly as it would between
turns. (A passive ctx here would silently change who answers a permission
request depending on whether the user happened to be mid-command.)

### D-5: `/verbose` requires the turn-ended gate to read session state

`run_session` currently gates the turn-ended line on its `verbose` function
parameter. `/verbose` toggles `SessionState.verbose`, so the turn-ended gate
must read `ctx.state.verbose` (single source of truth) or the toggle would
affect routing notices but not the turn-ended line — a two-sources drift bug
caught at design time (LESSON-456: one classifier per state, every surface).
The `--verbose` flag remains the initializer only.

### D-6: `/model` one-liner is a new tiny renderer over the same
`ModelStatusResult`

`model_ui` gains `render_current_model_line(&ModelStatusResult, &mut dyn
Surface)` — one `LineKind::Info` line derived from the same `model/status`
response `render_status` consumes (never a second query or cache — spec BR-4).
Declined local tier renders "local tier declined — running remote-only"
rather than nothing (spec AC-3).

## Data model / API / Service changes

None. No protocol types change, no new methods, no daemon edits (the
`crates/tetond` and `crates/teton-protocol` crates are untouched). All new
types (`Input`, `CommandOutcome`, the command table) are private to the
`teton` CLI crate.

## Additions proposed to `.adlc/context/architecture.md`

None — no new system-level decision. D-1..D-6 are CLI-internal and recorded
here.

## Lessons applied

- LESSON-470: the above-floor confirmation is dialogue via the existing
  plain `Prompter` (default no), never the framed entry prompter; piped
  stdin behavior identical (BR-9).
- LESSON-441 → D-3 (one confirmation flow, not two).
- LESSON-456 → D-5, D-6 (one source of fact per state).
- LESSON-475/479 → D-1 and the BR-8 bidirectional classification tests.
- LESSON-433: no platform-specific code is added; both CI legs cover it.

## Task graph

```
TASK-034  classifier + dispatch + /help /verbose /quit + loop wiring   (foundation)
   ├── TASK-035  /cost + /model handlers (renderer reuse)              (parallel)
   └── TASK-036  /model set via shared apply_model_set refactor        (parallel)
          └┬─────┘
TASK-037  e2e scripted-session tests + AC-8 mutation checks            (integration)
```
