---
id: TASK-285
title: "The child_env module: one composer, policy as parameters"
status: pending
parent: REQ-596
created: 2026-08-29
updated: 2026-08-29
dependencies: []
---

## Description

ADR-A / ADR-B / ADR-E. Create `crates/tetond/src/child_env.rs`, the only place
in the tree that builds a spawned child's environment. Nothing is wired to it
yet — TASK-286 and TASK-287 move the two call sites.

The composer takes the policy as arguments rather than reading a global, so the
shell's allowlist and the MCP server's are two constants that can diverge with a
one-line call-site edit and can never widen each other (BR-7.1).

## Files to Create/Modify

- `crates/tetond/src/child_env.rs` — **new**
- `crates/tetond/src/lib.rs` — `mod child_env;`

## Implementation

`compose_child_env<I>(daemon_vars: I, allow: &[&str], credential_env_names: &BTreeSet<String>, declared: &BTreeMap<String, String>) -> Vec<(String, String)>`, in this order:

1. keep only names in `allow` (BR-2)
2. drop survivors whose **value** satisfies `looks_like_credential_url` (BR-8) — base slice only, never `declared`
3. `crate::env_path::apply_path_floor` (BR-4)
4. layer `declared` (a declared var overrides a base one — preserves MCP semantics verbatim)
5. remove every name in `credential_env_names`, unconditionally and **last** (BR-1, BR-3)

Also in this module:

- `SHELL_ENV_ALLOW` — the twelve names of ADR-B, with the rejection table from
  the architecture doc reproduced as a doc comment. The reviewer must be able to
  tell an omission from a decision without leaving the file.
- `looks_like_credential_url` — moved **verbatim** from `shell.rs:574`, with its
  existing unit test. It is not rewritten; BR-8 is a survival requirement.

## Acceptance Criteria

- [ ] AC-3: a var that is neither allowlisted nor a credential (`RANDOM_UNRELATED_VAR=1`) is absent from the composed result
- [ ] AC-3.1: every name in `SHELL_ENV_ALLOW` except `PATH` is present with the daemon's value, composed from a **synthetic** `daemon_vars` list (not `std::env::vars()`, which need not have all twelve set). Without this, AC-3 is satisfiable by an allowlist that admits nothing
- [ ] AC-3.2: `SHELL_ENV_ALLOW`'s membership is asserted against a **literal** list of the twelve names written out in the test — never `SHELL_ENV_ALLOW` compared to itself. Adding a name to the constant fails the test
- [ ] AC-4.2: a var on the allowlist whose value is `scheme://user:pass@host` is withheld; the **same name** holding an ordinary non-URL value is admitted. The pair is what pins this to the value rule and not to the name
- [ ] AC-5 (BR-8 half): deleting step 2 makes AC-4.2 fail. Run the mutation, record it in the test's doc comment with what failed
- [ ] BR-3 ordering is asserted directly: a name present in **both** `allow` and `credential_env_names` is absent from the result; likewise a name present in both `declared` and `credential_env_names`
- [ ] AC-7: every fixture value contains `SENTINEL`; no realistic provider-key shapes
- [ ] `cargo test -p tetond --no-fail-fast` green; `cargo clippy` clean
