---
id: TASK-136
title: "Daemon: web_setup_catalog module + populate setup_plan"
status: complete
parent: REQ-573
created: 2026-08-14
updated: 2026-08-14
dependencies: ["TASK-135"]
---

## Description

Create the single catalog definition in the daemon (ADR-A): a pure factory
`suggestion_catalog() -> WebSetupCatalog` with the three backends carrying
today's exact strings (BR-7), and wire it into `runtime.web_setup_plan()`.

## Files to Create/Modify

- `crates/tetond/src/web_setup_catalog.rs` — new module: `pub fn
  suggestion_catalog() -> WebSetupCatalog` (searxng / brave / kagi rows per
  architecture.md table; `default_auth_template` =
  `GENERIC_SEARCH_AUTH_TEMPLATE`); unit tests in-module
- `crates/tetond/src/lib.rs` (or the crate's module root) — `pub mod
  web_setup_catalog;`
- `crates/tetond/src/runtime.rs` — `web_setup_plan()` (~line 3922) adds
  `suggestion_catalog: Some(web_setup_catalog::suggestion_catalog())`

## Acceptance Criteria

- [x] Golden-string unit test pins all three entries byte-exact (AC-6 daemon
      altitude): SearxNG `http://localhost:8888/search?format=json`
      keyless/no-host; Brave host `api.search.brave.com`, template
      `X-Subscription-Token: {key}`; Kagi host `kagi.com`, template
      `Authorization: Bot {key}`; default template `Authorization: Bearer
      {key}` via the shared const
- [x] Invariants unit-tested: ids unique; `auth_template.is_some() ==
      needs_key` per entry; every template contains `{key}`; no entry or
      field contains a secret-shaped value (BR-6)
- [x] The factory takes no arguments and reads no env/config/TTY state
      (LESSON-481 purity — reviewable by signature, pinned by a test module
      that calls it with no setup)
- [x] `web_setup_plan()` returns `Some(catalog)` — asserted by a runtime-level
      test if one exists for the method, else by the TASK-137 suite
- [x] `cargo test -p tetond` green (contract suite still passes untouched at
      this point — it parses source text until TASK-137 lands)

## Technical Notes

Factory precedent: `model_consent::list_entries`. Keep the module tiny and
dependency-free so the contract suite (`tests/` dir, links the lib) can call
it directly. Do NOT touch `web_setup_contracts.rs` here — TASK-137 owns that
rewrite; this task must leave the existing suite green, which it does because
the CLI constants it parses still exist until TASK-138.
