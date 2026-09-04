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

use super::read::shown_path;
use super::{str_arg, Resolved, Tool, ToolContext, ToolOutcome};

/// Replaces a single exact occurrence of a string in a file.
///
/// Carries an optional event emitter for REQ-615 BR-4's refusal record —
/// `ProjectsTool`'s shape, and optional for its reason: a tool built for a unit
/// test has no bus, and a missing bus means "no record" rather than a panic.
/// The `Default` is what keeps every existing `EditTool::default().run(…)` call
/// site honest about having no session to report to.
#[derive(Default)]
pub struct EditTool {
    events: Option<crate::harness::turn_loop::SessionEvents>,
}

impl EditTool {
    /// An edit tool reporting REQ-615 BR-4 refusals to `events`.
    #[must_use]
    pub fn with_events(events: Option<crate::harness::turn_loop::SessionEvents>) -> Self {
        Self { events }
    }
}

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
                "path": { "type": "string", "description": "Root-relative file path" },
                "old_string": { "type": "string", "description": "Exact text to replace (must be unique)" },
                "new_string": { "type": "string", "description": "Replacement text" }
            },
            "required": ["path", "old_string", "new_string"]
        })
    }

    fn run(&self, ctx: &ToolContext, args: &Value) -> ToolOutcome {
        // REQ-615 BR-4, before the arguments are even read: `edit` is
        // unconditionally a write, so at a root that gates writes there is
        // nothing to validate. AC-3 asserts the target file is untouched
        // afterwards by inspecting it, not by reading this error (LESSON-519).
        if crate::harness::root_gate::edit_gate(ctx.root_kind())
            == crate::harness::root_gate::WriteVerdict::RefusedNonProject
        {
            if let Some(events) = self.events.as_ref() {
                events.write_refused_non_project(teton_protocol::events::WriteRefusedNonProject {
                    tool: "edit".to_owned(),
                    root_display: ctx.root_display().to_owned(),
                    root_kind: ctx.root_kind(),
                    remedy: crate::harness::root_gate::WRITE_REMEDY.to_owned(),
                });
            }
            return ToolOutcome::error(crate::harness::root_gate::write_refusal(
                ctx.root_display(),
                ctx.root_kind(),
            ));
        }
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
        // Regular files only, off `metadata` and before the open — `read`'s
        // rule, for `read`'s reason (a FIFO's open blocks the turn).
        if let Err(e) = super::refuse_non_regular_file(&raw, &path) {
            return e.into();
        }

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
                    //
                    // REQ-571 BR-11: and the model is told *which* file it just
                    // rewrote — `shown_path` names the resolved target alongside
                    // the request whenever an edit through a link or an absolute
                    // path landed somewhere other than where the request reads.
                    Ok(()) => ToolOutcome::ok(format!(
                        "edited {}: replaced 1 occurrence. Verify the change before finishing.",
                        shown_path(&raw, &provenance)
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
    use teton_protocol::methods::RootKind;

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
        let out = EditTool::default().run(
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

    /// **A FIFO is refused before it is opened, so it cannot block the turn**
    /// (REQ-583 verify): `read`'s rule, asserted on `edit` with the same
    /// deadline, so a regression in either tool is a failed test and not a
    /// hung suite.
    #[test]
    fn a_fifo_is_refused_as_not_a_regular_file_without_blocking() {
        let root = temp_root("fifo");
        crate::mkfifo(&root.join("pipe"));
        let worker_root = root.clone();
        let out = crate::with_deadline("edit of a FIFO", move || {
            EditTool::default().run(
                &ToolContext::new(&worker_root),
                &json!({ "path": "pipe", "old_string": "a", "new_string": "b" }),
            )
        });
        assert!(out.is_error, "{}", out.content);
        assert!(
            out.content.contains("path `pipe` is not a regular file"),
            "{}",
            out.content
        );
        std::fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn rejects_non_matching_edit_without_modifying() {
        let root = temp_root("nomatch");
        let file = root.join("f.rs");
        std::fs::write(&file, "alpha\n").unwrap();
        let ctx = ToolContext::new(&root);
        let out = EditTool::default().run(
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
        let out = EditTool::default().run(
            &ctx,
            &json!({ "path": "f.rs", "old_string": "x", "new_string": "y" }),
        );
        assert!(out.is_error);
        assert!(out.content.contains("2 times"));
        assert_eq!(std::fs::read_to_string(&file).unwrap(), "x\nx\n");
        std::fs::remove_dir_all(&root).ok();
    }

    /// **REQ-571 AC-15, the common case**, on the `edit` success line. Exact
    /// comparison against the literal, not `contains`: the claim is that nothing
    /// was added, and `contains` cannot make that claim.
    #[test]
    fn a_matching_request_renders_byte_identically() {
        let root = temp_root("bytes");
        std::fs::write(root.join("f.rs"), "const V: u32 = 1;\n").unwrap();
        let ctx = ToolContext::new(&root);
        let out = EditTool::default().run(
            &ctx,
            &json!({ "path": "f.rs", "old_string": "= 1;", "new_string": "= 2;" }),
        );
        assert_eq!(
            out.content,
            "edited `f.rs`: replaced 1 occurrence. Verify the change before finishing."
        );

        // And `./f.rs` is the same request in different words, so the same bytes.
        let out = EditTool::default().run(
            &ctx,
            &json!({ "path": "./f.rs", "old_string": "= 2;", "new_string": "= 3;" }),
        );
        assert_eq!(
            out.content,
            "edited `./f.rs`: replaced 1 occurrence. Verify the change before finishing."
        );
        std::fs::remove_dir_all(&root).ok();
    }

    /// **REQ-571 AC-15, the divergent case**, on the `edit` success line: the
    /// model is told which file its write actually landed on, and the identity
    /// egress judges the turn by is unaffected by that text (BR-11).
    #[test]
    fn a_divergent_request_shows_both_forms() {
        use crate::harness::context::ToolProvenance;
        let root = temp_root("diverge");
        let file = root.join("f.rs");
        let absolute = {
            let p = root.canonicalize().unwrap().join("f.rs");
            p.to_string_lossy().into_owned()
        };
        let ctx = ToolContext::new(&root);

        for spelling in [absolute.as_str(), "src/../f.rs"] {
            std::fs::write(&file, "const V: u32 = 1;\n").unwrap();
            let out = EditTool::default().run(
                &ctx,
                &json!({ "path": spelling, "old_string": "= 1;", "new_string": "= 2;" }),
            );
            assert!(!out.is_error, "{spelling:?}: {}", out.content);
            assert_eq!(
                out.content,
                format!(
                    "edited `{spelling}` -> `f.rs`: replaced 1 occurrence. \
                     Verify the change before finishing."
                ),
                "spelling {spelling:?} must name the file it wrote"
            );
            let ToolProvenance::Sources(ids) = &out.provenance else {
                panic!("spelling {spelling:?} produced unknown provenance");
            };
            let ids: Vec<&str> = ids.iter().map(teton_core::ProvenanceId::as_str).collect();
            assert_eq!(
                ids,
                vec!["f.rs"],
                "the displayed text is not the identity: {spelling:?}"
            );
        }
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
            let out = EditTool::default().run(
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

    /// **REQ-615 BR-4 / AC-3: an edit at a home root is refused and writes
    /// nothing.**
    ///
    /// Asserted by reading the file back, not by reading the error: a gate that
    /// refused *after* writing would produce the same error text (LESSON-519,
    /// AC-3's own instruction).
    ///
    /// Mutation: move the gate below the `fs::write`, or drop `RootKind::Home`
    /// from `gates_writes` — the byte comparison goes red.
    #[test]
    fn an_edit_at_a_home_root_is_refused_and_writes_nothing() {
        let root = temp_root("homegate");
        let file = root.join("notes.md");
        std::fs::write(&file, "before\n").unwrap();
        let ctx = ToolContext::new(&root).with_root_kind(RootKind::Home);

        let out = EditTool::default().run(
            &ctx,
            &json!({ "path": "notes.md", "old_string": "before", "new_string": "after" }),
        );
        assert!(out.is_error, "{}", out.content);
        assert_eq!(
            std::fs::read_to_string(&file).unwrap(),
            "before\n",
            "the file must be untouched — the refusal is before the write"
        );
        assert!(
            out.content.contains("/cd <name>"),
            "the refusal names the remedy:\n{}",
            out.content
        );
        std::fs::remove_dir_all(&root).ok();
    }

    /// **REQ-615 BR-9 / BR-4's carve-out: an edit at a plain root still
    /// writes.**
    ///
    /// The benign path, and it guards a shipped feature: a `plain` root is
    /// where a user scaffolds a new project and where REQ-613's `TETON.md`
    /// write lands. Gating it would break that with nothing else noticing.
    ///
    /// Mutation: add `RootKind::Plain` to `gates_writes` — this goes red.
    #[test]
    fn an_edit_at_a_plain_root_still_writes() {
        let root = temp_root("plaingate");
        let file = root.join("TETON.md");
        std::fs::write(&file, "before\n").unwrap();
        for kind in [RootKind::Plain, RootKind::Project] {
            std::fs::write(&file, "before\n").unwrap();
            let ctx = ToolContext::new(&root).with_root_kind(kind);
            let out = EditTool::default().run(
                &ctx,
                &json!({ "path": "TETON.md", "old_string": "before", "new_string": "after" }),
            );
            assert!(!out.is_error, "{kind:?}: {}", out.content);
            assert_eq!(
                std::fs::read_to_string(&file).unwrap(),
                "after\n",
                "{kind:?}"
            );
        }
        std::fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn rejects_empty_old_string() {
        let root = temp_root("empty");
        std::fs::write(root.join("f.rs"), "a\n").unwrap();
        let ctx = ToolContext::new(&root);
        let out = EditTool::default().run(
            &ctx,
            &json!({ "path": "f.rs", "old_string": "", "new_string": "b" }),
        );
        assert!(out.is_error);
        std::fs::remove_dir_all(&root).ok();
    }
}
