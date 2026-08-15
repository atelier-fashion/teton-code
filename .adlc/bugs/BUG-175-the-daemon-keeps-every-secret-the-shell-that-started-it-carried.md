---
id: BUG-175
title: "The daemon keeps every secret the environment that started it carried"
status: invalid
severity: low
created: 2026-08-15
updated: 2026-08-15
component: "tetond/startup"
domain: "credentials"
stack: ["rust", "launchd"]
concerns: ["security", "privacy"]
tags: ["br-7", "secrets", "environment", "by-design", "env-reference"]
---

## Description

Filed after `launchctl print gui/501/homebrew.mxcl.teton` showed a third-party
`MOONSHOT_API_KEY` in plaintext in the Teton daemon's environment, where it had
been resident for 3½ days. The reported defect: the daemon inherits and retains
credential-shaped variables it has no use for, defeating the keys-in-keychain
property (BR-7).

**Investigated and closed as `invalid`.** The premise is wrong in two
independent ways. Recorded rather than deleted because the "fix" is attractive,
plausible, and would break a shipped feature — see [[LESSON-531]].

## Why this is not a defect

**1. The daemon's environment is a credential store *by design*.**
`crates/tetond/src/keychain.rs:228-237` resolves a secret reference of the form
`env:VARNAME` by reading `std::env::var(var)`. That is a supported, documented
reference form alongside the keychain and is how CI and headless installs supply
provider keys. So "the daemon should hold no environment secrets" is not the
product's design — the opposite is. Scrubbing the daemon's environment at
startup, or filtering it at `spawn_daemon`, would silently break every
`env:`-referenced provider. A user's key would stop resolving with `NotFound`
and nothing would explain why.

**2. Both paths that could leak it onward are already closed.** There are
exactly two consumers of `std::env::vars()` in the whole workspace, and each
already filters:

- `crates/tetond/src/harness/tools/shell.rs` — denylist `scrub` strips
  credential-shaped names and credential-bearing URLs before the model-driven
  child starts (BR-7).
- `crates/tetond/src/mcp/client.rs` — `compose_child_env` uses a positive
  allowlist (`MCP_BASE_ENV_ALLOW`), so nothing outside the essentials reaches a
  spawned MCP server at all (REQ-544 MED-2).

Nothing logs, dumps, or serialises the environment: no diagnostic bundle, no
`teton doctor` output, no crash handler. Grep for `env::vars` returns those two
sites and nothing else.

**3. The `launchctl print` visibility is not Teton's.** The variable is set in
the launchd *user-session domain* and exported from the user's shell profile:

```
$ launchctl getenv MOONSHOT_API_KEY          → set
$ grep -l MOONSHOT_API_KEY ~/.zshrc          → ~/.zshrc
```

launchd passes domain variables to **every** service it starts. The value is
therefore visible in the environment of every launchd job on that machine, and
was placed there by machine configuration Teton does not control and cannot
undo. Teton is a bystander that displays it, not the source. A code change here
would not remove it from launchd's records.

The residual exposure — the value being present in the daemon's process image,
readable by `ps eww` as the same user who exported it in their own shell — sits
inside the trust boundary the user already established by exporting it.

## What should actually happen

This is an **operational** remediation for the machine owner, not a code change:

1. **Rotate the key.** It has been resident in a launchd domain and a running
   process for 3½ days and has appeared in a session transcript.
2. `launchctl unsetenv MOONSHOT_API_KEY` — removes it from the user-session
   domain so no launchd job inherits it.
3. Reconsider the `~/.zshrc` export. A key exported into every interactive shell
   is inherited by every process started from one. The `adlc` delegate that
   needs it supports a config file holding the variable *name*, so the value can
   live in a keychain or a sourced-on-demand file instead.

## Resolution

No code change. Closed `invalid`; the reasoning is preserved here and in
[[LESSON-531]] so that a future reader who notices a provider key sitting in the
daemon's environment does not "fix" it and break `env:` references.

## Files Changed

- none (analysis only)
