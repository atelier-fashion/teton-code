---
id: ASSUME-035
title: "Shell output is held at egress only while at least one privacy boundary is in force"
status: validated
req: REQ-611
created: 2026-09-03
resolved: 2026-09-03
---

## Assumption

REQ-611 lets the transcript directory be a denied prefix for every file tool
and leaves `shell` as the named exception (REQ-596 BR-6). The spec's AC-12
shell leg assumed that a `cat` of the transcript through the shell tool is
harmless because shell output carries unknown provenance and unknown
provenance is fail-closed at egress — and that this holds on a stock install.

## Context

`shell.rs` documents that with **no** privacy boundary configured, unknown
provenance takes the egress fast path. REQ-597 made the thirteen default
boundaries active on every stock install, so the fail-closed posture now
reaches everyone unless `[privacy] disable_default_boundaries = true` is set
with no user rows. The transcript's shell leg inherits exactly that condition.

## Resolution

Validated by `crates/tetond/tests/transcript.rs::every_file_tool_refuses_the_transcript_and_shell_output_is_held_at_egress`:
with the shipped defaults in force, the shell `cat` succeeded, the next
remote-routed request raised `privacy_block`, the session was pinned local, and
no captured provider request carried the transcript's bytes. The empty-boundary
caveat is recorded in the spec's Assumptions rather than closed: sandboxing the
shell child is REQ-596's named follow-up, not this REQ's.
