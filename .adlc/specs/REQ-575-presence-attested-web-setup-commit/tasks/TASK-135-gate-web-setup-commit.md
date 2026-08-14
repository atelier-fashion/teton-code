---
id: TASK-135
title: "Gate web/setup_commit with refuse_unattested_commitment; move it off the reader loop"
status: draft
parent: REQ-575
created: 2026-08-14
updated: 2026-08-14
dependencies: []
repo: teton-code
---

## Description

Make `web/setup_commit` a REQ-570 BR-10(b) daemon-wide commitment: add the
shared presence check, make the handler async, and move it out of the
synchronous reader-loop `dispatch` onto the `blocks_on_a_human` spawn path so a
parked presence prompt cannot stall the connection. Migrate the existing
session-gate unit test + its mutation twin off `dispatch` in the same change so
the suite stays green. This is the foundational task; all other REQ-575 tasks
depend on it. See `architecture.md` ADR-1/ADR-3.

## Files to Create/Modify

- `crates/tetond/src/server.rs` — (1) in `handle_web_setup_commit` (~2380): add
  `if let Some(refusal) = refuse_unattested_commitment(daemon, conn, &id).await { return refusal; }`
  **after** `refuse_commit_without_session_access` and **before**
  `daemon.runtime.web_setup_commit(...)`; change the signature to `async fn`.
  (2) In `handle_client` (~1535): add `|| m == WebSetupCommitParams::METHOD` to
  the `blocks_on_a_human` match and a branch
  `else if method == WebSetupCommitParams::METHOD { handle_web_setup_commit(&daemon, &conn, id, params).await }`
  in the spawn body. (3) In `dispatch` (~2151): **remove** the
  `WebSetupCommitParams::METHOD => …` arm. (4) Update the adjacent comments to
  tell the truth: the `blocks_on_a_human` rationale block (~1532) names the
  commit as a fourth presence-parking method; the "three setup methods are
  session-scoped" comment (~2145) now covers only plan+preview; and the
  `refuse_unattested_commitment` doc "those two methods, and only those two"
  (~928) becomes the **three-method** set (BR-5/AC-7). (5) Migrate
  `a_commit_without_session_access_is_refused_and_the_session_is_told` and its
  mutation-doc twin (~6890-6947) from `dispatch(&daemon, &intruder, …, WebSetupCommitParams::METHOD, …)`
  to `handle_web_setup_commit(&daemon, &intruder, …).await` (the model-method
  unit tests' direct-call pattern; server.rs ~2569 documents why the no-runtime
  path is safe in tests).

## Acceptance Criteria

- [ ] `handle_web_setup_commit` is `async` and calls `refuse_unattested_commitment`
      after the session-access gate, before the runtime is touched (BR-1, BR-2).
- [ ] `web/setup_commit` is removed from the `dispatch` match and handled in the
      `blocks_on_a_human` spawn path in `handle_client` (no reader-loop parking).
- [ ] `web/setup_plan` and `web/setup_preview` remain in `dispatch`, unchanged.
- [ ] No surviving "only those two methods" / two-method framing for BR-10(b) in
      `server.rs`; the set is named as three where the split is explained (AC-7).
- [ ] The migrated session-gate test and its mutation twin compile and pass via
      the direct async handler call; behavior asserted is unchanged (REQ-572 BR-4
      coverage preserved and non-vacuous).
- [ ] `cargo build -p tetond` and `cargo test -p tetond` (workspace build first)
      are green; `cargo clippy` adds no new warnings.

## Technical Notes

- `refuse_unattested_commitment` is reused verbatim — do NOT write a parallel
  check (LESSON-499). Its `Unavailable` arm already degrades (allow + stderr
  notice), so shipped/CI builds gain no prompt (BR-3); that behavior is inherited,
  not re-implemented here.
- The `block_in_place` vs single-thread branch inside `refuse_unattested_commitment`
  is already handled by the function; the commit handler just awaits it.
- Do not reorder the existing REQ-572 gates — `refuse_unmintable_session_id` and
  `refuse_commit_without_session_access` stay first; attestation is last so an
  unattached/unmintable caller never triggers a prompt (BR-2).
- Build the workspace before running targeted `-p tetond` tests so the daemon
  binary is current (LESSON: targeted e2e runs test a stale daemon).
