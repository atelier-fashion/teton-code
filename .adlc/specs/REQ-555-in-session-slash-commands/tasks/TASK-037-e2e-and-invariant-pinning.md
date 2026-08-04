---
id: TASK-037
title: "Scripted-session e2e tests and bidirectional-invariant mutation checks"
status: complete
parent: REQ-555
created: 2026-08-04
updated: 2026-08-04
dependencies: ["TASK-035", "TASK-036"]
repo: teton-code
---

## Description

End-to-end coverage of the slash-command surface against a live test daemon
(piped stdin), plus the AC-8 mutation verification that the BR-8
classification guards actually fail when the code drifts. This is the
integration pass that proves the feature as the user experiences it.

## Files to Create/Modify

- `crates/teton/tests/cli_e2e.rs` — new scripted-session tests using
  `TestDaemon::run_cli_with_stdin`:
  1. `/help` prints all six commands + escape footer; no turn output (AC-1)
  2. `/cost` mid-session renders the cost report (AC-2 e2e leg)
  3. `/verbose` toggle: quiet by default → `route [...]` + turn-ended after
     toggle → quiet again after second toggle, one session (AC-4)
  4. `/quit` vs Ctrl-D in piped mode: identical session-end output for the
     same session history (AC-5)
  5. `//`-escape line reaches the model as a prompt with one leading slash
     (AC-7b e2e leg) and a plain prompt still round-trips (AC-7)
  6. `/model set` through a live daemon: at least the unknown-name leg
     (error lists catalog names) and one valid-name or above-floor leg —
     closes the residual gap TASK-036 flagged (AC-3b's scripted-session
     wording; the decision-level legs are already pinned in main.rs tests)
- `crates/teton/src/slash.rs` — any test-only helpers the e2e assertions
  need; ensure the BR-8 unit tests name their direction (LESSON-479).

## Acceptance Criteria

- [x] All six scripted-session e2e legs above pass against the test daemon
      (leg 5 with one documented limit — see Verification Notes)
- [x] Both existing e2e suites pass UNMODIFIED (AC-7 — non-slash input
      byte-identical). No existing test body was touched; the only harness
      change is additive (`spawn_scripted`, `run_cli_capture`).
- [x] AC-8 mutation check performed and recorded in the task on completion:
      (a) remove a dispatch-table row → table-reachability test goes red;
      (b) bypass the interception branch → passthrough/classification test
      goes red; (c) restore, suite green (a passing test proves nothing
      until it has been seen to fail — BUG-151 posture)
- [x] `cargo test --workspace` green (880 passed, 0 failed); fmt + clippy
      (`-D warnings`) clean

## Technical Notes

- e2e harness (integration-explorer): `TestDaemon::spawn` gives an isolated
  state dir with `TETON_TEST_SEAMS=1` and probe overrides;
  `run_cli_with_stdin(&teton_bin(), &[], "...\n")` drives the interactive
  loop over a pipe — the framed prompter auto-degrades to plain in piped
  mode, which is exactly the AC-5 comparison ground.
- The /verbose e2e leg needs a turn that produces a `route_decided` event —
  the scripted-engine fixture path used by existing session e2e tests
  provides it.
- Record the AC-8 mutation transcript (commands run, red output snippet) in
  this task file's completion notes — the claim is only real with evidence
  (LESSON-454).

## Verification Notes

### What landed

`crates/teton/tests/cli_e2e.rs`: 6 → 12 tests. The six existing tests are
untouched; the harness gained two additive members —
`TestDaemon::spawn_scripted` (a `TETON_LOCAL_SCRIPT` local tier) and
`run_cli_capture` (output **and** exit status, which AC-5 needs).

| New test | AC |
|---|---|
| `slash_help_lists_every_command_and_no_turn_is_attempted` | AC-1, AC-6 |
| `slash_cost_renders_the_daemons_report_mid_session` | AC-2 (e2e leg) |
| `slash_verbose_toggles_the_route_notice_around_real_turns` | AC-4 |
| `slash_quit_ends_the_session_exactly_as_ctrl_d_does` | AC-5 |
| `an_escaped_line_and_a_plain_line_both_reach_the_model` | AC-7, AC-7b (e2e legs) |
| `slash_model_set_runs_the_shared_flow_against_a_live_daemon` | AC-3b (e2e legs) |

Why a scripted local tier: a `TETON_LOCAL_SCRIPT` engine downloads nothing and
is therefore exempt from the first-run consent gate (`tetond` E-5), so no
proposal is outstanding and every piped line reaches the *entry loop* instead
of answering a consent question — and the tier can serve a real turn, which is
what makes AC-4's route notice exist at all. Prompt lines carry an auxiliary
signal ("explain", "what does") so the freeform heuristic routes them to that
local tier rather than to the configured remote default, which this suite
cannot call.

