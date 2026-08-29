---
id: TASK-289
title: "The CLI half: boundary list reports origin, and the warning reaches a person"
status: complete
parent: REQ-597
repo: teton-code
created: 2026-08-29
updated: 2026-08-29
dependencies: [TASK-286]
---

## Description

BR-6's rendering and BR-5's user-visible surface. Both `teton boundary list` and the in-session
`/boundary list` are served by one body, so one change covers both.

## Files to Create/Modify

- `crates/teton/src/main.rs` — `boundary_list_on`: render each row's origin; rewrite the
  empty-set sentence.
- `crates/teton/src/session_ui.rs` — render `unbounded_root_warning` and
  `boundary_defaults_applied`.

## Acceptance Criteria

- [ ] BR-6: each rendered line carries the row's glob, mode, and origin, in composed order —
      user rows first. The origin is rendered from the wire value, never inferred from position.
- [ ] The empty-set branch says *why* it is empty: the list is empty only under
      `disable_default_boundaries`, and that sentence names the key. The current
      "no privacy boundaries configured. Add one with `teton boundary add`." is now misleading
      in the one case it can still describe.
- [ ] BR-5: `unbounded_root_warning` renders as a `LineKind::Notice`, **not** verbose-gated.
      The sentence names the root kind and the remedy.
- [ ] `boundary_defaults_applied` is verbose-gated — it is confirmation that the normal thing
      happened, and an ungated line on every session start is chrome.
- [ ] An unknown/absent origin from an older daemon renders as a user row rather than panicking
      or printing a placeholder.
- [ ] A user row whose glob collides with a builtin renders as **two** lines, user first. The
      listing reports the composed set as it is, so a reader can see that their row shadows a
      builtin. Do not dedupe in the renderer for tidiness — that hides the shadowing, which is
      the one thing a person reading this list to check their protection needs to see.

## Technical Notes

The two events differ in gating on purpose, and the asymmetry is the design: one says a
protection is **off** (never suppressible — BR-5, and REQ-571 BR-4's rule that an audit signal
must not be gated by anything the indicted party controls), the other says a protection is
**on** (chrome, gate it). Follow `Event::CapabilityDeadEnd`'s verbose gate for the second and
`Event::TurnQueued`'s ungated notice for the first.

Do not add a second `config/get` call for the origin. `boundary_list_on` is one call, one
listing — REQ-582 BR-2 — and the origin rides the rows already in the snapshot.
