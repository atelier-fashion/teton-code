---
id: TASK-036
title: "/model set <name> via a single shared validate→confirm→set flow"
status: complete
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

- [x] One shared function contains catalog-name validation (unknown name →
      error listing available names), the BR-3 above-floor warning +
      `confirm_above_ram_floor` second confirmation, and the `model/set`
      call with `confirmed_above_ram_floor` — called by BOTH `teton model
      set` and `/model set` (BR-4b)
- [x] `/model set` three legs pinned by scripted tests: valid name changes
      selection; unknown name lists catalog names; above-floor warns and
      only proceeds after confirmation — declining leaves selection
      unchanged and says so (AC-3b) — pinned at the decision level with a
      `ScriptedPrompter`; see "Where AC-3b is pinned" below for what that
      covers and what it leaves to the e2e pass
- [x] Declining the confirmation consumes the dialogue prompter (plain, not
      framed) and the entry loop continues (LESSON-470: default-no dialogue)
- [x] `teton model set` subcommand behavior unchanged (existing tests pass
      unmodified)
- [x] `cargo test -p teton` green; fmt + clippy clean

## Verification Notes

**Where the shared flow lives, and why (`main.rs`).** `apply_model_set` and its
decision half `decide_model_set` both live in `crates/teton/src/main.rs`, beside
`query_and_render_cost`. Three reasons, in order of weight:

1. **Precedent.** `query_and_render_cost` is the other "one flow, N call sites"
   function this REQ needs (TASK-035), and it lives in `main.rs`. A second such
   function landing somewhere else would make "where does a shared flow live?"
   a question with two answers.
2. **`model_ui`'s stated contract.** Its module doc says "Everything is a pure
   function of a protocol payload plus a `Prompter`, so every path above … is
   unit-tested against scripted answers with no daemon and no socket." A
   `Connection`-taking function there would falsify that sentence for the whole
   module.
3. **Every RPC-sequencing flow is already in `main.rs`** (`run_model_list`,
   `run_model_status`, `run_model_set`, `query_and_render_cost`); `model_ui`
   renders payloads and collects answers.

The refactor is a **move**: every message string, every match arm, and the
`confirmed_above_ram_floor: above_floor` assignment are the pre-existing ones,
character for character. `run_model_set` is now five lines — open a connection,
build its passive ctx, call the helper — so `teton model set` is byte-identical
and its existing tests are unmodified.

**The split, and why it exists.** `apply_model_set` owns the two RPCs and
nothing else; `decide_model_set(name, assume_yes, &list, surface, prompter) ->
Option<ModelSetParams>` owns the whole decision — find the entry, compute
`above_floor`, run the BR-3 confirmation, build the params. `None` means send
nothing, and `apply_model_set` issues `model/set` only for `Some`, so "declining
sends nothing" is a property a unit test can assert without a socket. That is
what made the mutation check below possible at all.

**`assume_yes` in-session is `ctx.auto_accept_model`.** It is the same
`cli.yes` the subcommand passes: `run_session` sets `auto_accept_model:
auto_accept` from it (main.rs:333), and `--yes` is `global = true`. The
Permissions table calls for "the session's `--yes` as its explicit unattended
stand-in" for the second confirmation, and `auto_accept_model` is UiContext's
only carrier of that flag — so reusing it is the literal reading, not an
overload. The semantic it already had (REQ-547 BR-5: answer a first-run model
proposal without reading input) is the same semantic in the same domain —
an explicit unattended stand-in for a model-consent question — so the two uses
cannot diverge in meaning. A session started without `--yes` asks; one started
with it proceeds and still prints the "--yes supplies the second confirmation
(BR-3)" notice, so the install is never silent.

**The prompter is the plain one.** The handler passes `ctx.prompter`, which for
a session is the plain `StdinPrompter` — `FramedStdinPrompter` is a local of
`run_session`'s entry loop and is never in the ctx. A consent question is
dialogue, not entry (REQ-549 BR-5). Declining consumes exactly one prompt
(`asked == 1`, asserted) and the handler's only non-error return is
`CommandOutcome::Continue`, so the loop carries on (LESSON-470).

**A bare `/model set` is rejected before any RPC.** `CommandSpec.takes_args:
bool` became `args: Args::{None, Required(usage)}`, so "needs an argument" is
expressed in the table and enforced in `resolve` — pure, testable without a
`Connection`, and it means a half-typed command renders its usage rather than
opening a `model/list` with an empty name. The hint carries the usage clause:
`/model set needs a catalog name — `/model set <name>`, and `teton model list`
names them — type /help for the commands this session knows.`

**Where AC-3b is pinned.** All three legs are pinned in `main.rs`'s tests
against `model_ui::testing::list_result()` with a `ScriptedPrompter` and a
`RecordingSurface` — "scripted" in the prompter sense, not a live scripted
session. That is the level the consent gate actually lives at, and it is the
level the mutation below could be proven at. **Residual gap, flagged not
closed:** TASK-037's e2e list has legs for `/help`, `/cost`, `/verbose`,
`/quit`, and the `//` escape, but none for `/model set`; nothing in this REQ
currently drives `/model set` through a live daemon end to end. Adding that leg
belongs to TASK-037's file, not this one.

**Mutation-checked (LESSON-441/464 — a new guard needs its own known-bad).**
Three mutations, each reverted after the red was observed. The panic line
numbers quoted below were captured *before* the Phase-5 verify pass added its
doc comments (~6–8 lines of offset against the committed source); the tests
themselves are named here and all pass at HEAD, which is the claim that
matters — the line numbers are a transcript, not an index.

| Mutation | Test that went red |
|---|---|
| `decide_model_set`: replace the `confirm_above_ram_floor` call with `let confirmed = true` (the task's required check — skip the BR-3 confirmation for an above-floor pick) | `declining_the_ram_floor_warning_sends_nothing_and_says_so`, `an_above_floor_name_is_sent_only_after_the_second_confirmation` |
| delete the `model set` row from `COMMANDS` | `model_set_routes_to_its_own_row_and_a_bare_one_asks_for_a_name`, `the_table_carries_every_command_this_req_promises` |
| `resolve`: drop the `Args::Required(_) if args.is_empty()` arm (accept a bare `/model set`) | `model_set_routes_to_its_own_row_and_a_bare_one_asks_for_a_name` |

Red output from the required check:

```
---- tests::declining_the_ram_floor_warning_sends_nothing_and_says_so stdout ----
thread 'tests::declining_the_ram_floor_warning_sends_nothing_and_says_so'
panicked at crates/teton/src/main.rs:1410:13:
`n` was read as consent and the change was sent

failures:
    tests::an_above_floor_name_is_sent_only_after_the_second_confirmation
    tests::declining_the_ram_floor_warning_sends_nothing_and_says_so
test result: FAILED. 129 passed; 2 failed;
```

**Suite.** `teton` unit 126 → 131 (+5), `teton` e2e 6 → 6 (unmodified),
workspace 869 → 874, all green. `cargo fmt --all --check` clean;
`cargo clippy -p teton --all-targets -- -D warnings` clean. Only
`crates/teton/src/{main.rs,slash.rs}` changed — `model_ui.rs` was not touched
(the helper did not land there), and there is no daemon or protocol change.

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
