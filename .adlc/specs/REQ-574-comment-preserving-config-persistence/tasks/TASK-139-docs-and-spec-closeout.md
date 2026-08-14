---
id: TASK-139
title: "Docs: README hand-edit section, drift note, spec closeout"
status: complete
parent: REQ-574
created: 2026-08-14
updated: 2026-08-14
dependencies: ["TASK-137"]
repo: teton-code
---

## Description

Retire the documentation debt this REQ exists to fix: the README teaches a
commented hand-written `[web]` block next to `/web setup`; with preservation
in place the two stop being in tension and the docs say so. Check every other
doc surface that describes the rewrite behavior, then tick the spec's
acceptance boxes.

## Files to Create/Modify

- `README.md` — the "Or write the table by hand" section (~347-372): state that daemon-side saves preserve comments, unknown keys, and ordering (replacing any residual rewrite caveat); verify the drift-check note (~334-346) still names only surfaces that exist after TASK-137 (it names `web_setup_ui.rs` constants and the contract suite — confirm no reference to removed warning text)
- `crates/tetond/src/harness/self_config.md` — check for any description of whole-file rewrite behavior; update if present, note "not present" if absent
- `.adlc/specs/REQ-574-comment-preserving-config-persistence/requirement.md` — tick satisfied AC checkboxes; status flip to `complete` happens at Phase 6, not here

## Acceptance Criteria

- [x] README no longer warns (anywhere) that daemon-side saves destroy comments or unknown keys; the hand-written block section reflects preservation — the "Or write the table by hand" intro now names the four daemon-side savers and states the in-place edit, and a second paragraph after the block states the edit base (BR-5) and the parse/validate refusal (BR-4/BR-6). The fenced TOML is byte-untouched (it is TASK-135's fixture)
- [x] The README drift-check triple (backend rows / `[web]` keys / `web_setup_ui.rs` constants) still holds — no stale reference to the removed warning; `ENDPOINT_HELP` (`web_setup_ui.rs:817`) and `instruction_lines` (`:1032`) both still exist under those names, and `web_setup_contracts.rs` still reads `self_config.md` via `include_str!`. It is now a quartet: `crates/teton-core/src/config_doc.rs`'s `HAND_WRITTEN_CONFIG` was added as the fourth surface (TASK-135's flagged follow-up)
- [x] `self_config.md` reviewed; updated or explicitly confirmed clean — **confirmed clean**: it describes what `/web setup` writes (`[web]` keys, `search_auth` shapes, keychain reference) and never claims anything about how the file is rewritten. No edit made
- [x] Spec AC checkboxes updated to match reality (each box ticked only if its test exists and passes) — ticked AC-4, AC-6, AC-7, AC-9 and BR-2, BR-3, BR-6, BR-7; left AC-1, AC-2, AC-3, AC-5, AC-8, AC-10 and BR-1, BR-4, BR-5 unticked. Rationale per line in the notes below
- [x] `rg -n "rewrites the whole config" --hidden` over the repo returns no hits outside `.adlc/` history artifacts — one hit remains and it must: `crates/tetond/src/runtime.rs:16794`, inside TASK-137's `no_preview_claims_the_save_rewrites_the_whole_file`, which asserts the string's **absence** from every preview's warnings. That is the pin, not a claim; deleting it would delete the guard

## Technical Notes

- Doc-only task plus spec bookkeeping — no code. Runs after TASK-137 so the
  docs describe shipped behavior, in parallel with TASK-138.

## Implementation Notes (post-implementation)

**A fourth doc surface was edited, beyond the task's file list.**
`docs/manual-verification.md`'s `/web setup` keychain walkthrough (the AC-5/AC-6
"not automated" section) is the one manual procedure that runs a real commit
against a real file, and TASK-137 flagged it as understating what now happens.
Its setup step now seeds the scratch config by hand with a comment, a comment
inside `[web]` and an unknown key; step 3 expects those comments **in the
preview** (BR-3 — the preview is sliced from the document that lands, not
re-rendered); step 4 gains two `grep`s that the comment and the unknown key
survived the write. That makes the manual half of AC-1 observable by the person
running the flow, which no automated test can do against the real keychain.

**Spec bookkeeping, line by line.** Ticked only where a committed, passing
witness exists (TASK-135 `2006203`, TASK-136 `aae366a`, TASK-137 `0176d46`, each
reporting `cargo test -p tetond` green):

- AC-4 — `a_comment_only_hand_edit_between_preview_and_commit_is_refused`
  (`runtime.rs:16827`, TASK-137).
- AC-6 — `a_missing_file_is_written_fresh_at_owner_only_and_parses_back_to_the_candidate`
  (`runtime.rs:7884`, TASK-136).
- AC-7 — `no_preview_claims_the_save_rewrites_the_whole_file`
  (`runtime.rs:16777`, TASK-137), plus its falsification leg (a clean candidate
  now draws an empty warning list).
- AC-9 — `rewriting_the_config_preserves_its_permissions` (`:7726`), the
  readonly-dir migration leg (`:7688`) and
  `two_concurrent_commits_leave_one_candidates_state_and_one_notice_each`
  (`:16660`). "Unchanged" is true of the mechanic, not of one call line: the
  permissions test's third leg renders text inline because the writer's
  signature changed (TASK-136 recorded this).
- BR-2, BR-3, BR-6, BR-7 — one derivation + one write body (TASK-136/137), the
  preview/digest rewiring, the loud refusal on an unparseable document, and this
  task's doc retirement of the disclosure.

Left unticked, each because its witness is not committed yet — **TASK-138 was
running concurrently with this task and its witnesses are not in this branch at
the time of writing**; the Phase 5 verify pass should re-reconcile the list:

- AC-1, AC-2 — the five-writer preservation suite with the README block
  verbatim is TASK-138's whole subject. Preservation is witnessed once today, at
  `persist_web_tier` (`runtime.rs:~7900`), and at the delta engine against the
  README block (`config_doc.rs`), but not per writer.
- AC-3 — the runtime.rs half is done and asserted
  (`a_preview_renders_the_bytes_the_commit_goes_on_to_write`); the AC also names
  the `web_consent_matrix.rs` TASK-129 pin, and that file was untouched by
  `0176d46` (TASK-138 lists it).
- AC-5 — seam + `persist_web_tier` + migration warn-and-continue are pinned;
  `web_setup_commit` and `apply_config_update` refusals are TASK-138's.
- AC-8 — the read-back posture across every witness is TASK-138's.
- AC-10 — `a_hand_edit_that_fails_validation_refuses_the_write_and_survives_it`
  (`:7987`) pins the `persist_web_tier` half; the `web_setup_commit` half is
  TASK-138's.
- BR-1, BR-4, BR-5 — each is a claim about *all five* writers (BR-4 also owns
  the read-back-equivalence contract that AC-8 witnesses), so they follow
  AC-1/AC-2/AC-8 rather than leading them.

**Not done here, flagged instead.** OQ-1/OQ-2/OQ-3 are answered by the
architecture (ADR-1's attachment and set/remove rules, the current-vs-candidate
diff at the seam, and OQ-3's "keep the in-memory no-op check" recorded in
TASK-136's notes), but their checkboxes are left for the phase that owns closing
open questions rather than ticked from a doc task.

**`status` was not flipped on the requirement** (Phase 6 owns that), and its
`updated:` was already today's date.
