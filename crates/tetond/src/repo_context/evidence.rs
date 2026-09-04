//! The evidence gatherer for REQ-613 (ADR-3): one bounded walk, two closed
//! tables, a rendered tree, boundary exclusion, and the priority cut.
//!
//! [`gather`] is the whole of BR-3 and the exclusion half of BR-4, and it is
//! deliberately one function over two injected seams — REQ-612's
//! [`RepoFileReader`] for everything the filesystem *says*, and
//! [`walk::visit`] for everything it *lists*. Nothing here scans for projects,
//! nothing here opens a path that is not named by one of the two tables, and
//! nothing here runs before the human (or a durable `always`) said so: the
//! caller owns the gate, and this module owns what happens after it.
//!
//! # One walk, not one per table
//!
//! The walk is the expensive act — up to [`WalkBudget::DEFAULT_MAX_ENTRIES`]
//! entries and ten seconds of wall clock — so it runs exactly once and every
//! other question is answered from the listing it produced:
//!
//! - the **tree** the model reads is that listing, rendered breadth-first with
//!   a per-directory count by extension (a language profile, so a deep
//!   `src/main/java/com/…` layout is seen whole without one line per file);
//! - the **workspace-member manifests** are the `Cargo.toml`/`package.json`
//!   the listing found below the root — never a parse of the root manifest,
//!   which would need a TOML reader per ecosystem and would still miss the
//!   ecosystems that have no workspace concept;
//! - the **entry points** are the listing's files whose *name* is in
//!   [`ENTRY_POINTS`], at any depth;
//! - the `.github/workflows/*` **names** are the listing's, and they are names
//!   only: no read, and no identity (REQ-583 OQ-7 — a listed name is
//!   metadata, so it is not excluded and it does not pin the turn).
//!
//! The root-relative members of [`EVIDENCE_FILES`] are the one thing the
//! listing does *not* answer, and on purpose: AC-5 prices an absent member at
//! one `stat`, which is a fact about the seam the test asserts on. Reading
//! presence off the tree instead would cost zero and would quietly change what
//! "the table was exercised" means.
//!
//! # The order of the checks is the privacy order
//!
//! For every candidate the order is **resolve → match → `stat` → read**, and
//! the boundary match sits where it does for the reason BR-4 exists: a covered
//! file is not read, not `stat`ed, and never named in [`Evidence::provenance`]
//! — the recorded call log of a test double shows *nothing at all* for it, which
//! is a stronger claim than "its bytes did not reach the body". The identity a
//! boundary is matched on is minted by the seam that resolved the path
//! ([`ToolContext::resolve`] for a root-relative table member,
//! [`walk::visit`]'s own mint for a listing-found file), never by a second
//! parse here — LESSON-623, the same rule REQ-612's loader follows.
//!
//! # The cut is recorded, never silent
//!
//! The body is assembled in the requirement's priority order — tree,
//! manifests, README, entry points — and the **first** chunk that does not fit
//! under [`EvidenceBudget::max_bytes`] ends the assembly, which is exactly what
//! "drops entry points before README before manifests before the tree" means
//! when the classes are appended in that order. The tree is special-cased
//! because it is first and because it has a natural knob: it is re-rendered at
//! decreasing depth until it fits, and the depth it settled at is recorded on
//! [`Cut`]. Either way the fact lands on [`Evidence::cut`], the header line
//! (ADR-5) states it, and nothing is middle-elided in silence.
//!
//! Reads are **lazy** for the same reason: a class that the budget will drop is
//! never opened, so a repository with ten thousand `mod.rs` files pays for the
//! ones that fit and not for the rest.

use std::collections::BTreeMap;
use std::ops::ControlFlow;
use std::path::{Path, PathBuf};

use teton_core::boundary::BoundaryMatcher;
use teton_core::ProvenanceId;

use super::{FileStat, RepoFileReader};
use crate::harness::context::ToolProvenance;
use crate::harness::tools::walk::{self, WalkBudget};
use crate::harness::tools::ToolContext;
use crate::session_root::ProbedRoot;

/// How much of a present [`EVIDENCE_FILES`] member reaches the body: the
/// System Model's 16 KiB. A manifest or a README longer than this is read to
/// the ceiling and the chunk says so — the top of the file is the part an
/// author puts first (REQ-612 ADR-5's rule, at a different ceiling).
pub const EVIDENCE_FILE_CEILING_BYTES: u64 = 16 * 1024;

/// How much of a present [`ENTRY_POINTS`] member reaches the body: the System
/// Model's 4 KiB. Enough for the module doc and the import header, which is
/// where a source file says what it is, and not the file itself.
pub const ENTRY_POINT_CEILING_BYTES: u64 = 4 * 1024;

/// The **EvidenceFile** table (System Model): the closed set of root-relative
/// paths whose whole text — to [`EVIDENCE_FILE_CEILING_BYTES`] — is evidence,
/// each paired with the priority class it is assembled under.
///
/// Closed, `const`, and exercised by name in
/// `the_two_tables_are_read_by_name_and_nothing_else_is_opened`
/// — REQ-584 BR-4's pattern. "Nothing outside the tables is opened" is a claim
/// about *this array*, so a member added here without a test naming it is the
/// one drift the suite would not otherwise see.
///
/// The class is on the row rather than in a second `fn class_of(name)` so the
/// two cannot disagree: a member with no class is a compile error, not a member
/// that silently assembles last.
///
/// Two members of the System Model's documents row are **not** here, each for
/// its own reason:
///
/// - the workspace members' `Cargo.toml`/`package.json` ([`WORKSPACE_MANIFESTS`])
///   live at no fixed path and are found from the listing;
/// - `.github/workflows/*` contributes its **names** ([`WORKFLOWS_DIR`]), not
///   its bytes, and names come from the listing too.
pub const EVIDENCE_FILES: &[(&str, EvidenceClass)] = &[
    // The README, in the three spellings the System Model names. All three are
    // `stat`ed: a repository with both `README` and `README.md` has two files
    // worth reading, and the contest REQ-612 runs between `TETON.md` and
    // `AGENTS.md` is about *which notes are authoritative*, which is not the
    // question here.
    ("README.md", EvidenceClass::Readme),
    ("README", EvidenceClass::Readme),
    ("README.txt", EvidenceClass::Readme),
    ("CONTRIBUTING.md", EvidenceClass::Readme),
    ("ARCHITECTURE.md", EvidenceClass::Readme),
    // The build manifests, one per ecosystem.
    ("Cargo.toml", EvidenceClass::Manifests),
    ("package.json", EvidenceClass::Manifests),
    ("pyproject.toml", EvidenceClass::Manifests),
    ("setup.py", EvidenceClass::Manifests),
    ("go.mod", EvidenceClass::Manifests),
    ("Makefile", EvidenceClass::Manifests),
    ("justfile", EvidenceClass::Manifests),
    ("CMakeLists.txt", EvidenceClass::Manifests),
    ("build.gradle", EvidenceClass::Manifests),
    ("pom.xml", EvidenceClass::Manifests),
    ("Gemfile", EvidenceClass::Manifests),
    ("composer.json", EvidenceClass::Manifests),
    ("mix.exs", EvidenceClass::Manifests),
    ("Package.swift", EvidenceClass::Manifests),
    ("Dockerfile", EvidenceClass::Manifests),
    ("docker-compose.yml", EvidenceClass::Manifests),
    // The project's own written-down context, when it keeps any.
    (".adlc/context/project-overview.md", EvidenceClass::Readme),
    (".adlc/context/architecture.md", EvidenceClass::Readme),
];

