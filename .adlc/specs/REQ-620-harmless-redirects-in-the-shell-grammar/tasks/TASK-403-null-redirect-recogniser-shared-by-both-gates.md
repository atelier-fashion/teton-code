---
id: TASK-403
title: "One null-redirect recogniser, shared by the write gate and the classifier, with the differential table"
status: complete
parent: REQ-620
created: 2026-09-09
updated: 2026-09-09
dependencies: []
repo: teton-code
---

## Description

Promote `root_gate.rs`'s reading of `2>&1` / `>/dev/null` into a shared module, and make
`shell_provenance::classify_with_budget` strip those forms as whole words before the
unmodelled scan and before the segment split (ADR-620-1, ADR-620-2 steps 1–2). `/dev/null`
never becomes a path token. Land the differential table that pins every accepted form,
every look-alike, the `&`-bearing forms beside `&&`/`||`/lone `&`, opaque verbs with
redirects, and a boundary read with a redirect.

## Files to Create/Modify

- `crates/tetond/src/harness/tools/shell_syntax.rs` — new: `NullRedirect` (forms per the REQ's entity), `NullRedirect::parse(word)`, `strip_null_redirects(command) -> Stripped { residue, lifted }`; consumes the spaced form's `/dev/null` follower
- `crates/tetond/src/harness/tools/mod.rs` — declare the module
- `crates/tetond/src/harness/root_gate.rs` — `has_top_level_redirection` reads `NullRedirect::parse` instead of its inline cases; its benign table unchanged and still green
- `crates/tetond/src/harness/tools/shell_provenance.rs` — `classify_with_budget`: strip first, then `UNMODELLED` scan over the residue, then split; module docs and the mutation record updated; the differential table test
- `.adlc/specs/REQ-614-proportionate-shell-provenance/architecture.md` — ADR-614-1 consequence list amended (bold lead-in, dated)

## Acceptance Criteria

- [ ] Every BR-1 form, attached and spaced, on `ls`, `cat README.md`, `git status`, `test -s x`, `echo hi` → `Rooted`
- [ ] Every BR-2 look-alike on the same verbs → `Unknown` with the redirect-class reason (the class sentence lands in TASK-405; until then the existing single sentence)
- [ ] `ls 2>&1 && echo ok`, `ls &>/dev/null || echo no`, `ls 2>&1; ls` → `Rooted`; `ls & ls` still classifies as two segments
- [ ] `python x.py 2>/dev/null`, `curl example.com >/dev/null 2>&1`, `sh -c ls 2>/dev/null` → `Unknown` with the opaque-verb reason
- [ ] `cat secrets/prod.env 2>/dev/null` and `2>/dev/null cat secrets/prod.env` → `BoundaryTouch` on a root whose boundaries cover the path
- [ ] The 2026-09-09 command from the REQ, minus its `~/bin` segment → `Rooted`; with it → `Unknown` naming a path outside the root
- [ ] `the_write_gate_refuses_both_triggers_and_nothing_benign` unchanged and green; `the_toolkit_preamble_shapes_are_rooted_and_the_old_ones_are_not` green (its old rows carry quotes, so none flip)
- [ ] `cargo clippy --workspace --all-targets -- -D warnings`, `cargo fmt --all -- --check` clean

## Verification

| rule | kind | artifact | benign_path |
|------|------|----------|-------------|
| BR-1 | test-case | `crates/tetond/src/harness/tools/shell_provenance.rs::tests::null_redirects_are_lifted_before_the_scan_and_the_split` | yes |
| BR-2 | test-case | `crates/tetond/src/harness/tools/shell_provenance.rs::tests::every_other_redirect_stays_unmodelled` | yes |
| BR-3 | test-case | `crates/tetond/src/harness/tools/shell_provenance.rs::tests::dev_null_is_never_a_path_token` | no |
| BR-5 | test-case | `crates/tetond/src/harness/tools/shell_provenance.rs::tests::an_opaque_verb_with_a_null_redirect_is_still_unknown` | yes |
| BR-8 | test-case | `crates/tetond/src/harness/tools/shell_provenance.rs::tests::a_redirect_never_hides_a_boundary_read` | yes |
| BR-10 | test-case | `crates/tetond/src/harness/tools/shell_provenance.rs::tests::null_redirects_are_lifted_before_the_scan_and_the_split` | no |
| AC-1 | test-case | `crates/tetond/src/harness/tools/shell_provenance.rs::tests::the_2026_09_09_command_is_rooted_without_its_home_probe` | yes |
| AC-2 | test-case | `crates/tetond/src/harness/tools/shell_provenance.rs::tests::the_redirect_differential_table` | yes |
| AC-7 | test-case | `crates/tetond/src/harness/tools/shell_provenance.rs::tests::an_opaque_verb_with_a_null_redirect_is_still_unknown` | no |
| AC-9 | test-case | `crates/tetond/src/harness/tools/shell_provenance.rs::tests::the_toolkit_preamble_shapes_are_rooted_and_the_old_ones_are_not` | yes |

## Technical Notes

- Strip is whole-word only: split on whitespace, lift words `NullRedirect::parse` accepts,
  and for a bare operator word (`2>`, `>`, `>>`, `&>`, `<`) lift it only when the next word
  is exactly `/dev/null`; otherwise leave both words for the unmodelled scan.
- The residue must keep the original separators (`&&`, `||`, `;`, `|`, `&`) intact for
  TASK-404's splitter; rebuild it by joining the kept words with single spaces — the
  classifier already splits on whitespace, so byte fidelity is not required, but separator
  tokens must survive as their own words (`ls 2>&1 && echo` → `ls && echo`).
- `BoundaryTouch` precedence (BUG-216) is untouched: the strip runs before verbs are read,
  so the boundary path is still resolved by `classify_segment`.
- LESSON-494: the recogniser is the *only* place that spells `/dev/null`; grep the crate
  for the literal after the change and expect exactly this module, its tests, and docs.
- LESSON-550: the differential table asserts the verdict *and* that the old single
  sentence is absent for accepted forms.
