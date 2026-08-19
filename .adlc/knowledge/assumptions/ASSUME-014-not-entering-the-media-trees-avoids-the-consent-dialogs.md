---
id: ASSUME-014
title: "Not entering the top-level media and library trees is what keeps a home-rooted walk from raising macOS consent dialogs"
status: unresolved
req: REQ-583
created: 2026-08-19
resolved:
---

## Assumption

REQ-583 A-3: the Media & Apple Music / Photos / "data from other apps"
dialogs seen in the 2026-08-18 incident were the ordering effect of a walk
entering `~/Music`, `~/Pictures`, `~/Library/…`; a walker that does not enter
those trees (BR-12, `HOME_TOP_LEVEL_SKIPS` + media bundle suffixes) raises no
dialog, and Teton need not — and does not — query TCC for consent state.

## Context

The pruning position (directly under a home, plus bundles by suffix), the
`/System/Volumes/Data` firmlink handling and the `(dev, ino)` home identity all
rest on this. It is consistent with the incident's own shape (dialogs arrived
in APFS readdir order) and with ADR-007's attribution note, but the dialog
itself is an OS side effect no automated harness can observe: TASK-180's
scripted live A/B from `~` showed every walk ending by budget within seconds,
and could not see whether a dialog appeared.

## Resolution

(unresolved — `docs/manual-verification.md` REQ-583 runbook step (b): run
`cd ~ && teton`, ask for the repo, and watch for Media / Photos / other-apps
dialogs at a real terminal. Validate or invalidate here.)
