---
id: TASK-393
title: "Protocol — session_pinned, session_pin_lifted, and the shell/override method"
status: draft
parent: REQ-614
created: 2026-09-04
updated: 2026-09-04
dependencies: [TASK-392]
---

## Description

The wire vocabulary the pin and the lift are announced and requested through.
Two new events and one new RPC method, following the shape `web/override` and
`web_taint_overridden` already established.

## Files to Create/Modify

- `crates/teton-protocol/src/events.rs` — `SessionPinned`, `SessionPinLifted`; `PrivacyBlock.cause` distinguishes `unknown_shell` from `boundary`
- `crates/teton-protocol/src/methods.rs` — `ShellOverrideParams`, `ShellOverrideResult`, the method name

## Acceptance Criteria

- [ ] `session_pinned` carries `cause`, `liftable`, `remedy` (the exact command to type, or `none`) and `budget_tokens` — the **configured budget of the local tier**, a static fact of the tier, not a per-route derivation (the spec's System Model note)
- [ ] `session_pin_lifted` carries `session_id` and `turns_pinned`
- [ ] `BlockCause` distinguishes an unknown-shell block from a boundary block, and the existing `Boundary` reading of a `cause`-less frame is unchanged (forward compatibility)
- [ ] `ShellOverrideResult` reports whether the call was the pinned→lifted transition, so the handler can write exactly one ledger row and the client can say "already lifted" without a second row
- [ ] An older client ignoring an unknown event is not broken by either addition — pinned by whatever test the crate already uses for forward-compatible vocabulary (REQ-588)

## Verification

| rule | kind | artifact | benign_path |
|------|------|----------|-------------|
| BR-7 | test-case | `crates/teton-protocol/src/events.rs::session_pinned_carries_cause_liftable_remedy_and_budget` | no |
| AC-11 | test-case | `crates/teton-protocol/src/events.rs::the_two_new_events_round_trip` | no |

## Technical Notes

- `remedy` is a typed absent-or-command, not an empty string: "no remedy"
  and "the remedy is the empty command" must not share a representation.
- Follow the existing `WebOverrideParams` / `WebOverrideResult` shapes rather
  than inventing a second spelling for the same idea.
