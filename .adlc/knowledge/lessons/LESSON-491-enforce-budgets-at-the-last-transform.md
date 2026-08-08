---
id: LESSON-491
title: "Derive a budget through every transform, then enforce it after the last one"
component: "daemon/harness"
domain: "architecture"
stack: ["rust", "daemon"]
concerns: ["correctness", "reliability"]
tags: ["budgets", "derivation", "render-chain", "input-caps", "lesson-446"]
req: REQ-562
created: 2026-08-08
updated: 2026-08-08
---

## What Happened

REQ-562's scan input cap was wrong three times, each time because the
derivation stopped one transform short of what the engine receives:

1. **Picked beside the window, not from it** — a flat 64 KiB cap against an
   engine that refuses prompts over ~30 KiB. Every payload in the 30–64 KiB
   band passed the cap, rendered into a prompt, and came back as an opaque
   engine error — blocking with the wrong reason (LESSON-446's two-numbers
   shape, which the code even cited while committing it).
2. **Derived through the prompt builder only** — the fix subtracted the
   instruction, contract, and header, and budgeted the ADR-009 frame-defusing
   growth. But `render_duty` runs *after* the prompt builder: control-token
   defusing grows `<|`-runs by up to one byte in two, and the ChatML envelope
   adds a fixed overhead. A payload at the derived cap could still render 33%
   past the window.
3. **Fixed structurally** — `scan` now measures the *rendered* prompt (the real
   `render_duty` output, larger arm) against the engine budget and refuses
   before the model call. The constant stays as the cheap first filter; the
   measurement is the enforcement. The bytes-per-token convention is labeled an
   estimate, and the engine's own over-window error remains as backstop.

A fourth instance appeared in the same REQ at a different pair: the cap
(27 KiB) landed below the harness context budget (32 KiB) — two subsystem
budgets, independently chosen, whose collision guaranteed long sessions would
fail closed. Resolved by chunked scanning, with the total cap derived as a
stated multiple of the per-chunk cap clearing the context budget with margin.

## Lesson

A derived constant is only as good as the furthest transform it accounts for,
and transforms accrete. The durable fix is not a better formula — it is
enforcing the bound where the transformed artifact exists: measure the real
rendered output immediately before the consumer, and keep the constant as a
fast-path filter whose comment says it is an estimate.

Corollary: when two budgets constrain one flow (context budget → body size →
scan cap → prompt → engine window), write the chain down once and derive each
number from its neighbor. Any two "independent" numbers on one chain are a
collision waiting for the input that exposes it.

## Why It Matters

Every failure here was fail-closed, so nothing leaked — but each blocked with a
*wrong reason*, which is BR-3's distinction collapsed by arithmetic: the user
is told the scan could not run when the truth is the payload was too large, or
told nothing useful at all. Wrong-reason refusals send users to the wrong
remedy and, at sufficient frequency, get the feature switched off.

## Applies When

- Any input cap protecting a downstream window (token limits, message sizes,
  buffer bounds) where transforms (escaping, sanitizing, enveloping,
  tokenizing) sit between the cap check and the consumer.
- Reviewing a derivation comment: walk the actual call chain from the check to
  the consumer and list every transform; each one missing from the arithmetic
  is a latent band of wrong-reason failures.
- Two constants in different modules that constrain the same flow
  (LESSON-446): make one derive from the other or make the code measure.

Related: [[LESSON-446]], [[LESSON-488]].
