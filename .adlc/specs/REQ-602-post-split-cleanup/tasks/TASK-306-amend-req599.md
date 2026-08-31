---
id: TASK-306
title: "Amend REQ-599's three unevidenced criteria and reconcile ADR-4"
status: draft
parent: REQ-602
created: 2026-08-31
updated: 2026-08-31
dependencies: []
---

## Description

AC-6 and AC-8. Three REQ-599 criteria were ticked without evidence, and ADR-4's
plan table was never reconciled with what shipped.

Per ADR-4 of this REQ: **amend, do not re-tick.** Two of the three describe
things no present action can make true.

## Files to Create/Modify

- `.adlc/specs/REQ-599-decompose-the-turn-path/requirement.md`, `architecture.md`

## Acceptance Criteria

- [ ] **AC-11 amended, not met.** Two `macos-latest` runs were cancelled
      (`f64d99b`, `56f3777` — steps 6 and 7) by CI's `cancel-in-progress`. It is
      a claim about history; the amendment says what happened and names the
      cause. macOS is the runner that caught the last ordering defect, which is
      why this is worth recording rather than waving through.
- [ ] **AC-4 amended** to the re-attachment property that actually shipped. Its
      module-ownership clause describes a check REQ-599's own ADR-5 argued is
      uncomputable.
- [ ] **AC-6 met or narrowed**, not amended away — the fixture's scenario gap
      (no skill expansion, no consent) is fixable, unlike the other two.
- [ ] ADR-4's step table reconciles with what shipped: it names **five**
      modules that do not exist (`types`, `consent`, `egress`, `session`,
      `turn`).
- [ ] **The session-lifecycle slice shipped nothing and is recorded nowhere as
      deferred.** Either do it here or defer it explicitly with a reason — a
      third option, leaving it unrecorded, is what this AC exists to close.
