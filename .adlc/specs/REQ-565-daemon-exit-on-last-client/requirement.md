---
id: REQ-565
title: "On-demand daemon lifetime: exit with the last client"
status: complete
deployable: true
created: 2026-08-09
updated: 2026-08-12
component: "daemon/lifecycle"
domain: "distribution"
stack: ["rust", "daemon", "cli", "homebrew"]
concerns: ["reliability", "developer-experience"]
tags: ["launchd", "keepalive", "autostart", "shutdown", "last-client", "brew-services"]
---

## Description

The daemon currently runs forever. The Homebrew formula's service block
(`packaging/homebrew/teton.rb.tmpl:68-77`) declares `keep_alive true`, and
`brew services` generates a launchd agent with `RunAtLoad`, so `teton-code`
starts at login, holds the local model (~17 GB on the large band) resident
around the clock, survives every CLI exit, and — because launchd resurrects
it — cannot be stopped short of `brew services stop teton`. Two concrete harms
observed 2026-08-09: a 48 GB machine paying a standing 17 GB memory tax (with
swap pressure), and a stale-binary window where the running daemon was
v0.1.12 four hours after v0.1.13 was installed, because nothing ever restarts
the service after `brew upgrade`.

Goal: invert the lifetime. The CLI autostarts the daemon on demand (this
mechanism already exists — `crates/teton/src/client.rs:600` spawns
`teton-code` when the socket is absent); the daemon counts its connected
clients and exits gracefully after the last one disconnects; the shipped
packaging stops resurrecting or boot-starting it. Accepted tradeoff, per
product decision: the next CLI start pays the model load. The design must not
preclude a later linger-timeout, but the shipped default is
exit-on-last-disconnect.

This deliberately supersedes REQ-548's service semantics (BR-6/AC-6: a
keep-alive service block, brew-services-managed lifecycle): the service block
loses `keep_alive`, `brew services` becomes the explicit always-on opt-in,
and the release smoke that proved REQ-548's claim is re-pointed at the new
lifecycle claim (AC-5 here).

## System Model

### Entities

| Entity | Field | Type | Constraints |
|--------|-------|------|-------------|
| ClientConnection | conn_id | string | required, unique per live socket connection |
| ClientConnection | connected_at | timestamp | required |
| ShutdownPolicy | mode | enum: on-last-disconnect \| linger \| never | required; default on-last-disconnect |
| ShutdownPolicy | linger_seconds | number | ≥ 0; meaningful only in linger mode |
| ShutdownPolicy | source | enum: default \| config \| env | required; for diagnostics |

### Events

| Event | Trigger | Payload |
|-------|---------|---------|
| client_connected | handshake completes | conn_id, live_connection_count |
| client_disconnected | socket closes (any reason) | conn_id, live_connection_count |
| daemon_shutdown_armed | last client disconnected under on-last-disconnect/linger policy | policy mode, pending in-flight work summary |
| daemon_shutdown_deferred | shutdown armed but in-flight work exists | blocking_activity (turn \| model_download \| model_load \| ledger_flush) |
| daemon_shutdown | daemon exits | reason (last_client \| signal), uptime_seconds, sessions_closed |

## Business Rules

- [ ] BR-1: **A connected client pins the daemon.** With ≥ 1 live client
  connection, the daemon never self-terminates. Only the last disconnect arms
  shutdown, and a count that goes back above zero disarms it.
- [ ] BR-2: **Shutdown is graceful and defers to in-flight work.** An active
  prompt turn, an in-progress model download/verify/load, and unflushed cost-
  ledger writes each defer exit (`daemon_shutdown_deferred`); the daemon exits
  only from idle. A 17 GB download is never killed mid-flight by a client
  closing its terminal. (informed by LESSON-445 — the mutual-exclusion claim
  must cover the expensive phases, not just the cheap ones)
