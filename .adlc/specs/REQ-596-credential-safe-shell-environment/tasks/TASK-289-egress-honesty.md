---
id: TASK-289
title: "Name the shell exception in the egress header and architecture.md"
status: pending
parent: REQ-596
created: 2026-08-29
updated: 2026-08-29
dependencies: []
---

## Description

ADR-F / BR-6. Two documented claims are false of `shell`, which can run `curl`:

- `egress/mod.rs:3` — "Every byte that leaves this machine for a remote provider
  … passes through here."
- `.adlc/context/architecture.md:88` — "A tool that reaches the network is handed
  transport; it never constructs one."

Name the exception rather than softening the rule, so the guarantee that *is*
real stays legible and the residual is recorded rather than contradicted.
Closing the network path is explicitly out of scope for this REQ.

## Files to Create/Modify

- `crates/tetond/src/egress/mod.rs` — module header
- `.adlc/context/architecture.md` — the "Egress owns every destination" pattern bullet

## Acceptance Criteria

- [ ] Both texts state that the choke point covers **provider and MCP** traffic, and that a `shell` child is outside it and can reach the network directly
- [ ] Both name REQ-596 and say the network sandbox is deliberately out of scope, so a reader knows this is a known residual and not an oversight
- [ ] AC-9: a test asserts `.adlc/context/architecture.md` contains the exception sentence, so the claim cannot silently revert to the false form
- [ ] Merge note: REQ-597 also edits `architecture.md`. Expect a rebase here and keep the edit to its own bullet to keep the conflict trivial
