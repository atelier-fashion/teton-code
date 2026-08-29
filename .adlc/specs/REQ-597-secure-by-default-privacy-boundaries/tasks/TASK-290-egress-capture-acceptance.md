---
id: TASK-290
title: "Egress-capture proof that the stock install keeps the promise"
status: pending
parent: REQ-597
repo: teton-code
created: 2026-08-29
updated: 2026-08-29
dependencies: [TASK-287, TASK-288]
---

## Description

The acceptance evidence. conventions.md requires an egress-capture integration test for any
BR-1 claim — code inspection is not acceptance. Covers AC-1, AC-2, AC-3, AC-4, AC-4.1, AC-5,
AC-6, AC-7.

## Files to Create/Modify

- `crates/tetond/tests/egress_capture.rs` — the sentinel cases, following
  `scripted_session_leaks_zero_boundary_bytes_and_blocks_deliberate_egress` and
  `read_blocks_every_boundary_spelling_under_one_identity` as the in-file precedents.

## Acceptance Criteria

- [ ] AC-1: config with **no** `[[boundaries]]` table; a planted `.ssh/id_rsa` sentinel's bytes
      appear in no captured remote payload, and a `privacy_block` is emitted.
- [ ] AC-2: the same for `.env`, `.aws/credentials`, and `.netrc` sentinels.
- [ ] AC-3: with one unrelated user row (`src/vendor/**`) declared, the AC-1 sentinel is still
      blocked — BR-2's additive semantics.
- [ ] AC-4: with `disable_default_boundaries = true` and **no** user rows, the sentinel **is**
      forwarded, and `unbounded_root_warning` fires for a `Home` root. Paired case: same
      opt-out plus one unrelated user row → sentinel still forwarded, warning does **not** fire.
- [ ] AC-4.1: with a user row `**/.env` as `redact-then-remote` and the builtin `**/.env`
      (`local-only`) both present, `match_path(".env")` returns the **user's** row. Assert on
      the governing row's `origin` **and** `mode` — not on the block outcome, which both modes
      produce today and which therefore cannot distinguish the two orderings.
- [ ] AC-6: every sentinel contains the literal `SENTINEL` and is obviously synthetic. No
      fixture resembles a real provider key shape.
- [ ] AC-7: a path reaching the same file through a symlink and through a `..` segment is
      blocked in both spellings, proving the builtin globs are matched against the canonical
      provenance form (BR-8).
- [ ] AC-5 (mutation): each of AC-1, AC-2, and AC-3 records in its doc comment the mutation
      that makes it fail — removing `DEFAULT_BOUNDARIES` from `effective_boundaries()`'s
      composition — and the mutation is **actually run** and its failure count written down
      before the task is called complete.

## Technical Notes

AC-4.1 is the one most likely to be written wrong, and the spec says why: both `BoundaryMode`
arms fail closed at `egress::inspector` today ("fail-closed on every boundary mode"), so a test
that asserts "the payload was blocked" passes under **either** ordering and proves nothing
about BR-2.1. Assert the identity of the row the matcher returned. This is LESSON-550's rule —
assert the thing that would change if the guard were wrong.

AC-5 is not satisfied by writing the mutation in a comment. Run it: delete the builtin arm from
the composer, run the three tests, confirm all three go red, record the number, restore. A green
suite that could not have failed is REQ-592's LESSON-569, seven times over.

Sentinels: `SENTINEL-REQ597-SSH-KEY-NOT-A-REAL-KEY` and siblings. LESSON-497 — plant sentinels,
not lookalikes; a fixture shaped like a real `sk-…` or `AKIA…` credential trips scanners and
teaches the next reader the wrong lesson.
