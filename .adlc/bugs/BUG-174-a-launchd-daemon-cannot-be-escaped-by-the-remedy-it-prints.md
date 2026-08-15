---
id: BUG-174
title: "A launchd daemon cannot be escaped by the remedy it prints"
status: resolved
severity: high
created: 2026-08-15
updated: 2026-08-15
component: "teton/client"
domain: "daemon-lifecycle"
stack: ["rust", "launchd", "homebrew"]
concerns: ["upgrade", "developer-experience"]
tags: ["stale-daemon", "build-skew", "brew-services", "path", "shell-tool"]
---

## Description

Two symptoms of one root condition: **a launchd-managed daemon does not behave
like the CLI-spawned daemon the rest of the product assumes.**

1. **The build-skew remedy is unreachable.** When the CLI outruns the daemon,
   `build_skew_line` prints "Exit every teton session to stop it; the next one
   starts the new daemon." For a launchd service running a pre-0.1.14 daemon
   that sentence can never succeed — the daemon has no exit-with-last-client
   lifecycle (that shipped in 0.1.14, REQ-565), so it never exits, and
   `KeepAlive=true` means only an explicit `brew services stop` ends it. The
   user follows the printed instruction, nothing changes, and the same notice
   prints on the next run. Forever.

2. **The `shell` tool runs with a starved PATH.** The launchd plist declares no
   `EnvironmentVariables`, so the daemon's PATH is launchd's default
   `/usr/bin:/bin:/usr/sbin:/sbin`. The `shell` tool hands the daemon's own
   environment to its child, so **no Homebrew binary is reachable from inside a
   session** — `gh`, `rg`, `jq`, brew's `python3`, and `teton` itself. The agent
   gets exit 127 and reports that Teton is not installed while running inside
   Teton.

The two compound: the stale daemon is what makes the starved PATH observable,
and the starved PATH is what makes the stale daemon look like a missing install.

## Reproduction Steps

1. Install via Homebrew and register the service (`brew services start teton`).
2. Leave a daemon older than 0.1.14 running; `brew upgrade teton`.
3. Run `teton provider list` — the build-skew notice prints.
4. Exit every teton session, as the notice instructs. Run it again: the notice
   is unchanged. Repeat indefinitely.
5. From inside a `teton` session, ask the agent to run `teton provider list`
   (or any Homebrew-installed command) via the `shell` tool.

## Expected Behavior

1. The remedy names an action that actually ends *this* daemon. When the daemon
   is launchd-managed, that is `brew services stop teton` (after which the CLI
   starts one on demand per REQ-565), not "exit every session".
2. Ordinary user-installed commands resolve inside the `shell` tool regardless
   of how the daemon was started.

## Actual Behavior

1. The remedy is a no-op against a KeepAlive service running a pre-0.1.14
   daemon; the drift is permanent without out-of-band knowledge.
2. `sh: teton: command not found` (exit 127) for every brew-installed binary,
   which the model reports to the user as "Teton is not installed or not in
   PATH".

## Environment

- Platform: macOS 15.6 (darwin 25.6.0), Apple Silicon, Homebrew `/opt/homebrew`
- Version: CLI 0.1.16, running daemon 0.1.13

Observed state on the reporting machine:

```
$ launchctl print gui/501/homebrew.mxcl.teton
	default environment = { PATH => /usr/bin:/bin:/usr/sbin:/sbin }

$ ps -o pid,lstart,etime -p 53999
53999 Tue Aug 11 17:06:12 2026   03-14:03:43

$ lsof -p 53999 | awk '$4=="txt"{print $NF}'
/opt/homebrew/Cellar/teton/0.1.13/bin/teton-code    # version no longer installed

$ brew list --versions teton   → teton 0.1.16
$ readlink /opt/homebrew/opt/teton → ../Cellar/teton/0.1.16
```

The daemon has run continuously for 3½ days across many sessions, holding a
deleted 0.1.13 inode, while the `opt` symlink points at 0.1.16.

## Root Cause

**Symptom 1 — `crates/teton/src/client.rs:853` (`build_skew_line`).** The
function composes exactly one remedy sentence and appends it unconditionally.
That remedy is correct only for a daemon the CLI spawned, whose exit-with-last-
client lifecycle makes "exit every session" sufficient. It is never checked
against how the running daemon was actually started. The doc comment names the
harm REQ-565 was written for but the remedy silently assumes the non-service
install.

The trap is self-perpetuating: the code that would make the printed remedy true
(exit-with-last-client, 0.1.14+) is in the binary that cannot take over,
*because* the old daemon will not exit. Every user who registered the service
before 0.1.14 is stuck, and they are exactly the population that sees this
notice.

Note `crates/teton/src/service.rs:198` already ships
`brew_reports_service_running()` — the detector this fix needs exists and is
unused for this purpose.

**Symptom 2 — `crates/tetond/src/harness/tools/shell.rs:151-158`.** The tool
builds the child environment as `scrub(std::env::vars())` then
`env_clear().envs(scrubbed)`. It inherits whatever PATH the daemon happens to
carry. The module doc at line 13 asserts that "`PATH`, `HOME`, and the rest pass
through so ordinary commands still work" — true for a shell-spawned daemon,
false under launchd, and the assertion is what stopped anyone adding a floor.

A PATH floor is warranted independently of the lifecycle fix: any daemon started
from a thin environment (launchd, a GUI IDE, a system supervisor) has the same
defect.

