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

use super::{str_arg, Tool, ToolContext, ToolOutcome};

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

        let path = match ctx.resolve(&raw) {
            Ok(p) => p,
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
                    Ok(()) => ToolOutcome::ok(format!(
                        "edited `{raw}`: replaced 1 occurrence. Verify the change before finishing."
                    )),
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

    fn temp_root(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "teton-edit-{tag}-{}-{}",
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
