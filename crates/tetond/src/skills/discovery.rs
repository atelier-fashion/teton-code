//! The four globs, one level deep, behind a seam that records what it opened
//! (REQ-585 BR-1, ADR-4).
//!
//! [`discover`] opens exactly four directories:
//!
//! ```text
//! <session-root>/.claude/skills     <session-root>/.claude/commands
//! <home>/.claude/skills             <home>/.claude/commands
//! ```
//!
//! …lists each **one level deep**, and reads one file per candidate. It
//! recurses into nothing, follows no entry, and consults no other path. That
//! claim is the point of the [`DirLister`] seam: a test's recording
//! implementation captures every path handed to [`DirLister::list`] and
//! [`DirLister::read`], and the suite compares the recorded set for *equality*
//! rather than containment — a reach test cannot be passed by a walker that
//! also went somewhere else.
//!
//! # Why this is not `walk::visit`
//!
//! [`crate::harness::tools::walk`] is a recursive driver, and BR-1 forbids
//! recursion. Reusing it would also inherit `WalkBudget`'s 100,000 entries and
//! 10 second wall clock, which would turn the symlink fixture (`skills/link →
//! /`) from a *reach* test into a *budget* test: `/` would be enumerated, the
//! walk would stop on a limit, and the property the fixture exists to prove —
//! that `/` is never opened at all — would silently stop being asserted.
//! REQ-583 shipped **policy** seams (`WalkPolicy`, `WalkBudget`); this is an
//! **observation** seam, and it is new on purpose.
//!
//! # A **user** root is followed. A **project** root is bounded. Entries are not
//! followed at all.
//!
//! The rule has three clauses and the first two used to be one, which is the
//! hole REQ-587's verify found.
//!
//! A **user** root (`~/.claude/skills`, `~/.claude/commands`) is opened with no
//! symlink check: the dogfood machine's `~/.claude/skills` *is* a symlink into a
//! checked-out toolkit, and a rule that refused it would refuse the feature's
//! own author. That reason is about the **home** directory, which the person at
//! the keyboard owns.
//!
//! A **project** root (`<session-root>/.claude/skills`, `…/commands`) is
//! repository content. Two of the four roots are built from `session_root`, so
//! under the old blanket exemption a cloned repo shipping
//! `.claude/commands -> ../../..` — git stores a symlink verbatim — had every
//! lowercase-stemmed `*.md` under the target registered as a **project** skill:
//! in the roster, in the resident prompt, and callable by the model, while a
//! `read` of the same bytes is refused by REQ-583's jail. That is precisely the
//! *second classifier of what may be read* BR-10 says must not exist. So a
//! project root is [`DirLister::canonicalize`]d before it is listed and refused
//! when it does not resolve **under the session root**, with the refusal named
//! ([`SkipReason::EscapingRoot`]) rather than silent. Nothing about the dogfood
//! reason above extends to a repository's directory.
//!
//! Every **entry** a listing returns is refused if its `file_type()` says
//! symlink, reusing [`crate::harness::tools::skip_symlink_entry`] so the
//! predicate keeps one home. `DirEntry::file_type` has `lstat` semantics — it
//! does **not** traverse — which is precisely why the check can be made without
//! opening anything.
//!
//! This is *narrower* than the walkers' blanket refusal, so it is pinned by its
//! own test rather than riding theirs.
//!
//! The one path that is followed below an entry is the leaf whose name the
//! entry fully determines: `<dir>/SKILL.md`. Following that link enumerates
//! nothing — the danger a symlink poses to a walk is unbounded *ground*, and a
//! fixed leaf name is not ground.
//!
//! # The path a row carries is the path as spelled, not as resolved
//!
//! A registered [`Skill`] keeps the path discovery walked to it, because that is
//! the path the user recognizes and the one every surface renders. It is
//! therefore **not** guaranteed canonical: a project root may still be a symlink
//! *within* the repository (`.claude/skills -> vendor/skills`), which the check
//! above permits. Minting a provenance id off that spelling is the "one file,
//! two identities" bug the leaf comment below names, so the mint resolves the
//! path first and fails closed when it cannot — see
//! [`provenance_of`], which is the one home for that rule.

use std::ffi::OsString;
use std::io::Read;
use std::path::{Path, PathBuf};

use teton_protocol::methods::RootKind;

use super::{
    assemble, dynamic, frontmatter, is_valid_skill_name, roots, RootSpec, Shape, Skill,
    SkillRegistry, SkillSource, SkipReason, Skipped, MAX_DYNAMIC_COMMANDS, MAX_ENTRIES_PER_ROOT,
    SKILL_MAX_BYTES,
};
use crate::harness::tools::skip_symlink_entry;

