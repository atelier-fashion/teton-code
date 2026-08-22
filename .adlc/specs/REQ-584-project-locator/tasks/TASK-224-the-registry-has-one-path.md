---
id: TASK-224
title: "the registry has one path, on DaemonPaths"
status: complete
parent: REQ-584
created: 2026-08-22
updated: 2026-08-22
dependencies: []
---

## Description

`DaemonPaths` gains `projects` (ADR-2), so the registry is isolated by `XDG_RUNTIME_DIR` in every test for free and no second path computation can disagree with the first.

## Files to Create/Modify

- `crates/teton-protocol/src/socket_path.rs` — the field and its doc

## Acceptance Criteria

- `daemon_paths()` puts `projects.json` in the same base as the socket, lock and log
- the existing `resolve_base_dir` precedence tests still pass unchanged
- a test asserts the registry path is under the base rather than re-deriving it
