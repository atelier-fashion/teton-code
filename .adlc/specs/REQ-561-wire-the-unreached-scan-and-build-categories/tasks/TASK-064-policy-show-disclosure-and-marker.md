---
id: TASK-064
title: "policy show — drop the unreached marker, disclose the content class"
status: draft
parent: REQ-561
created: 2026-08-07
updated: 2026-08-07
dependencies: [TASK-063]
---

## Description

Close the loop on the user-visible surface: the four newly wired categories stop
rendering `declared, no call site yet` (AC-1), and every category discloses what
content it transmits (AC-16, BR-11 — OQ-4 resolved).

Depends on all four duty tasks, because the derived-marker scan compares the
marker against the daemon's *actual* call sites. Running it earlier would assert
a state that does not exist yet.

## Files to Create/Modify

- `crates/teton/src/main.rs` — the `policy show` renderer (~lines 1418-1505); drop the marker for the four wired categories and render each category's content class (~line 1493 holds the `"  — declared, no call site yet"` string).
- `crates/tetond/src/call_sites.rs` — verify `has_call_site()` agrees with the derived scan for all eleven categories; carry the content-class descriptor if it lives here.
- `crates/teton/tests/cli_e2e.rs` — update the `policy show` assertions (~line 1651 asserts the marker text) to the new expected output.
- `crates/teton/src/main.rs` — `policy_show_marks_the_unreached_categories_and_the_judgment_default` (~line 1908) renders a **hand-built** `migrated_snapshot()` fixture, so it tests the renderer rather than the derived marker and stayed green through TASK-060/061. Its fixture still lists all five categories as unreached, which is now stale: only `redact`, `title`, and `compact` are. Update the fixture to match reality — a rendering test fed an impossible snapshot still passes, which is exactly how a stale fixture survives unnoticed (LESSON-485).

## Acceptance Criteria

- [ ] **AC-1**: `teton policy show` renders **no** `declared, no call site yet` marker for `triage`, `shell`, `title`, or `compact`.
- [ ] The marker still renders for the categories that genuinely have no call site — `redact` (REQ-562). AC-1 is not "delete the marker"; it is "the marker becomes accurate". A test asserts the marker is still present for `redact`, which is what keeps this from being a deletion.
- [ ] `the_unreached_marker_matches_the_daemons_actual_call_sites` (`call_sites.rs:209`) passes with the derived set and the marked set agreeing on all eleven.
- [ ] **AC-16**: the rendered output names, for each of the eleven categories, the content class it transmits — and a test pins that `triage` and `compact` disclose **distinct** classes despite sharing the `scan` tier. That distinctness is the whole point of OQ-4's resolution.
- [ ] **`content_class` and `reached` render as a legible pair.** TASK-059 deliberately shipped no `Nothing` variant: `content_class` says *what a category would transmit* (intent), and `reached` says *whether it transmits anything today*. That division is right — `redact` will carry the outbound payload once REQ-562 wires it, so classing it `Nothing` would be a lie with a short shelf life. But it only discharges AC-16's "a category that transmits nothing today says so" **if the rendering presents both facts together**. A row showing a content class with no visible `reached` marker reads as a live egress path. Render them adjacently and assert it.
- [ ] `cargo test --workspace --no-fail-fast` is green.

## Technical Notes

The derived-marker test is the intended prompt (AC-1's own wording): it fails
from the moment the first call site lands and stays red until this task updates
the marker. That is by design — do not "fix" it early in a duty task by editing
the marker ahead of the call site, which would make the scan agree with a lie.

The scan at `call_sites.rs:220` finds `router.<method>(` + a literal `Category::X`.
If a duty task wired its category any other way, this task is where that surfaces
— and the fix belongs in the duty task's resolver, not in the scan. Changing the
scan to accommodate a call site it was built to detect would defeat its purpose.

Content-class disclosure is a **disclosure, not a control** (BR-11). Do not let
this task's rendering work imply enforcement; the enforcement assertion is AC-4's
egress capture in TASK-065.