/// One entry in a directory listing: its name, and the type of the entry
/// **itself**.
#[derive(Debug, Clone)]
pub struct Entry {
    /// The entry's own file name, with no directory part.
    pub file_name: OsString,
    /// From `DirEntry::file_type` — `lstat` semantics, so a symlink reports as
    /// a symlink rather than as whatever it points at.
    pub file_type: std::fs::FileType,
}

/// Why a directory could not be listed.
///
/// Two variants, because discovery has to tell them apart: a missing directory
/// is the **normal** case for three of the four roots on most machines and
/// costs nothing, while a refused one is a named diagnostic.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ListError {
    /// No such directory. Not a fault.
    NotFound,
    /// The directory exists and could not be opened — `EPERM`, or macOS's TCC
    /// consent gate in front of `~/Documents` and `~/Desktop`.
    Denied,
}

/// Why a file could not be read into a skill.
///
/// # Why this is typed, when `fs_util::read_regular_file_bounded` is not
///
/// [`crate::fs_util::read_regular_file_bounded`] answers `None` for every
/// failure, which is right for its callers (a `.git` file that is missing and
/// one that is a FIFO are equally "no answer"). BR-1 needs the opposite: a
/// `EPERM`, an oversize file and a non-UTF-8 one are three different sentences
/// shown to the user. [`RealFs::read`] therefore keeps its own read — in the
/// **same order** as `fs_util`'s and for the same reason (type and size checked
/// off `metadata` *before* the open, so a FIFO named `SKILL.md` is never opened
/// and cannot block for a writer; the read `take`n as well as size-checked, so
/// a file that grows under the read stays bounded) — and differs only in
/// returning which failure it was.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReadError {
    /// No such file. For a `skills/` entry this is the ordinary answer for a
    /// directory that is not a skill.
    NotFound,
    /// Exists, could not be opened.
    Denied,
    /// Not a regular file — a directory, FIFO, socket or device. Reported
    /// separately so the caller can treat it as "there is no skill file here"
    /// without the read having opened it.
    NotRegular,
    /// Past [`SKILL_MAX_BYTES`].
    Oversize {
        /// The file's real size.
        bytes: u64,
    },
    /// Not valid UTF-8.
    NotUtf8,
    /// The file itself is a symlink. Refused for the reason every entry
    /// symlink is refused — and reported separately so the diagnostic says
    /// *why* rather than blaming a permission the user has.
    SymlinkLeaf,
}

/// The filesystem, as discovery is allowed to see it.
///
/// Two methods, no recursion, no globbing, no `..`: the trait *is* the bound.
/// A caller cannot ask this seam for a walk because there is nothing on it that
/// walks.
pub trait DirLister {
    /// The entries directly under `dir`, in whatever order the filesystem
    /// gives them. Ordering is the caller's job — see [`discover`].
    ///
    /// # Errors
    ///
    /// [`ListError::NotFound`] or [`ListError::Denied`].
    fn list(&self, dir: &Path) -> Result<Vec<Entry>, ListError>;

    /// The contents of the regular file at `file`, bounded at
    /// [`SKILL_MAX_BYTES`].
    ///
    /// # Errors
    ///
    /// [`ReadError`], one variant per user-visible sentence.
    fn read(&self, file: &Path) -> Result<String, ReadError>;

    /// `path` with every symlink component followed, or `None` when it does not
    /// resolve (it is missing, or a component of it is unreadable).
    ///
    /// The one method here that neither lists nor reads. It exists because
    /// [`discover`] has a question about a **project** root that `list` cannot
    /// answer: `read_dir` follows, and `DirEntry::file_type` — the `lstat` the
    /// entry rule uses — is about entries *inside* a directory, so nothing on
    /// the old seam could see that the directory itself was a link out of the
    /// repository. It resolves paths; it enumerates nothing, which is why it
    /// does not widen what this trait can be asked to do.
    ///
    /// **This one has a default**, unlike the two above, and the default is the
    /// real filesystem's answer. A `DirLister` double exists to script what
    /// *listing* and *reading* return; a double that also had to script path
    /// resolution would be scripting the check itself, and the first thing an
    /// under-specified double would do is resolve everything to itself — which
    /// is the permissive answer. Overriding is possible and deliberate;
    /// forgetting to override gets the safe behaviour.
    fn canonicalize(&self, path: &Path) -> Option<PathBuf> {
        std::fs::canonicalize(path).ok()
    }
}

/// The production [`DirLister`]: the real filesystem.
#[derive(Debug, Clone, Copy, Default)]
pub struct RealFs;

impl DirLister for RealFs {
    fn list(&self, dir: &Path) -> Result<Vec<Entry>, ListError> {
        let listing = std::fs::read_dir(dir).map_err(|error| match error.kind() {
            std::io::ErrorKind::NotFound => ListError::NotFound,
            _ => ListError::Denied,
        })?;
        let mut entries = Vec::new();
        for entry in listing {
            // A per-entry failure (the entry vanished mid-listing, or its type
            // could not be read) drops that entry and nothing else: the
            // alternative is failing a whole root over one racing file.
            let Ok(entry) = entry else { continue };
            let Ok(file_type) = entry.file_type() else {
                continue;
            };
            entries.push(Entry {
                file_name: entry.file_name(),
                file_type,
            });
        }
        Ok(entries)
    }

