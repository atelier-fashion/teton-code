---
id: TASK-252
title: "Pre-flight: name the skills that will not fit"
status: draft
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

- [ ] `/doctor` names the skills exceeding the budget on the current route, with figures and bound matching the live path exactly (AC-17)
- [ ] A session with no decided route reports 'no route decided yet' — the diagnostic does not force a router resolution as a side effect (ADR-11)
- [ ] The answer is labelled a FLOOR: Body stage only, dynamic-context skills not pre-measurable
- [ ] A test asserts the pre-flight figures EQUAL the figures the live refusal produces for the same skill on the same route — one classifier, not two (LESSON-456)
- [ ] `/verbose` shows the route's budget and bound beside the count (AC-19)

## Technical Notes

`handle_skills_list` is a pure registry read today — no router, no system prompt, no budget in scope. This is new wiring to `Router::budget_for` (555) and `build_system_prompt` (turn_loop.rs:2173).
