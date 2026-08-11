---
id: TASK-101
title: "CLI: render only own-session envelopes (defense in depth)"
status: complete
parent: REQ-568
created: 2026-08-11
updated: 2026-08-11
dependencies: ["TASK-097"]
---

## Description

The CLI event pump filters envelopes against its own `Context.session_id`
before calling `render_event` (spec AC-8, ADR-E) — defense in depth atop the
daemon filter, never a substitute (BR-3). Daemon-scoped envelopes
(`session_id: None`) still render.

## Files to Create/Modify

- `crates/teton/src/client.rs` — in the event pump where incoming `EventEnvelope`s are routed to rendering: skip envelopes where `env.session_id` is `Some(sid)` and `sid != ctx.session_id` (when `ctx.session_id` is `Some`). While `ctx.session_id` is still `None` (pre-create), session-scoped envelopes for unknown sessions are skipped too — the CLI has no session yet, nothing session-scoped is "its own". Keep `render_event` pure and untouched (ADR-E); the filter lives at the single pump call site.
- `crates/teton/src/session_ui.rs` — only if the pump routing actually lives here rather than client.rs (implementer confirms at the real call site); same rule either way, one call site, no second copy of the predicate (LESSON-484: one definition).
- unit test alongside the pump: three-envelope table — own-session (rendered), other-session (skipped), daemon-scoped `None` (rendered); plus the pre-create state (session-scoped skipped, `None` rendered).

## Acceptance Criteria

- [x] Other-session envelopes never reach `render_event`; own-session and daemon-scoped envelopes render exactly as before.
- [x] Pre-create (no session yet): daemon-scoped events (model download progress, lifecycle) still render — first-run consent UX unaffected.
- [x] `render_event` itself is unchanged (purity preserved).
- [x] `cargo test -p teton` passes.

## Technical Notes

- After the daemon-side filter (TASK-098) lands, other-session envelopes should never arrive anyway — this filter exists so the CLI is safe against a stale/other daemon (defense in depth). Do not weaken the daemon-side tests on the strength of this one.
- Permission-request envelopes are session-scoped and mid-turn control flow; they pass the own-session rule by construction (the CLI only prompts its own session). Do not special-case them.
