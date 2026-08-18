---
id: ASSUME-011
title: "Every transient local-tier state ends through `apply_consent_outcome`, so a held turn always wakes"
status: unresolved
req: REQ-580
created: 2026-08-17
resolved:
---

## Assumption

REQ-580 holds a turn on `DaemonRuntime::tier_transitions`, a `watch` bumped
only by `apply_consent_outcome`. The hold therefore assumes that the two
transient states it waits in — `Installing` and `Loading` — always end with a
consent outcome being *applied*: the startup flow (`run_model_consent`) and
`model/set` (`install_selected_model`) both funnel there, and every outcome
(`Ready`, `EngineLoadFailed`, `InstallFailed`, `Declined`, `Superseded`,
`AlreadyInstalling`, …) bumps the watch unconditionally (ADR-2).

## Context

One known shape breaks the *predicate* rather than the wake: a loader that
reports `Ready` without committing an engine into the slot (LESSON-443's
"predicate only incidentally true"). `apply_consent_outcome` then flips
nothing (`local_available` is derived from the slot's own fact), the state
still reads `Loading` (loader present, no failure recorded), and a held turn
in that state waits until its client leaves rather than being refused each
time as it was before. Accepted as residual risk in REQ-580 because it is a
broken loader either way — the tier was already wedged until restart — and
because the client's own Ctrl-C ends the hold (ADR-3), so the wedge is bounded
by the connection, not the daemon. No timeout was added: a bound would be an
ETA in disguise, and every legitimate wait (an 18 GiB download, a slow load)
is longer than any bound one would pick.

## Resolution

(unresolved — revisit if a loader ever reports `Ready` over an empty slot in
the wild, or if a third transient state is added: the fix would be for
`apply_consent_outcome` to record a load failure when `Ready` arrives with the
slot empty, which settles the state for the classifier and the hold alike)
