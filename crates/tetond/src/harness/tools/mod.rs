//! Built-in agent tools: the small, verified tool set the loop dispatches.
//!
//! The tool set is deliberately tiny — read, edit, glob, grep, shell, and
//! `teton_docs` — because the harness is designed for **weak models** first
//! (the product thesis, BR-6): a short loop over a handful of legible tools that
//! a small local model can drive reliably, with a mandatory verification step.
//! Strong models simply get a longer leash (a higher `max_turns`), not a
//! different shape.
//!
//! Five of the six answer questions about the *user's* repository.
//! [`docs`] answers questions about Teton, out of bodies compiled into this
//! binary, and is the only built-in that touches no path at all (REQ-577).
//!
//! Every tool runs inside a **session-root jail** ([`ToolContext`]): a path that
//! escapes the root — via `..`, an absolute path, or a symlink that resolves
//! outside — is refused before any I/O, and the refusal names the root (REQ-583
//! BR-2). Tools never panic and never propagate an error to the loop; an
//! internal failure is folded into a [`ToolOutcome`] with `is_error = true` so
//! the *model* sees it and can retry (never a silent success — AC).
//!
//! # Symlink posture splits by tool class (REQ-571 ADR-C, BR-5)
//!
//! Explicit single-file access and directory traversal carry different risks, so
//! they answer links differently:
//!
//! - **`read` / `edit`** resolve the link in [`ToolContext::resolve`]. Inside the
//!   root the *target* is the identity — the link name is not — so a link to a
//!   boundary file is judged as that boundary file. Outside the root the call is
//!   refused by the jail.
//! - **`grep` / `glob`** skip link entries outright, wherever they resolve, via
//!   [`skip_symlink_entry`]. A walker that follows links surfaces one file under
//!   two names — two provenance ids for one identity, which ADR-A exists to
//!   prevent — and can cycle.

use std::path::{Component, Path, PathBuf};
use std::sync::Arc;

use async_trait::async_trait;
use serde_json::Value;
use teton_core::session_root::{bounded_field, display_for, DISPLAY_MAX_CHARS};
use teton_core::ProvenanceId;
use teton_protocol::methods::{RootKind, SessionRoot};

use super::context::ToolProvenance;
use super::duty::DutyRoute;

pub mod docs;
pub mod edit;
pub mod glob;
pub mod grep;
pub mod mcp;
pub mod read;
pub mod shell;
pub mod walk;
pub mod web;

pub use docs::{DocsTool, DOCS_TOOL_NAME};
pub use edit::EditTool;
pub use glob::GlobTool;
pub use grep::GrepTool;
pub use mcp::{register_mcp_tools, McpToolHandle};
pub use read::ReadTool;
pub use shell::ShellTool;
pub use web::{register_web_tool, WebTool, WEB_TOOL_NAME};

/// Shared execution context for every tool: the session-root jail.
///
/// All file access resolves relative to [`ToolContext::repo_root`] and is
/// verified to stay within it. The shell tool additionally runs with this as its
/// working directory and a scrubbed environment (see [`shell`]).
///
/// # The root is carried, not re-derived (REQ-583 ADR-1)
///
/// Beside the path the context carries the root's *display* spelling — what a
/// jail refusal names (BR-2) — and its *kind* — what the walkers key their
/// home-tree pruning on (ADR-3) and the shell's timeout hint reads (BR-14).
/// Both are the daemon's derivation, probed once per turn by
/// `tetond::session_root::probe` and handed in through [`ToolContext::for_root`],
/// so a refusal prints the same bytes as the environment block. [`ToolContext::new`]
/// is the plain constructor every existing call site uses: kind `Plain`, display
/// spelled from the path and `HOME` — the same rule, no probe.
#[derive(Debug, Clone)]
pub struct ToolContext {
    repo_root: PathBuf,
    /// The root's display spelling, already bounded (ADR-2).
    display: String,
    /// What kind of place the root is.
    kind: RootKind,
    /// The walk policy every walker under this context runs under (ADR-3).
    walk: walk::WalkPolicy,
}

impl ToolContext {
    /// A context jailed to `repo_root`, of kind [`RootKind::Plain`], displayed
    /// home-relative when `HOME` is set.
    ///
    /// The display rule itself is pure ([`display_for`]), and `HOME` is the
    /// daemon's one fact ([`crate::session_root::home`], the same reader the
    /// probe uses) — this constructor stands in for a caller that has not probed
    /// the root.
    pub fn new(repo_root: impl Into<PathBuf>) -> Self {
        let repo_root = repo_root.into();
        let home = crate::session_root::home();
        let display = bounded_field(&display_for(&repo_root, home.as_deref()), DISPLAY_MAX_CHARS);
        Self {
            repo_root,
            display,
            kind: RootKind::Plain,
            walk: walk::WalkPolicy::default(),
        }
    }

    /// A context jailed to `path`, carrying the display and kind the daemon
    /// probed for it — the runtime's constructor (ADR-1).
    ///
    /// `probed` is the probe's answer for `path`; the runtime hands both halves
    /// of one `session_root::ProbedRoot` here, so the jail's path and the probed
    /// view cannot come from two readings. The path rides alongside rather than
    /// inside the wire view because that view deliberately carries the display
    /// spelling only, never the absolute path.
    pub fn for_root(path: impl Into<PathBuf>, probed: &SessionRoot) -> Self {
        Self {
            repo_root: path.into(),
            display: probed.display.clone(),
            kind: probed.kind,
            walk: walk::WalkPolicy::default(),
        }
    }

    /// The same context with its root kind replaced — the test seam a walker's
    /// home-tree pruning (AC-16) and the shell's timeout hint (AC-19) are
    /// exercised through, without a real home folder as the jail.
    #[must_use]
    pub fn with_root_kind(mut self, kind: RootKind) -> Self {
        self.kind = kind;
        self
    }

    /// The same context with its walk budget replaced — the test seam a
    /// walker's stopped line (AC-14/AC-15) is exercised through, without a
    /// giant fixture or a sleep.
    #[must_use]
    pub fn with_walk_budget(mut self, budget: walk::WalkBudget) -> Self {
        self.walk = self.walk.with_budget(budget);
        self
    }

    /// The walk policy every walker under this context reads (ADR-3, BR-11).
    #[must_use]
    pub fn walk_policy(&self) -> &walk::WalkPolicy {
        &self.walk
    }

    /// The session root's path — the jail. Named for its call sites and for
    /// `TETON_REPO_ROOT`, the daemon-wide fallback it defaults to; the term the
    /// model and the user see is "session root" (BR-3).
    #[must_use]
    pub fn repo_root(&self) -> &Path {
        &self.repo_root
    }

    /// The one refusal for a session root that is not there — printed by
    /// [`Self::resolve`], `glob`, `grep` and `shell` alike, so the four sites
    /// cannot drift into four sentences. Names the display, as every jail
    /// refusal does (BR-2).
    #[must_use]
    pub fn root_missing_error(&self) -> ToolError {
        ToolError::jail(format!("the session root {} does not exist", self.display))
    }

