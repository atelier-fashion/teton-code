---
id: BUG-183
title: "AC-19's cost-attribution tests never touch the skill path, and their central assertion is implied by their own setup"
status: resolved
severity: medium
created: 2026-08-20
updated: 2026-08-22
component: "daemon/cost-ledger"
domain: "cost"
stack: ["rust", "daemon"]
concerns: ["developer-experience", "reliability"]
tags: ["testing", "vacuous-test", "cost-attribution", "req-585", "lesson-485", "lesson-520", "lesson-544"]
---

## Description

`crates/tetond/tests/cost_attribution.rs`'s two REQ-585 tests —
`a_skill_turns_forward_is_billed_exactly_as_a_typed_prompts_is` and
`a_skill_turn_pinned_by_its_own_file_is_billed_nothing` — are the only AC in
this REQ whose test would pass with the feature deleted. Neither calls
`run_prompt_turn`, `expand`, `accept_invocation`, or anything under
`crate::skills`. Both build an `Egress::send` by hand.

Three problems compound:

1. **The fixture is a shape production never emits.** `SKILL_EXPANSION` opens
   `Running the skill \`status\` (user, ~/.claude/skills/status/SKILL.md).` The
   real preamble is `The user invoked /status (a command defined in …); the
   instructions below are that command's body.` The envelope also drops the
   `tool="skill:status"` attribute the real one carries. That is LESSON-485's
   shape, and LESSON-544's for a hand-built value.
2. **The central assertion is implied by its own setup.** `skilled.session_id
   == typed.session_id`, `.phase`, `.provider_id`, `.model` cannot differ,
   because one `EgressContext` is reused for both sends. The assertion holds
   whatever the skill path does.
3. The pinned-turn leg is likewise a statement about `Egress` plus a boundary,
   with nothing skill-shaped in the causal path.

The file's own header argues `cli_e2e` cannot make this claim because its
scripted tier is local and local turns produce no billed row. That is true and
the reasoning is sound — but `crates/tetond/tests/skill_turn.rs`'s `Harness`
already drives real invocations against a remote `Vendor` mock (`h.vendor
.hits()`), which is exactly the instrument this claim needs.

## Impact

AC-19's attribution half is unverified. A regression that stopped attributing
a skill turn — or attributed it to the wrong session, phase or provider —
would ship green.

## Reproduction

Delete the whole `crates/tetond/src/skills/` module tree and stub
`run_prompt_turn`'s skill path; both tests still pass.

## Suggested fix

Move the positive leg into `skill_turn.rs`: run
`h.turn(&session, "", Harness::invoke("status", ""))` on a remote route with
the cost meter wired, and assert a ledger row exists carrying the same
attribution a typed turn on the same session produced. Keep the leak
assertions — they are meaningful — but drive them off the real expansion, or
reuse `provenance_egress.rs`'s `ran_expansion(...)` helper so the body and the
provenance both come from production.

## Found

REQ-585 Phase 5 verify (architecture review), 2026-08-20.

## Resolution — 2026-08-22

Closed by moving AC-19's attribution half into `skill_turn.rs`, which drives real invocations against a remote `Vendor` mock. It runs a typed turn and a skill turn on one session, reads `/cost` via `DaemonRuntime::cost_report`, and asserts the skill turn joins the typed turn's phase group. Non-vacuous by construction: an unregistered name fails the turn outright.