/// The manifest names read at **any** depth below the root, not just at it —
/// the System Model's "and every workspace member's".
///
/// Found from the listing rather than by parsing the root manifest's
/// `members = [..]`: one listing already in hand answers it for every
/// ecosystem, including the ones with no workspace concept at all.
pub const WORKSPACE_MANIFESTS: &[&str] = &["Cargo.toml", "package.json"];

/// The one directory whose **names** are evidence and whose bytes are not
/// (System Model: "the names of `.github/workflows/*`").
///
/// A CI workflow's file names say which platforms and which jobs a repository
/// runs; its YAML says how, at a length that would crowd out a README. Names
/// are metadata (REQ-583 OQ-7), so this contributes no [`ProvenanceId`] and is
/// never excluded by a boundary.
pub const WORKFLOWS_DIR: &str = ".github/workflows";

/// The **EntryPoint** table (System Model): the closed set of *file names* —
/// matched at any depth from the listing — whose first
/// [`ENTRY_POINT_CEILING_BYTES`] are evidence.
///
/// Names, not paths, and that is the whole difference from [`EVIDENCE_FILES`]:
/// a repository's entry points sit wherever its ecosystem puts them, and the
/// listing already knows where that is.
pub const ENTRY_POINTS: &[&str] = &[
    "lib.rs",
    "main.rs",
    "mod.rs",
    "index.ts",
    "index.js",
    "index.tsx",
    "main.ts",
    "main.js",
    "__init__.py",
    "main.py",
    "app.py",
    "main.go",
    "App.swift",
    "Main.java",
    "Program.cs",
];

/// The bucket a file with no extension is counted under in the tree's
/// per-directory profile (`Makefile`, `justfile`, `LICENSE`).
const NO_EXTENSION: &str = "(none)";

/// Why a walk stopped short of the whole tree.
///
/// A type **alias** for the walker's own [`walk::TruncatedBy`] rather than a
/// second enum: ADR-3 calls the field `stop`, and a translation layer between
/// two spellings of "the entry budget ran out" is exactly the kind of second
/// vocabulary REQ-583 collapsed when it made one module own the walk.
pub type WalkStop = walk::TruncatedBy;

/// The four priority classes the body is assembled in, **in priority order** —
/// the derived [`Ord`] is that order, so the enum's declaration is the
/// requirement's sentence.
///
/// [`Self::Readme`] is the prose class, named for its head member: the README
/// is what a repository writes about itself, and `CONTRIBUTING.md`,
/// `ARCHITECTURE.md` and the `.adlc/context` documents are the same kind of
/// evidence at the same priority. The requirement names four classes and this
/// is four.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum EvidenceClass {
    /// The rendered listing. First, because the shape is what a description
    /// most needs (BR-3).
    Tree,
    /// The build manifests, root and workspace-member alike, plus the CI
    /// workflow names.
    Manifests,
    /// The prose a repository already wrote about itself.
    Readme,
    /// The headers of the [`ENTRY_POINTS`] members.
    EntryPoints,
}

impl EvidenceClass {
    /// The classes in assembly order — the same order as the enum, restated as
    /// a value so the assembler iterates rather than repeating itself.
    pub const PRIORITY: [Self; 4] = [Self::Tree, Self::Manifests, Self::Readme, Self::EntryPoints];

    /// The class's name in a sentence a person reads (the header line, ADR-5).
    #[must_use]
    pub fn label(self) -> &'static str {
        match self {
            Self::Tree => "tree",
            Self::Manifests => "manifests",
            Self::Readme => "README",
            Self::EntryPoints => "entry points",
        }
    }

    /// The class's heading in the assembled body.
    #[must_use]
    pub fn heading(self) -> &'static str {
        match self {
            Self::Tree => "## Tree",
            Self::Manifests => "## Manifests",
            Self::Readme => "## README",
            Self::EntryPoints => "## Entry points",
        }
    }
}

/// Where the assembly stopped, and — for [`EvidenceClass::Tree`] — how deep it
/// got (BR-3: a cut is stated, never swallowed).
///
/// `depth` is `Some` only for a tree cut, which is the one class with a knob
/// finer than "in or out": the listing is re-rendered at decreasing depth until
/// it fits. Every other class is dropped whole, and its `depth` is `None`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Cut {
    /// The class the budget ran out in. Everything below it in
    /// [`EvidenceClass::PRIORITY`] was dropped with it.
    pub class: EvidenceClass,
    /// For a tree cut, the deepest directory depth still listed.
    pub depth: Option<usize>,
}

/// The byte half of the System Model's `EvidenceBudget` — the draft route's
/// context budget less the drafting prompt and the answer reservation, computed
/// by the caller (ADR-6) because only the caller knows the route.
///
/// The entry and wall-clock halves are not fields here: they are
/// [`WalkBudget`]'s, which is REQ-583's one budget for every walker, and giving
/// this struct a second copy of them would be a second place a walk's bound
/// lives. [`gather_with_walk_budget`] is where a test shrinks them.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct EvidenceBudget {
    /// The assembled body's ceiling, in bytes.
    pub max_bytes: usize,
}

impl EvidenceBudget {
    /// A budget of `max_bytes`.
    #[must_use]
    pub const fn new(max_bytes: usize) -> Self {
        Self { max_bytes }
    }
}

/// One directory of the listing: how deep it sits, and how many files of each
/// extension it holds directly.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
struct Dir {
    /// Root-relative depth. The root itself is `0`.
    depth: usize,
    /// Files held directly, counted by extension bucket.
    exts: BTreeMap<String, usize>,
    /// Files held directly, in total — the sum of `exts`, kept beside it so the
    /// rendered line does not re-derive it.
    files: usize,
}

/// The root's hierarchy as one walk saw it: every directory, every file, and
/// the depth of each.
///
/// Built by [`Self::from_listing`], which **sorts before it builds** — the
/// LESSON-540 rule. A directory listing arrives in whatever order the
/// filesystem hashed it into (APFS does; ext4 and tmpfs do not), so a tree that
/// rendered in arrival order would render differently on the two CI legs for
/// one repository. Every collection in here is ordered by construction: the
/// directories are a [`BTreeMap`], the files are sorted, and the per-directory
/// profile is a [`BTreeMap`] re-sorted by count at render time.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Tree {
    /// Root-relative directory path (the empty string is the root) → what it
    /// holds.
    dirs: BTreeMap<String, Dir>,
    /// Every file the walk handed over, root-relative and sorted.
    files: Vec<String>,
    /// Entries the walk handed over, files and directories alike.
    entries: usize,
}

