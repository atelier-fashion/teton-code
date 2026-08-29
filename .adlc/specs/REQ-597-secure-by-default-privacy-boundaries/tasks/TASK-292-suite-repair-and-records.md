---
id: TASK-292
title: "Suite repair, CHANGELOG, and the architecture record"
status: pending
parent: REQ-597
repo: teton-code
created: 2026-08-29
updated: 2026-08-29
dependencies: [TASK-290, TASK-291]
---

## Description

Close out the default flip: reconcile the whole workspace suite, and write down what changed
for a user and for the next author.

## Files to Create/Modify

- Whatever tests TASK-287 left moved — reconciled and justified.
- `CHANGELOG.md` — the user-facing entry.
- `.adlc/context/architecture.md` — the key-pattern entry for the secure default.
- `docs/` — the `[privacy] disable_default_boundaries` key, if the config keys are documented
  there.

## Acceptance Criteria

- [ ] `cargo test --workspace --no-fail-fast` is run and the output **grepped for `FAILED`** —
      a summed "N passed, 0 failed" from a fail-fast run is a floor, not a total (LESSON-533).
      Record the before/after test counts.
- [ ] Every test moved by TASK-287 is listed with the reason it moved and the shape of the fix
      (premise re-established vs fixture renamed). No test was fixed by weakening the assertion
      it exists to make.
- [ ] CHANGELOG entry states the behaviour change in the terms a user will feel it: sessions now
      block thirteen credential-shaped path patterns by default, the opt-out key is named, and
      the false-positive trade is stated rather than buried.
- [ ] `architecture.md` gains the pattern entry: *a secure default plus one explicit, greppable
      opt-out* — the shape BUG-202 settled on for `allow_cleartext`, now with a second instance
      (LESSON-578). Name the composition-site rule and why the writer never sees a builtin.
- [ ] The three deferred open questions (OQ-2 refusal-vs-warning, OQ-3 config-document
      discoverability, OQ-4 weakening-user-row) are recorded as still open, not silently
      inherited.

## Technical Notes

The suite repair is the real work here and it is where a wrong shortcut hides. The temptation
when a test goes red is to add `disable_default_boundaries` to a shared fixture helper and move
on — which would switch the defaults off for a large slice of the suite and quietly delete the
coverage this REQ just bought. Set the opt-out on the *individual* fixtures whose premise
genuinely requires an empty set, and say so in each.

Two counts belong in the commit body: how many tests moved, and the AC-5 mutation's failure
count from TASK-290. Both are the evidence that the suite can still fail.
