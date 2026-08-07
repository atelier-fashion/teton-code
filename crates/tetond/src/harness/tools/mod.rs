//! Built-in agent tools: the small, verified tool set the loop dispatches.
//!
//! The tool set is deliberately tiny — read, edit, glob, grep, shell — because
//! the harness is designed for **weak models** first (the product thesis, BR-6):
//! a short loop over a handful of legible tools that a small local model can
//! drive reliably, with a mandatory verification step. Strong models simply get
//! a longer leash (a higher `max_turns`), not a different shape.
//!
//! Every tool runs inside a **repo-root jail** ([`ToolContext`]): a path that
//! escapes the root — via `..`, an absolute path, or a symlink that resolves
//! outside — is refused before any I/O. Tools never panic and never propagate an
//! error to the loop; an internal failure is folded into a [`ToolOutcome`] with
//! `is_error = true` so the *model* sees it and can retry (never a silent
//! success — AC).

use std::path::{Component, Path, PathBuf};
use std::sync::Arc;

use async_trait::async_trait;
use serde_json::Value;

use super::context::ToolProvenance;
use super::duty::DutyRoute;

pub mod edit;
pub mod glob;
pub mod grep;
pub mod mcp;
pub mod read;
pub mod shell;

pub use edit::EditTool;
pub use glob::GlobTool;
pub use grep::GrepTool;
pub use mcp::{register_mcp_tools, McpToolHandle};
pub use read::ReadTool;
pub use shell::ShellTool;

/// Shared execution context for every tool: the repo-root jail.
///
/// All file access resolves relative to [`ToolContext::repo_root`] and is
/// verified to stay within it. The shell tool additionally runs with this as its
/// working directory and a scrubbed environment (see [`shell`]).
#[derive(Debug, Clone)]
pub struct ToolContext {
    repo_root: PathBuf,
}

impl ToolContext {
    /// A context jailed to `repo_root`.
    pub fn new(repo_root: impl Into<PathBuf>) -> Self {
        Self {
            repo_root: repo_root.into(),
        }
    }

    /// The jail root.
    #[must_use]
    pub fn repo_root(&self) -> &Path {
        &self.repo_root
    }

    /// Resolve a caller-supplied path against the jail, refusing any path that
    /// escapes the repo root.
    ///
    /// Relative paths join onto the root; absolute paths are taken as-is and then
    /// checked. `.`/`..` are collapsed lexically, and existing paths are
    /// canonicalized so a symlink pointing outside the root is caught too.
    ///
    /// # Errors
    /// Returns [`ToolError::Jail`] when the resolved path is not inside the root
    /// (or the root itself cannot be resolved).
    pub fn resolve(&self, raw: &str) -> Result<PathBuf, ToolError> {
        let root = self
            .repo_root
            .canonicalize()
            .map_err(|_| ToolError::jail("repo root does not exist"))?;

        let joined = if Path::new(raw).is_absolute() {
            PathBuf::from(raw)
        } else {
            root.join(raw)
        };
        let normalized = lexical_normalize(&joined);

        // Canonicalize when the target exists so a symlink cannot tunnel out of
        // the jail; fall back to the lexical form for not-yet-created paths.
        let checked = normalized.canonicalize().unwrap_or(normalized);

        if !checked.starts_with(&root) {
            return Err(ToolError::jail(format!(
                "path `{raw}` escapes the repo root"
            )));
        }
        Ok(checked)
    }
}

/// Collapse `.` and `..` components lexically, without touching the filesystem.
fn lexical_normalize(path: &Path) -> PathBuf {
    let mut out = PathBuf::new();
    for comp in path.components() {
        match comp {
            Component::ParentDir => {
                out.pop();
            }
            Component::CurDir => {}
            other => out.push(other.as_os_str()),
        }
    }
    out
}

