//! One walk policy for every walker (REQ-583 ADR-3, BR-10..BR-13).
//!
//! `glob` and `grep` used to carry the walk twice: each had its own skip list,
//! its own recursion, and no bound at all — from a home-folder session root a
//! `**/*.rs` crawled `~/Library`, `~/Pictures` and the rest, and on macOS that is
//! precisely the set of trees the OS gates behind a consent dialog (the incident
//! REQ-583 answers). This module is the single owner of everything a walk
//! decides that is not the tool's own business:
//!
//! - **the skip set** ([`WALK_SKIP_DIRS`]) — VCS and build-output names never
//!   descended into, at any depth;
//! - **the home top-level set** ([`HOME_TOP_LEVEL_SKIPS`]) — the media and cache
//!   trees pruned only when they sit *directly under a user's home directory*
//!   (BR-12: `~/Library` is pruned, `~/Documents/GitHub/app/Library` is project
//!   content and walked);
//! - **the media bundles** ([`MEDIA_BUNDLE_SUFFIXES`]) — pruned by suffix at any
//!   depth, again only from a home-kind root;
//! - **the budget** ([`WalkBudget`]) — entries and wall clock, whichever runs out
//!   first, and a walk that stops says so ([`trailer_lines`]);
//! - **unreadable folders** — counted and named rather than swallowed (BR-13).
//!
//! The tools decide what an entry *means* (a match, a file to read); the driver
//! [`visit`] decides which entries are seen, and a tool that has all it can use
//! ends the walk by returning [`ControlFlow::Break`] from its callback (grep's
//! match cap — a stop the tool asked for, not a budget stop). Every harness line
//! a walker appends is written here — the trailer ([`trailer_lines`]) and both
//! tools' cap notices ([`cap_notice`]) — and starts with
//! [`HARNESS_LINE_PREFIX`], and [`is_harness_line`] recognises exactly the known
//! line shapes, so the triage duty never ranks a harness sentence as a match
//! and never peels a match that merely looks like one.

use std::fs;
use std::ops::ControlFlow;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use teton_core::ProvenanceId;
use teton_protocol::methods::RootKind;

use super::{denied_prefix_forms, skip_symlink_entry, under_denied_prefix};

/// Directory names **never** descended into, at any depth, from any root kind,
/// and whatever the pattern names: VCS internals and build output hold no
/// source a model should read. Unlike the BR-12 sets below, naming one of these
/// in a pattern does not enter it — `target/**/*.d` from a project root finds
/// nothing, as it always has.
///
/// `.hg`, `.svn` and `__pycache__` are inert in a repository that has none of
/// them and cost nothing to name; they join the three the walkers always had.
pub const WALK_SKIP_DIRS: &[&str] = &[
    ".git",
    "target",
    "node_modules",
    ".hg",
    ".svn",
    "__pycache__",
];

/// Directory names pruned **only** when the directory sits directly under a
/// user's home directory (BR-12): the macOS media and library trees the OS
/// gates behind a consent dialog, and the developer caches that hold nothing a
/// model wants and everything that makes a walk slow.
///
/// The position is the rule, not the name: `~/Library` is pruned, a `Library/`
/// inside a project under `~/Documents` is walked. See `Walk::parent_is_a_home`
/// for how "directly under a home" is decided for each root kind.
pub const HOME_TOP_LEVEL_SKIPS: &[&str] = &[
    "Library", "Music", "Pictures", "Movies", ".Trash", ".cache", ".cargo", ".npm", ".rustup",
    ".gradle", ".m2", ".nvm",
];

/// Media bundle suffixes pruned at **any** depth from a home-kind root
/// (BR-12): a bundle is one opaque library to the user, and on macOS opening
/// one is what raises the Photos / Music consent dialog.
pub const MEDIA_BUNDLE_SUFFIXES: &[&str] = &[".photoslibrary", ".musiclibrary"];

/// How many unreadable folders the trailer names before saying "and N more".
/// `pub(crate)`, like the other rendering details below: a fact about the
/// line's shape, not part of the walk policy.
pub(crate) const UNREADABLE_NAMED_MAX: usize = 5;

/// The prefix every harness line a walker writes starts with — the trailer
/// lines here and both tools' cap notices. One constant, so every writer and
/// the one recogniser ([`is_harness_line`]) spell the same bytes.
pub(crate) const HARNESS_LINE_PREFIX: &str = "... (";

/// The macOS-only clause of the unreadable line (BR-13).
///
/// `cfg!`-selected text rather than a `#[cfg]` item so both spellings compile
/// on every platform (the `service.rs` idiom): the line reads the same
/// everywhere up to this clause, and a Linux build still type-checks the macOS
/// sentence. `pub(crate)` so the tools' tests assert the clause they were
/// written against rather than a retyped copy of it.
pub(crate) const MACOS_CONSENT_CLAUSE: &str =
    " — macOS may have blocked access to that folder, or be waiting on a consent dialog for it";

/// What to do about a stopped walk — the same words whether zero or many
/// matches were found before the stop (BR-10: never a silent partial).
/// `pub(crate)` for the same reason as [`MACOS_CONSENT_CLAUSE`].
pub(crate) const STOPPED_ADVICE: &str = "narrow the pattern, or move the session root with /cd";

/// A walk's bound: entries seen and wall-clock elapsed, whichever runs out
/// first (BR-10).
///
/// The defaults are ADR-3's: 100,000 entries covers every realistic single
/// repository several times over (this workspace is ~2.5k entries outside the
/// skip set) while stopping a home-folder crawl well inside a shell timeout;
/// 10 s is the wall-clock backstop for a slow disk or a network mount.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct WalkBudget {
    /// Directory entries the walk may see (files and directories alike) before
    /// it stops.
    pub max_entries: usize,
    /// Wall clock the walk may spend before it stops.
    pub max_wall: Duration,
}

impl WalkBudget {
    /// ADR-3's default entry budget.
    pub const DEFAULT_MAX_ENTRIES: usize = 100_000;
    /// ADR-3's default wall-clock budget.
    pub const DEFAULT_MAX_WALL: Duration = Duration::from_secs(10);
}

impl Default for WalkBudget {
    fn default() -> Self {
        Self {
            max_entries: Self::DEFAULT_MAX_ENTRIES,
            max_wall: Self::DEFAULT_MAX_WALL,
        }
    }
}

/// Everything a walk decides that is not the tool's own business: the three
/// name sets and the budget (BR-11: one skip set, one media set, one budget —
/// defined once, read by every walker).
///
/// Rides on [`ToolContext`](super::ToolContext); the default is the module's
/// constants and [`WalkBudget::default`], and
/// [`ToolContext::with_walk_budget`](super::ToolContext::with_walk_budget) is
/// the test seam that shrinks the budget without a giant fixture (AC-14).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WalkPolicy {
    skip: &'static [&'static str],
    home_top: &'static [&'static str],
    bundles: &'static [&'static str],
    budget: WalkBudget,
    /// Absolute directories no walk under this policy lists or enters (REQ-611
    /// BR-8) — the session's transcript directory. Not `&'static` like the
    /// three name sets above: this one is a fact about the running daemon's
    /// configuration, not a constant of the harness.
    denied_prefixes: Vec<PathBuf>,
}

impl Default for WalkPolicy {
    fn default() -> Self {
        Self {
            skip: WALK_SKIP_DIRS,
            home_top: HOME_TOP_LEVEL_SKIPS,
            bundles: MEDIA_BUNDLE_SUFFIXES,
            budget: WalkBudget::default(),
            denied_prefixes: Vec::new(),
        }
    }
}

impl WalkPolicy {
    /// The same policy under a different budget.
    #[must_use]
    pub fn with_budget(mut self, budget: WalkBudget) -> Self {
        self.budget = budget;
        self
    }

