---
id: TASK-035
title: "/cost and /model handlers reusing the existing renderers"
status: complete
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

- [x] `/cost` and `teton cost` execute the SAME rendering function — asserted
      structurally (one function, two call sites), not by string comparison
      (AC-2)
- [x] `/model` prints exactly one Info line from `ModelStatusResult`; the
      declined-local-tier case says so rather than printing nothing (AC-3)
- [x] Neither handler issues a `prompt/turn` RPC (BR-1) — pinned by test
- [x] No new protocol methods or daemon changes (BR-3)
- [x] `cargo test -p teton` green; fmt + clippy clean

## Verification Notes

**AC-2 is structural, and deliberately has no runtime assertion.**
`query_and_render_cost` is one `fn` item in `main.rs`, now `pub(crate)` and
taking `(&mut Connection, &mut UiContext<'_>)`. It has three call sites and no
sibling: `run_cost` (the `teton cost` subcommand), `run_session`'s session-end
summary, and `slash::handle_cost`. There is no observable difference for a test
to assert — sharing one `fn` is the guarantee itself, and a test that re-derived
it from output strings would be the "string coincidence" this AC rules out. A
copy-paste re-implementation for either surface is caught by review of the call
graph, not by a green/red test. Recorded here per the task's own instruction to
document rather than write a vacuous test.

The ctx moved from being built *inside* the helper to being supplied by the
caller, which is what makes D-4 possible: the subcommand and the session-end
summary keep their passive ctx, while `/cost` runs under the session's own
(`answer_permissions: true`) — so an event arriving mid-command is answered by
the same client that would answer it between turns.

**AC-3 / D-6.** `model_ui::render_current_model_line` renders exactly one
`LineKind::Info` line from the `ModelStatusResult` the handler just received —
no second query, no cache. The `one_line` test helper asserts the single-line
invariant (`surface.calls.len() == 1`) on *every* case it renders, including the
one where the payload also carries an outstanding proposal. Cases covered:
model + each of the four `InstallStatus` values, model with no install record,
declined local tier, no decision recorded, and a decision that named no model.
The `(source)` suffix comes from `firstrun::source_label` and the install words
from `install_label`, both asserted against those functions rather than against
hard-coded strings, so `teton model status` and `/model` cannot word the same
state differently (BR-4, LESSON-456).

One case the task did not enumerate is covered because it is a real
misattribution risk: `install` describes whichever weights are on disk, which
immediately after a `model set` is the *previous* model. `install_words` matches
`install.model_name` against the selection and falls back to "not installed yet"
when they differ — otherwise `/model` would report a freshly selected model as
`verified` on the strength of another model's weights, in one line the user has
no way to cross-check (the BUG-146 shape).

**BR-1 pin.** Handlers cannot be unit-tested end-to-end (`Connection` needs a
live socket), so the unit leg pins the classifier: the table-reachability test
iterates `COMMANDS` and proves `/cost` and `/model` classify as
`Input::Command` and resolve to a handler — never `Input::Prompt` — and the
entry loop `continue`s on `CommandOutcome::Continue` without ever constructing
`PromptTurnParams`. The e2e leg (an RPC log showing no `prompt/turn`) is
TASK-037's.

**Table coverage, both directions (LESSON-479).** The existing reachability loop
proves every row is reachable but stays green if a row is *deleted*, so
`the_table_carries_every_command_this_req_promises` was added as the other half.
`/help` picks the new rows up automatically — the existing generation test
iterates the same array.

**`/model set` seam.** The `model` row keeps `takes_args: false`; TASK-036 adds
`model set` as its own row and `split_name`'s longest match routes
`/model set <name>` there without touching the classifier.
`model_set_is_rejected_until_task_036_adds_its_row` pins today's behaviour —
`set …` is a stray argument and is rejected, never read as a model name — and is
the test TASK-036 must update when it flips.

**Mutation-checked (LESSON-441 — a new test is new code).** Three mutations were
introduced and each turned the intended test red, then were reverted:

| Mutation | Test that went red |
|---|---|
| drop the `install.model_name == selected` guard | `current_model_line_never_attributes_another_models_install_state` |
| remove the `cost` row from `COMMANDS` | `the_table_carries_every_command_this_req_promises`, `a_trailing_argument_to_cost_or_model_is_rejected` |
| remove the `declined_local` branch | `current_model_line_says_the_local_tier_was_declined` |

**Suite.** 114 → 126 unit tests (+12), 6 → 6 e2e; `cargo test -p teton` green,
`cargo fmt --all` clean, `cargo clippy -p teton --all-targets -- -D warnings`
clean. Only `crates/teton` was touched — no daemon or protocol change (BR-3).

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
