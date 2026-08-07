---
id: TASK-062
title: "The title duty — name a session once, and announce it on the wire"
status: draft
parent: REQ-561
created: 2026-08-07
updated: 2026-08-07
dependencies: [TASK-059, TASK-061]
---

## Description

Wire `Category::Title` (reflex tier — therefore always local; REQ-558 established
that `reflex` never inherits `default_provider`). The duty names a session once,
populates the existing `SessionSummary.title`, and emits `session_titled`.

Depends on TASK-059 for the event type.

## Files to Create/Modify

- `crates/tetond/src/harness/title.rs` — **new**. Prompt builder, `TITLE_OUTPUT_CONTRACT`, `TITLE_OUTPUT_MAX_BYTES`.
- `crates/tetond/src/harness/mod.rs` — declare the `title` module.
- `crates/tetond/src/runtime.rs` — add `title_route()` spelling `router.resolve(Category::Title)` literally; invoke the duty once per session when the title is absent; publish `Event::SessionTitled` on success.
- `crates/tetond/src/sessions.rs` — populate the session's title; the once-only guard lives here, keyed on the title being `None`.
- `crates/tetond/src/call_sites.rs` — flip `Category::Title` to `true`.

## Acceptance Criteria

- [ ] **AC-6**: `title` is requested exactly once across a multi-turn session, asserted by **call count**, not by inspecting the stored value. A session that already has a title requests zero times.
- [ ] **AC-15**: exactly one `session_titled` with a non-empty title reaches the wire per session; a session that already has one emits none. Asserted on **captured events**, not daemon-internal state.
- [ ] An existing title is never overwritten (BR-9). The guard is keyed on `title.is_none()`, so a re-derivation cannot occur even if the duty is invoked again.
- [ ] `router.resolve(Category::Title)` appears literally; the scan finds it.
- [ ] Emits `route_decided` (AC-2).
- [ ] Failure path leaves the session with **no** title and does not retry on every subsequent turn — a failed title must not become a per-turn model call (BR-3, and a cost trap).
- [ ] Runs on the local tier even when `reflex` has a remote binding attempt, and under a tainted session (BR-5). Asserted by captured bytes.
- [ ] Bounded by `TITLE_OUTPUT_MAX_BYTES`, test reads the constant (AC-11). A title is a handful of words — this is the tightest ceiling of the five.
- [ ] `ScriptedFileEngine` arm + no-block-consumed test (AC-12, BR-10) + contract-verbatim test.
- [ ] `cargo test --workspace --no-fail-fast` is green.

## Technical Notes

`SessionSummary.title: Option<String>` already exists at
`teton-protocol/src/methods.rs:77` — populate it, do not add a second field
(ADR-6).

**The retry trap.** "Runs once per session" and "runs once per session *unless it
failed*" are different rules with very different costs. A failed title that
retries every turn turns the cheapest category into a per-turn model call. Decide
explicitly: a failed attempt marks the session as attempted, so it does not
re-fire. Test it — a two-turn session with a failing duty must show exactly one
call, not two.

`title` is reflex/local, so it has no remote duty impl. It still goes through the
shared seam so BR-2's emission and BR-3's fallback are inherited rather than
re-implemented.