    /// A policy for a **security** scan rather than a discovery walk (REQ-614
    /// ADR-614-5): nothing is pruned by name.
    ///
    /// [`WALK_SKIP_DIRS`] exists so a walk the *model* reads is not drowned in
    /// `node_modules` and `target`. A scan asking "does any file under this
    /// subtree match a privacy boundary?" is answering a different question,
    /// and the default skip set is actively wrong for it: `**/.npmrc` is one of
    /// the thirteen builtin boundaries and `node_modules/<pkg>/.npmrc` is where
    /// it most often lives. A pruned walk would report *no boundary file here*
    /// about a tree a `grep -r` reads in full — a false `rooted`, which is a
    /// leak rather than a slow walk.
    ///
    /// The home-top-level prune and the media-bundle suffixes go for the same
    /// reason; the denied prefixes stay, because a transcript is a file no tool
    /// may read at all and pruning it is not a boundary decision.
    ///
    /// The budget is the caller's, and it is expected to be far tighter than
    /// [`WalkBudget::default`]: this runs synchronously before a shell command
    /// spawns. A scan that exhausts it has **not** shown the absence of a
    /// boundary file — it stopped looking — and the caller must read
    /// [`WalkReport::truncated_by`] and fail closed.
    #[must_use]
    pub fn for_boundary_scan(budget: WalkBudget, denied_prefixes: Vec<PathBuf>) -> Self {
        Self {
            skip: &[],
            home_top: &[],
            bundles: &[],
            budget,
            denied_prefixes,
        }
    }

    /// The same policy with `dir` denied — REQ-611 BR-8 / ADR-7, the walker
    /// half of the transcript denial.
    ///
    /// Composed by
    /// [`ToolContext::with_denied_prefix`](super::ToolContext::with_denied_prefix),
    /// which sets the jail's half in the same call so the two cannot be given
    /// different directories. Additive, and the forms are
    /// [`denied_prefix_forms`]'s — see there for why the prefix is
    /// canonicalized and why a directory that does not exist yet still counts.
    #[must_use]
    pub fn with_denied_prefix(mut self, dir: impl AsRef<Path>) -> Self {
        self.denied_prefixes
            .extend(denied_prefix_forms(dir.as_ref()));
        self
    }

    /// The directories no walk under this policy lists or enters (BR-8).
    #[must_use]
    pub fn denied_prefixes(&self) -> &[PathBuf] {
        &self.denied_prefixes
    }

    /// The budget every walk under this policy runs under.
    #[must_use]
    pub fn budget(&self) -> WalkBudget {
        self.budget
    }

    /// The names never descended into, at any depth.
    #[must_use]
    pub fn skip_dirs(&self) -> &'static [&'static str] {
        self.skip
    }

    /// The names pruned directly under a home directory (BR-12).
    #[must_use]
    pub fn home_top_level_skips(&self) -> &'static [&'static str] {
        self.home_top
    }

    /// The bundle suffixes pruned at any depth from a home-kind root (BR-12).
    #[must_use]
    pub fn media_bundle_suffixes(&self) -> &'static [&'static str] {
        self.bundles
    }
}

/// Which half of the budget stopped a walk.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TruncatedBy {
    /// The entry budget: this many entries were seen, then the walk stopped.
    Entries(usize),
    /// The wall-clock budget: this much time had elapsed when the clock was
    /// read — after an entry was handed over, or before a descent — and the
    /// walk stopped there.
    WallClock(Duration),
}

/// What a walk has to say about itself beyond the entries it handed to the
/// tool — rendered by [`trailer_lines`], after the tool's own result lines.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct WalkReport {
    /// `Some` when the budget stopped the walk before the tree was exhausted.
    pub truncated_by: Option<TruncatedBy>,
    /// The first [`UNREADABLE_NAMED_MAX`] folders `read_dir` refused with a
    /// permission error, as root-relative display paths ending in `/`.
    pub unreadable: Vec<String>,
    /// Every folder `read_dir` failed on, permission errors and otherwise
    /// (BR-13: a vanished directory keeps today's skip-and-continue but is
    /// counted in the same line).
    pub unreadable_total: usize,
    /// Entries skipped because they sit under a denied prefix (REQ-611 BR-8) —
    /// neither handed to the tool nor descended into.
    ///
    /// **A count, and deliberately no trailer line.** Every other thing this
    /// report carries is rendered by [`trailer_lines`] for the model to read.
    /// This one is not, for the reason the denial exists: a transcript path is
    /// boundary content (REQ-569 BR-10, REQ-611 BR-15), and a walk that
    /// announced *"skipped 1 transcript directory"* would hand back a fact
    /// about where the file lives to the caller it was hidden from. It is the
    /// posture [`WALK_SKIP_DIRS`] already has — a prune nothing names — and the
    /// count exists so a test can tell a pruned tree from an absent one.
    pub denied: usize,
}

/// Walk `root` under `policy`, handing every entry seen to `on_entry` as
/// `(path, file type, root-relative identity)`, and report how the walk went.
/// `on_entry` answers [`ControlFlow::Continue`] to go on and
/// [`ControlFlow::Break`] to end the walk where it stands — the tool has all
/// it can use (grep at its match cap). A tool's stop is **not** a budget stop:
/// the report's `truncated_by` stays `None`, and the tool says what stopped it
/// (its cap notice) in its own words (BR-10: the caps stay as they are).
///
/// `root` is the jail root, already canonicalized by the caller (both walkers
/// canonicalize before anything else). `kind` gates the home-tree pruning
/// (BR-12 is inert from a `project`/`plain` root). `named_prefix` is the
/// pattern's leading literal segments ([`leading_literal_segments`]): a pruned
/// directory is still entered when the pattern names it or something under it,
/// so `Library/**/*.plist` from `~` enters `~/Library` (BR-12's "unless named").
///
/// # What the driver decides, and what it leaves to the tool
///
/// - Symlinks are skipped before anything else ([`skip_symlink_entry`],
///   REQ-571 BR-5): a link is never followed, so a walk cannot cycle and one
///   file cannot surface under two names.
/// - An entry under one of the policy's denied prefixes is skipped before
///   anything else sees it (REQ-611 BR-8): not listed, not entered, counted on
///   the report as [`WalkReport::denied`] and named nowhere.
/// - An entry whose identity cannot be minted surfaces nothing (REQ-571).
/// - Every entry seen — file or directory — is counted against the entry
///   budget and handed to `on_entry` **before** the driver decides whether to
///   descend, so a pruned directory is still *seen* (glob can list `Library/`
///   from `~`) even though nothing under it is.
/// - Within one directory, every entry is handed over in the order the
///   directory lists them; descents into children happen afterwards, **in name
///   order** — so which of two sibling trees a walk reaches first is a fact a
///   test can build on rather than whatever the filesystem's hash order
///   happened to be. The wall clock is read after **every** entry (a read is
///   tens of nanoseconds; the callback it follows may have read megabytes) and
///   again before each descent. The root itself is always entered.
///
/// A budget hit stops the whole walk, not one branch, and is recorded on the
/// report so the tool can say so ([`trailer_lines`]) whether it had found
/// nothing or everything by then (BR-10).
pub fn visit(
    root: &Path,
    kind: RootKind,
    named_prefix: &[&str],
    policy: &WalkPolicy,
    on_entry: &mut dyn FnMut(&Path, &fs::FileType, &ProvenanceId) -> ControlFlow<()>,
) -> WalkReport {
    let mut walk = Walk {
        root,
        kind,
        named_prefix,
        policy,
        on_entry,
        started: Instant::now(),
        seen: 0,
        report: WalkReport::default(),
    };
    walk.dir(root);
    walk.report
}