    fn read(&self, file: &Path) -> Result<String, ReadError> {
        // `symlink_metadata`, not `metadata`: the entry rule refuses a symlink
        // and this leaf must be refused by the same rule.
        //
        // Under `Shape::Skills` the *entry* is the directory, so `consider`'s
        // symlink check never sees `<dir>/SKILL.md` — and `metadata` follows.
        // Two things went wrong through that hole, and only the first is about
        // walk cost, which is what the module doc used to argue about:
        //
        // 1. **The jail.** `.claude/skills/x/SKILL.md -> ~/.ssh/id_rsa` in a
        //    cloned repo put that file's bytes into a prompt. `ReadTool`
        //    resolves a link and refuses a target outside the root; discovery
        //    did neither.
        // 2. **One file, two identities.** `ProvenanceId::from_resolved` is
        //    documented as taking a *canonical* path, and was handed the link.
        //    So `SKILL.md -> ../../../.env` minted the id
        //    `.claude/skills/x/SKILL.md`, which no `local_only = [".env"]` glob
        //    matches — BR-7's "pins exactly as a `read` would" was false for
        //    precisely the shape that most needed it (LESSON-503: mint an id at
        //    the scope that resolves it).
        //
        // Refusing is the cheaper of the two fixes and the consistent one: the
        // rule the rest of this module states is already "a symlinked entry is
        // not followed", and a skill file is an entry.
        let metadata = std::fs::symlink_metadata(file).map_err(|error| match error.kind() {
            std::io::ErrorKind::NotFound => ReadError::NotFound,
            _ => ReadError::Denied,
        })?;
        if metadata.file_type().is_symlink() {
            return Err(ReadError::SymlinkLeaf);
        }
        if !metadata.is_file() {
            return Err(ReadError::NotRegular);
        }
        if metadata.len() > SKILL_MAX_BYTES {
            return Err(ReadError::Oversize {
                bytes: metadata.len(),
            });
        }
        let handle = std::fs::File::open(file).map_err(|error| match error.kind() {
            std::io::ErrorKind::NotFound => ReadError::NotFound,
            _ => ReadError::Denied,
        })?;
        let mut bytes = Vec::new();
        // `read_to_string` would flatten invalid UTF-8 into an `io::Error`,
        // which is exactly the distinction BR-1 asks for; read bytes and
        // convert, so `not UTF-8` is a verdict rather than an inference.
        handle
            .take(SKILL_MAX_BYTES)
            .read_to_end(&mut bytes)
            .map_err(|_| ReadError::Denied)?;
        String::from_utf8(bytes).map_err(|_| ReadError::NotUtf8)
    }
}

