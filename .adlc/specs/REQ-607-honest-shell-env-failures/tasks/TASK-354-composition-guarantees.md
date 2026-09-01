---
id: TASK-354
title: "Pin what the opt-in admits, and what it must not reach"
status: complete
parent: REQ-607
created: 2026-08-31
updated: 2026-08-31
dependencies: [TASK-351, TASK-352]
repo: teton-code
---

## Description

The composition guarantees: AC-7, AC-8, AC-9, AC-10, AC-11. This task adds no
production code — it is the evidence that ADR-B's shape actually holds.

AC-8 is the one with teeth and the one most easily faked. It is a **set
difference in both directions over the whole composed map**, not a spot check of
names the test happens to think of. A widened allowlist — the exact regression
`allow_ssh_agent` could become — passes any spot check and fails this.

## Files to Create/Modify

- `crates/tetond/src/child_env.rs` — AC-7, AC-8, AC-10, AC-11 unit tests
- `crates/tetond/src/mcp/client.rs` — AC-9 test, beside REQ-596's existing AC-4.1 test
- `crates/teton-core/src/config.rs` — the BR-6 shape check, if it is cheaper to assert here than in `tetond`

## Acceptance Criteria

- [ ] AC-7: with `allow_ssh_agent = true`, `SSH_AUTH_SOCK` is present in the
      composed child environment; with it `false` or absent, it is not.
      Asserted by **inspecting the composed map**, not by observing a command
      succeed
- [ ] AC-8: the composed map with the flag on differs from the map with it off by
      **exactly one entry**, `SSH_AUTH_SOCK`. Asserted as a set difference in
      **both** directions over the whole map — nothing gained beyond that name,
      nothing lost
- [ ] AC-8 is non-vacuous: the fixture plants `SSH_AUTH_SOCK` in the daemon vars,
      and the test asserts the composed map is non-empty. With the variable unset
      the true difference is *zero* entries and the assertion would be measuring
      nothing
- [ ] AC-9: with `allow_ssh_agent = true`, a spawned MCP server's composed
      environment is byte-identical to its value with the flag off
- [ ] AC-10: with `allow_ssh_agent = true`, a variable named by a configured
      `auth_ref = "env:<NAME>"` is still absent
- [ ] AC-10 covers the pathological overlap: `auth_ref = "env:SSH_AUTH_SOCK"`
      with the flag **on** still yields an absent `SSH_AUTH_SOCK` — REQ-596 BR-3's
      unconditional removal runs last and the new flag cannot reach around it
- [ ] AC-11: every planted fixture value contains `SENTINEL`
- [ ] `cargo test --workspace --no-fail-fast` passes

## Verification

| rule | kind | artifact | benign_path |
|---|---|---|---|
| BR-5 | test-case | `crates/tetond/src/child_env.rs` — `the_opt_in_admits_exactly_one_more_entry_in_both_directions` | no |
| BR-6 | structural-check | `crates/teton-core/src/config.rs` — `shell_config_carries_one_boolean_key` | no |
| BR-7 | test-case | `crates/tetond/src/mcp/client.rs` — `the_shell_opt_in_does_not_reach_a_spawned_mcp_server` | yes |
| BR-8 | test-case | `crates/tetond/src/child_env.rs` — `with_the_flag_off_the_agent_socket_is_absent` | yes |
| AC-7 | test-case | `crates/tetond/src/child_env.rs` — `with_the_flag_off_the_agent_socket_is_absent` | yes |
| AC-8 | test-case | `crates/tetond/src/child_env.rs` — `the_opt_in_admits_exactly_one_more_entry_in_both_directions` | no |
| AC-9 | test-case | `crates/tetond/src/mcp/client.rs` — `the_shell_opt_in_does_not_reach_a_spawned_mcp_server` | yes |
| AC-10 | test-case | `crates/tetond/src/child_env.rs` — `a_configured_credential_is_absent_even_with_the_agent_admitted` | no |
| AC-11 | structural-check | `crates/tetond/src/child_env.rs` — fixture values in this module's tests | no |

## Technical Notes

**AC-8's oracle must not be the subject** (conventions.md, LESSON-569). Build the
two composed maps by calling `compose_child_env` with the two allowlists and one
fixed `daemon_vars` fixture, then diff the two `BTreeMap`s. Do not compute the
expected difference by calling `shell_env_allow` and reasoning about it — that
would be the subject computing its own expected value.

**AC-9's assertion is about a path that does not exist**, which is the point. The
MCP composer never reads the policy global, so both compositions are identical by
construction; the test fails only if someone wires the flag into that path. Say
so in the doc comment, so a later reader does not mistake it for a tautology and
delete it.

**AC-10's second leg is the sharp one.** `auth_ref = "env:SSH_AUTH_SOCK"` with the
flag on puts the allowlist and the credential-removal step in direct conflict.
Composer step 5 runs last and unconditionally, so removal wins. This is REQ-596
BR-3 holding against a capability that did not exist when it was written.
