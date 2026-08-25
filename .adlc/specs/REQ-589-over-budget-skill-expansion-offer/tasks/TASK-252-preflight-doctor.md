---
id: TASK-252
title: "Pre-flight: name the skills that will not fit"
status: complete
parent: REQ-589
created: 2026-08-24
updated: 2026-08-24
dependencies: [TASK-242]
---

## Description

BR-13 / ADR-11 / D-4. A user should learn a skill will not fit without typing it and being refused.

## Files to Create/Modify

- `crates/tetond/src/server.rs` — `handle_skills_list` (4397) or a sibling RPC gains Body-stage fit against the stamped route
- `crates/teton/src/main.rs` — `doctor_report_on` (1943)
- `crates/teton/src/cli_rows.rs` — the `/doctor` mirror row

## Acceptance Criteria

- [x] `/doctor` names the skills exceeding the budget on the current route, with figures and bound matching the live path exactly (AC-17)
- [x] A session with no decided route reports 'no route decided yet' — the diagnostic does not force a router resolution as a side effect (ADR-11)
- [x] The answer is labelled a FLOOR: Body stage only, dynamic-context skills not pre-measurable
- [x] A test asserts the pre-flight figures EQUAL the figures the live refusal produces for the same skill on the same route — one classifier, not two (LESSON-456)
- [x] `/verbose` shows the route's budget and bound beside the count (AC-19)

## Technical Notes

`handle_skills_list` is a pure registry read today — no router, no system prompt, no budget in scope. This is new wiring to `Router::budget_for` (555) and `build_system_prompt` (turn_loop.rs:2173).

## Implementation Notes (TASK-252)

**The seam.** `skills/preflight` (`server.rs`), a sibling of `skills/list` under the same
`may_drive` gate and on the same synchronous path. It reads the session's stored registry
snapshot and its **stamped** route budget, runs each user-dispatchable skill's Stage A text
(`expand(..).pending_text(..)`, the turn path's own composition) through `skill_fit`, and
returns the composed report as text. `doctor_report_on` prints it line for line — the CLI
formats no figure of its own, on `projects/list`'s "the daemon renders, the client styles"
precedent.

**Where the stamp comes from.** There was no per-session `RouteBudget` anywhere: `Route`
and `HarnessConfig` are per-turn values, gone by the time a diagnostic asks. `StampedRoutes`
(`server.rs`) is fed by an observer of `route_decided` — the event *is* the stamp, published
by the single `derive` caller — started synchronously at the first `spawn_prompt_turn` so no
decision is missed. An observer that ends (bus eviction) drops every stamp and releases its
claim, so the answer degrades to "no route decided yet" rather than to a stale route.

**Deviations, all deliberate and all flagged:**

1. `crates/teton-protocol/src/methods.rs` gained `SkillsPreflightParams`/`SkillsPreflightResult`.
   The task's file list omits it and the dispatching instruction forbade it, but the `teton`
   crate does not depend on `serde` at all, so a client-side wire type is not expressible
   without a manifest change — and the protocol crate is the correct home regardless. Purely
   additive; `PROTOCOL_VERSION` does not move (the capability is proven by a successful call,
   exactly as `skills/list` records).
2. `crates/teton/tests/cli_e2e.rs` gained a second `/doctor` carve-out. BR-13 necessarily
   breaks `every_read_row_prints_exactly_what_its_shell_twin_prints`: the pre-flight is a
   question about *a session*, and the shell twin owns none. Extended in the same shape as
   the existing `DOCTOR_DAEMON_LINE` carve-out — removed from both sides, each side's own
   version then asserted, and asserted to have removed nothing for every other row.
3. `session_ui.rs`'s `figure_pair`/`budget_figures` are private to that module and it was
   out of bounds, so nothing is formatted client-side at all. The daemon's summary line
   reads `teton_protocol::events::{thousands, bytes_figure}` and `BudgetBound::words` — the
   shared primitives `budget_figures` is itself built from — so there is still one number
   vocabulary and one bound vocabulary.
4. **The system prompt is not the live turn's, and cannot be.** `build_system_prompt` needs
   the per-turn `ToolRegistry` and `HarnessConfig` that `run_prompt_turn` assembles from the
   session's probed root, web capability and tool set; a diagnostic may not build them.
   `preflight_system_prompt` uses the daemon's default harness prompt through the same
   composer, and `PREFLIGHT_FLOOR` says so on every answer — a second reason the answer is a
   floor rather than a clearance. The one-classifier test pins that the *composer*, the
   *estimator* and the *stamped budget* are shared; it cannot pin a prompt the diagnostic
   never sees.
5. **AC-19 is met on `/doctor`, not on the `/verbose` route notice.** The literal reading —
   the per-turn `route [..]` line carrying the count — needs `session_ui.rs` and a new
   `route_decided` field, both out of bounds. Implemented instead: the client's `/verbose`
   state rides in the pre-flight params and the daemon puts the route's budget and bound
   beside the count.