/// Build the registry for a session rooted at `session_root`, for a user whose
/// home is `home`.
///
/// Pure over the seam: given the same listings, bytes and resolutions it returns
/// the same registry, on any filesystem, in any order. Everything that could
/// vary — listing order, which entries survive the cap, the order of the rows —
/// is decided here rather than inherited from the filesystem.
///
/// `root_kind` is the session root's classification, and `RootKind::Home` skips
/// the project pair entirely: a session whose root *is* `$HOME` reaches
/// `~/.claude/skills` through both pairs, and without the skip every skill
/// would register twice — under two sources, shadowing itself, with two
/// permission keys for one file.
#[must_use]
pub fn discover(
    home: Option<&Path>,
    session_root: &Path,
    root_kind: RootKind,
    fs: &dyn DirLister,
) -> SkillRegistry {
    let mut candidates: Vec<Skill> = Vec::new();
    let mut skipped: Vec<Skipped> = Vec::new();

    // The session root as the filesystem resolves it, taken once. The
    // containment test below must compare two paths of the same kind, and
    // `session_root` arrives as the registry stores it — `ProbedRoot::probe`
    // deliberately never canonicalizes, so on macOS a session opened at
    // `/tmp/repo` is stored that way while everything under it resolves through
    // `/private`. Comparing the resolved root against the unresolved one would
    // refuse every project root on that machine.
    //
    // **Taken once, and kept.** REQ-589 D-13: this value leaves on the registry
    // (`SkillRegistry::read_under`) because it is the tree the bodies below are
    // about to be read out of, and BR-4's durable trust name is minted from it
    // rather than re-derived at turn time. A second resolution of
    // `session_root`, taken whenever the model reaches for a skill, is a second
    // answer to *which tree is this* — and the session is long enough for a
    // link to have been re-pointed in between.
    let boundary = fs.canonicalize(session_root);

    // In precedence order (project before user, `skills/` before `commands/`),
    // so the first candidate registered under a name is the one that wins it.
    for root in roots(home, session_root, root_kind) {
        // **Before the listing**, because `read_dir` follows and there is no
        // undoing it: a project root that resolves out of the repository is
        // refused, and refused by name. A **user** root is not asked — see the
        // module doc for why that exemption is the home directory's and not the
        // repository's.
        if root.source == SkillSource::Project
            && !resolves_under(fs, &root.dir, boundary.as_deref())
        {
            skipped.push(Skipped {
                path_display: root.display(&root.dir),
                path: root.dir.clone(),
                name: None,
                reason: SkipReason::EscapingRoot,
            });
            continue;
        }

        let mut entries = match fs.list(&root.dir) {
            Ok(entries) => entries,
            // A root that is not there is the ordinary state of three of the
            // four on most machines: no diagnostic, no cost.
            Err(ListError::NotFound) => continue,
            Err(ListError::Denied) => {
                skipped.push(Skipped {
                    path_display: root.display(&root.dir),
                    path: root.dir.clone(),
                    name: None,
                    reason: SkipReason::Unreadable,
                });
                continue;
            }
        };

        // Sorted **before** the cap, and before anything is registered. APFS
        // lists in hash order and ext4 does not, so an unsorted cap would make
        // *which* 512 entries survive a property of the filesystem, and an
        // unsorted registry would make `/help`'s order one too (LESSON-540).
        entries.sort_by(|a, b| a.file_name.cmp(&b.file_name));
        if entries.len() > MAX_ENTRIES_PER_ROOT {
            entries.truncate(MAX_ENTRIES_PER_ROOT);
            skipped.push(Skipped {
                path_display: root.display(&root.dir),
                path: root.dir.clone(),
                name: None,
                reason: SkipReason::RootTruncated,
            });
        }

        for entry in entries {
            consider(&root, &entry, fs, &mut candidates, &mut skipped);
        }
    }

    let mut registry = assemble(candidates, skipped);
    // Written here rather than passed through `assemble`, which is the name
    // contest and has nothing to do with the filesystem. A child module may
    // reach a private field of its parent's type, and this is the one place
    // entitled to fill this one: it is the only code that resolved anything.
    registry.read_under = boundary;
    registry
}

/// One skill file's repo-relative identity for the egress provenance channel,
/// or `None` when it has none (REQ-585 BR-7, REQ-587 BR-10 / ADR-8).
///
/// **The** mint for a skill body, so the user path and the model path cannot
/// come to disagree about which file a block came from.
///
/// `None` is **fail-closed**, and every caller must spell it that way: as
/// `unknown: true` on a seed block, or `ToolProvenance::Unknown` on a tool
/// outcome. It is the ordinary answer for a **user** skill, which has no
/// repo-relative identity in a repo-rooted session (ADR-9 refused to widen the
/// minter), and it is the safe answer in the two cases below.
///
/// # Why the path is resolved first
///
/// [`teton_core::ProvenanceId::from_resolved`] is documented as taking a
/// **canonical** path, and [`Skill::path`] is not one: it is the path discovery
/// walked to the file, kept because that is the spelling a user recognizes. A
/// project root may be a symlink *within* the repository
/// (`.claude/skills -> vendor/skills`), which [`discover`] permits, and minting
/// off the spelling would give one file two identities — the id would read
/// `.claude/skills/x/SKILL.md` for a file that lives at
/// `vendor/skills/x/SKILL.md`, so a `vendor/**` boundary would match nothing and
/// BR-7's *"pins exactly as a `read` would"* would be false for precisely the
/// shape that most needs it (LESSON-503: mint an id at the scope that resolves
/// it). It is the same defect the `RealFs::read` comment above names at the leaf,
/// one level up.
///
/// `root` is resolved too, because it arrives as the session registry stores it
/// and `ProbedRoot::probe` deliberately never canonicalizes: comparing a
/// resolved file against an unresolved root would strip nothing on any machine
/// where the session root is reached through a link (`/tmp` on macOS), turning
/// every project skill unknown. A root that will not resolve is used as given —
/// the *file's* resolution is the half that closes the hole.
///
/// A file that no longer resolves at all — moved or deleted since discovery
/// built the snapshot — answers `None` rather than minting an id for a path
/// nothing is at.
#[must_use]
pub fn provenance_of(root: &Path, skill: &Skill) -> Option<teton_core::ProvenanceId> {
    let root = std::fs::canonicalize(root).unwrap_or_else(|_| root.to_path_buf());
    let resolved = std::fs::canonicalize(&skill.path).ok()?;
    teton_core::ProvenanceId::from_resolved(&root, &resolved).ok()
}

