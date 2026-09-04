---
id: TASK-390
title: "The shell provenance classifier — a fail-closed allowlist grammar over the command"
status: complete
parent: REQ-614
created: 2026-09-04
updated: 2026-09-04
dependencies: []
---

## Description

A new pure module that answers one question: given a session root, its
`RootKind`, the effective boundary set and a command string, could this
command have read a file the session must not send remotely?

The module has no I/O beyond the filesystem reads needed to resolve a path
and to run the bounded subtree scan, and takes **no exit status** — BR-8's
requirement that a failed command classify exactly like a successful one is
enforced by the signature, not by a branch.

The default answer is `unknown`. `rooted` is reachable only through a path
where every token was understood (ADR-614-1).

## Files to Create/Modify

- `crates/tetond/src/harness/tools/shell_provenance.rs` — new: `ShellProvenanceVerdict`, the verb tables, the tokenizer, `classify`
- `crates/tetond/src/harness/tools/mod.rs` — declare the module

## Acceptance Criteria

- [ ] `ShellProvenanceVerdict` carries `kind` (`Rooted` / `BoundaryTouch` / `Unknown`), `sources: BTreeSet<ProvenanceId>` (empty for the two non-rooted kinds) and a content-free `reason: String` naming why the verdict was reached — no command text, no file content
- [ ] `classify(root, root_kind, boundaries, command)` takes no exit status and no output (BR-8)
- [ ] `RootKind::Home`, `Plain` and `FilesystemRoot` yield `Unknown` (ADR-614-2 / OQ-1)
- [ ] The opaque-verb set is **one pinned table** with a test that enumerates it; at minimum `sh`, `bash`, `zsh`, `python`, `python3`, `node`, `cargo`, `npm`, `make`, `curl`, `wget`, `ssh`, `scp`, `eval`, `xargs`, and `find` when `-exec` is present
- [ ] The name-only verb set is a second pinned table: `ls`, `pwd`, `find` (without `-exec`), `git status`, `git log`, `wc` with `-l` only, `du`
- [ ] Any command containing a quote, `>`, `<`, `>>`, `$`, a backtick, `$(`, or a backslash yields `Unknown` — the grammar refuses what it cannot tokenize the way `sh -c` will
- [ ] A path token resolving **outside** the root yields `BoundaryTouch` when its resolved absolute path (leading `/` stripped) matches a boundary glob, else `Unknown` (BR-1(b), BR-3, AC-5)
- [ ] A path token resolving **inside** the root that matches a boundary glob yields `BoundaryTouch` (BR-1(c))
- [ ] A content-reading verb given a directory, a wildcard, or no path runs the bounded subtree scan; a scan that **hits its walker budget** yields `Unknown`, never `Rooted` (ADR-614-5)
- [ ] An empty boundary set short-circuits before any scan (BR-9) — no walk is performed
- [ ] A differential table of adversarial spellings is a test: `sh -lc`, `bash -ec`, `env sh -c`, `/bin/sh -c`, `cat</etc/passwd`, `ls;curl x`, `xargs cat`, `find . -exec cat {} +` — every one `Unknown` (AC-9)
- [ ] Benign path: `pwd`, `ls -la`, `git status`, `git log -3`, `cat src/main.rs` from a project root with the builtin set in force are all `Rooted` — the classifier must not fire on the legitimate actor

## Verification

| rule | kind | artifact | benign_path |
|------|------|----------|-------------|
| BR-1 | test-case | `crates/tetond/src/harness/tools/shell_provenance.rs::rooted_only_when_every_token_is_understood` | yes |
| BR-3 | test-case | `crates/tetond/src/harness/tools/shell_provenance.rs::a_boundary_path_token_is_a_boundary_touch` | yes |
| BR-8 | test-case | `crates/tetond/src/harness/tools/shell_provenance.rs::the_verdict_takes_no_exit_status` | yes |
| BR-9 | test-case | `crates/tetond/src/harness/tools/shell_provenance.rs::an_empty_boundary_set_short_circuits_before_any_walk` | yes |
| AC-5 | test-case | `crates/tetond/src/harness/tools/shell_provenance.rs::ssh_config_from_a_project_root_is_boundary_touch_not_unknown` | no |
| AC-9 | test-case | `crates/tetond/src/harness/tools/shell_provenance.rs::adversarial_spellings_are_all_unknown` | yes |

## Technical Notes

- **Do not reuse `command_position_programs` as the basis for a `Rooted`
  verdict.** Its documented misses are false negatives, which are safe for
  REQ-607's advisory and unsafe here (ADR-614-1). Reuse it for segment
  splitting only; the decision must additionally require that every token in
  the segment was recognized.
- The AC-5 glob test is load-bearing and must be written **before** the code
  is believed: LESSON-623 is precisely the mistake of assuming a boundary
  glob reaches a path. Assert `**/.ssh/**` matches `Users/x/.ssh/config`
  empirically; if it does not, the stripping rule is wrong and the design
  note in ADR-614-3 must change, not the test.
- The subtree scan uses the existing `walk::WalkPolicy`. Return the
  budget-exhausted signal as a distinct value the caller must handle — do not
  let "found nothing" and "stopped looking" share a `bool`.
- Show each test can fail: invert the verdict default and count what goes
  red; record the number in the module's doc comment (conventions.md,
  LESSON-569).
- **Match against every row of `effective_boundaries()`, with no
  `BoundaryMode` branch.** The spec says "a `local-only` boundary glob", and
  `LocalOnly` is the only mode in use — `RedactThenRemote` is declared
  post-MVP and unimplemented. `BoundaryMatcher::match_path` does not branch
  on mode and neither does the egress inspector; matching all rows is both
  consistent with them and the fail-closed direction. A mode branch here
  would be a second opinion about a question the matcher already answers.
- The subtree scan takes its budget from the `WalkPolicy` already on
  `ToolContext` (`walk`, tools/mod.rs:167) — passed in as a parameter, so the
  module stays pure policy and testable without a context.
