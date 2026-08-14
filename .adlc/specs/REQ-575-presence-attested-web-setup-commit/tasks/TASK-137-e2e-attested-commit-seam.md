---
id: TASK-137
title: "Spawned-binary e2e: an attested /web setup commit through the presence seam"
status: complete
parent: REQ-575
created: 2026-08-14
updated: 2026-08-14
dependencies: [TASK-135]
repo: teton-code
---

## Description

Prove the attested commit works against a **real spawned daemon binary** driven
through the same environment seam the REQ-570 acceptance suite uses
(`TETON_TEST_SEAMS` + `TETON_PRESENCE_ACCEPT`), and confirm the shipped/release
build still refuses those seams (AC-6). See `architecture.md` ADR-3.

## Files to Create/Modify

- `crates/tetond/tests/web_setup_flow.rs` (or the existing spawned-binary e2e
  harness that already exercises `/web setup`, whichever hosts the real-daemon
  path) — add a case that boots a daemon with `TETON_TEST_SEAMS=1` and
  `TETON_PRESENCE_ACCEPT=1`, runs the guided commit, and asserts the capability
  is live in-session without a restart. If the existing e2e already boots a
  daemon for `/web setup`, extend it rather than adding a second harness.

## Acceptance Criteria

- [ ] An attested `/web setup` commit through the spawned daemon lands and the
      lookup serves in the same session (AC-6 **and AC-2** — reconciled from
      TASK-136: in a spawned-binary harness the `TETON_PRESENCE_ACCEPT` seam is
      the only way to reach the accepting/granted path, so AC-2 and AC-6 are the
      same test).
- [ ] The release-build refusal of `TETON_TEST_SEAMS` is untouched — do not weaken
      or bypass the `seam_verifier`/master-switch contract (a release build must
      still refuse to honor the seam).
- [ ] The e2e run is green after a full workspace build (not a stale daemon).

## Technical Notes

- `seam_verifier()` (attest/mod.rs) reads `TETON_PRESENCE_ACCEPT` only when
  `test_seams_enabled()`; `Some("1")` installs `AcceptingVerifier`. This is the
  only way a spawned binary reaches the accepting double — an in-process
  `with_presence_verifier` is unavailable across a process boundary.
- Reuse the existing e2e daemon-boot helpers; do NOT introduce a new spawn
  mechanism. Prefer extending the current `/web setup` e2e coverage.
- This task is the real-process complement to TASK-136's in-process granted-path
  test; both are needed (in-process for speed/inspection, spawned for the seam
  contract).