impl Tree {
    /// Build the tree from one walk's listing of `(root-relative id, is_dir)`.
    ///
    /// Depth is the root-relative path's component count, per ADR-3's technical
    /// note: `crates` is 1, `crates/tetond/src/main.rs` is 4. The root is
    /// inserted explicitly because the walk never hands *itself* over — it
    /// starts by listing the root's children.
    ///
    /// A directory the policy pruned (`.git`, `target`, `node_modules`) is
    /// still in the listing and still gets a line: the walker hands every entry
    /// over *before* it decides whether to descend, and a name that is present
    /// is a fact about the repository. Nothing under it is, which is the skip
    /// set holding.
    #[must_use]
    pub fn from_listing(listing: Vec<(String, bool)>) -> Self {
        let mut listing = listing;
        // Before anything is counted or bucketed: two listings of one tree in
        // two orders must build one `Tree` (LESSON-540).
        listing.sort();
        let entries = listing.len();
        let mut dirs: BTreeMap<String, Dir> = BTreeMap::new();
        dirs.insert(String::new(), Dir::default());
        let mut files = Vec::new();
        for (id, is_dir) in listing {
            let depth = id.split('/').count();
            if is_dir {
                dirs.entry(id).or_insert(Dir {
                    depth,
                    ..Dir::default()
                });
                continue;
            }
            let parent = id.rsplit_once('/').map_or("", |(parent, _)| parent);
            let bucket = extension_bucket(&id);
            let dir = dirs.entry(parent.to_owned()).or_insert(Dir {
                depth: depth.saturating_sub(1),
                ..Dir::default()
            });
            dir.files += 1;
            *dir.exts.entry(bucket).or_insert(0) += 1;
            files.push(id);
        }
        Self {
            dirs,
            files,
            entries,
        }
    }

    /// Every file in the listing, root-relative and sorted.
    #[must_use]
    pub fn files(&self) -> &[String] {
        &self.files
    }

    /// Entries the walk handed over, files and directories alike — the figure
    /// the `/verbose` drafting line prints (BR-5).
    #[must_use]
    pub fn entries(&self) -> usize {
        self.entries
    }

    /// The deepest directory in the listing. `0` for a flat repository, which
    /// is also the floor a depth cut can reach.
    #[must_use]
    pub fn depth(&self) -> usize {
        self.dirs.values().map(|dir| dir.depth).max().unwrap_or(0)
    }

    /// Render breadth-first — every directory at depth 0, then every directory
    /// at depth 1, and so on — one line each, with the per-extension profile of
    /// the files it holds directly.
    ///
    /// `max_depth` omits directories below it, which is the tree's half of the
    /// priority cut. Breadth-first is what makes that cut meaningful: the lines
    /// a shallow render keeps are the ones that say what kind of repository
    /// this is.
    ///
    /// The profile is ordered by count descending and then by extension, so the
    /// language a repository is mostly written in leads the line. Both keys are
    /// total orders, so the render is a function of the tree alone.
    #[must_use]
    pub fn render(&self, max_depth: Option<usize>) -> String {
        let mut rows: Vec<(usize, &String, &Dir)> = self
            .dirs
            .iter()
            .filter(|(_, dir)| max_depth.is_none_or(|max| dir.depth <= max))
            .map(|(path, dir)| (dir.depth, path, dir))
            .collect();
        // `BTreeMap` already delivered these in path order; a stable sort by
        // depth turns that into breadth-first without losing it.
        rows.sort_by(|a, b| a.0.cmp(&b.0).then_with(|| a.1.cmp(b.1)));
        let mut out = String::new();
        for (_, path, dir) in rows {
            if !out.is_empty() {
                out.push('\n');
            }
            out.push_str(&render_dir_line(path, dir));
        }
        out
    }
}

/// One directory's line: its path, how many files it holds, and of what.
fn render_dir_line(path: &str, dir: &Dir) -> String {
    let name = if path.is_empty() {
        ".".to_owned()
    } else {
        format!("{path}/")
    };
    let noun = if dir.files == 1 { "file" } else { "files" };
    let mut line = format!("{name} — {} {noun}", dir.files);
    if dir.files > 0 {
        // Commonest extension first — the language profile leads with the
        // language. Ties break on the extension itself, with the
        // no-extension bucket last: `.rs 1, .toml 1, (none) 1` reads as a
        // profile, `(none) 1, .rs 1, .toml 1` reads as an accident of ASCII.
        let mut profile: Vec<(&String, &usize)> = dir.exts.iter().collect();
        profile.sort_by(|a, b| {
            b.1.cmp(a.1)
                .then_with(|| (a.0 == NO_EXTENSION).cmp(&(b.0 == NO_EXTENSION)))
                .then_with(|| a.0.cmp(b.0))
        });
        let profile: Vec<String> = profile
            .into_iter()
            .map(|(ext, count)| format!("{ext} {count}"))
            .collect();
        line.push_str(&format!(" ({})", profile.join(", ")));
    }
    line
}

/// The extension bucket a root-relative file id counts under: `.rs`, `.md`, or
/// [`NO_EXTENSION`].
///
/// Taken off the file *name*, so a dotted directory (`.adlc/context/x.md`)
/// contributes `.md` and a dotfile with no extension (`.gitignore`) contributes
/// the no-extension bucket rather than `.gitignore`.
fn extension_bucket(id: &str) -> String {
    let name = id.rsplit_once('/').map_or(id, |(_, name)| name);
    Path::new(name).extension().map_or_else(
        || NO_EXTENSION.to_owned(),
        |ext| format!(".{}", ext.to_string_lossy()),
    )
}

/// Everything the gatherer produced, and everything the surfaces and the header
/// line need to say about how it went.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Evidence {
    /// The assembled prompt body, at most [`EvidenceBudget::max_bytes`].
    pub body: String,
    /// The identities whose bytes are in [`Self::body`] — never
    /// [`ToolProvenance::Unknown`] (BR-4). Listing names contribute nothing
    /// here: the tree, and the `.github/workflows` names, are metadata.
    pub provenance: ToolProvenance,
    /// How many candidate files a configured boundary covered, and which were
    /// therefore neither `stat`ed nor read (BR-4).
    pub excluded: usize,
    /// Entries the walk handed over, files and directories alike.
    pub entries: usize,
    /// `Some` when the walk's own budget stopped it before the tree was
    /// exhausted (BR-3, AC-4).
    pub stop: Option<WalkStop>,
    /// `Some` when the byte budget ended the assembly early (BR-3, AC-4).
    pub cut: Option<Cut>,
}

impl Evidence {
    /// The empty answer: no body, no provenance, nothing walked.
    ///
    /// Returned when the session root cannot be canonicalized — the one
    /// pre-walk failure this module can meet, and the same condition
    /// `glob`/`grep` answer with their root-missing refusal. There is no
    /// evidence to gather from a root that is not there, and the pipeline
    /// (ADR-6) turns an empty body into a `failed` outcome rather than drafting
    /// from nothing.
    #[must_use]
    pub fn empty() -> Self {
        Self {
            body: String::new(),
            provenance: ToolProvenance::none(),
            excluded: 0,
            entries: 0,
            stop: None,
            cut: None,
        }
    }
}

/// Gather the evidence for `root` under the tool walk's own budget (ADR-3).
///
/// The four arguments are the four seams: the probed root (one probe, so the
/// jail and the view cannot disagree), REQ-612's file reader, the compiled
/// boundary set, and the byte budget the caller derived from the draft route.
///
/// Runs **only after consent** — that is the caller's rule (ADR-1, ADR-2), and
/// it is what makes REQ-584 BR-3 hold by construction: this function is the only
/// walker on the generation path, and nothing calls it at launch.
#[must_use]
pub fn gather(
    root: &ProbedRoot,
    reader: &dyn RepoFileReader,
    matcher: &BoundaryMatcher<'_>,
    budget: EvidenceBudget,
) -> Evidence {
    gather_with_walk_budget(root, reader, matcher, budget, WalkBudget::default())
}

