---
id: TASK-408
title: "Docs: ADR-614-1 amendment carried to the architecture index, CHANGELOG entry, and the toolkit conventions follow-up"
status: complete
parent: REQ-620
created: 2026-09-09
updated: 2026-09-09
dependencies: ["TASK-406", "TASK-407"]
repo: teton-code
---

## Description

Record what shipped where readers look for it: the project architecture index, the
changelog's upgrade notes (the prompt-margin raise changes every redact-scanning route's
budget), and a filed follow-up for adlc-toolkit's `conventions.md`, whose preamble grammar
paragraph (BUG-220) says pipes into readers and `2>/dev/null` pin — true until this REQ,
false after.

## Files to Create/Modify

- `.adlc/context/architecture.md` — the bold lead-in under ADR-614-1 (text in `architecture.md` of this REQ, "Proposed additions")
- `CHANGELOG.md` — `## [Unreleased]` → Added: the grammar widening and the contract; Changed: `REDACT_BODY_OVERHEAD_BYTES` 23 → 24 KiB and what that does to redact-scanning routes
- `docs/manual-verification.md` — a row for the cleared-command check (the 2026-09-09 command, minus the `~/bin` probe, on a remote route)
- `.adlc/specs/REQ-620-harmless-redirects-in-the-shell-grammar/requirement.md` — Deferred section if any AC was descoped

## Acceptance Criteria

- [x] The architecture index's ADR-614-1 names the three amendments and the date. **Where it landed, and why not literally "inside ADR-614-1":** `.adlc/context/architecture.md` has no ADR-614-1 block — its `## ADRs` section holds the nine project-wide ADRs (ADR-001..009), and every REQ-scoped ADR is carried in the key-decisions bullet list above it. The amendment is therefore a new bullet at the end of that list, opening `**REQ-620 amends ADR-614-1's consequence list (2026-09-09).**` and naming (a) the strip before the unmodelled scan and the split, (b) the position-aware splitter and the piped-stdin rule, (c) the class-naming scan, plus the tool contract and the ceiling raise. ADR-614-1 *itself* — in `.adlc/specs/REQ-614-proportionate-shell-provenance/architecture.md`, where TASK-403 had already added the one-sentence lead-in — is extended with the same (b) and (c), so the ADR and the index say the same thing.
- [x] The changelog entry states the upgrade consequence of the margin raise in one sentence (`### Changed`, with an explicit **Upgrade note**: every `[privacy] redact = true` route scans 931 fewer bytes of context, 184,265 → 183,334, and nothing else moves). A **second** upgrade note sits under the grammar entry, because the widening changes *where a session's turns run* — a session that used to fall back to the local tier on a model-written `2>/dev/null` now stays on its remote provider — and the file's own preamble says that is exactly what belongs here.
- [ ] A toolkit issue or BUG is filed (`adlc_alloc_id bug` in the toolkit repo) to loosen `conventions.md`'s "no `| head`, no `2>/dev/null`" rule to "on Teton Code ≥ the release carrying REQ-620"; its id is recorded in this task. **Not discharged here, by instruction:** this task does not edit the adlc-toolkit repo, so no toolkit id was allocated from it. The follow-up's text is drafted verbatim under *Toolkit follow-up (to file)* below, and the orchestrator files it in that repo; the box stays unticked until it does.
- [x] BR-5 in the requirement reworded to match ADR-620-2's order — the strip runs first and cannot change the verb the opaque check reads; every observable is unchanged (TASK-403 finding)
- [x] BR-4 in the requirement reworded: a piped content verb that names no *existing* file reads its stdin (a pattern word is not a path; TASK-404 finding)
- [x] AC-6 in the requirement reworded: the marker appears in no event other than the two that quote the command by REQ-611 BR-4 design (`tool_call` title, `permission_request`), named in the test (TASK-405 finding)
- [x] A Deferred section in the requirement records that a *typed* `/skill` pin carries no class (`Provenance::User` has no reason field, `skill_invoked.reach_reason` does), with a follow-up BUG or REQ id allocated via the partial — **BUG-223**, `.adlc/bugs/BUG-223-a-typed-skill-pin-carries-no-syntax-class.md` (`status: open`, `severity: low`, `introduced_by: ["REQ-620"]`). The section also records the glued-redirect misses and AC-1's unit-level discharge.
- [x] No source file changed by this task — this commit touches `.md` files only, and `cargo test -p tetond --lib shell_provenance` was re-run after the edits (34 passed, 0 failed, 2,207 filtered out).

## Verification

| rule | kind | artifact | benign_path |
|------|------|----------|-------------|
| AC-9 | structural-check | `cargo test -p tetond --lib shell_provenance` (the toolkit preamble table stays green) | no |

## Technical Notes

- House style for ADR extensions: a bold lead-in with the REQ id and date inside the
  parent ADR block, not a new ADR number in the index.
- The toolkit follow-up is filed, not done, from this repo: `~/.claude/skills` symlinks to
  the toolkit checkout, and its conventions file is that repo's to change.

## What landed

