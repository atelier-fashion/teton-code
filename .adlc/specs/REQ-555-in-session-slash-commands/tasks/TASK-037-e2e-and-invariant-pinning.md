---
id: TASK-037
title: "Scripted-session e2e tests and bidirectional-invariant mutation checks"
status: draft
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
- `crates/teton/src/slash.rs` — any test-only helpers the e2e assertions
  need; ensure the BR-8 unit tests name their direction (LESSON-479).

## Acceptance Criteria

- [ ] All five scripted-session e2e legs above pass against the test daemon
- [ ] Both existing e2e suites pass UNMODIFIED (AC-7 — non-slash input
      byte-identical)
- [ ] AC-8 mutation check performed and recorded in the task on completion:
      (a) remove a dispatch-table row → table-reachability test goes red;
      (b) bypass the interception branch → passthrough/classification test
      goes red; (c) restore, suite green (a passing test proves nothing
      until it has been seen to fail — BUG-151 posture)
- [ ] `cargo test --workspace` green; fmt + clippy clean

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
