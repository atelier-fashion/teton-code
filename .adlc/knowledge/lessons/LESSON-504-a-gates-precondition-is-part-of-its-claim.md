---
id: LESSON-504
title: "A gate's precondition is part of its security claim — check whether the adversary can mint it"
component: "daemon/session"
domain: "privacy"
stack: ["rust", "daemon"]
concerns: ["security"]
tags: ["authorization", "consent", "precondition", "routing", "self-approval", "req-569"]
req: REQ-569
created: 2026-08-11
updated: 2026-08-11
---

## What Happened

REQ-569 gated the `monitor` declaration behind a grant, and grants behind user
consent. The consent request had to be *routed* to somebody, and the rule was
written as: "a monitor is a whole-daemon read capability, so it is approved by a
surface the user demonstrably already owns — never self-rendered." That
sentence was implemented as `connection != requester && !attached.is_empty()`.

`session/create` is ungated and auto-attaches its creator. So "attached to
something" — the property standing in for "a surface the user owns" — is one
RPC an attacker issues for itself. One process, two connections: A creates a
throwaway session and becomes an eligible approver; B declares `monitor`; the
daemon routes B's prompt to A; A approves. B then reads every session on the
machine. No human anywhere, and the self-approval detector stayed quiet because
the two connections had different ids.

The gate was correct. `may_monitor` did exactly what it claimed. What was wrong
sat one level up, in the predicate deciding *who was allowed to satisfy it* —
and the review that found it had to reason about the rule's precondition, not
about the gate. A 2054-test suite was green throughout, and the task that built
the routing rule had already found and closed a *neighbouring* version of the
same hole (a daemon child as approver), which is exactly why the remaining half
looked settled.

## Lesson

When a control's decision depends on a precondition — "someone else approved",
"a peer is attached", "an admin is present", "the request came from a trusted
surface" — the precondition is part of the security claim and inherits its
threat model. Ask the only question that matters: *can the adversary bring this
about?* If any ungated operation lets the attacker manufacture the property,
the control is decorative no matter how correct the gate is.

Write the predicate against the property you actually mean. If you cannot
express it, that is the finding — you have discovered the mechanism does not
exist yet, and the honest move is to remove the path rather than ship a proxy
for it. REQ-569 removed the monitor consent path entirely rather than
re-predicating it: no sound approver predicate exists over a socket where the
daemon cannot distinguish an attacker's second connection from the user's real
client.

## Why It Matters

This is [[LESSON-443]] one level up. That lesson warns against a guard keyed on
an incidental property; this is a guard keyed on a *correct* property whose
**precondition** is attacker-mintable. The failure mode is nastier because the
gate reads as sound in isolation and the tests around it pass: the flaw is only
visible when you ask where the input to the gate comes from.

The cost of missing it was a complete bypass of the REQ's headline claim, found
only because an adversarial reviewer built a working exploit against the real
binary. Related: [[LESSON-502]] (an invariant enforced at several seams needs a
test at each), [[LESSON-505]] (the audit control added to make the residual
visible had the same blind spot).

## Applies When

Designing or reviewing any consent, approval, quorum, or delegation flow;
reading a routing rule that decides who may answer a security question;
reviewing a predicate phrased as "someone who already has X" — enumerate every
way the adversary obtains X, especially through operations deliberately left
ungated for usability.
