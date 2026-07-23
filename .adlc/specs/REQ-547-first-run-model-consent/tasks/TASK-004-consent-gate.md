---
id: TASK-004
title: "Consent gate + decision persistence in the daemon"
status: draft
parent: REQ-547
created: 2026-07-21
updated: 2026-07-21
dependencies: [TASK-001]
---

## Description

The heart of the REQ (BR-1..BR-5, BR-10, BR-12): probe → propose → await answer
→ only then proceed. Sessions keep working remote-only while awaiting (D-3).

## Files to Create/Modify

- `crates/tetond/src/model_consent.rs` — new: proposal assembly (probe report + proposed entry + alternatives + required disk), a pending-decision registry mirroring `PendingPermissions`, and the await/resolve path for `model/confirm`
- `crates/tetond/src/selection_store.rs` — new: persist/read `ModelSelection` in the daemon state dir (D-4); accepted/declined/chosen + source + timestamp
- `crates/tetond/src/server.rs` — handlers for `model/confirm`, `model/list`, `model/set`, `model/status`
- `crates/tetond/src/runtime.rs` — first-run flow: consult the store; if undecided and not auto-accept, emit `model_selection_proposed` and leave the local tier unavailable; on decision, hand off to the install pipeline
- `crates/tetond/tests/model_consent.rs` — the gate's behavioral tests

## Acceptance Criteria

- [ ] **Zero download requests are issued before a decision** — asserted with a fetcher double that records every call (AC-1); a session started while undecided completes remote-only rather than blocking (BR-1)
- [ ] Accepting proceeds to install; choosing an alternative installs that entry instead (AC-2/AC-3 backend)
- [ ] Declining persists and a subsequent daemon start does NOT re-prompt (AC-4)
- [ ] `auto_accept` completes the flow with no proposal emitted (AC-5)
- [ ] Offline accept → clear network error, no partial install, and the decision is **not** recorded as declined; a later run re-prompts (AC-10, BR-12)
- [ ] A recorded decision is not re-litigated; re-prompt occurs only when weights are missing/corrupt or `model/set` is called (BR-10)
- [ ] Proposal payload carries no absolute path or credential (BR-11)

## Technical Notes

Reuse the `PendingPermissions` resolve pattern — the server's reader loop must
stay free to process `model/confirm` while the flow awaits, exactly like the
permission round-trip (a prior REQ-544 review confirmed that ordering is
deadlock-free; mirror it rather than inventing a second mechanism).
