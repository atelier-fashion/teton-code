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

**Still unresolved (updated 2026-09-01, REQ-607).** The assumption is that twelve
names are *enough*, and only real sessions can settle that. REQ-607 did not test
it. What REQ-607 changed is the **cost of it being wrong**, on both halves the
assumption's own wording separates:

- *The misattributed error.* The signal below used to require a human to notice a
  command failing in an unfamiliar way and to guess that the daemon was
  responsible. The daemon now says so itself, on the failing call, naming Teton
  and the config key. The dogfooding signal is produced rather than inferred.
- *The capability loss.* `[shell] allow_ssh_agent` exists, so the one rejection
  this assumption predicted would bite is now escapable by a config author who has
  read what it grants.

Two things this deliberately did **not** do. It did not admit `SSH_AUTH_SOCK` by
default — REQ-596's reasoning stands and the default is unchanged. And it did not
answer OQ-2's user-extensible allowlist, which remains the other resolution path
and remains out of scope: a general `extra_env` trades a narrow known risk for a
broad unknown one.

**The resolution criterion is unchanged and still dogfooding.** Run normal agent
sessions and watch for commands failing on a missing-environment symptom. The
advisory only speaks for names in the diagnosis table — one row today — so a
session that turns up `CARGO_HOME` instead still fails silently and still needs a
human to notice. That gap is the reason this stays open rather than closing on
REQ-607's merge: a mechanism for making withheld-variable failures self-describing
is not evidence that twelve names are enough.

Close this when a period of real use has either produced no such failures, or
produced a specific name whose absence is worth its own decision.
