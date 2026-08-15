---
id: LESSON-531
title: "A supervisor-started daemon does not have your shell's environment"
component: "tetond/startup"
domain: "daemon-lifecycle"
stack: ["rust", "launchd", "homebrew"]
concerns: ["security", "developer-experience"]
tags: ["path", "environment", "launchd", "env-reference", "br-7", "remedy-text"]
req: BUG-174
created: 2026-08-15
updated: 2026-08-15
---

## What Happened

An in-session agent reported "Teton is not installed or not in PATH" while
running inside Teton. Teton was installed — `/opt/homebrew/bin/teton`, v0.1.16 —
and the command worked fine from a terminal.

The daemon serving that session had been started by launchd 3½ days earlier and
was still holding a **deleted** `Cellar/teton/0.1.13` inode. Two independent
defects fell out of that one condition (BUG-174), and a third suspicion turned
out to be a feature (BUG-175).

## Lesson

**Three distinct traps, one root idea: a daemon's environment and lifetime come
from whatever started it, and a supervisor is not a login shell.**

**1. `PATH` is not a given.** launchd's default is
`/usr/bin:/bin:/usr/sbin:/sbin` — no Homebrew prefix, no `/usr/local`. Any code
that hands a child `std::env::vars()` and assumes ordinary commands resolve is
correct only for a shell-spawned parent. Floor the `PATH` for every child the
daemon spawns, and **append, never prepend**, so a daemon started from a real
login shell keeps byte-identical resolution order.

Check *every* spawn site. Here the `shell` tool was the obvious one; the stdio
MCP client had the same defect one module over, where `PATH` rides in on a
security allowlist and so looked deliberate. Fixing one and shipping would have
left `npx`-style MCP servers unlaunchable under launchd.

**2. Remedy text is code, and it can be unreachable.** The build-skew notice
told the user to "Exit every teton session to stop it." An always-on daemon runs
under `--shutdown-policy never` and *by design* does not exit with its last
client — so that advice can never succeed. The user follows it, sees the
identical notice, and concludes the tool is broken. Worse, it was
self-perpetuating: the code that would have made the advice true shipped in a
version that could not take over precisely because the old daemon would not
exit.

If a user-facing remedy depends on how the process was started, it must branch
on how the process was started. A test that asserts the two branches differ
(`the_two_lifetimes_give_different_remedies`) is what stops them silently
collapsing back into one sentence later.

**3. Before "scrubbing secrets", check whether the secrets are load-bearing.**
Finding a third-party API key in the daemon's environment looks like an obvious
BR-7 violation. It is not: `keychain.rs` resolves `env:VARNAME` references by
reading `std::env::var`, so the daemon's environment is a *supported credential
store*. Scrubbing it at startup — or filtering at `spawn_daemon` — would have
silently broken every `env:`-referenced provider, failing with `NotFound` and no
explanation. Verify who reads a value before removing it.

## Why It Matters

- Trap 1 makes the agent report the product as uninstalled — the most
  confusing failure mode available, and it silently disables every
  Homebrew-installed tool the agent might use.
- Trap 2 traps a user permanently while printing instructions on every command.
  Documentation cannot rescue this: the formula's `caveats` already named the
  correct command, but caveats print once during an upgrade and scroll away.
  REQ-565 AC-6 records that a formula upgrade *cannot* unload another version's
  launchd agent, so the running product is the only surface that reaches the
  stuck user.
- Trap 3 is a plausible-looking security fix that breaks a shipped feature. That
  is worse than the non-problem it addresses.

## Applies When

- Writing or reviewing code that spawns a child process from a daemon, or that
  passes an inherited environment onward.
- Writing user-facing remedy text whose correctness depends on install shape,
  process lifetime, or how something was started.
- Reacting to a credential found somewhere it "shouldn't" be — establish who
  reads it before removing it.
- Diagnosing "command not found" / exit 127 from an agent tool when the same
  command works in a terminal. Check the daemon's `PATH` first
  (`launchctl print gui/$(id -u)/<label>`), not the install.
