---
id: TASK-077
title: "CLI: consent options incl. enable-permanent, /web override + refresh, status line, /cost web"
status: complete
parent: REQ-563
created: 2026-08-08
updated: 2026-08-08
dependencies: ["TASK-073", "TASK-076"]
---

## Description

The user-facing half (architecture D-5/D-7/D-8 surfaces): consent prompt
options with the new persistent-enable path, the user-only override and
cache-refresh commands, web state in the status row, event rendering, and
`/cost` lookup lines.

## Files to Create/Modify

- `crates/tetond/src/harness/permissions.rs` — add the `enable_permanent` option id to the web tool's Ask options (allow once / allow for session / enable permanently / no — spec BR-4); daemon handles `enable_permanent` by persisting the granted tier to config (REQ-547 persistence precedent) and treating it as a session grant thereafter.
- `crates/teton/src/slash.rs` — `/web allow` (session taint override → `web/override` RPC; user-only by construction) and `/web refresh <url>` (cache evict RPC); both listed in `/help` (BUG-153 discoverability rule: aliases/commands appear in help).
- `crates/tetond/src/server.rs` — `web/override` and `web/refresh` RPC handlers wired to the session override flag and `WebCache::evict`.
- `crates/teton/src/session_ui.rs` + `crates/teton/src/render.rs` — status row gains the web state field (`web: off` / `web: fetch` / `web: search` / `web: restricted (taint)` / `web: overridden`), rendered beside permission + effort levels (REQ-560 row); `web_lookup` / `web_consent_decided` / `web_taint_overridden` events render as Notice lines — taint restriction renders as a Notice naming cause and effect (BR-13's never-silent rule), NOT as `error:` (BUG-152).
- `crates/teton/src/main.rs` — verbose-gated per-lookup notice lines (host + outcome), same gate as routing notices (charter BR-5/D-5).
- `crates/tetond/src/cost/mod.rs` (or the `/cost` assembly site) — `/cost` output includes lookup count + bytes per session from the `web_lookups` table.

## Acceptance Criteria

- [x] Ask prompt for the web tool offers exactly: allow once / allow for this session / enable permanently / no; `enable_permanent` writes the tier to config and survives a daemon restart (AC-2's persistence half).
- [x] `/web allow` lifts the taint restriction for the session, renders a confirmation, emits `web_taint_overridden`; a fresh session is restricted again (AC-12 surface); `/web allow` with nothing restricted says so (no-op notice).
- [x] `/web refresh <url>` evicts the cache entry so the next lookup re-fetches (AC-10's refresh half).
- [x] Status row shows the web state including `restricted (taint)` after a taint trip; the trip itself produced a visible Notice naming cause + effect (BR-13).
- [x] `/cost` shows lookup count + bytes for a session with recorded lookups (AC-6).
- [x] Both new commands appear in `/help` output (BUG-153 rule); e2e-visible strings pinned by test.

## Technical Notes

- The override RPC arrives on the client socket — the model has no path to it
  (tool dispatch cannot invoke RPCs); assert with a test that the tool
  registry has no "override" tool and the RPC requires a client connection.
- Persistent enablement writes config through the daemon's existing config
  write path (REQ-547 flow) — never client-side file writes.
- Taint-restriction notice copy must name BOTH cause (boundary content read)
  and effect (model-composed web lookup disabled) — spec BR-13 wording.

## Implementation Notes (TASK-077)

Two premises in the file above did not hold against the branch, and both were
resolved rather than worked around:

- **There is no REQ-560 status row to extend.** REQ-560 (named permission levels
  + status line) and REQ-559 (reasoning effort) are both `status: draft` with no
  code on this branch — `grep PermissionLevel|effort crates/` is empty. The web
  field therefore ships as `SessionState::web` + `WebState::status_field()`, a
  pure function returning the five pinned strings, drawn by `paint_status` in
  `main.rs` as a row **above** REQ-556's loading indicator (the indicator stays
  last so `STATUS_ROWS_ABOVE_CURSOR` still describes the geometry). It draws
  only when the capability is engaged, so a default (BR-1, opted-out) session's
  layout is unchanged. REQ-560 composes its permission and effort fields onto
  the same row by extending `paint_status`.
- **`enable_permanent` is a fifth option id, realizing BR-4's four choices.**
  BR-4 names "allow once / allow for this session / enable permanently / no";
  the existing prompt already spells "no" as two ids (`reject_once`,
  `reject_always`), which the CLI maps to `n` and `d`. Web prompts therefore
  carry five ids and every other tool keeps four.

Other decisions worth carrying forward:

- `WebCache::evict` now returns `Result<bool, _>` so `web/refresh` can answer
  `evicted` vs `absent`. A `get`-then-remove probe would have been wrong: `get`
  answers `None` for a *stale but present* entry.
- REQ-547's precedent is `model-selection.toml` (machine state), **not** config.
  `[web] tier` persistence therefore reuses `apply_config_update`'s shape —
  clone under the lock, mutate, `validate()`, `write_config_atomically`, commit
  only on a successful write — via `DaemonRuntime::persist_web_tier`, reached
  from the gate through the `WebTierPersistence` trait so the harness has no
  other route to config.
- A persistence failure (no config file, or a candidate that would not load)
  downgrades the recorded scope to `session` rather than denying: the user said
  yes, and only the durability is missing.
