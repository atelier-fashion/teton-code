---
id: TASK-234
title: "the ceiling's configuration, opt-in and absent by default"
status: complete
parent: REQ-588
created: 2026-08-22
updated: 2026-08-22
dependencies: []
---

## Description

BR-5's config half, ADR-6/OQ-3. `[cost] prompt_ceiling_usd`, absent by default.

"Off" means the check **does not exist** rather than runs-and-permits: with no ceiling configured nothing builds an accumulator and nothing looks up a price, mirroring how `[privacy] redact` installs its gate only when true.

## Files to Create/Modify

- `crates/teton-core/src/config.rs` — the `[cost]` section and its field
- `crates/teton-core/src/config_doc.rs` — comment-preserving round trip (REQ-574)

## Acceptance Criteria

- absent ⇒ `None`, and a config with no `[cost]` section loads unchanged
- a declared value survives a comment-preserving round trip with its comments intact (REQ-574's rule)
- a malformed or negative value is a **structural** refusal at load, matching conventions.md's split between structural and incomplete
- the value is parsed to integral micro-cents at the edge, so no float reaches the arithmetic (ADR-3)
