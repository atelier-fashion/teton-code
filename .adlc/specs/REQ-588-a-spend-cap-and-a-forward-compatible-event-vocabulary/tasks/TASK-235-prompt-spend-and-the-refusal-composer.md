---
id: TASK-235
title: "PromptSpend, SpendBound, and the one refusal sentence"
status: complete
parent: REQ-588
created: 2026-08-22
updated: 2026-08-22
dependencies: []
---

## Description

ADR-1/ADR-2/ADR-5/ADR-7. The value types, all pure:

- `PromptSpend` — an atomic micro-cent accumulator plus an `unpriced` flag, whose **lifetime is the prompt** (ADR-1);
- `SpendBound` — which ceiling bound a refusal, in REQ-586's shape (ADR-7);
- the refusal composer — **one** sentence naming spend, ceiling, bound, and the one-call overshoot, rendered by both the model-facing error and the CLI (LESSON-529).

## Files to Create/Modify

- `crates/teton-core/src/cost_ceiling.rs` — new module: `PromptSpend`, `SpendBound`, the composer
- `crates/teton-core/src/lib.rs` — declare it

## Acceptance Criteria

- `PromptSpend` accumulates and reads back across threads; `reached(ceiling)` is the floor-crossing predicate ADR-2 defines, not a prediction
- the composer names the spend, the ceiling, the bound, and — **only when a call actually completed past the line** — the one-call overshoot
- an `unpriced` accumulator composes the ADR-3 sentence instead, naming the provider whose price is missing
- no `std::fs` and no float arithmetic in the module (source-scanned, the shape `teton_core::projects` uses)
