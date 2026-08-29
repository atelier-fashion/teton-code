---
id: TASK-286
title: "Move the MCP spawn path onto the shared composer, provably unchanged"
status: pending
parent: REQ-596
created: 2026-08-29
updated: 2026-08-29
dependencies: [TASK-285]
---

## Description

ADR-A. `mcp/client.rs::compose_child_env` becomes a thin wrapper that calls
`child_env::compose_child_env`, passing `MCP_BASE_ENV_ALLOW` and an **empty**
credential set.

The empty set is deliberate and is what makes AC-4.1 satisfiable: "Changing the
MCP path" is out of scope for this REQ, and BR-1 is a rule about the shell. The
MCP path already excludes provider credentials by allowlist.

## Files to Create/Modify

- `crates/tetond/src/mcp/client.rs` — delegate; `MCP_BASE_ENV_ALLOW` **unchanged**; fix the stale doc comment at `:788` that describes the shell's now-retired "denylist scrub"

## Acceptance Criteria

- [ ] AC-4.1a: a test asserts `MCP_BASE_ENV_ALLOW`'s membership against a literal twelve-name list — this REQ did not widen it (BR-7.1)
- [ ] AC-4.1b: for a **fixed** synthetic daemon environment plus a fixed `declared` map, the composed MCP environment is byte-identical to a pinned expected vector written out in the test. Sharing the composer must be provably free for the MCP path, not merely intended to be
- [ ] The existing `compose_child_env` tests in `mcp/client.rs` still pass unmodified — if one needs editing, that is a behavior change and must be justified in the PR, not quietly absorbed
- [ ] `cargo test -p tetond --no-fail-fast` green
