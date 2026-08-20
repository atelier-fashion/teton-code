---
id: TASK-208
title: "The suites that can actually see it: e2e, pty, egress capture, pressure silence, cost"
status: draft
parent: REQ-585
created: 2026-08-20
updated: 2026-08-20
dependencies: [TASK-207]
---

## Description

The ACs whose only honest harness is an end-to-end one. Each leg names the
existing test it copies, because a new suite that invents its own fixture shape
is a suite that stops matching the product.

## Files to Create/Modify

- `crates/teton/tests/cli_e2e.rs` — AC-1, AC-2, AC-4 (surface bytes), AC-9, AC-14, AC-17, AC-19 (`/verbose` half)
- `crates/teton/tests/pty_e2e.rs` — AC-8's consent-prompt bytes
- `crates/tetond/tests/egress_capture.rs`, `crates/tetond/tests/provenance_egress.rs` — AC-11 (a), (b), (c)
- `crates/tetond/tests/context_pressure.rs` — AC-16's assembled-prompt and silence legs
- `crates/tetond/tests/cost_attribution.rs` — AC-19's attribution half
- `crates/tetond/tests/symlink_posture.rs` — BR-1's root-followed / entry-not narrowing

## Acceptance Criteria

- [ ] **Fixture HOME**: `cli_e2e` tests use `run_cli_from(..., Some(&fixture), &[("HOME", &home)])` **and** `TestDaemon::spawn_scripted_with_env`, following `slash_cd_to_home_on_a_pipe_is_byte_identical_to_a_move_to_a_project` (`cli_e2e.rs:5637`) which hands the same fake HOME to both processes. Tests written against `run_cli`/`run_cli_with_stdin` inherit the runner's environment and **cannot** see a fixture HOME — do not use them here.
- [ ] AC-1: `/help` lists `/alpha`, `/beta`, `/gamma` with sources and hints, and the diagnostic `3 skills (user 2, project 1); 0 skipped`; the built-in section is byte-identical to the pre-REQ golden at this REQ's merge base.
- [ ] `cli_e2e.rs:4967`'s family-contiguity loop is bounded to the lines above the skills header — otherwise `/alpha` reads as a family. `every_read_row_prints_exactly_what_its_shell_twin_prints` (`:4539`) still passes: nothing new prints into the session body at start-up.
- [ ] AC-4's "the substituted body reached the model" is asserted in `context_pressure.rs`, whose `ScriptedEngine.prompts` (`:98-114`, drained via `prompt(n)` at `:274`) is the **only** harness that records the exact string handed to the engine. `cli_e2e` cannot see it — assert the surface bytes there and the prompt content here.
- [ ] AC-9: `plan` / `full` / the pipe refusal, with the "does not eat a stdin line" leg written as a negative assertion.
- [ ] AC-11(a): a skill file under a `local-only` boundary pins the turn and nothing leaves the machine — copy `egress_capture.rs:566 read_blocks_every_boundary_spelling_under_one_identity`. **Project** skill, because a user skill outside the root is pinned by the unpinnable rule instead (ADR-9); assert that case separately and by its own name.
- [ ] AC-11(b): any dynamic command + any configured boundary ⇒ pinned local, copying `provenance_egress.rs:350 shell_cat_of_a_boundary_file_blocks_the_next_remote_turn`.
- [ ] AC-11(c): no boundary ⇒ the expansion reaches the remote provider and the captured payload **is** the expansion.
- [ ] AC-16: the four route shapes via `remote_provider_block_with_window` (`e2e/harness.rs:1943`) — 128000, 0, 4096 — plus the local route; and the drain-and-assert-empty for `context_pressure`.
- [ ] AC-19: the attribution half runs in `cost_attribution.rs` (`:134 every_egress_call_yields_exactly_one_attributed_cost_record`), **not** `cli_e2e` — `cli_e2e`'s scripted tier is local and local turns produce no billed row, so a `/cost` assertion there would be vacuous.
- [ ] AC-6's EPERM leg skips itself under root (`libc::geteuid() == 0`), with the skip stated in the test body.
- [ ] Every fixture's ordering is deterministic across filesystems: entries sorted by name, no reliance on `read_dir` order (LESSON-540 — two REQ-583 tests passed on APFS and failed on ext4 for exactly this).

## Technical Notes

- `cli_e2e`'s scripted model is `TETON_LOCAL_SCRIPT` with replies separated by `\n---\n` (`:204-208`); a scripted engine is exempt from the first-run consent gate, which is what lets every piped stdin line reach the entry loop.
- Fixtures live under `/tmp` with short names because the daemon socket must fit `SUN_LEN`; canonicalize before comparing paths on macOS (`/tmp` → `/private/tmp`, `cli_e2e.rs:5205`).
