---
id: TASK-286
title: "The wire half: boundary origin on config/get, and the two session-start events"
status: pending
parent: REQ-597
repo: teton-code
created: 2026-08-29
updated: 2026-08-29
dependencies: []
---

## Description

`teton-protocol` learns the two facts the daemon needs to report: which rows are builtin, and
that a session started with either an unbounded root or an applied default set. Covers the
wire half of BR-5 and BR-6, and AC-9.1.

Independent of TASK-285 — a different crate, and the protocol enum mirrors the core enum by
name rather than importing it, which is the repo's existing no-drift-across-the-wire rule.

## Files to Create/Modify

- `crates/teton-protocol/src/methods.rs` — `BoundaryOriginConfig { Builtin, User }`
  (`#[serde(rename_all = "kebab-case")]`, `#[default] User`); `origin` on
  `PrivacyBoundaryConfig` with `#[serde(default)]`. Update the 5 in-file literals.
- `crates/teton-protocol/src/events.rs` — `UnboundedRootWarning { root_kind: RootKind }` and
  `BoundaryDefaultsApplied { count: usize }` payload structs, their `Event` variants, and
  their arms in `Event::name()`.

## Acceptance Criteria

- [ ] AC-9.1 (round trip): a `ConfigSnapshot` carrying rows of both origins serializes and
      deserializes with each row's origin preserved.
- [ ] AC-9.1 (older daemon): a `PrivacyBoundaryConfig` JSON object with **no** `origin` key
      deserializes successfully as `User`. Assert the value, not merely that it parsed.
- [ ] `Event::name()` returns `"unbounded_root_warning"` and `"boundary_defaults_applied"`,
      matching the spec's Events table spellings; both round-trip through the tagged
      representation.
- [ ] Neither new event sets `ENDS_TURN`.
- [ ] `cargo test -p teton-protocol --no-fail-fast` is green.

## Technical Notes

Mirror `BoundaryMode`'s spelling exactly (`kebab-case`, so `local-only` / `redact-then-remote`
have siblings in `builtin` / `user`) — the wire and core enums must not drift, and the repo
tests that property elsewhere.

`#[serde(default)]` on the wire `origin` is AC-9.1's whole point: a newer CLI talking to an
older daemon must read a snapshot that omits the field, and must read it as `User` — the
conservative reading, since an older daemon has no builtins to report.

Do **not** add `skip_serializing_if` on the wire copy. Unlike the on-disk entity (TASK-285),
the snapshot is a report, and a report that omits "user" makes the two origins asymmetric on
a surface whose entire job is to distinguish them.
