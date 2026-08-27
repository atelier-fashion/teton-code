---
id: LESSON-568
title: "An ADR's causal sentence is exactly as unverified as an untested line of code"
component: "cli"
domain: "clients"
stack: ["rust", "cli", "daemon"]
concerns: ["developer-experience", "reliability", "test-determinism"]
tags: ["adr", "doc-drift", "mutation-testing", "architecture-record", "rationale", "verify-dont-trust"]
req: REQ-592
created: 2026-08-26
updated: 2026-08-26
---

## What Happened

REQ-592 shipped nine ADRs. **Three of them had correct conclusions resting on incorrect
mechanisms**, and not one was caught by reading.

- **ADR-3** justified putting the flush verb in the event pump with: "a flush hung in
  `hand_off_after_turn` drops buffered text on **every failed turn**." It would not have. Every arm
  of the turn loop writes through `Surface::line()` after `conn.call` returns, and `line()` emits
  the pending buffer first. An implementer deleted *both* flush calls and the entire pty suite
  stayed green. The decision was right; the reason was invented.
- **ADR-4** prescribed a call site before the permission prompt. Implementing it showed the call
  bought nothing (the callee already flushed) and cost something real (it cleared the code-fence
  bit at a mid-turn pause, re-flowing a resumed code block). Withdrawn — then the security audit
  showed the *withdrawal* was also wrong, because the property was left resting on the callee
  happening to flush. Two amendments, on one ADR, in one REQ.
- **ADR-9's OQ-4** recorded "width is read per flushed block", which contradicted ADR-1's own
  constructor signature in the same document. The implementation followed the signature, so the
  width froze at startup and a terminal resize never took effect — reintroducing the exact defect
  the REQ existed to fix.

Every one surfaced the same way: someone mutated the fix and watched what failed.

## Lesson

**A confident causal sentence in an architecture document has the same epistemic status as a line
of code with no test.** It compiles in the reader's head, it survives review, and it is load-bearing
for every task brief that quotes it. ADR-3's wrong sentence propagated into three task briefs
before anything contradicted it.

The operational corollary is sharper than "write better ADRs":

**When you fix an invariant, enumerate every site that can violate it before you fix the one in
front of you.** REQ-592 got this wrong three times in a row, always with correct reasoning at one
scope too narrow — removed a fence-clearing call at a mid-turn pause and left the same call on a
120 ms poll; split the verb so the poll was safe and left it bound to *every RPC*, so `/cost`
mid-broadcast did the same damage; guarded one consent arm and left two siblings in the same
`match` resting on the property that commit had just called insufficient.

**The call-site sweep was worth more than any individual fix**, because it turns enumeration from
something you remember into something the build checks. And a sweep that *counts* is weaker than
one that *region-checks*: moving a required call outside the region it guards keeps the count
identical.

## Why It Matters

Doc drift here is not cosmetic. `.adlc/context/architecture.md` — the durable project record,
outliving the REQ — described the pre-fix design until a confirmation review caught it. A future
reader would have inherited a design that no longer existed, stated with total confidence.

## Applies When

Writing any ADR that explains *why* a decision is correct — mark the causal claim as a hypothesis
until something fails when it is false. Amending an ADR mid-implementation — propagate the
retraction to the source comments the ADR was extracted from, not just the ADR (REQ-592 shipped
retracted rationales in `render.rs` and `client.rs` while the amended ADRs said otherwise). Fixing
any invariant with more than one enforcement point — write the sweep first.

## Related

- [[LESSON-547]] — a rule that crosses a seam is owned by exactly one side; the pointer-not-
  description discipline is what makes the enumeration writable down.
- [[LESSON-517]] — the sanitizing seam owns the styling too; REQ-592's one ADR whose mechanism
  held, because it was derived from a defect that had already shipped.
