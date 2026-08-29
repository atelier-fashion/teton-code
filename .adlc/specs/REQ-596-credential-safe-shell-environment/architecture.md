---
id: REQ-596
title: "Architecture — a credential-safe environment for the shell tool"
status: complete
created: 2026-08-29
updated: 2026-08-29
---

## Summary

One new module, `crates/tetond/src/child_env.rs`, owns **every** decision about
what a spawned child inherits from the daemon. Both existing spawn paths — the
`shell` tool's `run_bounded` and the MCP server's `StdioConnection::spawn` —
compose their child environment by calling its one function. The shell path
stops being a denylist; the MCP path keeps behaving exactly as it does today,
and a test proves it.

The credential set (BR-1) reaches the composer through a process-level provider
that reads the daemon's **live** config at the moment of the spawn, installed
once at daemon bootstrap. Nothing snapshots the config.

## What is actually there today

| Fact | Location |
|---|---|
| `run_bounded` is the single spawn body for `shell` **and** for skill dynamic-context commands | `crates/tetond/src/harness/tools/shell.rs:378`; second caller `crates/tetond/src/skills/dynamic.rs:531` |
| Shell filters with `scrub` → `is_secret_var(k,v)` = `is_secret_key(k) \|\| looks_like_credential_url(v)` | `shell.rs:537`, `:549`, `:560`, `:574` |
| MCP composes with a positive allowlist of twelve | `crates/tetond/src/mcp/client.rs:720` (`MCP_BASE_ENV_ALLOW`), `:740` (`compose_child_env`) |
| `env:<VAR>` credential refs are read with `std::env::var` | `crates/tetond/src/keychain.rs:228` |
| **Two** fields are gated by `is_recognized_auth_ref` | `teton-core/src/config.rs:1615` (`providers[].auth_ref`) and `:2197` (`web.search_key_ref`) |
| Daemon holds live config as `Mutex<Config>` | `crates/tetond/src/runtime.rs:2422` |
| Single production bootstrap, with post-construction wiring precedent | `crates/tetond/src/main.rs:115`–`:127` (`set_work_claim`) |
| Reusable source-scan test helpers | `crates/tetond/src/call_sites.rs:102` (`scan` module) |
| `scrub` / `is_secret_key` / `is_secret_var` / `looks_like_credential_url` have **no** consumer outside `shell.rs` | verified by tree-wide grep |

## ADRs

### ADR-A: One composer, and the policy is its parameters (BR-7, BR-7.1)

`child_env::compose_child_env` is the only function in the tree that builds a
child environment. It takes the policy as arguments rather than reading a
global:

```rust
pub(crate) fn compose_child_env<I>(
    daemon_vars: I,
    allow: &[&str],
    credential_env_names: &BTreeSet<String>,
    declared: &BTreeMap<String, String>,
) -> Vec<(String, String)>
```

**Why a parameter and not a widened `MCP_BASE_ENV_ALLOW`** (BR-7.1): widening
the MCP constant to serve the shell would hand every third-party `npx`/`uvx`
server the increment as a side effect of a change made for a different tool —
a regression in the path that is currently correct. The two call sites each
pass their own constant, so a future divergence is a one-line edit at a call
site rather than a fork of the composer.

The two constants have **identical membership today** (see ADR-B). BR-7.1
anticipates this and requires the parameter anyway. They stay two named
constants: `MCP_BASE_ENV_ALLOW` (unchanged, still in `mcp/client.rs`) and
`SHELL_ENV_ALLOW` (new, in `child_env.rs`).

**Composition order** — load-bearing, and each step is justified:

1. Keep only `daemon_vars` whose **name** is in `allow`. (BR-2)
2. Drop any surviving entry whose **value** satisfies
   `looks_like_credential_url`. (BR-8)
3. `apply_path_floor` over what remains. (BR-4)
4. Layer `declared` on top; a declared var overrides a base one. (existing MCP
   semantics, preserved verbatim)
5. Remove every name in `credential_env_names`, **unconditionally and last**.
   (BR-1, BR-3)

Step 5 runs after step 4, not merely after step 1. BR-3 only requires that the
allowlist cannot re-admit a credential; running last means a *declared* var
cannot re-admit one either. That is strictly stronger and costs nothing.

Step 2 applies to the base slice only, never to `declared`. A user who declares
`MY_DB=postgres://u:p@h` for their own MCP server declared it on purpose; step 2
exists to catch what the daemon's environment leaks in, not to veto a user's
explicit per-server config.

