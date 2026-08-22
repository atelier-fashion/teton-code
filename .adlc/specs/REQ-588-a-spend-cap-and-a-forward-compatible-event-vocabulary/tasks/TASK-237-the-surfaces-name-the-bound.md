---
id: TASK-237
title: "the surfaces name the binding ceiling"
status: complete
parent: REQ-588
created: 2026-08-22
updated: 2026-08-22
dependencies: ["TASK-236"]
---

## Description

BR-2, AC-2. `/verbose` and the refusal both name which ceiling bound, through the one composer.

## Files to Create/Modify

- `crates/teton/src/session_ui.rs` — the refusal line and the `/verbose` field
- `crates/teton-protocol/src/**` — whatever the bound rides on

## Acceptance Criteria

- **AC-2**: the binding ceiling is named on `/verbose` and in the refusal, and a test diffs both against the one composer rather than against a literal
- a turn with no ceiling in force renders **byte-identically** to today
