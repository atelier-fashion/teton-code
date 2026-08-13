---
id: TASK-122
title: "Fail-closed malformed-provenance guard with a client-visible event"
status: complete
parent: REQ-571
created: 2026-08-13
updated: 2026-08-13
dependencies: [TASK-119]
---

## Description

Implement BR-4 (ADR-D): reject a malformed provenance source at the egress
inspection point, before boundary matching, whether or not a boundary is
configured — and report it on the protocol rather than only to daemon logs.

Protocol variant and CLI arm land together: the `Event` match in
`crates/teton/src/session_ui.rs:273-494` is exhaustive (verified — no wildcard),
so splitting them would leave the workspace un-buildable between tasks.

## Files to Create/Modify

- `crates/teton-protocol/src/events.rs` — `ProvenanceRejected` struct; `Event::ProvenanceRejected` variant; arm in `Event::name()`.
- `crates/tetond/src/egress/inspector.rs` — fail-closed well-formedness check ahead of boundary matching.
- `crates/tetond/src/egress/mod.rs` — publish the event through the existing sink.
- `crates/teton/src/session_ui.rs` — render the new variant.
- `crates/tetond/tests/provenance_rejection.rs` — new. AC-5 and AC-14.

Beyond the plan, for the reason in the as-landed notes below:

- `crates/tetond/src/egress/provenance.rs` — the `<malformed-provenance>` sentinel, the source sanitizer, and the `ProvenanceError` → wire-reason map.
- `crates/tetond/src/mcp/client.rs` — `CallProvenance`/`RejectedSource`: `collect_paths` keeps the refused assertion rather than only its effect.
- `crates/tetond/src/mcp/registry.rs` — the mint-time publication point (`with_event_sink`, `report_rejected_sources`).
- `crates/tetond/src/runtime.rs` — wire the bus into the session's registry; forward the event through `TaintingPrivacySink`.
- `crates/teton-core/{Cargo.toml,src/provenance_id.rs}`, `crates/tetond/Cargo.toml` — the `test-seam` feature and `ProvenanceId::unvalidated_for_test`.
- `crates/tetond/src/harness/tools/mcp.rs` — doc only: why the result path does *not* emit a second report.

## Acceptance Criteria

- [x] AC-5: a unit test asserts the rejection fires for an absolute source and for a `..`-bearing source, **with no boundary configured**.
- [x] AC-14: an integration test asserts `provenance_rejected` is delivered to a subscribed client, not merely logged (LESSON-505).
- [x] The guard runs BEFORE boundary matching and fails closed — a malformed source is never matched-and-passed.
- [x] The workspace builds at this commit: the `session_ui` arm is present in the same change as the enum variant.
- [x] `PROTOCOL_VERSION` is unchanged, with a note recording why the addition is wire-compatible.
- [x] The test carries a comment stating the guard is redundant by construction and why it is tested anyway (LESSON-508), so it is not deleted as noise.
- [x] AC-9 regression: the six existing egress suites still pass.

## Technical Notes

Follow `PrivacyBlock` (`teton-protocol/src/events.rs:361-384`) for the struct
shape and `emit_provider_degraded` (`crates/tetond/src/router.rs:787-793`) for
publication. Event capture in tests: subscribe to the bus before driving, then
`tokio::time::timeout` on `recv` — the idiom at `provenance_egress.rs:232-239`.

ADR-A should make this guard unreachable from first-party tools. It stays
because `ProvenanceId::claimed()` accepts third-party MCP assertions, and
because a redundant guard with no test is one refactor from being deleted.

## Implementation Notes (as landed)

### The reconciliation: mint time, not inspection time

This task was written against a `Provenance` that carried `String`. TASK-119
landed `ProvenanceId` underneath it first, and that moved the ground: a
`Provenance` is now well-formed **by construction**, so no typed path in the
daemon can hand the egress inspector a malformed source. The description above —
"reject a malformed provenance source at the egress inspection point" — describes
a refusal that no longer occurs in production.