All three AC-3b legs run against the live daemon: unknown name (the error lists
the daemon's own four catalog entries), a fitting name (`selection:
qwen2.5-coder-1.5b (user override)`, read back by the next `/model` — i.e. the
change is the *daemon's* state), and an above-floor name declined with a piped
`n` (`selection unchanged`, selection still the fitting one). The decline leg
doubles as a guard that the question was actually asked: an unasked question
would leave the `n` to fall through to the entry loop as a prompt, which the
test's no-turn assertion catches.

### One documented limit (leg 5)

AC-7b's byte-level claim — that `//x` sends `/x`, exactly one leading slash
collapsed — is **not observable from the CLI's stdout**: the client never
echoes what it sent, and no daemon surface quotes the prompt back. That half
stays pinned by `slash.rs`'s
`the_double_slash_escape_collapses_only_the_leading_pair`, over the classifier
whose output the entry loop hands straight to `PromptTurnParams`. What the e2e
adds is the half no unit test can reach: `//help …` — a line naming a *real*
table row — defeats the dispatch table in the shipped binary and becomes a
turn, and the daemon's own routing reason names the signal it matched inside
that text (`matched 'what does'`), so the prompt text reached the daemon and
was classified there.

### An ordering property found while writing these tests

The daemon's per-client writer drains two independent producers — request
responses and the broadcast event stream (`server.rs`, `forward_events`) — so a
turn's trailing streamed text can be queued *after* that turn's own response
and render at the head of the next pump, one command later. Seen live, roughly
1 run in 10:

```
› verbose on
› scripted-turn-one complete.        <- turn 1's text, rendered after the
>> route [freeform] → local …           /verbose that followed it
```

Nothing about the slash surface causes it and no daemon code was touched here.
The tests are anchored so it cannot make them lie: per-segment assertions use
only `route [` (published before the turn runs, so FIFO-bound to its own pump)
and `turn ended` (printed by the entry loop on the response itself), plus
whole-output counts; the AC-5 byte-comparison uses a command-only history,
since a byte-comparison across two processes needs deterministic output. Worth
its own bug: in a real session the tail of an answer can print after the next
entry frame.

### AC-8 mutation record (LESSON-454 — the kill must come from an assertion)

**(a) Remove a dispatch-table row.** Deleted the `quit` row from `COMMANDS` in
`crates/teton/src/slash.rs`, then `cargo test -p teton --bins`:

```
---- slash::tests::the_table_carries_every_command_this_req_promises stdout ----
panicked at crates/teton/src/slash.rs:469:13:
/quit is missing from the dispatch table: ["help", "cost", "model", "model set", "verbose"]

test result: FAILED. 130 passed; 1 failed
```

The *forward* test (`every_table_row_is_reachable_from_a_typed_command_line`)
stayed green — it iterates the table, so a deleted row is simply one fewer
iteration. That is exactly the BUG-151/LESSON-479 shape the reverse test
exists for. The e2e leg died on the same mutation
(`cargo test --workspace --test cli_e2e slash_help`):

```
panicked at crates/teton/tests/cli_e2e.rs:683:9:
`/quit` is missing from /help with its summary; output:
…
/verbose    Toggle the routing and turn-end notices for this session.
//text sends text as a prompt with one leading slash …
```

**(b) Bypass the interception branch.** In `run_session`'s entry loop,
`match slash::classify(text)` → `match slash::Input::Prompt(text)`, so every
line is treated as a prompt. `cargo test -p teton --bins`:

```
test result: ok. 131 passed; 0 failed
```

**All 131 unit tests stayed green** — the classifier is still correct in
isolation; nothing calls it. Only the e2e layer notices
(`cargo test --workspace --test cli_e2e`):

```
test an_escaped_line_and_a_plain_line_both_reach_the_model ... FAILED
test slash_cost_renders_the_daemons_report_mid_session ... FAILED
test slash_verbose_toggles_the_route_notice_around_real_turns ... FAILED
test slash_help_lists_every_command_and_no_turn_is_attempted ... FAILED
test slash_quit_ends_the_session_exactly_as_ctrl_d_does ... FAILED
test slash_model_set_runs_the_shared_flow_against_a_live_daemon ... FAILED

---- slash_help_lists_every_command_and_no_turn_is_attempted stdout ----
panicked at crates/teton/tests/cli_e2e.rs:651:5:
an unknown command must name what was typed; output:
session sess-0 ready (freeform). Type a prompt; Ctrl-D to end.
› error: prompt failed: provider failed and no fallback is configured
› error: prompt failed: provider failed and no fallback is configured
```

`/frobnicate` and `/help` were both shipped to the model — the exact BR-1
violation this REQ exists to prevent, and the six existing e2e tests all stayed
green through it.

**(c) Both mutations reverted** (`git checkout -- crates/teton/src/slash.rs
crates/teton/src/main.rs`), then:

```
cargo fmt --all --check                                   # clean
cargo clippy --workspace --all-targets -- -D warnings     # clean
cargo test --workspace                                    # 880 passed, 0 failed
```

`cli_e2e` was additionally run 10 times in a row after the anchoring fix
described above: 12/12 green each time.
