//! The `glob` tool: list repo files matching a shell-style pattern.
//!
//! A small hand-rolled matcher (no extra dependency, per ADR-001's lean-binary
//! stance) supporting `**` (any run of path segments), `*` (any run within a
//! segment), and `?` (one character). Results are repo-relative, `/`-separated,
//! sorted, and capped so a weak model gets a legible file list rather than a
//! flood.

use serde_json::{json, Value};
use std::path::Path;
use teton_core::ProvenanceId;

use super::{skip_symlink_entry, str_arg, Tool, ToolContext, ToolOutcome};

/// Directory names never descended into (noise / not source).
const SKIP_DIRS: &[&str] = &[".git", "target", "node_modules"];

/// Cap on returned paths.
const MAX_RESULTS: usize = 200;

/// Lists files matching a glob pattern, jailed to the repo root.
#[derive(Debug, Default, Clone, Copy)]
pub struct GlobTool;

impl Tool for GlobTool {
    fn name(&self) -> &str {
        "glob"
    }

    fn description(&self) -> &str {
        "List repository files matching a glob pattern (supports **, *, ?)."
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
            Err(_) => return ToolOutcome::error("repo root does not exist"),
        };

        let mut matches = Vec::new();
        walk(&root, &root, &pattern, &mut matches);
        matches.sort();

        if matches.is_empty() {
            return ToolOutcome::ok(format!("no files match `{pattern}`"));
        }
        let truncated = matches.len() > MAX_RESULTS;
        matches.truncate(MAX_RESULTS);
        let mut out = matches
            .iter()
            .map(ProvenanceId::as_str)
            .collect::<Vec<_>>()
            .join("\n");
        if truncated {
            out.push_str(&format!("\n... (capped at {MAX_RESULTS} results)"));
        }
        // REQ-544 C-1: the enumerated files ARE the result's content, so tag the
        // outcome with them — a glob that surfaces a `local-only` file blocks the
        // next remote turn at egress. REQ-571: the listed name and the tagged
        // identity are one value, so the two cannot disagree about what was
        // surfaced.
        ToolOutcome::ok(out).with_paths(matches)
    }
}

/// Recursively collect the identities of files under `dir` matching `pattern`.
///
/// REQ-571: minted by [`ProvenanceId::from_resolved`] rather than by an inline
/// `strip_prefix` + separator fixup — the same arithmetic, but stated once (see
/// [`grep::search`](super::grep)). The id doubles as the listed name, so a file
/// cannot be shown under one spelling and tagged under another.
fn walk(root: &Path, dir: &Path, pattern: &str, out: &mut Vec<ProvenanceId>) {
    let entries = match std::fs::read_dir(dir) {
        Ok(e) => e,
        Err(_) => return,
    };
    for entry in entries.flatten() {
        let path = entry.path();
        let file_type = match entry.file_type() {
            Ok(t) => t,
            Err(_) => continue,
        };
        // REQ-571 ADR-C: a link is skipped before either branch below — this is
        // the deliberate posture for walking tools, and it is tested on the entry
        // rather than inferred from `!is_dir()`. See `skip_symlink_entry` for both
        // halves of why. Here the harm is the sharper one: the listed name *is*
        // the minted id, so a followed link both names a second identity for one
        // file and, for a link out of the jail, advertises an outside file under
        // an in-jail path.
        if skip_symlink_entry(file_type) {
            continue;
        }
        let name = entry.file_name();
        let name = name.to_string_lossy();
        if file_type.is_dir() {
            if SKIP_DIRS.contains(&name.as_ref()) {
                continue;
            }
            walk(root, &path, pattern, out);
        } else if let Ok(id) = ProvenanceId::from_resolved(root, &path) {
            if glob_match(pattern, id.as_str()) {
                out.push(id);
            }
        }
    }
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
    use std::path::PathBuf;

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
        assert!(out.content.contains("no files match"));
        std::fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn provenance_is_the_set_of_enumerated_files() {
        use crate::harness::context::ToolProvenance;
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
}
