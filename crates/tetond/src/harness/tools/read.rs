//! The `read` tool: return a file's contents (optionally a line window).
//!
//! Reads are jailed to the repo root ([`ToolContext`]). Output is line-numbered
//! so the model can cite lines back to the `edit` tool, and an optional
//! `offset`/`limit` window keeps a large file from blowing the small-model
//! context budget in one call.

use serde_json::{json, Value};
use teton_core::ProvenanceId;

use super::{opt_u64_arg, str_arg, Resolved, Tool, ToolContext, ToolOutcome};

/// Maximum lines returned when no `limit` is given — keeps a single read from
/// overwhelming a weak model's context.
const DEFAULT_LINE_LIMIT: usize = 400;

/// How a resolved request is named back to the model: the request text alone
/// when it already spells the resolved identity, and `` `request` -> `resolved` ``
/// when it does not (REQ-571 BR-11). Shared with [`super::edit`], so the two
/// tools cannot drift into two vocabularies for one fact.
///
/// A model that asked for `notes.txt` and got a link's target back is otherwise
/// told it read `notes.txt` while holding the bytes of `secrets/prod.env`. The
/// canonical identity is what the boundary is matched on, but it never reaches
/// the model's context — so without this the model is the one party to the
/// exchange that does not know which file it is holding.
///
/// **Display only.** Nothing built here can reach enforcement:
/// [`ToolContext::resolve`] mints the [`ProvenanceId`] and `with_paths` accepts
/// ids and not text (ADR-A), so a boundary verdict is unreachable from this
/// string by construction.
pub(super) fn shown_path(raw: &str, resolved: &ProvenanceId) -> String {
    divergence_note(raw, resolved).unwrap_or_else(|| format!("`{raw}`"))
}

/// The `` `request` -> `resolved` `` rendering, or `None` when the request
/// already spells the resolved identity and there is nothing to disclose.
///
/// `None` is what keeps the common case free: the caller renders exactly what it
/// rendered before BR-11, byte for byte (AC-15), and this text — which enters
/// the model's context on every affected call — costs nothing on the reads that
/// have nothing to say.
///
/// # `./x` is not a divergence
///
/// One leading `./` is stripped from the request before comparing. A model that
/// spells a path `./src/main.rs` asked for exactly the file it got; calling that
/// a divergence would print a second name on a large share of ordinary reads and
/// train the reader to skim past the line that matters. Every other difference is
/// shown, including the neighbouring spellings `.//x` and `././x` — the rule is
/// deliberately dumb, because it is a display heuristic and a heuristic that
/// guessed harder would be one more thing that can be wrong about a path.
fn divergence_note(raw: &str, resolved: &ProvenanceId) -> Option<String> {
    let requested = raw.strip_prefix("./").unwrap_or(raw);
    (requested != resolved.as_str()).then(|| format!("`{raw}` -> `{}`", resolved.as_str()))
}

/// Reads a file within the repo-root jail.
#[derive(Debug, Default, Clone, Copy)]
pub struct ReadTool;

impl Tool for ReadTool {
    fn name(&self) -> &str {
        "read"
    }

    fn description(&self) -> &str {
        "Read a text file under the session root. Returns line-numbered contents."
    }