### ADR-B: The shell allowlist is the MCP twelve, and the rejections are recorded (BR-2.1)

`SHELL_ENV_ALLOW` = `PATH`, `HOME`, `TMPDIR`, `TZ`, `TERM`, `USER`, `LOGNAME`,
`SHELL`, `LANG`, `LANGUAGE`, `LC_ALL`, `LC_CTYPE`. **No additions.**

BR-2.1's criterion for an addition is "a variable an ordinary development
command needs in order to run at all, which cannot hold a credential". Nothing
cleared both halves. BR-2.1 asks for each addition's justification; recording
what was *considered and rejected* answers the reviewer's actual question —
omission or decision? — which a list of zero additions cannot:

| Considered | Rejected because |
|---|---|
| `SSH_AUTH_SOCK` | A `git push` over ssh wants it, so it passes the first half. It fails the second: it is a handle to an agent that holds keys. It cannot *hold* a credential and it does something worse — it lends them. |
| `CARGO_HOME`, `RUSTUP_HOME` | Only needed when the layout is non-default; `HOME` covers the default. "Needs in order to run at all" is not met. |
| `PWD`, `OLDPWD`, `SHLVL` | `sh` sets these itself from `current_dir`; passing them in is redundant. |
| `LC_NUMERIC`, `LC_TIME`, `LC_COLLATE`, `LC_MESSAGES` | `LANG` / `LC_ALL` / `LC_CTYPE` already cover encoding, which is the half that breaks a command outright. The rest change formatting, not whether a command runs. |
| `EDITOR`, `PAGER`, `COLUMNS`, `LINES` | An interactive convenience. Nothing in a non-interactive `sh -c` needs them. |

The Assumptions section of the requirement already accepts that withholding an
unexpected variable may break a user's command. This table is where that cost
is priced rather than discovered.

### ADR-C: The credential set is pulled from live config, never pushed (BR-1, BR-1.1, Assumptions)

`run_bounded` is a free function three layers below anything holding a `Config`,
and `ToolContext` carries no config. Two rejected options and the chosen one:

- **Rejected — thread `Config` down to `run_bounded`.** `ShellTool` is
  `Copy` and constructed as `ShellTool::default()` at its registration site;
  `ToolContext` is a jail descriptor. Plumbing config through both, plus
  `skills/dynamic.rs`, is a wide change for a narrow fact.
- **Rejected — publish the set on every config write.** `runtime.rs` has five
  distinct `let mut config = self.config.lock()` mutation sites. A push hook at
  each is precisely the "invariant with more than one enforcement point" shape
  `conventions.md` warns about, and a missed hook is a silently stale
  credential set.
- **Chosen — a pull-based provider.** `child_env` holds a
  `OnceLock<Box<dyn Fn() -> BTreeSet<String> + Send + Sync>>`. It is installed
  once in `main.rs`, capturing `Arc<DaemonRuntime>`, and calls
  `DaemonRuntime::credential_env_var_names()` — which locks the live config at
  that moment. There is no second copy to drift, and a provider added
  mid-session is visible to the very next spawn. This is the Assumptions
  section's requirement read literally (LESSON-539: read the authoritative
  state at the point of use, not a snapshot).

The precedent is in the same file: `main.rs:124` already wires
`daemon_runtime.consent().set_work_claim(...)` after construction, for the same
reason — the thing being wired needs the runtime that is being wired into.

**Degraded mode.** With no provider installed (unit tests, the CLI, any
non-daemon consumer) the set is empty. That is safe rather than fail-open: under
ADR-A step 1 a credential variable whose name is not one of the twelve is
already absent. The residual is the pathological intersection — a user who
writes `auth_ref = "env:HOME"` — and the daemon, which is the only context where
that config exists, always has the provider installed.

**Why `tetond` and not `teton-core`, and what pays for it.** The natural home for this enumeration is beside `is_recognized_auth_ref` in `teton-core/src/config.rs` — the fields and the function that classifies them would sit together. It goes in `tetond` instead for a scheduling reason (REQ-597 is editing `config.rs` concurrently), and a scheduling reason is not an architectural one, so it has to be paid for rather than waved through. Proximity was never much of a guarantee anyway: a third gated field could be added in `config.rs` without anyone updating a co-located enumeration either. TASK-288 therefore carries a derived guard — a scan of `config.rs` for `is_recognized_auth_ref(` call sites, asserted against the number of fields the enumeration reads. That is strictly stronger than co-location, and it is what makes BR-1.1's "covered without amending this rule" true rather than hoped for.