/// The result of running a tool: text folded back into the model's context, a
/// flag distinguishing a failure from a success, and the egress
/// [`ToolProvenance`] of the files the tool touched (REQ-544 C-1).
///
/// A failed tool call is a first-class outcome, not an exception: the loop folds
/// `content` into context so the model can react. `is_error` lets the loop mark
/// it visibly (and lets a verification step tell a real failure from a pass).
/// `provenance` is what the loop stamps onto the context block so BR-1 egress
/// enforcement sees the files a tool *actually* touched — the `read` path,
/// `grep`/`glob`'s surfaced files, an MCP call's path arguments — or
/// [`ToolProvenance::Unknown`] for `shell`, whose touched files are unknowable.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ToolOutcome {
    /// Text shown to the model as the tool result.
    pub content: String,
    /// Whether the call failed (rejected edit, jail violation, timeout, …).
    pub is_error: bool,
    /// The files this result was derived from (or `Unknown`). Defaults to no
    /// provenance for tools that surface no repo-file content.
    pub provenance: ToolProvenance,
    /// **How much this call found** — the one number [`Tool::refine`]'s duty
    /// trigger is decided on, carried beside `content` rather than re-derived
    /// from it (REQ-561 verify M3).
    ///
    /// The unit is the measuring tool's own and only that tool's `refine` reads
    /// it. `None` means **nothing was measured** — the call did not get far
    /// enough to produce a result of the kind this number describes, which for
    /// `shell` is exactly "no command ran".
    ///
    /// **It is not a total, and only one tool measures past its own cap.** Read
    /// it as "at least this much", never as "exactly this much":
    ///
    /// - `shell` counts characters of stdout+stderr **before** the
    ///   8,000-character cap, so it is the true length — that is the whole of
    ///   ADR-5, since the reason to interpret a *successful* command is that the
    ///   cap threw information away, and a post-cap length could never say so.
    /// - `grep` counts matching lines, but its search stops walking at the
    ///   200-match cap, so a 5,000-hit pattern reports about 201. Only
    ///   `>= TRIAGE_MIN_MATCHES` is asked of it, and that survives the
    ///   under-count; a caller wanting a hit *total* would have to change the
    ///   search, not read this field harder.
    ///
    /// It is a field and not a line in `content` because `content` is
    /// *model-visible text*, and every tool that recovered this number by
    /// parsing its own rendered prose got it wrong in the same way. `grep`
    /// answers a zero-hit search in prose, so a pattern containing a newline —
    /// which can never match, since the search compares against
    /// `contents.lines()` — read back as two "matches" and bought a `triage`
    /// call that then rebuilt the message out of its own words. `shell` read a
    /// length off its last line, which command output can forge. Neither is
    /// reachable through a number the tool hands over directly.
    pub measured: Option<usize>,
}

impl ToolOutcome {
    /// A successful outcome with no file provenance and nothing measured.
    pub fn ok(content: impl Into<String>) -> Self {
        Self {
            content: content.into(),
            is_error: false,
            provenance: ToolProvenance::none(),
            measured: None,
        }
    }

    /// A failed outcome the model must see and react to (no file provenance).
    pub fn error(content: impl Into<String>) -> Self {
        Self {
            content: content.into(),
            is_error: true,
            provenance: ToolProvenance::none(),
            measured: None,
        }
    }

    /// Record what this call found, in the measuring tool's own unit — see
    /// [`ToolOutcome::measured`].
    #[must_use]
    pub fn measuring(mut self, measured: usize) -> Self {
        self.measured = Some(measured);
        self
    }

    /// Tag this outcome with the [`ToolProvenance`] of the files it touched.
    #[must_use]
    pub fn with_provenance(mut self, provenance: ToolProvenance) -> Self {
        self.provenance = provenance;
        self
    }

    /// Tag this outcome with the set of repo-relative `paths` it read/enumerated.
    #[must_use]
    pub fn with_paths<I, S>(self, paths: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        self.with_provenance(ToolProvenance::paths(paths))
    }

    /// Tag this outcome as having indeterminate provenance (fail-closed at
    /// egress) — the `shell` tool, whose touched files cannot be parsed.
    #[must_use]
    pub fn with_unknown_provenance(self) -> Self {
        self.with_provenance(ToolProvenance::Unknown)
    }
}

impl From<ToolError> for ToolOutcome {
    fn from(err: ToolError) -> Self {
        ToolOutcome::error(err.to_string())
    }
}