/// The root-relative segments under which macOS mounts the data volume — a
/// firmlink, so `/System/Volumes/Data/Users/<name>` *is* `/Users/<name>` and a
/// walk from `/` meets each home twice. Read as absent by
/// `Walk::parent_is_a_home`, so the BR-12 position rule holds on both spellings.
const MACOS_DATA_VOLUME_FIRMLINK: [&str; 3] = ["System", "Volumes", "Data"];

/// Why a directory is not descended into.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Prune {
    /// The skip set ([`WALK_SKIP_DIRS`]): never entered, from any root, whatever
    /// the pattern names.
    Always,
    /// The BR-12 sets ([`HOME_TOP_LEVEL_SKIPS`], [`MEDIA_BUNDLE_SUFFIXES`]) from
    /// a home-kind root: entered only when the pattern names the tree.
    UnlessNamed,
}

/// The state of one walk: the inputs, the clock, the count, and the report
/// being built.
struct Walk<'a> {
    root: &'a Path,
    kind: RootKind,
    named_prefix: &'a [&'a str],
    policy: &'a WalkPolicy,
    on_entry: &'a mut dyn FnMut(&Path, &fs::FileType, &ProvenanceId) -> ControlFlow<()>,
    started: Instant,
    /// Entries seen so far, files and directories alike.
    seen: usize,
    report: WalkReport,
}

impl Walk<'_> {
    /// Handle one directory. `false` means the walk must stop here — the
    /// budget ran out, or the tool said it has all it can use.
    fn dir(&mut self, dir: &Path) -> bool {
        let entries = match fs::read_dir(dir) {
            Ok(entries) => entries,
            Err(error) => {
                self.unreadable(dir, &error);
                return true;
            }
        };
        let mut children = Vec::new();
        for entry in entries.flatten() {
            self.seen += 1;
            if self.seen > self.policy.budget.max_entries {
                self.report.truncated_by =
                    Some(TruncatedBy::Entries(self.policy.budget.max_entries));
                return false;
            }
            let path = entry.path();
            let Ok(file_type) = entry.file_type() else {
                continue;
            };
            // REQ-571 ADR-C: a link is skipped before either branch below — this
            // is the deliberate posture for walking tools, tested on the entry
            // rather than inferred from `!is_dir()`. See `skip_symlink_entry` for
            // both halves of why.
            if skip_symlink_entry(file_type) {
                continue;
            }
            // REQ-611 BR-8 / ADR-7: a denied prefix is pruned **before** the
            // entry is handed over — the one prune that is. Every other one
            // below still lists the directory and only declines to descend
            // (`glob` can list `Library/` from `~`), which is right for a tree
            // whose *name* is unremarkable and wrong for a transcript
            // directory: a listed name is content, and BR-8 says a transcript
            // is never listed either. The entry has already been counted
            // against the budget, because `read_dir` did the work of producing
            // it whatever happens next.
            if under_denied_prefix(&self.policy.denied_prefixes, &path) {
                self.report.denied += 1;
                continue;
            }
            let Ok(id) = ProvenanceId::from_resolved(self.root, &path) else {
                continue;
            };
            if (self.on_entry)(&path, &file_type, &id).is_break() {
                // The tool's stop, not the budget's: `truncated_by` stays as it
                // is, and the tool reports the stop in its own words.
                return false;
            }
            // The clock, after every entry: the callback just returned from may
            // have read megabytes (grep's per-file cap), and a flat directory
            // of very many entries never reaches a descent — so neither a
            // batched read nor the per-descent read alone would bound it. A
            // read is tens of nanoseconds; it is never the cost of the walk.
            if self.wall_ran_out() {
                return false;
            }
            if file_type.is_dir() {
                let name = entry.file_name();
                let name = name.to_string_lossy();
                let segments: Vec<&str> = id.as_str().split('/').collect();
                match self.prune(&name, &segments) {
                    Some(Prune::Always) => continue,
                    Some(Prune::UnlessNamed) if !self.is_named(&segments) => continue,
                    _ => children.push(path),
                }
            }
        }
        // Name order, so the walk's reach is deterministic (see `visit`).
        children.sort();
        for child in children {
            if self.wall_ran_out() {
                return false;
            }
            if !self.dir(&child) {
                return false;
            }
        }
        true
    }

    /// Whether the wall-clock budget is spent — recording the stop on the
    /// report when it is, so every caller reports the same reading.
    fn wall_ran_out(&mut self) -> bool {
        if self.started.elapsed() >= self.policy.budget.max_wall {
            self.report.truncated_by = Some(TruncatedBy::WallClock(self.policy.budget.max_wall));
            return true;
        }
        false
    }

    /// Why — if at all — the policy says not to descend into the directory
    /// `name`, whose root-relative path is `segments`.
    ///
    /// The skip set is [`Prune::Always`]: never entered, and no pattern names
    /// its way in. The BR-12 sets are [`Prune::UnlessNamed`]: pruned only from a
    /// home-kind root, and entered when the pattern's leading literal segments
    /// name the tree ([`Walk::is_named`]).
    fn prune(&self, name: &str, segments: &[&str]) -> Option<Prune> {
        if self.policy.skip.contains(&name) {
            return Some(Prune::Always);
        }
        // BR-12 is inert from a `project`/`plain` root: a `Library/` there is
        // project content, and so is a bundle someone checked in.
        if !matches!(self.kind, RootKind::Home | RootKind::FilesystemRoot) {
            return None;
        }
        if self
            .policy
            .bundles
            .iter()
            .any(|suffix| name.ends_with(suffix))
        {
            return Some(Prune::UnlessNamed);
        }
        (self.policy.home_top.contains(&name) && self.parent_is_a_home(segments))
            .then_some(Prune::UnlessNamed)
    }

    /// Whether the directory at `segments` sits directly under a user's home
    /// directory — the position [`HOME_TOP_LEVEL_SKIPS`] applies at (BR-12).
    ///
    /// From a `home` root the home *is* the root, so the directory is a
    /// top-level child. From `/` a home is `/Users/<name>` or `/home/<name>`,
    /// so the directory is three deep with `Users`/`home` as its first segment
    /// — and on macOS the data volume is firmlinked at `/System/Volumes/Data`,
    /// so `/System/Volumes/Data/Users/<name>` is the same home reached the long
    /// way round; that leading run is read as if absent. Nowhere else:
    /// `~/Documents/GitHub/app/Library` is four deep from `~` and walked.
    fn parent_is_a_home(&self, segments: &[&str]) -> bool {
        match self.kind {
            RootKind::Home => segments.len() == 1,
            RootKind::FilesystemRoot => {
                let segments = segments
                    .strip_prefix(&MACOS_DATA_VOLUME_FIRMLINK)
                    .unwrap_or(segments);
                segments.len() == 3 && matches!(segments[0], "Users" | "home")
            }
            RootKind::Project | RootKind::Plain => false,
        }
    }

    /// Whether the pattern's leading literal segments name this directory or
    /// something under it — BR-12's "unless named": a [`Prune::UnlessNamed`]
    /// directory the caller asked for by name is entered. Never consulted for
    /// the skip set ([`Prune::Always`]).
    fn is_named(&self, segments: &[&str]) -> bool {
        self.named_prefix.starts_with(segments)
    }

    /// Record a `read_dir` failure (BR-13): a permission error is named (up to
    /// [`UNREADABLE_NAMED_MAX`]) and counted; any other error is counted only.
    fn unreadable(&mut self, dir: &Path, error: &std::io::Error) {
        self.report.unreadable_total += 1;
        if error.kind() != std::io::ErrorKind::PermissionDenied {
            return;
        }
        if self.report.unreadable.len() >= UNREADABLE_NAMED_MAX {
            return;
        }
        let display = match ProvenanceId::from_resolved(self.root, dir) {
            Ok(id) if !id.as_str().is_empty() => format!("{}/", id.as_str()),
            // The root itself, or a path with no identity: name it as "here".
            _ => "./".to_owned(),
        };
        self.report.unreadable.push(display);
    }
}