/// [`gather`] under a caller-supplied [`WalkBudget`] — the test seam AC-4's
/// budget-stop leg is exercised through.
///
/// The same seam and the same reason as
/// [`ToolContext::with_walk_budget`](crate::harness::tools::ToolContext::with_walk_budget),
/// which is what this composes: a walk that stops has to be provable without
/// planting a hundred thousand files, and the alternative — putting
/// `max_entries` on [`EvidenceBudget`] — would give a walk's bound a second
/// home outside REQ-583's one owner.
#[must_use]
pub fn gather_with_walk_budget(
    root: &ProbedRoot,
    reader: &dyn RepoFileReader,
    matcher: &BoundaryMatcher<'_>,
    budget: EvidenceBudget,
    walk_budget: WalkBudget,
) -> Evidence {
    let jail = ToolContext::for_root(root).with_walk_budget(walk_budget);
    // The walker's contract: the root it is handed is already canonical, and
    // every id it mints is relative to that spelling. The reads below resolve
    // through the same jail, so the two agree on how this directory is spelled.
    let Ok(canonical) = jail.repo_root().canonicalize() else {
        return Evidence::empty();
    };

    let mut listing: Vec<(String, bool)> = Vec::new();
    // The walker's own path and the walker's own mint, kept together and
    // carried to the read: re-minting the id here would be the second parse
    // LESSON-623 is about.
    let mut found: BTreeMap<String, (PathBuf, ProvenanceId)> = BTreeMap::new();
    let report = walk::visit(
        &canonical,
        jail.root_kind(),
        // No named prefix: this walk is not answering a pattern, so BR-12's
        // "unless named" escape has nothing to open.
        &[],
        jail.walk_policy(),
        &mut |path, file_type, id| {
            let is_dir = file_type.is_dir();
            if !is_dir {
                found.insert(id.as_str().to_owned(), (path.to_path_buf(), id.clone()));
            }
            listing.push((id.as_str().to_owned(), is_dir));
            // Never a tool stop: the tree is the evidence, so there is no "all
            // I can use" short of the budget's own end (BR-3).
            ControlFlow::Continue(())
        },
    );

    let tree = Tree::from_listing(listing);
    // `EvidenceClass::PRIORITY`'s order, one call per class: the tree is the
    // constructor's (it is the one class with a depth knob), then manifests,
    // then README, then entry points.
    let mut assembly = Assembly::new(budget, &tree, report.truncated_by);
    assembly.add_manifests(&jail, &tree, &found, reader, matcher);
    assembly.add_class(EvidenceClass::Readme, &jail, reader, matcher);
    assembly.add_entry_points(&tree, &found, reader, matcher);

    Evidence {
        body: assembly.body,
        provenance: ToolProvenance::paths(assembly.sources),
        excluded: assembly.excluded,
        entries: tree.entries(),
        stop: report.truncated_by,
        cut: assembly.cut,
    }
}

/// The body under construction: what has been appended, what pinned it, and
/// where the budget ran out.
struct Assembly {
    budget: EvidenceBudget,
    body: String,
    sources: Vec<ProvenanceId>,
    excluded: usize,
    cut: Option<Cut>,
    /// The class whose heading is already in the body, so a second document of
    /// the same class does not repeat it.
    heading_written: Option<EvidenceClass>,
}

impl Assembly {
    /// Start the body with the tree, cut to depth if the whole of it will not
    /// fit (BR-3: a repository whose tree alone exceeds the budget is drafted
    /// from a tree cut at a stated depth, not refused).
    fn new(budget: EvidenceBudget, tree: &Tree, stop: Option<WalkStop>) -> Self {
        let mut assembly = Self {
            budget,
            body: String::new(),
            sources: Vec::new(),
            excluded: 0,
            cut: None,
            heading_written: None,
        };
        let full = tree.depth();
        for depth in (0..=full).rev() {
            let limit = (depth < full).then_some(depth);
            let section = tree_section(tree, stop, limit);
            if section.len() <= budget.max_bytes {
                assembly.body = section;
                assembly.heading_written = Some(EvidenceClass::Tree);
                if limit.is_some() {
                    assembly.cut = Some(Cut {
                        class: EvidenceClass::Tree,
                        depth: limit,
                    });
                }
                return assembly;
            }
        }
        // Not even the root's own line fits. The body stays empty and the cut
        // says where it stopped, which is what the header line needs to print.
        assembly.cut = Some(Cut {
            class: EvidenceClass::Tree,
            depth: Some(0),
        });
        assembly
    }

    /// Whether the assembly has already ended — the first chunk that did not
    /// fit dropped its class and every class below it, so nothing after it is
    /// read (BR-3's priority order, and the reason the reads are lazy).
    fn stopped(&self) -> bool {
        self.cut.is_some()
    }

    /// Append `chunk` if it fits; otherwise record the cut in `class` and end
    /// the assembly. Answers whether it was appended.
    fn push(&mut self, class: EvidenceClass, chunk: &str) -> bool {
        let heading = if self.heading_written == Some(class) {
            String::new()
        } else {
            format!("{}\n", class.heading())
        };
        let separator = if self.body.is_empty() { "" } else { "\n" };
        let addition = separator.len() + heading.len() + chunk.len();
        if self.body.len() + addition > self.budget.max_bytes {
            self.cut = Some(Cut { class, depth: None });
            return false;
        }
        self.body.push_str(separator);
        self.body.push_str(&heading);
        self.body.push_str(chunk);
        self.heading_written = Some(class);
        true
    }

    /// The manifests class: the root-relative table members first, then the
    /// workspace members the listing found, then the CI workflow names.
    fn add_manifests(
        &mut self,
        jail: &ToolContext,
        tree: &Tree,
        found: &BTreeMap<String, (PathBuf, ProvenanceId)>,
        reader: &dyn RepoFileReader,
        matcher: &BoundaryMatcher<'_>,
    ) {
        self.add_class(EvidenceClass::Manifests, jail, reader, matcher);
        for id in tree.files() {
            if self.stopped() {
                return;
            }
            let Some((parent, name)) = id.rsplit_once('/') else {
                // A root-level manifest is the table's, and it has been read
                // (or refused) above; reading it again here would double both
                // the calls and the bytes.
                continue;
            };
            if parent.is_empty() || !WORKSPACE_MANIFESTS.contains(&name) {
                continue;
            }
            self.add_listing_file(
                EvidenceClass::Manifests,
                id,
                found,
                reader,
                matcher,
                EVIDENCE_FILE_CEILING_BYTES,
            );
        }
        self.add_workflow_names(tree);
    }