/// A failure inside a tool. Always converted into a [`ToolOutcome`] before it
/// reaches the loop — the model, not the daemon, handles tool failures.
#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum ToolError {
    /// The path escaped the repo-root jail.
    #[error("path jail violation: {0}")]
    Jail(String),
    /// The tool arguments were missing or the wrong shape.
    #[error("invalid arguments: {0}")]
    Args(String),
    /// A filesystem or process error (message never carries file content).
    #[error("{0}")]
    Io(String),
}

impl ToolError {
    /// A jail-violation error.
    pub fn jail(msg: impl Into<String>) -> Self {
        Self::Jail(msg.into())
    }

    /// An argument error.
    pub fn args(msg: impl Into<String>) -> Self {
        Self::Args(msg.into())
    }

    /// An I/O error.
    pub fn io(msg: impl Into<String>) -> Self {
        Self::Io(msg.into())
    }
}

/// The harness duties a tool may refine its **own** result through, resolved for
/// this turn (REQ-561).
///
/// One field per tool-owned duty, and nothing else: the route type, the trait,
/// the local and remote implementations, the egress scoping and the output
/// ceiling all live once on the shared [`duty`](super::duty) seam (BR-6, ADR-1).
/// This is the wire that carries a resolved route from the runtime — which owns
/// the router — down to the tool that knows what to do with it.
///
/// It exists so a duty is never selected by **matching on a tool name** (BR-1).
/// The loop hands the same context to every tool and each tool answers for
/// itself, so no string comparison anywhere assigns a category.
pub struct ToolDuties<'a> {
    /// The `triage` category, resolved for this turn (TASK-060). Ranks a `grep`
    /// result against the request before it enters context.
    pub triage: &'a DutyRoute,
    /// The `shell` category, resolved for this turn (TASK-061). Says what a
    /// command's output means — but only when the command failed or its output
    /// was capped, so an ordinary command costs nothing.
    pub shell: &'a DutyRoute,
}

/// A tool result after its own tool had the chance to refine it through a duty.
///
/// The degradation is on the **value**, not only in a log (BR-3): a caller that
/// wants to surface "the duty could not be served" — a log line, an event, a
/// test assertion — reads it here rather than inferring it from the text. This
/// mirrors [`SummarizeOutcome`](super::context::SummarizeOutcome), the duty
/// outcome the loop already handles.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RefinedOutcome {
    /// What to fold into context: the refined result, or — on any failure — the
    /// tool's own result, unchanged.
    pub outcome: ToolOutcome,
    /// Why the tool fell back to its own unrefined result, when it did. `None`
    /// when the duty served, and `None` when the tool has no duty or did not
    /// need one.
    pub duty_error: Option<String>,
}

impl RefinedOutcome {
    /// `outcome` as it stands: no duty ran, so there is nothing to report.
    #[must_use]
    pub fn unrefined(outcome: ToolOutcome) -> Self {
        Self {
            outcome,
            duty_error: None,
        }
    }

    /// `outcome` as it stands, because the duty could not be served — carrying
    /// the reason so the caller can surface it (LESSON-447).
    #[must_use]
    pub fn degraded(outcome: ToolOutcome, error: impl Into<String>) -> Self {
        Self {
            outcome,
            duty_error: Some(error.into()),
        }
    }
}

/// A built-in agent tool. Jailed, and infallible from the loop's point of view
/// (failures come back as `ToolOutcome { is_error: true }`).
///
/// [`Tool::run`] is **synchronous**: tool work is filesystem and process I/O,
/// and the loop dispatches it inline. [`Tool::refine`] is the async half, for
/// the one thing a tool cannot do synchronously — make a model call.
#[async_trait]
pub trait Tool: Send + Sync {
    /// Stable tool name the model calls it by.
    fn name(&self) -> &str;

    /// One-line, model-facing description.
    fn description(&self) -> &str;

    /// JSON Schema for the tool's arguments (rendered into the prompt).
    fn input_schema(&self) -> Value;

    /// Run the tool against `args`, jailed to `ctx`.
    fn run(&self, ctx: &ToolContext, args: &Value) -> ToolOutcome;

