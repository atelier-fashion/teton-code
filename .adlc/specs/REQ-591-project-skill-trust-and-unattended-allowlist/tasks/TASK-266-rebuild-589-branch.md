---
id: TASK-266
title: "Rebuild REQ-589's branch without the trust work"
status: draft
parent: REQ-591
created: 2026-08-25
updated: 2026-08-25
dependencies: [TASK-264]
---

## Description

ADR-3 + ADR-7. `git rebase --onto origin/main` dropping exactly the five SHAs. Not `git revert` — a revert leaves the change AND its undo in history, so the branch would still contain the trust work.

## Files to Create/Modify

- `crates/tetond/src/runtime.rs` — `accept_invocation` returns to the `origin/main` sync signature (ADR-1)
- `crates/tetond/src/harness/permissions.rs` — remove the now-dead `trusted_project_roots` and `project_trust_persistence` fields (ADR-7)
- `crates/tetond/src/harness/tools/skill.rs`, `crates/tetond/src/skills/mod.rs` — trust-only helpers gone
- `crates/teton/tests/cli_e2e.rs` — the moved test returns to its origin/main form
- one comment in `607cb74`'s hunk mentioning `accept_invocation` (ADR-4)

## Acceptance Criteria

- [ ] The five SHAs are absent from `git log origin/main..HEAD`
- [ ] `accept_invocation` is `fn`, not `async fn`, and its caller does not await (ADR-1)
- [ ] NO dead trust fields remain on `PermissionGate` — a cherry-pick leaves them initialized and 'working'; remove them (ADR-7)
- [ ] `grep -rn 'trusted_project_roots\|durable_trust_root_name\|acknowledged_unattended\|read_under'` over `crates/*/src` returns NOTHING
- [ ] **`cli_e2e::a_typed_invocation_names_the_swap_and_its_flags_and_counts_no_turn_budget` passes in its ORIGINAL origin/main form.** This is the sharpest test in the REQ: it passes only if the gate that broke it is genuinely gone rather than disabled
- [ ] `cargo test --workspace --no-fail-fast` green
- [ ] The offer's own behaviour is unchanged — REQ-589's AC-1 (the reported /analyze failure) still reproduces and still offers

## Technical Notes

Rebase locally only. Do NOT push. TASK-267 owns that, and only with the owner's confirmation.
