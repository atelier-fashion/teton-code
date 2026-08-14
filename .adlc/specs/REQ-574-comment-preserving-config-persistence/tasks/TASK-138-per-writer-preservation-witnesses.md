---
id: TASK-138
title: "Per-writer preservation witnesses and refusal tests"
status: complete
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

- [x] `persist_web_tier` witness: comments, unknown key, unknown table, key order survive; only the operation's keys differ; read-back via `Config::load` matches (spec AC-1) — `a_consent_answer_moves_its_own_keys_and_leaves_the_readme_config_alone`, two legs (pure insertion on the README block as written; the tier line too when the answer raises the ceiling)
- [x] `web_setup_commit` witness: same preservation property (spec AC-2) — `a_setup_commit_writes_the_bytes_its_preview_showed_and_moves_nothing_else`, which also ties AC-3 to the README fixture: `preview.toml` == the `[web]` section of the written file, `preview.digest` == sha256 of its bytes
- [x] `apply_config_update` (provider registration) witness: same property — `[web]` comments survive a `[[providers]]` append (spec AC-2) — `registering_a_provider_leaves_the_web_table_and_its_comments_alone`, which also pins ADR-1's array-wholesale cost (the comment *inside* the changed array does not survive; the `[web]` table is byte-identical)
- [x] Both startup migrations' witnesses: a pre-REQ-557 / pre-REQ-558 config **with comments** migrates with comments intact outside the migrated keys (spec AC-2) — `the_model_migration_carries_a_commented_config_across_the_upgrade` and `the_routing_migration_retires_its_table_without_taking_the_rest_of_the_file`, both driven through `DaemonRuntime::from_env` so the writer under test is the startup path; the second adds the idempotence leg (a second start writes nothing, asserted as byte-equality of a commented file)
- [x] Unparseable on-disk config: each RPC writer refuses with the inner parse reason; file bytes byte-identical after the refusal (spec AC-5) — `an_unparseable_document_is_refused_by_the_writers_that_would_have_rewritten_it` covers `apply_config_update` (`CONFIG_REJECTED`) and `web_setup_commit` (`INTERNAL_ERROR`); `persist_web_tier`'s witness is TASK-136's `an_unparseable_document_refuses_the_write_and_names_the_parse_failure` and is referenced rather than repeated
- [x] Parseable-but-invalid drift (hand edit failing `Config::validate` at an unrelated key): `persist_web_tier` and `web_setup_commit` refuse with the validator's sentence; the invalid edit is NOT overwritten (spec AC-10) — `a_hand_edit_that_fails_validation_refuses_both_writers_and_survives_them`, plus a third leg proving the preview refuses for the same reason
- [x] Missing file: write produces a fresh 0600 document whose parse equals the candidate (spec AC-6) — `a_config_file_that_does_not_exist_yet_is_created_owner_only`, through `persist_web_tier` on a `from_env` daemon whose config path exists and whose file does not. Its doc comment records what it cannot see (a daemon with no file starts on `Config::default()`, so the delta-base choice is unobservable here — that falsification stays in TASK-136's seam test)
- [x] Written bytes always re-parsed through `Config::load` in every witness (spec AC-8, web_consent_matrix posture) — `Daemon::reload()`, called by all seven write witnesses
- [x] All tests run in the default-feature `cargo test --workspace` CI leg — no feature-gated targets (BUG-166/LESSON-515) — one plain `#[test]` file, no `cfg`, no features; `cargo test --workspace` green (2444 passed, 0 failed, 1 pre-existing `live` ignore)

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

## Implementation Notes (post-implementation)

**Everything landed in one new file**, `crates/tetond/tests/config_preservation.rs`
(9 tests). `web_consent_matrix.rs` was **not** extended, and that is a
deliberate departure from this task's second file bullet — see "What was not
done and why" below.

**The construction obstacle, and the way through it.** `DaemonRuntime`'s
`config_path` is a private field, so an integration test cannot hand one to a
`minimal()` runtime; `web_setup_flow.rs` says so in its own header and answers it
by spawning a real daemon process. That would have been heavy here and would
still not have reached the startup migrations as *writers*. What works instead is
`DaemonRuntime::from_env(&scratch_dir, &events)`: with `TETON_CONFIG` unset the
config path falls back to `base_dir/config.toml`, so the path is per-test rather
than a process-global, and **both migrations run on the real startup path**. The
helper asserts `TETON_CONFIG` is absent rather than assuming it, because a set
one would silently point every daemon in the file at a single shared file. This
appears to be the first integration test in the tree to use `from_env`; it costs
a hardware probe, a bundled-catalog load and a SQLite ledger open per test, and
the whole file still runs in 0.08s.

**Two migrations, two fixtures, one writer each** (LESSON-502). Both migrations
fire from the same `from_env` call, so a fixture that trips both would name two
candidates on failure. `PRE_REQ_557_CONFIG` therefore carries a
`default_provider` and a `[[tiers]]` row (nothing for the routing migration to
do) and `PRE_REQ_558_CONFIG` gives every provider a `model` (nothing for the
model migration to do). Both were confirmed to fire exactly once, by the
migration's own stderr line and by a non-vacuity assertion on the migrated key.

**The assertion mechanic is a real diff, not a content-keyed filter.** The first
attempt dropped "expected changed lines" from both texts by content and compared
what was left; it collapses the moment a changed line is not unique
(`[[providers]]`, `[providers.capabilities]`, blank lines), because dropping the
*first* occurrence in `after` leaves the surviving sequence misaligned. The
working shape is `line_diff` — a longest-common-subsequence walk returning
deletions and insertions in document order — with the test asserting both lists
equal an explicit expectation. "Everything else is unchanged **and in the same
order**" is then not a second assertion; it is what is left over when those two
lists match. For the appending writers (both migrations) an exact insert list
would be a transcription of the rows REQ-558 exists to add, so those use
`assert_lines_survive_in_order`, which quotes the contiguous block the write is
allowed to retire and requires everything else to appear in `after` as a
subsequence.

**The README fixture is checked against the README.** `README_WEB_BLOCK` is the
fenced block verbatim, and `the_fixture_is_the_readmes_own_block_byte_for_byte`
**reads `README.md` at test time** (via `CARGO_MANIFEST_DIR`) and compares. That
turns the README's existing drift note — which promises that editing inside the
fence "fails that module's tests" — from an aspiration into a fact: before this,
editing the fence broke nothing, because `config_doc.rs`'s copy is only a copy.
The one cost is a test that depends on a file two directories up; it fails loudly
and legibly if that ever stops being true. (TASK-139 owns README edits; this test
was written against the block as it stands and passes with the sibling's prose
work in flight.)

**Mutation-checked.** With `render_config_document` reverted to the pre-REQ-574
behaviour (`candidate.to_toml()` over the whole document), **7 of the 9 tests
fail**. The two that survive are the right two: the README drift check is not a
write test, and AC-6's first write is the one case where a full serialization
*is* the correct answer.

### What was not done and why

`crates/tetond/tests/web_consent_matrix.rs` was left alone. Its
`enable_permanent` byte tests do not reach the daemon's writer at all: they go
through `FileTierSink`, a test double that does `config.to_toml()` +
`std::fs::write`. Seeding it with a commented config would assert that a *double*
destroys comments — pinning the fake's behaviour as if it were the daemon's,
which is exactly the inversion LESSON-451 warns about. Its neighbour
(`a_setup_commit_enables_the_tier_and_answers_no_consent_question`) runs on
`DaemonRuntime::minimal()` with no config path and asks about candidate bytes,
never a file. The read-back *posture* those tests established is what carried
over: every witness in the new file re-parses the written bytes through
`Config::load` (spec AC-8).

### Verification

- `cargo test -p tetond --test config_preservation` — 9 passed, 0 failed.
- `cargo test -p tetond` — 36 targets, 1546 passed, 0 failed, 1 ignored (the
  pre-existing `live` e2e).
- `cargo test --workspace` — 51 targets, 2444 passed, 0 failed, 1 ignored.
- `cargo clippy -p tetond --all-targets` — no warnings.
- `cargo fmt -p tetond -- --check` — clean.