- [ ] BR-3: **The connect-vs-shutdown race resolves cleanly.** A client
  arriving while shutdown is armed either cancels the shutdown (pre-commit) or
  is refused the handshake by the exiting daemon and reconnects to a fresh
  autostarted one. There is no window where a daemon accepts a session it will
  not serve. The ADR-007 single-instance flock is the serialization point: the
  exiting daemon holds it until after the socket is unlinked, so a racing
  autostart cannot start a second daemon early or find a stale socket.
  (informed by LESSON-445)
- [ ] BR-4: **First-turn warm-up is a notice, not an error.** After an
  on-demand start, the model-load window is surfaced to the CLI as the
  existing warming/progress state; it must never be reported as an engine
  failure. (informed by BUG-146, BUG-152)
- [ ] BR-5: **The shipped install does not resurrect the daemon.** The
  default install path has no `keep_alive` and no boot-time start; after the
  last CLI exits, no `teton-code` process remains. `brew services start
  teton` remains possible for users who explicitly want an always-on daemon —
  that path must pair with the `never` shutdown policy so launchd's
  keep-alive and the daemon's self-exit do not fight (a keep-alive service
  wrapping an exit-on-idle daemon would flap: exit, resurrect, reload 17 GB,
  repeat). (informed by LESSON-443 — the policy is an explicit setting, never
  inferred from incidental conditions like "was I started by launchd")
- [ ] BR-6: **Version skew is surfaced at handshake.** When the daemon's
  version differs from the CLI's, the CLI shows a one-line warning naming
  both versions and the remedy (exit all sessions; next start runs the new
  binary). This closes the silent stale-daemon failure mode directly.
  (informed by LESSON-456 — say the true state instead of a misleading proxy)
- [ ] BR-7: **Policy is one explicit knob.** Shutdown behavior is a single
  config value (`on-last-disconnect` default, `linger` with `linger_seconds`,
  `never`), overridable by environment for tests. Adding or changing a linger
  default later must not require protocol or packaging changes.
- [ ] BR-8: **Exit is clean.** Shutdown closes open sessions, flushes the
  cost ledger, unlinks the socket, and releases the flock — in an order that
  guarantees no half-written ledger rows and no stale socket for the next
  autostart.
- [ ] BR-9: **The lifetime logic is testable without launchd or a TTY.** The
  connection-count/arm/defer/exit decision is a pure state machine exercised
  directly by unit tests; e2e tests drive it over the real socket. No
  behavior may exist only behind launchd. (informed by LESSON-481)

## Acceptance Criteria

- [x] AC-1: e2e: start `teton` with no daemon running (autostart), exit the
  CLI → the daemon process exits cleanly within the idle grace window; no
  `teton-code` process remains; `daemon_shutdown` logged with reason
  `last_client`.
- [x] AC-2: e2e: two concurrent CLI sessions; exiting one leaves the daemon
  running (`client_disconnected` shows count 1); exiting the second stops it.
- [x] AC-3: e2e: exit the last CLI while a scripted turn is in flight → the
  daemon defers (`daemon_shutdown_deferred`, blocking_activity `turn`),
  completes the turn, then exits; the ledger row for that turn is intact.
- [x] AC-4: race test: a new client connects while shutdown is armed →
  either the shutdown is cancelled and the handshake succeeds, or the
  handshake is refused and the client's autostart path connects to a fresh
  daemon; the prompt turn succeeds either way; never two daemons (flock
  assertion).
- [ ] AC-5: packaging: the rendered formula's service block carries no
  `keep_alive` and the default install performs no boot-time start; the
  release smoke that previously asserted `brew services` health is updated to
  prove the new lifecycle claim (install → CLI round-trip → process gone)
  rather than silently weakened. (informed by LESSON-459 — a gate proves only
  what it exercises)
- [ ] AC-6: upgrade path: formula caveats (and release notes) instruct
  existing users to run `brew services stop teton` once; a doc section
  explains the old vs new lifecycle.
- [x] AC-7: handshake version-skew warning appears when CLI and daemon
  versions differ (unit-testable via injected versions) and does not appear
  when they match.
- [ ] AC-8: config: `never` mode + `brew services` keeps today's always-on
  behavior (daemon survives last disconnect); `linger` mode with
  `linger_seconds=N` exits N seconds after the last disconnect unless a
  client returns.
