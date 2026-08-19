---
id: TASK-176
title: "One walk policy: bounded glob/grep with directory matches, home-media pruning, unreadable reporting, harness trailers; shell timeout hint; tool docs say session root"
status: draft
parent: REQ-583
created: 2026-08-18
updated: 2026-08-18
dependencies: ["TASK-175"]
---

## Description

Leg C (BR-9..BR-14, AC-6, AC-13..AC-19) per `architecture.md` ADR-3. Create
`harness/tools/walk.rs` as the single owner of the skip set, the home-top-level
set, the media bundle suffixes, the budget, the walk driver, and the harness
trailer lines; make `glob` and `grep` use it; make `glob` return directories;
make grep's trailer splitter peel every harness line; give `shell`'s timeout
message the BR-14 sentence; reword the five tool descriptions off
"repository". **File ownership (parallel tier):** this task owns
`harness/tools/{walk.rs, glob.rs, grep.rs, shell.rs, read.rs, edit.rs}` and
`tests/boundary_coverage.rs`. It must not edit `tools/mod.rs` beyond adding
`pub mod walk;` + the `WalkPolicy` slot on `ToolContext` (`with_walk_budget`
seam, default policy in `new`/`for_root`), `turn_loop.rs`, `redact.rs`,
`web.rs`, `server.rs`, `runtime.rs`, or `sessions.rs`.

## Files to Create/Modify

