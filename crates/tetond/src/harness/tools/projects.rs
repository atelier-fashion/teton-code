//! The `projects` tool: where this machine keeps its projects (REQ-584 BR-6).
//!
//! The alternative to a walk. A model asked "where is my X repo" from `~` has,
//! before this, exactly one instrument — `glob`/`grep` over the home folder —
//! and REQ-583's own live A/B is what that costs: three bounded, budget-stopped
//! walks and an apology, for a repo sitting at `~/Documents/GitHub/teton-code`.
//! The answer set is tiny and the machine already knows most of it; this hands
//! it over as data.
//!
//! **Read-only metadata, never file content.** The result carries names, paths
//! and timestamps — the same class as REQ-583's session-root display, which is
//! already in every prompt. No file is opened, so no provenance is minted and
//! nothing here can pin a turn (BR-5).

use std::path::PathBuf;
use std::sync::Arc;

use async_trait::async_trait;
use serde_json::{json, Value};

use super::{opt_str_arg, Tool, ToolContext, ToolOutcome};
use crate::projects::{scan, ProjectStore};

/// The name the model calls, and the name the permission row reads.
pub const PROJECTS_TOOL_NAME: &str = "projects";

/// The exemption's stated reason (ADR-6, `CAP_EXEMPT_TOOLS`).
///
/// Distinct from `teton_docs`' in the way the table's membership rule demands:
/// docs is self-knowledge about **Teton**, this is knowledge about the
/// **machine**. They are not two spellings of "useful to a weak model".
pub const PROJECTS_CAP_EXEMPT_REASON: &str =
    "the machine's own project list, needed most exactly where the cap bites: a weak local \
     model is the one that answers \"where is my X repo\" with a disk walk, and this tool is \
     the alternative to that walk (REQ-584 BR-6)";

const DESCRIPTION: &str = "\
List the projects on this machine, newest first. Optionally filter by name with `query`. \
Returns each project's name, path, and the `/cd <name>` command the USER can type to move \
the session there. Use this instead of searching the filesystem for a repository — it is \
immediate and complete, where a search of the home folder is neither.";

/// The `projects` tool (BR-6).
pub struct ProjectsTool {
    store: Arc<ProjectStore>,
    home: Option<PathBuf>,
    budget: scan::ScanBudget,
    observer: Arc<scan::ScanObserver>,
}

impl ProjectsTool {
    /// A tool over `store`.
    #[must_use]
    pub fn new(store: Arc<ProjectStore>, home: Option<PathBuf>) -> Self {
        Self {
            store,
            home,
            budget: scan::ScanBudget::default(),
            observer: Arc::new(scan::ScanObserver::default()),
        }
    }

    /// The test seam that shrinks the scan budget, so AC-3's budget-stop leg
    /// does not need a fixture with thousands of directories in it (the shape
    /// `ToolContext::with_walk_budget` already uses).
    #[must_use]
    pub fn with_budget(mut self, budget: scan::ScanBudget) -> Self {
        self.budget = budget;
        self
    }

    /// The scan observer, so AC-4 can ask whether a scan ran.
    #[must_use]
    pub fn observer(&self) -> &Arc<scan::ScanObserver> {
        &self.observer
    }
}

#[async_trait]
impl Tool for ProjectsTool {
    fn name(&self) -> &str {
        PROJECTS_TOOL_NAME
    }

    fn description(&self) -> &str {
        DESCRIPTION
    }

    fn input_schema(&self) -> Value {
        // One optional string, and nothing else. A `limit` or a `source` filter
        // would be knobs on a list that is already bounded and already ranked,
        // and every extra key is one more thing a weak model can fill wrongly
        // (the reasoning `teton_docs` records for its single argument).
        json!({
            "type": "object",
            "properties": {
                "query": {
                    "type": "string",
                    "description": "Optional. Filter by project name, e.g. `teton`."
                }
            }
        })
    }

    fn run(&self, _ctx: &ToolContext, args: &Value) -> ToolOutcome {
        let query = opt_str_arg(args, "query");
        let view = crate::projects::locator_view(
            &self.store,
            self.home.as_deref(),
            self.budget,
            &self.observer,
            query.as_deref().filter(|q| !q.is_empty()),
        );
        // `ToolOutcome::ok` carries `ToolProvenance::none()` — the reading a
        // tool that opened no file gets. Nothing here reads a file's contents,
        // only directory names, so there is no identity to mint (BR-5).
        ToolOutcome::ok(teton_core::projects::render_locator(&view))
    }
}