**BR-1.1 derivation.** `child_env::credential_env_names_of(&Config)` enumerates
both gated fields at one site — `providers[].auth_ref` and `web.search_key_ref`
— strips the `env:` prefix, and ignores every other scheme. It lives in `tetond`
rather than `teton-core` so this REQ does not touch `config.rs` (REQ-597 is
editing that file concurrently).

### ADR-D: OQ-1 is settled — `shell_env_withheld` is not emitted

The requirement's Events table specifies it and OQ-1 asks whether it should
exist. It should not, and the two halves of OQ-1's own doubt are the reason:
a bare count is not actionable, and BR-5 forbids the only payload that would
make it actionable (the withheld names). An event that cannot say anything
useful is a surface that can only ever leak. Under ADR-B the withheld set is
also large and boring by construction — every variable the daemon has that is
not one of twelve — so a count would be noise on every single call.

If a user-facing "why did my command lose `$FOO`" answer is wanted later, the
right shape is a documented allowlist, not a runtime event.

### ADR-E: `is_secret_key` retires; `looks_like_credential_url` survives (BR-8)

BR-8 requires an explicit verdict on each half of today's scrub, checked in
both directions (BUG-155's mirror).

- `looks_like_credential_url` **survives**, moved verbatim into `child_env.rs`
  as ADR-A step 2, with its tests. The allowlist reasons about names; this
  reasons about values, and no allowlist subsumes it.
- `is_secret_key`, `is_secret_var` and `scrub` **retire**. Under a positive
  allowlist a name-shaped denylist can only ever remove something the allowlist
  already excluded. A tree-wide grep confirms no consumer outside `shell.rs`;
  their unit tests retire with them.
- Three doc comments describe the retired rule and must be corrected, or the
  code is honest and the prose is not: `mcp/client.rs:788` ("stricter than the
  `shell` tool's denylist scrub"), `runtime.rs:4213`, `skills/dynamic.rs:479`.

### ADR-F: The egress claim gets an exception clause, not a weaker sentence (BR-6, AC-9)

`egress/mod.rs`'s header and `architecture.md`'s "a tool that reaches the
network is handed transport; it never constructs one" both overclaim: `shell`
can run `curl`. The fix names the exception explicitly rather than softening the
rule, so the guarantee that *is* real stays legible and the residual is
recorded. A test asserts the sentence is present in `architecture.md`, so the
claim cannot silently revert.

## Task graph

```
TASK-285 (child_env module)
   ├── TASK-286 (MCP call site + AC-4.1 byte-identical guard)
   ├── TASK-287 (shell call site, retire the denylist)
   │      └── TASK-288 (provider wiring + AC-1/AC-1.1 integration)
   └── TASK-290 (AC-8 source region check)

TASK-289 (BR-6 / AC-9 docs)   — independent
```

## Files

| File | Change |
|---|---|
| `crates/tetond/src/child_env.rs` | **new** — composer, `SHELL_ENV_ALLOW`, value rule, credential-name derivation + provider |
| `crates/tetond/src/lib.rs` | register the module |
| `crates/tetond/src/harness/tools/shell.rs` | `run_bounded` calls the composer; `scrub`/`is_secret_key`/`is_secret_var` deleted; header rewritten |
| `crates/tetond/src/mcp/client.rs` | `compose_child_env` delegates to `child_env`; `MCP_BASE_ENV_ALLOW` unchanged; stale doc comment fixed |
| `crates/tetond/src/runtime.rs` | one accessor: `credential_env_var_names()`; one stale doc comment |
| `crates/tetond/src/main.rs` | install the provider after `DaemonRuntime::from_env` |
| `crates/tetond/src/skills/dynamic.rs` | one stale doc comment |
| `crates/tetond/src/egress/mod.rs` | header names the `shell` exception |
| `.adlc/context/architecture.md` | the exception sentence |

## Acceptance-criteria coverage

| AC | Where |
|---|---|
| AC-1, AC-1.1 | TASK-288 |
| AC-2 | TASK-287 |
| AC-3, AC-3.1, AC-3.2 | TASK-285 |
| AC-4 | TASK-287 |
| AC-4.1 | TASK-286 |
| AC-4.2 | TASK-285 |
| AC-5 | TASK-287 (BR-1 mutation), TASK-285 (BR-8 mutation) |
| AC-6, AC-7 | TASK-287, TASK-288 |
| AC-8 | TASK-290 |
| AC-9 | TASK-289 |
