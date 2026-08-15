---
id: TASK-157
title: "E2E: provider_setup_flow.rs against a spawned daemon; CLI piped degradation"
status: draft
parent: REQ-579
created: 2026-08-15
updated: 2026-08-15
dependencies: ["TASK-154", "TASK-155"]
---

## Description

The end-to-end evidence (LESSON-523: at least one real registration through the real seam). A `tetond` integration suite mirroring `web_setup_flow.rs`: spawn a daemon with a temp config, drive plan → preview → commit over the socket as the session's own connection, assert the config file bytes equal the preview's TOML, assert the next `think` route resolves to the new id without restart, and assert every refusal path leaves the file byte-identical. Plus the CLI e2e for BR-11: piped stdin `/provider setup kimi` prints the instruction lines and consumes no further stdin.

**Covers:** AC-2, AC-4 (config bytes hold only keychain://), AC-7, AC-9 (cli_e2e), AC-10, AC-11, AC-12 — all asserted on file bytes

## Files to Create/Modify

- `crates/tetond/tests/provider_setup_flow.rs` — new; using the `tests/e2e/harness.rs` fixtures (`Daemon`, `Client`, temp `TETON_CONFIG`): (1) happy path: plan → preview(kimi, kimi-k3, key_ref keychain://teton/kimi, [think]) → commit(expect_digest) → file bytes == preview toml; `provider_setup_completed` received; `teton policy show`-equivalent RPC reports think→kimi; (2) stale digest → refused, bytes unchanged; (3) second connection (did not open the session) → `SETUP_REJECTED_NONUSER`, bytes unchanged, `provider_setup_rejected_nonuser` event; (4) replace existing `kimi`: preview `replaces` populated, commit keeps other providers and comments byte-identical (seed the temp config with a comment); (5) `TETON_PRESENCE_ACCEPT=fail` (under `TETON_TEST_SEAMS`) → commit refused, bytes unchanged — skip with a printed reason if the build lacks the presence feature, exactly as the web suite does; (6) unchanged candidate → `applied: false`, file mtime/bytes unchanged
- `crates/teton/tests/cli_e2e.rs` — `echo '/provider setup kimi' | teton` (or the suite's session-piping helper) prints the `teton provider add kimi …` and `teton policy set-tier think kimi` lines, exits 0, and a following stdin line is not consumed (mirror the `/model set` piped test)
- `crates/teton/tests/pty_e2e.rs` — OPTIONAL: a pty walk of `/provider setup kimi think` with `TETON_TEST_SEAMS` and an env-injected fake keychain if the pty harness already supports one for `/web setup`; if it does not, record "pty walk not added — no fake-keychain seam in pty harness" in the completion note rather than adding a seam here

## Acceptance Criteria

- [ ] All six daemon scenarios pass on macOS and the Linux CI leg (the presence scenario prints its skip reason on builds without the feature)
- [ ] The CLI piped test passes
- [ ] No test reads its own writes back through the RPC that made them — every "unchanged" assertion is on the file bytes (LESSON-519)
- [ ] `cargo test --workspace` green

## Technical Notes

Read `web_setup_flow.rs` L1–150 for the fixture pattern and how it seeds the config and calls `client.call("web/setup_plan", …)`. The routing assertion: find the RPC or runtime accessor the web suite uses to prove "live pickup" (REQ-572 BR-8) and reuse it. For the presence scenario, copy the web suite's guard verbatim. Keep the suite under ~1 min: one daemon per scenario, or one daemon with per-scenario config reset — match whichever the web suite does.
