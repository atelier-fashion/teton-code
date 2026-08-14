---
id: TASK-138
title: "Per-writer preservation witnesses and refusal tests"
status: draft
parent: REQ-574
created: 2026-08-14
updated: 2026-08-14
dependencies: ["TASK-136", "TASK-137"]
repo: teton-code
---

## Description

The preservation invariant is one seam with five caller paths; per LESSON-502
each path gets its own witness. Add the integration tests that pin spec
AC-1/AC-2/AC-4/AC-5/AC-6/AC-8/AC-10 end-to-end, using the README's commented
`[web]` block verbatim as the shared fixture (LESSON-512).

## Files to Create/Modify

- `crates/tetond/src/runtime.rs` (test modules) or `crates/tetond/tests/config_preservation.rs` (NEW; prefer a dedicated integration test file, default features, so the suite reads as the REQ's witness list) — fixture: README `[web]` block verbatim + unknown key in `[web]` + unknown top-level `[experimental]` table + comments
- `crates/tetond/tests/web_consent_matrix.rs` — extend the enable_permanent byte tests (~1860-1960) and read-back posture (~778) to the commented fixture where they seed configs

## Acceptance Criteria

- [ ] `persist_web_tier` witness: comments, unknown key, unknown table, key order survive; only the operation's keys differ; read-back via `Config::load` matches (spec AC-1)
- [ ] `web_setup_commit` witness: same preservation property (spec AC-2)
- [ ] `apply_config_update` (provider registration) witness: same property — `[web]` comments survive a `[[providers]]` append (spec AC-2)
- [ ] Both startup migrations' witnesses: a pre-REQ-557 / pre-REQ-558 config **with comments** migrates with comments intact outside the migrated keys (spec AC-2)
- [ ] Unparseable on-disk config: each RPC writer refuses with the inner parse reason; file bytes byte-identical after the refusal (spec AC-5)
- [ ] Parseable-but-invalid drift (hand edit failing `Config::validate` at an unrelated key): `persist_web_tier` and `web_setup_commit` refuse with the validator's sentence; the invalid edit is NOT overwritten (spec AC-10)
- [ ] Missing file: write produces a fresh 0600 document whose parse equals the candidate (spec AC-6)
- [ ] Written bytes always re-parsed through `Config::load` in every witness (spec AC-8, web_consent_matrix posture)
- [ ] All tests run in the default-feature `cargo test --workspace` CI leg — no feature-gated targets (BUG-166/LESSON-515)

## Technical Notes

- Reuse `runtime_on_disk`/`scratch_dir` helpers (runtime.rs ~14279, ~15468)
  where in-crate; the new integration file can spin the same
  `DaemonRuntime` construction path the consent-matrix tests use.
- "Only the operation's keys differ" is assertable mechanically: diff the
  before/after texts line-wise and assert the changed-line set ⊆ the expected
  key lines — stronger and cheaper than field-by-field re-parsing alone.
- The AC-10 invalid-drift fixture: hand-write `cache_ttl_secs = "not-a-number"`?
  No — that fails parse, not validate. Use a *validator*-level breach that
  parses fine, e.g. `[web] tier = "search"` with no `search_endpoint`, or a
  provider `auth_ref` carrying a raw-key shape (BR-7 class). Pick one that
  `Config::validate` (not the TOML parser) rejects.
