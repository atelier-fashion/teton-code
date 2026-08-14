---
id: TASK-139
title: "Docs: README hand-edit section, drift note, spec closeout"
status: draft
parent: REQ-574
created: 2026-08-14
updated: 2026-08-14
dependencies: ["TASK-137"]
repo: teton-code
---

## Description

Retire the documentation debt this REQ exists to fix: the README teaches a
commented hand-written `[web]` block next to `/web setup`; with preservation
in place the two stop being in tension and the docs say so. Check every other
doc surface that describes the rewrite behavior, then tick the spec's
acceptance boxes.

## Files to Create/Modify

- `README.md` — the "Or write the table by hand" section (~347-372): state that daemon-side saves preserve comments, unknown keys, and ordering (replacing any residual rewrite caveat); verify the drift-check note (~334-346) still names only surfaces that exist after TASK-137 (it names `web_setup_ui.rs` constants and the contract suite — confirm no reference to removed warning text)
- `crates/tetond/src/harness/self_config.md` — check for any description of whole-file rewrite behavior; update if present, note "not present" if absent
- `.adlc/specs/REQ-574-comment-preserving-config-persistence/requirement.md` — tick satisfied AC checkboxes; status flip to `complete` happens at Phase 6, not here

## Acceptance Criteria

- [ ] README no longer warns (anywhere) that daemon-side saves destroy comments or unknown keys; the hand-written block section reflects preservation
- [ ] The README drift-check triple (backend rows / `[web]` keys / `web_setup_ui.rs` constants) still holds — no stale reference to the removed warning
- [ ] `self_config.md` reviewed; updated or explicitly confirmed clean
- [ ] Spec AC checkboxes updated to match reality (each box ticked only if its test exists and passes)
- [ ] `rg -n "rewrites the whole config" --hidden` over the repo returns no hits outside `.adlc/` history artifacts

## Technical Notes

- Doc-only task plus spec bookkeeping — no code. Runs after TASK-137 so the
  docs describe shipped behavior, in parallel with TASK-138.
