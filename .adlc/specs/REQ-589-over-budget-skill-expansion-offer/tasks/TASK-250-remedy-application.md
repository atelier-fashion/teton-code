---
id: TASK-250
title: "Apply the going-forward remedy through config/set, ordered"
status: draft
parent: REQ-589
created: 2026-08-24
updated: 2026-08-24
dependencies: [TASK-240, TASK-244]
---

## Description

BR-7 + BR-8 + BR-9 / ADR-4 + ADR-5 + ADR-12. Every remedy writes through `config/set`, inheriting its posture verbatim. The two-write `BindTierRemote` remedy is ordered so the forbidden state is unreachable.

## Files to Create/Modify

- `crates/tetond/src/runtime.rs` — remedy application calling `apply_config_update` (2760); provider choice for ADR-12
- `crates/tetond/src/server.rs` — the answer path reaching `handle_config_set` (3258)

## Acceptance Criteria

- [ ] `RaiseWindow`/`RaiseCap`/`DeclareWindow` write via `RegisterProvider` field-wise; other fields stay `None` and existing values are preserved
- [ ] `BindTierRemote` writes `max_context` FIRST and the tier binding SECOND, so a partial failure leaves a declared window on an unbound tier and never the reverse (AC-8, ADR-5)
- [ ] Exactly one configured remote provider is proposed by name; two or more are presented as a choice, never picked silently (ADR-12)
- [ ] The offer's attestation wording matches what the running build performs — no claim of a verified human on a build without `presence` (AC-20, BR-8)
- [ ] `proceed` and `apply_remedy` are honored independently across all four option ids, including remedy-only and proceed-only (AC-7b)
- [ ] After a `BindTierRemote` remedy, an identical second invocation reaches NO offer because the route now fits (AC-24) — the end-to-end proof the reported circle is closed

## Technical Notes

`config/set` persists one update per call and architecture.md:169-172 forbids generalizing it — ordering, not atomicity, is the mechanism (ADR-5). Do NOT introduce a second durable-write path via `persist_web_tier` (ADR-4 rejected it).
