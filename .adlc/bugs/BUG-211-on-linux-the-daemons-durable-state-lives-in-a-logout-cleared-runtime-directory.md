---
id: BUG-211
title: "On Linux the daemon's durable state — cost.db, the web cache, config.toml, the model weights — lives under $XDG_RUNTIME_DIR, which is cleared at logout"
status: open
severity: medium
created: 2026-09-03
updated: 2026-09-03
component: "daemon/lifecycle"
domain: "devtools"
stack: ["rust", "daemon", "linux"]
concerns: ["reliability", "developer-experience"]
tags: ["xdg", "state-directory", "cost-ledger", "web-cache", "weights", "linux", "req-611", "adr-4"]
introduced_by: []
attribution: none
---

## Description

`teton_protocol::socket_path::resolve_base_dir` prefers `$XDG_RUNTIME_DIR` when
it is set, and `DaemonRuntime::from_env` uses that one directory for everything:

- `cost.db` — `CostLedger::open(base_dir.join("cost.db"), …)`, the ledger whose
  whole purpose is that "history survives restarts";
- the web cache — `web-cache/`, a sibling of `cost.db` under the same dir;
- the projects registry — `socket_path::projects_path(base_dir)`;
- `config.toml` — the daemon's own settings;
- the downloaded model weights — `<base_dir>/models/<name>.gguf`, a multi-
  gigabyte artifact the first run asks the user's consent to fetch.

On Linux `$XDG_RUNTIME_DIR` is `/run/user/<uid>`: a tmpfs, owned by the login
session, and removed when the user's last session ends. That is what the XDG
Base Directory specification asks of it: the directory's lifetime is bound to
the login session, and it is meant for sockets, pipes and other runtime
objects — which is what it was originally chosen for here.

So on Linux the daemon's durable state is not durable. A logout (or a reboot)
silently takes the cost history, the cached pages, the project registry, the
user's `config.toml`, and the weights they consented to download.

This is **pre-existing** and was surfaced, not caused, by REQ-611. That REQ
needed a data location for transcripts, found the runtime directory behind
`data_dir`, and added `resolve_data_dir` beside `resolve_base_dir` — used for
transcripts **only**. Moving the other four stores in the same change would
have been a silent migration of stores whose tests assume the current place, so
REQ-611's ADR-4 filed it here instead:

> Relocating `cost.db` and the web cache in the same change would be a silent
> migration of two stores whose tests assume the current place; it is filed as a
> follow-up in TASK-367 and not done here. On macOS the two resolvers agree, so
> the default install is unchanged.

The consequence REQ-611 accepted, and this bug is the other half of: Linux now
has two state directories — transcripts under `~/.local/share/teton` while
everything above stays under `$XDG_RUNTIME_DIR/teton`.

## Reproduction Steps

On a Linux machine with a systemd login session (so `$XDG_RUNTIME_DIR` is set):

1. Run a session that costs something: `teton` and one prompt to a remote
   provider. Confirm the history with `teton cost`.
2. `ls $XDG_RUNTIME_DIR/teton` — `cost.db`, `config.toml`, `web-cache/`,
   `projects.json` and `models/` are all there.
3. Log out of the desktop session entirely (not just the terminal), then log
   back in.
4. `teton cost` — the history is gone. `teton doctor` reports config from a
   directory that no longer holds the file the user edited, and the local model
   proposes its download again.

## Expected Behavior

State that is meant to outlive a session lives in a data directory:
`$XDG_DATA_HOME/teton` (default `~/.local/share/teton`) on Linux,
`~/Library/Application Support/teton` on macOS. The runtime directory holds
what it is for — the socket and the lock.

## Actual Behavior

Every store above resolves against `resolve_base_dir`, which returns
`$XDG_RUNTIME_DIR/teton` whenever that variable is set. The daemon's own docs
(`teton_docs doctor`, "Where config lives") describe the behaviour accurately,
which means the shipped answer to "where is my config?" is a directory the
system deletes.

## Environment

- Platform: Linux with a systemd-managed login session; macOS is unaffected
  (`resolve_base_dir` and `resolve_data_dir` agree there).
- Version: v0.1.28 and every version before it.

## Root Cause

(to be confirmed at fix time) One resolver serves two purposes.
`resolve_base_dir` was written for the socket path, where the runtime directory
is exactly right, and `data_dir` was then set to the same value —
`data_dir: base_dir.to_path_buf()` in `DaemonRuntime::from_env` — so every
durable store inherited a runtime location. REQ-611 added the second resolver;
what remains is moving the four stores onto it.

## Resolution

(filled after fix)

Notes for whoever takes it, so the migration is not itself the incident:

- **The move needs a migration, not a switch.** A user upgrading with a live
  `cost.db` under the runtime directory must not lose the history to the fix.
  The honest shape is: read from the new location, fall back to the old one
  when only it exists, and move the file once.
- **`config.toml` is the delicate one.** `TETON_CONFIG` overrides the resolver
  and must keep doing so; a daemon that starts reading a different file than
  the one the user has been editing is a worse bug than this one.
- **The weights are large and verified.** Moving a multi-gigabyte GGUF is a
  rename when the two paths share a filesystem and a copy when they do not,
  and the runtime directory is a tmpfs, so they never do. Re-download is not
  acceptable; a copy with the existing verification is.
- **Four stores, four tests.** Each has tests that assume the current place;
  the sweep is over `resolve_base_dir`'s callers, not over `cost.db` alone.

## Files Changed

- `crates/tetond/src/runtime/mod.rs` — `data_dir` and the `cost.db` open
- `crates/tetond/src/web/cache.rs` — the cache directory's parent
- `crates/teton-protocol/src/socket_path.rs` — which resolver each caller uses
