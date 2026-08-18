---
id: TASK-173
title: "e2e: read parity, writes, piped refusals, /help, presence; CHANGELOG, README, runbook, spec ticks"
status: draft
parent: REQ-582
created: 2026-08-18
updated: 2026-08-18
dependencies: [TASK-170, TASK-171, TASK-172]
repo: teton-code
---

## Description

Close the acceptance criteria end to end over real binaries and write the
user-facing docs. AC-1: for each read row drive `teton <sub>` and a piped
session `/<sub>` against one scripted daemon and diff the lines (session
output from the session-ready line onward; `/doctor` minus the connect arm).
AC-2: under `TETON_TEST_SEAMS=1` (`run_cli_seamed`), `/policy set-tier build
<id>`, `/policy set-category edit <id> --fallback <id>`, `/boundary add
<glob> --mode local-only`, then `teton policy show` / `teton boundary list`
reflect them. AC-3: pty test — `/provider add …` reads the key echo-off
(REQ-579's keychain seam / MockKeychain path from `provider_setup` pty
tests), key absent from the transcript, config carries `keychain://`. AC-4:
each write row on a pipe prints the shell pointer; reads work. AC-8: `/help`
over a pipe lists every row. AC-11: on a `presence`-featured build with
`TETON_PRESENCE_ACCEPT=fail`, `/policy set-tier` is refused and `config.toml`
is byte-identical, paired with the accept seam (feature-gated test; skip
cleanly otherwise). Docs: CHANGELOG `[Unreleased]` entry, README session
section, `docs/manual-verification.md` runbook (dogfood the screenshot flow:
ask to test Kimi, type `teton provider list`), tick the spec's AC boxes with
evidence, mark tasks complete.

## Files to Create/Modify

- `crates/teton/tests/cli_e2e.rs` — AC-1, AC-2, AC-4, AC-8, AC-5/6 (if not landed in TASK-170), AC-11 (feature-gated).
- `crates/teton/tests/pty_e2e.rs` — AC-3 (`/provider add` echo-off; keychain seam).
- `CHANGELOG.md` — `[Unreleased]` → Added.
- `README.md` — session commands section.
- `docs/manual-verification.md` — REQ-582 dogfood check.
- `.adlc/specs/REQ-582-run-cli-commands-from-the-session/requirement.md` — AC ticks with pointers to tests.

## Acceptance Criteria

- [ ] AC-1..AC-4, AC-8, AC-11 (feature-gated) covered by tests named for their AC; `cargo test --workspace --no-fail-fast` shows no `FAILED` (grep — LESSON-533).
- [ ] `git diff main...HEAD -- crates/teton-protocol/src/` is empty (AC-12).
- [ ] CHANGELOG entry follows the file's style (what an upgrade changes for a running machine; the screenshot story in one paragraph).
- [ ] fmt/clippy clean.

## Technical Notes

- Build the workspace before targeted e2e runs (`cargo build --workspace`) — LESSON-510/BUG-164.
- Byte-diff parity: compare `LineKind`-agnostic text; the session prints `>> ` prefixes for notices — compare the same surface class the shell prints (both use `stdout_surface`), so bytes should match exactly; if a legitimate prefix difference exists, assert on the text after the prefix and say why in the test.
- The presence test needs `--features presence`; gate with `#[cfg(feature = "presence")]` or a runtime check on the built binary's posture, mirroring how REQ-576's e2e is gated (`crates/tetond/tests/config_set_attestation.rs`).
