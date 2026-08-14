---
id: TASK-142
title: "Consent-path persist_web_tier disposition (accept), four-method docs, AC-6 stub, finding-7 update"
status: complete
parent: REQ-576
created: 2026-08-14
updated: 2026-08-14
dependencies: [TASK-140]
repo: teton-code
---

## Description

Close the consent-path disposition (BR-6/AC-4) with a documented **accept**
decision and a no-regression test, and finish the non-code documentation:
manual-verification stub (AC-6), the four-method sweep in non-code docs (BR-5 /
AC-5 non-code half), and the REQ-572/REQ-575 disposition updates now that
config/set is closed. Non-`server.rs` files (no overlap with TASK-140). See
`architecture.md` ADR-3.

## Files to Create/Modify

- `crates/tetond/src/harness/permissions.rs` (near `persist_web_tier`, the
  consent `enable_permanent` path) — add a comment recording the ADR-3
  **accept** decision: `persist_web_tier` is raise-only within an
  already-configured `[web]` table, cannot author an endpoint/credential, and is
  deliberately **not** brought under BR-10(b) because gating it would put a
  presence prompt on the ordinary "enable permanently" answer (REQ-570 AC-8
  regression) for marginal security value.
- A consent-flow test (extend the existing `web_consent_matrix.rs` or the
  permissions unit tests) — assert the `enable_permanent` answer still persists
  the tier **without** any presence prompt/refusal (the no-regression half of
  AC-4), so the accept decision is pinned, not just prose.
- `docs/manual-verification.md` — append a REQ-576 AC-6 entry, marked
  **outstanding**: on a macOS `--features presence` build, `teton provider add`
  (or a `config/set`) raises the OS presence prompt; approve lands, cancel refuses
  with nothing written.
- `.adlc/specs/REQ-572-capability-aware-refusals-and-guided-enablement/architecture.md`
  — update the finding-7 disposition: config/set is **now closed by REQ-576** (was
  the tracked residual); the four-method BR-10(b) set is complete for the
  daemon-wide config-writers known today.
- `.adlc/specs/REQ-575-presence-attested-web-setup-commit/requirement.md` (OQ-3)
  and/or `architecture.md` (ADR-2) — mark the config/set follow-up **landed**
  (REQ-576 merged) rather than pending.

## Acceptance Criteria

- [x] `persist_web_tier` carries the ADR-3 accept rationale in a comment (BR-6).
- [x] A test asserts `enable_permanent` still persists without a presence prompt
      (AC-4 no-regression), at the strength the accept decision was made.
- [x] `docs/manual-verification.md` has a REQ-576 AC-6 entry marked outstanding.
- [x] No stale three-method / "config/set is the known next candidate" framing
      remains in `.adlc` docs; the four-method set is named where the split is
      explained (AC-5 non-code half; TASK-140 owns the `server.rs` half).
- [x] REQ-572 finding-7 disposition and REQ-575 OQ-3/ADR-2 record config/set as
      closed by REQ-576.

## Technical Notes

- This task deliberately touches no `server.rs` code (TASK-140 owns it) — clean
  file boundaries for parallel execution with TASK-141.
- The accept decision is architecture's (ADR-3); this task *records and tests* it,
  it does not re-litigate gate-vs-accept.