/// The harness lines a walker appends after its own result lines, in order:
/// the stopped line (if the budget ran out), then the unreadable line (if any
/// folder could not be read). Empty for a walk with nothing to add.
///
/// Every line starts with [`HARNESS_LINE_PREFIX`] (`... (`) and is a shape
/// [`is_harness_line`] knows; the tool appends its own [`cap_notice`] **after**
/// these, so that notice keeps the last position it always had.
#[must_use]
pub fn trailer_lines(report: &WalkReport) -> Vec<String> {
    let mut lines = Vec::new();
    match report.truncated_by {
        Some(TruncatedBy::Entries(n)) => {
            lines.push(format!(
                "{HARNESS_LINE_PREFIX}stopped after {n} entries; {STOPPED_ADVICE})"
            ));
        }
        Some(TruncatedBy::WallClock(elapsed)) => {
            lines.push(format!(
                "{HARNESS_LINE_PREFIX}stopped after {} s; {STOPPED_ADVICE})",
                format_secs(elapsed)
            ));
        }
        None => {}
    }
    if report.unreadable_total > 0 {
        lines.push(unreadable_line(report));
    }
    lines
}

/// The cap notice a tool appends when its own result cap was hit — `... (capped
/// at 200 matches)` for grep, `... (capped at 200 results)` for glob — written
/// here beside the other harness lines so every writer spells the prefix and
/// the shape [`is_harness_line`] knows from one place. `noun` is the tool's
/// unit (`matches`, `results`); a tool appends this **after** the walk's
/// trailer, keeping the last position it always had.
#[must_use]
pub fn cap_notice(cap: usize, noun: &str) -> String {
    format!("{HARNESS_LINE_PREFIX}capped at {cap} {noun})")
}

/// The BR-13 line: how many folders could not be read, which (up to
/// [`UNREADABLE_NAMED_MAX`], permission errors only), and — on macOS — why that
/// might be.
fn unreadable_line(report: &WalkReport) -> String {
    let n = report.unreadable_total;
    let noun = if n == 1 { "folder" } else { "folders" };
    let mut line = format!("{HARNESS_LINE_PREFIX}{n} {noun} could not be read");
    if !report.unreadable.is_empty() {
        line.push_str(" (permission denied): ");
        line.push_str(&report.unreadable.join(", "));
        let more = n.saturating_sub(report.unreadable.len());
        if more > 0 {
            line.push_str(&format!(" and {more} more"));
        }
        if cfg!(target_os = "macos") {
            line.push_str(MACOS_CONSENT_CLAUSE);
        }
    }
    line.push(')');
    line
}

/// Seconds, whole when they are whole (`10 s`), to one decimal otherwise.
fn format_secs(duration: Duration) -> String {
    if duration.subsec_nanos() == 0 {
        duration.as_secs().to_string()
    } else {
        format!("{:.1}", duration.as_secs_f64())
    }
}

/// Append `lines` to `out`, one per line, after whatever `out` already holds
/// — the one way a tool puts the walk's trailer (and its own cap notice) under
/// its result, so the five places that used to hand-roll the loop cannot drift
/// in where the newline goes.
pub fn append_trailer<I, S>(out: &mut String, lines: I)
where
    I: IntoIterator<Item = S>,
    S: AsRef<str>,
{
    for line in lines {
        out.push('\n');
        out.push_str(line.as_ref());
    }
}

/// Whether `line` is one the harness wrote rather than a tool's own result
/// line — the one recogniser grep's triage split uses (ADR-3), so a harness
/// line is never ranked as a match.
///
/// Recognised by **exact known shape**, not by the prefix alone: after
/// [`HARNESS_LINE_PREFIX`] the line continues `stopped after ` (the budget
/// lines), `<digits> folder(s) could not be read` (the unreadable line), or
/// `capped at ` (both tools' cap notices, [`cap_notice`]). A grep match from a
/// file whose path begins `... (` — which a `MATCH` line spelling
/// `... (x.rs:1: y` would produce — is a match, and is not peeled.
///
/// **The rule is two-sided.** Every writer of a harness line lives in this
/// module ([`trailer_lines`], [`cap_notice`]) and every shape is named here:
/// authoring a new harness line means adding its shape to this function in the
/// same change, and the test that enumerates every writer against this
/// recogniser (`every_harness_line_writer_is_recognised`) is what fails when
/// one side moves without the other.
#[must_use]
pub fn is_harness_line(line: &str) -> bool {
    let Some(body) = line.strip_prefix(HARNESS_LINE_PREFIX) else {
        return false;
    };
    if body.starts_with("stopped after ") || body.starts_with("capped at ") {
        return true;
    }
    let after_digits = body.trim_start_matches(|c: char| c.is_ascii_digit());
    after_digits.len() < body.len()
        && (after_digits.starts_with(" folder could not be read")
            || after_digits.starts_with(" folders could not be read"))
}

/// The segments of `pattern` before the first one containing a wildcard
/// (`*`/`?`): what the pattern names literally, and therefore what
/// [`visit`]'s `named_prefix` is. `Library/**/*.plist` → `["Library"]`;
/// `**/teton-code` → `[]`.
#[must_use]
pub fn leading_literal_segments(pattern: &str) -> Vec<&str> {
    pattern
        .split('/')
        .filter(|segment| !segment.is_empty())
        .take_while(|segment| !segment.contains(['*', '?']))
        .collect()
}

