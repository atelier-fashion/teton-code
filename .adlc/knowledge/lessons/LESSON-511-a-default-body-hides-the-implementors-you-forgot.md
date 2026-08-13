---
id: LESSON-511
title: "A default trait-method body makes 'who forgot to override this' a stale human census"
component: "daemon/egress"
domain: "privacy"
stack: ["rust", "daemon"]
concerns: ["security", "reliability"]
tags: ["trait-default", "silent-inheritance", "compile-time-enforcement", "event-sink", "audit-signal", "br-1"]
req: REQ-571
created: 2026-08-13
updated: 2026-08-13
---

## What Happened

REQ-571 added a client-visible `provenance_rejected` audit event and a
`PrivacyEventSink::provenance_rejected` method to carry it. The method shipped
with a **default no-op body**, on the documented reasoning that every sink which
reaches a user already overrides it and only the subscriber-less sinks inherit
the drop. The Phase-5 security review examined this and signed off, writing down
the set of inheritors explicitly: "the implementations that would inherit the
default are `NoopSink` and the capture fixtures in the egress suites."

That census was wrong. When the default body was later removed — making the
method required — the compiler immediately failed on an **eighth** sink impl the
review had not counted: `CountingSink`, an inline test fixture in
`egress/lookup.rs`. A careful reader, reasoning about a security-relevant audit
signal, had enumerated the implementors of a trait and missed one.

## Lesson

A default method body converts "did every implementor handle this?" from a
compiler question into a human one, and a human enumeration of a trait's
implementors goes stale the moment someone adds an impl in a file the enumerator
did not open. For a method that carries a **security or audit signal**, the
silent inheritance is the failure itself: the new sink does not error, does not
warn, and drops the signal (LESSON-505 — an audit control only as strong as its
weakest silent path).

Make the method **required** (no default body). The cost is an explicit — often
empty — impl at each subscriber-less site, and that is exactly the point: an
empty body is a *stated* decision at a known location, and a *new* implementor
that forgets becomes a compile error, not a silent gap. This is LESSON-443's
"a required field with no `Default` enforces every call states X," applied to
trait methods rather than struct fields: the same move, the same reason.

Corollary for reviewers: when your finding depends on an enumeration of a
trait's (or an enum's, or a call graph's) members, do not hand-count them —
that census is the artifact most likely to be stale. Ask whether the compiler
can be made to keep the list instead, and prefer that fix to writing the list
down correctly this once.

## Why It Matters

The forgotten implementor is invisible precisely when it matters: a future sink
that is a real delivery path to a user, added by someone who never saw the
default body, drops every `provenance_rejected` and the user is never told a
boundary refusal happened. The mitigation reads as sound in review ("all current
sinks override it") and is one added file away from false. The required-method
change did not just theoretically close the trap — it found a live gap on the
same change that introduced the discussion, which is the whole argument in one
event.

## Applies When

- Adding a trait method that carries a security decision, an audit/telemetry
  signal, or any "you must handle this" obligation — reach for a required method,
  not a defaulted one, unless the default is genuinely the safe answer for every
  present *and future* implementor.
- Writing or reviewing a finding whose correctness rests on "the implementors of
  X are {…}" / "the only callers are {…}" — treat the enumeration as the weak
  link and ask whether the compiler can enforce it (required method, non-`Default`
  field, exhaustive match, `#[non_exhaustive]` forcing a wildcard-with-intent).
- Deciding whether a defaulted trait method is a convenience or a latent hole:
  it is a hole exactly when a wrong (silent) override would be a defect and the
  set of implementors is open. See [[LESSON-443]], [[LESSON-505]], [[LESSON-508]].
