---
id: TASK-137
title: "Preview, digest, and warnings derive from the edited document"
status: complete
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

- [x] Preview `toml` field == the `[web]` section of the file the subsequent commit writes, byte-for-byte, including user comments inside `[web]` (spec AC-3) — `a_preview_renders_the_bytes_the_commit_goes_on_to_write`, now seeded with a hand-written commented config; the preview is asserted to carry the seed's comment block, its inline comment and its unknown key
- [x] Preview `digest` == sha256 of the full written file bytes (spec AC-3) — same test, second assertion
- [x] Preview has no write path: derivation reads the document but writes nothing (existing contract kept) — `render_config_document` performs one `read_to_string`; `web_setup_flow.rs` leg (3) still asserts the file byte-identical after a preview
- [x] A comment-only hand edit between preview and commit → commit refuses with the existing `SETUP_DIGEST_STALE` message; nothing written (spec AC-4 — new test) — `a_comment_only_hand_edit_between_preview_and_commit_is_refused`, with a non-vacuity leg proving the edit is invisible to the schema and a third leg proving the check is a detector, not a wedge
- [x] `an_answer_that_omits_a_key_removes_it_and_says_so` passes: omitted answers remove keys (and their attached comments) through the delta path — passes; its section assertion now goes through `table_section`
- [x] The unconditional rewrite warning is gone; remaining conditional warnings unaffected (spec AC-7); no CLI-side assertion on the removed string remains — the string exists nowhere in `crates/`; `no_preview_claims_the_save_rewrites_the_whole_file` replaces the pinning test with its inverse
- [x] Concurrency test `two_concurrent_commits_leave_one_candidates_state_and_one_notice_each` passes (derivation + write still serialized under the config mutex)
- [x] `a_commit_with_no_digest_is_checked_against_nothing_and_still_lands` passes unchanged
- [x] `cargo test -p tetond` (default features) green — 1253 lib + 35 integration targets, 0 failed

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

## Implementation Notes (post-implementation)

**The seam is now three named pieces, not two.** `persist_config` split into
`render_config_document(path: Option<&Path>, current, candidate) -> Result<String,
RenderError>` (read, delta, validate) and the unchanged `write_config_atomically`.
`persist_config` is the two of them in order, for the four writers that just want
the file updated; `web_setup_commit` is the one caller that splits them, because
it has to digest the derived text and check it against the confirmed preview
before writing. `render_persisted_document` wraps the derivation for the RPC
seams and adds the `[web]` slice and the digest. So there is one derivation and
one write body, reached two ways (BR-2). `candidate_digest` is gone — the digest
is now a field of the derivation's result, which is what stops it from being
taken over something other than the bytes that land.

`RenderError` is typed (not `anyhow`) because preview and commit have to
*classify*: a document that would not load is `WEB_SETUP_INVALID` carrying the
validator's sentence, everything else is `INTERNAL_ERROR`. Each variant's
`Display` reproduces TASK-136's sentences byte-for-byte, so `persist_web_tier`'s
pinned messages are unchanged.

**Three behaviour changes worth a reviewer's eye:**

1. **The commit derives unconditionally, before the no-op short-circuit.** It has
   to: the digest check sits before the short-circuit by design, and deriving
   twice is the TOCTOU this task exists to avoid. Consequence: a commit whose
   candidate already matches the live config now reads and validates the document
   instead of returning `applied: false` blind. On a *broken* document such a
   commit refuses where it used to report `applied: false`. Judged the better
   answer — "your config already says this" is not something the daemon can
   honestly assert about a file it cannot parse — but it is a change.
2. **`render_persisted_document` errors when the derived document names no
   `[web]` table.** Reachability is exact: the delta only ever touches `[web]`
   keys, so an absent section means an *empty* delta, which means
   `candidate == current`, which is precisely the commit's no-op case. It is
   therefore reachable only by hand-deleting `[web]` from the file while the
   daemon runs and then re-running `/web setup` with the answers already in
   memory. For the preview that error is the honest answer (there is nothing to
   show); for the commit it lands in case (1) above. The alternative —
   `web_section: Option<String>`, with only the preview refusing — was rejected
   to keep the helper's shape the one ADR-3 names, and is the one call here I
   would happily see revisited.
3. **The preview holds the config mutex across a file read.** It used to clone
   the config and drop the lock immediately. The lock now spans candidate
   construction, the read, the delta and the digest, matching what the commit
   already did — otherwise a commit could land between the base a preview read
   and the digest it took over it, and the daemon would have manufactured its own
   race. The critical section gains one `read_to_string` of a small file.

**Two tests adapted, both because they pinned whole-file re-serialization:**

- `the_digest_covers_the_whole_document_not_the_rendered_table` moved its "a
  change the `[web]` section does not show" from an in-memory `privacy.redact`
  flip to a write of the file. Under the old seam the commit re-serialized
  memory, so an in-memory flip moved the bytes that would land; now the document's
  own `[privacy]` table rides through untouched (BR-5), so an in-memory flip moves
  nothing. Same claim, asked of the seam that now exists — and the falsification
  half (identical `toml`, different digest) is unchanged.
- `every_preview_says_the_save_rewrites_the_whole_file` was deleted per the task
  and replaced by `no_preview_claims_the_save_rewrites_the_whole_file`, which
  asserts the absence *and* that a clean fetch candidate now draws an empty
  warning list — the falsification that "unconditional" is really gone.

**`web_table_toml` is retired**, with its one-key `WebTableDocument` and its
`teton_core` crate-root re-export. After the preview stopped calling it, the only
callers were two of its own tests. Both were adapted rather than deleted, because
their fixtures are worth keeping: `the_web_table_renderer_reproduces_the_documents
_section_byte_for_byte` became `the_sliced_web_section_is_the_documents_own_bytes`
(same three realistic `WebConfig` shapes, now asking whether `table_section`
returns the document's own bytes, cross-checked against an independent line
walk), and `an_unset_web_table_is_the_one_section_the_document_omits` keeps its
name and now asserts `table_section` returns `None` — which is exactly the state
note (2) above is about. A comment at the old site records why the function is
gone rather than leaving a silent hole.

**Flags for the remaining tasks.**

- *TASK-138*: preview/commit is witnessed here; the five-writer preservation
  suite with the README's `[web]` block verbatim is still TASK-138's. Note the
  new `COMMENTED_SEED` fixture in `runtime::tests::web_setup_flow` is a
  *paraphrase* of that block, not the block itself — AC-1 wants the real one.
- *TASK-139*: `rg "rewrites the whole config"` over `crates/` and `docs/` is
  already clean. Two doc surfaces now understate what happens:
  `docs/manual-verification.md` step 3 of the `/web setup` walk says "the preview
  shows the exact `[web]` table" — true, and now worth asking the verifier to run
  it against a *commented* config and confirm the comments appear in the preview
  and survive the write. And `web_setup_ui.rs`'s `commit_params` doc was
  repointed from `candidate_digest` to `render_persisted_document`; the README's
  drift-check note TASK-135 flagged is still open.
- Not gated, and pre-existing: `cargo doc` reports broken/private intra-doc links
  throughout this workspace (including `teton-core`'s `tetond::lifetime` on
  `main`). The new links (`render_persisted_document`, `write_config_atomically`
  from public items) are private-item warnings of the same class the file already
  carried for `persist_config` and `candidate_digest`.
