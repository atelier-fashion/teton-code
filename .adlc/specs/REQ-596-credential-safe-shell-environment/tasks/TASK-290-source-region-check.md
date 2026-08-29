---
id: TASK-290
title: "AC-8: a region check that the child env has one construction site"
status: pending
parent: REQ-596
created: 2026-08-29
updated: 2026-08-29
dependencies: [TASK-285, TASK-287]
---

## Description

AC-8. Assert over the daemon's own source that a spawned child's environment is
constructed **only** by the shared composer. A second construction site must
fail the check.

This is a **region check, not a count** (conventions.md / LESSON-568): relocating
a required call keeps a count identical, so counting `.envs(` proves nothing.
Reuse `crate::call_sites::scan` (`daemon_src`, `production_sources`,
`code_only`) rather than writing a second "production source only" rule — two
spellings of that rule drift, and a drifted one is a scan that stops seeing a
file.

## Files to Create/Modify

- `crates/tetond/src/child_env.rs` — the test (it belongs with the invariant it guards)

## Implementation

For every production source under `crates/tetond/src`, find each `.envs(`
occurrence. For each, assert the enclosing region binds its argument from a
`compose_child_env` call. Equivalently and more simply: assert the set of files
containing `.envs(` is exactly `{shell.rs, mcp/client.rs}` **and** that in each,
the identifier passed to `.envs(` is bound by a `child_env::compose_child_env`
(or the MCP wrapper delegating to it) within the preceding region of the same
function.

## Acceptance Criteria

- [ ] The check fails when a second `Command::new(...).envs(...)` is added with a hand-built vector — **demonstrate this**, do not assert it. Add the violating site, watch it go red, remove it, and record the mutation and the failure text in the test's doc comment
- [ ] The check fails when the composer call is deleted but `.envs(` stays
- [ ] The scan asserts a non-zero file floor, so a walk that silently found nothing cannot pass vacuously
- [ ] `cargo test -p tetond --no-fail-fast` green
