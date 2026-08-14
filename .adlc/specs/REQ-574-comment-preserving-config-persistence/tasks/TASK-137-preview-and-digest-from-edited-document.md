---
id: TASK-137
title: "Preview, digest, and warnings derive from the edited document"
status: draft
parent: REQ-574
created: 2026-08-14
updated: 2026-08-14
dependencies: ["TASK-136"]
repo: teton-code
---

## Description

Make the `/web setup` preview show — and the commit write — the same edited
bytes (ADR-3, LESSON-451). One shared derivation helper feeds both:
`web_setup_preview` returns the `[web]` section sliced from the edited
document and a digest of the full edited text; `web_setup_commit` re-derives
through the same helper, compares digests, and writes that exact text. The
now-false unconditional rewrite warning is removed with its pinning test.

## Files to Create/Modify

- `crates/tetond/src/runtime.rs` — NEW `render_persisted_document(path, current, candidate) -> Result<(String /*full*/, String /*web section*/), ...>` shared by preview and commit; `candidate_digest` becomes sha256 over the edited text (rename/re-doc as needed, ~6642); `web_setup_preview` (~3957) slices `toml` via `config_doc::table_section` and reads the on-disk doc; `web_setup_commit` (~4022) re-derives + digest-compares + persists the derived text; `web_setup_warnings` (~6749) — remove the unconditional first warning; DELETE test `every_preview_says_the_save_rewrites_the_whole_file` (~16228); STRENGTHEN `a_preview_renders_the_bytes_the_commit_goes_on_to_write` (~15570) with a commented seed config; keep `the_digest_covers_the_whole_document_not_the_rendered_table` (~16526) and `a_commit_whose_document_moved_under_the_preview_is_refused` (~16426) passing under the new derivation
- `crates/teton-core/src/config.rs` — retire `web_table_toml` (~531) if nothing but the old preview path uses it; otherwise document why it stays

## Acceptance Criteria

- [ ] Preview `toml` field == the `[web]` section of the file the subsequent commit writes, byte-for-byte, including user comments inside `[web]` (spec AC-3)
- [ ] Preview `digest` == sha256 of the full written file bytes (spec AC-3)
- [ ] Preview has no write path: derivation reads the document but writes nothing (existing contract kept)
- [ ] A comment-only hand edit between preview and commit → commit refuses with the existing `SETUP_DIGEST_STALE` message; nothing written (spec AC-4 — new test)
- [ ] `an_answer_that_omits_a_key_removes_it_and_says_so` (~15967) passes: omitted answers remove keys (and their attached comments) through the delta path
- [ ] The unconditional rewrite warning is gone; remaining conditional warnings unaffected (spec AC-7); no CLI-side assertion on the removed string remains (check crates/teton/src/web_setup_ui.rs tests and cli_e2e)
- [ ] Concurrency test `two_concurrent_commits_leave_one_candidates_state_and_one_notice_each` (~16111) passes (derivation + write still serialized under the config mutex)
- [ ] `a_commit_with_no_digest_is_checked_against_nothing_and_still_lands` (~16494) passes unchanged
- [ ] `cargo test -p tetond` (default features) green

## Technical Notes

- Preview must read the on-disk document (new, deliberate I/O in the preview
  path) under the config lock so preview and commit see the same base absent
  interleaved writes — interleaved writes are exactly what the digest catches.
- Commit hands the ALREADY-derived text to the writer rather than re-deriving
  inside `persist_config` a second time — avoid double file reads producing a
  digest-checked text and a differently-based written text (TOCTOU inside the
  daemon itself). Suggested shape: `persist_config` gains a variant that
  accepts pre-rendered text + runs validation, or commit validates and calls
  `write_config_atomically(path, &text)` directly — keep "one write body"
  true either way (spec BR-2).
- Missing file at preview time: base is the empty document (ADR-1); preview
  still renders and digests what a commit would create.
