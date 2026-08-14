---
id: LESSON-520
title: "A gate that fires before deserialization makes an invalid-payload test vacuous — use a persistable payload + a refuse/accept pair"
component: "daemon/session"
domain: "harness"
stack: ["rust", "daemon", "serde"]
concerns: ["security", "reliability", "developer-experience"]
tags: ["test-vacuity", "inspect-dont-infer", "gate-ordering", "config-set", "presence", "req-576"]
req: REQ-576
created: 2026-08-14
updated: 2026-08-14
---

## What Happened

REQ-576's AC-1 test asserted a presence-refused `config/set` writes nothing, by
inspecting the config bytes (before == after). It used a `RegisterProvider`
payload with `kind: "openai"` — **not a valid `ProviderKind`** (`openai-compatible`
is). The presence gate in `handle_config_set` runs *before* `serde_json::from_value`,
so the test was green. But the "nothing was written" assertion was **vacuous**:
even with the gate fully deleted, the invalid payload would die at
`INVALID_PARAMS` and write nothing anyway. The test could not distinguish
"gate refused it" from "parser rejected it" — the exact inference LESSON-519
warns against. The review's mutation check confirmed it: deleting the gate left
the byte-identical assertion still green.

## Lesson

When a security gate fires **before** the payload is deserialized (the correct
ordering — a caller who may not act should be refused before their input is even
parsed), a test that feeds an **invalid** payload proves nothing about the gate:
the "no side effect" inspection passes because an invalid payload has no side
effect regardless. Two things make it non-vacuous:

1. Use a payload that **would actually persist** if the gate were bypassed —
   valid, and surviving downstream validation (`Config::validate`, "remote
   provider must declare a model", etc.).
2. Pair the refused test with an **accepted** counterpart (accepting-verifier
   seam) that proves the *same* payload writes. Now a bypassed gate is
   detectable: the refused test's byte-identical assertion goes red because the
   payload would have written. Verify with a mutation (delete the gate line →
   the refused test must fail).

## Why It Matters

A green "the gate blocks the write" test that stays green with the gate deleted
is worse than no test — it certifies a security control that isn't there. The
before-parse ordering is common (it's the right ordering), so this trap recurs
wherever a gate precedes deserialization.

## Applies When

Testing any gate that runs before params-parse (presence/attestation, auth,
ancestry) with an "inspect the side effect" assertion. See
[[lesson-519-inspect-not-infer-needs-the-real-artifact]] and
[[lesson-508-a-redundant-guard-needs-its-own-test]].
