---
id: TASK-080
title: "prefix_cache protocol event with an outcome enum"
status: draft
parent: REQ-564
created: 2026-08-10
updated: 2026-08-10
dependencies: []
---

## Description

Add the wire event the spec's Events table calls for (architecture D-5), as a
single `Event` variant carrying an outcome enum — the shape REQ-563 established
for `WebLookup`, not three near-identical variants.

## Files to Create/Modify

- `crates/teton-protocol/src/events.rs` — `PrefixCache`, `PrefixCacheOutcome`,
  `PrefixCacheMiss`, `EvictionReason`; new `Event::PrefixCache` variant

## Acceptance Criteria

- [ ] `Event::PrefixCache` serializes with the snake_case `event` tag
      (`"event":"prefix_cache"`), consistent with every sibling variant
- [ ] `PrefixCacheOutcome` covers `Hit { cached_tokens, new_tokens }`,
      `Miss { reason, processed_tokens }`, `Evicted { reason }`
- [ ] `PrefixCacheMiss` covers `cold`, `divergent`, `session_switch`, `evicted`
- [ ] A serde round-trip test per outcome variant
- [ ] Any exhaustive `match` over `Event` elsewhere in the workspace still compiles
- [ ] `cargo test -p teton-protocol` passes

## Technical Notes

Mirror the doc-comment density of the surrounding variants — each one states
what it is and cites its REQ.

Check for exhaustive matches on `Event` (the CLI's `session_ui.rs` /
`cost_ui.rs` render events) and extend them rather than adding a catch-all
arm: a wildcard would silently swallow the next event variant too.

Token counts are `u64` to match `CostRecord`'s `input_tokens` / `output_tokens`
(LESSON-446 — one currency, not two).
