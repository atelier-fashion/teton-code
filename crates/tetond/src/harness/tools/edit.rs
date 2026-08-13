//! The `edit` tool: an exact-match, single-occurrence string replacement.
//!
//! This is the harness's edit primitive and it is deliberately unforgiving,
//! because silent or ambiguous edits are how weak models corrupt code:
//!
//! - the `old_string` must match **exactly once**. Zero matches is a failure
//!   (nothing was edited); more than one match is a failure (which one did the
//!   model mean?). Both come back to the model as errors so it can add context
//!   and retry — never a silent partial success (AC).
//! - `old_string` and `new_string` must differ, and `old_string` may not be
//!   empty (that is a "create", not an "edit").
//!
//! On success the file is rewritten and the model is told the replacement
//! landed, so a following verification step can confirm it.

use serde_json::{json, Value};

use super::{str_arg, Resolved, Tool, ToolContext, ToolOutcome};

/// Replaces a single exact occurrence of a string in a file.
#[derive(Debug, Default, Clone, Copy)]
pub struct EditTool;

impl Tool for EditTool {
    fn name(&self) -> &str {
        "edit"
    }

    fn description(&self) -> &str {
        "Replace an exact, unique string in a file. Fails if the old string is \
         missing or appears more than once — include surrounding context to make \
         it unique."
    }

    fn input_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "path": { "type": "string", "description": "Repo-relative file path" },
                "old_string": { "type": "string", "description": "Exact text to replace (must be unique)" },
                "new_string": { "type": "string", "description": "Replacement text" }
            },
            "required": ["path", "old_string", "new_string"]
        })
    }

    fn run(&self, ctx: &ToolContext, args: &Value) -> ToolOutcome {
        let raw = match str_arg(args, "path") {
            Ok(p) => p,
            Err(e) => return e.into(),
        };
        let old_string = match str_arg(args, "old_string") {
            Ok(s) => s,
            Err(e) => return e.into(),
        };
        let new_string = match str_arg(args, "new_string") {
            Ok(s) => s,
            Err(e) => return e.into(),
        };

        if old_string.is_empty() {
            return ToolOutcome::error(
                "old_string must not be empty; the edit tool replaces existing text, \
                 it does not create files",
            );
        }
        if old_string == new_string {
            return ToolOutcome::error("old_string and new_string are identical; nothing to do");
        }

        // One call yields the file to write AND the identity egress judges it by
        // (REQ-571 ADR-B); see `read` for why the request text is not that
        // identity, and why an in-root symlink is attributed to the file it
        // resolves to while one leaving the root is refused (ADR-C).
        let Resolved { path, provenance } = match ctx.resolve(&raw) {
            Ok(r) => r,
            Err(e) => return e.into(),
        };

        let contents = match std::fs::read_to_string(&path) {
            Ok(c) => c,
            Err(e) => return ToolOutcome::error(format!("could not read `{raw}`: {}", e.kind())),
        };

        let matches = contents.matches(old_string.as_str()).count();
        match matches {
            0 => ToolOutcome::error(format!(
                "old_string not found in `{raw}`; the file was not modified. Re-read the \
                 file and copy the exact text (including whitespace) you want to replace."
            )),
            1 => {
                let updated = contents.replacen(old_string.as_str(), &new_string, 1);
                match std::fs::write(&path, &updated) {
                    // REQ-544 C-1: an edit touches a specific file. Tagging the
                    // result with its identity is harmless over-tagging (egress
                    // only blocks *boundary* sources) and defends the case where
                    // the model edits a `local-only` file then routes a later turn
                    // remotely. REQ-571 BR-2: the identity is the resolved one,
                    // never `raw` — a `./`- or absolute-spelled edit of a boundary
                    // file used to tag a value no boundary glob could match.
                    Ok(()) => ToolOutcome::ok(format!(
                        "edited `{raw}`: replaced 1 occurrence. Verify the change before finishing."
                    ))
                    .with_paths([provenance]),
                    Err(e) => ToolOutcome::error(format!("could not write `{raw}`: {}", e.kind())),
                }
            }
            n => ToolOutcome::error(format!(
                "old_string appears {n} times in `{raw}`; the file was not modified. Add \
                 surrounding context so the match is unique."
            )),
        }
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
            "teton-edit-{tag}-{}-{}-{}",
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
    fn replaces_a_unique_occurrence() {
        let root = temp_root("uniq");
        let file = root.join("f.rs");
        std::fs::write(&file, "const V: u32 = 1;\n").unwrap();
        let ctx = ToolContext::new(&root);
        let out = EditTool.run(
            &ctx,
            &json!({ "path": "f.rs", "old_string": "const V: u32 = 1;", "new_string": "const V: u32 = 2;" }),
        );
        assert!(!out.is_error, "{}", out.content);
        assert_eq!(
            std::fs::read_to_string(&file).unwrap(),
            "const V: u32 = 2;\n"
        );
        std::fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn rejects_non_matching_edit_without_modifying() {
        let root = temp_root("nomatch");
        let file = root.join("f.rs");
        std::fs::write(&file, "alpha\n").unwrap();
        let ctx = ToolContext::new(&root);
        let out = EditTool.run(
            &ctx,
            &json!({ "path": "f.rs", "old_string": "beta", "new_string": "gamma" }),
        );
        assert!(out.is_error);
        assert!(out.content.contains("not found"));
        // Unchanged.
        assert_eq!(std::fs::read_to_string(&file).unwrap(), "alpha\n");
        std::fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn rejects_non_unique_edit_without_modifying() {
        let root = temp_root("dup");
        let file = root.join("f.rs");
        std::fs::write(&file, "x\nx\n").unwrap();
        let ctx = ToolContext::new(&root);
        let out = EditTool.run(
            &ctx,
            &json!({ "path": "f.rs", "old_string": "x", "new_string": "y" }),
        );
        assert!(out.is_error);
        assert!(out.content.contains("2 times"));
        assert_eq!(std::fs::read_to_string(&file).unwrap(), "x\nx\n");
        std::fs::remove_dir_all(&root).ok();
    }

    /// **REQ-571 BR-2/BR-3 at the tool**, the `edit` half — its own fixture, not
    /// a rider on `read`'s (LESSON-502). Every spelling of one boundary file tags
    /// the same canonical identity, so an edit of a `local-only` file pins the
    /// session however the model spelled the path.
    #[test]
    fn every_spelling_of_one_file_tags_the_same_canonical_identity() {
        use crate::harness::context::ToolProvenance;
        let root = temp_root("spell");
        std::fs::create_dir_all(root.join("secrets")).unwrap();
        let file = root.join("secrets/prod.env");
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
            // Restore the file so every spelling performs a real, unique edit.
            std::fs::write(&file, "API_KEY=1\n").unwrap();
            let out = EditTool.run(
                &ctx,
                &json!({
                    "path": spelling,
                    "old_string": "API_KEY=1",
                    "new_string": "API_KEY=2",
                }),
            );
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

    #[test]
    fn rejects_empty_old_string() {
        let root = temp_root("empty");
        std::fs::write(root.join("f.rs"), "a\n").unwrap();
        let ctx = ToolContext::new(&root);
        let out = EditTool.run(
            &ctx,
            &json!({ "path": "f.rs", "old_string": "", "new_string": "b" }),
        );
        assert!(out.is_error);
        std::fs::remove_dir_all(&root).ok();
    }
}
