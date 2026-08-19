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
//! [`visit`] decides which entries are seen. Every harness line a walker appends
//! is written here and starts with `... (` — the shape grep's cap notice already
//! had — so [`is_harness_line`] is one recogniser for all of them and the triage
//! duty never ranks a harness sentence as a match.

use std::fs;
use std::path::Path;
use std::time::{Duration, Instant};

use teton_core::ProvenanceId;
use teton_protocol::methods::RootKind;

use super::skip_symlink_entry;

/// Directory names never descended into, at any depth and from any root kind:
/// VCS internals and build output hold no source a model should read.
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
pub const UNREADABLE_NAMED_MAX: usize = 5;

/// The macOS-only clause of the unreadable line (BR-13).
///
/// `cfg!`-selected text rather than a `#[cfg]` item so both spellings compile
/// on every platform (the `service.rs` idiom): the line reads the same
/// everywhere up to this clause, and a Linux build still type-checks the macOS
/// sentence.
const MACOS_CONSENT_CLAUSE: &str =
    " — macOS may have blocked access to that folder, or be waiting on a consent dialog for it";

/// What to do about a stopped walk — the same words whether zero or many
/// matches were found before the stop (BR-10: never a silent partial).
const STOPPED_ADVICE: &str = "narrow the pattern, or move the session root with /cd";

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
}

