---
id: TASK-006
title: "The per-turn skill invocation cap becomes a route property: 12 remote, 3 local"
status: complete
parent: REQ-617
created: 2026-09-04
updated: 2026-09-04
dependencies: []
---

## Description

BR-8 and AC-9. A small local model must not spend its window re-expanding the
same 16 KB skill body twelve times.

## Files to Create/Modify

- `crates/tetond/src/harness/tools/skill.rs` — `PER_TURN_INVOCATION_CAP` keeps
  its name and value as the **remote** cap; a `per_turn_invocation_cap(local:
  bool) -> usize` returns 3 on the local route. The refusal sentence renders the
  cap it actually applied.
- `crates/tetond/src/harness/turn_loop.rs` — pass the route's locality from the
  budget the router already stamped.
- `crates/tetond/tests/skill_tool_loop.rs` — AC-9 both halves.

## Acceptance Criteria

- [ ] AC-9: on the local route a fourth `skill` invocation in one turn is refused
      with `cap: 3`; on a remote route the cap stays 12 and a fourth invocation
      is admitted.
- [ ] The refusal's rendered sentence names the cap that applied, not the
      constant — a local refusal saying "12" would be a lie the model then relays.
- [ ] `crates/teton/tests/cli_e2e.rs`'s hardcoded `12` and its comment naming the
      constant stay true (the constant remains the remote value).

## Verification

| rule | kind | artifact | benign_path |
|------|------|----------|-------------|
| BR-8 | test-case | `crates/tetond/tests/skill_tool_loop.rs::the_local_route_caps_skill_invocations_at_three` | yes |
| AC-9 | test-case | `crates/tetond/tests/skill_tool_loop.rs::the_local_route_caps_skill_invocations_at_three` | yes |

The benign path is the remote half: the cap must **not** drop to 3 on a remote
route, or the rule has become a global regression wearing a route's name.
