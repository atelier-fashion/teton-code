---
id: TASK-180
title: "Integration: CLI e2e for --cwd and /cd, final headroom record, manual-verification runbook, live A/B on the local tier"
status: complete
parent: REQ-583
created: 2026-08-18
updated: 2026-08-18
dependencies: ["TASK-177", "TASK-178", "TASK-179"]
---

## Description

Close the loop across the parallel tasks (TASK-176 lands transitively through TASK-177): the socket-level CLI legs of
AC-9/AC-10/AC-11/AC-12, the final resident-headroom figure, the AC-20/AC-21
by-hand runbook, and — where the machine allows — the AC-20 live A/B itself,
recorded as an observation.

## Files to Create/Modify

- `crates/teton/tests/cli_e2e.rs` — piped runs (the existing `run_cli_with_stdin` harness): (a) `teton --cwd <tmpdir>` then `/cd` bare form prints that root; (b) `--cwd /nope` → the refusal names the path, exit non-zero, no `ready (freeform)` marker; (c) `/cd <other tmpdir>` in one session → transcript shows `context cleared;` **and** `session root is now …`, and a subsequent `read` of a file only under the old root fails with "is outside the session root"; (d) `/cd /nope` → refusal line, root unchanged (a following `/cd` bare form prints the old root); (e) `/cd ~` from a project fixture (`HOME` pointed at a tmp dir by the harness) prints the not-a-project notice **only when interactive** — on a pipe assert its absence (byte parity), and cover the interactive path in `pty_e2e.rs` if the pty harness allows a `HOME` override; (f) `--cwd rel` resolves against the process cwd. Extend the existing `slash_clear_runs_no_turn…` scan if `/cd`'s `context cleared;` line would now count.
- `crates/tetond/src/egress/redact.rs` — headroom note: replace TASK-177's provisional figures with the final measured worst prompt / spent / margin at the merged tip (both sweeps), stating what TASK-176's doc rewording cost.
- `docs/manual-verification.md` — append `# Manual verification runbook — REQ-583 (session root awareness)` in the REQ-582 shape (`**Status: OUTSTANDING.**`, `## Procedure`, `## Sign-off`): `cd ~ && teton` (a) the notice line appears under the banner; (b) ask "look in my development folder for the Teton repo" — no macOS consent dialog for Media & Apple Music / Photos / "data from other apps" appears during the turn (Desktop/Documents may still ask if never granted — say so); (c) any `glob`/`grep` the model ran ended with a `... (stopped after …)` line or completed, never hung; (d) `/cd ~/Documents/GitHub/teton-code` → `context cleared;` + `session root is now ~/Documents/GitHub/teton-code (project teton-code, branch …)`; (e) `/cd` alone prints it; record the model's prose as an observation, not a pass/fail (LESSON-532).
- AC-20 live A/B: if a local model is loadable on this machine (`teton model status`), run the runbook once against the built binaries (`cargo run -p teton` with a scratch state dir per `docs/manual-verification.md`'s isolation recipe / `testing-a-teton-daemon-in-isolation`) and paste the transcript excerpt into the runbook's Sign-off block; otherwise mark it OUTSTANDING with the reason.

## Acceptance Criteria

- [ ] New cli_e2e legs green; whole suite `cargo test --workspace --no-fail-fast` green — grep the output for `FAILED` (LESSON-533).
- [ ] Redact note carries the final headroom figures for both sweeps; both sweeps pass.
- [ ] `docs/manual-verification.md` has the REQ-583 runbook; the AC-20 outcome (run, or OUTSTANDING with reason) is recorded.
- [ ] `cargo clippy --workspace --all-targets -- -D warnings` and `cargo fmt --check` clean.

## Technical Notes

- Byte-parity legs compare piped output before/after with `mask_session_id` as the suite already does.
- The consent-dialog non-appearance cannot be automated; it is a runbook step by design (LESSON-481's "pay for the harness or record the gap").
- Commit as `test(REQ-583): e2e for --cwd and /cd, final headroom, runbook [TASK-180]`.

## Implementation record (2026-08-18)

- **cli_e2e legs** (`crates/teton/tests/cli_e2e.rs`, new section "REQ-583 —
  the session root"): (a)+(c)+(d) `cwd_scopes_the_session_and_slash_cd_moves_it_and_reports_each_step`
  — one session: bare `/cd` names the `--cwd` root (`project proj`), a
  scripted absolute `read` is `[done]`, `/cd <plain>` draws `context cleared;
  N …` **then** `session root is now … (not a project)`, the bare form names
  the new root, the same `read` is `[failed]`, `/cd /nope` prints the daemon's
  refusal naming the path, clears nothing, and the root stays; the notice never
  reaches the pipe. (b) `a_cwd_that_does_not_exist_is_refused_before_any_session_output_and_exits_non_zero`
  — exit code 1, one stderr line naming `/nope`, no ready/cost/root/clear
  marker, no turn. (f) + `~/x`:
  `a_relative_cwd_joins_the_shell_directory_and_a_tilde_cwd_expands_home` (the
  CLI run *from* the fixture root with `--cwd proj`; `HOME` set for both
  processes so `~/x` is spelled `~/x` by the daemon). (e) piped half:
  `slash_cd_to_home_on_a_pipe_is_byte_identical_to_a_move_to_a_project` — with
  the one root line and the session id masked, `/cd ~` and `/cd <project>`
  transcripts are byte-identical. Harness: `run_cli_from` (cwd + env) and
  `spawn_scripted_with_env`. (e) terminal half: `pty_e2e::a_move_to_a_non_project_root_re_fires_the_notice_at_a_terminal`
  (`spawn_with_env` added to the pty harness) — banner `cwd:` line, notice,
  ready line in that order for a plain `--cwd`; no notice for a project; `/cd
  ~` draws clear, root line, notice. `slash_clear_runs_no_turn…`'s scan
  needed no change: that session types only `/clear`.
- **Found by the e2e and fixed here:** TASK-179's `session_root_changed` arm
  re-fired the notice on a pipe (BR-5's gate covered launch only). The gate is
  now `SessionState.interactive` (set once in `run_session` from the same
  `is_terminal` read the banner uses; `false` by default), and the arm draws
  the notice only under it — unit tests `a_root_move_to_home_on_a_pipe_draws_the_root_line_and_no_notice`
  (new) and `a_root_move_to_home_refires_the_launch_notice` (now sets the
  flag).
- **Multibyte hardening** (TASK-177's open note): `teton_core::session_root::bounded_field`
  now also holds every value to `byte_ceiling(max_chars) = max_chars + 2`
  bytes — the cost of an ASCII value cut to the character ceiling — eliding
  further at char boundaries around one `…`; the ASCII rendering is unchanged
  and the function is idempotent. Tests: `the_byte_ceiling_is_what_an_elided_ascii_value_costs`,
  `a_multibyte_value_is_bounded_in_bytes_too_and_still_elided_in_the_middle`
  (3- and 4-byte chars, both ceilings). `turn_loop::worst_case_session_root`
  is now provably the byte-worst:
  `the_worst_case_root_is_the_byte_worst_for_multibyte_roots_too` drives a
  200-char CJK path and 33-char CJK name/branch (and astral-plane twins)
  through `environment_block` and asserts none renders longer than the fixture,
  whose three values sit exactly at their byte ceilings. **Deviation from the
  brief:** the brief named a byte ceiling of `2 * max_chars`; at that width the
  block's worst row grows 203 → 341 bytes and both sweeps go red (redact margin
  49 → −89) with the constants pinned, so the ceiling is the ASCII cost
  instead — the only width that makes the AC-4 row the byte-worst without
  paying, recorded in `byte_ceiling`'s doc and the redact note.
- **Final headroom, re-measured at the merged tip:** opted-out (`egress::redact`)
  worst 5,891 / spent 9,167 / margin **49**; opted-in (`tools::web`) 5,844 /
  9,120 / **96** — unchanged from TASK-177 (this task added no resident byte).
  Constants unchanged (9,216 / 48). Both notes carry the re-measurement and the
  byte-bound account.
- **Runbook:** `docs/manual-verification.md` gained `# Manual verification
  runbook — REQ-583 (session root awareness)` (OUTSTANDING; five steps;
  sign-off block) plus a run record.
- **AC-20 live A/B: RAN from a script**, release build with `tetond/llama`
  (51 s), isolated daemon under `/tmp/t583` with the real
  `qwen3-coder-30b-a3b` weights symlinked; three runs recorded verbatim in the
  runbook — piped from `~` (three `glob` walks `[done]`, a shell call refused
  for want of a piped answer, the model found nothing; the home has > 100k
  entries under the skip rules, so the walk ended by budget — inferred, not
  read), piped `--cwd <repo>` (`/cd` bare, a root-relative `read Cargo.toml`,
  `/cd ~` clear+root lines, `/cd /nope` refused, root unchanged), and a
  `script(1)` pty run (notice under the banner before ready; none after a move
  to the project; re-fired after `/cd ~`). **The consent-dialog step is
  OUTSTANDING**: it cannot be observed from a script. Cleanup done: daemon
  stopped, symlink unlinked before `rm -rf`, real weights listed intact.
