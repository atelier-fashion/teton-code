---
id: ASSUME-003
title: "A 3B local model can identify paraphrased credentials and PII at useful recall"
status: unresolved
req: REQ-562
created: 2026-08-08
updated: 2026-08-08
component: "daemon/egress"
domain: "privacy"
---

## The assumption, as written

REQ-562's spec named it directly, and flagged it:

> A 3B local model can identify obvious credentials and PII at acceptable
> recall. **This is the assumption most likely to be wrong**, and unlike
> REQ-558's classifier — where "any model beats a ten-word substring list"
> made accuracy a non-issue — here a miss is a leak.

## Why it was reasonable to ship on

OQ-2 resolved to running the model pass *alongside* a deterministic pattern
pass whose five credential shapes carry all blocking power. The model pass can
therefore fail at recall without weakening the enforceable guarantee — its
misses cost only the marginal capability (paraphrased secrets, shapeless PII)
that justifies its existence, never the pattern-backed floor. OQ-3's opt-in
means nobody is exposed to the latency until they choose it.

## What is at stake if it is wrong

The model pass adds a local inference to every remote call (chunked, p50 ≤ 2s
per chunk). If its recall on paraphrased secrets is near zero, that latency
buys a decorative pass, and the honest response is to remove it — not to keep
paying for it because it is already wired (REQ-562 OQ-2 recorded exactly this
pressure: "a pattern pass makes the feature *look* like it works").

## How it gets resolved

The measurement exists and is NOT RUN: `docs/manual-verification.md` (REQ-562
section) plants paraphrased credentials no pattern shape matches and greps the
daemon log for `redact — low-confidence` report lines. The question is scoped
per OQ-2's mitigation: *what did the model catch that patterns did not* — that
number, not raw recall, decides whether the model call earns its latency.
Dogfooding with `[privacy] redact = true` produces it. Zero lines across the
planted set **is** the finding, and the trigger to revisit is that number, or
users disabling `redact` after false-positive blocks.
