//! The `glob` tool: list repo files matching a shell-style pattern.
//!
//! A small hand-rolled matcher (no extra dependency, per ADR-001's lean-binary
//! stance) supporting `**` (any run of path segments), `*` (any run within a
//! segment), and `?` (one character). Results are repo-relative, `/`-separated,
//! sorted, and capped so a weak model gets a legible file list rather than a
//! flood.

use serde_json::{json, Value};
use std::path::Path;

use super::{str_arg, Tool, ToolContext, ToolOutcome};

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
        let mut out = matches.join("\n");
        if truncated {
            out.push_str(&format!("\n... (capped at {MAX_RESULTS} results)"));
        }
        ToolOutcome::ok(out)
    }
}

/// Recursively collect relative paths under `dir` that match `pattern`.
fn walk(root: &Path, dir: &Path, pattern: &str, out: &mut Vec<String>) {
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
        let name = entry.file_name();
        let name = name.to_string_lossy();
        if file_type.is_dir() {
            if SKIP_DIRS.contains(&name.as_ref()) {
                continue;
            }
            walk(root, &path, pattern, out);
        } else if let Ok(rel) = path.strip_prefix(root) {
            let rel = rel.to_string_lossy().replace('\\', "/");
            if glob_match(pattern, &rel) {
                out.push(rel);
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

    fn temp_root(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "teton-glob-{tag}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
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
}
