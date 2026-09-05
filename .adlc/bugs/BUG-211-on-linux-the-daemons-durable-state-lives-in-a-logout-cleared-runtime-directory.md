---
id: BUG-211
title: "On Linux the daemon's durable state — cost.db, the web cache, config.toml, the model weights — lives under $XDG_RUNTIME_DIR, which is cleared at logout"
status: resolved
severity: medium
created: 2026-09-03
updated: 2026-09-05
component: "daemon/lifecycle"
domain: "devtools"
stack: ["rust", "daemon", "linux"]
concerns: ["reliability", "developer-experience"]
tags: ["xdg", "state-directory", "cost-ledger", "web-cache", "weights", "linux", "req-611", "adr-4"]
introduced_by: ["REQ-544"]
attribution: derived
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

**Confirmed 2026-09-05.** One resolver served two purposes. `resolve_base_dir`
was written for the socket path, where the runtime directory is exactly
right, and `DaemonRuntime::from_env` took that one directory as its only
argument: `config.toml`, `cost.db`, the project registry, `model-selection.toml`,
the weights (through `build_installer` / `build_engine_loader`) and the web
cache (through the `data_dir` field, set to `base_dir.to_path_buf()`) all
inherited a runtime location. REQ-611 added `resolve_data_dir` and used it for
transcripts alone — and even there the per-turn jail and doctor re-read the
environment rather than the runtime's own directory. On the CLI side
`teton uninstall`, `teton model status` and the `service-declined` marker all
derived the state directory from the socket's parent. No migration code
existed anywhere in the tree.

Attribution: `adlc_attr_blame_reqs` over `resolve_base_dir` names REQ-544
(the charter, which chose the runtime directory for the socket and then
placed the ledger beside it); the `data_dir: base_dir` alias in `from_env`
predates the trailer convention and names nothing.

(as filed) One resolver serves two purposes.
`resolve_base_dir` was written for the socket path, where the runtime directory
is exactly right, and `data_dir` was then set to the same value —
`data_dir: base_dir.to_path_buf()` in `DaemonRuntime::from_env` — so every
durable store inherited a runtime location. REQ-611 added the second resolver;
what remains is moving the four stores onto it.

## Resolution

Two directories, one resolver each, and a one-time move between them.

- `DaemonPaths` gains `data` (= `socket_path::data_dir()`); `projects` now
  lives there. `daemon_paths()` is the one derivation the daemon and the CLI
  share.
- `DaemonRuntime::from_dirs(runtime_dir, data_dir, events)` opens every
  durable store under `data_dir` — config (unless `TETON_CONFIG`), `cost.db`,
  projects, the model decision, the installer and loader's weights directory,
  the web cache and the transcripts — and first runs
  `state_dir::migrate_durable_state(runtime_dir, data_dir)`. `from_env(base)`
  is `from_dirs(base, base)`: every in-process test and the macOS default are
  unchanged. `main.rs` calls `from_dirs` with the socket's parent and
  `paths.data`.
- `state_dir::migrate_durable_state` moves each `DURABLE_ENTRIES` item found
  under the runtime directory and absent under the data directory: `rename`,
  and on `CrossesDevices` a copy (temp name, then rename, so no truncated
  weights file ever sits under the real name) followed by removal. An entry
  present at both paths is kept at both and reported; a failed move leaves the
  source in place. The `cost.db-wal`/`-shm`/`-journal` sidecars ride with the
  database. Every outcome prints one `tetond: state — …` line naming BUG-211.
- `effective_transcript_dir` takes the runtime's `data_dir` instead of
  re-reading the environment, so the sink, the jail and doctor cannot name
  three directories.
- `from_dirs` creates the data directory (owner-only) before any store opens.
  Found by the fresh-daemon e2e claim: `CostLedger::open` on a missing parent
  falls back to an in-memory ledger **silently**, which never showed while the
  ledger lived in a directory the login session creates, and would have made
  every first run on Linux forget its cost history until the next start.
- CLI: `teton doctor` prints a `data:` line; `teton model status` derives the
  weights path from `paths.data`; the service-decline marker lives there;
  `teton uninstall` deletes and sizes the data directory and, when it is a
  different place, the runtime directory too.
- Docs: the doctor topic, README, manual-verification §0 and the release
  runbook now describe the data directory and the runtime directory as two
  things.

Tests: `state_dir` unit tests (move, keep-both, same-place, forced copy arm,
missing runtime dir), `tests/state_dir_migration.rs` (a runtime built with the
two directories apart reads the moved config and opens its ledger under the
data dir; `from_env` over one directory is unchanged), `e2e/state_dir.rs`
(through the real binary: a fresh daemon creates nothing durable beside the
socket; planted legacy state is moved once and announced, and a second start
moves nothing), `socket_path` tests for `DaemonPaths::data`, and an uninstall
test for the split directories. The e2e harness now gives every workspace one
stable `XDG_DATA_HOME`, so every existing restart test exercises the new
layout.

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

- `crates/teton-protocol/src/socket_path.rs` — `DaemonPaths::data`; `projects` under it; tests
- `crates/tetond/src/state_dir.rs` — new: `DURABLE_ENTRIES`, `migrate_durable_state`, the copy arm; tests
- `crates/tetond/src/lib.rs` — `pub mod state_dir`
- `crates/tetond/src/runtime/mod.rs` — `from_dirs`; `from_env` delegates; migration call; `config_path()` accessor; transcripts and doctor read the runtime's `data_dir`
- `crates/tetond/src/runtime/turn.rs` — `effective_transcript_dir(config, data_dir)`
- `crates/tetond/src/main.rs` — `from_dirs(&base_dir, &paths.data, …)`
- `crates/teton/src/main.rs` — doctor `data:` line; `model status` weights path from `paths.data`; test fixtures
- `crates/teton/src/service.rs` — decline marker in the data dir
- `crates/teton/src/uninstall.rs` — `Plan::runtime_dir`; both directories sized, named and deleted; tests
- `crates/tetond/tests/e2e/harness.rs` — `Workspace::data_dir` (stable per workspace), `state_dir()` under it, `runtime_state_dir()`
- `crates/tetond/tests/e2e/state_dir.rs`, `tests/e2e.rs` — two end-to-end claims
- `crates/tetond/tests/state_dir_migration.rs` — two runtime-level claims
- `crates/tetond/src/harness/docs/doctor.md`, `README.md`, `docs/manual-verification.md`, `docs/release-runbook.md` — the two directories described

## Deployment

- Merged to `main` as `36687f3` via [PR #302](https://github.com/atelier-fashion/teton-code/pull/302) on 2026-09-05.
- Staging / production: n/a — this repo ships through PR-gated CI on `main` and the release runbook; no deploy pipeline. The migration runs on each user's next daemon start after the release that carries it.
