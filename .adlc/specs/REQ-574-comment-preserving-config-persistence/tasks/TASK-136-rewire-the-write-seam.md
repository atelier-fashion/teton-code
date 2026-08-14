---
id: TASK-136
title: "Rewire the daemon write seam through the delta engine"
status: draft
parent: REQ-574
created: 2026-08-14
updated: 2026-08-14
dependencies: ["TASK-135"]
repo: teton-code
---

## Description

Split the write seam: `write_config_atomically` keeps only the atomic I/O
mechanics (temp file + mode preservation + fsync + rename) and now takes
pre-rendered text; a new `persist_config(path, current, candidate)` wrapper
reads the on-disk document, applies the TASK-135 delta, validates the edited
bytes through `Config::load`, and hands the text to the writer. Adapt all
five callers. Loud refusal on unparseable on-disk documents; no fallback
re-serialization (spec BR-6).

## Files to Create/Modify

- `crates/tetond/src/runtime.rs` — `write_config_atomically(path, text: &str)` (mechanics unchanged, ~4658); NEW `persist_config(path: &Path, current: &Config, candidate: &Config) -> Result<...>`; adapt callers: `apply_config_update` (~2189), `persist_web_tier` (~3858), `web_setup_commit` (~4068; full rewiring of preview/digest lands in TASK-137 — here it just calls persist_config), `migrate_and_report_provider_models` (~4799; clone the pre-mutation config for `current`), `migrate_and_report_routing_table` (~5015; same clone-before-mutate)

## Acceptance Criteria

- [ ] Exactly one atomic-write body remains; temp-file + mode-preservation + fsync + rename mechanics byte-identical to today (existing tests `rewriting_the_config_preserves_its_permissions` ~7484 and the readonly-dir atomicity test ~7437 pass unchanged)
- [ ] All five callers persist through `persist_config`; no caller serializes via `Config::to_toml()` for an existing document anymore
- [ ] Unparseable on-disk file: RPC writers (`persist_web_tier`, `web_setup_commit`, `apply_config_update`) refuse with their existing error codes carrying the inner parse reason (LESSON-456/BUG-146 — never generic); file bytes untouched; in-memory config untouched (swap-after-write ordering kept)
- [ ] Startup migrations warn-and-continue on a failed write exactly as today, message carrying the inner reason
- [ ] Missing file (config_path set, file absent): fresh document written at mode 0600 whose parse equals the candidate (delta base = default config, per ADR-1)
- [ ] Validation runs on the edited bytes (`Config::load` on the delta output) before any write; a validation failure surfaces the validator's own sentence (spec BR-4, AC-10 groundwork)
- [ ] Migration idempotence unaffected: `the_routing_migration_persists_and_never_runs_twice` (~7389) and REQ-557 migration tests pass
- [ ] `cargo test -p tetond` (default features) green

## Technical Notes

- `persist_config` reads the file under the caller's existing config-mutex
  critical section — all RPC callers already hold it across validate+write.
- Migrations mutate `config: &mut Config` in place today; capture
  `let before = config.clone();` before mutation and pass as `current`.
- Read: `std::fs::read_to_string`; absent file → empty string base. Reuse the
  existing error-message shapes (`the configuration could not be saved (…)`)
  with the delta/parse error's display inside.
- Do NOT touch `candidate_digest`/preview rendering here (TASK-137) — but the
  no-op checks (`candidate.web == config.web`) stay in-memory (ADR-4/OQ-3).