impl Default for WalkPolicy {
    fn default() -> Self {
        Self {
            skip: WALK_SKIP_DIRS,
            home_top: HOME_TOP_LEVEL_SKIPS,
            bundles: MEDIA_BUNDLE_SUFFIXES,
            budget: WalkBudget::default(),
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
    /// The wall-clock budget: this much time had elapsed at a directory
    /// boundary, and the walk stopped there.
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
}

/// Walk `root` under `policy`, handing every entry seen to `on_entry` as
/// `(path, file type, root-relative identity)`, and report how the walk went.
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
/// - An entry whose identity cannot be minted surfaces nothing (REQ-571).
/// - Every entry seen — file or directory — is counted against the entry
///   budget and handed to `on_entry` **before** the driver decides whether to
///   descend, so a pruned directory is still *seen* (glob can list `Library/`
///   from `~`) even though nothing under it is.
/// - Within one directory, files are handed over first and child directories
///   are entered afterwards; the wall clock is read once per directory
///   boundary, before each descent. The root itself is always entered.
///
/// A budget hit stops the whole walk, not one branch, and is recorded on the
/// report so the tool can say so ([`trailer_lines`]) whether it had found
/// nothing or everything by then (BR-10).
pub fn visit(
    root: &Path,
    kind: RootKind,
    named_prefix: &[&str],
    policy: &WalkPolicy,
    on_entry: &mut dyn FnMut(&Path, &fs::FileType, &ProvenanceId),
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

/// The state of one walk: the inputs, the clock, the count, and the report
/// being built.
struct Walk<'a> {
    root: &'a Path,
    kind: RootKind,
    named_prefix: &'a [&'a str],
    policy: &'a WalkPolicy,
    on_entry: &'a mut dyn FnMut(&Path, &fs::FileType, &ProvenanceId),
    started: Instant,
    /// Entries seen so far, files and directories alike.
    seen: usize,
    report: WalkReport,
}

impl Walk<'_> {
    /// Handle one directory. `false` means the budget ran out and the whole
    /// walk must stop.
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
            let Ok(id) = ProvenanceId::from_resolved(self.root, &path) else {
                continue;
            };
            (self.on_entry)(&path, &file_type, &id);
            if file_type.is_dir() {
                let name = entry.file_name();
                let name = name.to_string_lossy();
                let segments: Vec<&str> = id.as_str().split('/').collect();
                if self.is_pruned(&name, &segments) && !self.is_named(&segments) {
                    continue;
                }
                children.push(path);
            }
        }
        for child in children {
            if self.started.elapsed() >= self.policy.budget.max_wall {
                self.report.truncated_by =
                    Some(TruncatedBy::WallClock(self.policy.budget.max_wall));
                return false;
            }
            if !self.dir(&child) {
                return false;
            }
        }
        true
    }

    /// Whether the policy says not to descend into the directory `name`, whose
    /// root-relative path is `segments`.
    fn is_pruned(&self, name: &str, segments: &[&str]) -> bool {
        if self.policy.skip.contains(&name) {
            return true;
        }
        // BR-12 is inert from a `project`/`plain` root: a `Library/` there is
        // project content, and so is a bundle someone checked in.
        if !matches!(self.kind, RootKind::Home | RootKind::FilesystemRoot) {
            return false;
        }
        if self
            .policy
            .bundles
            .iter()
            .any(|suffix| name.ends_with(suffix))
        {
            return true;
        }
        self.policy.home_top.contains(&name) && self.parent_is_a_home(segments)
    }

    /// Whether the directory at `segments` sits directly under a user's home
    /// directory — the position [`HOME_TOP_LEVEL_SKIPS`] applies at (BR-12).
    ///
    /// From a `home` root the home *is* the root, so the directory is a
    /// top-level child. From `/` a home is `/Users/<name>` or `/home/<name>`,
    /// so the directory is three deep with `Users`/`home` as its first segment.
    /// Nowhere else: `~/Documents/GitHub/app/Library` is four deep from `~` and
    /// walked.
    fn parent_is_a_home(&self, segments: &[&str]) -> bool {
        match self.kind {
            RootKind::Home => segments.len() == 1,
            RootKind::FilesystemRoot => {
                segments.len() == 3 && matches!(segments[0], "Users" | "home")
            }
            RootKind::Project | RootKind::Plain => false,
        }
    }

    /// Whether the pattern's leading literal segments name this directory or
    /// something under it — BR-12's "unless named": a pruned directory the
    /// caller asked for by name is entered.
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
/// Every line starts with `... (` — see [`is_harness_line`] — and the tool
/// appends its own cap notice **after** these, so that notice keeps the last
/// position it always had.
#[must_use]
pub fn trailer_lines(report: &WalkReport) -> Vec<String> {
    let mut lines = Vec::new();
    match report.truncated_by {
        Some(TruncatedBy::Entries(n)) => {
            lines.push(format!("... (stopped after {n} entries; {STOPPED_ADVICE})"));
        }
        Some(TruncatedBy::WallClock(elapsed)) => {
            lines.push(format!(
                "... (stopped after {} s; {STOPPED_ADVICE})",
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

/// The BR-13 line: how many folders could not be read, which (up to
/// [`UNREADABLE_NAMED_MAX`], permission errors only), and — on macOS — why that
/// might be.
fn unreadable_line(report: &WalkReport) -> String {
    let n = report.unreadable_total;
    let noun = if n == 1 { "folder" } else { "folders" };
    let mut line = format!("... ({n} {noun} could not be read");
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

/// Whether `line` is one the harness wrote rather than a tool's own result
/// line: every trailer line here, and the walkers' cap notices, start with
/// `... (`. The one recogniser grep's triage split uses (ADR-3), so a new
/// harness line is never ranked as a match by accident.
#[must_use]
pub fn is_harness_line(line: &str) -> bool {
    line.starts_with("... (")
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
    /// only that one; naming `Library` does not unprune `Music`.
    #[test]
    fn a_named_prefix_enters_a_pruned_directory() {
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

        // The rule is one rule: naming a skip-set directory enters it too
        // (from a project root, where BR-12 is inert and only the skip set
        // prunes).
        let (seen, _) = files_seen(&root, RootKind::Project, &[], &policy);
        assert!(
            !seen.contains(&"target/debug/build.log".to_owned()),
            "{seen:?}"
        );
        let (seen, _) = files_seen(&root, RootKind::Project, &["target", "debug"], &policy);
        assert!(
            seen.contains(&"target/debug/build.log".to_owned()),
            "{seen:?}"
        );

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

    /// **BR-10, wall clock.** The clock is read at each directory boundary,
    /// so a zero budget lets the root's own files through and stops at the
    /// first descent — deterministic, no sleep, no giant fixture.
    #[test]
    fn the_wall_clock_budget_stops_the_walk_at_a_directory_boundary() {
        let root = temp_root("wall");
        plant(&root, "top.rs");
        plant(&root, "sub/deep.rs");
        let policy = WalkPolicy::default().with_budget(WalkBudget {
            max_entries: 1_000,
            max_wall: Duration::ZERO,
        });
        let (seen, report) = files_seen(&root, RootKind::Plain, &[], &policy);
        assert_eq!(seen, vec!["top.rs"], "the root is always entered");
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
        // SAFETY: `geteuid` reads the process's effective uid; no arguments.
        if unsafe { libc::geteuid() } == 0 {
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
