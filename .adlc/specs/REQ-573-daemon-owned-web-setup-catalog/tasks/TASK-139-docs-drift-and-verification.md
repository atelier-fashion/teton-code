---
id: TASK-139
title: "README drift comment collapse + workspace verification"
status: draft
parent: REQ-573
created: 2026-08-14
updated: 2026-08-14
dependencies: ["TASK-136", "TASK-137", "TASK-138"]
---

## Description

Collapse the three-way drift comment to name the daemon catalog as the single
in-tree source (AC-5), and run the full-workspace verification including e2e
against a freshly built daemon (ADR-E / LESSON-510).

## Files to Create/Modify

- `README.md` — rewrite the drift-check comment (~334–345): the backend rows
  are prose mirrors of `crates/tetond/src/web_setup_catalog.rs`; the contract
  suite enumerates that catalog typed; the bundled guide is CI-checked
  against it; the CLI renders it over RPC (no in-tree copy to name)
- `docs/manual-verification.md` — touch the `/web setup` section only if its
  described behavior changed (expected: no change; verify and say so)

## Acceptance Criteria

- [ ] The README comment names exactly one in-tree source of backend strings
      and states where each other surface's sync is enforced (AC-5)
- [ ] README backend rows still match the catalog strings byte-exact (manual
      diff recorded in the task completion note — they are prose, the comment
      is the enforcement pointer)
- [ ] `cargo build --workspace` then `cargo test --workspace` green — build
      first so `cli_e2e` exercises a fresh `tetond` (LESSON-510; the repo's
      known stale-daemon trap)
- [ ] `a_piped_web_setup_prints_the_instructions_and_asks_nothing` and the
      full-walk e2e pass against the fresh daemon: piped output still carries
      the SearxNG line, sourced from the RPC catalog (AC-6 end-to-end)
- [ ] `cargo clippy --workspace` introduces no new warnings

## Technical Notes

If any doc besides README names the deleted constants (`ENDPOINT_HELP` etc.),
sweep with a tree-wide grep and update (docs/manual-verification.md:1082
references web_setup_ui.rs — confirm context still reads true). Use
`--no-fail-fast` when a workspace test run reports failures, per the repo's
counting convention.
