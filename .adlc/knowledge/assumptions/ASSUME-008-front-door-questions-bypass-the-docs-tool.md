---
id: ASSUME-008
title: "Front-door setup questions reach the docs tool when the guide thins"
status: open
component: "tetond/harness"
domain: "harness"
made_during: REQ-577
created: 2026-08-15
updated: 2026-08-15
---

## Assumption

REQ-577's spec assumed the local model will call `teton_docs` when the tool's
topic index names the subject, making the tool a safe "growth path" (BR-10):
knowledge moved out of the resident guide into a bundled topic would still be
reached on the question shapes that need it.

## Evidence So Far

Live A/B (verification.md, rounds 1–3, 2026-08-14): on the provider-setup
front-door shapes, the model answered from the resident guide's inline
recipes and called `teton_docs` in **0 of 11** candidate sessions — the
cheaper path won every time. Explicit probes ("what topics can teton_docs
show?", a web-setup question) did reach the tool and completed without a
prompt, so the tool *works when asked for*; what is unproven is that the
model reaches for it when the resident guide no longer holds the answer.

## Status: open (partially invalidated for front-door shapes)

The growth-path premise is unexercised in exactly the case it exists for.
The prompt margins are now thin (93/141 bytes over the floor), so the
pressure to move guide content into topics is real and will arrive.

## Implication

Before moving any load-bearing fact out of `self_config.md` into a bundled
topic (the ADR-2 fallback posture), run the front-door A/B against the
thinned guide and prove the shapes still succeed via the tool. If they do
not, the fix is a prompt affordance (a dictated "for X, read `teton_docs`
topic Y" clause), not a bigger guide — and that clause needs its own live
verification (BUG-168's rule: rewordings are unverified until A/B'd).
