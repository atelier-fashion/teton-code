---
id: TASK-112
title: "TurnRequest carries a required ResolvedEffort; both adapters emit exactly one shape"
status: pending
parent: REQ-559
created: 2026-08-11
updated: 2026-08-11
dependencies: [TASK-111]
---

## Description

The wire change. `TurnRequest` gains a **required** `effort: ResolvedEffort`
field (ADR-B), and each adapter `match`es it to emit exactly one reasoning field
or none (ADR-A). This is where BR-1 (always state the effort) and BR-4 (never
both shapes) become properties of the code rather than of a test.

Provider-native spellings live here and nowhere else (BR-3).

## Files to Create/Modify

- `crates/teton-providers/src/lib.rs` — add `pub effort: ResolvedEffort` to
  `TurnRequest` (:185), re-export the effort types from `teton-core`
- `crates/teton-providers/src/anthropic.rs` — emit `output_config.effort` in the
  body builder (:86)
- `crates/teton-providers/src/openai_compat.rs` — emit `reasoning_effort` in the
  body builder (:87)
- `crates/teton-providers/tests/conformance.rs` — the mock-transport capture
  tests for AC-1 and AC-2
- `crates/tetond/src/harness/completion.rs` (:484) and
  `crates/tetond/src/harness/duty.rs` (:832) — supply the field; **TASK-114 owns
  the real value**, so pass `ResolvedEffort::Omit(EffortOmission::ShapeNone)`
  here only if TASK-114 has not landed, and leave a `// TASK-114` marker

## Acceptance Criteria

- [ ] `TurnRequest.effort: ResolvedEffort` exists with **no `#[serde(default)]`**
      and `ResolvedEffort` has **no `Default` impl**, so every construction site
      must state an answer. There are 5 sites tree-wide (2 production, 3 test/doc)
      — all compile after the change, none by accident.
- [ ] Anthropic adapter: `ResolvedEffort::Effort(level)` → `body["output_config"]["effort"]`
      = the canonical snake_case spelling. `ThinkingFlag` → `body["thinking"]`
      = `{"type": "adaptive"}`. `Omit(_)` → neither key present.
- [ ] OpenAI-compatible adapter: `Effort(level)` → top-level
      `body["reasoning_effort"]` = the canonical spelling. `ThinkingFlag` →
      `body["thinking"]` = `true`. `Omit(_)` → neither key present.
- [ ] **AC-2 / never-both**: a test iterates every `ResolvedEffort` variant against
      both adapters and asserts no produced body contains both a reasoning-effort
      key and a thinking key. Assert on the **parsed JSON body**, not on a
      substring of the serialized bytes.
- [ ] **AC-1 / always-sent**: a mock-transport test asserts that for a provider
      resolving to `Effort(_)`, the captured body carries the effort field —
      across all four tiers and both adapters. Driven through the same
      `Transport` seam the existing conformance tests use, so no HTTP client is
      introduced into `teton-providers` (the crate has none by design, D-2).
- [ ] **ADR-H is pinned by a test**: with `Effort(_)`, the Anthropic body carries
      `output_config.effort` and carries **no** `thinking` key, even though
      Anthropic accepts both. The test names ADR-H so a future reader does not
      "fix" it.
- [ ] Every existing conformance test still passes with `Omit(ShapeNone)` supplied,
      and a test asserts the produced bodies are **byte-identical to the pre-REQ
      bodies** under `Omit` — proving the addition is inert when effort does not
      apply (BR-6, and the compatibility posture for the local tier).

## Technical Notes

**The `match` must be exhaustive with no `_` arm.** A wildcard arm defeats
ADR-A's whole purpose: adding a fourth shape later must break these two functions
until someone decides what they emit. Write out all three variants in both
adapters.

**`Omit`'s reason is not sent anywhere.** The adapter discards the
`EffortOmission` payload — it exists for the surface (TASK-116) and the event
(TASK-114), not for the wire. Do not encode the reason into the request body.

**Canonical spellings are wire spellings.** `low`/`medium`/`high`/`xhigh`/`max`
are what Anthropic, DeepSeek and Kimi accept verbatim. Do not build a
per-provider spelling map; there is nothing for it to translate. If a future
provider needs one, it belongs in that provider's adapter, not in `teton-core`
(BR-3).

**Do not clamp here.** The level arriving in `ResolvedEffort::Effort` is already
clamped by TASK-110's resolver at route time (ADR-G). An adapter that re-clamps
would be a second implementation of the rule, which is the drift AC-8's
shared-resolver requirement exists to prevent.