/// Whether `dir` resolves to a path at or under `boundary` — the containment
/// test a **project** root must pass before it is listed (BR-10).
///
/// Three answers, and each of the two `None`s is deliberate:
///
/// - `dir` does not resolve at all → **true**, deferring to the listing. This is
///   the ordinary state of both project roots on a repository with no `.claude/`
///   in it, which is most of them, and a refusal here would put a diagnostic on
///   every such session. `list` answers `NotFound` a line later and continues in
///   silence, which is the behaviour BR-1 specifies; a root that exists but
///   cannot be resolved fails `list` too and is named `Unreadable` there. In
///   neither case has anything been enumerated.
/// - `boundary` is `None` — the session root itself does not resolve → **false**.
///   Fail closed: there is nothing to compare against, so the question cannot be
///   answered, and an unanswerable containment question is not a yes. (Reaching
///   this with a resolving `dir` would take a root that vanished between the two
///   calls.)
///
/// `Path::starts_with` is component-wise, so `/repo-other` does not pass for a
/// boundary of `/repo` — the string-prefix bug this would otherwise have.
fn resolves_under(fs: &dyn DirLister, dir: &Path, boundary: Option<&Path>) -> bool {
    let Some(resolved) = fs.canonicalize(dir) else {
        return true;
    };
    boundary.is_some_and(|boundary| resolved.starts_with(boundary))
}

/// Decide one listed entry: register it, name why it was skipped, or pass over
/// it in silence.
///
/// Silence is reserved for the two things BR-1 calls normal — an entry that was
/// never a candidate (a stray file under `skills/`, a non-Markdown file under
/// `commands/`) and a directory with no `SKILL.md` in it. Everything else is
/// named.
fn consider(
    root: &RootSpec,
    entry: &Entry,
    fs: &dyn DirLister,
    candidates: &mut Vec<Skill>,
    skipped: &mut Vec<Skipped>,
) {
    let path = root.dir.join(&entry.file_name);

    // First, before the entry's name or type is consulted for anything else:
    // a symlink is refused, and is *named*. Naming it even when its name would
    // not have made it a candidate is deliberate — the rule is about what
    // discovery declines to follow, and the user whose `commands/` is a tree of
    // symlinks needs to be told that rather than left with an empty `/help`.
    if skip_symlink_entry(entry.file_type) {
        // Named where the shape can name it: a symlinked `commands/status.md`
        // is the reason `/status` is missing, and saying so is the whole point
        // of naming a refusal (BR-1, BR-10).
        let name = entry
            .file_name
            .to_str()
            .and_then(|entry_name| root.shape.name_of(entry_name))
            .map(str::to_owned);
        skipped.push(Skipped {
            path_display: root.display(&path),
            path,
            name,
            reason: SkipReason::SymlinkEntry,
        });
        return;
    }

    // A non-UTF-8 file name cannot be a valid name, and cannot be one the user
    // typed after a `/` either; it is not a candidate.
    let Some(entry_name) = entry.file_name.to_str() else {
        return;
    };
    let Some(name) = root.shape.name_of(entry_name) else {
        return;
    };

    match root.shape {
        Shape::Skills => {
            if !entry.file_type.is_dir() {
                // A README or a `.DS_Store` sitting beside the skill
                // directories. Not a skill, not a fault.
                return;
            }
            // The name is checked *after* the read for this shape, because
            // "a directory with no SKILL.md is not a skill" (BR-1) has to
            // dominate: a `.git` or a `node_modules` under a `skills/` root
            // must not be reported as an invalid name.
            let file = path.join("SKILL.md");
            let Some(text) = read_or_name(&file, name, root, fs, skipped) else {
                return;
            };
            register(name, root, file, &text, candidates, skipped);
        }
        Shape::Commands => {
            // Here the `.md` entry *is* the candidate, so a bad name is a bad
            // name and is named before anything is opened.
            if !is_valid_skill_name(name) {
                skipped.push(Skipped {
                    path_display: root.display(&path),
                    path,
                    name: Some(name.to_owned()),
                    reason: SkipReason::InvalidName,
                });
                return;
            }
            let Some(text) = read_or_name(&path, name, root, fs, skipped) else {
                return;
            };
            register(name, root, path, &text, candidates, skipped);
        }
    }
}

/// Read `file`, or push the diagnostic its failure earns and answer `None`.
///
/// `NotFound` and `NotRegular` produce no diagnostic: both mean "there is no
/// skill file here", and the second one means it **without having opened**
/// anything — the FIFO case.
fn read_or_name(
    file: &Path,
    name: &str,
    root: &RootSpec,
    fs: &dyn DirLister,
    skipped: &mut Vec<Skipped>,
) -> Option<String> {
    let reason = match fs.read(file) {
        Ok(text) => return Some(text),
        Err(ReadError::NotFound | ReadError::NotRegular) => return None,
        Err(ReadError::Denied) => SkipReason::Unreadable,
        // The same verdict a symlinked entry earns, for the same reason: the
        // rule is about what discovery declines to follow, and a `SKILL.md`
        // that is a link is a link.
        Err(ReadError::SymlinkLeaf) => SkipReason::SymlinkEntry,
        Err(ReadError::Oversize { bytes }) => SkipReason::Oversize { bytes },
        Err(ReadError::NotUtf8) => SkipReason::NotUtf8,
    };
    skipped.push(Skipped {
        path_display: root.display(file),
        path: file.to_path_buf(),
        name: Some(name.to_owned()),
        reason,
    });
    None
}

