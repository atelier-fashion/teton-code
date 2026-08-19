---
id: TASK-175
title: "The session-root value: teton-core pure module, tetond probe, ToolContext carries it, jail refusals name it, HarnessConfig field"
status: draft
parent: REQ-583
created: 2026-08-18
updated: 2026-08-18
dependencies: ["TASK-174"]
---

## Description

Create the one derivation of "what ground the session stands on" (ADR-1) and
carry it into the tool jail so refusals can name the root (BR-2, AC-5). Split
pure/I-O per `architecture.md`: classification, display, bounding and the
marker table in `teton-core::session_root` (no I/O); the marker probe and
`.git/HEAD` read in `tetond::session_root`. `ToolContext` gains the root and a
`WalkPolicy` slot (the walk policy type itself is TASK-176's — here only the
kind/display half; expose `root_kind()`/`root_display()` and constructor seams
TASK-176/178 will use). Add the *unrendered* `HarnessConfig.session_root`
field so TASK-177 (render) and TASK-178 (set it per turn) can proceed in
parallel without both editing the struct.

## Files to Create/Modify

- `crates/teton-core/src/session_root.rs` — NEW. `pub const PROJECT_MARKERS: &[&str]` (`.git`, `.hg`, `.svn`, `Cargo.toml`, `package.json`, `pyproject.toml`, `go.mod`, `pom.xml`, `build.gradle`, `Gemfile`, `mix.exs`, `.adlc`); `pub fn classify(path: &Path, home: Option<&Path>, has_marker: bool) -> RootKind` (`Home` when `path == home`, `FilesystemRoot` when `path == Path::new("/")`, `Project` when `has_marker`, else `Plain`; home wins over marker — a `~/.git` must not make the home folder a project); `pub fn display_for(path: &Path, home: Option<&Path>) -> String` (the `banner::cwd_display` rule: `~`, `~/rest`, else absolute); `pub fn bounded_field(s: &str, max_chars: usize) -> String` (control chars and `\n`/`\r` → `?`, middle-elide with `…` past `max_chars`); `pub const DISPLAY_MAX_CHARS: usize = 80; NAME_MAX_CHARS: usize = 32;` `pub fn resolve_cwd_argument(raw: &str, shell_cwd: &Path, home: Option<&Path>) -> Result<PathBuf, CwdArgError>` (`~` and `~/x` expand; relative joins onto `shell_cwd`; empty → error; result absolute; NO filesystem check — the daemon validates). Unit tests: AC-7's four kinds + every marker by name (iterate `PROJECT_MARKERS`), display cases, bounding (control char, 200-char path → ≤ 80 chars with `…`), a grammar table for `resolve_cwd_argument` (`~`, `~/x`, `rel`, `/abs`, `""`) that TASK-179 will reuse by name.
- `crates/teton-core/src/lib.rs` — `pub mod session_root;` + re-exports.
- `crates/tetond/src/session_root.rs` — NEW. `pub fn probe(path: &Path, home: Option<&Path>) -> SessionRoot` (teton-protocol type): `has_marker` = any `PROJECT_MARKERS` entry exists as **file or dir** (a linked worktree's `.git` is a file); `project_name` = bounded basename when `Project`; `vcs_branch` = `read_git_branch(path)`: read `<path>/.git` — if a file starting `gitdir: `, follow it (relative to `path`) — then `HEAD`; `ref: refs/heads/<b>` → `Some(bounded(b))`; detached SHA / unreadable / not a git project → `None`. `display` = `bounded_field(display_for(path, home), DISPLAY_MAX_CHARS)`. Register the module (`crates/tetond/src/lib.rs` or wherever sibling modules like `env_path` are declared). Unit tests: AC-1 (git repo fixture with `HEAD` → branch `main`), AC-3 (detached HEAD → `None`; unreadable → `None`), linked-worktree fixture (`.git` file with `gitdir:`), `$HOME` fixture → `Home` with no branch even if a `.git` exists.
- `crates/tetond/src/harness/tools/mod.rs` — `ToolContext` gains `display: String`, `kind: RootKind` (keep `repo_root: PathBuf`); `ToolContext::new(path)` = kind `Plain`, display via `display_for(path, HOME)` (89 call sites unchanged); `pub fn for_root(path: PathBuf, root: &SessionRoot) -> Self` (runtime seam, TASK-178); `pub fn with_root_kind(self, RootKind) -> Self` (test seam, AC-16/AC-19); `pub fn root_kind()`, `pub fn root_display()`. Rewrite the jail refusals in `resolve` (L126-162) to the BR-2 shape: ``path `{raw}` is outside the session root {display}`` for the escape arm; the "names no file"/"broken symlink"/"root does not exist" arms lose the words "repo root" (say "session root"). Keep `ToolError::Jail`'s `#[error("path jail violation: {0}")]` prefix — it is the error *category*, and the spec's "and nothing else" is about content (no listings, no suggestions). Update `mod.rs:977` (`contains("escapes the repo root")` → the new shape) and add AC-5: `read ../outside.txt` and `edit /etc/hosts` errors both contain the caller's path, "is outside the session root", and the display — one assertion helper used for both.
- `crates/tetond/tests/symlink_posture.rs` — update the six `contains("escapes the repo root")` sites (447, 480, 827, 840, 855 (negated), 906) to the new shape; behaviour unchanged.
- `crates/tetond/src/harness/turn_loop.rs` — `HarnessConfig.session_root: Option<teton_protocol::SessionRoot>` with the `web_capability` doc contract (`None` = not supplied), `None` in `Default`. **Do not render it** (TASK-177). This is the only edit to this file.

## Acceptance Criteria

- [ ] `cargo test -p teton-core session_root` and `cargo test -p tetond session_root` green; AC-7 (four kinds + every marker by name), AC-2/AC-3 (home/root/plain say no branch; detached/unreadable → no branch), the linked-worktree case, and the `resolve_cwd_argument` grammar table.
- [ ] AC-5: `read` and `edit` outside-jail errors share one shape naming the path, "is outside the session root", and the display; `cargo test -p tetond --test symlink_posture` green with the six assertions moved to the new wording.
- [ ] `ToolContext::new(&root)` call sites untouched and compiling (`cargo build --workspace --tests`).
- [ ] `HarnessConfig` has the field, defaults `None`, and every existing `build_system_prompt` caller compiles unchanged.

## Technical Notes

- `home` is `std::env::var_os("HOME")` at the call site (runtime/CLI), passed in — the pure fns never read env.
- Bounding lives in teton-core so the CLI banner, the daemon block and refusals all print one spelling (ADR-2 "built once").
- The two refusal messages in `server.rs` (`cwd must be an absolute path` …) are TASK-178's.
- Commit as `feat(root): derive the session root once — kind, display, branch — and name it in jail refusals [TASK-175]`.