    /// The `.github/workflows/*` names — the one evidence member that
    /// contributes a listing and not a file (REQ-583 OQ-7): no read, no
    /// `stat`, no [`ProvenanceId`], and no boundary exclusion, because a name
    /// is metadata.
    fn add_workflow_names(&mut self, tree: &Tree) {
        if self.stopped() {
            return;
        }
        let prefix = format!("{WORKFLOWS_DIR}/");
        let names: Vec<&str> = tree
            .files()
            .iter()
            .filter_map(|id| id.strip_prefix(&prefix))
            .filter(|rest| !rest.contains('/'))
            .collect();
        if names.is_empty() {
            return;
        }
        self.push(
            EvidenceClass::Manifests,
            &format!("### {WORKFLOWS_DIR}\n{}\n", names.join(", ")),
        );
    }

    /// The entry points: every listing file whose *name* is in
    /// [`ENTRY_POINTS`], at any depth, read to [`ENTRY_POINT_CEILING_BYTES`].
    fn add_entry_points(
        &mut self,
        tree: &Tree,
        found: &BTreeMap<String, (PathBuf, ProvenanceId)>,
        reader: &dyn RepoFileReader,
        matcher: &BoundaryMatcher<'_>,
    ) {
        for id in tree.files() {
            if self.stopped() {
                return;
            }
            let name = id.rsplit_once('/').map_or(id.as_str(), |(_, name)| name);
            if !ENTRY_POINTS.contains(&name) {
                continue;
            }
            self.add_listing_file(
                EvidenceClass::EntryPoints,
                id,
                found,
                reader,
                matcher,
                ENTRY_POINT_CEILING_BYTES,
            );
        }
    }

    /// Every [`EVIDENCE_FILES`] member of `class`, in table order.
    fn add_class(
        &mut self,
        class: EvidenceClass,
        jail: &ToolContext,
        reader: &dyn RepoFileReader,
        matcher: &BoundaryMatcher<'_>,
    ) {
        for (name, member_class) in EVIDENCE_FILES {
            if self.stopped() {
                return;
            }
            if *member_class != class {
                continue;
            }
            // Resolved and matched **before** the `stat`: a covered file costs
            // the seam nothing at all (BR-4, and see the module docs). The mint
            // is the jail's, never a second parse of the same path
            // (LESSON-623).
            let Ok(resolved) = jail.resolve(name) else {
                continue;
            };
            if matcher.match_path(resolved.provenance.as_str()).is_some() {
                self.excluded += 1;
                continue;
            }
            let Some(stat) = readable(reader.stat(&resolved.path).ok()) else {
                // An absent member costs exactly this one `stat` (AC-5).
                continue;
            };
            let Ok(text) =
                reader.read(&resolved.path, EVIDENCE_FILE_CEILING_BYTES, stat.identity())
            else {
                continue;
            };
            let chunk = document_chunk(name, &text, stat.len, EVIDENCE_FILE_CEILING_BYTES);
            if self.push(class, &chunk) {
                self.sources.push(resolved.provenance);
            }
        }
    }

    /// One file the listing found, whose identity the **walker** minted.
    ///
    /// The path and the id both come from `walk::visit`, which resolved the
    /// entry and minted from that resolution — so this is the same LESSON-623
    /// rule as [`Self::add_class`]'s, entered from the other seam.
    fn add_listing_file(
        &mut self,
        class: EvidenceClass,
        id: &str,
        found: &BTreeMap<String, (PathBuf, ProvenanceId)>,
        reader: &dyn RepoFileReader,
        matcher: &BoundaryMatcher<'_>,
        ceiling: u64,
    ) {
        let Some((path, provenance)) = found.get(id) else {
            return;
        };
        if matcher.match_path(provenance.as_str()).is_some() {
            self.excluded += 1;
            return;
        }
        let Some(stat) = readable(reader.stat(path).ok()) else {
            return;
        };
        let Ok(text) = reader.read(path, ceiling, stat.identity()) else {
            return;
        };
        let chunk = document_chunk(id, &text, stat.len, ceiling);
        if self.push(class, &chunk) {
            self.sources.push(provenance.clone());
        }
    }
}

/// The entry rule, in the one form this module needs it: a candidate is read
/// only when the `stat` says regular file and not a symlink.
///
/// Weaker than REQ-612's loader by one check — a hardlinked `Cargo.toml` is
/// read here and a hardlinked `TETON.md` is refused there — and deliberately:
/// the loader's refusal is about *authorship* of the notes Teton obeys, while
/// this is ordinary repository content that a build system may well have
/// hardlinked.
fn readable(stat: Option<FileStat>) -> Option<FileStat> {
    stat.filter(|stat| stat.is_regular && !stat.is_symlink)
}

/// One document's chunk: its root-relative path, its text, and — when the file
/// was longer than the ceiling — a line saying so.
///
/// No fence around the text. A `README.md` holds fenced code blocks of its own,
/// and a fence that a document can close from the inside is worse than no fence
/// at all; the `###` heading is the delimiter, and it is one the model reads
/// the same way in every section.
fn document_chunk(path: &str, text: &str, on_disk: u64, ceiling: u64) -> String {
    let mut chunk = format!("### {path}\n{text}");
    if !chunk.ends_with('\n') {
        chunk.push('\n');
    }
    if on_disk > ceiling {
        chunk.push_str(&format!("(truncated at {ceiling} bytes)\n"));
    }
    chunk
}

/// The tree section: the heading, whatever the walk has to say about itself,
/// and the rendered listing.
///
/// The two notices are inside the measured section on purpose — a cut or a stop
/// that the budget then dropped would be a fact stated nowhere, and BR-3 says
/// neither is ever swallowed.
fn tree_section(tree: &Tree, stop: Option<WalkStop>, max_depth: Option<usize>) -> String {
    let mut section = format!("{}\n", EvidenceClass::Tree.heading());
    match stop {
        Some(WalkStop::Entries(entries)) => {
            section.push_str(&format!(
                "(walk stopped after {entries} entries; the listing is partial)\n"
            ));
        }
        Some(WalkStop::WallClock(elapsed)) => {
            section.push_str(&format!(
                "(walk stopped after {:.1} s; the listing is partial)\n",
                elapsed.as_secs_f64()
            ));
        }
        None => {}
    }
    if let Some(depth) = max_depth {
        section.push_str(&format!(
            "(tree cut at depth {depth}; deeper directories are not listed)\n"
        ));
    }
    section.push_str(&tree.render(max_depth));
    section.push('\n');
    section
}

#[cfg(test)]
mod tests {
    use std::sync::Mutex;

    use teton_core::entities::{BoundaryMode, PrivacyBoundary};
    use teton_protocol::methods::{RootKind, SessionRoot};

    use super::*;
    use crate::repo_context::{FileIdentity, RealFiles, RepoFileError};

    /// A [`RepoFileReader`] that answers from the real filesystem and records
    /// every path it was asked about.
    ///
    /// Wrapping [`RealFiles`] rather than serving a second, in-memory copy of
    /// the tree is the point: the walk under test is a **real** walk over a
    /// planted directory, so a double with its own idea of what exists could
    /// disagree with what the walk found and the disagreement would look like a
    /// passing test. Everything the double adds is the recording — and the
    /// recording is compared for **equality**, the `DirLister` rule REQ-612
    /// states: a reach claim ("nothing outside the tables was opened") cannot be
    /// passed by a gatherer that also went somewhere else.
    struct Recorded {
        root: PathBuf,
        calls: Mutex<Vec<String>>,
    }

