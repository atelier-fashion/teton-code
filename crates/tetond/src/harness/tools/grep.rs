//! The `grep` tool: find lines containing a literal substring.
//!
//! Deliberately a **literal** (not regex) matcher for the MVP: it needs no
//! dependency (ADR-001) and literal + case-insensitive triage is what the
//! local-tier "grep triage" duty actually needs. An optional `glob` narrows the
//! file set (reusing the [`glob`](super::glob) matcher). Output is
//! `path:line: text`, capped for a small model's context.

use serde_json::{json, Value};
use std::collections::BTreeSet;
use std::path::Path;

use super::glob::glob_match;
use super::{opt_str_arg, str_arg, Tool, ToolContext, ToolOutcome};

/// Directory names never searched.
const SKIP_DIRS: &[&str] = &[".git", "target", "node_modules"];

/// Cap on returned matching lines.
const MAX_MATCHES: usize = 200;

/// Searches repository files for a literal substring, jailed to the repo root.
#[derive(Debug, Default, Clone, Copy)]
pub struct GrepTool;

impl Tool for GrepTool {
    fn name(&self) -> &str {
        "grep"
    }

    fn description(&self) -> &str {
        "Search repository files for a literal substring. Optional glob narrows \
         the files; set ignore_case for case-insensitive matching."
    }

    fn input_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "pattern": { "type": "string", "description": "Literal substring to find" },
                "glob": { "type": "string", "description": "Optional file glob, e.g. **/*.rs" },
                "ignore_case": { "type": "boolean", "description": "Case-insensitive match" }
            },
            "required": ["pattern"]
        })
    }

    fn run(&self, ctx: &ToolContext, args: &Value) -> ToolOutcome {
        let pattern = match str_arg(args, "pattern") {
            Ok(p) => p,
            Err(e) => return e.into(),
        };
        if pattern.is_empty() {
            return ToolOutcome::error("pattern must not be empty");
        }
        let file_glob = opt_str_arg(args, "glob");
        let ignore_case = args
            .get("ignore_case")
            .and_then(Value::as_bool)
            .unwrap_or(false);

        let root = match ctx.repo_root().canonicalize() {
            Ok(r) => r,
            Err(_) => return ToolOutcome::error("repo root does not exist"),
        };

        let needle = if ignore_case {
            pattern.to_lowercase()
        } else {
            pattern.clone()
        };

        let mut hits = Vec::new();
        // REQ-544 C-1: the files whose *content* appears in the output. Only
        // matched files surface content into context, so those are the paths the
        // result's egress provenance must carry (a file that was scanned but had
        // no match contributes nothing and is not tagged).
        let mut matched_files = BTreeSet::new();
        search(
            &root,
            &root,
            &needle,
            ignore_case,
            file_glob.as_deref(),
            &mut hits,
            &mut matched_files,
        );

        if hits.is_empty() {
            return ToolOutcome::ok(format!("no matches for `{pattern}`"));
        }
        let truncated = hits.len() > MAX_MATCHES;
        hits.truncate(MAX_MATCHES);
        let mut out = hits.join("\n");
        if truncated {
            out.push_str(&format!("\n... (capped at {MAX_MATCHES} matches)"));
        }
        ToolOutcome::ok(out).with_paths(matched_files)
    }
}

/// Recursively search text files under `dir`. `matched` accumulates the
/// repo-relative paths of every file that produced at least one hit — the egress
/// provenance of the result (REQ-544 C-1).
#[allow(clippy::too_many_arguments)]
fn search(
    root: &Path,
    dir: &Path,
    needle: &str,
    ignore_case: bool,
    file_glob: Option<&str>,
    out: &mut Vec<String>,
    matched: &mut BTreeSet<String>,
) {
    let entries = match std::fs::read_dir(dir) {
        Ok(e) => e,
        Err(_) => return,
    };
    for entry in entries.flatten() {
        if out.len() > MAX_MATCHES {
            return;
        }
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
            search(root, &path, needle, ignore_case, file_glob, out, matched);
            continue;
        }
        let rel = match path.strip_prefix(root) {
            Ok(r) => r.to_string_lossy().replace('\\', "/"),
            Err(_) => continue,
        };
        if let Some(g) = file_glob {
            if !glob_match(g, &rel) {
                continue;
            }
        }
        // Skip binary-ish files: read_to_string fails on invalid UTF-8, which is
        // the cheap heuristic we want.
        let contents = match std::fs::read_to_string(&path) {
            Ok(c) => c,
            Err(_) => continue,
        };
        for (i, line) in contents.lines().enumerate() {
            let haystack = if ignore_case {
                line.to_lowercase()
            } else {
                line.to_owned()
            };
            if haystack.contains(needle) {
                matched.insert(rel.clone());
                out.push(format!("{rel}:{}: {}", i + 1, line.trim_end()));
                if out.len() > MAX_MATCHES {
                    return;
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn temp_root(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "teton-grep-{tag}-{}-{}",
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
    fn finds_literal_matches_with_location() {
        let root = temp_root("hit");
        std::fs::write(root.join("a.rs"), "fn main() {}\nlet needle = 1;\n").unwrap();
        let ctx = ToolContext::new(&root);
        let out = GrepTool.run(&ctx, &json!({ "pattern": "needle" }));
        assert!(!out.is_error);
        assert!(out.content.contains("a.rs:2:"));
        std::fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn ignore_case_matches() {
        let root = temp_root("case");
        std::fs::write(root.join("a.rs"), "HELLO world\n").unwrap();
        let ctx = ToolContext::new(&root);
        assert!(GrepTool
            .run(&ctx, &json!({ "pattern": "hello" }))
            .content
            .contains("no matches"));
        let out = GrepTool.run(&ctx, &json!({ "pattern": "hello", "ignore_case": true }));
        assert!(out.content.contains("a.rs:1:"));
        std::fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn glob_narrows_the_file_set() {
        let root = temp_root("narrow");
        std::fs::write(root.join("a.rs"), "TODO fix\n").unwrap();
        std::fs::write(root.join("b.txt"), "TODO fix\n").unwrap();
        let ctx = ToolContext::new(&root);
        let out = GrepTool.run(&ctx, &json!({ "pattern": "TODO", "glob": "**/*.rs" }));
        assert!(out.content.contains("a.rs"));
        assert!(!out.content.contains("b.txt"));
        std::fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn provenance_is_the_set_of_matched_files_only() {
        use crate::harness::context::ToolProvenance;
        let root = temp_root("prov");
        std::fs::create_dir_all(root.join("secrets")).unwrap();
        std::fs::write(root.join("secrets/prod.env"), "API_KEY=sk-live\n").unwrap();
        std::fs::write(root.join("public.rs"), "// nothing to see\n").unwrap();
        let ctx = ToolContext::new(&root);
        // REQ-544 C-1: a grep whose only match is in a boundary file tags the
        // result with that file — so the next remote turn is blocked at egress.
        let out = GrepTool.run(&ctx, &json!({ "pattern": "sk-live" }));
        assert!(!out.is_error);
        assert_eq!(out.provenance, ToolProvenance::path("secrets/prod.env"));
        std::fs::remove_dir_all(&root).ok();
    }
}