    fn input_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "path": { "type": "string", "description": "Root-relative file path" },
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
        //
        // A symlink is a spelling too (ADR-C): `resolve` canonicalizes, so an
        // in-root link is attributed to its *target* — reading `notes.txt ->
        // secrets/prod.env` is a read of `secrets/prod.env` and pins the turn as
        // one — and a link resolving outside the root is refused by the jail.
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
            return ToolOutcome::ok(format!("{} is empty.", shown_path(&raw, &provenance)))
                .with_paths([provenance]);
        }

        let start = offset.saturating_sub(1).min(lines.len());
        let end = start.saturating_add(limit).min(lines.len());

        let mut out = String::new();
        // BR-11: when the file opened is not the one the request names — a
        // symlink, an absolute path, a `..` traversal — say so once, above the
        // content, since everything below it is what the model will attribute to
        // whichever name it believes it read. When the two agree this adds
        // nothing at all and the rendering is byte-identical to pre-BR-11.
        if let Some(note) = divergence_note(&raw, &provenance) {
            out.push_str(&note);
            out.push('\n');
        }
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
        use crate::fixture_id;
        use crate::harness::context::ToolProvenance;
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

    /// **REQ-571 AC-15, the common case.** When the request already spells the
    /// resolved identity the rendering is *byte-identical* to pre-BR-11 — pinned
    /// by exact comparison against the literal, never `contains`, because
    /// "contains the old text" is exactly what a stray divergence line would also
    /// satisfy. Both of `read`'s output shapes are covered: the content window
    /// and the empty-file note.
    #[test]
    fn a_matching_request_renders_byte_identically() {
        let root = temp_root("bytes");
        std::fs::write(root.join("f.txt"), "alpha\nbeta\n").unwrap();
        std::fs::write(root.join("empty.txt"), "").unwrap();
        let ctx = ToolContext::new(&root);

        let out = ReadTool.run(&ctx, &json!({ "path": "f.txt" }));
        assert_eq!(out.content, "     1\talpha\n     2\tbeta\n");

        let out = ReadTool.run(&ctx, &json!({ "path": "empty.txt" }));
        assert_eq!(out.content, "`empty.txt` is empty.");
        std::fs::remove_dir_all(&root).ok();
    }

    /// A leading `./` is *not* a divergence: `./f.txt` and `f.txt` name the same
    /// file in the same words, so the output is the same bytes. Without this the
    /// note would fire on a large share of ordinary reads and stop being read.
    #[test]
    fn a_dot_slash_request_is_not_a_divergence() {
        let root = temp_root("dotslash");
        std::fs::write(root.join("f.txt"), "alpha\nbeta\n").unwrap();
        let ctx = ToolContext::new(&root);
        let plain = ReadTool.run(&ctx, &json!({ "path": "f.txt" }));
        let dotted = ReadTool.run(&ctx, &json!({ "path": "./f.txt" }));
        assert_eq!(dotted.content, plain.content);
        assert_eq!(dotted.content, "     1\talpha\n     2\tbeta\n");
        std::fs::remove_dir_all(&root).ok();
    }

    /// **REQ-571 AC-15, the divergent case.** A spelling that resolves to a
    /// different name shows both, above the content — and the identity that
    /// governs enforcement is untouched by what was displayed (BR-11: display and
    /// provenance are separate values).
    #[test]
    fn a_divergent_request_shows_both_forms() {
        use crate::harness::context::ToolProvenance;
        let root = temp_root("diverge");
        std::fs::write(root.join("f.txt"), "alpha\nbeta\n").unwrap();
        let absolute = root.canonicalize().unwrap().join("f.txt");
        let absolute = absolute.to_string_lossy().into_owned();
        let ctx = ToolContext::new(&root);

        for spelling in [absolute.as_str(), "src/../f.txt"] {
            let out = ReadTool.run(&ctx, &json!({ "path": spelling }));
            assert!(!out.is_error, "{spelling:?}: {}", out.content);
            assert_eq!(
                out.content,
                format!("`{spelling}` -> `f.txt`\n     1\talpha\n     2\tbeta\n"),
                "spelling {spelling:?} must show what it asked for and what it got"
            );
            let ToolProvenance::Sources(ids) = &out.provenance else {
                panic!("spelling {spelling:?} produced unknown provenance");
            };
            let ids: Vec<&str> = ids.iter().map(teton_core::ProvenanceId::as_str).collect();
            assert_eq!(
                ids,
                vec!["f.txt"],
                "the displayed text is not the identity: {spelling:?}"
            );
        }
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
