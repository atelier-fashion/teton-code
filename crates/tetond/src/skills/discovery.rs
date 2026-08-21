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
//! # Roots are followed. Entries are not.
//!
//! The four roots are opened with no symlink check: the dogfood machine's
//! `~/.claude/skills` *is* a symlink into a checked-out toolkit, and a rule
//! that refused it would refuse the feature's own author. Every **entry** a
//! listing returns is refused if its `file_type()` says symlink, reusing
//! [`crate::harness::tools::skip_symlink_entry`] so the predicate keeps one
//! home. `DirEntry::file_type` has `lstat` semantics — it does **not** traverse
//! — which is precisely why the check can be made without opening anything.
//!
//! This is *narrower* than the walkers' blanket refusal, so it is pinned by its
//! own test rather than riding theirs.
//!
//! The one path that is followed below an entry is the leaf whose name the
//! entry fully determines: `<dir>/SKILL.md`. Following that link enumerates
//! nothing — the danger a symlink poses to a walk is unbounded *ground*, and a
//! fixed leaf name is not ground.

use std::ffi::OsString;
use std::io::Read;
use std::path::{Path, PathBuf};

use teton_protocol::methods::RootKind;

use super::{
    assemble, frontmatter, is_valid_skill_name, roots, RootSpec, Shape, Skill, SkillRegistry,
    SkillSource, SkipReason, Skipped, MAX_ENTRIES_PER_ROOT, SKILL_MAX_BYTES,
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
/// Pure over the seam: given the same listings and bytes it returns the same
/// registry, on any filesystem, in any order. Everything that could vary —
/// listing order, which entries survive the cap, the order of the rows — is
/// decided here rather than inherited from the filesystem.
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

    // In precedence order (project before user, `skills/` before `commands/`),
    // so the first candidate registered under a name is the one that wins it.
    for root in roots(home, session_root, root_kind) {
        let mut entries = match fs.list(&root.dir) {
            Ok(entries) => entries,
            // A root that is not there is the ordinary state of three of the
            // four on most machines: no diagnostic, no cost.
            Err(ListError::NotFound) => continue,
            Err(ListError::Denied) => {
                skipped.push(Skipped {
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
                path: root.dir.clone(),
                name: None,
                reason: SkipReason::RootTruncated,
            });
        }

        for entry in entries {
            consider(&root, &entry, fs, &mut candidates, &mut skipped);
        }
    }

    assemble(candidates, skipped)
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
            let Some(text) = read_or_name(&file, name, fs, skipped) else {
                return;
            };
            register(name, root.source, file, &text, candidates, skipped);
        }
        Shape::Commands => {
            // Here the `.md` entry *is* the candidate, so a bad name is a bad
            // name and is named before anything is opened.
            if !is_valid_skill_name(name) {
                skipped.push(Skipped {
                    path,
                    name: Some(name.to_owned()),
                    reason: SkipReason::InvalidName,
                });
                return;
            }
            let Some(text) = read_or_name(&path, name, fs, skipped) else {
                return;
            };
            register(name, root.source, path, &text, candidates, skipped);
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
    source: SkillSource,
    path: PathBuf,
    text: &str,
    candidates: &mut Vec<Skill>,
    skipped: &mut Vec<Skipped>,
) {
    if !is_valid_skill_name(name) {
        skipped.push(Skipped {
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
            path,
            name: Some(name.to_owned()),
            reason: SkipReason::MalformedFrontmatter,
        });
        return;
    };
    // A frontmatter `name` that differs is a note, never a second spelling:
    // one spelling reaches one handler (BR-2, REQ-555's rule).
    let name_note = parsed.name.as_deref().and_then(|declared| {
        (declared != name).then(|| {
            format!("frontmatter name `{declared}` differs; this command dispatches as `/{name}`")
        })
    });
    candidates.push(Skill {
        name: name.to_owned(),
        source,
        path,
        description: parsed.description,
        argument_hint: parsed.argument_hint,
        body: parsed.body,
        // Carried, never re-derived: the frontmatter is read once, and BR-3's
        // safe readings for an unparseable value live in that one place. A row
        // that arrived here with a flag dropped would be a skill the model may
        // run because a field defaulted (REQ-587 BR-3).
        model_invocable: parsed.model_invocable,
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
        register(
            "alpha",
            SkillSource::User,
            PathBuf::from("/h/.claude/commands/alpha.md"),
            text,
            &mut candidates,
            &mut skipped,
        );
        assert!(skipped.is_empty(), "the file must register: {skipped:?}");
        candidates.pop().expect("exactly one candidate")
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
}