    impl Recorded {
        fn new(root: &Path) -> Self {
            Self {
                root: root.to_path_buf(),
                calls: Mutex::new(Vec::new()),
            }
        }

        /// The recorded calls, root-relative, in order.
        fn calls(&self) -> Vec<String> {
            self.calls.lock().unwrap().clone()
        }

        fn record(&self, verb: &str, path: &Path) {
            let shown = path.strip_prefix(&self.root).map_or_else(
                |_| path.display().to_string(),
                |rel| rel.display().to_string(),
            );
            self.calls.lock().unwrap().push(format!("{verb} {shown}"));
        }
    }

    impl RepoFileReader for Recorded {
        fn stat(&self, path: &Path) -> Result<FileStat, RepoFileError> {
            self.record("stat", path);
            RealFiles.stat(path)
        }

        fn read(
            &self,
            path: &Path,
            ceiling: u64,
            expected: FileIdentity,
        ) -> Result<String, RepoFileError> {
            self.record("read", path);
            RealFiles.read(path, ceiling, expected)
        }
    }

    /// A real, canonical project root to walk, and the probed root naming it.
    ///
    /// Real because the walker is the production walker and lists the real
    /// filesystem; canonical because the walk's ids and the jail's resolutions
    /// must be the same spelling for the recorded calls to read as relative
    /// paths.
    fn fixture(tag: &str, files: &[(&str, &str)]) -> (PathBuf, ProbedRoot) {
        use std::sync::atomic::{AtomicU64, Ordering};
        static NEXT: AtomicU64 = AtomicU64::new(0);
        let dir = std::env::temp_dir().join(format!(
            "teton-evidence-{tag}-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let path = std::fs::canonicalize(&dir).unwrap();
        for (rel, text) in files {
            let file = path.join(rel);
            std::fs::create_dir_all(file.parent().unwrap()).unwrap();
            std::fs::write(&file, text).unwrap();
        }
        let probed = ProbedRoot {
            path: path.clone(),
            view: SessionRoot {
                display: "~/repo".to_owned(),
                kind: RootKind::Project,
                project_name: Some("repo".to_owned()),
                vcs_branch: None,
            },
        };
        (path, probed)
    }

    /// The workspace fixture the table and exclusion tests share: one file of
    /// each interesting kind, and two files (`docs/guide.md`,
    /// `crates/a/src/notes.txt`) that are in **neither** table and must
    /// therefore never be opened.
    const WORKSPACE: &[(&str, &str)] = &[
        ("README.md", "the readme\n"),
        ("Cargo.toml", "[workspace]\nmembers = [\"crates/a\"]\n"),
        ("Makefile", "build:\n\tcargo build\n"),
        (".github/workflows/ci.yml", "name: ci\n"),
        (".adlc/context/architecture.md", "the architecture\n"),
        ("crates/a/Cargo.toml", "[package]\nname = \"a\"\n"),
        ("crates/a/src/lib.rs", "//! crate a\n"),
        ("crates/a/src/notes.txt", "not in either table\n"),
        ("docs/guide.md", "not in either table\n"),
        (".git/objects/pack/x", "never descended into\n"),
        ("target/debug/y", "never descended into\n"),
    ];

    /// No boundaries at all — the ordinary session.
    fn no_boundaries() -> Vec<PrivacyBoundary> {
        Vec::new()
    }

    /// A budget nothing in these fixtures comes close to.
    fn roomy() -> EvidenceBudget {
        EvidenceBudget::new(1 << 20)
    }

    /// BR-3 / AC-4: the walk lists the whole hierarchy to its leaves with
    /// per-directory extension counts, the skip set holds, a symlinked entry is
    /// not followed, the reader is untouched until `gather` is called, and a
    /// walk over a small entry budget stops with the stop recorded.
    ///
    /// Mutations, each run: `stop: report.truncated_by` → `stop: None` in
    /// `gather_with_walk_budget` fails the budget-stop leg; capping
    /// `Tree::render`'s filter at depth 2 whatever the caller asked fails the
    /// six-level leaf assertion (and the order-independence test with it).
    /// Raising the injected `max_entries` to the default makes the second leg's
    /// `stop` `None`; removing `.git` from `WALK_SKIP_DIRS` renders a
    /// `.git/objects/` line, and following the symlink makes `linked/` appear —
    /// both in `walk.rs`, so they are named rather than run here.
    #[test]
    fn the_full_tree_is_listed_to_its_leaves_and_a_budget_stop_is_recorded() {
        let mut planted: Vec<(&str, &str)> = WORKSPACE.to_vec();
        // Six levels of directory, so the leaf sits at depth 6 (AC-4).
        planted.push(("a/b/c/d/e/f/deep.rs", "//! the leaf\n"));
        let (dir, root) = fixture("full-tree", &planted);
        std::os::unix::fs::symlink(dir.join("docs"), dir.join("linked")).unwrap();
        let boundaries = no_boundaries();
        let matcher = BoundaryMatcher::new(&boundaries).unwrap();

        let files = Recorded::new(&dir);
        // AC-4's first clause: nothing has been asked of the filesystem seam.
        assert!(
            files.calls().is_empty(),
            "the reader was touched before gather"
        );

        let evidence = gather(&root, &files, &matcher, roomy());

        // The leaves, at every depth, with the counts.
        assert!(
            evidence.body.contains("a/b/c/d/e/f/ — 1 file (.rs 1)"),
            "the six-level tree did not reach its leaf:\n{}",
            evidence.body
        );
        assert!(
            evidence
                .body
                .contains("crates/a/src/ — 2 files (.rs 1, .txt 1)"),
            "the per-directory extension profile is wrong:\n{}",
            evidence.body
        );
        // The skip set: the names are listed (the walker hands an entry over
        // before it prunes) and nothing under them is.
        assert!(evidence.body.contains(".git/ — 0 files"));
        assert!(evidence.body.contains("target/ — 0 files"));
        assert!(
            !evidence.body.contains(".git/objects"),
            "the skip set did not hold:\n{}",
            evidence.body
        );
        assert!(
            !evidence.body.contains("target/debug"),
            "the skip set did not hold:\n{}",
            evidence.body
        );
        // The symlink rule (REQ-571 BR-5): the link is not followed, so it is
        // not an entry at all.
        assert!(
            !evidence.body.contains("linked/"),
            "a symlinked entry was followed:\n{}",
            evidence.body
        );
        assert_eq!(evidence.stop, None, "the default budget stopped nothing");
        assert!(evidence.entries > 10, "entries: {}", evidence.entries);

        // The same tree over a budget of three entries.
        let stopped = Recorded::new(&dir);
        let evidence = gather_with_walk_budget(
            &root,
            &stopped,
            &matcher,
            roomy(),
            WalkBudget {
                max_entries: 3,
                ..WalkBudget::default()
            },
        );
        assert_eq!(evidence.stop, Some(WalkStop::Entries(3)));
        assert_eq!(evidence.entries, 3);
        assert!(
            evidence
                .body
                .contains("(walk stopped after 3 entries; the listing is partial)"),
            "the stop was swallowed:\n{}",
            evidence.body
        );

        std::fs::remove_dir_all(&dir).ok();
    }

    /// BR-3 / AC-5: both tables are exercised by name, a present EvidenceFile
    /// member contributes its whole text to 16 KiB and an EntryPoint member its
    /// first 4 KiB, an absent member costs exactly one `stat`, and **nothing
    /// outside the tables is opened** — the reader's recorded path log is the
    /// assertion, compared for equality.
    ///
    /// Mutation: adding `docs/guide.md` or `crates/a/src/notes.txt` to either
    /// table adds two calls to the log and fails the equality; dropping the
    /// workspace-member sweep removes `crates/a/Cargo.toml`'s pair; raising
    /// either ceiling fails the two length assertions; reading presence off the
    /// tree instead of `stat`ing removes every absent member's `stat` and fails
    /// the equality.
    #[test]
    fn the_two_tables_are_read_by_name_and_nothing_else_is_opened() {
        // The tables, by name, exactly as the System Model lists them.
        let evidence_names: Vec<&str> = EVIDENCE_FILES.iter().map(|(name, _)| *name).collect();
        assert_eq!(
            evidence_names,
            vec![
                "README.md",
                "README",
                "README.txt",
                "CONTRIBUTING.md",
                "ARCHITECTURE.md",
                "Cargo.toml",
                "package.json",
                "pyproject.toml",
                "setup.py",
                "go.mod",
                "Makefile",
                "justfile",
                "CMakeLists.txt",
                "build.gradle",
                "pom.xml",
                "Gemfile",
                "composer.json",
                "mix.exs",
                "Package.swift",
                "Dockerfile",
                "docker-compose.yml",
                ".adlc/context/project-overview.md",
                ".adlc/context/architecture.md",
            ]
        );
        assert_eq!(
            ENTRY_POINTS,
            [
                "lib.rs",
                "main.rs",
                "mod.rs",
                "index.ts",
                "index.js",
                "index.tsx",
                "main.ts",
                "main.js",
                "__init__.py",
                "main.py",
                "app.py",
                "main.go",
                "App.swift",
                "Main.java",
                "Program.cs",
            ]
        );

        // A README over the 16 KiB ceiling and a `lib.rs` over the 4 KiB one,
        // so both ceilings are load-bearing rather than inert. The filler
        // characters appear nowhere else in the body — not in a path, not in a
        // heading, not in another fixture file — so counting them counts
        // exactly the bytes that crossed the ceiling.
        let long_readme = format!("{}\n", "Z".repeat(20_000));
        let long_lib = format!("{}\n", "Q".repeat(6_000));
        let mut planted: Vec<(&str, &str)> = WORKSPACE.to_vec();
        planted[0] = ("README.md", long_readme.as_str());
        planted[6] = ("crates/a/src/lib.rs", long_lib.as_str());
        let (dir, root) = fixture("two-tables", &planted);
        let boundaries = no_boundaries();
        let matcher = BoundaryMatcher::new(&boundaries).unwrap();

        let files = Recorded::new(&dir);
        let evidence = gather(&root, &files, &matcher, roomy());

        // The whole log, in order: the manifests class (table order), the
        // workspace member the listing found, the README class, the entry
        // point. `docs/guide.md` and `crates/a/src/notes.txt` are in neither
        // table and appear nowhere.
        assert_eq!(
            files.calls(),
            vec![
                "stat Cargo.toml",
                "read Cargo.toml",
                "stat package.json",
                "stat pyproject.toml",
                "stat setup.py",
                "stat go.mod",
                "stat Makefile",
                "read Makefile",
                "stat justfile",
                "stat CMakeLists.txt",
                "stat build.gradle",
                "stat pom.xml",
                "stat Gemfile",
                "stat composer.json",
                "stat mix.exs",
                "stat Package.swift",
                "stat Dockerfile",
                "stat docker-compose.yml",
                "stat crates/a/Cargo.toml",
                "read crates/a/Cargo.toml",
                "stat README.md",
                "read README.md",
                "stat README",
                "stat README.txt",
                "stat CONTRIBUTING.md",
                "stat ARCHITECTURE.md",
                "stat .adlc/context/project-overview.md",
                "stat .adlc/context/architecture.md",
                "read .adlc/context/architecture.md",
                "stat crates/a/src/lib.rs",
                "read crates/a/src/lib.rs",
            ]
        );

        // The ceilings, measured on the body: 16 KiB of the README's bytes and
        // 4 KiB of the entry point's, and not one byte more.
        assert_eq!(
            evidence.body.matches('Z').count(),
            usize::try_from(EVIDENCE_FILE_CEILING_BYTES).unwrap(),
            "the EvidenceFile ceiling is not 16 KiB"
        );
        assert_eq!(
            evidence.body.matches('Q').count(),
            usize::try_from(ENTRY_POINT_CEILING_BYTES).unwrap(),
            "the EntryPoint ceiling is not 4 KiB"
        );
        assert!(evidence.body.contains("(truncated at 16384 bytes)"));
        assert!(evidence.body.contains("(truncated at 4096 bytes)"));

        // The names-only member: listed, never read, and it pins nothing.
        assert!(evidence.body.contains("### .github/workflows\nci.yml"));

        // The provenance is every file whose bytes are in the body, and only
        // those (BR-4: never `Unknown`).
        let ToolProvenance::Sources(sources) = &evidence.provenance else {
            panic!("evidence provenance must be Sources, never Unknown");
        };
        let sources: Vec<&str> = sources.iter().map(ProvenanceId::as_str).collect();
        assert_eq!(
            sources,
            vec![
                ".adlc/context/architecture.md",
                "Cargo.toml",
                "Makefile",
                "README.md",
                "crates/a/Cargo.toml",
                "crates/a/src/lib.rs",
            ]
        );
        assert_eq!(evidence.excluded, 0);
        assert_eq!(evidence.cut, None);

        std::fs::remove_dir_all(&dir).ok();
    }

    /// BR-4: a covered `Cargo.toml` is absent from the body and from the
    /// provenance, `excluded == 1`, the seam was never asked about it at all —
    /// and the covered directory's own name still lists in the tree, because a
    /// listing name is metadata (REQ-583 OQ-7).
    ///
    /// Mutations, both run: deleting the `matcher.match_path` guard in
    /// `add_listing_file` makes `excluded` 0, puts `[package]` in the body,
    /// adds the identity to the provenance, and adds a `stat`/`read` pair to
    /// the log — four assertions red; deleting the same guard in `add_class`
    /// fails the second leg, which is why both seams are exercised here rather
    /// than one standing in for the other. Excluding the tree's listing names
    /// as well would make the `crates/a/` line vanish and fail the last one.
    #[test]
    fn a_covered_evidence_file_is_excluded_and_counted_and_its_directory_name_still_lists() {
        let (dir, root) = fixture("covered", WORKSPACE);
        let boundaries = vec![PrivacyBoundary::user(
            "crates/a/Cargo.toml",
            BoundaryMode::LocalOnly,
        )];
        let matcher = BoundaryMatcher::new(&boundaries).unwrap();

        let files = Recorded::new(&dir);
        let evidence = gather(&root, &files, &matcher, roomy());

        assert_eq!(evidence.excluded, 1);
        assert!(
            !evidence.body.contains("### crates/a/Cargo.toml"),
            "a covered file reached the body:\n{}",
            evidence.body
        );
        assert!(
            !evidence.body.contains("name = \"a\""),
            "a covered file's bytes reached the body:\n{}",
            evidence.body
        );
        let ToolProvenance::Sources(sources) = &evidence.provenance else {
            panic!("evidence provenance must be Sources, never Unknown");
        };
        assert!(
            !sources
                .iter()
                .any(|id| id.as_str() == "crates/a/Cargo.toml"),
            "a covered file was named in the provenance"
        );
        // The stronger claim: the covered file was not even `stat`ed.
        assert!(
            !files
                .calls()
                .iter()
                .any(|call| call.contains("crates/a/Cargo.toml")),
            "a covered file was touched on the seam: {:?}",
            files.calls()
        );
        // And the listing name is still there.
        assert!(
            evidence.body.contains("crates/a/ — "),
            "the covered file's directory name stopped listing:\n{}",
            evidence.body
        );

        // The same rule from the other seam: a covered root-relative table
        // member is excluded before its `stat` too.
        let root_boundaries = vec![PrivacyBoundary::user("Cargo.toml", BoundaryMode::LocalOnly)];
        let root_matcher = BoundaryMatcher::new(&root_boundaries).unwrap();
        let files = Recorded::new(&dir);
        let evidence = gather(&root, &files, &root_matcher, roomy());
        assert_eq!(evidence.excluded, 1);
        assert!(!files.calls().contains(&"stat Cargo.toml".to_owned()));
        assert!(!evidence.body.contains("[workspace]"));

        std::fs::remove_dir_all(&dir).ok();
    }

    /// The priority cut drops entry points before README before manifests
    /// before the tree, the tree is cut by depth with `cut.depth` recorded, and
    /// the body never exceeds `max_bytes`.
    ///
    /// Driven as a table over descending budgets so the *order* is the claim,
    /// not one budget's outcome.
    ///
    /// Mutation: reordering the three `add_*` calls in
    /// `gather_with_walk_budget` (adding the README class before the manifests)
    /// swaps two rows' expected classes and fails; dropping the
    /// `body.len() + addition > max_bytes` guard in `Assembly::push` fails
    /// every row's length assertion; rendering the tree at full depth
    /// regardless of fit fails the last row's `depth` assertion.
    #[test]
    fn the_priority_cut_drops_the_lowest_class_first_and_the_tree_is_cut_by_depth() {
        let mut planted: Vec<(&str, &str)> = WORKSPACE.to_vec();
        planted.push(("a/b/c/d/e/f/deep.rs", "//! the leaf\n"));
        let (dir, root) = fixture("priority", &planted);
        let boundaries = no_boundaries();
        let matcher = BoundaryMatcher::new(&boundaries).unwrap();

        let files = Recorded::new(&dir);
        let whole = gather(&root, &files, &matcher, roomy());
        assert_eq!(whole.cut, None, "the roomy budget cut nothing");

        // Where each class starts in the uncut body: a budget one byte short of
        // a class's first chunk is a budget that cuts exactly there.
        let mut seen: Vec<EvidenceClass> = Vec::new();
        for class in [
            EvidenceClass::EntryPoints,
            EvidenceClass::Readme,
            EvidenceClass::Manifests,
        ] {
            let at = whole
                .body
                .find(class.heading())
                .unwrap_or_else(|| panic!("{} is not in the uncut body", class.heading()));
            let files = Recorded::new(&dir);
            let evidence = gather(&root, &files, &matcher, EvidenceBudget::new(at));
            assert!(
                evidence.body.len() <= at,
                "the body overran the budget: {} > {at}",
                evidence.body.len()
            );
            let cut = evidence.cut.expect("a budget below a class heading cuts");
            assert_eq!(
                cut.class, class,
                "cut in the wrong class at budget {at}: {:?}",
                evidence.cut
            );
            assert_eq!(cut.depth, None, "only a tree cut carries a depth");
            assert!(
                !evidence.body.contains(class.heading()),
                "the cut class still reached the body:\n{}",
                evidence.body
            );
            seen.push(cut.class);
        }
        assert_eq!(
            seen,
            vec![
                EvidenceClass::EntryPoints,
                EvidenceClass::Readme,
                EvidenceClass::Manifests,
            ],
            "the classes did not drop in priority order"
        );

        // And below the whole tree: cut by depth, with the depth recorded and
        // stated in the body.
        let tree_at = whole.body.find("## Manifests").unwrap();
        let files = Recorded::new(&dir);
        let evidence = gather(&root, &files, &matcher, EvidenceBudget::new(tree_at / 2));
        let cut = evidence.cut.expect("half a tree does not fit");
        assert_eq!(cut.class, EvidenceClass::Tree);
        let depth = cut.depth.expect("a tree cut carries its depth");
        assert!(depth < 6, "the tree was not actually cut: depth {depth}");
        assert!(evidence.body.len() <= tree_at / 2);
        assert!(
            evidence
                .body
                .contains(&format!("(tree cut at depth {depth};")),
            "the tree cut was swallowed:\n{}",
            evidence.body
        );
        assert!(
            !evidence.body.contains("## Manifests"),
            "a class below the tree cut still reached the body"
        );

        std::fs::remove_dir_all(&dir).ok();
    }

    /// LESSON-540: two listings of the same entries in different orders build
    /// one tree and render identically — the fixture must not depend on the
    /// order a directory happened to hash into.
    ///
    /// Mutation: deleting the `listing.sort()` in `Tree::from_listing` leaves
    /// `files()` in arrival order and fails the second assertion of every
    /// permutation (the rendered lines survive because the directories are a
    /// `BTreeMap` — which is exactly why the file order has to be asserted too,
    /// and why the entry-point read order is deterministic).
    #[test]
    fn a_listing_renders_the_same_whatever_order_its_entries_arrive_in() {
        let listing: Vec<(String, bool)> = [
            ("crates", true),
            ("crates/a", true),
            ("crates/a/src", true),
            ("crates/a/src/lib.rs", false),
            ("crates/a/src/main.rs", false),
            ("crates/a/Cargo.toml", false),
            ("docs", true),
            ("docs/guide.md", false),
            ("Makefile", false),
            ("README.md", false),
        ]
        .into_iter()
        .map(|(id, is_dir)| (id.to_owned(), is_dir))
        .collect();

        let canonical = Tree::from_listing(listing.clone());
        for rotation in 0..listing.len() {
            let mut shuffled = listing.clone();
            shuffled.rotate_left(rotation);
            if rotation % 2 == 1 {
                shuffled.reverse();
            }
            let other = Tree::from_listing(shuffled);
            assert_eq!(
                canonical.render(None),
                other.render(None),
                "rotation {rotation} rendered differently"
            );
            assert_eq!(
                canonical.files(),
                other.files(),
                "rotation {rotation} ordered its files differently"
            );
            assert_eq!(
                canonical, other,
                "rotation {rotation} built a different tree"
            );
        }

        // And the render is the breadth-first shape the model reads.
        assert_eq!(
            canonical.render(None),
            ". — 2 files (.md 1, (none) 1)\n\
             crates/ — 0 files\n\
             docs/ — 1 file (.md 1)\n\
             crates/a/ — 1 file (.toml 1)\n\
             crates/a/src/ — 2 files (.rs 2)"
        );
        assert_eq!(canonical.depth(), 3);
        assert_eq!(canonical.entries(), 10);
    }
}
