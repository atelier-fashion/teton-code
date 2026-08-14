---
id: TASK-138
title: "CLI: render suggestions from the RPC catalog; delete client-side constants"
status: complete
parent: REQ-573
created: 2026-08-14
updated: 2026-08-14
dependencies: ["TASK-135"]
---

## Description

Make the CLI a pure renderer of the daemon catalog (BR-1/BR-8): delete
`ENDPOINT_HELP`, `KNOWN_BACKEND_AUTH`, `DEFAULT_SEARCH_AUTH`; drive help
lines, piped `instruction_lines`, and the offered auth default from
`plan.suggestion_catalog`; degrade per BR-3 when the catalog is absent.

## Files to Create/Modify

- `crates/teton/src/web_setup_ui.rs` — delete the three constants (~79, ~816,
  ~824–843); `collect()` (~596, help render at ~637) and `plan_lines()`
  (~735) take suggestions from the plan; `instruction_lines()` (~1031)
  builds from the catalog; `offered_auth()` (~851) becomes
  `offered_auth(endpoint, catalog)` matching parsed host against
  `backends[].host` else `catalog.default_auth_template`; degraded path uses
  `teton_protocol::GENERIC_SEARCH_AUTH_TEMPLATE` (ADR-B), no named
  suggestion lines, needs-key default unchanged (yes); update fixtures
  `plan_ready_for_search()` / `plan_without_search()` and every
  `WebSetupPlanResult` literal; rewrite the two constant-consistency tests
  (~1959, ~2000); add the AC-2 synthetic-catalog test and the AC-7
  catalog-absent test

## Acceptance Criteria

- [x] Zero backend endpoint/auth literals remain in the teton crate: grep for
      `api.search.brave.com`, `kagi.com`, `X-Subscription-Token`,
      `Authorization: Bot`, `Bearer {key}` finds no non-test occurrence
      (test fixtures use sentinels; the degraded default is the shared
      protocol const, not a local literal) (AC-2/BR-1)
- [x] Synthetic-catalog test (LESSON-497 sentinels, e.g. `sentinel-backend` /
      `X-Sentinel-Header: {key}`): help lines, `instruction_lines`, and the
      offered default all track the injected data (AC-2)
- [x] Catalog-absent test: plan with `suggestion_catalog: None` → flow
      completes; no named suggestions rendered; offered default is
      `Authorization: Bearer {key}`; needs-key default yes; no panic (AC-7,
      BR-3)
- [x] With a catalog carrying the real three backends, rendered help lines
      and piped instructions are byte-identical to the v0.1.14 strings
      (parity fixture pinned in-test, AC-6 CLI altitude); Brave-host match
      offers `X-Subscription-Token: {key}`, unknown host offers the Bearer
      default (BR-8)
- [x] All existing web_setup_ui unit tests pass with updated fixtures;
      `Answers` debug-redaction and keychain/undo tests unchanged in
      behavior
- [x] `cargo test -p teton` green

## Technical Notes

The ~30 unit tests share `FakeIo` + two plan fixtures, so the field lands in
few places; give `plan_ready_for_search()` the real-shaped catalog (that is
what parity asserts) and let the AC-2 test build its own sentinel catalog.
Piped e2e text ("self-hosted SearxNG…") must keep rendering byte-identical —
`cli_e2e.rs:2762` asserts it against the real daemon in TASK-139's
verification. LESSON-514's undo paths are untouched — do not refactor
commit/cleanup code while editing this file.
