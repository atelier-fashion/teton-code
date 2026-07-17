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

use serde_json::Value;

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

/// The result of running a tool: text folded back into the model's context, plus
/// a flag distinguishing a failure from a success.
///
/// A failed tool call is a first-class outcome, not an exception: the loop folds
/// `content` into context so the model can react. `is_error` lets the loop mark
/// it visibly (and lets a verification step tell a real failure from a pass).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ToolOutcome {
    /// Text shown to the model as the tool result.
    pub content: String,
    /// Whether the call failed (rejected edit, jail violation, timeout, …).
    pub is_error: bool,
}

impl ToolOutcome {
    /// A successful outcome.
    pub fn ok(content: impl Into<String>) -> Self {
        Self {
            content: content.into(),
            is_error: false,
        }
    }

    /// A failed outcome the model must see and react to.
    pub fn error(content: impl Into<String>) -> Self {
        Self {
            content: content.into(),
            is_error: true,
        }
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

/// A built-in agent tool. Synchronous, jailed, and infallible from the loop's
/// point of view (failures come back as `ToolOutcome { is_error: true }`).
pub trait Tool: Send + Sync {
    /// Stable tool name the model calls it by.
    fn name(&self) -> &str;

    /// One-line, model-facing description.
    fn description(&self) -> &str;

    /// JSON Schema for the tool's arguments (rendered into the prompt).
    fn input_schema(&self) -> Value;

    /// Run the tool against `args`, jailed to `ctx`.
    fn run(&self, ctx: &ToolContext, args: &Value) -> ToolOutcome;
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