- [x] AC-9: unit: the lifetime state machine is covered without any socket,
  launchd, or TTY — arm, disarm, defer, exit ordering (BR-9).

## External Dependencies

- The `atelier-fashion/homebrew-tap` publish pipeline (existing, ADR-006):
  the rendered formula change rides the normal release flow; no new
  infrastructure.

## Assumptions

- No current user-facing workflow depends on a daemon outliving all clients:
  the CLI has no detach/reattach or session-resume command today (verified
  2026-08-09 in `crates/teton/src/main.rs`). ADR-002's "sessions outlive any
  client" remains the architectural direction for attach-capable clients
  (e.g. the VS Code extension); when such a client ships, it holds a
  connection while attached, so this lifecycle still serves it. If a
  disconnected-but-resumable session model ever ships, the default policy
  likely changes to `linger` — BR-7 keeps that a config change.
- Existing installs must be migrated by hand (`brew services stop teton`):
  a formula upgrade cannot reliably unload another formula version's launchd
  agent. Caveats + docs are the mechanism (AC-6).
- The ADR-007 flock is currently held for the daemon's full lifetime and can
  be extended to cover ordered teardown (BR-3/BR-8) without a protocol
  change.
- macOS/launchd only for now; Linux (systemd socket activation) follows the
  platform roadmap. (informed by LESSON-433 — no cross-platform claims from
  single-platform verification)
- The charter's "persistent daemon / always-on cheap tier" language (REQ-544)
  is read as: the local tier is available whenever a client is attached.
  On-demand lifetime preserves that for every attached client; it trades away
  only zero-client residency, on which no charter BR depends.

## Open Questions

- [x] OQ-1 — RESOLVED (product decision, 2026-08-10): idle grace is **0 s** —
  strict exit on last disconnect. `linger` mode is the documented path for
  scripting users; the default does not soften.
- [x] OQ-2 — RESOLVED (product decision, 2026-08-10): **hard-wire it** — the
  formula's service block passes the `never` policy explicitly (flag/env), so
  launchd keep-alive and daemon self-exit cannot be misconfigured into a
  flap. Documentation alone is insufficient (LESSON-443 posture).
- [ ] OQ-3: Does any background duty (cost-ledger compaction, catalog
  refresh) ever legitimately need the daemon alive with zero clients? Current
  code review says no — confirm at architecture time (not user-blocking).

## Out of Scope

- Changing the default to a linger timeout (config exists, default stays
  exit-on-last-disconnect per product decision).
- Model unload-on-idle while clients remain connected (a different memory
  lever; candidate follow-up).
- Linux/systemd and Windows service lifecycles.
- Automated migration of existing `brew services` state beyond caveats and
  documentation.
- Session persistence/resume across daemon restarts (interacts with ADR-002's
  session model; separate REQ if wanted).
- REQ-564's KV cache lifetime (dies with the daemon; accepted there).

## Retrieved Context

- LESSON-481 (lesson, score 6): A gate that hides a feature also hides its tests
- LESSON-456 (lesson, score 6): The daemon knew but the error didn't say
- LESSON-457 (lesson, score 6): Executable name is a trust surface
- BUG-146 (bug, score 6): Misleading turn failure during tier load
- LESSON-495 (lesson, score 5): A grant is only as narrow as its key
- BUG-152 (bug, score 5): Warming tier reported as a turn error
- LESSON-459 (lesson, score 5): A gate proves only what it exercises
- LESSON-441 (lesson, score 5): Fix passes introduce regressions
- LESSON-433 (lesson, score 5): Single-platform verification, false confidence
- LESSON-496 (lesson, score 4): Last in the order can mean never
- LESSON-491 (lesson, score 4): Enforce budgets at the last transform
- BUG-153 (bug, score 4): Exit is not a command
- LESSON-443 (lesson, score 4): Guard conditions that disable themselves
- LESSON-445 (lesson, score 4): Stage, then commit after authority re-check
- LESSON-497 (lesson, score 3): Plant sentinels, not lookalikes