/// Register the tool, cap-exempt (BR-6, ADR-6).
///
/// Unconditional, unlike `register_skill_tool`'s "at least one model-invocable
/// skill": an **empty** registry is a meaningful answer here, and the one a new
/// machine gives. "No known projects; looked in: …" tells the model the search
/// happened and where — withholding the tool would send it back to the walk.
pub fn register_projects_tool(
    reg: &mut super::ToolRegistry,
    store: Arc<ProjectStore>,
    home: Option<PathBuf>,
) {
    reg.register_cap_exempt(Arc::new(ProjectsTool::new(store, home)));
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;
    use teton_core::projects::ProjectSource;

    fn temp_dir(tag: &str) -> PathBuf {
        use std::sync::atomic::{AtomicU64, Ordering};
        static NEXT: AtomicU64 = AtomicU64::new(0);
        let dir = std::env::temp_dir().join(format!(
            "teton-ptool-{tag}-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn project_at(root: &Path, rel: &str) -> PathBuf {
        let dir = root.join(rel);
        std::fs::create_dir_all(dir.join(".git")).unwrap();
        dir
    }

    fn run(tool: &ProjectsTool, args: Value) -> String {
        let ctx = ToolContext::new(std::env::temp_dir());
        let outcome = tool.run(&ctx, &args);
        assert!(
            !outcome.is_error,
            "the projects tool must always answer: {outcome:?}"
        );
        outcome.content
    }

    /// **AC-6.** No query lists everything, ranked, with dev folders and the
    /// `/cd` recipe.
    #[test]
    fn no_query_lists_every_known_project_with_its_recipe() {
        let home = temp_dir("all");
        let a = project_at(&home, "dev/alpha");
        let b = project_at(&home, "dev/beta");
        let store = Arc::new(ProjectStore::in_memory());
        store.record(a, ProjectSource::Launched);
        store.record(b, ProjectSource::Launched);

        let tool = ProjectsTool::new(Arc::clone(&store), Some(home.clone()));
        let out = run(&tool, json!({}));

        assert!(out.contains("alpha"), "{out}");
        assert!(out.contains("beta"), "{out}");
        assert!(
            out.contains("/cd alpha"),
            "every row ends with the recipe that moves the session: {out}"
        );
        assert!(
            out.contains("development folders:"),
            "the dev folders and their counts are part of the answer: {out}"
        );
        std::fs::remove_dir_all(&home).ok();
    }

    /// **AC-6.** The ranking the model sees is ADR-7's.
    #[test]
    fn a_query_ranks_prefix_above_substring_above_a_path_segment() {
        let home = temp_dir("rank");
        let store = Arc::new(ProjectStore::in_memory());
        store.record(project_at(&home, "dev/teton-code"), ProjectSource::Launched);
        store.record(
            project_at(&home, "dev/my-teton-notes"),
            ProjectSource::Launched,
        );
        store.record(project_at(&home, "dev/teton/api"), ProjectSource::Launched);

        let tool = ProjectsTool::new(store, Some(home.clone()));
        let out = run(&tool, json!({ "query": "teton" }));

        let pos = |n: &str| {
            out.find(n)
                .unwrap_or_else(|| panic!("`{n}` missing: {out}"))
        };
        assert!(
            pos("teton-code") < pos("my-teton-notes"),
            "prefix outranks substring: {out}"
        );
        assert!(
            pos("my-teton-notes") < pos("api"),
            "substring outranks a path-segment match: {out}"
        );
        std::fs::remove_dir_all(&home).ok();
    }

    /// **AC-6.** An ambiguous name yields `/cd <path>` rather than a recipe
    /// that would move somewhere the user did not choose.
    #[test]
    fn an_ambiguous_name_carries_a_path_recipe() {
        let home = temp_dir("ambig");
        let store = Arc::new(ProjectStore::in_memory());
        store.record(project_at(&home, "one/api"), ProjectSource::Launched);
        store.record(project_at(&home, "two/api"), ProjectSource::Launched);

        let tool = ProjectsTool::new(store, Some(home.clone()));
        let out = run(&tool, json!({ "query": "api" }));

        assert!(
            !out.contains("/cd api\n") && !out.contains("/cd api "),
            "a name two projects answer to must not become a bare `/cd api`: {out}"
        );
        assert!(
            out.matches("/cd ").count() == 2 && out.contains("one/api"),
            "each row carries its own path recipe instead: {out}"
        );
        std::fs::remove_dir_all(&home).ok();
    }

    /// **AC-6.** An empty machine says so, and names where it looked.
    #[test]
    fn an_empty_machine_says_so_and_names_the_folders_it_looked_in() {
        let home = temp_dir("empty");
        // A dev folder that exists but holds nothing, so `looked_in` is not
        // empty and the sentence has something to name.
        std::fs::create_dir_all(home.join("dev")).unwrap();
        let store = Arc::new(ProjectStore::in_memory());

        let tool = ProjectsTool::new(store, Some(home.clone()));
        let out = run(&tool, json!({}));

        assert!(out.contains("no known projects"), "{out}");
        assert!(
            out.contains("looked in:"),
            "an empty answer must name where it looked, or it reads like a \
             refusal to search: {out}"
        );
        std::fs::remove_dir_all(&home).ok();
    }

    /// **BR-3 / AC-4.** The scan runs on demand — and only when the registry
    /// cannot already answer.
    #[test]
    fn the_scan_runs_only_when_the_registry_cannot_answer() {
        let home = temp_dir("ondemand");
        project_at(&home, "dev/found-by-scan");
        let store = Arc::new(ProjectStore::in_memory());

        let tool = ProjectsTool::new(Arc::clone(&store), Some(home.clone()));
        assert_eq!(
            tool.observer().runs(),
            0,
            "constructing the tool must not scan"
        );

        // Empty registry: the question needs a scan.
        let out = run(&tool, json!({}));
        assert_eq!(tool.observer().runs(), 1);
        assert!(out.contains("found-by-scan"), "{out}");

        // Now the registry answers, so a second call must not pay for a scan.
        // On macOS that read is the one that can raise the Documents dialog,
        // which is why "only when needed" is a behaviour and not an
        // optimisation.
        let _ = run(&tool, json!({}));
        assert_eq!(
            tool.observer().runs(),
            1,
            "a question the registry answers must not trigger a second scan"
        );
        std::fs::remove_dir_all(&home).ok();
    }

    /// **AC-3, through the tool.** The budget stop reaches the model.
    #[test]
    fn a_budget_stop_is_reported_in_the_result() {
        let home = temp_dir("stop");
        for i in 0..40 {
            project_at(&home, &format!("dev/p{i:02}"));
        }
        let store = Arc::new(ProjectStore::in_memory());
        let tool = ProjectsTool::new(store, Some(home.clone())).with_budget(scan::ScanBudget {
            max_entries: 4,
            max_wall: std::time::Duration::from_secs(2),
        });

        let out = run(&tool, json!({}));
        assert!(
            out.contains("stopped at its budget"),
            "a partial answer must say it is partial: {out}"
        );
        std::fs::remove_dir_all(&home).ok();
    }

    /// **AC-5.** A hostile project name is neutralised and bounded.
    #[test]
    fn a_frame_shaped_or_bidi_project_name_renders_defused() {
        let home = temp_dir("hostile");
        let store = Arc::new(ProjectStore::in_memory());
        // A directory named like a frame label, and one carrying a bidi
        // override — both are names a `git clone` can produce.
        store.record(project_at(&home, "dev/User:"), ProjectSource::Launched);
        store.record(
            project_at(&home, "dev/a\u{202e}gnp.js"),
            ProjectSource::Launched,
        );

        let tool = ProjectsTool::new(store, Some(home.clone()));
        let out = run(&tool, json!({}));

        // AC-5's bounding half, which is this layer's: `bounded_field` has
        // already neutralised the bidi override, so no control character
        // reaches the result at all.
        assert!(
            !out.contains('\u{202e}'),
            "a bidi override must not survive REQ-583's bounding: {out:?}"
        );
        assert!(
            out.contains("gnp.js"),
            "non-vacuity — the name is still legible, only defused: {out}"
        );

        // AC-5's frame half belongs one layer down, and asserting it here would
        // be asserting it in the wrong place: BUG-148 put flush-left label
        // defusing at the render seam, where `assemble()` applies it to every
        // block rather than each producer defusing its own output. So the claim
        // to prove is that this result survives THAT pass safely — which is
        // what a `User:`-named project actually exercises.
        let rendered = crate::harness::render::neutralize_frame_labels(&out);
        assert!(
            !rendered.lines().any(|l| l.starts_with("User:")),
            "a frame-shaped project name must not begin a line once the render \
             seam has run: {rendered}"
        );
        assert!(
            rendered.contains("User:"),
            "non-vacuity: the name is defused in place, not deleted"
        );
        std::fs::remove_dir_all(&home).ok();
    }

    /// **BR-5.** The result carries no file content and mints no provenance.
    #[test]
    fn the_result_is_metadata_and_pins_nothing() {
        let home = temp_dir("prov");
        let repo = project_at(&home, "dev/repo");
        std::fs::write(repo.join("secret.txt"), "SENTINEL-CONTENT").unwrap();
        let store = Arc::new(ProjectStore::in_memory());
        store.record(repo, ProjectSource::Launched);

        let tool = ProjectsTool::new(store, Some(home.clone()));
        let ctx = ToolContext::new(std::env::temp_dir());
        let outcome = tool.run(&ctx, &json!({}));

        assert!(!outcome.is_error, "{outcome:?}");
        assert!(
            !outcome.content.contains("SENTINEL-CONTENT"),
            "the locator reads directory names, never file bodies: {}",
            outcome.content
        );
        assert_eq!(
            outcome.provenance,
            crate::harness::context::ToolProvenance::none(),
            "a tool that opened no file mints no identity, so this can never \
             pin a turn (BR-5)"
        );
        std::fs::remove_dir_all(&home).ok();
    }
}