- `crates/tetond/src/harness/tools/walk.rs` — NEW. `pub const WALK_SKIP_DIRS: &[&str] = &[".git", "target", "node_modules", ".hg", ".svn", "__pycache__"]`; `pub const HOME_TOP_LEVEL_SKIPS: &[&str] = &["Library", "Music", "Pictures", "Movies", ".Trash", ".cache", ".cargo", ".npm", ".rustup", ".gradle", ".m2", ".nvm"]`; `pub const MEDIA_BUNDLE_SUFFIXES: &[&str] = &[".photoslibrary", ".musiclibrary"]`; `pub struct WalkBudget { pub max_entries: usize, pub max_wall: Duration }` (`Default` = 100_000 / 10 s); `pub struct WalkPolicy { skip: &'static [&'static str], home_top: …, bundles: …, budget: WalkBudget }` (`Default`); `pub enum TruncatedBy { Entries(usize), WallClock(Duration) }`; `pub struct WalkReport { pub truncated_by: Option<TruncatedBy>, pub unreadable: Vec<String>, pub unreadable_total: usize }`; `pub fn visit(root: &Path, kind: RootKind, named_prefix: &[&str], policy: &WalkPolicy, on_entry: &mut dyn FnMut(&Path, &fs::FileType, &ProvenanceId)) -> WalkReport` — recursion, `skip_symlink_entry`, budget checks per directory (entries counted = every dir entry seen), pruning: `WALK_SKIP_DIRS` by name anywhere; `HOME_TOP_LEVEL_SKIPS` only when the *parent* directory is a home directory (`kind == Home` → parent == root; `kind == FilesystemRoot` → parent's parent == root and parent's name ∈ {`Users`, `home`}); bundle suffixes anywhere; a pruned dir is still entered when `named_prefix` (root-relative segments) starts with the dir's segments; `read_dir` `Err(PermissionDenied)` → push display path (root-relative, `/`-terminated, capped at 5 named + total count), other errors counted in the total only. `pub fn trailer_lines(report: &WalkReport) -> Vec<String>` — `... (stopped after {n} entries; narrow the pattern, or move the session root with /cd)`, `... (stopped after {s} s; …)`, `... ({n} folder(s) could not be read (permission denied): a/, b/ and {k} more)` + on macOS (`cfg!(target_os = "macos")`) ` — macOS may have blocked access to that folder, or be waiting on a consent dialog for it`; `pub fn is_harness_line(line: &str) -> bool` (`starts_with("... (")`). `pub fn leading_literal_segments(pattern: &str) -> Vec<&str>` (segments before the first one containing `*`/`?`). Unit tests here for the pruning rules, budget, unreadable (mode `000`, skip as root), `named_prefix` override, bundle suffix at depth.
- `crates/tetond/src/harness/tools/mod.rs` — ONLY: `pub mod walk;`, a `walk: WalkPolicy` field on `ToolContext` defaulted in `new`/`for_root`, `pub fn with_walk_budget(self, WalkBudget) -> Self`, `pub fn walk_policy(&self)`. (TASK-175 owns everything else in this file — coordinate by touching only these lines.)
- `crates/tetond/src/harness/tools/glob.rs` — drop private `SKIP_DIRS`; description → `"List files and directories under the session root matching a glob pattern (supports **, *, ?)."`; walk via `walk::visit`; directory match rule (ADR-3): match a directory only when the pattern's final segment is not `**` and `glob_match(pattern, id)`; list `id/`, tag `id`; results sorted; empty → ``no matches for `{pattern}` `` (update the `contains("no files match")` test); after matches append `walk::trailer_lines`, then the existing cap notice last; provenance unchanged (`with_paths` of ids). Tests: AC-13 (`**/teton-code` returns `a/teton-code/`, tagged `a/teton-code`; `**/*.rs` files only; `secrets/**` unchanged — `provenance_is_the_set_of_enumerated_files` still passes; `**` on the symlink fixture unchanged), AC-14/15 (injected `WalkBudget{max_entries: 3, ..}` → stopped line with zero and with many matches; a tiny `max_wall` with a fixture large enough — or a policy seam that lets a test force the clock — → wall line), AC-16 (`with_root_kind(Home)` fixture with `Library/`, `Music/`, `Pictures/`, `x.photoslibrary/`, `Documents/app/Library/`; `**/*.rs` skips the top-level three and the bundle, finds `Documents/app/Library/*.rs`; `Library/**/*.rs` enters; `with_root_kind(Project)` finds all), AC-17 (mode-000 dir → unreadable line; matches elsewhere present; the macOS sentence asserted on macOS, absent on Linux).
- `crates/tetond/src/harness/tools/grep.rs` — drop private `SKIP_DIRS`; description → `"Search files under the session root for a literal substring. Optional glob narrows the files; set ignore_case for case-insensitive matching."`; walk via `walk::visit` (named_prefix from the optional glob); after matches append trailer lines then the cap notice; rename `split_cap_notice` → `split_harness_trailer` peeling every trailing `walk::is_harness_line` line (cap notice included) and returning them so `render_ranked` re-appends the same lines; update the `match_lines` test helper (filter `... (`) and the split test; add AC-14/15/16/17 twins for grep (shared fixture helpers are fine).
- `crates/tetond/src/harness/tools/shell.rs` — description → `"Run a shell command in the session root under a timeout. Use it to verify changes (build, test, grep). Secrets in the environment are removed."`; timeout arm: when `ctx.root_kind()` ∈ {`Home`, `FilesystemRoot`} and `cfg!(target_os = "macos")`, append ` On macOS a consent dialog for a protected folder holds a command until it is answered — narrow the command to a project path or move the session root with /cd.`; `measuring(NO_OUTPUT_CAPTURED)` unchanged; test AC-19 (home-kind ctx gets the sentence on macOS; project-kind ctx gets today's message byte-for-byte). `"repo root does not exist"` → `"session root does not exist"`.
- `crates/tetond/src/harness/tools/read.rs` — description → `"Read a text file under the session root. Returns line-numbered contents."`; schema `"Root-relative file path"`.
- `crates/tetond/src/harness/tools/edit.rs` — schema `"Root-relative file path"`.
- `crates/tetond/tests/boundary_coverage.rs` — add `walk.rs` to `TOOL_SOURCES` (L56-66; it has no `impl Tool`, so not to `COVERAGE`); add the AC-18 scan: no file under `harness/tools/` other than `walk.rs` contains `SKIP_DIRS`/`const .*SKIP`, and both `glob.rs` and `grep.rs` contain `walk::visit`.
- AC-6 test (put in `glob.rs` or a new `tools/mod.rs`-adjacent test only if TASK-175 has finished with mod.rs — otherwise in `boundary_coverage.rs`): render `ToolRegistry::with_builtins().docs(None)` and assert it contains no "repository"/"repo-relative"/"Repo-relative".

## Acceptance Criteria

- [ ] `cargo test -p tetond -- tools::` green, plus `--test boundary_coverage`, `--test symlink_posture` (unchanged assertions still hold: `**` lists exactly the two files; cycle terminates), `--test provenance_egress` unchanged.
- [ ] AC-13, AC-14, AC-15, AC-16, AC-17, AC-18, AC-19 each have a named test; AC-6 rendered-docs assertion passes.
- [ ] Both walkers consume `walk::visit` and `ToolContext::walk_policy()`; no private skip list remains (the scan proves it).
- [ ] The trailer lines survive `refine`: a test drives `GrepTool::refine` (or `render_ranked`) on an output carrying stopped + unreadable + cap lines and asserts all three come back verbatim after the matches.
- [ ] Docs bytes: the five descriptions together grow by ≤ 40 bytes versus today (TASK-177 measures the ceiling after this task lands — keep the rewording tight).

## Technical Notes

- Entry counting: count every `DirEntry` yielded (files and dirs); check the wall clock at each directory boundary (`Instant::now()` once per dir), so a huge flat directory still stops within one listing.
- Do not follow symlinks (REQ-571 BR-5); `skip_symlink_entry` before everything.
- Keep `MAX_RESULTS`/`MAX_MATCHES` = 200 and their notices as the LAST line.
- The macOS sentence is `cfg!`-selected text (both branches compile); tests assert presence on macOS and absence on Linux via `cfg!` too.
- If TASK-175's `ToolContext` changes are not yet in the tree when you start (parallel tier boundary is T1→T2, so they should be), stop and wait — do not re-implement them.
- Commit as `feat(tools): one walk policy — bounded, directory-aware, media-pruning walkers with harness trailers; shell timeout hint [TASK-176]`.