    /// What kind of place the jail root is.
    #[must_use]
    pub fn root_kind(&self) -> RootKind {
        self.kind
    }

    /// The jail root's display spelling — the one every refusal names (BR-2),
    /// already bounded.
    #[must_use]
    pub fn root_display(&self) -> &str {
        &self.display
    }

    /// Resolve a caller-supplied path against the jail, refusing any path that
    /// escapes the session root.
    ///
    /// Relative paths join onto the root; absolute paths are taken as-is and then
    /// checked. `.`/`..` are collapsed lexically, and the result is resolved
    /// against the filesystem — through the deepest ancestor that exists, so a
    /// symlink pointing outside the root is caught whether or not the leaf has
    /// been created yet (`canonical_through_existing_ancestor`, below).
    ///
    /// # A link is answered by its target (REQ-571 ADR-C)
    ///
    /// Canonicalization is what implements the `read`/`edit` half of the
    /// split-by-tool-class posture: an in-root link resolves to its target, so
    /// both halves of the identity below describe the file actually opened and a
    /// link named `notes.txt` pointing at `secrets/prod.env` is judged as
    /// `secrets/prod.env`. A link resolving outside the root fails the
    /// `starts_with` check and is refused. Walking tools take the opposite
    /// posture — see [`skip_symlink_entry`].
    ///
    /// # One call yields both halves of the identity (REQ-571 ADR-B)
    ///
    /// The return is a [`Resolved`], not a bare path: the file the tool opens and
    /// the [`ProvenanceId`] the privacy boundary matches on are derived here,
    /// together, from the same canonical value — so the gate decides on the parse
    /// the executor used and the two cannot drift (LESSON-494). A tool that
    /// resolved a path and then tagged provenance from its own argument is not a
    /// mistake this signature permits.
    ///
    /// # Errors
    /// Returns [`ToolError::Jail`] when the resolved path is not inside the root
    /// (or the root itself cannot be resolved), when the path cannot be resolved
    /// at all because it passes through a broken link, and — the same refusal, one
    /// step later — when the resolved path has no root-relative identity to mint.
    ///
    /// **There is no fallback to `raw`, ever** (ADR-B). A mint failure means the
    /// resolved path is not a file under the root, which is precisely the case
    /// where substituting the caller's own string would be worst: it is the
    /// attacker-controlled value, and the boundary would then be matched against
    /// something the daemon never opened. The operation is refused instead.
    ///
    /// # The escape refusal names the root (REQ-583 BR-2)
    ///
    /// An outside-the-jail refusal is one shape for every tool: the caller's own
    /// path, the words "is outside the session root", and the root's display —
    /// and nothing else (no listing, no suggested paths). The display is a value
    /// the model already holds from the environment block, so naming it leaks
    /// nothing new, and it is what lets a small model say "I can only see under
    /// `~/x`" instead of retrying blind.
    pub fn resolve(&self, raw: &str) -> Result<Resolved, ToolError> {
        let root = self
            .repo_root
            .canonicalize()
            .map_err(|_| self.root_missing_error())?;

        let joined = if Path::new(raw).is_absolute() {
            PathBuf::from(raw)
        } else {
            root.join(raw)
        };
        let normalized = lexical_normalize(&joined);

        // Containment is decided on a *resolved* path — never on one that still
        // holds untraversed components (REQ-571 BR-6).
        let checked = canonical_through_existing_ancestor(&normalized).ok_or_else(|| {
            ToolError::jail(format!(
                "path `{raw}` cannot be resolved: it passes through a broken symlink"
            ))
        })?;

        if !checked.starts_with(&root) {
            return Err(ToolError::jail(format!(
                "path `{raw}` is outside the session root {}",
                self.display
            )));
        }
        // The `starts_with` above is exactly `strip_prefix`'s precondition, so the
        // only reachable mint failure is a resolved path that names no file under
        // the root — `.`, or the root itself. The message stays content-free and
        // repeats only what the caller already supplied.
        let provenance = ProvenanceId::from_resolved(&root, &checked).map_err(|_| {
            ToolError::jail(format!("path `{raw}` names no file in the session root"))
        })?;
        Ok(Resolved {
            path: checked,
            provenance,
        })
    }
}

/// A path the jail accepted, together with the identity egress will judge it by
/// (REQ-571 ADR-B).
///
/// The two fields are produced by one [`ToolContext::resolve`] call and are two
/// views of one canonical value: `path` is what the tool opens, `provenance` is
/// what a privacy boundary matches. Returning them as a pair is what makes
/// "the gate decides on the parse the executor used" structural — there is no
/// intermediate state in which a tool holds one without the other.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Resolved {
    /// The canonical, in-jail path to open.
    pub path: PathBuf,
    /// The repo-relative identity of that path, for egress provenance.
    pub provenance: ProvenanceId,
}

/// Whether a directory entry must be skipped by a **walking** tool (`grep`,
/// `glob`) — REQ-571 ADR-C, BR-5.
///
/// # Skipping links is the decision, not a gap
///
/// A future contributor will read `grep`/`glob` ignoring a symlink as an
/// oversight and be tempted to follow it. It is not. Two reasons, both
/// load-bearing:
///
/// 1. **One file, two identities.** A followed link surfaces the same bytes
///    under the link's name *and* under the target's, so egress sees two
///    provenance ids for one file identity — precisely what ADR-A exists to
///    prevent. `read`/`edit` can resolve to a single target because they name one
///    file; a walk cannot, because it reports the name it arrived by.
/// 2. **A walk that follows links can cycle** (`a -> b`, `b -> a`, or a
///    directory link pointing at an ancestor).
///
/// It is also ripgrep's default, so it matches what a user expects of a repo
/// search.
///
/// # Why the test is `is_symlink()` and never `!is_dir()`
///
/// [`std::fs::DirEntry::file_type`] reads the *entry*, so it does **not**
/// traverse the link ([`std::fs::metadata`] does). That non-traversal is the root
/// cause this closes: a link reported `is_dir() == false`, fell into the walkers'
/// file branch, and that branch's `read_to_string` *did* follow it — so a link to
/// a file outside the jail was read out and reported under an in-jail relative
/// path. Inferring "not a link" from `!is_dir()` reproduces the bug exactly.
pub(crate) fn skip_symlink_entry(file_type: std::fs::FileType) -> bool {
    file_type.is_symlink()
}

