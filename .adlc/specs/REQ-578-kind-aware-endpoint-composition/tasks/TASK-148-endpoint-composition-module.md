---
id: TASK-148
title: "teton-core endpoint composition module with unit table"
status: draft
parent: REQ-578
created: 2026-08-15
updated: 2026-08-15
dependencies: []
repo: teton-code
---

## Description

Create the pure composition module (ADR-1): canonical path constants, the
Anthropic default endpoint, and `compose_endpoint(kind, Option<&str>) ->
ComposedEndpoint` implementing the BR-2 classes, with the full unit table.

## Files to Create/Modify

- `crates/teton-core/src/endpoint_composition.rs` — new module per ADR-1:
  three constants, `ComposedEndpoint { stored, changed }`, `compose_endpoint`,
  dependency-free path classification, unit tests per (kind × class) cell
  plus trailing-slash, `/v1/`, missing-scheme, and `None` inputs.
- `crates/teton-core/src/lib.rs` — declare and export the module.

## Acceptance Criteria

- [ ] Every BR-2 class behaves per spec for both remote kinds; `Local` kind
  passes input through untouched (`changed: false`).
- [ ] `compose_endpoint(Anthropic, None)` yields the default endpoint with
  `changed: true`; `compose_endpoint(OpenaiCompatible, None)` yields `None`.
- [ ] Canonical path facts verified against vendor docs at implementation
  time (BR-2's both-halves rule; cite in doc comments — the values must
  equal what the REQ-577 catalog shipped).
- [ ] Missing-scheme or otherwise odd input is class (c) verbatim — no panic,
  no new error class (BR-6).
- [ ] `cargo test -p teton-core` green; clippy + fmt clean.

## Technical Notes

- Mirror `web_setup_catalog.rs` doc-comment discipline; the module is the
  ONE spelling of the composition rule (the protected seam test's hand copy
  is pinned against it by TASK-150's bridge).
- No `url` crate: find `"://"`, then the first `/` after the authority;
  classify the remainder against `""`, `"/"`, `"/v1"`, `"/v1/"`, the
  canonical suffix, or anything else.