/// Validate the name, parse the frontmatter, and push either a candidate or the
/// reason there is none.
fn register(
    name: &str,
    root: &RootSpec,
    path: PathBuf,
    text: &str,
    candidates: &mut Vec<Skill>,
    skipped: &mut Vec<Skipped>,
) {
    if !is_valid_skill_name(name) {
        skipped.push(Skipped {
            path_display: root.display(&path),
            path,
            name: Some(name.to_owned()),
            reason: SkipReason::InvalidName,
        });
        return;
    }
    let Ok(parsed) = frontmatter::parse(text) else {
        // Skipped **whole**: there is no partial value to register, because
        // the parser returns none (ADR-5).
        skipped.push(Skipped {
            path_display: root.display(&path),
            path,
            name: Some(name.to_owned()),
            reason: SkipReason::MalformedFrontmatter,
        });
        return;
    };
    // BUG-185: a body's dynamic-command count is bounded here, before the row
    // exists. Scanned against the **raw** body, which is the right conservative
    // reading: `$ARGUMENTS` substitution happens later and can only *add*
    // openers (an argument may carry a `` !` `` of its own), never remove one,
    // so a body that passes here can still be capped at expansion but a body
    // that fails here could never have got smaller.
    let declared_commands = dynamic::scan(&parsed.body).1.len();
    if declared_commands > MAX_DYNAMIC_COMMANDS {
        skipped.push(Skipped {
            path_display: root.display(&path),
            path,
            name: Some(name.to_owned()),
            reason: SkipReason::TooManyCommands {
                count: declared_commands,
            },
        });
        return;
    }
    // A frontmatter `name` that differs is a note, never a second spelling:
    // one spelling reaches one handler (BR-2, REQ-555's rule).
    let name_note = parsed.name.as_deref().and_then(|declared| {
        (declared != name).then(|| {
            format!("frontmatter name `{declared}` differs; this command dispatches as `/{name}`")
        })
    });
    candidates.push(Skill {
        name: name.to_owned(),
        source: root.source,
        path_display: root.display(&path),
        path,
        description: parsed.description,
        argument_hint: parsed.argument_hint,
        body: parsed.body,
        // Carried, never re-derived: the frontmatter is read once, and BR-3's
        // safe readings for an unparseable value live in that one place. A row
        // that arrived here with a flag dropped would be a skill the model may
        // run because a field defaulted (REQ-587 BR-3).
        model_invocable: parsed.model_invocable,
        // REQ-615 BR-5's forward path, carried as declared.
        requires_project: parsed.requires_project,
        user_invocable: parsed.user_invocable,
        ignored_keys: parsed.ignored_keys,
        name_note,
        shadowed: None,
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_root(tag: &str) -> PathBuf {
        use std::sync::atomic::{AtomicU64, Ordering};
        static NEXT: AtomicU64 = AtomicU64::new(0);
        let dir = std::env::temp_dir().join(format!(
            "teton-skills-{tag}-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    /// The bounded read tells its four failures apart, which is the whole
    /// reason `skills` does not call `fs_util::read_regular_file_bounded`.
    #[test]
    fn the_real_read_names_which_failure_it_was() {
        let root = temp_root("read");
        assert_eq!(RealFs.read(&root.join("absent")), Err(ReadError::NotFound));
        assert_eq!(RealFs.read(&root), Err(ReadError::NotRegular));

        let big = root.join("big.md");
        std::fs::write(&big, "x".repeat(SKILL_MAX_BYTES as usize + 1)).unwrap();
        assert_eq!(
            RealFs.read(&big),
            Err(ReadError::Oversize {
                bytes: SKILL_MAX_BYTES + 1
            }),
            "the diagnostic carries the file's real size, not the bound"
        );

        let binary = root.join("bin.md");
        std::fs::write(&binary, b"\xff\xfe").unwrap();
        assert_eq!(RealFs.read(&binary), Err(ReadError::NotUtf8));

        let ok = root.join("ok.md");
        std::fs::write(&ok, "body").unwrap();
        assert_eq!(RealFs.read(&ok).as_deref(), Ok("body"));

        // Exactly at the bound still reads.
        let edge = root.join("edge.md");
        std::fs::write(&edge, "x".repeat(SKILL_MAX_BYTES as usize)).unwrap();
        assert_eq!(
            RealFs.read(&edge).map(|text| text.len()),
            Ok(SKILL_MAX_BYTES as usize)
        );

        std::fs::remove_dir_all(&root).ok();
    }

    /// A FIFO named `SKILL.md` is refused off `metadata` **before** any open,
    /// so discovery returns instead of blocking forever on a writer that never
    /// comes. The deadline is the assertion: a regression here is a hang, and a
    /// hung suite reports nothing.
    #[test]
    fn a_fifo_is_not_regular_and_is_never_opened() {
        let root = temp_root("fifo");
        let fifo = root.join("SKILL.md");
        crate::mkfifo(&fifo);
        let read = crate::with_deadline("RealFs::read on a FIFO", move || RealFs.read(&fifo));
        assert_eq!(read, Err(ReadError::NotRegular));
        std::fs::remove_dir_all(&root).ok();
    }

    /// One file's bytes, as `register` turns them into a candidate row.
    fn registered(text: &str) -> Skill {
        let mut candidates = Vec::new();
        let mut skipped = Vec::new();
        // BUG-187: `register` takes the root it came from, because the root
        // owns the display spelling. A user root, so the home rule applies.
        let root = RootSpec::new(
            SkillSource::User,
            Shape::Commands,
            Path::new("/h"),
            None,
            Some(Path::new("/h")),
        );
        register(
            "alpha",
            &root,
            PathBuf::from("/h/.claude/commands/alpha.md"),
            text,
            &mut candidates,
            &mut skipped,
        );
        assert!(skipped.is_empty(), "the file must register: {skipped:?}");
        candidates.pop().expect("exactly one candidate")
    }

    /// The `register` outcome for `text`, whether it landed or was skipped.
    fn outcome(text: &str) -> (Vec<Skill>, Vec<Skipped>) {
        let mut candidates = Vec::new();
        let mut skipped = Vec::new();
        let root = RootSpec::new(
            SkillSource::User,
            Shape::Commands,
            Path::new("/h"),
            None,
            Some(Path::new("/h")),
        );
        register(
            "alpha",
            &root,
            PathBuf::from("/h/.claude/commands/alpha.md"),
            text,
            &mut candidates,
            &mut skipped,
        );
        (candidates, skipped)
    }

    /// BUG-185: a body over the slot cap never becomes a row.
    ///
    /// Refused at discovery rather than at expansion, and that placement is the
    /// security claim: an unregistered file is not invocable, so its commands
    /// never reach a consent prompt at all. That is what closes the
    /// consent-flooding surface — 400 innocuous commands with a dangerous one
    /// buried cannot be rendered to a user who has no row to invoke.
    #[test]
    fn a_body_over_the_dynamic_command_cap_is_skipped_with_its_real_count() {
        let over = MAX_DYNAMIC_COMMANDS + 1;
        let body = "!`echo hi`\n".repeat(over);
        let (candidates, skipped) = outcome(&body);

        assert!(
            candidates.is_empty(),
            "an over-cap body must not register: {candidates:?}"
        );
        assert_eq!(
            skipped.len(),
            1,
            "and it is reported, never silently dropped: {skipped:?}"
        );
        assert_eq!(
            skipped[0].reason,
            SkipReason::TooManyCommands { count: over },
            "the real count rides the reason, so the author knows what to cut"
        );
        assert!(
            skipped[0].reason.to_string().contains(&over.to_string())
                && skipped[0]
                    .reason
                    .to_string()
                    .contains(&MAX_DYNAMIC_COMMANDS.to_string()),
            "both figures are in the sentence: {}",
            skipped[0].reason
        );
    }

    /// Non-vacuity for the cap: exactly at the limit still registers.
    ///
    /// Without this leg the test above would pass on a cap of zero, which would
    /// break every shipped skill — `template-drift` declares six.
    #[test]
    fn a_body_exactly_at_the_dynamic_command_cap_still_registers() {
        let body = "!`echo hi`\n".repeat(MAX_DYNAMIC_COMMANDS);
        let (candidates, skipped) = outcome(&body);
        assert_eq!(
            candidates.len(),
            1,
            "at the cap is not over it: {skipped:?}"
        );
        assert!(skipped.is_empty(), "{skipped:?}");
    }

    /// **REQ-587 BR-3, carried.** `disable-model-invocation` reaches the row.
    ///
    /// Both directions on purpose: a `model_invocable` hard-coded to either
    /// constant on the way through — the shape "the flag was dropped" takes,
    /// since `Skill` has no `Default` to omit it into — fails one leg or the
    /// other.
    #[test]
    fn the_model_invocation_flag_reaches_the_registered_row() {
        assert!(
            !registered("---\ndisable-model-invocation: true\n---\nbody").model_invocable,
            "a file that hides itself from the model must arrive hidden"
        );
        assert!(registered("---\ndisable-model-invocation: false\n---\nbody").model_invocable);
        assert!(
            registered("# no frontmatter at all\n").model_invocable,
            "the majority case — no header — is the model's"
        );
    }

    /// The same, for `user-invocable`, which defaults the other way.
    #[test]
    fn the_user_invocation_flag_reaches_the_registered_row() {
        let model_only = registered("---\nuser-invocable: false\n---\nbody");
        assert!(!model_only.user_invocable, "the flag must arrive");
        assert!(
            model_only.is_dispatchable() && !model_only.dispatchable_by_user(),
            "and it must be the *third* state: it owns its name (ADR-12) and \
             the user still may not type it"
        );
        assert!(
            model_only.invocable_by_model(),
            "model-only means the model reaches it"
        );

        assert!(registered("---\nuser-invocable: true\n---\nbody").user_invocable);
        assert!(registered("# no frontmatter at all\n").user_invocable);
    }

    /// A flag whose *value* is not a boolean does not skip the file: it
    /// registers, with the safe reading and the key named where `/verbose`
    /// renders it (BR-3, `frontmatter`'s module doc).
    #[test]
    fn a_bad_flag_value_registers_the_file_and_names_the_key() {
        let skill = registered("---\nuser-invocable: yes\n---\nbody");
        assert!(
            skill.user_invocable,
            "the safe reading: the user keeps `/name`"
        );
        assert_eq!(skill.ignored_keys, vec!["user-invocable"]);
        assert_eq!(skill.body, "body");

        let skill = registered("---\ndisable-model-invocation: sure\n---\nbody");
        assert!(
            !skill.model_invocable,
            "the safe reading: a typo can never widen what the model may run"
        );
        assert_eq!(skill.ignored_keys, vec!["disable-model-invocation"]);
    }

    /// `list` tells a missing root from a refused one — the distinction that
    /// keeps three absent roots free and a TCC-guarded one loud.
    #[test]
    fn listing_a_missing_directory_is_not_listing_a_refused_one() {
        let root = temp_root("list");
        assert_eq!(
            RealFs.list(&root.join("absent")).unwrap_err(),
            ListError::NotFound
        );
        assert!(RealFs.list(&root).unwrap().is_empty());
        std::fs::remove_dir_all(&root).ok();
    }

    /// **The snapshot names the tree its bodies came out of, not the path it
    /// was asked about (REQ-589 D-13).**
    ///
    /// The discovery half of the fix that made the durable trust name
    /// un-substitutable. `read_under` is what the trust door mints
    /// `[skills] trusted_project_roots` from, and its whole value is that it is
    /// **this** resolution — the one the containment test used and the one the
    /// bodies below were read through — rather than a second one taken at the
    /// door, hours later, off a path a link may have been re-pointed under.
    ///
    /// Both halves are asserted, because either alone is passable by a mistake:
    /// the link's own spelling must not be the answer (that is the substitution)
    /// **and** the target must be, byte for byte, so the name minted from it is
    /// the acknowledged tree's rather than nothing at all.
    ///
    /// A root that will not resolve answers `None`, and answers it together with
    /// an empty project set: the door is fail-closed and unreachable at once.
    ///
    /// **Mutation:** leave `read_under` at `assemble`'s `None` and the whole
    /// unattended path refuses everything; fill it from `session_root` instead
    /// of from `boundary` and the first assertion fails.
    #[test]
    fn the_registry_records_the_root_its_bodies_were_read_under() {
        let base = temp_root("read-under");
        let real = base.join("real");
        std::fs::create_dir_all(real.join(".claude/skills/marked")).unwrap();
        std::fs::write(
            real.join(".claude/skills/marked/SKILL.md"),
            "---\ndescription: marked\n---\n\nbody\n",
        )
        .unwrap();
        let link = base.join("link");
        std::os::unix::fs::symlink(&real, &link).unwrap();

        let registry = discover(None, &link, RootKind::Project, &RealFs);
        assert!(
            registry
                .skills()
                .iter()
                .any(|skill| skill.name == "marked" && skill.source == SkillSource::Project),
            "non-vacuity: the body really was read through the link, so the \
             identity below is one that authorizes something: {:?}",
            registry.skipped()
        );
        assert_eq!(
            registry.read_under(),
            Some(std::fs::canonicalize(&real).unwrap().as_path()),
            "the snapshot must name the tree it read, or a row written for that \
             tree can be spent by a link standing somewhere else"
        );
        assert_ne!(
            registry.read_under(),
            Some(link.as_path()),
            "the path as spelled is exactly what must not be the identity"
        );

        // A session root that does not resolve: no identity, and nothing for one
        // to have authorized.
        let gone = discover(None, &base.join("absent"), RootKind::Project, &RealFs);
        assert_eq!(gone.read_under(), None);
        assert!(
            gone.skills().is_empty(),
            "fail-closed twice: an unresolvable root registers no project skill \
             either, so the door it would refuse is never reached"
        );

        std::fs::remove_dir_all(&base).ok();
    }
}
