---
id: LESSON-512
title: "A spec's named example is a test case, not decoration"
component: "daemon/egress"
domain: "harness"
stack: ["rust", "daemon", "keychain"]
concerns: ["security", "developer-experience"]
tags: ["web-search", "auth", "config", "spec-examples", "hardcoded-assumption", "bug-165"]
req: REQ-563
created: 2026-08-13
updated: 2026-08-13
---

## What Happened

REQ-563's External Dependencies named three example search backends — Brave,
Kagi, and self-hosted SearxNG — and its implementation hardcoded the search
credential as `Authorization: Bearer <key>`, on the reasoning (recorded in the
doc comment) that Bearer "is what an unblessed one is most likely to accept."
Two of the three named examples are the counterexamples: Brave wants
`X-Subscription-Token: <key>` and Kagi wants `Authorization: Bot <key>`, so a
user configuring either got a 401 on every search — a failure that looks
exactly like a bad key, at the moment the user is doing something else. The
feature shipped, passed review and its acceptance criteria, and worked only
for backends nobody had named (BUG-165).

## Lesson

When a spec names concrete external systems as examples, walk each one
against the implementation's assumptions before calling the requirement done
— the examples are the requirement's own test vector, chosen because they are
what users will actually configure. An assumption spelled "most likely to
accept" is a probability claim, and the named examples are the population it
must be checked against. Corollary: a "no blessed X" rule (BR-8: no default
backend ships) cuts both ways — if nothing can be defaulted, nothing about
any X's protocol can be assumed either, so the varying part must be a
config surface, not a constant. Keep the constant only as the *default* of
that surface, so existing configs are unchanged.

## Why It Matters

An acceptance suite exercises the shape the implementer assumed — every
REQ-563 auth test asserted `Authorization: Bearer` because that was the
implementation's own constant, so the suite could not disagree with it. The
only artifact that *could* disagree was the spec's example list, and nothing
in the pipeline reads examples as obligations. The cost lands on the first
real adopter of a named example, as a credential-shaped failure (401) that
sends them rotating a key that was never wrong.

## How to Apply

- At verify time, for each concrete product/API a spec names as an example,
  state what the implementation assumes about it (auth shape, wire format,
  required parameters) and check the named system's actual contract.
- When a component must speak to "any" external system with no blessed
  default, treat every fixed protocol detail as a smell: either it is
  genuinely universal or it needs a config key whose default is today's
  constant.
- Keep secrets-by-reference rules intact when adding such keys: a shape
  template with a `{key}` placeholder (validated to contain it) lets config
  describe the wire without ever carrying the credential (BR-7's pattern).

## Related

- BUG-165 — the fix this lesson is drawn from.
- LESSON-506 — config validity vs usability (the didactic-validation posture
  the new keys follow).