    /// Refine this tool's own `outcome` through the harness duty this tool owns,
    /// given the `request` the turn is serving and the `args` it was called with.
    ///
    /// **The default is identity**, and most tools keep it: a tool with no duty
    /// of its own returns exactly what it produced.
    ///
    /// This method is why a duty is never chosen by a name comparison in the
    /// loop (BR-1). The loop calls it for every tool result; the tool that
    /// *is* `grep` is the thing that knows a `grep` result wants ranking, so
    /// nothing has to read a tool name — or the result text — to work that out.
    ///
    /// It is separate from [`Tool::run`] because refinement is a **model call**,
    /// which is async and belongs on the loop's async path: doing it inside
    /// `run` would mean blocking a runtime worker for the length of an
    /// inference. Implementations must hold [`Tool::run`]'s contract: an
    /// implementation that cannot serve returns the outcome it was given,
    /// unchanged, with the reason on
    /// [`RefinedOutcome::duty_error`](RefinedOutcome::duty_error).
    async fn refine(
        &self,
        _args: &Value,
        _request: &str,
        _duties: &ToolDuties<'_>,
        outcome: ToolOutcome,
    ) -> RefinedOutcome {
        RefinedOutcome::unrefined(outcome)
    }
}

/// The set of tools available to a session.
///
/// Insertion order is the exposure order: [`ToolRegistry::docs`] can be capped to
/// the first `max_tools` for a degraded (weak) provider (BR-6), so put the most
/// load-bearing tools first.
pub struct ToolRegistry {
    tools: Vec<Arc<dyn Tool>>,
}

impl ToolRegistry {
    /// An empty registry.
    #[must_use]
    pub fn new() -> Self {
        Self { tools: Vec::new() }
    }

    /// A registry with the full built-in tool set, in weak-model priority order:
    /// read, edit, grep, glob, shell.
    #[must_use]
    pub fn with_builtins() -> Self {
        let mut reg = Self::new();
        reg.register(Arc::new(ReadTool));
        reg.register(Arc::new(EditTool));
        reg.register(Arc::new(GrepTool));
        reg.register(Arc::new(GlobTool));
        reg.register(Arc::new(ShellTool::default()));
        reg
    }

    /// Add a tool (later registrations with the same name shadow earlier ones on
    /// lookup order but are kept for exposure ordering — register uniquely).
    pub fn register(&mut self, tool: Arc<dyn Tool>) {
        self.tools.push(tool);
    }

    /// Look up a tool by name.
    #[must_use]
    pub fn get(&self, name: &str) -> Option<&Arc<dyn Tool>> {
        self.tools.iter().find(|t| t.name() == name)
    }

    /// Every registered tool name, in exposure order.
    #[must_use]
    pub fn names(&self) -> Vec<&str> {
        self.tools.iter().map(|t| t.name()).collect()
    }

    /// Number of registered tools.
    #[must_use]
    pub fn len(&self) -> usize {
        self.tools.len()
    }

