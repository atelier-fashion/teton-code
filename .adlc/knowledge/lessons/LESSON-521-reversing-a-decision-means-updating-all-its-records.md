---
id: LESSON-521
title: "Reversing a documented decision means updating every record of it, not just the current REQ's docs"
component: "adlc/knowledge"
domain: "process"
stack: ["adlc"]
concerns: ["developer-experience", "reliability"]
tags: ["documentation", "decision-reversal", "traceability", "stale-docs", "req-576", "bug-162"]
req: REQ-576
created: 2026-08-14
updated: 2026-08-14
---

## What Happened

REQ-576 reversed a deliberate, documented decision: config/set had been kept at
BR-10(b) layer (a) only after BUG-162, on the "removes immediacy, not capability"
reasoning. The implementation rewrote the *code* comment (`handle_config_set`)
and the current REQ's docs thoroughly — but the architecture review found the
decision was still recorded, unqualified and now false, in **three other places**:
REQ-570's architecture (the table row `config/set … layer (b): no`), BUG-162.md
(the "related surface" row calling it "deliberately downgraded"), and REQ-575's
architecture ADR-2 (which named config/set a "stated residual for the life of
REQ-576"). A reader consulting any of those would take a superseded conclusion as
current.

## Lesson

A deliberate decision is usually recorded in more than one artifact: the origin
bug or finding that prompted it, the spec/architecture that codified it, and any
intermediate REQ that deferred or deferred-to it. When a later REQ **reverses**
it, grep the repo for the decision's fingerprints (the method name, the
catchphrase — here "removes immediacy", "deliberately downgraded", "stated
residual") and update *every* hit to point at the reversal, in the style each
file already uses (a "Correction (date, REQ)" addendum for a resolved bug, an
"Update (REQ-N has landed)" note for a residual). Reversing the code but leaving
the upstream records stale is how a future audit reads the old decision as still
in force.

## Why It Matters

Stale decision records are load-bearing lies: the next person who audits the
resolved bug, or reads the foundational spec, gets the pre-reversal answer with
no signal it changed. The cost is a wrong security/architecture conclusion drawn
confidently from an authoritative-looking doc.

## Applies When

Any REQ that reverses or supersedes a previously-documented decision (especially
a security posture). Add the record-sweep to the task that owns the reversal's
docs. Relatedly, BR-5-style "standing obligation" notes should name where the
decision is recorded so the sweep target is known.