/// Resolve `path` as far as the filesystem actually goes: canonicalize the
/// deepest ancestor that exists, then re-join the components that do not exist
/// yet. `None` means the path has no answer at all — see below.
///
/// # Containment is decided on a resolved path, never a lexical one (REQ-571 BR-6)
///
/// The obvious spelling — `path.canonicalize().unwrap_or(path)` — is a jail
/// escape waiting for a `write`/`create` tool. `canonicalize` fails for a target
/// that does not exist *yet*, and the fallback then hands the containment check a
/// string whose components were never traversed: `link/new`, where `link` points
/// outside the repo, is lexically inside the root and passes, and the **OS**
/// resolves the link on open. Resolving the ancestor closes it, because `link`
/// itself does exist — canonicalizing it yields the outside directory, and the
/// re-joined `link/new` fails `starts_with(root)` like any other escape.
///
/// A not-yet-existing path under a genuine in-root directory still resolves and
/// is still accepted: creating new files in the repo is the case this must not
/// break, and it is what distinguishes "resolve the ancestor" from "require the
/// path to exist".
///
/// # A dangling link is refused, not resolved lexically
///
/// If a component exists ([`Path::symlink_metadata`] succeeds) yet does not
/// canonicalize, it is a broken symlink — or, rarely, a component that just
/// became unreadable. Either way the daemon cannot say where an open would land,
/// and the discarded fallback would have minted an identity under the *link's*
/// own in-jail name for a file that may live anywhere. ADR-B's no-fallback rule
/// applies exactly here, so the walk stops and the caller refuses.
fn canonical_through_existing_ancestor(path: &Path) -> Option<PathBuf> {
    let mut tail: Vec<&std::ffi::OsStr> = Vec::new();
    let mut cursor = path;
    loop {
        if let Ok(canonical) = cursor.canonicalize() {
            let mut resolved = canonical;
            resolved.extend(tail.iter().rev().copied());
            return Some(resolved);
        }
        if cursor.symlink_metadata().is_ok() {
            return None;
        }
        tail.push(cursor.file_name()?);
        cursor = cursor.parent()?;
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
    /// **The capability this call dead-ended on**, as a catalog id
    /// (`web_fetch_any_url`, `web_search`, …), or `None` — which is every call
    /// that did not end that way (REQ-572 ADR-4).
    ///
    /// Carried as data for the same reason [`Self::measured`] is: the only
    /// place with the session's event sink is the turn loop, and the only other
    /// way for it to learn that a refusal was a *capability* dead end would be
    /// to re-read the tool's own sentence — a second classifier over text,
    /// which is precisely what ADR-4 declines to build (LESSON-456). The tool
    /// that refused knows which capability it refused, says so once here, and
    /// [`run_session_turn`](crate::harness::turn_loop::run_session_turn)
    /// publishes `capability_dead_end` from it.
    ///
    /// It is deliberately **not** set by every refusal. A declined permission
    /// prompt, an allowlist refusal or an unreachable host are things a
    /// *configured* capability did; a dead end is the turn running out of
    /// capability altogether, which is the only thing the user can act on by
    /// enabling something.
    pub dead_end: Option<String>,
}

impl ToolOutcome {
    /// A successful outcome with no file provenance and nothing measured.
    pub fn ok(content: impl Into<String>) -> Self {
        Self {
            content: content.into(),
            is_error: false,
            provenance: ToolProvenance::none(),
            measured: None,
            dead_end: None,
        }
    }

    /// A failed outcome the model must see and react to (no file provenance).
    pub fn error(content: impl Into<String>) -> Self {
        Self {
            content: content.into(),
            is_error: true,
            provenance: ToolProvenance::none(),
            measured: None,
            dead_end: None,
        }
    }

    /// Mark this outcome as the point a turn ran out of `capability` — a
    /// catalog id, never a sentence. See [`ToolOutcome::dead_end`].
    ///
    /// The refusal text is the tool's own and is unchanged by this call: what
    /// the model reads and what the daemon announces are two audiences, and
    /// this is only the second one.
    #[must_use]
    pub fn dead_ending(mut self, capability: impl Into<String>) -> Self {
        self.dead_end = Some(capability.into());
        self
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

    /// Tag this outcome with the identities of the files it read/enumerated.
    ///
    /// `paths` are [`ProvenanceId`]s — minted by [`ToolContext::resolve`] for the
    /// file a tool opened, or by [`ProvenanceId::from_resolved`] for each entry a
    /// walker surfaced. A raw request string is not accepted and cannot be made
    /// into one implicitly (REQ-571 ADR-A), which is what stops the next tool
    /// added here from re-opening the hole `read`/`edit` had.
    #[must_use]
    pub fn with_paths<I>(self, paths: I) -> Self
    where
        I: IntoIterator<Item = ProvenanceId>,
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
    /// The path escaped the session-root jail. The prefix is the error's
    /// *category*; the payload is the BR-2 shape (path, "is outside the session
    /// root", display) for an escape, and names no other path for the rest.
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

    /// Whether this tool raises its **own** permission prompt inside
    /// [`Tool::run`], so the loop must not raise one on its behalf.
    ///
    /// **The default is `false`, and that is the safe answer**: a tool that says
    /// nothing is authorized by the loop, by name, before it runs — which is
    /// what every built-in and every MCP tool wants, because for them the tool
    /// name *is* the whole consent question.
    ///
    /// It is asked of the tool rather than decided from a list of names for the
    /// same reason [`Tool::refine`] is (BR-1): the thing that knows how a call
    /// is authorized is the thing that performs it. Today exactly one tool
    /// answers `true` — [`web`](web::WebTool) — and only because its consent
    /// question is **finer than its name and is not always asked**:
    ///
    /// - fetching a URL and searching the web are separately consented
    ///   capabilities (REQ-563 BR-3), so one session grant on a fetch must not
    ///   silently grant a search;
    /// - a lookup above the configured ceiling is refused *before* any prompt
    ///   (AC-4), and a lookup already in the local cache performs no egress and
    ///   must not prompt at all (BR-12).
    ///
    /// Both of those are facts only the tool holds, and both are decided inside
    /// the same `run` that would perform the lookup — which is what keeps the
    /// "no egress without consent" pairing atomic instead of split across two
    /// calls that could come to disagree.
    fn gates_itself(&self) -> bool {
        false
    }

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

/// One tool in a [`ToolRegistry`], carrying whether the degraded-profile tool
/// cap (BR-6) may displace it.
struct Registration {
    /// The tool itself.
    tool: Arc<dyn Tool>,
    /// When `true`, the tool is exempt from the `max_tools` cut: it is always
    /// exposed, and the cap bounds only the remaining (non-exempt) tools — see
    /// [`ToolRegistry::register_cap_exempt`].
    cap_exempt: bool,
}

/// The set of tools available to a session.
///
/// Insertion order is the exposure order: [`ToolRegistry::docs`] can be capped to
/// the first `max_tools` non-exempt tools for a degraded (weak) provider (BR-6),
/// so put the most load-bearing tools first.
pub struct ToolRegistry {
    tools: Vec<Registration>,
}

impl ToolRegistry {
    /// An empty registry.
    #[must_use]
    pub fn new() -> Self {
        Self { tools: Vec::new() }
    }

    /// A registry with the full built-in tool set, in weak-model priority order:
    /// read, edit, grep, glob, shell — plus `teton_docs`, registered
    /// **cap-exempt** (REQ-577 ADR-3).
    ///
    /// `teton_docs` is here, in the constructor, precisely because it is
    /// *unconditional*: making it a step every caller performs would make
    /// "present in every session" a claim about call sites, and BR-7's one
    /// unacceptable outcome is that it is silently absent from one. Registered
    /// here it is inherited by every fixture, the offline path and the templated
    /// smoke without any of them being edited. Its exemption is explained on
    /// [`Self::register_cap_exempt`], where the exempt set is enumerated.
    ///
    /// The **opt-in** `web` tool is deliberately not here (REQ-563 D-1): it
    /// exists only on a machine whose `[web] tier` is above `off`, which is a
    /// config fact this constructor does not have and should not acquire — a
    /// registry that always held the tool and hid it behind a flag would make
    /// "absent by construction" a claim about a code path rather than about the
    /// tool set. Its caller adds it with [`register_web_tool`](web::register_web_tool),
    /// **cap-exempt** and **last** — after the built-ins *and* after any MCP
    /// tools — so an explicitly opted-in capability is never displaced by a
    /// degraded provider's `max_tools` cap (REQ-563 decision 2026-08-09) while
    /// the cap still bounds the optional MCP tools around it (BR-6).
    ///
    /// The two are therefore opposite by design and for the same reason: what
    /// decides where a tool registers is whether its presence is a *config*
    /// question. `web`'s is, so it registers where the config is known;
    /// `teton_docs`'s is not, so it registers where no config can reach it.
    #[must_use]
    pub fn with_builtins() -> Self {
        let mut reg = Self::new();
        reg.register(Arc::new(ReadTool));
        reg.register(Arc::new(EditTool));
        reg.register(Arc::new(GrepTool));
        reg.register(Arc::new(GlobTool));
        reg.register(Arc::new(ShellTool::default()));
        reg.register_cap_exempt(Arc::new(DocsTool));
        reg
    }

    /// Add a tool (later registrations with the same name shadow earlier ones on
    /// lookup order but are kept for exposure ordering — register uniquely).
    ///
    /// The tool is subject to the degraded-profile `max_tools` cap (BR-6); a tool
    /// the cap must never displace is registered with
    /// [`Self::register_cap_exempt`] instead.
    pub fn register(&mut self, tool: Arc<dyn Tool>) {
        self.tools.push(Registration {
            tool,
            cap_exempt: false,
        });
    }

    /// Add a tool the degraded-profile `max_tools` cap (BR-6) may never displace.
    ///
    /// A cap-exempt tool is always exposed by [`Self::exposed_names`]/[`Self::docs`]
    /// regardless of the cut, and the cap applies to the remaining (non-exempt)
    /// tools — so exempting one tool raises the effective exposure budget by
    /// exactly one and never displaces a built-in.
    ///
    /// # The exempt set, and why each member is in it
    ///
    /// Two tools register this way, and their reasons are **different**. That is
    /// the membership rule: an exemption is granted for a stated reason, and a
    /// reason that merely repeats an existing member's is a sign the cap is being
    /// argued with rather than an exemption being earned. Enumerated here so the
    /// set stays something a reader can check instead of a place tools accumulate.
    ///
    /// - **`web`** (REQ-563 decision 2026-08-09) — *the user's opt-in must
    ///   survive the cap.* The capability exists only because someone turned it
    ///   on, and a setting that silently stops applying on some providers is a
    ///   setting that lies. It also registers *late*, after any MCP tools, so
    ///   without exemption it would be first against the wall.
    /// - **`teton_docs`** (REQ-577 BR-7, ADR-3) — *self-serving product
    ///   knowledge, needed most exactly where the cap bites.* The cap exists
    ///   because weak tool-callers drown in tool schemas; this tool is one string
    ///   argument and one line of description, so it barely adds to that load —
    ///   while the profiles the cap applies to are the local and degraded ones,
    ///   which are precisely the sessions whose model does not know Teton's own
    ///   setup surface and would otherwise search the user's repository for it
    ///   (BUG-160).
    ///
    /// Neither is a raised cap, deliberately. `DEGRADED_MAX_TOOLS` equals the
    /// built-in count, so with a count-based fix availability is an arithmetic
    /// coincidence that the next built-in silently breaks — the trap LESSON-496
    /// documents, where "cut first under pressure" became "never available"
    /// because the limit equalled the count. An enumerated exempt set is a rule;
    /// a number that happens to be big enough is not.
    pub fn register_cap_exempt(&mut self, tool: Arc<dyn Tool>) {
        self.tools.push(Registration {
            tool,
            cap_exempt: true,
        });
    }

    /// Look up a tool by name.
    #[must_use]
    pub fn get(&self, name: &str) -> Option<&Arc<dyn Tool>> {
        self.tools
            .iter()
            .map(|r| &r.tool)
            .find(|t| t.name() == name)
    }

    /// Every registered tool name, in exposure order.
    #[must_use]
    pub fn names(&self) -> Vec<&str> {
        self.tools.iter().map(|r| r.tool.name()).collect()
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
        let mut out = String::new();
        for tool in self.exposed_tools(max_tools) {
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
        self.exposed_tools(max_tools).map(|t| t.name()).collect()
    }

    /// The tools exposed under a `max_tools` cap (BR-6), in registration order.
    ///
    /// A cap-exempt tool ([`Self::register_cap_exempt`]) is always present; the
    /// cap bounds only the non-exempt tools, so the first `max_tools` of *those*
    /// are kept and a cap-exempt tool is never one displaced. `None` means no
    /// cap — every tool is exposed.
    fn exposed_tools(&self, max_tools: Option<u32>) -> impl Iterator<Item = &Arc<dyn Tool>> {
        let non_exempt = self.tools.iter().filter(|r| !r.cap_exempt).count();
        let limit = max_tools
            .map(|n| n as usize)
            .unwrap_or(non_exempt)
            .min(non_exempt);
        let mut taken = 0usize;
        self.tools.iter().filter_map(move |r| {
            if r.cap_exempt {
                Some(&r.tool)
            } else if taken < limit {
                taken += 1;
                Some(&r.tool)
            } else {
                None
            }
        })
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

    /// The counter, not the timestamp, guarantees uniqueness: `SystemTime::now()`
    /// can return the same value for two calls within one clock tick.
    fn temp_root(tag: &str) -> PathBuf {
        use std::sync::atomic::{AtomicU64, Ordering};
        static NEXT: AtomicU64 = AtomicU64::new(0);
        let dir = std::env::temp_dir().join(format!(
            "teton-tooljail-{tag}-{}-{}-{}",
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
    fn resolve_accepts_paths_inside_the_jail() {
        let root = temp_root("in");
        std::fs::write(root.join("a.txt"), "hi").unwrap();
        let ctx = ToolContext::new(&root);
        let resolved = ctx.resolve("a.txt").unwrap();
        assert!(resolved.path.starts_with(root.canonicalize().unwrap()));
        std::fs::remove_dir_all(&root).ok();
    }

    /// **REQ-571 ADR-B: one call, one identity, no drift.**
    ///
    /// Every spelling of one file resolves to the same canonical path *and* the
    /// same [`ProvenanceId`] — the value a boundary glob is matched against. The
    /// tool-layer half of BR-3: `teton-core` proves the arithmetic, this proves
    /// that the daemon's canonicalization feeds it, which is what the
    /// `..`-traversing spelling needs.
    #[test]
    fn every_spelling_of_one_file_resolves_to_one_identity() {
        let root = temp_root("spell");
        std::fs::create_dir_all(root.join("secrets")).unwrap();
        std::fs::write(root.join("secrets/prod.env"), "API_KEY=1\n").unwrap();
        let canonical_root = root.canonicalize().unwrap();
        let ctx = ToolContext::new(&root);

        let absolute = canonical_root.join("secrets/prod.env");
        let absolute = absolute.to_string_lossy().into_owned();
        for spelling in [
            "secrets/prod.env",
            "./secrets/prod.env",
            ".//secrets/prod.env",
            "././secrets/prod.env",
            &absolute,
            "src/../secrets/prod.env",
        ] {
            let resolved = ctx
                .resolve(spelling)
                .unwrap_or_else(|e| panic!("spelling {spelling:?} must resolve: {e}"));
            assert_eq!(
                resolved.provenance.as_str(),
                "secrets/prod.env",
                "spelling {spelling:?} minted a divergent identity"
            );
            assert_eq!(resolved.path, canonical_root.join("secrets/prod.env"));
        }
        std::fs::remove_dir_all(&root).ok();
    }

    /// **ADR-B, the no-fallback rule.** A path that resolves to no file under the
    /// root has no identity, and the answer is a refusal — never the caller's own
    /// string standing in for one.
    #[test]
    fn a_path_with_no_repo_relative_identity_is_refused() {
        let root = temp_root("noid");
        let ctx = ToolContext::new(&root);
        // The root itself: inside the jail, but it names no file to attribute.
        let err = ctx.resolve(".").unwrap_err();
        assert!(matches!(err, ToolError::Jail(_)), "{err:?}");
        assert!(err.to_string().contains("names no file"), "{err}");
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

    /// **REQ-583 AC-5, the one shape.** The escape refusal every tool gives
    /// names the caller's own path, says "is outside the session root", and
    /// names the root's display — and nothing else. One helper judges both
    /// tools' errors, so the two cannot drift into two shapes.
    fn assert_outside_root_shape(content: &str, raw: &str, display: &str) {
        assert!(
            content.contains(&format!("`{raw}`")),
            "the refusal must name the caller's path {raw:?}: {content}"
        );
        assert!(
            content.contains("is outside the session root"),
            "the refusal must say the path is outside the session root: {content}"
        );
        assert!(
            content.contains(display),
            "the refusal must name the root display {display:?}: {content}"
        );
        // Nothing else: no listing, no suggested alternative.
        assert!(
            !content.contains("try ") && !content.contains("did you mean"),
            "the refusal must not suggest other paths: {content}"
        );
        assert_eq!(
            content.lines().count(),
            1,
            "the refusal is one line, not a listing: {content}"
        );
    }

    #[test]
    fn read_and_edit_outside_the_jail_share_one_refusal_shape_naming_the_root() {
        use serde_json::json;
        let root = temp_root("ac5");
        std::fs::write(root.join("in.txt"), "inside\n").unwrap();
        let ctx = ToolContext::new(&root);
        let display = ctx.root_display().to_owned();
        assert!(!display.is_empty());

        let read = ReadTool.run(&ctx, &json!({ "path": "../outside.txt" }));
        assert!(read.is_error, "{}", read.content);
        assert_outside_root_shape(&read.content, "../outside.txt", &display);

        let edit = EditTool.run(
            &ctx,
            &json!({ "path": "/etc/hosts", "old_string": "a", "new_string": "b" }),
        );
        assert!(edit.is_error, "{}", edit.content);
        assert_outside_root_shape(&edit.content, "/etc/hosts", &display);

        // And the two are literally one template: strip the path and the two
        // messages are the same bytes.
        let shape = |content: &str, raw: &str| content.replace(&format!("`{raw}`"), "`<path>`");
        assert_eq!(
            shape(&read.content, "../outside.txt"),
            shape(&edit.content, "/etc/hosts")
        );
        std::fs::remove_dir_all(&root).ok();
    }

    /// The refusal prints the *probed* display when the context was built from a
    /// [`SessionRoot`] — the same bytes the environment block carries — and the
    /// plain constructor spells the path home-relative by the same rule.
    #[test]
    fn for_root_carries_the_probed_display_and_kind_and_new_is_plain() {
        let root = temp_root("forroot");
        let probed = SessionRoot {
            display: "~/probed/display".to_owned(),
            kind: RootKind::Home,
            project_name: None,
            vcs_branch: None,
        };
        let ctx = ToolContext::for_root(root.clone(), &probed);
        assert_eq!(ctx.root_kind(), RootKind::Home);
        assert_eq!(ctx.root_display(), "~/probed/display");
        assert_eq!(ctx.repo_root(), root.as_path());
        let err = ctx.resolve("/etc/hosts").unwrap_err();
        assert!(
            err.to_string()
                .contains("path `/etc/hosts` is outside the session root ~/probed/display"),
            "{err}"
        );

        let plain = ToolContext::new(&root);
        assert_eq!(plain.root_kind(), RootKind::Plain);
        let home = crate::session_root::home();
        assert_eq!(
            plain.root_display(),
            bounded_field(&display_for(&root, home.as_deref()), DISPLAY_MAX_CHARS)
        );

        let kinded = ToolContext::new(&root).with_root_kind(RootKind::FilesystemRoot);
        assert_eq!(kinded.root_kind(), RootKind::FilesystemRoot);
        assert_eq!(kinded.root_display(), plain.root_display());
        std::fs::remove_dir_all(&root).ok();
    }

    /// **AC-6, the refusal half.** Every jail arm — the escape (asserted above),
    /// the broken link, the no-identity path and the missing root — says
    /// "session root" and never "repo root" or "repository" (BR-3: one term for
    /// the jail; the word "repository" appears only where the kind is a
    /// project). Enumerated rather than sampled, so a new arm has to be added
    /// here to ship.
    #[test]
    fn no_jail_refusal_says_repo_root() {
        let root = temp_root("noterm");
        let outside = temp_root("noterm-out");
        std::os::unix::fs::symlink(outside.join("never-created.txt"), root.join("dangling"))
            .unwrap();
        let ctx = ToolContext::new(&root);
        let names_no_file = ctx.resolve(".").unwrap_err().to_string();
        let broken_link = ctx.resolve("dangling").unwrap_err().to_string();
        assert!(broken_link.contains("broken symlink"), "{broken_link}");
        let escape = ctx.resolve("/etc/hosts").unwrap_err().to_string();

        std::fs::remove_dir_all(&root).ok();
        std::fs::remove_dir_all(&outside).ok();
        let gone = ctx.resolve("a.txt").unwrap_err().to_string();
        assert!(gone.contains(ctx.root_display()), "{gone}");
        assert_eq!(gone, ctx.root_missing_error().to_string());

        // The three arms that name the jail name it as the session root; the
        // broken-link arm names only the path (it says nothing about the jail
        // at all). None of the four says "repo root" or "repository".
        for text in [&names_no_file, &escape, &gone] {
            assert!(text.contains("session root"), "{text}");
        }
        for (arm, text) in [
            ("names no file", &names_no_file),
            ("broken link", &broken_link),
            ("escape", &escape),
            ("root missing", &gone),
        ] {
            for forbidden in ["repo root", "repository", "Repository"] {
                assert!(
                    !text.contains(forbidden),
                    "{arm} says {forbidden:?}: {text}"
                );
            }
        }
    }

    /// **REQ-571 BR-6, the case the ancestor walk exists for.** The leaf does not
    /// exist, so canonicalizing the whole path fails — but `link` does exist and
    /// resolves outside the repo, so the re-joined path is outside and refused.
    /// The discarded lexical fallback accepted this.
    #[test]
    fn resolve_refuses_a_new_leaf_reached_through_an_escaping_link() {
        let root = temp_root("newleaf-esc");
        let outside = temp_root("newleaf-out");
        std::os::unix::fs::symlink(&outside, root.join("link")).unwrap();
        // Fixture control: the link is live and really does leave the root.
        assert_eq!(
            root.join("link").canonicalize().unwrap(),
            outside.canonicalize().unwrap()
        );
        let ctx = ToolContext::new(&root);

        let err = ctx.resolve("link/new.txt").unwrap_err();
        assert!(matches!(err, ToolError::Jail(_)), "{err:?}");
        assert!(
            err.to_string().contains("is outside the session root"),
            "{err}"
        );
        std::fs::remove_dir_all(&root).ok();
        std::fs::remove_dir_all(&outside).ok();
    }

    /// **The over-refusal guard.** Resolving the ancestor is not "the path must
    /// exist": a file that has not been created yet, under a real in-root
    /// directory, still resolves and still mints its own identity — including
    /// through an in-root link, which is answered by its target like any other
    /// spelling (ADR-C).
    #[test]
    fn resolve_still_accepts_a_not_yet_existing_path_under_the_root() {
        let root = temp_root("newleaf-ok");
        std::fs::create_dir_all(root.join("src")).unwrap();
        std::os::unix::fs::symlink("src", root.join("link")).unwrap();
        let canonical_root = root.canonicalize().unwrap();
        let ctx = ToolContext::new(&root);

        let resolved = ctx.resolve("src/new.rs").unwrap();
        assert_eq!(resolved.provenance.as_str(), "src/new.rs");
        assert_eq!(resolved.path, canonical_root.join("src/new.rs"));

        // A missing intermediate directory is a new path too, not an escape.
        let resolved = ctx.resolve("src/deep/new.rs").unwrap();
        assert_eq!(resolved.provenance.as_str(), "src/deep/new.rs");

        // Through an in-root link: the target names the file, so the id is the
        // target's — the same rule an existing file gets.
        let resolved = ctx.resolve("link/new.rs").unwrap();
        assert_eq!(resolved.provenance.as_str(), "src/new.rs");
        assert_eq!(resolved.path, canonical_root.join("src/new.rs"));
        std::fs::remove_dir_all(&root).ok();
    }

    /// **The dangling link (TASK-120 flagged it, this closes it).** The link
    /// exists but resolves nowhere, so the daemon cannot say where an open would
    /// land — and the discarded fallback would have minted an id under the link's
    /// own in-jail name for a file that lives outside. Refused instead (ADR-B).
    #[test]
    fn resolve_refuses_a_dangling_link_rather_than_minting_its_own_name() {
        let root = temp_root("dangling");
        let outside = temp_root("dangling-out");
        std::os::unix::fs::symlink(outside.join("never-created.txt"), root.join("notes.txt"))
            .unwrap();
        // Fixture control: the entry exists, and it is the *resolution* that fails.
        assert!(root.join("notes.txt").symlink_metadata().is_ok());
        assert!(root.join("notes.txt").canonicalize().is_err());
        let ctx = ToolContext::new(&root);

        let err = ctx.resolve("notes.txt").unwrap_err();
        assert!(matches!(err, ToolError::Jail(_)), "{err:?}");
        assert!(err.to_string().contains("broken symlink"), "{err}");
        std::fs::remove_dir_all(&root).ok();
        std::fs::remove_dir_all(&outside).ok();
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

    /// The cap the daemon actually applies to a weak tool-caller, read from the
    /// production mapping rather than re-typed.
    ///
    /// `DEGRADED_MAX_TOOLS` is private to `teton-providers`, and copying its
    /// value here would make this file's assertions agree with a number instead
    /// of with the profile. Derived through
    /// [`CapabilityProfile::harness_profile`] it is the same value the runtime
    /// hands `docs`/`exposed_names`, so a change to the cap is felt here rather
    /// than quietly diverged from.
    fn degraded_cap() -> Option<u32> {
        use teton_core::ToolCallTier;
        use teton_providers::capability::CapabilityProfile;

        CapabilityProfile {
            tool_call_tier: ToolCallTier::Degraded,
            ..CapabilityProfile::default()
        }
        .harness_profile()
        .max_tools
    }

    /// **BR-6's cut, and REQ-577 BR-7's exemption from it, on the real
    /// built-in registry.**
    ///
    /// The cut is asserted on the *non-exempt* five: `Some(2)` keeps `read` and
    /// `edit` and drops the rest of them, while `teton_docs` rides through
    /// untouched at every setting — which is the mechanism, not the ordering.
    /// `teton_docs` is registered **last**, so a registry that had merely got
    /// lucky with position would be the first thing any of these caps discarded.
    #[test]
    fn docs_are_capped_by_max_tools_for_degraded_providers() {
        let reg = ToolRegistry::with_builtins();
        assert_eq!(
            reg.exposed_names(None),
            vec!["read", "edit", "grep", "glob", "shell", "teton_docs"]
        );
        assert_eq!(
            reg.exposed_names(Some(2)),
            vec!["read", "edit", "teton_docs"],
            "the cap bounds the five non-exempt built-ins and never the exempt tool"
        );
        // Matched on the rendered entry (`- <name>: …`) rather than on the bare
        // name: `teton_docs`'s own description carries the topic index, which
        // spells `web` and `doctor`, so a substring search over the whole block
        // would answer for tools that are not in it.
        assert!(reg.docs(Some(1)).contains("- read:"));
        assert!(!reg.docs(Some(1)).contains("- shell:"));

        // AC-5: on the profile the daemon derives for a weak tool-caller — the
        // one whose cap *equals* the built-in count, and therefore the one a
        // non-exempt sixth tool would never survive (LESSON-496) — the five
        // built-ins and `teton_docs` are all exposed.
        let cap = degraded_cap();
        assert!(
            cap.is_some(),
            "the degraded profile no longer caps tools at all, so this test asserts \
             nothing about the cut `teton_docs` is exempt from"
        );
        let exposed = reg.exposed_names(cap);
        for builtin in ["read", "edit", "grep", "glob", "shell"] {
            assert!(exposed.contains(&builtin), "{exposed:?}");
        }
        assert!(
            exposed.contains(&DOCS_TOOL_NAME),
            "`{DOCS_TOOL_NAME}` is absent under the degraded cap {cap:?}, which is BR-7's \
             one unacceptable outcome: the profiles that cap tools are exactly the ones \
             whose model does not know Teton's own setup surface. Restore the \
             `register_cap_exempt` registration in `with_builtins`; do not raise the \
             cap.\nexposed: {exposed:?}"
        );
        assert!(
            reg.docs(cap).contains(&format!("- {DOCS_TOOL_NAME}:")),
            "the tool docs the model reads must mirror `exposed_names`"
        );
    }

    /// A stand-in for any tool, to exercise the registry's exposure rules
    /// without one of the real built-ins.
    struct StubTool(&'static str);

    #[async_trait]
    impl Tool for StubTool {
        fn name(&self) -> &str {
            self.0
        }
        fn description(&self) -> &str {
            "stub"
        }
        fn input_schema(&self) -> Value {
            serde_json::json!({})
        }
        fn run(&self, _ctx: &ToolContext, _args: &Value) -> ToolOutcome {
            ToolOutcome::ok("stub")
        }
    }

    /// **REQ-563 decision 2026-08-09, extended by REQ-577 ADR-3: every
    /// cap-exempt tool survives the cut that bounds the rest.**
    ///
    /// `DEGRADED_MAX_TOOLS` equals the built-in count (5), so a plain cap of 5
    /// would leave no room for an opted-in `web` tool registered after the
    /// built-ins and MCP. Cap-exemption raises the effective budget by exactly
    /// the number of exempt tools: the five built-ins survive, the optional
    /// (non-exempt) MCP tool is still cut, and **both** exempt tools are exposed
    /// regardless of the cap.
    ///
    /// The fixture now starts from a registry that *already* carries one exempt
    /// tool — `with_builtins` registers `teton_docs` that way — which is the
    /// case worth pinning: the arithmetic must hold for a set, not for the one
    /// exemption that happened to exist when it was written.
    #[test]
    fn a_cap_exempt_tool_is_never_displaced_by_the_max_tools_cut() {
        // The registration order the daemon uses: built-ins (including the
        // cap-exempt `teton_docs`), then MCP, then the cap-exempt web tool.
        let mut reg = ToolRegistry::with_builtins();
        reg.register(Arc::new(StubTool("mcp")));
        reg.register_cap_exempt(Arc::new(StubTool("web")));

        let exposed = reg.exposed_names(Some(5));
        for builtin in ["read", "edit", "grep", "glob", "shell"] {
            assert!(
                exposed.contains(&builtin),
                "the cap displaced a built-in: {exposed:?}"
            );
        }
        assert!(
            !exposed.contains(&"mcp"),
            "the cap must still bound optional (non-exempt) tools: {exposed:?}"
        );
        assert!(
            exposed.contains(&"web"),
            "an explicitly opted-in cap-exempt tool was cut: {exposed:?}"
        );
        assert!(
            exposed.contains(&DOCS_TOOL_NAME),
            "the built-in cap-exempt tool was cut: {exposed:?}"
        );
        assert_eq!(
            exposed.len(),
            7,
            "exemption raises the effective budget by exactly the number of exempt \
             tools — five capped built-ins plus two exempt: {exposed:?}"
        );

        // docs mirror exposed_names. Matched on the rendered entry, because
        // `teton_docs`'s description names `web` in its topic index and would
        // otherwise answer for the web tool's own line.
        let docs = reg.docs(Some(5));
        assert!(docs.contains("- web:"), "{docs}");
        assert!(docs.contains(&format!("- {DOCS_TOOL_NAME}:")), "{docs}");
        assert!(!docs.contains("- mcp:"), "{docs}");
    }

    /// **REQ-567 AC-6, the structural half: the model cannot clear a session.**
    ///
    /// BR-8 says a clear is user-only, and AC-6 says that must hold *by
    /// construction* rather than by a check someone has to remember. The
    /// registry is the whole surface a model's tool call can reach, so the claim
    /// has two parts and both are here.
    ///
    /// **No tool is named for it.** A tool called `clear` — or any spelling of
    /// it — would make "the model may not clear the conversation" a runtime
    /// branch; its absence is what makes the rule structural. Asserted with the
    /// full built-in set present, so the claim is about the registry a real
    /// session runs with and not about an empty one. (`web` is opt-in and absent
    /// here by construction; REQ-563's own AC-12 test asserts its namespace with
    /// the tool registered.)
    ///
    /// **And no MCP tool can reach it either.** MCP is the one path that adds
    /// tools this crate did not write, so "no built-in is named clear" would be
    /// a partial answer: a server is free to publish a tool called `clear`.
    /// What that tool would do is call *its own server* —
    /// [`mcp::McpToolHandle::run`] forwards to `McpRegistry::call_tool` and has
    /// no other effect on this process — and `session/clear` is a method on
    /// `DaemonRuntime`, which no [`Tool`] is ever handed. The scan below is the
    /// mechanical form of that argument: no production source under `harness/`
    /// — where every `impl Tool` lives — so much as names the runtime's clear or
    /// its params type. Wire one in and this fails, which is the point: the next
    /// person to try it has to answer for it here rather than ship a tool that
    /// quietly drops a user's conversation on the model's say-so (LESSON-495).
    #[test]
    fn no_tool_can_clear_a_session_and_no_mcp_wiring_path_could() {
        let reg = ToolRegistry::with_builtins();
        // Non-vacuity: the registry under test is the populated one.
        assert!(
            reg.get("read").is_some() && reg.names().len() == 6,
            "the built-in set is missing, so the sweep below proves nothing: {:?}",
            reg.names()
        );

        for forbidden in [
            "clear",
            "session/clear",
            "session_clear",
            "clear_session",
            "clear_context",
            "context_clear",
            "reset",
        ] {
            assert!(
                reg.get(forbidden).is_none(),
                "`{forbidden}` is reachable from tool dispatch, so a model could issue it"
            );
        }
        // Belt and braces over the whole namespace: no registered tool mentions
        // the verb, whatever it is spelled.
        for name in reg.names() {
            assert!(
                !name.contains("clear"),
                "`{name}` is a tool the model can call, and it names a user-only action"
            );
        }

        // The MCP half, and every other tool this crate might grow: nothing a
        // tool call reaches names the runtime's clear. Whole-line comments are
        // stripped, because this REQ documented in prose is not a second
        // implementation of it; test modules are already stripped by the scan.
        let harness: Vec<_> = crate::call_sites::scan::production_sources()
            .into_iter()
            .filter(|(path, _)| path.starts_with("harness/"))
            .collect();
        assert!(
            harness
                .iter()
                .any(|(path, _)| path.contains("tools/mcp.rs")),
            "the scan missed the MCP wiring it is about: {:?}",
            harness.iter().map(|(p, _)| p).collect::<Vec<_>>()
        );
        for (path, source) in &harness {
            let code = crate::call_sites::scan::code_only(source);
            for named in ["clear_session", "SessionClearParams"] {
                assert!(
                    !code.contains(named),
                    "{path} names `{named}`, so tool dispatch has a path to clearing a \
                     session's conversation — AC-6's by-construction claim is false"
                );
            }
        }
    }

    /// **REQ-583 ADR-4, the structural half: the model cannot move its own
    /// jail.** `session/set_cwd` is a **client** RPC and, like `session/clear`,
    /// it lives outside `harness/` on purpose: a model that could move the
    /// session root would move every boundary anchored at it (OQ-1) and re-jail
    /// its own tools on the user's say-so never given.
    ///
    /// Two halves, for [`no_tool_can_clear_a_session_and_no_mcp_wiring_path_could`]'s
    /// reasons. No tool is named for it, spelled however a model might spell it
    /// — asserted against the full built-in set. And no production source under
    /// `harness/` — where every `impl Tool` lives, MCP wiring included — names
    /// the runtime's mover, its params type or its wire method. The scan is
    /// over identifiers: the walkers' user-facing "move the session root with
    /// /cd" advice is a sentence, not a call path, and does not trip it.
    #[test]
    fn no_tool_can_move_a_sessions_root_and_no_harness_source_names_it() {
        let reg = ToolRegistry::with_builtins();
        assert!(
            reg.get("read").is_some() && reg.names().len() == 6,
            "the built-in set is missing, so the sweep below proves nothing: {:?}",
            reg.names()
        );

        let ctx = ToolContext::new(std::env::temp_dir());
        for forbidden in [
            "cd",
            "set_cwd",
            "chdir",
            "session/set_cwd",
            "session_set_cwd",
            "set_session_cwd",
            "set_root",
            "move_root",
        ] {
            assert!(
                reg.get(forbidden).is_none(),
                "`{forbidden}` is reachable from tool dispatch, so a model could issue it"
            );
            let outcome = reg.dispatch(forbidden, &ctx, &serde_json::json!({}));
            assert!(
                outcome.is_error && outcome.content.contains("unknown tool"),
                "a model that emits `{forbidden}` must be told there is no such tool, \
                 not served: {}",
                outcome.content
            );
        }
        for name in reg.names() {
            assert!(
                !name.contains("cwd") && !name.contains("chdir") && name != "cd",
                "`{name}` is a tool the model can call, and it names a user-only action"
            );
        }

        let harness: Vec<_> = crate::call_sites::scan::production_sources()
            .into_iter()
            .filter(|(path, _)| path.starts_with("harness/"))
            .collect();
        assert!(
            harness
                .iter()
                .any(|(path, _)| path.contains("tools/mcp.rs"))
                && harness
                    .iter()
                    .any(|(path, _)| path.contains("tools/walk.rs")),
            "the scan missed the sources it is about: {:?}",
            harness.iter().map(|(p, _)| p).collect::<Vec<_>>()
        );
        for (path, source) in &harness {
            let code = crate::call_sites::scan::code_only(source);
            for named in ["set_session_cwd", "SessionSetCwdParams", "session/set_cwd"] {
                assert!(
                    !code.contains(named),
                    "{path} names `{named}`, so tool dispatch has a path to moving a \
                     session's root — ADR-4's by-construction claim is false"
                );
            }
        }
    }

    /// **REQ-579 BR-12, the structural half: the model cannot register a
    /// provider.**
    ///
    /// `provider/setup_*` are **client** RPCs, and that placement is the
    /// enforcement rather than a convention: tool dispatch and the client socket
    /// are structurally distinct channels, so a model emitting a tool call named
    /// `provider/setup_commit` reaches this registry, finds no such tool, and is
    /// told so. That is the same argument `web/setup_commit` rests on
    /// (`teton-protocol`'s guided-setup module header states it), asserted here
    /// for this flow rather than assumed inherited from that one (LESSON-525).
    ///
    /// **Absence is the mechanism, so nothing is added to make this pass.** A
    /// name-matching refusal inside dispatch would turn "the model may not
    /// register a provider" into a runtime branch — one more thing to delete —
    /// where today it is a fact about which channel the handler hangs off. The
    /// registry has no allowlist and must not grow one for this.
    ///
    /// Two halves, for [`no_tool_can_clear_a_session_and_no_mcp_wiring_path_could`]'s
    /// reasons. No tool is named for it, spelled however a model might spell it —
    /// asserted against the full built-in set, so the claim is about the registry
    /// a real session runs with. And no source under `harness/` — where every
    /// `impl Tool` lives, MCP wiring included — so much as names the runtime's
    /// commit or its params type. Wire one in and this fails, which is the point:
    /// the next person to try has to answer for it here rather than ship a tool
    /// that registers an egress endpoint on the model's say-so.
    #[test]
    fn no_tool_can_commit_a_provider_setup_and_no_harness_source_names_it() {
        let reg = ToolRegistry::with_builtins();
        // Non-vacuity: the registry under test is the populated one.
        assert!(
            reg.get("read").is_some() && reg.names().len() == 6,
            "the built-in set is missing, so the sweep below proves nothing: {:?}",
            reg.names()
        );

        let ctx = ToolContext::new(std::env::temp_dir());
        for forbidden in [
            "provider/setup_commit",
            "provider/setup_preview",
            "provider/setup_plan",
            "provider_setup_commit",
            "provider_setup",
            "register_provider",
            "add_provider",
        ] {
            assert!(
                reg.get(forbidden).is_none(),
                "`{forbidden}` is reachable from tool dispatch, so a model could issue it"
            );
            let outcome = reg.dispatch(forbidden, &ctx, &serde_json::json!({}));
            assert!(
                outcome.is_error && outcome.content.contains("unknown tool"),
                "a model that emits `{forbidden}` must be told there is no such tool, \
                 not served: {}",
                outcome.content
            );
        }
        // Belt and braces over the whole namespace: no registered tool is named
        // for provider registration, whatever it is spelled.
        for name in reg.names() {
            assert!(
                !name.contains("provider"),
                "`{name}` is a tool the model can call, and it names a user-only action"
            );
        }

        // The MCP half, and every other tool this crate might grow: nothing a
        // tool call reaches names the runtime's commit or its params type.
        // Whole-line comments are stripped, so a doc comment that *discusses*
        // the flow is not a second implementation of it.
        let harness: Vec<_> = crate::call_sites::scan::production_sources()
            .into_iter()
            .filter(|(path, _)| path.starts_with("harness/"))
            .collect();
        assert!(
            harness
                .iter()
                .any(|(path, _)| path.contains("tools/mcp.rs")),
            "the scan missed the MCP wiring it is about: {:?}",
            harness.iter().map(|(p, _)| p).collect::<Vec<_>>()
        );
        for (path, source) in &harness {
            let code = crate::call_sites::scan::code_only(source);
            for named in ["provider_setup_commit", "ProviderSetupCommitParams"] {
                assert!(
                    !code.contains(named),
                    "{path} names `{named}`, so tool dispatch has a path to registering \
                     a provider — BR-12's by-construction claim is false"
                );
            }
        }
    }
}
