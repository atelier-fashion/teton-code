---
id: TASK-008
title: "End-to-end proof: piped legs, egress-capture at full, a real terminal, and the mutation check"
status: complete
parent: REQ-560
created: 2026-08-11
updated: 2026-08-11
dependencies: [TASK-007]
---

## Description

The criteria that cannot be met by inspection or by a unit test. AC-4 is the one
that keeps permission levels orthogonal to the privacy boundary, and AC-14 is the
one that proves the suite would notice if the feature were switched off.

## Files to Create/Modify

- `crates/teton/tests/cli_e2e.rs` — piped legs:
  - **AC-2**: one test, three legs — `edit` prompts at `guarded`; after
    `/permissions edits` the next `edit` runs unprompted and a `shell` still
    prompts; after `/permissions plan` both are denied and `Denied` reaches the
    model
  - **AC-3**: allow-always at `guarded` → `plan` denies → back to `guarded`
    re-applies the grant with no re-prompt
  - **AC-15**: with a prompt pending on `shell`, a `/permissions full` arriving
    before the answer leaves it pending and awaiting the user; the user's answer
    decides that call and the *next* `shell` evaluates at `full`. Both
    directions — the `/permissions plan` inverse must not auto-deny the
    in-flight call
  - **AC-6**: `/permissions full`, daemon restart, fresh session → `guarded`
  - **AC-8**: assert the existing whole-output and `/quit`-equals-Ctrl-D tests
    are **unmodified** — this is a review check on the diff as much as a test
- `crates/tetond/tests/egress_capture.rs` (or a new
  `permission_level_egress.rs` following its harness):
  - **AC-4**: at `full`, a session touching a `local-only` boundary produces
    zero remote calls containing boundary content and still emits
    `privacy_block`; a session tainted by unknown-provenance results stays
    pinned to the local tier at `full`
- `crates/teton/tests/pty_e2e.rs`:
  - **AC-10**: at a real terminal the status row renders below the bottom rule,
    a typed line is accepted intact with the frame uncorrupted, and a REQ-556
    loading indicator drawn above the frame at the same time leaves neither row
    stranded after a redraw. Model on the existing
    `the_status_row_shows_the_session_s_web_capability` test
- **AC-14 mutation check** — run manually, record the result in the PR body:
  1. freeze `status_line` to a constant → at least one test red
  2. remove the level-before-grants ordering (restore grants-first) → at least
     one test red
  3. make `full` skip the gate instead of allowing → at least one test red

## Acceptance Criteria

- [ ] All six piped legs above pass, and the pre-existing `cli_e2e` tests are
      byte-for-byte unmodified in the diff
- [ ] **AC-4** passes with egress capture asserting on payload *content*, not on
      call count alone — LESSON-432's shape is a hole that inspection cannot see
- [ ] **AC-10** passes at a real pty; if the `\x1b[J` assumption from ADR-E is
      contradicted empirically, `erase` gains its own below-count and the ADR is
      amended in the same commit
- [ ] **AC-14**: each of the three mutations is applied, the red test named, and
      the mutation reverted. Record which test caught each in the PR body. A
      mutation that leaves the suite green is a coverage gap to close in this
      task, not a note for later
- [ ] `cargo test --workspace` green after all mutations are reverted
- [ ] No clippy warnings

## Technical Notes

Run the workspace build before the targeted e2e legs: a `-p teton --test cli_e2e`
run does not rebuild `tetond`, so a daemon-side change can look verified when it
was never compiled into the binary under test (recorded in project memory).

`cargo test` fail-fast hides sibling targets — use `--no-fail-fast` for the
confirmation run so the reported failure count is a total rather than a floor.

For AC-14, do the mutations one at a time and revert each before the next.
Because TASK-007's scanners read `src/` while running, do not edit source while
a suite is in flight (BUG-159).
