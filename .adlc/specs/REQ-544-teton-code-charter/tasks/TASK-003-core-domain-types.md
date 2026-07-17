---
id: TASK-003
title: "teton-core: domain entities, config schema, policy evaluation"
status: complete
parent: REQ-544
created: 2026-07-17
updated: 2026-07-17
dependencies: [TASK-001]
---

## Description

Pure domain layer: the entities from REQ-544's System Model, the TOML config
schema, the Phase enum (D-4), and routing-policy evaluation as pure functions.
No I/O anywhere in this crate.

## Files to Create/Modify

- `crates/teton-core/src/entities.rs` — ModelProvider, RoutingPolicy, PrivacyBoundary, Session, CostRecord, TaskArtifact per spec System Model
- `crates/teton-core/src/phase.rs` — `Phase` enum {Spec, Architect, Implement, Review, Io, Freeform} (D-4)
- `crates/teton-core/src/config.rs` — TOML schema: providers (auth_ref only — never raw keys, BR-7), routing policy table, privacy boundaries, pinned local model
- `crates/teton-core/src/policy.rs` — pure fn: (Phase, RoutingPolicy, provider health) → provider id + reason string (feeds `route_decided`)
- `crates/teton-core/src/boundary.rs` — pure fn: path → Option<PrivacyBoundary> (glob matching, repo-relative)

## Acceptance Criteria

- [ ] Policy evaluation is table-driven-tested: every Phase × policy-present/absent × fallback case
- [ ] Boundary matching tested incl. nested globs, case sensitivity, and paths outside the repo (no match — never panic)
- [ ] Config round-trips TOML; a config containing a raw API key string fails validation with a message pointing to keychain refs (BR-7)
- [ ] Crate has no dependencies on tokio, reqwest, or any I/O crate

## Technical Notes

`reason` strings on policy decisions are user-facing (the "control =
legibility" promise, BR-5) — write them as sentences, test their content, not
just the chosen provider. Keep serde derives here; keychain *resolution* is
tetond's job.
