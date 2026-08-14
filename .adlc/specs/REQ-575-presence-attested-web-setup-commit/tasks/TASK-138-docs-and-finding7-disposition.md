---
id: TASK-138
title: "AC-8 manual-verification stub, REQ-572 finding-7 disposition, config/set residual note"
status: draft
parent: REQ-575
created: 2026-08-14
updated: 2026-08-14
dependencies: [TASK-135]
repo: teton-code
---

## Description

Close the documentation-side acceptance criteria: record the outstanding human
presence-build pass (AC-8), stamp the REQ-572 security-review finding-7
disposition (AC-9), and make the config/set residual a stated, tracked fact
(ADR-2 / BR-6). Non-code files only — no overlap with the server.rs edits in
TASK-135.

## Files to Create/Modify

- `docs/manual-verification.md` — append a REQ-575 AC-8 entry: outstanding until
  a person runs, on a macOS `--features presence` build, an attested `/web setup`
  commit (approve → lands; cancel → refused, nothing written). Record it as
  **outstanding** at the strength actually verified (REQ-556 precedent) — not
  ticked by reasoning or by the seam tests.
- `.adlc/specs/REQ-572-capability-aware-refusals-and-guided-enablement/architecture.md`
  — add the finding-7 disposition (AC-9): the `web/setup_commit` residual is
  closed by REQ-575; the config/set sibling surfaced during REQ-575 validation is
  tracked separately (see the follow-up REQ), and the consent-path
  `persist_web_tier` is a documented low-severity residual. Point at the closure;
  do not restate the whole analysis (the BUG-162 Resolution precedent).

## Acceptance Criteria

- [ ] `docs/manual-verification.md` has a REQ-575 AC-8 entry marked outstanding
      with the exact presence-build check to run.
- [ ] REQ-572 architecture records finding-7 as closed-by-REQ-575, with config/set
      → follow-up REQ and the consent-path residual both named (AC-9, BR-6).
- [ ] No stale "only those two methods" BR-10(b) framing remains in the docs swept
      here (AC-7's non-code half; the server.rs half is TASK-135's).

## Technical Notes

- The config/set follow-up is **REQ-576**
  (`.adlc/specs/REQ-576-presence-attested-config-set/`), created during this
  architecture phase. Reference it by id in the REQ-572 disposition and the
  config/set residual note.
- Keep the REQ-572 edit surgical: a short disposition block near ADR-3's
  accepted-residual section, matching that file's voice.
