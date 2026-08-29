---
id: TASK-288
title: "Pull the credential env names from live config at spawn time"
status: pending
parent: REQ-596
created: 2026-08-29
updated: 2026-08-29
dependencies: [TASK-287]
---

## Description

ADR-C. Install the credential-name provider so BR-1 has a set to enforce.

`child_env` holds a `OnceLock<Box<dyn Fn() -> BTreeSet<String> + Send + Sync>>`.
`main.rs` installs it after `DaemonRuntime::from_env`, capturing
`Arc<DaemonRuntime>` — the same post-construction wiring shape `main.rs:124`
already uses for `set_work_claim`, and for the same reason.

Pull, not push: the closure locks the **live** config each time it is called, so
a provider added mid-session is visible to the very next spawn and there is no
second copy to go stale (Assumptions / LESSON-539).

## Files to Create/Modify

- `crates/tetond/src/child_env.rs` — the `OnceLock`, its installer, its reader, and `credential_env_names_of(&Config) -> BTreeSet<String>`
- `crates/tetond/src/runtime.rs` — `pub fn credential_env_var_names(&self) -> BTreeSet<String>`: lock config, delegate to `credential_env_names_of`
- `crates/tetond/src/main.rs` — install after `DaemonRuntime::from_env`

## Implementation notes

`credential_env_names_of` enumerates **both** fields gated by
`is_recognized_auth_ref` at one site — `providers[].auth_ref` and
`web.search_key_ref` — strips the `env:` prefix and ignores every other scheme
(BR-1.1). It lives in `tetond`, not `teton-core`, so this REQ does not edit
`config.rs` while REQ-597 is editing it.

## Acceptance Criteria

- [ ] AC-1: with `auth_ref = "env:DEEPSEEK_AUTH_SENTINEL"` configured and that var set, a `shell` run of `env` contains neither the name nor the value
- [ ] AC-1.1: the same holds for `[web] search_key_ref = "env:WEB_SEARCH_SENTINEL"` — the second gated field. A suite green on the provider field alone is not evidence BR-1 covers the set it claims
- [ ] AC-5 (BR-1 half): deleting composer step 5 makes AC-1 and AC-1.1 fail. Run the mutation, record it in the tests' doc comments with what failed and how many assertions went red
- [ ] `credential_env_names_of` ignores `keychain:` and `op://` refs, and a bare `env:` yields nothing
- [ ] BR-5: no credential value and no withheld name appears in any log line, error text, or event payload on this path
- [ ] AC-7: sentinel values only
