---
id: TASK-287
title: "Compose the shell child's environment by allowlist; retire the name denylist"
status: complete
parent: REQ-596
created: 2026-08-29
updated: 2026-08-29
dependencies: [TASK-285]
---

## Description

ADR-A / ADR-E. `run_bounded` (`shell.rs:378`) stops calling `scrub` and calls
`child_env::compose_child_env(std::env::vars(), SHELL_ENV_ALLOW, &credentials, &BTreeMap::new())`,
where `credentials` comes from `child_env`'s provider (TASK-288 installs it; an
uninstalled provider yields an empty set, which is safe under the allowlist).

This covers both `run_bounded` callers — the `shell` tool and
`skills/dynamic.rs` — because `run_bounded` is the single spawn body.

`scrub`, `is_secret_var` and `is_secret_key` are **deleted** along with their
unit tests. A tree-wide grep confirmed no consumer outside `shell.rs`; re-run it
before deleting rather than trusting this sentence (BUG-155: a claim about the
fate of pre-existing code is checked, in both directions).

## Files to Create/Modify

- `crates/tetond/src/harness/tools/shell.rs` — `run_bounded`; delete `scrub`/`is_secret_var`/`is_secret_key` and their tests; rewrite the module header's "env scrub" bullet as an "env allowlist" bullet naming BR-1/BR-2/BR-8
- `crates/tetond/src/skills/dynamic.rs` — stale doc comment at `:479`
- `crates/tetond/src/runtime.rs` — stale doc comment at `:4213`

## Acceptance Criteria

- [ ] AC-2: with `MY_LLM_CRED`, `GEMINI_PW`, `LLM_AUTH` set in the composed input — names matching **no** retired denylist substring — none appears in the child's `env` output. Proves the fix is not the old substring rule in new clothing
- [ ] AC-4: `PATH` in the spawned child names the machine's package-manager prefixes (BUG-174 regression guard). The existing `PATH`-floor test at `shell.rs:894` must still pass, including its "exactly one PATH" half
- [ ] AC-6: the assertion is over **captured child output** (`run_bounded` running `env`), not over the presence of a composer call
- [ ] AC-7: sentinel values only
- [ ] The `is_secret_key` / `is_secret_var` / `scrub` symbols are gone from the tree; grep proves it
- [ ] `cargo test --workspace --no-fail-fast`, output grepped for `FAILED` (conventions.md — a summed count from a fail-fast run is a floor, not a total)
