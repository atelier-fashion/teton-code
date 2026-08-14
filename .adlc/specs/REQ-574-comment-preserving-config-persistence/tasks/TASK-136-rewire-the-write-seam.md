---
id: TASK-136
title: "Rewire the daemon write seam through the delta engine"
status: complete
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

- [x] Exactly one atomic-write body remains; temp-file + mode-preservation + fsync + rename mechanics byte-identical to today (existing tests `rewriting_the_config_preserves_its_permissions` and the readonly-dir atomicity test pass; the former's third leg now renders the text inline because the writer's signature changed — the mechanic it pins is untouched)
- [x] All five callers persist through `persist_config`; no caller serializes via `Config::to_toml()` for an existing document anymore
- [x] Unparseable on-disk file: RPC writers (`persist_web_tier`, `web_setup_commit`, `apply_config_update`) refuse with their existing error codes carrying the inner parse reason (LESSON-456/BUG-146 — never generic); file bytes untouched; in-memory config untouched (swap-after-write ordering kept). Witnessed at the seam and through `persist_web_tier`; the other two writers' witnesses are TASK-138's
- [x] Startup migrations warn-and-continue on a failed write exactly as today, message carrying the inner reason (`{err}` over an error whose `Display` embeds it; the readonly-dir test still pins the byte-for-byte leg)
- [x] Missing file (config_path set, file absent): fresh document written at mode 0600 whose parse equals the candidate (delta base = default config, per ADR-1)
- [x] Validation runs on the edited bytes (`Config::load` on the delta output) before any write; a validation failure surfaces the validator's own sentence (spec BR-4, AC-10 groundwork)
- [x] Migration idempotence unaffected: `the_routing_migration_persists_and_never_runs_twice` and REQ-557 migration tests pass
- [x] `cargo test -p tetond` (default features) green — 1536 passed, 0 failed, 1 pre-existing `live` ignore

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

## Implementation Notes (post-implementation)

**One existing test was adapted, and it is the interesting one.**
`persisting_a_lower_tier_never_demotes_the_configured_ceiling` (runtime.rs,
`web_lookup_seam`) set `[web] tier = "search"` **in memory only** over an empty
config file, then asserted the reload still said `search`. That assertion was
only ever true because the old seam re-serialized the whole in-memory config —
i.e. it passed by writing state the file never held. Under ADR-1 an in-memory
value that is not in the delta is not written, which is precisely BR-5's
drift rule seen from the other side. The fix seeds the file with the ceiling the
test calls *configured*, so memory and disk agree the way a real start makes
them; the no-demote property it exists to pin is asserted unchanged.

`rewriting_the_config_preserves_its_permissions`'s third leg called
`write_config_atomically(path, &Config::default())`; it now renders that config
to text on the line above, because the writer takes text. Same mechanic, same
assertion.

**Error shape.** `persist_config` returns errors whose `Display` *embeds* the
inner reason rather than attaching it as anyhow context, because every caller
formats with `{err}` and anyhow shows only the outermost layer (LESSON-456).
So `persist_web_tier` on a broken document says "the configuration could not be
saved (the config file could not be parsed for editing, so nothing was written:
…)", and a validation refusal carries the validator's own sentence.

**Flagged for TASK-137**: `candidate_digest` still digests
`candidate.to_toml()`, which is no longer the bytes that land. Its doc comment
now says so explicitly and names TASK-137; the preview/commit byte-equality
tests still pass because their seed configs are canonical, so the delta output
equals the canonical output for them. Nothing was `#[ignore]`d.

**Flagged for TASK-138**: preservation is witnessed here once, at
`persist_web_tier` (comments, key order, an unknown key inside `[web]`, an
unknown top-level table), plus the seam-level refusal/missing-file/invalid-drift
tests. The per-writer suite across all five writers, with the README's `[web]`
block verbatim, is TASK-138's.