/// Whether the test process runs as root — who can read a mode-`000` directory,
/// so every unreadable-folder fixture skips itself. Through the daemon's own
/// uid reader rather than a third `unsafe { libc::geteuid() }`.
#[cfg(test)]
pub(crate) fn running_as_root() -> bool {
    crate::auth::current_uid() == 0
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
            "teton-walk-{tag}-{}-{}-{}",
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

    /// Create `rel` (a file) under `root`, with its parents.
    fn plant(root: &Path, rel: &str) {
        let path = root.join(rel);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, "planted\n").unwrap();
    }

    /// Every file identity a walk of `root` hands over, sorted, and its report.
    fn files_seen(
        root: &Path,
        kind: RootKind,
        named_prefix: &[&str],
        policy: &WalkPolicy,
    ) -> (Vec<String>, WalkReport) {
        let root = root.canonicalize().unwrap();
        let mut seen = Vec::new();
        let report = visit(&root, kind, named_prefix, policy, &mut |_, ft, id| {
            if !ft.is_dir() {
                seen.push(id.as_str().to_owned());
            }
            ControlFlow::Continue(())
        });
        seen.sort();
        (seen, report)
    }

    #[test]
    fn the_default_policy_is_the_shared_definition() {
        let policy = WalkPolicy::default();
        assert_eq!(policy.skip_dirs(), WALK_SKIP_DIRS);
        assert_eq!(policy.home_top_level_skips(), HOME_TOP_LEVEL_SKIPS);
        assert_eq!(policy.media_bundle_suffixes(), MEDIA_BUNDLE_SUFFIXES);
        assert_eq!(policy.budget(), WalkBudget::default());
        assert_eq!(WalkBudget::default().max_entries, 100_000);
        assert_eq!(WalkBudget::default().max_wall, Duration::from_secs(10));
        // REQ-611 BR-8: nothing is denied until a caller names a directory, so
        // every existing walker keeps the reach it had.
        assert!(policy.denied_prefixes().is_empty());
    }

    /// **REQ-611 BR-8 / ADR-7 — the walker seam of the transcript denial.**
    ///
    /// A denied prefix is pruned *before* the entry is handed over, which is
    /// what makes it different from every other prune here: the directory is
    /// never listed, nothing under it is walked, and the report counts it. The
    /// sibling tree beside it is walked exactly as before — the benign path,
    /// without which a walk that had simply found nothing would pass.
    ///
    /// The prefix is also named by the caller's `named_prefix` in the last leg.
    /// BR-12's sets are entered when a pattern names them; this one is not, and
    /// that is the whole difference between "an unremarkable tree you did not
    /// ask for" and "a file the tools do not read".
    ///
    /// **Mutation, run (conventions: show the test can fail).** Neutering the
    /// `under_denied_prefix` guard in `Walk::dir` takes `harness::tools` to
    /// **3 failed / 248 passed**: this test (the listing grows back to all
    /// seven entries, transcript directory included) and the `grep` legs of
    /// `tools/mod.rs::each_file_tool_refuses_an_in_root_transcript` and
    /// `the_transcript_denial_ignores_the_boundary_set`. Neutering the guard in
    /// `ToolContext::resolve` instead gives the same 3 / 248 the other way
    /// round, with **this** test green — the two seams are genuinely two
    /// (LESSON-502).
    #[test]
    fn visit_prunes_a_denied_prefix_and_walks_its_sibling() {
        let root = temp_root("denied");
        plant(&root, "transcripts/2026-09-03-abcdef.jsonl");
        plant(&root, "transcripts/nested/older.jsonl");
        plant(&root, "sibling/notes.jsonl");
        plant(&root, "top.rs");

        // Non-vacuity: with nothing denied the walk finds all three files, so a
        // shrunken result below is the denial and not a broken fixture.
        let (all, _) = files_seen(&root, RootKind::Project, &[], &WalkPolicy::default());
        assert_eq!(
            all,
            vec![
                "sibling/notes.jsonl",
                "top.rs",
                "transcripts/2026-09-03-abcdef.jsonl",
                "transcripts/nested/older.jsonl",
            ],
            "the fixture is not the one this test was written against"
        );

        let policy = WalkPolicy::default().with_denied_prefix(root.join("transcripts"));
        assert!(
            !policy.denied_prefixes().is_empty(),
            "the prefix composed to nothing, so the legs below would pass for the \
             wrong reason"
        );

        // Every entry, directories included — `files_seen` reports files only,
        // and BR-8's claim is that the directory is never *listed* either.
        let canonical = root.canonicalize().unwrap();
        let mut seen = Vec::new();
        let report = visit(
            &canonical,
            RootKind::Project,
            &[],
            &policy,
            &mut |_, _, id| {
                seen.push(id.as_str().to_owned());
                ControlFlow::Continue(())
            },
        );
        seen.sort();
        assert_eq!(
            seen,
            vec!["sibling", "sibling/notes.jsonl", "top.rs"],
            "a denied prefix was listed or entered"
        );
        assert_eq!(
            report.denied, 1,
            "the prune was not counted, so a future walk could stop pruning and \
             report exactly the same thing as one with no transcript directory"
        );
        assert_eq!(report.truncated_by, None, "{report:?}");
        assert_eq!(report.unreadable_total, 0, "{report:?}");

        // Naming the tree does not enter it (contrast BR-12's `UnlessNamed`).
        let mut named_seen = Vec::new();
        visit(
            &canonical,
            RootKind::Project,
            &["transcripts"],
            &policy,
            &mut |_, _, id| {
                named_seen.push(id.as_str().to_owned());
                ControlFlow::Continue(())
            },
        );
        assert!(
            named_seen.iter().all(|id| !id.starts_with("transcripts")),
            "naming the transcript directory walked into it: {named_seen:?}"
        );

        std::fs::remove_dir_all(&root).ok();
    }

    /// The skip set applies at any depth and from any root kind.
    #[test]
    fn skip_dirs_are_pruned_at_any_depth_from_any_root_kind() {
        let root = temp_root("skip");
        for name in WALK_SKIP_DIRS {
            plant(&root, &format!("{name}/hidden.rs"));
            plant(&root, &format!("deep/er/{name}/hidden.rs"));
        }
        plant(&root, "src/lib.rs");
        for kind in [
            RootKind::Project,
            RootKind::Plain,
            RootKind::Home,
            RootKind::FilesystemRoot,
        ] {
            let (seen, report) = files_seen(&root, kind, &[], &WalkPolicy::default());
            assert_eq!(seen, vec!["src/lib.rs"], "{kind:?}: {seen:?}");
            assert_eq!(report, WalkReport::default());
        }
        std::fs::remove_dir_all(&root).ok();
    }

    /// **BR-12, the position rule.** From a `home` root the top-level media
    /// and cache names are pruned; the same names deeper down are walked; a
    /// bundle suffix is pruned at any depth. From a `project`/`plain` root all
    /// of it is walked.
    #[test]
    fn home_top_level_names_are_pruned_only_directly_under_a_home() {
        let root = temp_root("home");
        for name in HOME_TOP_LEVEL_SKIPS {
            plant(&root, &format!("{name}/top.rs"));
            plant(&root, &format!("Documents/app/{name}/nested.rs"));
        }
        plant(&root, "Pictures/Photos Library.photoslibrary/deep.rs");
        plant(&root, "Documents/x.musiclibrary/deep.rs");
        plant(&root, "Documents/app/src/main.rs");

        let (seen, _) = files_seen(&root, RootKind::Home, &[], &WalkPolicy::default());
        for name in HOME_TOP_LEVEL_SKIPS {
            assert!(
                !seen.contains(&format!("{name}/top.rs")),
                "{name} directly under the home was entered: {seen:?}"
            );
            assert!(
                seen.contains(&format!("Documents/app/{name}/nested.rs")),
                "{name} inside a project under the home was pruned: {seen:?}"
            );
        }
        assert!(
            !seen.iter().any(|f| f.ends_with("deep.rs")),
            "a media bundle was entered: {seen:?}"
        );
        assert!(seen.contains(&"Documents/app/src/main.rs".to_owned()));

        for kind in [RootKind::Project, RootKind::Plain] {
            let (seen, _) = files_seen(&root, kind, &[], &WalkPolicy::default());
            for name in HOME_TOP_LEVEL_SKIPS {
                assert!(
                    seen.contains(&format!("{name}/top.rs")),
                    "{kind:?}: BR-12 must be inert, but {name} was pruned: {seen:?}"
                );
            }
            assert!(
                seen.contains(&"Documents/x.musiclibrary/deep.rs".to_owned()),
                "{kind:?}: BR-12 must be inert, but a bundle was pruned: {seen:?}"
            );
        }
        std::fs::remove_dir_all(&root).ok();
    }

    /// From `/`, a home is `Users/<name>` or `home/<name>`; the pruned position
    /// is directly under those and nowhere else.
    #[test]
    fn from_the_filesystem_root_a_home_is_users_or_home_slash_name() {
        let root = temp_root("fsroot");
        plant(&root, "Users/ada/Library/top.rs");
        plant(&root, "home/ada/Library/top.rs");
        plant(&root, "Users/ada/Documents/app/Library/nested.rs");
        plant(&root, "Users/Library/not-a-home.rs");
        plant(&root, "Library/root-level.rs");
        plant(&root, "opt/ada/Library/elsewhere.rs");

        let (seen, _) = files_seen(&root, RootKind::FilesystemRoot, &[], &WalkPolicy::default());
        assert!(
            !seen.contains(&"Users/ada/Library/top.rs".to_owned()),
            "{seen:?}"
        );
        assert!(
            !seen.contains(&"home/ada/Library/top.rs".to_owned()),
            "{seen:?}"
        );
        assert!(
            seen.contains(&"Users/ada/Documents/app/Library/nested.rs".to_owned()),
            "{seen:?}"
        );
        assert!(
            seen.contains(&"Users/Library/not-a-home.rs".to_owned()),
            "{seen:?}"
        );
        assert!(
            seen.contains(&"Library/root-level.rs".to_owned()),
            "{seen:?}"
        );
        assert!(
            seen.contains(&"opt/ada/Library/elsewhere.rs".to_owned()),
            "{seen:?}"
        );
        std::fs::remove_dir_all(&root).ok();
    }

    /// **BR-12's "unless named".** A pruned directory is entered when the
    /// pattern's leading literal segments name it or something under it — and
    /// only that one; naming `Library` does not unprune `Music`. The skip set
    /// is not nameable: `target/` stays out whatever the pattern says.
    #[test]
    fn a_named_prefix_enters_a_pruned_directory_but_never_the_skip_set() {
        let root = temp_root("named");
        plant(&root, "Library/Preferences/a.plist");
        plant(&root, "Library/Caches/b.plist");
        plant(&root, "Music/c.plist");
        plant(&root, "target/debug/build.log");

        let policy = WalkPolicy::default();
        let (seen, _) = files_seen(&root, RootKind::Home, &["Library"], &policy);
        assert_eq!(
            seen,
            vec!["Library/Caches/b.plist", "Library/Preferences/a.plist"]
        );

        // Naming something *under* the pruned directory enters it too; the
        // prefix overrides pruning, it does not narrow the walk (the pattern
        // matcher does that), so the sibling under `Library/` is seen as well.
        let (seen, _) = files_seen(&root, RootKind::Home, &["Library", "Preferences"], &policy);
        assert_eq!(
            seen,
            vec!["Library/Caches/b.plist", "Library/Preferences/a.plist"]
        );

        // The skip set is a different rule: never descended, from any root
        // kind, and naming it does not open it — "never" means never.
        for kind in [RootKind::Project, RootKind::Home] {
            for named in [&[][..], &["target"][..], &["target", "debug"][..]] {
                let (seen, _) = files_seen(&root, kind, named, &policy);
                assert!(
                    !seen.contains(&"target/debug/build.log".to_owned()),
                    "{kind:?} named {named:?}: the skip set was entered: {seen:?}"
                );
            }
        }

        let (seen, _) = files_seen(&root, RootKind::Home, &["Documents"], &policy);
        assert!(seen.is_empty(), "{seen:?}");
        std::fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn leading_literal_segments_stop_at_the_first_wildcard() {
        assert_eq!(
            leading_literal_segments("Library/**/*.plist"),
            vec!["Library"]
        );
        assert_eq!(
            leading_literal_segments("Library/Preferences/*.plist"),
            vec!["Library", "Preferences"]
        );
        assert_eq!(
            leading_literal_segments("**/teton-code"),
            Vec::<&str>::new()
        );
        assert_eq!(leading_literal_segments("a/b?/c"), vec!["a"]);
        assert_eq!(leading_literal_segments("/a//b/"), vec!["a", "b"]);
        assert_eq!(leading_literal_segments(""), Vec::<&str>::new());
    }

    /// **BR-10, entries.** The walk stops after `max_entries` entries, and the
    /// report says so; a tree under the budget reports nothing.
    #[test]
    fn the_entry_budget_stops_the_walk_and_is_reported() {
        let root = temp_root("entries");
        for n in 0..10 {
            plant(&root, &format!("f{n}.rs"));
        }
        let policy = WalkPolicy::default().with_budget(WalkBudget {
            max_entries: 3,
            max_wall: Duration::from_secs(60),
        });
        let (seen, report) = files_seen(&root, RootKind::Plain, &[], &policy);
        assert_eq!(seen.len(), 3, "{seen:?}");
        assert_eq!(report.truncated_by, Some(TruncatedBy::Entries(3)));
        assert_eq!(
            trailer_lines(&report),
            vec!["... (stopped after 3 entries; narrow the pattern, or move the session root with /cd)"]
        );

        let (seen, report) = files_seen(&root, RootKind::Plain, &[], &WalkPolicy::default());
        assert_eq!(seen.len(), 10);
        assert_eq!(report.truncated_by, None);
        assert!(trailer_lines(&report).is_empty());
        std::fs::remove_dir_all(&root).ok();
    }

    /// **BR-10, wall clock.** The clock is read after every entry, so a zero
    /// budget hands over exactly the root's first entry — whichever the
    /// directory lists first — and stops before anything under a child is
    /// seen: deterministic, no sleep, no giant fixture. The root itself is
    /// always entered.
    #[test]
    fn the_wall_clock_budget_stops_the_walk_after_the_first_entry() {
        let root = temp_root("wall");
        plant(&root, "top.rs");
        plant(&root, "sub/deep.rs");
        let policy = WalkPolicy::default().with_budget(WalkBudget {
            max_entries: 1_000,
            max_wall: Duration::ZERO,
        });
        let canonical = root.canonicalize().unwrap();
        let mut handed = Vec::new();
        let report = visit(
            &canonical,
            RootKind::Plain,
            &[],
            &policy,
            &mut |_, _, id| {
                handed.push(id.as_str().to_owned());
                ControlFlow::Continue(())
            },
        );
        assert_eq!(handed.len(), 1, "one entry, then the clock: {handed:?}");
        assert!(
            handed[0] == "top.rs" || handed[0] == "sub",
            "the root's own entry, never a child's: {handed:?}"
        );
        assert_eq!(
            report.truncated_by,
            Some(TruncatedBy::WallClock(Duration::ZERO))
        );
        assert_eq!(
            trailer_lines(&report),
            vec!["... (stopped after 0 s; narrow the pattern, or move the session root with /cd)"]
        );
        std::fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn seconds_render_whole_or_to_one_decimal() {
        assert_eq!(format_secs(Duration::from_secs(10)), "10");
        assert_eq!(format_secs(Duration::from_millis(1_500)), "1.5");
        assert_eq!(format_secs(Duration::ZERO), "0");
    }

    /// **BR-13.** An unreadable folder is named and counted; the walk goes on
    /// and the rest of the tree is still seen. Skipped as root, who can read
    /// anything.
    #[test]
    fn an_unreadable_folder_is_reported_and_the_walk_continues() {
        use std::os::unix::fs::PermissionsExt;
        if running_as_root() {
            eprintln!("skipped: running as root, mode 000 does not refuse");
            return;
        }
        let root = temp_root("unreadable");
        plant(&root, "secrets/hidden.rs");
        plant(&root, "src/main.rs");
        let locked = root.join("secrets");
        std::fs::set_permissions(&locked, std::fs::Permissions::from_mode(0o000)).unwrap();

        let (seen, report) = files_seen(&root, RootKind::Project, &[], &WalkPolicy::default());
        std::fs::set_permissions(&locked, std::fs::Permissions::from_mode(0o755)).unwrap();

        assert_eq!(seen, vec!["src/main.rs"], "{seen:?}");
        assert_eq!(report.unreadable, vec!["secrets/"]);
        assert_eq!(report.unreadable_total, 1);
        let lines = trailer_lines(&report);
        assert_eq!(lines.len(), 1, "{lines:?}");
        assert!(
            lines[0].starts_with("... (1 folder could not be read (permission denied): secrets/"),
            "{}",
            lines[0]
        );
        assert_eq!(
            lines[0].contains(MACOS_CONSENT_CLAUSE),
            cfg!(target_os = "macos"),
            "the consent clause is macOS-only: {}",
            lines[0]
        );
        assert!(is_harness_line(&lines[0]));
        std::fs::remove_dir_all(&root).ok();
    }

    /// The unreadable line names at most [`UNREADABLE_NAMED_MAX`] folders and
    /// counts the rest; a failure that is not a permission error is counted
    /// but not named.
    #[test]
    fn the_unreadable_line_caps_the_names_and_counts_the_rest() {
        let report = WalkReport {
            truncated_by: None,
            unreadable: vec![
                "a/".into(),
                "b/".into(),
                "c/".into(),
                "d/".into(),
                "e/".into(),
            ],
            unreadable_total: 7,
            ..WalkReport::default()
        };
        let line = unreadable_line(&report);
        assert!(
            line.starts_with("... (7 folders could not be read (permission denied): a/, b/, c/, d/, e/ and 2 more"),
            "{line}"
        );
        assert!(line.ends_with(')'), "{line}");

        let counted_only = WalkReport {
            truncated_by: None,
            unreadable: vec![],
            unreadable_total: 2,
            ..WalkReport::default()
        };
        assert_eq!(
            unreadable_line(&counted_only),
            "... (2 folders could not be read)"
        );
    }

    /// Both trailer lines, in order, and every one of them is a harness line.
    #[test]
    fn trailer_lines_come_in_order_and_all_wear_the_prefix() {
        let report = WalkReport {
            truncated_by: Some(TruncatedBy::Entries(5)),
            unreadable: vec!["x/".into()],
            unreadable_total: 1,
            ..WalkReport::default()
        };
        let lines = trailer_lines(&report);
        assert_eq!(lines.len(), 2);
        assert!(lines[0].contains("stopped after 5 entries"), "{}", lines[0]);
        assert!(
            lines[1].contains("1 folder could not be read"),
            "{}",
            lines[1]
        );
        assert!(lines.iter().all(|l| is_harness_line(l)), "{lines:?}");
        assert!(!is_harness_line("src/a.rs:1: ... (not a harness line)"));
        assert!(!is_harness_line(
            "[triage: 2 of 3 matches, most useful first]"
        ));
    }

    /// The recogniser knows the harness's line shapes and nothing else: the
    /// prefix alone is not enough. A match line from a file whose *path*
    /// begins `... (` is a match, not a trailer.
    #[test]
    fn a_harness_line_is_recognised_by_its_exact_shape_not_by_the_prefix_alone() {
        for line in [
            format!("{HARNESS_LINE_PREFIX}stopped after 3 entries; {STOPPED_ADVICE})"),
            format!("{HARNESS_LINE_PREFIX}stopped after 0.5 s; {STOPPED_ADVICE})"),
            format!("{HARNESS_LINE_PREFIX}1 folder could not be read (permission denied): a/)"),
            format!("{HARNESS_LINE_PREFIX}12 folders could not be read)"),
            format!("{HARNESS_LINE_PREFIX}capped at 200 matches)"),
            format!("{HARNESS_LINE_PREFIX}capped at 200 results)"),
        ] {
            assert!(is_harness_line(&line), "{line}");
        }
        for line in [
            "... (x.rs:1: let needle = 1;",
            "... (stopped.rs:1: fn stopped_after()",
            "... (folder/a.rs:3: x",
            "... (3 folder-shaped names.rs:1: y",
            "... (",
            "...",
            "",
            "capped at 200 matches",
        ] {
            assert!(
                !is_harness_line(line),
                "{line:?} must not be peeled as a harness line"
            );
        }
    }

    /// The tool ends the walk with `Break`, and that is **not** a budget
    /// stop: nothing after the break is seen, and the report says the budget
    /// was not touched.
    #[test]
    fn a_tools_break_ends_the_walk_without_a_budget_stop() {
        let root = temp_root("break");
        plant(&root, "a/one.rs");
        plant(&root, "a/two.rs");
        plant(&root, "b/three.rs");
        let canonical = root.canonicalize().unwrap();
        let mut seen = Vec::new();
        let report = visit(
            &canonical,
            RootKind::Plain,
            &[],
            &WalkPolicy::default(),
            &mut |_, ft, id| {
                if ft.is_dir() {
                    return ControlFlow::Continue(());
                }
                seen.push(id.as_str().to_owned());
                if id.as_str().starts_with("a/") {
                    ControlFlow::Break(())
                } else {
                    ControlFlow::Continue(())
                }
            },
        );
        // Children are entered in name order, so `a/` is entered before `b/`;
        // the first file under `a/` breaks, and `b/` is never reached.
        assert_eq!(seen.len(), 1, "{seen:?}");
        assert!(seen[0].starts_with("a/"), "{seen:?}");
        assert_eq!(
            report,
            WalkReport::default(),
            "a tool's stop is not a budget stop"
        );
        std::fs::remove_dir_all(&root).ok();
    }

    /// **BR-10, wall clock inside one directory.** The clock is read after
    /// every entry, so a flat directory of many files — which never reaches a
    /// descent — stops on a spent budget after its first entry, not at its end.
    #[test]
    fn the_wall_clock_is_read_after_every_entry_within_a_flat_directory() {
        let root = temp_root("wall-flat");
        for n in 0..300 {
            plant(&root, &format!("f{n:04}.rs"));
        }
        let policy = WalkPolicy::default().with_budget(WalkBudget {
            max_entries: 100_000,
            max_wall: Duration::ZERO,
        });
        let (seen, report) = files_seen(&root, RootKind::Plain, &[], &policy);
        assert_eq!(
            report.truncated_by,
            Some(TruncatedBy::WallClock(Duration::ZERO)),
            "a flat directory must still meet the wall clock"
        );
        assert_eq!(
            seen.len(),
            1,
            "the stop lands after the first entry, not at the end: {} seen",
            seen.len()
        );
        std::fs::remove_dir_all(&root).ok();
    }

    /// **Descents are name-ordered, pinned directly** (`children.sort()`):
    /// three sibling directories created as `c/`, `a/`, `b/` — an order a
    /// filesystem's hash order may well return — are entered `a`, `b`, `c`,
    /// which the *raw* order the files under them are handed over in shows.
    /// Captured here rather than through `files_seen`, which sorts.
    #[test]
    fn child_directories_are_entered_in_name_order_whatever_the_listing_order() {
        let root = temp_root("order");
        for dir in ["c", "a", "b"] {
            plant(&root, &format!("{dir}/inside.rs"));
        }
        let canonical = root.canonicalize().unwrap();
        let mut descents = Vec::new();
        let report = visit(
            &canonical,
            RootKind::Plain,
            &[],
            &WalkPolicy::default(),
            &mut |_, ft, id| {
                if !ft.is_dir() {
                    descents.push(id.as_str().to_owned());
                }
                ControlFlow::Continue(())
            },
        );
        assert_eq!(
            descents,
            vec!["a/inside.rs", "b/inside.rs", "c/inside.rs"],
            "descents must be lexicographic"
        );
        assert_eq!(report, WalkReport::default());
        std::fs::remove_dir_all(&root).ok();
    }

    /// **Every writer of a harness line is recognised** — the two-sided rule
    /// `is_harness_line` documents, enumerated: each `trailer_lines` variant
    /// (the entry stop, the wall-clock stop whole and fractional, the unreadable
    /// line with names, with names and "and N more", and counted-only) and
    /// both tools' cap notices. A new writer belongs in this list, and a new
    /// shape belongs in the recogniser, in the same change.
    #[test]
    fn every_harness_line_writer_is_recognised() {
        // `WalkReport::denied` (REQ-611 BR-8) is absent from this enumeration on
        // purpose: it writes no harness line, because naming a transcript
        // directory in tool output is exactly what the denial exists to stop.
        let reports = [
            WalkReport {
                truncated_by: Some(TruncatedBy::Entries(100_000)),
                ..WalkReport::default()
            },
            WalkReport {
                truncated_by: Some(TruncatedBy::WallClock(Duration::from_secs(10))),
                ..WalkReport::default()
            },
            WalkReport {
                truncated_by: Some(TruncatedBy::WallClock(Duration::from_millis(1_500))),
                ..WalkReport::default()
            },
            WalkReport {
                truncated_by: None,
                unreadable: vec!["a/".into()],
                unreadable_total: 1,
                ..WalkReport::default()
            },
            WalkReport {
                truncated_by: None,
                unreadable: vec!["a/".into(), "b/".into()],
                unreadable_total: 9,
                ..WalkReport::default()
            },
            WalkReport {
                truncated_by: None,
                unreadable: vec![],
                unreadable_total: 3,
                ..WalkReport::default()
            },
            // Both halves at once, in order.
            WalkReport {
                truncated_by: Some(TruncatedBy::Entries(7)),
                unreadable: vec!["x/".into()],
                unreadable_total: 1,
                ..WalkReport::default()
            },
        ];
        let mut lines: Vec<String> = reports.iter().flat_map(trailer_lines).collect();
        assert_eq!(lines.len(), 8, "{lines:?}");
        lines.push(cap_notice(200, "matches"));
        lines.push(cap_notice(200, "results"));
        lines.push(cap_notice(1, "results"));
        for line in &lines {
            assert!(
                line.starts_with(HARNESS_LINE_PREFIX),
                "a writer dropped the prefix: {line}"
            );
            assert!(
                is_harness_line(line),
                "a writer's shape is unknown to the recogniser: {line}"
            );
        }
        // And the empty report writes nothing, so nothing is peeled.
        assert!(trailer_lines(&WalkReport::default()).is_empty());
    }

    /// **The macOS firmlink (BR-12 from `/`).** `/System/Volumes/Data/Users/<n>`
    /// is `/Users/<n>` reached the long way round, so its top-level media trees
    /// are pruned exactly as the short spelling's are — and only at that
    /// position: `/System/Volumes/Data/Library` is a system tree, walked.
    #[test]
    fn the_data_volume_firmlink_is_read_as_absent_when_judging_a_home() {
        let root = temp_root("firmlink");
        plant(&root, "System/Volumes/Data/Users/ada/Library/top.rs");
        plant(&root, "System/Volumes/Data/Users/ada/Music/song.rs");
        plant(
            &root,
            "System/Volumes/Data/Users/ada/Documents/app/Library/nested.rs",
        );
        plant(&root, "System/Volumes/Data/Library/system.rs");
        plant(&root, "System/Volumes/Library/other.rs");
        plant(&root, "Users/ada/Library/top.rs");

        let (seen, _) = files_seen(&root, RootKind::FilesystemRoot, &[], &WalkPolicy::default());
        for pruned in [
            "System/Volumes/Data/Users/ada/Library/top.rs",
            "System/Volumes/Data/Users/ada/Music/song.rs",
            "Users/ada/Library/top.rs",
        ] {
            assert!(
                !seen.contains(&pruned.to_owned()),
                "{pruned} was entered: {seen:?}"
            );
        }
        for walked in [
            "System/Volumes/Data/Users/ada/Documents/app/Library/nested.rs",
            "System/Volumes/Data/Library/system.rs",
            "System/Volumes/Library/other.rs",
        ] {
            assert!(
                seen.contains(&walked.to_owned()),
                "{walked} was pruned: {seen:?}"
            );
        }
        // Naming the tree still enters it through the long spelling.
        let (seen, _) = files_seen(
            &root,
            RootKind::FilesystemRoot,
            &["System", "Volumes", "Data", "Users", "ada", "Library"],
            &WalkPolicy::default(),
        );
        assert!(
            seen.contains(&"System/Volumes/Data/Users/ada/Library/top.rs".to_owned()),
            "{seen:?}"
        );
        std::fs::remove_dir_all(&root).ok();
    }

    /// The root itself unreadable: the walk hands over nothing, and the report
    /// names the root as `./` — the one path with no identity to spell.
    #[test]
    fn an_unreadable_root_is_reported_as_here() {
        use std::os::unix::fs::PermissionsExt;
        if running_as_root() {
            eprintln!("skipped: running as root, mode 000 does not refuse");
            return;
        }
        let root = temp_root("unreadable-root");
        plant(&root, "a.rs");
        std::fs::set_permissions(&root, std::fs::Permissions::from_mode(0o000)).unwrap();
        // Driven on the raw path: `read_dir` fails before any entry exists to
        // resolve, so nothing here needs the canonical spelling.
        let mut seen = 0usize;
        let report = visit(
            &root,
            RootKind::Plain,
            &[],
            &WalkPolicy::default(),
            &mut |_, _, _| {
                seen += 1;
                ControlFlow::Continue(())
            },
        );
        std::fs::set_permissions(&root, std::fs::Permissions::from_mode(0o755)).unwrap();
        assert_eq!(seen, 0);
        assert_eq!(report.unreadable, vec!["./"]);
        assert_eq!(report.unreadable_total, 1);
        let lines = trailer_lines(&report);
        assert!(
            lines[0].starts_with(&format!(
                "{HARNESS_LINE_PREFIX}1 folder could not be read (permission denied): ./"
            )),
            "{}",
            lines[0]
        );
        std::fs::remove_dir_all(&root).ok();
    }

    /// The other unreadable arm (BR-13's "other `read_dir` errors keep
    /// skip-and-continue but are counted"): a directory that vanishes between
    /// being seen and being entered — removed by the callback itself, which is
    /// the deterministic way to stage "removed mid-walk" — is counted, not
    /// named, and the walk goes on.
    #[test]
    fn a_directory_that_vanishes_mid_walk_is_counted_but_not_named() {
        let root = temp_root("vanish");
        plant(&root, "gone/inside.rs");
        plant(&root, "kept/inside.rs");
        let canonical = root.canonicalize().unwrap();
        let mut seen = Vec::new();
        let report = visit(
            &canonical,
            RootKind::Plain,
            &[],
            &WalkPolicy::default(),
            &mut |path, ft, id| {
                if ft.is_dir() && id.as_str() == "gone" {
                    std::fs::remove_dir_all(path).unwrap();
                } else if !ft.is_dir() {
                    seen.push(id.as_str().to_owned());
                }
                ControlFlow::Continue(())
            },
        );
        assert_eq!(seen, vec!["kept/inside.rs"], "{seen:?}");
        assert_eq!(report.unreadable_total, 1, "{report:?}");
        assert!(
            report.unreadable.is_empty(),
            "a vanished folder is not a permission error"
        );
        assert_eq!(
            trailer_lines(&report),
            vec![format!("{HARNESS_LINE_PREFIX}1 folder could not be read)")]
        );
        std::fs::remove_dir_all(&root).ok();
    }

    /// The filesystem root under a one-entry budget: no panic, a stopped line,
    /// nothing else. The smallest walk `/` can be asked for.
    #[test]
    fn the_filesystem_root_under_a_one_entry_budget_stops_without_panicking() {
        let policy = WalkPolicy::default().with_budget(WalkBudget {
            max_entries: 1,
            max_wall: Duration::from_secs(60),
        });
        let mut seen = 0usize;
        let report = visit(
            Path::new("/"),
            RootKind::FilesystemRoot,
            &[],
            &policy,
            &mut |_, _, _| {
                seen += 1;
                ControlFlow::Continue(())
            },
        );
        assert!(seen <= 1, "{seen}");
        assert_eq!(report.truncated_by, Some(TruncatedBy::Entries(1)));
        assert_eq!(
            trailer_lines(&report),
            vec![format!(
                "{HARNESS_LINE_PREFIX}stopped after 1 entries; {STOPPED_ADVICE})"
            )]
        );
    }

    /// Symlinks are skipped before anything else (REQ-571 BR-5): a link to a
    /// directory is neither entered nor counted as a place to descend.
    #[test]
    fn symlinks_are_never_followed() {
        let root = temp_root("links");
        plant(&root, "real/a.rs");
        std::os::unix::fs::symlink(root.join("real"), root.join("link")).unwrap();
        std::os::unix::fs::symlink(root.join("real/a.rs"), root.join("a-link.rs")).unwrap();
        let (seen, _) = files_seen(&root, RootKind::Plain, &[], &WalkPolicy::default());
        assert_eq!(seen, vec!["real/a.rs"], "{seen:?}");
        std::fs::remove_dir_all(&root).ok();
    }
}
