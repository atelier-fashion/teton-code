//! The `glob` tool: list files and directories under the session root matching
//! a shell-style pattern.
//!
//! A small hand-rolled matcher (no extra dependency, per ADR-001's lean-binary
//! stance) supporting `**` (any run of path segments), `*` (any run within a
//! segment), and `?` (one character). Results are root-relative, `/`-separated,
//! sorted, and capped so a weak model gets a legible list rather than a flood.
//!
//! The walk itself — what is skipped, what is pruned from a home-kind root, how
//! far it may go, and what it says when it stops — is [`walk`]'s (REQ-583
//! ADR-3); this tool decides only what an entry it is handed *means*.

use serde_json::{json, Value};
use teton_core::ProvenanceId;

use super::walk;
use super::{str_arg, Tool, ToolContext, ToolOutcome};

/// Cap on returned paths.
const MAX_RESULTS: usize = 200;

/// Lists files and directories matching a glob pattern, jailed to the session
/// root.
#[derive(Debug, Default, Clone, Copy)]
pub struct GlobTool;

impl Tool for GlobTool {
    fn name(&self) -> &str {
        "glob"
    }

    fn description(&self) -> &str {
        "List files and directories under the session root matching a glob pattern \
         (supports **, *, ?)."
    }

    fn input_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "pattern": { "type": "string", "description": "Glob, e.g. src/**/*.rs" }
            },
            "required": ["pattern"]
        })
    }

    fn run(&self, ctx: &ToolContext, args: &Value) -> ToolOutcome {
        let pattern = match str_arg(args, "pattern") {
            Ok(p) => p,
            Err(e) => return e.into(),
        };
        let root = match ctx.repo_root().canonicalize() {
            Ok(r) => r,
            Err(_) => return ToolOutcome::error("session root does not exist"),
        };

        // BR-9 / ADR-3: a directory is a match when the pattern's *final*
        // segment is not `**` and the whole pattern matches its identity.
        // `**`-terminated patterns (`**`, `secrets/**`) keep enumerating files
        // only — "everything beneath" is a file enumeration — so `**/teton-code`
        // returns the directory and `secrets/**` still tags exactly the files.
        let files_only = last_segment_is_doublestar(&pattern);
        let named = walk::leading_literal_segments(&pattern);
        let mut matches: Vec<(ProvenanceId, bool)> = Vec::new();
        let report = walk::visit(
            &root,
            ctx.root_kind(),
            &named,
            ctx.walk_policy(),
            &mut |_, file_type, id| {
                let is_dir = file_type.is_dir();
                if is_dir && files_only {
                    return;
                }
                if glob_match(&pattern, id.as_str()) {
                    matches.push((id.clone(), is_dir));
                }
            },
        );
        matches.sort();
        let trailer = walk::trailer_lines(&report);

        if matches.is_empty() {
            // BR-10: a budget hit is never a silent partial — the stopped line
            // rides under the empty answer exactly as it rides under a full one.
            let mut out = format!("no matches for `{pattern}`");
            for line in &trailer {
                out.push('\n');
                out.push_str(line);
            }
            return ToolOutcome::ok(out);
        }
        let truncated = matches.len() > MAX_RESULTS;
        matches.truncate(MAX_RESULTS);
        // A directory is listed as `id/` and tagged as `id`: `ProvenanceId::mint`
        // elides a trailing `/`, so the two spellings are one identity, and the
        // trailing `/` is the conventional marker of a directory name rather
        // than a second name (ADR-3).
        let mut out = matches
            .iter()
            .map(|(id, is_dir)| {
                if *is_dir {
                    format!("{}/", id.as_str())
                } else {
                    id.as_str().to_owned()
                }
            })
            .collect::<Vec<_>>()
            .join("\n");
        for line in &trailer {
            out.push('\n');
            out.push_str(line);
        }
        if truncated {
            out.push_str(&format!("\n... (capped at {MAX_RESULTS} results)"));
        }
        // REQ-544 C-1: the enumerated entries ARE the result's content, so tag
        // the outcome with them — a glob that surfaces a `local-only` file blocks
        // the next remote turn at egress. REQ-571: the listed name and the tagged
        // identity are one value, so the two cannot disagree about what was
        // surfaced. A bare directory identity carries whatever verdict the
        // matcher gives it (OQ-7): `secrets/**` covers the files under
        // `secrets/`, not the name `secrets`.
        ToolOutcome::ok(out).with_paths(matches.into_iter().map(|(id, _)| id))
    }
}

