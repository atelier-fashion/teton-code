---
id: TASK-422
title: "The [agent] config table: AgentConfig, defaults, validation, is_unset"
status: complete
parent: REQ-623
created: 2026-10-05
updated: 2026-10-05
dependencies: []
repo: teton-code
---

## Description

`AgentConfig` in `teton-core` following `CostConfig`'s shape: `enabled` (true),
`max_children_per_call` (5), `max_children_per_turn` (8), `child_max_turns` (12),
`child_deadline_secs` (600), `report_max_bytes` (32768). Structural validation in
`Config::validate` (every cap ≥ 1, `report_max_bytes` ≥ 1024); `is_unset()` so a default
table is not written back to disk (ASSUME-007 round-trip).

## Files to Create/Modify

- `crates/teton-core/src/config.rs` — `AgentConfig`, `Config.agent`, validation arms, defaults, round-trip and validation tests

## Acceptance Criteria

- [x] A config with no `[agent]` table loads with the six defaults and serializes without an `[agent]` section
- [x] `max_children_per_call = 0` and `report_max_bytes = 10` are structural errors naming the key
- [x] `enabled = false` loads and is distinguishable from unset
- [x] `cargo test -p teton-core` green

## Verification

| rule | kind | artifact | benign_path |
|------|------|----------|-------------|
| BR-3 | test-case | `crates/teton-core/src/config.rs::tests::agent_caps_default_and_validate` | yes |
| BR-14 | test-case | `crates/teton-core/src/config.rs::tests::agent_enabled_false_is_not_unset` | yes |

## Technical Notes

- Copy the `CostConfig` pattern at `config.rs:258-305` and its `Config` field at `:1302` (`#[serde(default, skip_serializing_if = "…::is_unset")]`).
- Conventions "Config validity vs usability": a cap of zero is *structural* (fail-closed at load); do not add a usability pass.
