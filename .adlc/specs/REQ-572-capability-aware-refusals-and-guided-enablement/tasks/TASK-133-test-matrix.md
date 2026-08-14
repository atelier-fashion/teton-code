---
id: TASK-133
title: "Tests: consent-matrix extension, e2e flow, secret sweep, backend contracts"
status: draft
parent: REQ-572
created: 2026-08-13
updated: 2026-08-13
dependencies: ["TASK-131", "TASK-132"]
---

## Description

The acceptance evidence: extend the REQ-563 matrices and e2e suites to cover
every REQ-572 AC that is automatable, including the egress-capture zero-traffic
assertions, the same-session live-pickup proof, the secret sweep, and the
backend-suggestion contract tests (AC-8).

## Files to Create/Modify

- `crates/tetond/tests/web_setup_flow.rs` — new integration suite over the existing harness fixtures (`LookupCapture`, fake `KeychainBackend`, scripted engine): AC-1 (no `[web]` table → prompt carries the OffAvailable clause, zero lookup packets captured for the session); AC-3 core (plan → preview → commit against a real runtime; after commit a consented lookup succeeds in the SAME runtime with no restart — egress captured); AC-6 (commit-validation-failure leaves config bytes identical); AC-7 (search + no local model → plan says SearchUnavailable with reason; preview refuses tier=search; a keyless SearxNG-shaped config previews clean); BR-13 (egress capture proves the flow itself sent zero packets through the choke point).
- `crates/tetond/tests/web_consent_matrix.rs` — extend: post-commit consent behavior (commit does NOT grant — the next lookup still prompts at Ask; LESSON-495 key scoping unchanged); the AC-4 second-connection rejection (cross-check with TASK-130's server tests, matrix-side assertion of the event at a subscriber).
- `crates/tetond/tests/web_setup_contracts.rs` — AC-8: for each backend named in `self_config.md`/flow suggestions (SearxNG keyless `?format=json`, Brave via `X-Subscription-Token: {key}`, Kagi via `Authorization: Bot {key}`), drive the PRODUCTION `search_request` + `search_auth` template path against a fixture asserting the exact method/URL-shape/header each backend documents; a helper enumerates the suggestion list from the bundled text so an added suggestion without a contract test fails the suite.
- `crates/teton/tests/pty_e2e.rs` — AC-5 hooks: key entry does not echo (pty capture contains no fixture-secret bytes); the full transcript sweep for the planted secret after a completed flow.
- `crates/teton/tests/cli_e2e.rs` — AC-10 non-TTY degradation; `/web setup` happy path against the test daemon; completion notice renders.

## Acceptance Criteria

- [ ] Every automatable REQ-572 AC (1, 3–8, 10, 11-client-leg) maps to at least one named test; the suite header comments carry the AC map like `web_consent_matrix.rs` lines 11–28
- [ ] The AC-8 enumeration helper fails the suite when a suggestion string is added to the bundled text without a matching contract fixture
- [ ] Secret sweep: a planted fixture key appears in no config file, no event payload, no captured RPC frame, and no pty transcript after a completed flow
- [ ] All new tests pass with `cargo test --workspace` (BUG-164 rule: workspace build, not `-p` targeted, before claiming green)

## Technical Notes

AC-2's remote-tier dead-end event asserts in the existing unserved-turn test
area (TASK-129 wrote the emission; assert delivery here at a subscriber).
AC-9's live dedup (model compresses a repeat offer) is model behavior — record
it as a **manual gate** in the suite header per the repo's deferred-AC
convention (commit 9c2d2ed precedent), with the prompt-instruction presence
pinned automatically.