/// Whether the pattern's final segment is `**` — the shape that means "every
/// file beneath", never "the directories beneath" (BR-9).
fn last_segment_is_doublestar(pattern: &str) -> bool {
    pattern
        .split('/')
        .rfind(|s| !s.is_empty())
        .is_some_and(|last| last == "**")
}

/// Whether `path` (a `/`-separated relative path) matches `pattern`.
pub(crate) fn glob_match(pattern: &str, path: &str) -> bool {
    let p: Vec<&str> = pattern.split('/').filter(|s| !s.is_empty()).collect();
    let t: Vec<&str> = path.split('/').filter(|s| !s.is_empty()).collect();
    match_segments(&p, &t)
}

fn match_segments(p: &[&str], t: &[&str]) -> bool {
    match p.first() {
        None => t.is_empty(),
        Some(&"**") => (0..=t.len()).any(|i| match_segments(&p[1..], &t[i..])),
        Some(seg) => match t.first() {
            Some(first) if wild(seg.as_bytes(), first.as_bytes()) => {
                match_segments(&p[1..], &t[1..])
            }
            _ => false,
        },
    }
}

/// Classic recursive wildcard match for a single segment (`*` and `?`).
fn wild(p: &[u8], s: &[u8]) -> bool {
    match p.first() {
        None => s.is_empty(),
        Some(b'*') => wild(&p[1..], s) || (!s.is_empty() && wild(p, &s[1..])),
        Some(b'?') => !s.is_empty() && wild(&p[1..], &s[1..]),
        Some(&c) => !s.is_empty() && s[0] == c && wild(&p[1..], &s[1..]),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::fixture_id;
    use crate::harness::context::ToolProvenance;
    use crate::harness::tools::walk::{WalkBudget, HOME_TOP_LEVEL_SKIPS, WALK_SKIP_DIRS};
    use std::path::{Path, PathBuf};
    use std::time::Duration;
    use teton_protocol::methods::RootKind;

    #[test]
    fn matcher_handles_star_doublestar_and_question() {
        assert!(glob_match("*.rs", "lib.rs"));
        assert!(!glob_match("*.rs", "lib.txt"));
        assert!(glob_match("src/**/*.rs", "src/a/b/c.rs"));
        assert!(glob_match("src/**/*.rs", "src/c.rs"));
        assert!(!glob_match("src/**/*.rs", "tests/c.rs"));
        assert!(glob_match("f?.rs", "f1.rs"));
        assert!(!glob_match("f?.rs", "f12.rs"));
        assert!(glob_match("**", "any/deep/path.txt"));
    }

    #[test]
    fn a_doublestar_final_segment_means_files_only() {
        assert!(last_segment_is_doublestar("**"));
        assert!(last_segment_is_doublestar("secrets/**"));
        assert!(last_segment_is_doublestar("secrets/**/"));
        assert!(!last_segment_is_doublestar("**/teton-code"));
        assert!(!last_segment_is_doublestar("**/*.rs"));
        assert!(!last_segment_is_doublestar("src/*"));
        assert!(!last_segment_is_doublestar(""));
    }

    /// The counter, not the timestamp, guarantees uniqueness: `SystemTime::now()`
    /// can return the same value for two calls within one clock tick.
    fn temp_root(tag: &str) -> PathBuf {
        use std::sync::atomic::{AtomicU64, Ordering};
        static NEXT: AtomicU64 = AtomicU64::new(0);
        let dir = std::env::temp_dir().join(format!(
            "teton-glob-{tag}-{}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    /// Create `rel` (a file) under `root`, with its parents.
    fn plant(root: &Path, rel: &str) {
        let path = root.join(rel);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, "planted\n").unwrap();
    }

    /// The listed lines of a result, with the harness's own trailer lines
    /// removed.
    fn listed(out: &ToolOutcome) -> Vec<&str> {
        out.content
            .lines()
            .filter(|l| !walk::is_harness_line(l))
            .collect()
    }

    #[test]
    fn walks_the_tree_and_matches() {
        let root = temp_root("walk");
        std::fs::create_dir_all(root.join("src/inner")).unwrap();
        std::fs::write(root.join("src/lib.rs"), "").unwrap();
        std::fs::write(root.join("src/inner/mod.rs"), "").unwrap();
        std::fs::write(root.join("README.md"), "").unwrap();
        let ctx = ToolContext::new(&root);

        let out = GlobTool.run(&ctx, &json!({ "pattern": "src/**/*.rs" }));
        assert!(!out.is_error);
        assert!(out.content.contains("src/lib.rs"));
        assert!(out.content.contains("src/inner/mod.rs"));
        assert!(!out.content.contains("README.md"));
        std::fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn reports_no_matches() {
        let root = temp_root("none");
        let ctx = ToolContext::new(&root);
        let out = GlobTool.run(&ctx, &json!({ "pattern": "*.zzz" }));
        assert!(!out.is_error);
        assert_eq!(out.content, "no matches for `*.zzz`");
        std::fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn provenance_is_the_set_of_enumerated_files() {
        let root = temp_root("prov");
        std::fs::create_dir_all(root.join("secrets")).unwrap();
        std::fs::write(root.join("secrets/prod.env"), "API_KEY=1\n").unwrap();
        std::fs::write(root.join("secrets/dev.env"), "API_KEY=2\n").unwrap();
        let ctx = ToolContext::new(&root);
        // REQ-544 C-1: enumerating boundary files tags the result with them.
        let out = GlobTool.run(&ctx, &json!({ "pattern": "secrets/**" }));
        assert!(!out.is_error);
        assert_eq!(
            out.provenance,
            ToolProvenance::paths([
                fixture_id("secrets/dev.env"),
                fixture_id("secrets/prod.env")
            ])
        );
        std::fs::remove_dir_all(&root).ok();
    }

    /// **AC-13 (BR-9).** A directory whose identity matches is listed — marked
    /// with a trailing `/` — and tagged under the same identity; a pattern
    /// ending in a file wildcard still returns files only.
    #[test]
    fn ac13_glob_lists_a_matching_directory_marked_and_tagged() {
        let root = temp_root("ac13");
        plant(&root, "a/teton-code/x.rs");
        plant(&root, "a/teton-code/src/lib.rs");
        plant(&root, "b/other.rs");
        let ctx = ToolContext::new(&root);

        let out = GlobTool.run(&ctx, &json!({ "pattern": "**/teton-code" }));
        assert!(!out.is_error, "{}", out.content);
        assert_eq!(listed(&out), vec!["a/teton-code/"], "{}", out.content);
        assert_eq!(
            out.provenance,
            ToolProvenance::path(fixture_id("a/teton-code")),
            "the listed name and the tagged identity are one value"
        );

        let out = GlobTool.run(&ctx, &json!({ "pattern": "**/*.rs" }));
        assert_eq!(
            listed(&out),
            vec!["a/teton-code/src/lib.rs", "a/teton-code/x.rs", "b/other.rs"],
            "{}",
            out.content
        );
        assert!(
            !listed(&out).iter().any(|l| l.ends_with('/')),
            "a file-wildcard pattern listed a directory: {}",
            out.content
        );

        // A `**`-terminated pattern enumerates files only (the `secrets/**`
        // shape egress relies on), so no directory is listed or tagged.
        let out = GlobTool.run(&ctx, &json!({ "pattern": "a/**" }));
        assert_eq!(
            listed(&out),
            vec!["a/teton-code/src/lib.rs", "a/teton-code/x.rs"],
            "{}",
            out.content
        );
        std::fs::remove_dir_all(&root).ok();
    }

    /// **AC-14 / AC-15 (BR-10), entries.** An injected budget smaller than the
    /// fixture ends the result with the stopped-by-entries line — with matches
    /// found and with none — and a fixture under the budget produces no such
    /// line.
    #[test]
    fn ac14_ac15_glob_reports_the_entry_budget_with_and_without_matches() {
        let root = temp_root("ac14-entries");
        for n in 0..8 {
            plant(&root, &format!("f{n}.rs"));
        }
        let small = WalkBudget {
            max_entries: 3,
            max_wall: Duration::from_secs(60),
        };
        let stopped =
            "... (stopped after 3 entries; narrow the pattern, or move the session root with /cd)";

        // With matches: three entries were seen, all `.rs`, then the walk stopped.
        let ctx = ToolContext::new(&root).with_walk_budget(small);
        let out = GlobTool.run(&ctx, &json!({ "pattern": "*.rs" }));
        assert!(!out.is_error, "{}", out.content);
        assert_eq!(listed(&out).len(), 3, "{}", out.content);
        assert_eq!(out.content.lines().last(), Some(stopped), "{}", out.content);

        // With no matches at all: the same words (BR-10, never a silent partial).
        let out = GlobTool.run(&ctx, &json!({ "pattern": "*.zzz" }));
        assert_eq!(
            out.content,
            format!("no matches for `*.zzz`\n{stopped}"),
            "{}",
            out.content
        );

        // Under the budget: no stopped line.
        let out = GlobTool.run(&ToolContext::new(&root), &json!({ "pattern": "*.rs" }));
        assert_eq!(listed(&out).len(), 8);
        assert!(!out.content.contains("stopped after"), "{}", out.content);
        std::fs::remove_dir_all(&root).ok();
    }

    /// **AC-14 (BR-10), wall clock.** A zero wall budget lets the root's own
    /// listing through and stops at the first descent, so the result ends with
    /// the stopped-by-time line — with matches, and with none.
    #[test]
    fn ac14_glob_reports_the_wall_clock_budget() {
        let root = temp_root("ac14-wall");
        plant(&root, "top.rs");
        plant(&root, "sub/deep.rs");
        let ctx = ToolContext::new(&root).with_walk_budget(WalkBudget {
            max_entries: 1_000,
            max_wall: Duration::ZERO,
        });
        let stopped =
            "... (stopped after 0 s; narrow the pattern, or move the session root with /cd)";

        let out = GlobTool.run(&ctx, &json!({ "pattern": "**/*.rs" }));
        assert_eq!(listed(&out), vec!["top.rs"], "{}", out.content);
        assert_eq!(out.content.lines().last(), Some(stopped), "{}", out.content);

        let out = GlobTool.run(&ctx, &json!({ "pattern": "**/deep.rs" }));
        assert_eq!(
            out.content,
            format!("no matches for `**/deep.rs`\n{stopped}")
        );

        let out = GlobTool.run(&ToolContext::new(&root), &json!({ "pattern": "**/*.rs" }));
        assert_eq!(listed(&out), vec!["sub/deep.rs", "top.rs"]);
        assert!(!out.content.contains("stopped after"), "{}", out.content);
        std::fs::remove_dir_all(&root).ok();
    }

    /// The trailer rides *between* the matches and the cap notice: matches,
    /// then the walk's lines, then the cap notice last (ADR-3).
    #[test]
    fn the_cap_notice_stays_last_under_a_trailer() {
        let root = temp_root("cap-order");
        for n in 0..(MAX_RESULTS + 5) {
            plant(&root, &format!("f{n:04}.rs"));
        }
        // Budget hit after the cap is already exceeded: both lines, cap last.
        let ctx = ToolContext::new(&root).with_walk_budget(WalkBudget {
            max_entries: MAX_RESULTS + 2,
            max_wall: Duration::from_secs(60),
        });
        let out = GlobTool.run(&ctx, &json!({ "pattern": "*.rs" }));
        let lines: Vec<&str> = out.content.lines().collect();
        assert_eq!(lines.len(), MAX_RESULTS + 2, "{}", out.content);
        assert!(
            lines[MAX_RESULTS].starts_with("... (stopped after"),
            "{}",
            lines[MAX_RESULTS]
        );
        assert_eq!(
            lines[MAX_RESULTS + 1],
            format!("... (capped at {MAX_RESULTS} results)")
        );
        std::fs::remove_dir_all(&root).ok();
    }

    /// A fixture shaped like a home folder: the BR-12 trees at the top, a
    /// bundle at depth, and a project underneath `Documents/` with a `Library/`
    /// of its own.
    fn home_fixture(tag: &str) -> PathBuf {
        let root = temp_root(tag);
        plant(&root, "Library/Preferences/lib.rs");
        plant(&root, "Music/tune.rs");
        plant(&root, "Pictures/pic.rs");
        plant(&root, "Documents/x.photoslibrary/inside.rs");
        plant(&root, "Documents/app/Library/proj_lib.rs");
        plant(&root, "Documents/app/src/main.rs");
        root
    }

    /// **AC-16 (BR-12).** From a `home`-kind root the top-level media trees and
    /// a bundle at any depth are not entered; a project's own `Library/` is;
    /// naming the tree enters it; from a `project` root everything is found.
    #[test]
    fn ac16_glob_prunes_home_media_trees_unless_named() {
        let root = home_fixture("ac16");
        let home = ToolContext::new(&root).with_root_kind(RootKind::Home);

        let out = GlobTool.run(&home, &json!({ "pattern": "**/*.rs" }));
        assert_eq!(
            listed(&out),
            vec![
                "Documents/app/Library/proj_lib.rs",
                "Documents/app/src/main.rs"
            ],
            "{}",
            out.content
        );

        let out = GlobTool.run(&home, &json!({ "pattern": "Library/**/*.rs" }));
        assert_eq!(
            listed(&out),
            vec!["Library/Preferences/lib.rs"],
            "{}",
            out.content
        );

        let project = ToolContext::new(&root).with_root_kind(RootKind::Project);
        let out = GlobTool.run(&project, &json!({ "pattern": "**/*.rs" }));
        assert_eq!(
            listed(&out),
            vec![
                "Documents/app/Library/proj_lib.rs",
                "Documents/app/src/main.rs",
                "Documents/x.photoslibrary/inside.rs",
                "Library/Preferences/lib.rs",
                "Music/tune.rs",
                "Pictures/pic.rs",
            ],
            "{}",
            out.content
        );
        std::fs::remove_dir_all(&root).ok();
    }

    /// **AC-17 (BR-13).** An unreadable folder ends the result with the
    /// permission-denied line; matches elsewhere are still returned; the macOS
    /// consent sentence is present on macOS and absent elsewhere. Skipped as
    /// root, who can read anything.
    #[test]
    fn ac17_glob_reports_an_unreadable_folder_and_still_lists_the_rest() {
        use std::os::unix::fs::PermissionsExt;
        // SAFETY: `geteuid` reads the process's effective uid; no arguments.
        if unsafe { libc::geteuid() } == 0 {
            eprintln!("skipped: running as root, mode 000 does not refuse");
            return;
        }
        let root = temp_root("ac17");
        plant(&root, "secrets/hidden.rs");
        plant(&root, "src/main.rs");
        let locked = root.join("secrets");
        std::fs::set_permissions(&locked, std::fs::Permissions::from_mode(0o000)).unwrap();

        let out = GlobTool.run(&ToolContext::new(&root), &json!({ "pattern": "**/*.rs" }));
        std::fs::set_permissions(&locked, std::fs::Permissions::from_mode(0o755)).unwrap();

        assert!(!out.is_error, "{}", out.content);
        assert_eq!(listed(&out), vec!["src/main.rs"], "{}", out.content);
        let last = out.content.lines().last().unwrap();
        assert!(
            last.starts_with("... (1 folder could not be read (permission denied): secrets/"),
            "{last}"
        );
        let sentence = "macOS may have blocked access to that folder, or be waiting on a consent dialog for it";
        assert_eq!(
            last.contains(sentence),
            cfg!(target_os = "macos"),
            "the consent sentence is macOS-only: {last}"
        );
        std::fs::remove_dir_all(&root).ok();
    }

    /// **AC-18 (BR-11), the behavioural half.** The walker is built from the
    /// shared definition: every name in [`WALK_SKIP_DIRS`] is skipped and every
    /// name in [`HOME_TOP_LEVEL_SKIPS`] is pruned from a home root — read off
    /// the definition, not off a second list. (`grep`'s twin lives beside it;
    /// the source scan in `tests/boundary_coverage.rs` proves neither tool
    /// declares a list of its own.)
    #[test]
    fn ac18_glob_is_built_from_the_shared_walk_definition() {
        let root = temp_root("ac18");
        for name in WALK_SKIP_DIRS.iter().chain(HOME_TOP_LEVEL_SKIPS) {
            plant(&root, &format!("{name}/inside.rs"));
        }
        plant(&root, "src/main.rs");
        let home = ToolContext::new(&root).with_root_kind(RootKind::Home);
        let out = GlobTool.run(&home, &json!({ "pattern": "**/*.rs" }));
        assert_eq!(listed(&out), vec!["src/main.rs"], "{}", out.content);
        // Same policy object the walker read — the context's, which defaults to
        // the module's constants.
        assert_eq!(home.walk_policy(), &walk::WalkPolicy::default());
        std::fs::remove_dir_all(&root).ok();
    }
}
