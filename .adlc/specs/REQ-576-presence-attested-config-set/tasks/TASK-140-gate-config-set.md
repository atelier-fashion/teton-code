---
id: TASK-140
title: "Gate config/set with refuse_unattested_commitment; move it off the reader loop; reverse the BUG-162 comment"
status: draft
parent: REQ-576
created: 2026-08-14
updated: 2026-08-14
dependencies: []
repo: teton-code
---

## Description

Make `config/set` the fourth REQ-570 BR-10(b) daemon-wide commitment: add the
shared presence check, make the handler async, move it off the synchronous
`dispatch` onto the `blocks_on_a_human` task, and — because config/set is a
genuine `daemon_wide_method` — add it to the shared `commitments` list so the
existing harness asserts its refusal/degradation. Reverse the documented
BUG-162 layer-(a)-only comment. Foundational task; the others depend on it. See
`architecture.md` ADR-1/ADR-2/ADR-4.

## Files to Create/Modify

- `crates/tetond/src/server.rs` — (1) in `handle_config_set` (~2723): add
  `if let Some(refusal) = refuse_unattested_commitment(daemon, conn, &id).await { return refusal; }`
  **after** `refuse_daemon_wide` and **before** `apply_config_update`; change the
  signature to `async fn`. (2) In `handle_client`'s `blocks_on_a_human`: add
  `|| m == ConfigSetParams::METHOD` to the `matches!` guard and an explicit
  `else if method == ConfigSetParams::METHOD { handle_config_set(&daemon, &conn, id, params).await }`
  branch **before** the REQ-575 `unreachable!()` else. (3) In `dispatch` (~2197):
  **remove** the `ConfigSetParams::METHOD => …` arm. (4) In `route_for_test`
  (~4190): add a `ConfigSetParams::METHOD` async branch (so the daemon_wide tests
  reach the real handler off the reader loop). (5) In
  `only_a_daemon_wide_commitment_demands_presence` (~4287): add
  `ConfigSetParams::METHOD` to the `commitments` array (this test loops
  `daemon_wide_methods()`, which already includes config/set, and checks
  membership — so this one edit makes it assert config/set refuses).
  (5b) **Also add `ConfigSetParams::METHOD` to the hardcoded
  `[ModelConfirmParams::METHOD, ModelSetParams::METHOD]` list in
  `a_commitment_degrades_to_layer_a_where_no_mechanism_exists`** — that test does
  NOT loop `daemon_wide_methods()`, so config/set's **degradation (AC-2)** is not
  asserted without this. (`layer_a_refuses_independently_of_any_attestation_mechanism`
  loops `daemon_wide_methods()`, so it auto-covers config/set — no edit needed.)
  Consider refactoring `a_commitment_degrades` to iterate the shared `commitments`
  set to prevent this drift for the next method; minimal fix is adding config/set
  to its list. (6) Rewrite the
  `handle_config_set` doc comment (BR-7): it no longer claims layer (a) suffices —
  a daemon-wide commitment now attests, and the "can edit config.toml directly"
  mitigation is insufficient for a commitment (REQ-570/REQ-575 reasoning). (7)
  Update `refuse_unattested_commitment`'s doc: the set is now **four** methods,
  and the "config/set is the known next candidate … tracked in REQ-576" paragraph
  becomes "now gated (REQ-576)"; the BR-5 standing-obligation sentence stays.

## Acceptance Criteria

- [ ] `handle_config_set` is `async` and calls `refuse_unattested_commitment`
      after `refuse_daemon_wide`, before `apply_config_update` (BR-1, BR-2 order).
- [ ] `config/set` removed from `dispatch`, handled in `blocks_on_a_human`; the
      explicit branch sits before `unreachable!()` (no method hits unreachable! in
      production).
- [ ] `config/set` added to `route_for_test` and to the `commitments` list; the
      shared `only_a_daemon_wide_commitment_demands_presence` now asserts config/set
      **refuses** under `AlwaysFailsVerifier` (AC-3 mutation for this seam), and
      **AC-2 (degradation)** is asserted by `a_commitment_degrades_to_layer_a_where_no_mechanism_exists`
      + `layer_a_refuses_independently_of_any_attestation_mechanism` now covering
      config/set — a shipped no-mechanism build lets config/set reach the runtime,
      gaining no new prompt.
- [ ] No stale three-method / "known next candidate" BR-10(b) framing in
      `server.rs`; the set is named as four (BR-5 code half, AC-5 code half).
- [ ] The `handle_config_set` comment records the reversal honestly (BR-7).
- [ ] The **integration** config/set suites — `event_response_ordering.rs`,
      `multi_client.rs`, `e2e/*` (model_identity, conversation_carry, ac_matrix) —
      stay green: event→response ordering is preserved by the spawn path's
      `fence.sync().await` (verify, don't assume).
- [ ] Workspace build first, then `cargo test -p tetond`; clippy adds no warnings.

## Technical Notes

- Reuse `refuse_unattested_commitment` verbatim (LESSON-499); its `Unavailable`
  arm degrades, so shipped/CI builds gain no prompt.
- OQ-1 (verified at architecture): `apply_config_update` has exactly one
  production caller (this handler). Re-verify with
  `grep -rn "apply_config_update" crates/ --include=*.rs | grep -v "fn apply_config_update"`
  before gating — a new programmatic caller would change the analysis.
- The `commitments`-list entry is config/set's per-seam mutation test (AC-3):
  deleting the attestation line turns `only_a_daemon_wide_commitment_demands_presence`
  red for config/set alone.
- Build the workspace before targeted `-p tetond` runs (stale-daemon pitfall).
