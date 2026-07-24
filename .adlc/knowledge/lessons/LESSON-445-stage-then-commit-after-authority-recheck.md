---
id: LESSON-445
title: "Side effects of a minutes-long operation must be staged, then committed only after re-checking authority"
component: "daemon/consent"
domain: "inference"
stack: ["rust", "tokio", "daemon"]
concerns: ["security", "reliability", "concurrency"]
tags: ["check-then-act", "stage-commit", "supersede", "model-set-race", "engine-slot", "adversarial-review"]
req: REQ-547
created: 2026-07-24
updated: 2026-07-24
---

## What Happened

Wiring the real engine put a minutes-long operation (map 18 GB of weights + run a
benchmark) between "the recorded decision authorizes this model" and "make its
engine live". The first implementation checked the decision, loaded, and installed
the engine — so a `model/set` landing during the load could open the tier for a
deselected model. The first *fix* re-checked after the load and evicted by name —
and the adversarial re-review showed the loser still installed **before** the
re-check, so its eviction could destroy the successor's live engine, wedging the
tier open over an empty slot. Only the third shape held: the loader **stages** its
engine per-model, and the gate **commits** it to the serving slot strictly after the
authority re-check (abandoning it otherwise), with the load phase under the same
in-flight claim as the download.

## Lesson

When an operation takes long enough for its authorizing state to change, a trailing
re-check is necessary but not sufficient — the operation must also be **effect-free
until after that re-check**. Structure it as stage → re-check authority → commit or
abandon, key every staged artifact by the identity it serves (so a loser can never
touch a winner's work), and extend whatever mutual-exclusion claim covered the cheap
phase to cover the expensive one. Then re-review the fix as new code: both of the
first two shapes looked correct and passed every test.

## Why It Matters

These races only exist once the slow real implementation lands, so no test written
against fast fakes can fail beforehand; the failure modes (serving a model the user
deselected; a permanently wedged tier) are silent, security-adjacent, and survive
until a restart nobody knows to perform.

## Applies When

Any check-then-act where the act is slow: model/weights loading, large downloads
finalized into shared state, cache rebuilds, migrations applied after validation;
any fix pass touching gate or consent logic (LESSON-441 — re-verify the fix
adversarially, twice if it changed shape).