Documentation only — **no source file changed**, and no test was added or
edited. The verification obligation is the one in the table above:
`cargo test -p tetond --lib shell_provenance` re-run after these edits, 34
passed / 0 failed.

- `.adlc/context/architecture.md` — a new key-decisions bullet,
  `**REQ-620 amends ADR-614-1's consequence list (2026-09-09).**`, and a
  `**REQ-620 is the claimant that raised the ceiling (2026-09-09).**`
  paragraph appended to the `REDACT_BODY_OVERHEAD_BYTES` ledger chain (REQ-612
  → REQ-617 → ASSUME-044), which carried the now-stale 105/152 margins and the
  now-false "the `shell` tool's description says nothing about the pin".
- `.adlc/specs/REQ-614-proportionate-shell-provenance/architecture.md` —
  TASK-403's lead-in inside ADR-614-1 extended with the position-aware split
  and the class-naming scan, so the ADR and the index agree.
- `CHANGELOG.md` — `## [Unreleased]` gains `### Added` (three entries: the
  grammar widening with its own upgrade note, the class-named pin reason as an
  additive wire field at an unchanged protocol version, and the shell tool's
  reach contract) and `### Changed` (the 23 → 24 KiB ceiling with the
  931-byte upgrade note). `### Fixed` (BUG-221, BUG-222) is untouched.
- `docs/manual-verification.md` — a REQ-620 runbook at the end: one leg running
  the 2026-09-09 command minus its `~/bin` probe on a remote route (expect no
  `session pinned` line and the next turn on the provider), one control leg
  adding the probe back (expect a pin naming the path, liftable by
  `/shell allow`), and a short sign-off block.
- `.adlc/specs/REQ-620-.../requirement.md` — BR-4, BR-5 and AC-6 reworded to
  what shipped; every BR and AC ticked; a `## Deferred` section before
  `## Retrieved Context`.
- `.adlc/bugs/BUG-223-a-typed-skill-pin-carries-no-syntax-class.md` — new,
  `status: open`.

## Toolkit follow-up (to file)

**Not filed from this repo.** `~/.claude/skills` symlinks to the adlc-toolkit
checkout and its conventions file is that repo's to change, so the text below
is the follow-up, verbatim, for the orchestrator to file there (allocating the
id with `adlc_alloc_id bug` **from that repo**).

---

**Title:** The preamble-grammar paragraph names two rules Teton Code lifted in
REQ-620

**Severity:** low. **Component:** `.adlc/context/conventions.md` (the BUG-220
preamble-grammar paragraph).

`conventions.md`'s BUG-220 paragraph — "Every `` !`…` `` preamble stays inside
the same grammar" — tells skill authors two things that stop being true of a
Teton Code host from the release carrying REQ-620 onward:

1. **that `2>/dev/null` is enough to pin.** REQ-620 lifts a redirect that reads
   nothing out of the command *before* it classifies the rest: `2>/dev/null`,
   `>/dev/null`, `1>/dev/null`, `2>>/dev/null`, `&>/dev/null`, `</dev/null`,
   `2>&1` and `1>&2`, each as a whole whitespace-delimited word (or the
   operator word immediately followed by a separate `/dev/null` word). A form
   glued to its verb (`ls>/dev/null`) or to a following command (`2>&1;ls`) is
   still refused, and every other use of `>` and `<` still refuses the whole
   command.
2. **that `| head`, `| tail` or `| grep` after a producer pins.** REQ-620 reads
   a content verb after a single `|` that names no existing file as taking its
   stdin from the previous segment — which was itself classified — so
   `ls .adlc/partials/ | head` and `git log | grep fix` are `rooted`. Recursive
   `grep` (`-r`, `-R`, `--recursive`, `-d recurse`, or a short-flag cluster
   containing `r`/`R`) still reads the tree whatever its stdin, and still pins.

**The ask:** scope **those two rules only** with "on Teton Code < the release
carrying REQ-620", and keep the rest of the paragraph exactly as it stands — no
quoting, no globs, no `$`, no parentheses or `;`, recognised verbs only, in-root
paths only (a `~/…` fallback in *any* segment still makes the whole line
unprovable even when it never runs), and the missing-file rule: a content verb
whose file argument does **not** exist is still scored as a read of the whole
root in a **first** segment, so the `cat .adlc/context/…` lines still pin on a
project that has not run `/init`. `/canary`'s `gcloud` lines and
`/template-drift`'s toolkit listing remain the known exceptions.

**Why scope rather than delete:** the narrow forms are correct on *both* host
versions, and a skill's preamble runs on whatever host the consumer has. The
paragraph should keep recommending them and mark the two rules as
version-scoped, so an author reading it on an older host is not misled in the
unsafe direction.

**Teton-code side:** REQ-620 BR-1, BR-2 and BR-4
(`.adlc/specs/REQ-620-harmless-redirects-in-the-shell-grammar/`), shipped in
`crates/tetond/src/harness/tools/shell_syntax.rs` and `shell_provenance.rs`.
The classifier's own toolkit-preamble table
(`the_toolkit_preamble_shapes_are_rooted_and_the_old_ones_are_not`) is the test
that will need a row per rule this paragraph relaxes.

---
