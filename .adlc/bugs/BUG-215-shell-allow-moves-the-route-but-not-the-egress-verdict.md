---
id: BUG-215
title: "`/shell allow` moves the route but not the egress verdict — the prompt after a lift is routed remote, blocked again at the choke point, and served local anyway"
status: resolved
severity: medium
created: 2026-09-05
updated: 2026-09-05
component: "daemon/privacy"
domain: "devtools"
stack: ["rust", "daemon"]
concerns: ["privacy", "routing", "developer-experience"]
tags: ["taint", "shell-allow", "unknown-provenance", "egress", "req-614"]
introduced_by: ["REQ-614"]
attribution: derived
---

<!--
Attribution (2026-09-05, /bugfix Phase 2): `adlc_attr_blame_reqs` over the
lift's own lines — `RoutePin` (runtime/taint.rs) and `shell_override`
(runtime/mod.rs) — returns REQ-614 alone. Blaming the choke point
(egress/mod.rs `send`) and `context_provenance` (harness/completion.rs) also
names REQ-544, REQ-562, REQ-571, REQ-585 and REQ-612, but those lines behaved
correctly until a lift existed for them to ignore; the defect is the lift's
reach, which is REQ-614's.
-->

## Reproduction Steps

1. Configure a remote `build` tier and at least one `local-only` boundary
   (the 13 builtins suffice).
2. Prompt so the model runs an opaque command, e.g. `sh -c 'echo x'`; the
   session is pinned `unknown_shell` and the turn is served locally.
3. Type `/shell allow`; it answers lifted.
4. Prompt again.

## Expected Behavior

The prompt after the lift is routed remote **and its request leaves the
machine**; no second `privacy_block` is published.

## Actual Behavior

`route_decided` names the remote provider, the send is blocked at egress
against `<unknown-provenance>`, a second `privacy_block` is published, and
the turn is rerouted local. Every later prompt repeats this.

## Environment

- Platform: macOS, daemon 0.1.31, found under the e2e harness (mock provider,
  scripted local tier).

## Observed

Found while landing BUG-214's fix, by a probe in the `e2e::shell_pin_shape`
suite (removed before merge; the numbers below are its output).

Session shape: `build` routed to a remote mock, one `local-only` boundary,
a scripted local tier. Prompt 1: the remote model runs `sh -c 'echo opaque'`;
the result is `Unknown`, the next send is blocked, the session is pinned
`unknown_shell` and rerouted local. `shell/override` answers `lifted_now: true`
and `session_pin_lifted` is published. Prompt 2:

| fact | value |
|------|-------|
| `route_decided` to the remote provider | 2 (prompt 2 **was** routed remote — the lift held at `RoutePin`) |
| requests that reached the mock | 1 (prompt 2's never left) |
| `privacy_block` events | 2 (prompt 2 blocked again, path `<unknown-provenance>`) |
| `route_decided` to local | 3 (prompt 2 rerouted local after the second block) |

So the lift changes what `route_decided` says and nothing about where the
turn is served. Every prompt after `/shell allow` pays a remote route
decision, a blocked send, and a local reroute, and the user who typed the lift
because they *know* the command touched nothing protected is still on the
local tier.

## Root Cause

REQ-614 ADR-614-4 composed the lift into one predicate, `RoutePin::pins`, and
made the seven route sites read it. The egress choke point did not: the
prompt turn's `Egress::send` is handed
`harness::completion::context_provenance(ctx)`, which unions every block's
provenance, and the unknown-provenance `shell` result is still a block in the
carried conversation. `egress::inspector::inspect` fail-closes on
`Provenance::is_unknown` whenever any boundary is configured, and nothing on
that path consulted `ShellTaintOverride`. The route said "remote"; the
inspection said "no"; M-1 rerouted. REQ-614 BR-6 *specified* this ("a
carried `unknown` block from a lifted session that reaches a remote request
is still blocked") while BR-4 promised the opposite remedy; the spec's AC-3
"the next prompt routes remotely" was true only of the `route_decided` line.

Validated by the inventory: exactly two non-test choke points send
conversation-derived provenance — the prompt turn (`runtime/turn.rs`
`run_one_attempt`) and the harness duties (`runtime/duty.rs`
`build_duty_route`, whose `compact` sends the whole conversation and whose
`digest`/`shell` send one result). The MCP path sends argument-derived
provenance only, the web lookup never calls `send`, and `/provider test`
sends `Provenance::empty()`.

No test in the REQ exercised a prompt *after* a lift through the daemon —
TASK-397's AC-3 and AC-12 tests were named but never written (BUG-214).

## Resolution

The lift now reaches the inspection through the same predicate the route
reads, and a boundary read after a lift escalates the pin instead of being
re-blocked forever under a liftable cause.

- `Egress` gained `with_unknown_lift(Arc<dyn UnknownLift>)`; `send` inspects
  a lifted session's provenance through `Provenance::with_unknown_lifted()`,
  which clears the `unknown` bit and keeps every source and the
  `boundary_touch` sentinel. Read per request, so a lift typed mid-turn
  reaches that turn's next send.
- `RoutePin` implements `UnknownLift` as `pins` negated — one composer for
  route and inspection.
- Both session-scoped `Egress::new` sites hand it `self.route_pin()`; the
  `taint.rs` source scan holds them to it.
- `SessionTaint::mark_escalating` (sink and carry seam, whose cause comes off
  the block's path) upgrades a liftable pin to `boundary_hit`, announced
  again with no remedy; the path-less backstop arm keeps first-cause-wins
  `mark`. The first draft let `mark` itself escalate and every liftable pin
  went permanent one frame after the sink recorded it — the backstop marks
  `BoundaryHit` for every boundary block.
- REQ-614 BR-6 and ADR-614-4 amended in place, original text retained.

Tests: `egress::tests::a_lift_releases_unknown_provenance_for_that_session_only`,
`…::a_lift_does_not_release_a_boundary_source_or_a_boundary_touch`,
`provenance::tests::a_lift_releases_opacity_but_keeps_sources_and_a_boundary_touch`,
`runtime::taint::shell_pin::a_boundary_read_after_a_lift_escalates_the_pin_to_permanent`,
`…::the_egress_lift_view_is_the_route_predicate_negated`, and in
`tests/e2e/shell_pin_shape.rs` `after_shell_allow_the_next_prompt_leaves_the_machine`
(the probe that found this, now a claim) and
`a_boundary_read_after_a_lift_escalates_the_pin_and_nothing_later_leaves`.

## Files Changed

- `crates/tetond/src/egress/mod.rs` — `UnknownLift` trait, `with_unknown_lift`, lifted inspection in `send`; two tests
- `crates/tetond/src/egress/provenance.rs` — `Provenance::with_unknown_lifted`; one test
- `crates/tetond/src/runtime/taint.rs` — `mark_escalating`, `record(.., escalate)`, `impl UnknownLift for RoutePin`, sink uses the escalating mark; source scan extended; two tests
- `crates/tetond/src/runtime/turn.rs` — prompt-turn egress takes the lift view; backstop comment
- `crates/tetond/src/runtime/duty.rs` — duty egress takes the lift view
- `crates/tetond/tests/e2e/shell_pin_shape.rs` — two end-to-end claims and a shared fixture
- `.adlc/specs/REQ-614-proportionate-shell-provenance/requirement.md`, `architecture.md`, `tasks/TASK-397-*.md` — BR-6 / ADR-614-4 amended, verification rows updated

## Proposed fix (as filed)

Make the lift reach the inspection, at the one seam that already knows the
session: the `TaintView`-shaped read the lookup path uses. Options, in order
of preference:

1. **Downgrade, don't ignore.** When the session's pin is liftable and lifted,
   have the turn's provenance composition treat the unknown-provenance
   `shell` blocks as *rooted with no sources* — the verdict the user asserted
   by typing the lift — while leaving every real boundary path in force. A
   `read` of `.env` after a lift must still block; only the opacity is lifted.
   This needs the lift to be visible where `context_provenance` is composed
   (or a `Provenance::lifted_unknown()` transform applied at the send site in
   `run_one_attempt`, which already holds the session id and the runtime).
2. Alternatively, mark the blocks themselves at lift time: `shell_override`
   walks the carried conversation and rewrites `ToolProvenance::Unknown` on
   `shell` results to `Sources({})`. Simpler to reason about at egress, but it
   mutates history and a block committed after the lift by a *new* opaque
   command would need to pin again (REQ-614's "second opaque command after a
   lift" rule), which is a second question for the same walk.

Either way, the e2e claim to add back is the probe that found this: after
`shell/override`, the next prompt's request reaches the mock, no second
`privacy_block` is published, and a later `cat secrets/prod.env` still pins
permanently (the lift must not widen into `boundary_hit`).

## Workaround

None that keeps the session. Start a new session; the pinned one will not
leave the local tier however many times the pin is lifted.

## Deployment

- Merged to `main` as `34d4fde` via [PR #300](https://github.com/atelier-fashion/teton-code/pull/300) on 2026-09-05.
- Staging / production: n/a — this repo ships through PR-gated CI on `main` and the release runbook; no deploy pipeline.
