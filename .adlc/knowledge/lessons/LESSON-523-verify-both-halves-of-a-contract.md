---
id: LESSON-523
title: "A named example is verified against both halves of its contract — the vendor's and the product's own"
component: "daemon/config"
domain: "harness"
stack: ["rust", "daemon"]
concerns: ["correctness", "developer-experience"]
tags: ["contract-verification", "endpoint-semantics", "named-examples", "seam-test", "provider-recipes", "bug-170", "lesson-512"]
req: REQ-577
created: 2026-08-15
updated: 2026-08-15
---

## What Happened

REQ-577's vendor recipe catalog shipped six provider entries, each carrying a
`Verified 2026-08-14` comment naming the vendor doc page its endpoint was read
from — LESSON-512 applied conscientiously. Every entry was wrong anyway. The
recipes shipped vendor **base URLs** (`https://api.moonshot.ai/v1`) while
Teton's `--endpoint` is the **absolute request URL the adapter POSTs
verbatim**; and the Anthropic entry shipped `endpoint: None` on the premise
"the kind knows its own address," which `Config::validate` refutes with
`MissingEndpoint` — after `provider add` has already stored the user's key.
Six CI gates were green throughout, because every one compared the catalog to
prose copies of itself, and the invariant test restated the endpoint rule by
hand instead of reading `ProviderKind::is_remote()` — it agreed with the
mistake by sharing it. The live A/B was green too: it verified the model
*prints* the commands, and nobody ran one.

## Lesson

A named example sits on a contract with two halves: the third party's (what
the vendor serves) and the product's own (what the consuming code does with
the value). Verifying only the external half produces facts that are true and
commands that are broken. The closer is a **seam test that crosses the two**:
push each example through the product's real acceptance path
(`every_recipe_is_a_registration_the_daemon_accepts_and_an_adapter_can_post`
runs `Config::validate` on the registration each recipe implies and pins the
URL path against the adapter that will POST it — with a separate pin that the
adapter POSTs its endpoint verbatim, so the premise itself is load-bearing,
not prose). Corollary: an acceptance run that checks what the model *says*
must execute at least one of the commands it blesses — text-level A/B cannot
see a semantic defect in the text.

## Why It Matters

Canonicalizing a wrong fact is worse than leaving it loose: REQ-577 took two
pre-existing README defects (BUG-170), promoted them into a typed catalog
described as "verified, not recalled," resident in every system prompt, and
locked them in with bidirectional drift gates — so the wrong value became the
thing a maintainer must fight the test suite to change. The failure lands on
the first real user as a 404 one step removed from its cause, or a stored
credential for a registration that was always going to be rejected.

## Applies When

Shipping any catalog/example/recipe naming an external system (LESSON-512's
territory, one layer deeper); reviewing a "verified against docs" claim — ask
*which* contract halves were consulted; writing drift gates — check they
reach the consuming seam, not only prose copies; designing acceptance runs
for generated commands — execute one, don't just read it.

## Related

- [[LESSON-512]] — the external half (named examples are test vectors).
- BUG-170 — the shipped defect this lesson is drawn from.
