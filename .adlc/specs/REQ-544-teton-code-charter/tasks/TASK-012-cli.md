---
id: TASK-012
title: "teton CLI: interactive sessions, config, cost meter, first-run"
status: draft
parent: REQ-544
created: 2026-07-17
updated: 2026-07-17
dependencies: [TASK-009, TASK-010]
---

## Description

The first client surface: interactive terminal sessions rendering protocol
streams, config commands (providers, boundaries, routing policy), the cost
meter display, and the zero-config first-run flow (daemon autostart, probe,
model download with progress). Front half of AC-1/AC-2/AC-4.

## Files to Create/Modify

- `crates/teton/src/main.rs` — command tree (clap): default = interactive session; `provider add/list`, `boundary add/list`, `policy set/show`, `cost`, `doctor`
- `crates/teton/src/client.rs` — UDS connection, handshake, daemon autostart-if-absent, event subscription
- `crates/teton/src/session_ui.rs` — streaming turn rendering, tool-call display, permission prompts (y/n/always-this-session), diff preview on edits
- `crates/teton/src/firstrun.rs` — first-run: probe summary, model choice confirmation, download progress bar, benchmark result display (AC-1, AC-8 visibility)
- `crates/teton/src/cost_ui.rs` — session-end cost summary + `teton cost`: total, per-phase table, savings estimate WITH methodology line (AC-4)
- `crates/teton/src/keychain.rs` — `provider add` stores keys via macOS Security framework (BR-7); config gets the ref

## Acceptance Criteria

- [ ] Fresh machine (no daemon, no model): `teton` reaches a working session with zero manual config — autostart, probe, download, benchmark, session (AC-1)
- [ ] `teton provider add` (Anthropic + one OpenAI-compatible) stores the key in the keychain, never in a file; sessions route to them (AC-2, BR-7)
- [ ] Session end prints cost summary; `teton cost` shows per-phase attribution and labeled savings estimate (AC-4)
- [ ] Permission prompts render and round-trip; "always" grants are session-scoped
- [ ] `route_decided`, `privacy_block`, `provider_degraded` events render as visible one-line notices (control = legibility)
- [ ] Tests: rendering unit-tested via the rendering trait against scripted event streams (session, permission round-trip, cost summary, first-run progress); command tree covered by `clap` parse tests; keychain module behind a trait with a mock for CI (no real keychain in tests)

## Technical Notes

Keep the UI plain streaming text for MVP — no TUI framework yet (a ratatui
upgrade is post-MVP; don't paint into that corner though: isolate rendering
behind a small trait). `doctor` prints daemon status, socket path, model
state, provider reachability — build it early, it pays for itself in support.
