---
id: TASK-262
title: "An unattended session can run a pre-trusted repository's skills"
status: complete
parent: REQ-589
created: 2026-08-24
updated: 2026-08-24
dependencies: [TASK-248, TASK-261]
---

## Description

**Created mid-Phase-4 from product-owner decision D-13.**

D-10's trust gate on the typed path means a piped or unattended session cannot run a typed
project skill at all — no permission level clears it, `/permissions full` included, because
`LevelAllow::DoesNotSettle` sends every caller to a human who is not there. It fails
`cli_e2e::a_typed_invocation_names_the_swap_and_its_flags_and_counts_no_turn_budget`,
confirmed independently by TASK-251, TASK-252 and TASK-261.

The owner chose to **preserve automation** rather than accept the refusal.

## The shape to build — reuse `[web] permission_allow`, do not invent

There is an established precedent for exactly this problem: a durable, human-made consent
recorded in config and consulted later without re-asking. `[web] permission_allow` is written
by the `enable_permanent` option on an interactive prompt, and its label **names the key it
writes** (`[web] permission_allow += "…"`).

Mirror it:

- A durable list of acknowledged project roots in config.
- An **interactive** trust prompt gains an option to record the root permanently — labelled
  with the concrete write, per ADR-1.
- An **unattended** session consults that list. Root present → proceed. Root absent → refuse
  exactly as today.

**The invariant that must survive:** a human still decided to trust that repository. The
unattended path *consults* a decision; it never *invents* one. An unattended session that
reaches an unlisted root must still refuse — otherwise the gate is decorative and D-10 bought
nothing.

## Files to Create/Modify

- `crates/teton-protocol/src/methods.rs` — the config field
- `crates/tetond/src/harness/permissions.rs` — the gate consults the list before asking
- `crates/tetond/src/runtime.rs` — feed the list into `PermissionGate` construction
- `crates/teton/src/session_ui.rs` — the permanent option on the interactive prompt
- `crates/teton/tests/cli_e2e.rs` — the failing fixture

## Acceptance Criteria

- [x] `cli_e2e::a_typed_invocation_names_the_swap_and_its_flags_and_counts_no_turn_budget` passes
- [x] An unattended session at an **unlisted** root still refuses — a test pins it, and this is
      the criterion that keeps the gate meaningful
- [x] An unattended session at a **listed** root proceeds with no prompt
- [x] The interactive option's label names the concrete write (ADR-1's rule, `enable_permanent`'s precedent)
- [x] The durable write is verified by reading the config FILE and re-parsing it, paired with a
      refusal leg on the same fixture proving nothing was written (LESSON-519, LESSON-520)
- [x] The model-invoked path is unchanged — a test pins it
- [x] Mutating the list consultation away reddens

## Technical Notes

Check whether a durable project-trust store already exists before adding one —
`drop_project_skill_grants` suggests the existing grants are session-scoped only.

**This is a security-relevant widening the owner chose deliberately**, trading the guarantee
that every project-authored body is acknowledged in-session for the ability to automate. Say
so in the commit message and the PR; do not present it as a neutral bug fix.
