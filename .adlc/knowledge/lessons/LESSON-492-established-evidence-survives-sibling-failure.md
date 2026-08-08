---
id: LESSON-492
title: "A composite guard's failure path must not discard evidence a completed pass established"
component: "daemon/egress"
domain: "privacy"
stack: ["rust", "daemon"]
concerns: ["security", "correctness"]
tags: ["fail-closed", "composite-guards", "session-taint", "evidence", "observability"]
req: REQ-562
created: 2026-08-08
updated: 2026-08-08
---

## What Happened

REQ-562's scan is two passes: a deterministic pattern pass over the whole
payload, and a chunked model pass. The composition rule was honest about
completeness — `scanned: true` only when both passes finished — so any chunk
failure collapsed the whole verdict to `Unavailable`, *discarding the pattern
pass's already-established High findings*.

Both blocks refuse the payload, so nothing leaked. But the two causes carry
different consequences: a `Redaction` block taints the session (the model that
authored the payload can restate the secret next turn), while `ScanUnavailable`
deliberately does not (a scanner that never looked learned nothing — a rule
this same REQ ratified after a transient engine stall was found permanently
pinning sessions). So a transient chunk failure *downgraded a
deterministically-earned pin* and told the user "the scan could not run" when a
credential-shaped string had in fact been found by a pass that ran to
completion.

The same REQ produced the sibling failure at the other end: the model pass's
Low-confidence findings were computed, then consumed by nothing — no event, no
log, no CLI line. The pass existed, ran, cost latency on every remote call, and
had no observable effect except that its *failure* could block. Three reviewers
found it independently.

The fixes: the verdict carries established High evidence in a field outside the
findings-iff-`Findings` invariant, and the block cause consults it — an
`Unavailable` with evidence reports (and taints) as `Redaction`; Low findings
gained a reporting surface (daemon log, kind+span only) that the dogfooding
recall measurement reads.

## Lesson

Two dual rules for guards built from multiple passes:

1. **Facts survive failures.** When pass B fails, everything pass A completed
   and established is still true. A composition that collapses to a single
   "could not run" state erases facts the system already paid to learn — and
   any downstream decision keyed on the cause (taint, reporting, remedy)
   silently degrades.
2. **Every computed verdict needs a consumer.** A pass whose output nothing
   reads is decorative, and worse than absent: it costs latency, implies
   coverage in every design document, and its only causal power is its failure
   mode. Trace each computed value to the surface that consumes it before
   calling the feature shipped.

## Why It Matters

Composite guards are the norm at privacy boundaries (provenance + content,
pattern + model, static + dynamic). The failure-handling code is written last,
under review pressure, and "collapse everything to the safe state" *feels*
conservative — but the safe state for the *payload* (block) is not
automatically the safe state for the *session* (taint) or the *user* (remedy).
Enumerate the downstream decisions keyed on the guard's outcome and check each
against the failure path separately.

## Applies When

- Any multi-pass validator where passes can fail independently — ask "what did
  the completed passes establish, and where does that evidence go on the
  sibling's failure path?"
- Wiring a new outcome/cause enum into taint, billing, retry, or reporting:
  table every (outcome × consequence) cell, not just the blocking column.
- Shipping a pass whose output is "reported": name the surface, and make a test
  fail if the report line disappears (a fixture-held value is not a surface —
  LESSON-485).

Related: [[LESSON-485]], [[LESSON-447]], [[LESSON-488]].
