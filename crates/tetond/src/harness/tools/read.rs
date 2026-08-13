//! The `read` tool: return a file's contents (optionally a line window).
//!
//! Reads are jailed to the repo root ([`ToolContext`]). Output is line-numbered
//! so the model can cite lines back to the `edit` tool, and an optional
//! `offset`/`limit` window keeps a large file from blowing the small-model
//! context budget in one call.

use serde_json::{json, Value};

use super::{opt_u64_arg, str_arg, Resolved, Tool, ToolContext, ToolOutcome};

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
        // One call yields the file to open AND the identity egress judges it by
        // (REQ-571 ADR-B) — so the boundary is matched on the same canonical
        // value this read opens, whatever spelling the model used to ask for it.
        let Resolved { path, provenance } = match ctx.resolve(&raw) {
            Ok(r) => r,
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
        // BR-1 (REQ-544 C-1), corrected by REQ-571 BR-2: the result surfaces this
        // file's content, so tag the outcome with the **resolved** identity — not
        // with `raw`, which is the model's request text and may spell the same
        // file as `./secrets/x`, `/abs/repo/secrets/x`, or `src/../secrets/x`.
        // None of those match a `secrets/**` boundary glob; the minted id always
        // does.
        if lines.is_empty() {
            return ToolOutcome::ok(format!("`{raw}` is empty.")).with_paths([provenance]);
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
        ToolOutcome::ok(out).with_paths([provenance])
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    /// The counter, not the timestamp, guarantees uniqueness: `SystemTime::now()`
    /// can return the same value for two calls within one clock tick.
    fn temp_root(tag: &str) -> PathBuf {
        use std::sync::atomic::{AtomicU64, Ordering};
        static NEXT: AtomicU64 = AtomicU64::new(0);
        let dir = std::env::temp_dir().join(format!(
            "teton-read-{tag}-{}-{}-{}",
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
        use crate::harness::context::{fixture_id, ToolProvenance};
        let root = temp_root("prov");
        std::fs::write(root.join("secrets.env"), "API_KEY=1\n").unwrap();
        let ctx = ToolContext::new(&root);
        let out = ReadTool.run(&ctx, &json!({ "path": "secrets.env" }));
        assert!(!out.is_error);
        // REQ-544 C-1: the result is tagged with the file it read, so a later
        // remote turn carrying it is caught at egress.
        assert_eq!(
            out.provenance,
            ToolProvenance::path(fixture_id("secrets.env"))
        );
        std::fs::remove_dir_all(&root).ok();
    }

    /// **REQ-571 BR-2/BR-3 at the tool.** Every spelling of one boundary file
    /// tags the *same* identity, and it is the canonical repo-relative one a
    /// `secrets/**` glob matches — not the request text.
    ///
    /// The absolute and `..`-traversing spellings are the ones that used to slip
    /// through: `with_paths([raw])` tagged `/abs/repo/secrets/prod.env`, which no
    /// repo-relative boundary glob matches, so the read was surfaced to the model
    /// with provenance that could never block a later remote turn.
    #[test]
    fn every_spelling_of_one_file_tags_the_same_canonical_identity() {
        use crate::harness::context::ToolProvenance;
        let root = temp_root("spell");
        std::fs::create_dir_all(root.join("secrets")).unwrap();
        std::fs::write(root.join("secrets/prod.env"), "API_KEY=1\n").unwrap();
        let absolute = root.canonicalize().unwrap().join("secrets/prod.env");
        let absolute = absolute.to_string_lossy().into_owned();
        let ctx = ToolContext::new(&root);

        for spelling in [
            "secrets/prod.env",
            "./secrets/prod.env",
            ".//secrets/prod.env",
            "././secrets/prod.env",
            &absolute,
            "src/../secrets/prod.env",
        ] {
            let out = ReadTool.run(&ctx, &json!({ "path": spelling }));
            assert!(!out.is_error, "{spelling:?}: {}", out.content);
            let ToolProvenance::Sources(ids) = &out.provenance else {
                panic!("spelling {spelling:?} produced unknown provenance");
            };
            let ids: Vec<&str> = ids.iter().map(teton_core::ProvenanceId::as_str).collect();
            assert_eq!(
                ids,
                vec!["secrets/prod.env"],
                "spelling {spelling:?} tagged the wrong identity"
            );
        }
        std::fs::remove_dir_all(&root).ok();
    }
}
