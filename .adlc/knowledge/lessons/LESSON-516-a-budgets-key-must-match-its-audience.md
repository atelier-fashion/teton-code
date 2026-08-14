---
id: LESSON-516
title: "A notice budget's key must match its audience, and its spend must match delivery"
component: "daemon/server"
domain: "web-setup"
stack: ["rust", "daemon"]
concerns: ["security", "observability"]
tags: ["rate-limiting", "budget", "announcement", "burn-attack", "audience-keying", "spend-on-delivery", "BUG-166"]
req: BUG-166
created: 2026-08-14
updated: 2026-08-14
---

## What Happened

REQ-572's verify pass budgeted the `WebSetupRejected` announcement at one per
connection lifetime — an `AtomicBool` swapped unconditionally before the
publish. The reasoning ("the second notice says nothing the first did not")
was sound *for one audience*, but the key didn't carry the audience: the
notice lands in the **targeted session's** transcript, and the bool was keyed
on the **offending connection**. Two failures followed from that one mismatch
(BUG-166, rated High by the post-merge security re-verification):

- **Burn**: one refused commit against a plausible-length session id naming
  *nothing* spent the bool on an audience of zero — publishing into a session
  nobody holds reaches nobody — and every later refused commit from that
  connection against **real** sessions was silent. The attacker needed no
  real id to disarm the notice; the real ids came afterwards, from the
  ungated `session/list`.
- **Misroute**: a connection refused on session A and then on session B
  announced only into A. B's user — a different person watching a different
  transcript — was never told.

The fix keyed the budget per (connection, session) and spent it only when the
registry answered for the id. The existence check ordered *before* the spend
did double duty: junk ids burn nothing, and — because only daemon-minted ids
ever enter the set — the per-connection `HashSet` stays bounded by real
sessions instead of attacker-mintable strings (the `session/attach`
allocation trap). It also stopped handing monitor-scope subscribers, whose
delivery policy is "all sessions", envelopes wearing attacker-chosen ids.

## Lesson

When budgeting or rate-limiting a user-facing notice, ask three questions
with one answer each, and make the code's shape give the same answers:

1. **Who is the audience?** The budget's key must include every dimension the
   audience varies over. A per-connection key for a per-session audience
   means the first target's user hears for everyone. "The second notice says
   nothing the first did not" is only true when both notices reach the same
   reader.
2. **When is it spent?** Debit at *delivery* (or as close to it as the
   architecture allows), never unconditionally at decision time. A budget
   spent before the audience is known to exist can be **burned** by aiming at
   nobody — one free call disarms the alarm.
3. **What bounds the key set?** If the key includes an attacker-supplied
   value, gate entry to values the system itself minted (existence check
   before insert), or the budget map becomes the allocation attack it was
   protecting against.

And when a suppression budget exists, decide *explicitly* whether suppressed
events carry information. If they do (the grant-announcement precedent: each
suppressed grant is a different grant), carry arrears. If they are
byte-identical duplicates to an identical audience, arrears is noise — record
which case you're in where the next reader will look.

## Why It Matters

A defense-in-depth *announcement* leg is exactly the leg nobody notices
failing: enforcement still refuses, tests of the refusal stay green, and the
only symptom is a user who was never told. BUG-166's burn attack reduced
BR-4's "the user hears about tampering attempts" to "the user hears about at
most one, if the attacker is polite" — with one free RPC. The audit rated it
High; the fix was small; the design review question ("does the budget's key
match the notice's audience?") would have caught it at the spec table.

## Applies When

- Adding any rate limit, budget, or once-only guard on user-facing notices,
  alerts, or audit events — especially ones an untrusted caller can trigger.
- Reviewing a "publish at most once" bool or counter: check what the *key*
  is, what the *audience* is, and whether a caller can spend the budget
  without the audience existing.
- Designing dedup/suppression for security notifications, where "says
  nothing new" claims must be scoped to "to the same reader".
