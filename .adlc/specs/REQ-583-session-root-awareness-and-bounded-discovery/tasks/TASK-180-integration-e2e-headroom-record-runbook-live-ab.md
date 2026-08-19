---
id: TASK-180
title: "Integration: CLI e2e for --cwd and /cd, final headroom record, manual-verification runbook, live A/B on the local tier"
status: draft
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