**Symptom 2 has a second spawn site**, found while fixing the first.
`crates/tetond/src/mcp/client.rs` composes a stdio MCP server's environment from
a positive allowlist (`MCP_BASE_ENV_ALLOW`, REQ-544 MED-2) — and `PATH` is on
that allowlist, drawn from the daemon's own. So an MCP server declared as
`npx @scope/server` is not merely degraded under launchd, it **cannot be
launched at all**. Same root condition, same fix; fixing only the `shell` tool
would have left the identical bug one module over.

## Why the fix belongs here and not in the formula

The obvious reading — "`brew upgrade` should stop the daemon" — turns out to be
already handled on the tap side, and *cannot* be handled any better there. The
current formula (`atelier-fashion/homebrew-tap`, `Formula/teton.rb`) is correct:

- it declares **no `keep_alive`**, deliberately, with a comment explaining that
  resurrecting a crashed daemon is how the stale-binary harm came back;
- it passes `--shutdown-policy never`, so an always-on daemon is always-on *by
  explicit design* (REQ-565 BR-5);
- its `caveats` already name the exact remedy: "If you started the daemon with
  `brew services` before upgrading, run this once … `brew services stop teton`".

And it documents the hard limit at REQ-565 AC-6: **"A formula upgrade cannot
unload another formula version's launchd agent."** That is a Homebrew
constraint, not an oversight. `brew upgrade` physically cannot stop the running
service, and the plist of a registered service is not regenerated on upgrade —
which is why the reporting machine's plist still carries `KeepAlive=true` and
lacks `--shutdown-policy never`, both artifacts of a pre-REQ-565 formula.

So the only remaining lever is the **running product**. `caveats` are printed
once during an upgrade and scroll away; the build-skew notice is printed on
every single command, to a user who is by definition already stuck. It is the
one surface that reliably reaches this user, and today it tells them to do
something that cannot work. That is the defect this bug fixes.

Note the two facts compose into a permanent trap: `--shutdown-policy never`
means a launchd daemon is *designed* never to exit with its last client, while
the notice tells the user that exiting every session will stop it.

## Resolution

**The remedy now depends on how the daemon was started.** `build_skew_line`
takes a new `DaemonLifetime` (`OnDemand` | `AlwaysOnService`) and selects the
remedy from it. `AlwaysOnService` names `brew services stop teton` and says why
closing sessions will not work; `OnDemand` keeps the existing sentence verbatim.

The function stays **pure and version-injected**, which is what makes AC-7
provable without a daemon or a socket — the lifetime is passed in, never probed
inside. `report_build_skew` does the probing, and only on the rare skew path: it
asks `handshake::build_skew` whether there is anything to say *before* paying
for a `brew services info` subprocess, so the common (no-skew) attach is
unchanged. The detector itself already existed as
`service::brew_reports_service_running` and only needed `pub(crate)`.

**Both child-spawning sites now floor their `PATH`.** The floor lives in a new
daemon-wide module, `crates/tetond/src/env_path.rs`, rather than inside the
`shell` tool — precisely because it turned out not to be a `shell` concern:
`PATH_FLOOR` lists the package-manager prefixes a supervisor-started daemon
lacks, and `floored_path` appends any that exist and are not already named.

Appended, never prepended: a directory already in the inherited `PATH` keeps its
position, so a daemon started from a real login shell gets byte-identical
behaviour and the floor can never change which binary an already-working `PATH`
selects. An empty result falls back to the POSIX default rather than handing the
child no `PATH`. The existence check is injected so the behaviour is testable
off whatever the host happens to have installed.

For MCP the floor is applied to the *inherited* pairs **before** the per-server
`declared` map is layered on, so a server that declares its own `PATH` still
overrides it untouched.

The module doc's false claim — that `PATH` "pass[es] through so ordinary
commands still work" — is corrected, since that assertion is what stopped anyone
adding a floor earlier.

**Tests** (11 new, all passing; full suite 2610 passed / 0 failed):
`an_always_on_daemon_is_told_to_stop_the_service_not_close_sessions` is the
direct regression; `the_two_lifetimes_give_different_remedies` fails if the two
branches ever converge back onto one sentence, which is how this bug would
return silently; `a_declared_path_overrides_the_floor_untouched` pins the MCP
layering order.

## Files Changed

- `crates/teton/src/client.rs` — `DaemonLifetime` enum; `build_skew_line` takes
  it and branches the remedy; `report_build_skew` probes the service only when
  skew exists; 4 new tests, 5 existing call sites updated
- `crates/teton/src/service.rs` — `brew_reports_service_running` made
  `pub(crate)` so the skew path can reach the existing detector
- `crates/tetond/src/env_path.rs` — **new**: `PATH_FLOOR`, `floored_path`,
  `apply_path_floor`, and the 5 unit tests for the floor's decision
- `crates/tetond/src/lib.rs` — registers `env_path`
- `crates/tetond/src/harness/tools/shell.rs` — floors `PATH` before the child
  inherits it; module doc corrected
- `crates/tetond/src/mcp/client.rs` — floors the allowlisted `PATH` beneath the
  declared vars, so `npx`-style servers launch under launchd; 2 new tests

## Deployment

Merged to `main` as PR #157 on 2026-08-15 (squash, CI green on all 7 checks).

This repo is a plain OSS flow with no Cloud Run or iOS targets, so there is no
staging/production promotion to confirm — the fix ships to users in the next
Homebrew tap release. Until then, anyone already stuck behind the old remedy
needs the manual `brew services stop teton` this bug is about; the fix ensures
the product tells them so from that release onward.

## Follow-up captured

`[[LESSON-531]]` — "A supervisor-started daemon does not have your shell's
environment", filed with BUG-175. Its three sections are the three traps this
investigation hit: `PATH` is not a given, remedy text can be unreachable, and
"scrub the secrets" can break a feature that reads them on purpose.
