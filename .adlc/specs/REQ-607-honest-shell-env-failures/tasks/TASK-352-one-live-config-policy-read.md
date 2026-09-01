---
id: TASK-352
title: "Widen the child-env provider to one live-config policy read"
status: complete
parent: REQ-607
created: 2026-08-31
updated: 2026-08-31
dependencies: [TASK-350, TASK-351]
repo: teton-code
---

## Description

ADR-C. `run_bounded` needs a second fact from the live config at the same moment
it already reads the first. Widen REQ-596's existing `OnceLock` provider rather
than adding a second global:

- `ChildEnvPolicy { credential_env_names: BTreeSet<String>, allow_ssh_agent: bool }`
- `set_credential_env_names_provider` → `set_child_env_policy_provider`
- `credential_env_names()` → `child_env_policy()`

Add `DaemonRuntime::child_env_policy()` beside `credential_env_var_names`,
reading **both** facts under **one** config lock — the argument
`boundary_posture` already makes: two readings across a concurrent `config/set`
can disagree, and one derivation is what stops that.

Rewire `main.rs`'s bootstrap closure to the new name. Keep REQ-596's rationale
prose intact and widen its noun from "the credential set" to "the policy" —
the "pulled, not pushed / live, not a snapshot" argument (LESSON-539) is
unchanged and still load-bearing.

## Files to Create/Modify

- `crates/tetond/src/child_env.rs` — `ChildEnvPolicy`, the renamed provider setter and reader, updated module docs
- `crates/tetond/src/runtime/mod.rs` — `child_env_policy()` accessor reading both facts under one lock
- `crates/tetond/src/main.rs` — bootstrap closure rewired to `set_child_env_policy_provider`
- `crates/tetond/src/harness/tools/shell.rs` — the existing test that installs the provider, updated to the new setter

## Acceptance Criteria

- [ ] `ChildEnvPolicy::default()` is `{ credential_env_names: empty, allow_ssh_agent: false }`
      — both fields safe, preserving REQ-596's uninstalled-provider argument
- [ ] `DaemonRuntime::child_env_policy()` takes the config lock **once**
- [ ] Exactly one `OnceLock` provider exists in `child_env.rs` after the change —
      no second global was added
- [ ] Every REQ-596 test that installed the old provider still passes through the
      new setter
- [ ] `cargo test --workspace --no-fail-fast` shows no new failures

## Verification

| rule | kind | artifact | benign_path |
|---|---|---|---|
| BR-7 | test-case | `crates/tetond/src/child_env.rs` — `an_uninstalled_policy_provider_withholds_the_agent_and_names_no_credentials` | yes |

## Technical Notes

This is the one task that touches REQ-596's heavily-documented prose. Per
LESSON-599 (conventions.md), a rename bounded by a word-boundary regex reaches
string literals and comments, which is the one place the compiler and the whole
suite are structurally incapable of noticing a mistake. **Diff the prose**:

```
git diff origin/main..HEAD -- crates/tetond/src/child_env.rs | grep '^[-+].*//'
```

Read every changed comment line and confirm it still says something true.

`credential_env_names_of(&Config)` is a **pure** function and does not move — it
is what the runtime's accessor calls under the lock. Only the global reader and
its setter are renamed.
