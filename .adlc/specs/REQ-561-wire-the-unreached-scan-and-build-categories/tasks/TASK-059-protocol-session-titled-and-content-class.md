---
id: TASK-059
title: "Protocol: the session_titled event and the per-category content class"
status: complete
parent: REQ-561
created: 2026-08-07
updated: 2026-08-07
dependencies: []
---

## Description

Add the two wire-visible surfaces this REQ needs: the `session_titled` event
(BR-9a) and the per-category content-class disclosure that `policy show` will
render (BR-11).

Independent of TASK-058 — it touches `teton-protocol` only, so it can be built
against the current tree.

## Files to Create/Modify

- `crates/teton-protocol/src/events.rs` — add `SessionTitled { session_id, title }` payload struct and the `Event::SessionTitled` variant; add its arm to the `Event::name()` match (returning `"session_titled"`) and to the module-level event-table doc comment at the top of the file.
- `crates/teton-protocol/src/methods.rs` — add a content-class field to the per-category routing view that `policy show` renders (the type carrying the `declared, no call site yet` flag, ~line 575).

## Acceptance Criteria

- [x] `Event::SessionTitled` exists, serialises with `"event": "session_titled"`, and round-trips through the wire encoding.
- [x] `teton-protocol` still has **no** dependency on `teton-core` — assert by inspecting `crates/teton-protocol/Cargo.toml`. The payload is `SessionId` + `String`, both plain, so this holds. Asserted by `the_protocol_crate_depends_on_no_other_teton_crate` (`lib.rs`), which reads the manifest's dependency tables; the manifest is unchanged.
- [x] The content class is expressible for **all eleven** categories, including the ones with no call site. A category that transmits nothing today says so explicitly rather than being absent (AC-16).
- [x] The module-level event table at the head of `events.rs` lists `session_titled` — that comment is a documented index, and omitting the new event makes it wrong.
- [x] `cargo test --workspace --no-fail-fast` is green.

## Deviations

- **`SessionTitled` carries `title` only; the session is named by
  `EventEnvelope.session_id`.** ADR-6's literal `SessionTitled { session_id,
  title }` is unrepresentable: `Event` is internally tagged and flattened, so a
  payload `session_id` collides with the envelope's, emits the key twice, and
  fails to deserialize (`Error("duplicate field \`session_id\`", line: 1, column:
  64)`, observed). The **wire object is exactly ADR-6's shape** —
  `{"session_id":…,"seq":…,"event":"session_titled","title":…}` — assembled by
  the envelope, as it is for `route_decided` and every other session-scoped
  event. Consequence for TASK-062: the emitter must scope the envelope with
  `Some(session_id)`, since the payload no longer self-identifies.
- **Three construction sites outside `teton-protocol` were updated because the
  compiler requires it**, not because this task wires anything: the new `Event`
  variant makes `teton/src/session_ui.rs`'s match non-exhaustive, and the new
  `CategoryRouteView` field makes two struct literals incomplete
  (`teton/src/main.rs` test helper, `tetond/src/runtime.rs::category_route_view`).
  No resolver, call site, or `has_call_site` arm was touched.

## Technical Notes

`SessionSummary.title: Option<String>` **already exists** at
`methods.rs:77` — do not add a second title field. The spec's Entities table
calls `Session.title` new; the code disagrees, and the code is right (ADR-6).
This task adds the *event*, not the storage.

Content class is a fixed descriptor per category, not free text chosen at
runtime — `triage` transmits file content, `compact` transmits conversation
blocks, `shell` transmits command output, `title` transmits the session's first
prompt, `digest` transmits tool output. The point (BR-11) is that `triage` and
`compact` disclose *different* classes despite sharing the `scan` tier.

Declaring a class for a still-unreached category (`redact`) describes intent, not
a call site — it does not wire that category and does not intrude on REQ-562.