    /// Whether the registry is empty.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.tools.is_empty()
    }

    /// Dispatch a call by name. An unknown tool is a failed outcome the model
    /// sees (with the list of valid tools), never a panic — so a weak model that
    /// hallucinates a tool name is corrected rather than crashing the loop.
    #[must_use]
    pub fn dispatch(&self, name: &str, ctx: &ToolContext, args: &Value) -> ToolOutcome {
        match self.get(name) {
            Some(tool) => tool.run(ctx, args),
            None => ToolOutcome::error(format!(
                "unknown tool `{name}`; available tools: {}",
                self.names().join(", ")
            )),
        }
    }

    /// Give the tool named `name` the chance to refine its own `outcome`
    /// through its duty (REQ-561).
    ///
    /// The name resolves the same tool [`Self::dispatch`] just ran, so this is
    /// not a category being inferred from a string — it is the same lookup the
    /// model's own tool call already made, asked a second question. A name that
    /// resolves to nothing cannot have produced this outcome in the first place;
    /// it is returned unrefined rather than treated as an error, because
    /// `dispatch` already reported the unknown tool to the model.
    pub async fn refine(
        &self,
        name: &str,
        args: &Value,
        request: &str,
        duties: &ToolDuties<'_>,
        outcome: ToolOutcome,
    ) -> RefinedOutcome {
        match self.get(name) {
            Some(tool) => tool.refine(args, request, duties, outcome).await,
            None => RefinedOutcome::unrefined(outcome),
        }
    }

    /// Model-facing documentation for the exposed tools, capped to `max_tools`
    /// when set (BR-6: a degraded provider gets a smaller tool set).
    #[must_use]
    pub fn docs(&self, max_tools: Option<u32>) -> String {
        let limit = max_tools
            .map(|n| n as usize)
            .unwrap_or(self.tools.len())
            .min(self.tools.len());
        let mut out = String::new();
        for tool in &self.tools[..limit] {
            out.push_str(&format!(
                "- {}: {}\n  arguments: {}\n",
                tool.name(),
                tool.description(),
                tool.input_schema()
            ));
        }
        out
    }

    /// The names actually exposed under a `max_tools` cap (BR-6).
    #[must_use]
    pub fn exposed_names(&self, max_tools: Option<u32>) -> Vec<&str> {
        let limit = max_tools
            .map(|n| n as usize)
            .unwrap_or(self.tools.len())
            .min(self.tools.len());
        self.tools[..limit].iter().map(|t| t.name()).collect()
    }
}

impl Default for ToolRegistry {
    fn default() -> Self {
        Self::new()
    }
}

// ---------------------------------------------------------------------------
// Shared argument helpers
// ---------------------------------------------------------------------------

/// Extract a required string argument.
pub(crate) fn str_arg(args: &Value, key: &str) -> Result<String, ToolError> {
    args.get(key)
        .and_then(Value::as_str)
        .map(str::to_owned)
        .ok_or_else(|| ToolError::args(format!("missing required string argument `{key}`")))
}

/// Extract an optional string argument.
pub(crate) fn opt_str_arg(args: &Value, key: &str) -> Option<String> {
    args.get(key).and_then(Value::as_str).map(str::to_owned)
}

/// Extract an optional unsigned-integer argument.
pub(crate) fn opt_u64_arg(args: &Value, key: &str) -> Option<u64> {
    args.get(key).and_then(Value::as_u64)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_root(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "teton-tooljail-{tag}-{}-{}",
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
    fn resolve_accepts_paths_inside_the_jail() {
        let root = temp_root("in");
        std::fs::write(root.join("a.txt"), "hi").unwrap();
        let ctx = ToolContext::new(&root);
        let resolved = ctx.resolve("a.txt").unwrap();
        assert!(resolved.starts_with(root.canonicalize().unwrap()));
        std::fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn resolve_rejects_dotdot_escape() {
        let root = temp_root("esc");
        let ctx = ToolContext::new(&root);
        let err = ctx.resolve("../../etc/passwd").unwrap_err();
        assert!(matches!(err, ToolError::Jail(_)));
        std::fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn resolve_rejects_absolute_outside_root() {
        let root = temp_root("abs");
        let ctx = ToolContext::new(&root);
        let err = ctx.resolve("/etc/hosts").unwrap_err();
        assert!(matches!(err, ToolError::Jail(_)));
        std::fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn dispatch_reports_unknown_tools_to_the_model() {
        let reg = ToolRegistry::with_builtins();
        let ctx = ToolContext::new(std::env::temp_dir());
        let outcome = reg.dispatch("nonexistent", &ctx, &serde_json::json!({}));
        assert!(outcome.is_error);
        assert!(outcome.content.contains("unknown tool"));
        assert!(outcome.content.contains("read"));
    }

    #[test]
    fn docs_are_capped_by_max_tools_for_degraded_providers() {
        let reg = ToolRegistry::with_builtins();
        assert_eq!(reg.exposed_names(None).len(), 5);
        assert_eq!(reg.exposed_names(Some(2)), vec!["read", "edit"]);
        assert!(reg.docs(Some(1)).contains("read"));
        assert!(!reg.docs(Some(1)).contains("shell"));
    }
}