The refusal that *does* occur is one layer earlier and was invisible:
`ProvenanceId::claimed()` returns `Err` on an absolute or `..`-bearing MCP
argument, and TASK-119 wired that to taint the whole call `Unknown` (fail-closed,
because an absolute argument may well name a boundary file). Silent until now —
the session goes local and the user is never told why.

So ADR-D's *intent* landed at both levels, at the level each is real at:

1. **The mint-time report** — the signal a user actually sees. `collect_paths`
   keeps the refused assertion beside the provenance instead of only folding it
   into the unknown bit, because once tainted it is indistinguishable from a
   `shell` result. `McpRegistry::call_tool` publishes it.
2. **The inspection guard** — `egress::inspector::first_malformed_source`, ahead
   of boundary matching, dead by construction and kept anyway (LESSON-508).

### Where the report is published, and why not at the choke point

`McpRegistry::call_tool` is the narrowest place holding **both** an event sink
and the tool name: `collect_paths` (where the refusal happens) has neither, and
`Egress::send` (where the task file put it) has the sink but sees an assembled
provenance long after it left any one tool.

The **local** server case decided it. A local stdio server's `tools/call` never
reaches egress at all, so a refusal on its arguments would surface only much
later and much vaguer — as an unknown-provenance block on whichever remote turn
happened to assemble its result. The registry is the one funnel local and remote
calls share. `harness/tools/mcp.rs` deliberately does *not* emit: it walks the
same arguments the registry already walked, so emitting there would double-report
every refusal.

`ProvenanceRejected::tool` is therefore `Option<String>`: `Some` from the mint
funnel, `None` from the inspection guard, which has no honest answer.

### The guard runs twice, on purpose

`Egress::send` calls `first_malformed_source` unconditionally, **outside** the
`!boundaries.is_empty()` fast path — a malformed source matches no glob, so
ignoring it means allowing it, and ADR-D's "whether or not a boundary is
configured" is only true if the check precedes that early-out. `inspect` calls it
too, so the pure decision function fails closed on its own and no caller can
obtain a verdict that skipped it. Removing either leaves a hole the other does not
cover; both sites say so.

The fast path was *not* widened to always call `inspect` — that would block every
unknown-provenance send on a machine with no boundaries configured, i.e. every
session that ever ran `shell`.

### The test seam

The guard cannot be tested without a value the type system says cannot exist.
`ProvenanceId::unvalidated_for_test` is that value's only constructor, behind a
`teton-core` `test-seam` feature that **only `tetond`'s dev-dependency** enables.
With resolver 2 a dev-dependency feature is not unified into `cargo build`, so
the seam is absent from every shipped binary and daemon code calling it does not
compile. Verified both directions rather than assumed: a production call site
fails `cargo build` with E0599 and compiles under `cargo test`.

### Blocked-as-what

A malformed source blocks with `path: <malformed-provenance>` (a new sentinel
beside `<unknown-provenance>`) and `cause: BlockCause::Boundary` — the same
reading the unknown-provenance refusal already takes, since `BlockCause` names
*which inspection* refused the payload and this is the provenance one. The
sentinel keeps `PrivacyBlock::path`'s "repo-relative path" contract honest and
keeps attacker-influenced text out of a field consumers read as a path; the
offending source rides on the paired `provenance_rejected`, sanitized.

### Sanitization

Control characters become `?` and the value is capped at 256 bytes **where it is
recorded**, not where it is rendered, so every downstream consumer inherits the
bound. The CLI renders it with `{:?}` on top of that, which is what stops a
hostile spelling from moving the terminal cursor or forging a second notice line.

### Verification

`cargo fmt --all --check`, `cargo clippy --all-targets` clean, and
`cargo test -p tetond -p teton-protocol -p teton -p teton-core --no-fail-fast`
green (1206 tetond lib, 152 protocol, 272 CLI, plus every integration binary).
The six AC-9 egress suites pass **unmodified** — no file among them was touched.

Mutation-checked rather than assumed: dropping the registry report reddens the
two mint-time tests and neither guard test; dropping the `send`-time guard
reddens only AC-5; dropping the `inspect` guard reddens only its unit tests. No
leg rides on another's coverage (LESSON-502).
