//! The `read` tool: return a file's contents (optionally a line window).
//!
//! Reads are jailed to the repo root ([`ToolContext`]). Output is line-numbered
//! so the model can cite lines back to the `edit` tool, and an optional
//! `offset`/`limit` window keeps a large file from blowing the small-model
//! context budget in one call.

use serde_json::{json, Value};

use super::{opt_u64_arg, str_arg, Tool, ToolContext, ToolOutcome};

/// Maximum lines returned when no `limit` is given — keeps a single read from
/// overwhelming a weak model's context.
const DEFAULT_LINE_LIMIT: usize = 400;

/// Reads a file within the repo-root jail.
#[derive(Debug, Default, Clone, Copy)]
pub struct ReadTool;

impl Tool for ReadTool {
    fn name(&self) -> &str {
        "read"
    }

    fn description(&self) -> &str {
        "Read a text file within the repository. Returns line-numbered contents."
    }

    fn input_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "path": { "type": "string", "description": "Repo-relative file path" },
                "offset": { "type": "integer", "description": "1-based first line to return" },
                "limit": { "type": "integer", "description": "Maximum number of lines" }
            },
            "required": ["path"]
        })
    }

    fn run(&self, ctx: &ToolContext, args: &Value) -> ToolOutcome {
        let raw = match str_arg(args, "path") {
            Ok(p) => p,
            Err(e) => return e.into(),
        };
        let path = match ctx.resolve(&raw) {
            Ok(p) => p,
            Err(e) => return e.into(),
        };

        let contents = match std::fs::read_to_string(&path) {
            Ok(c) => c,
            Err(e) => {
                return ToolOutcome::error(format!("could not read `{raw}`: {}", e.kind()));
            }
        };

        let offset = opt_u64_arg(args, "offset").unwrap_or(1).max(1) as usize;
        let limit = opt_u64_arg(args, "limit").map_or(DEFAULT_LINE_LIMIT, |n| n as usize);

        let lines: Vec<&str> = contents.lines().collect();
        // BR-1 (REQ-544 C-1): the result surfaces this file's content, so tag the
        // outcome with the path the model gave (repo-relative, the form
        // boundaries match against). Egress blocks a later remote turn that
        // carries this if `raw` is under a `local-only` boundary.
        if lines.is_empty() {
            return ToolOutcome::ok(format!("`{raw}` is empty.")).with_paths([raw]);
        }

        let start = offset.saturating_sub(1).min(lines.len());
        let end = start.saturating_add(limit).min(lines.len());

        let mut out = String::new();
        for (i, line) in lines[start..end].iter().enumerate() {
            let n = start + i + 1;
            out.push_str(&format!("{n:>6}\t{line}\n"));
        }
        if end < lines.len() {
            out.push_str(&format!(
                "... ({} more lines; call read again with offset={})\n",
                lines.len() - end,
                end + 1
            ));
        }
        ToolOutcome::ok(out).with_paths([raw])
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn temp_root(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "teton-read-{tag}-{}-{}",
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
    fn reads_a_file_with_line_numbers() {
        let root = temp_root("ok");
        std::fs::write(root.join("f.txt"), "alpha\nbeta\n").unwrap();
        let ctx = ToolContext::new(&root);
        let out = ReadTool.run(&ctx, &json!({ "path": "f.txt" }));
        assert!(!out.is_error);
        assert!(out.content.contains("1\talpha"));
        assert!(out.content.contains("2\tbeta"));
        std::fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn windows_with_offset_and_limit() {
        let root = temp_root("win");
        let body: String = (1..=10).map(|n| format!("line{n}\n")).collect();
        std::fs::write(root.join("f.txt"), body).unwrap();
        let ctx = ToolContext::new(&root);
        let out = ReadTool.run(&ctx, &json!({ "path": "f.txt", "offset": 3, "limit": 2 }));
        assert!(out.content.contains("3\tline3"));
        assert!(out.content.contains("4\tline4"));
        assert!(!out.content.contains("line5"));
        assert!(out.content.contains("more lines"));
        std::fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn missing_file_is_a_model_visible_error() {
        let root = temp_root("miss");
        let ctx = ToolContext::new(&root);
        let out = ReadTool.run(&ctx, &json!({ "path": "nope.txt" }));
        assert!(out.is_error);
        std::fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn a_successful_read_reports_the_touched_path_as_provenance() {
        use crate::harness::context::ToolProvenance;
        let root = temp_root("prov");
        std::fs::write(root.join("secrets.env"), "API_KEY=1\n").unwrap();
        let ctx = ToolContext::new(&root);
        let out = ReadTool.run(&ctx, &json!({ "path": "secrets.env" }));
        assert!(!out.is_error);
        // REQ-544 C-1: the result is tagged with the file it read, so a later
        // remote turn carrying it is caught at egress.
        assert_eq!(out.provenance, ToolProvenance::path("secrets.env"));
        std::fs::remove_dir_all(&root).ok();
    }
}
