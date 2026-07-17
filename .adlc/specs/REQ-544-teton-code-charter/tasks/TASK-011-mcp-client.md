---
id: TASK-011
title: "tetond MCP client: servers as tool providers, egress-gated"
status: complete
parent: REQ-544
created: 2026-07-17
updated: 2026-07-17
dependencies: [TASK-007, TASK-009]
---

## Description

ADR-003: user-registered MCP servers (stdio and HTTP transports) surface their
tools in sessions under the standard permission model; remote MCP traffic
flows through egress so BR-1 holds for tool calls too. Delivers AC-9.

## Files to Create/Modify

- `crates/tetond/src/mcp/client.rs` — MCP protocol client: initialize, tools/list, tools/call; stdio + streamable-HTTP transports (HTTP via egress `Transport`)
- `crates/tetond/src/mcp/registry.rs` — config-declared servers; lifecycle (spawn/connect on demand, health, restart); tool namespacing `mcp__<server>__<tool>`
- `crates/tetond/src/harness/tools/mcp.rs` — bridge MCP tools into the harness tool set; results enter context as provenance-tagged data
- `crates/tetond/tests/mcp_egress.rs` — AC-9: registered mock server's tools appear + execute under permission prompts; boundary content blocked from remote MCP server via egress capture

## Acceptance Criteria

- [x] Config-registered MCP server's tools appear in sessions and execute behind permission prompts (AC-9)
- [x] `local-only` content never reaches a remote MCP server (egress-capture test, AC-9)
- [x] Local stdio MCP servers are exempt from remote-egress rules but their RESULTS are provenance-tagged when they read boundary paths
- [x] MCP server crash/timeout degrades that server's tools only; session continues
- [x] Tool results are treated as data: injection-shaped result content is passed to the model with untrusted-content framing, never executed as harness commands

## Technical Notes

Note the asymmetry: a LOCAL MCP server reading local-only files is fine
(nothing left the machine) — but its results entering context must carry
boundary provenance so a LATER remote call can't launder them (TASK-007's
tagging). This is subtle and needs its own test. Keep MVP scope to tools
(defer MCP resources/prompts — record in spec Out of Scope at wrapup).
