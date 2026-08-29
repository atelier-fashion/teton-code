---
id: ASSUME-024
title: "A twelve-name environment allowlist is enough for ordinary shell commands"
status: unresolved
req: REQ-596
created: 2026-08-29
resolved:
---

## Assumption

The `shell` tool's child needs only `PATH`, `HOME`, `TMPDIR`, `TZ`, `TERM`,
`USER`, `LOGNAME`, `SHELL`, `LANG`, `LANGUAGE`, `LC_ALL` and `LC_CTYPE` from the
daemon's environment for the commands an agent actually runs — build, test,
grep, git — to work.

## Context

REQ-596 BR-2 replaced a denylist with a positive allowlist, and the requirement
explicitly priced the trade: "Withholding an unexpected variable may break a
user's shell command. BR-2 accepts that cost; the alternative silently leaks
credentials."

ADR-B took the strictest defensible reading and added **nothing** to the MCP
path's twelve. Four candidate additions were considered and rejected on the
record — `SSH_AUTH_SOCK` (lends credentials rather than holding them),
`CARGO_HOME`/`RUSTUP_HOME` (HOME covers the default layout), `PWD`/`SHLVL` (`sh`
sets them), and the remaining `LC_*` (formatting, not whether a command runs).

Each rejection is a prediction about real usage, and none of them has been tested
against a real session yet. `SSH_AUTH_SOCK` is the likeliest to bite: a `git
push` over ssh inside a shell command will now fail where it previously worked,
and the failure will look like an ssh problem rather than a Teton one.

## Resolution

Unresolved. Validate by dogfooding: run a normal agent session and watch for
shell commands that fail in ways they did not before. The signal to watch for is
a command failing with a *missing-environment* symptom — ssh agent refused, a
tool not finding its config, a locale warning turning into an error.

If `SSH_AUTH_SOCK` proves necessary, note that admitting it does not violate
BR-1 or BR-8 (it holds a socket path, not a secret) but does widen what a
model-driven command can reach, and deserves its own decision rather than being
folded in as an oversight correction. OQ-2's user-extensible allowlist is the
other resolution path.
