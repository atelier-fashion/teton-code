//! `TETON.md` — the repository's own notes, read at the session root and
//! carried as resident data (REQ-612 BR-1, BR-3, BR-4, BR-5).
//!
//! Two halves, and the split is the point (ADR-3):
//!
//! - **this file** is the filesystem half — the candidate names, the one `stat`
//!   per candidate, the read ceiling, the jail, the identity, the boundary
//!   verdict and the six states a session can be in;
//! - [`render`] is the pure half — strip, truncate, frame. It touches no
//!   filesystem and is a function of `(RepoContextFile, cap)` alone, which is
//!   what lets the two resident-ceiling sweeps measure a block at the cap with
//!   no fixture tree.
//!
//! # Everything the filesystem can say goes through one seam
//!
//! [`RepoFileReader`] is the [`DirLister`](crate::skills::discovery::DirLister)
//! shape, for the reason written down there: a *reach* claim ("nothing above or
//! below the root was opened") cannot be made by a test that hands the code the
//! real filesystem and then inspects the answer. A recording double captures
//! every path the loader asks about and the suite compares the recorded calls
//! for **equality**, so a loader that also went somewhere else fails even when
//! its answer is right.
//!
//! Two methods, no listing, no globbing, no recursion: BR-1 says one file at one
//! place, and the trait is what makes that structural rather than a convention —
//! there is nothing on this seam that can enumerate a directory.
//!
//! # The jail is `ToolContext::resolve`'s, not a second spelling of it
//!
//! The candidate is resolved through
//! [`ToolContext::resolve`](crate::harness::tools::ToolContext::resolve), which
//! is where the outside-root refusal, the broken-symlink refusal and the mint
//! live for every file tool. Re-spelling those rules here would be a second
//! classifier of what may be read — precisely what REQ-583 BR-10 forbids — and
//! LESSON-623 is the other half: the identity a boundary glob matches has to be
//! minted **by the seam that resolved the path**, or a `local_only` glob names
//! one spelling and the daemon reads another.
//!
//! That call is not on the [`RepoFileReader`] seam and deliberately so. It
//! resolves a path; it opens nothing. A double that scripted it would be
//! scripting the check itself (the note on
//! [`DirLister::canonicalize`](crate::skills::discovery::DirLister::canonicalize)
//! is the same argument), so the fixtures below plant a real, canonical
//! directory for the root and let the real resolver answer.
//!
//! # `lstat` the spelling, read the resolution
//!
//! The entry check and the jail check are about two different paths and both are
//! needed. `<root>/TETON.md` is `lstat`ed — the [`RepoFileReader::stat`] call,
//! which does not traverse — so a symlinked entry is named
//! [`RepoContextState::Unreadable`] and never followed (BR-1, REQ-571 BR-5); the
//! *resolved* path is what is opened, because the gate must decide on the parse
//! the executor used (LESSON-494). A `resolve` that followed the link would have
//! answered about the target, which is exactly the check the entry rule exists to
//! make.
//!
//! # What closes the `lstat` → `open` window, and what it is closed against
//!
//! The `lstat` is a **check**, not a guarantee: between it and the open, a
//! same-UID process can put a symlink — or another regular file — where the one
//! that was `stat`ed used to be. Two things close that window, and the second is
//! the one that closes it *end to end*:
//!
//! - the open carries `O_NOFOLLOW` ([`RealFiles::read`]), so a symlink at the
//!   final component is refused by the kernel rather than followed;
//! - the entry's **identity** — `(dev, ino)` off the `lstat` — is carried
//!   forward as a [`FileIdentity`] and compared against an `fstat` of the opened
//!   handle. Anything else is [`RepoFileError::Changed`].
//!
//! The identity check is what the flag alone cannot do. The path the loader
//! opens is [`ToolContext::resolve`]'s answer, and `resolve` **canonicalizes** —
//! so a symlink planted after the `lstat` is already dereferenced by the time
//! `O_NOFOLLOW` sees the path, and the flag has nothing left to refuse. Passing
//! the identity rather than trusting the path is therefore not a second belt on
//! the same braces: it is the only check that spans both syscalls.
//!
//! It is an identity comparison and **not** a length one. Length is not
//! identity: an in-place edit between the two syscalls keeps `(dev, ino)` and is
//! accepted, which is right — the file at the root is the file the entry rule
//! judged, and the next turn's staleness check re-reads it. A *replacement* — a
//! rename over the top, a new file at the same name — moves the inode and is
//! refused.
//!
//! **Who can plant one.** Any process running as this user, which on a
//! developer's machine includes the daemon's own `shell` tool: a model that runs
//! `ln -s ~/.ssh/id_ed25519 TETON.md` in the working tree is inside the threat
//! model, not outside it. And the trigger is **automatic and silent** — BR-6's
//! staleness check re-reads at the start of any turn whose `stat` key moved, so
//! nobody has to type `/context` for the read to happen.
//!
//! # A hardlink is refused
//!
//! A **hardlinked** `TETON.md` passes both checks above: `lstat` says regular
//! file and not a symlink because that is what a hardlink is — a second name for
//! one inode — and the identity of the link *is* the identity of the target, so
//! `O_NOFOLLOW` has nothing to refuse and the `fstat` agrees. A user who can
//! create a hardlink at the session root to a file outside it would therefore
//! make its bytes resident.
//!
//! So the entry rule refuses `nlink > 1` outright, which is one comparison on a
//! field the `lstat` already answered. It costs a legitimate `TETON.md` nothing
//! — a repository file with a second name is not a shape anyone maintains on
//! purpose — and it closes the last path by which the loader can be pointed at
//! bytes outside the root without following anything.
//!
//! # What is deliberately not here
//!
//! No call into [`crate::projects::scan`] — `scan.rs`'s forbid list guards the
//! `session/create` derivation, and a one-file `stat` + `read` has no reason to
//! reach for a scanner. No watcher: BR-6's staleness check is [`RepoContext::verdict`],
//! run by the turn's `assemble` stage, and REQ-585 BR-1 already decided against
//! watching. No wiring: `HarnessConfig`, the session record and the runtime are
//! TASK-373/374's, and nothing in this module knows they exist.

pub mod render;

use std::fs::OpenOptions;
use std::io::Read;
use std::os::unix::fs::{MetadataExt, OpenOptionsExt};
use std::path::{Path, PathBuf};
use std::time::SystemTime;

use teton_core::boundary::BoundaryMatcher;
use teton_core::ProvenanceId;
use teton_protocol::methods::{RepoContextSource, RepoContextStateKind, RootKind};

use crate::harness::tools::ToolContext;
use crate::session_root::ProbedRoot;

pub use render::RepoContextBlock;

/// The names read at the session root, in the order they win (ADR-7 / OQ-1).
///
/// `TETON.md` first because it is this build's own name; `AGENTS.md` second
/// because it is the vendor-neutral one a repository may already carry.
/// `CLAUDE.md` is **not** a third entry, and its absence is a decision rather
/// than an omission: it names another tool's commands, which is BUG-181's shape
/// with the repository as author.
///
/// The order is precedence and nothing else — the contest is decided on
/// *existence*, at the first candidate that `stat`s, so a present-but-empty
/// `TETON.md` does not fall through to `AGENTS.md` (see [`RepoContext::load`]).
pub const CANDIDATE_NAMES: [&str; 2] = ["TETON.md", "AGENTS.md"];

/// [`CANDIDATE_NAMES`] paired with the source each one is, in the same order.
///
/// The pairing is written once and checked by
/// `only_a_project_root_is_read_and_teton_md_wins_over_agents_md`: a name and
/// the enum value that stands for it are two spellings of one fact, and the
/// frame line renders the *name* while every wire surface reports the *source*.
const CANDIDATES: [(&str, RepoContextSource); 2] = [
    (CANDIDATE_NAMES[0], RepoContextSource::TetonMd),
    (CANDIDATE_NAMES[1], RepoContextSource::AgentsMd),
];

/// The file name a [`RepoContextSource`] stands for.
///
/// A free function rather than an inherent method because the enum is
/// `teton-protocol`'s (TASK-370) and the orphan rule puts inherent impls in the
/// defining crate. One `match`, so the frame line and the loader cannot come to
/// disagree about which bytes a `teton_md` is.
#[must_use]
pub fn file_name(source: RepoContextSource) -> &'static str {
    match source {
        RepoContextSource::TetonMd => CANDIDATE_NAMES[0],
        RepoContextSource::AgentsMd => CANDIDATE_NAMES[1],
    }
}

/// The resident byte cap (BR-3, ADR-5): 8 KiB, a quarter of the local tier's
/// 32,768-byte budget, decided as product on 2026-09-03.
///
/// This is the **maximum**, not the figure every route uses. ADR-5 makes the cap
/// route-aware — the effective cap is `min(REPO_CONTEXT_MAX_BYTES,
/// route.budget_bytes / 4)` — so a floored 16,384-byte route renders the same
/// file at 4,096. That is why
/// [`RepoContextBlock::render`](render::RepoContextBlock::render) takes the cap
/// as a **parameter**: the route decides it, and the loader here classifies
/// against this ceiling because it is the widest any route can ask for.
pub const REPO_CONTEXT_MAX_BYTES: usize = 8_192;

/// The bound on the *read* itself (ADR-5, the REQ-585 body cap): 64 KiB.
///
/// The cap above bounds what reaches the prompt; this bounds what reaches
/// memory. A 10 MiB `TETON.md` costs 64 KiB and a truncation marker, not 10 MiB
/// — the difference matters because the loader runs at `session/create`, at every
/// `/cd`, and at any turn whose staleness check fires.
pub const REPO_CONTEXT_READ_CEILING_BYTES: u64 = 65_536;

/// What a `stat` says about a candidate: the staleness key BR-6 compares, plus
/// the two facts that decide whether the file may be opened at all.
///
/// `mtime` is optional because a filesystem may not report one, and the
/// comparison in [`RepoContext::verdict`] treats a missing timestamp as
/// **changed**: an absent fact cannot prove sameness, and the cost of being
/// wrong in that direction is one re-read.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FileStat {
    /// The file's size on disk, in bytes.
    pub len: u64,
    /// Last modification time, when the filesystem reports one.
    pub mtime: Option<SystemTime>,
    /// Whether the entry *itself* is a symlink — an `lstat` answer, so asking
    /// it has followed nothing (BR-1, REQ-571 BR-5).
    pub is_symlink: bool,
    /// Whether the entry is a regular file. A directory, FIFO, socket or device
    /// named `TETON.md` is not opened: a FIFO's open for reading blocks until a
    /// writer appears, and this runs on the turn path.
    pub is_regular: bool,
    /// The device this entry lives on — half of [`Self::identity`].
    pub dev: u64,
    /// The inode number — the other half. Unique only *within* a device, which
    /// is why the pair travels together.
    pub ino: u64,
    /// How many names this inode answers to. `1` for an ordinary file; anything
    /// more is a hardlink, which the entry rule refuses (see the module docs).
    pub nlink: u64,
}

impl FileStat {
    /// The identity the read is checked against ([`RepoFileReader::read`]).
    #[must_use]
    pub fn identity(&self) -> FileIdentity {
        FileIdentity {
            dev: self.dev,
            ino: self.ino,
        }
    }
}

/// Which file on which device — the one fact that survives a path being
/// re-resolved.
///
/// Carried from the entry `lstat` into the read so the two syscalls are about
/// the **same inode** and not merely about the same string. It is deliberately
/// not a `Path`: the loader opens [`ToolContext::resolve`]'s canonical answer,
/// and canonicalizing is exactly the step that dereferences a symlink planted
/// after the check (see the module docs).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FileIdentity {
    /// The device the entry was on when it was `stat`ed.
    pub dev: u64,
    /// The inode it was.
    pub ino: u64,
}

/// Why the filesystem could not answer.
///
/// Typed rather than an `Option` for [`ReadError`](crate::skills::discovery::ReadError)'s
/// reason: a missing file is the **normal** case and costs one `stat`, while a
/// refused one is a named `unreadable` state the user is shown. Collapsing them
/// would put "there are no notes" and "your notes could not be read" behind one
/// silence.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RepoFileError {
    /// No such file. Not a fault — BR-1's normal case.
    NotFound,
    /// It exists and could not be opened: `EPERM`, or macOS's TCC consent gate.
    ///
    /// **`PermissionDenied` and nothing else.** Every other `io::Error` is
    /// [`Self::Unavailable`], because the two name different remedies: a user
    /// told "permission denied" about a file they own and can `cat` will go
    /// looking for a permission that was never the problem.
    Denied,
    /// Not a regular file — a directory, FIFO, socket or device.
    NotRegular,
    /// Not valid UTF-8. Reported separately so the state names the reason
    /// rather than blaming a permission the user has.
    NotUtf8,
    /// The entry is a symlink and the open refused to follow it (`ELOOP` from
    /// `O_NOFOLLOW`).
    ///
    /// The same verdict [`RepoContext::load`]'s `lstat` reaches, from the other
    /// end: the check runs first and this closes the window behind it.
    Symlink,
    /// The opened handle is **not the inode that was `stat`ed**: the entry was
    /// replaced between the check and the open.
    ///
    /// Its own variant, and never [`Self::NotRegular`]: the file that is there
    /// now may be a perfectly ordinary regular file, and telling a user their
    /// `TETON.md` is "not a regular file" would send them looking at a
    /// directory that does not exist. A symlink and a hardlink have their own
    /// sentences too — this is what is left, and what is left is a race.
    Changed,
    /// It exists, it is not a permission problem, and it could not be read
    /// anyway — an I/O error, a device that went away, a filesystem that
    /// answered `EIO`.
    ///
    /// Its own variant so the sentence a user is shown is neutral about cause
    /// rather than wrong about it (see [`Self::Denied`]).
    Unavailable,
}

/// The filesystem, as the repository-notes loader is allowed to see it.
///
/// Two methods, no recursion, no listing, no globbing: the trait *is* BR-1's
/// bound. A caller cannot ask this seam for a walk because there is nothing on
/// it that walks, and a test can therefore assert on the exact set of paths the
/// loader asked about.
pub trait RepoFileReader {
    /// `lstat` of `path` — the entry as it is spelled, **not** as it resolves.
    ///
    /// This is the one call BR-1 pays for a missing file, and it is what makes
    /// the symlink refusal possible without opening anything.
    ///
    /// # Errors
    ///
    /// [`RepoFileError::NotFound`] or [`RepoFileError::Denied`].
    fn stat(&self, path: &Path) -> Result<FileStat, RepoFileError>;

    /// The contents of the regular file at `path`, read to at most `ceiling`
    /// bytes — **provided it is still `expected`**.
    ///
    /// Over-ceiling is **not** a refusal: a huge file is read to the ceiling and
    /// truncated with a marker, because the top of the file is the part a
    /// repository author puts first (ADR-5).
    ///
    /// `expected` is the identity the caller's [`Self::stat`] answered. An
    /// implementation must refuse [`RepoFileError::Changed`] when the bytes it
    /// would return come from any other inode — that is what makes the entry
    /// rule a guarantee rather than a check with a window behind it, and it is a
    /// term of the *trait* rather than of one impl so a double cannot pass a
    /// loader the production reader would refuse.
    ///
    /// # Errors
    ///
    /// [`RepoFileError`], one variant per user-visible sentence.
    fn read(
        &self,
        path: &Path,
        ceiling: u64,
        expected: FileIdentity,
    ) -> Result<String, RepoFileError>;
}

/// The production [`RepoFileReader`]: the real filesystem.
#[derive(Debug, Clone, Copy, Default)]
pub struct RealFiles;

impl RepoFileReader for RealFiles {
    fn stat(&self, path: &Path) -> Result<FileStat, RepoFileError> {
        // `symlink_metadata`, never `metadata`: the question is about the entry,
        // and `metadata` follows — which would answer about the target and
        // silently pass the check the entry rule exists to make.
        let metadata = std::fs::symlink_metadata(path).map_err(map_io)?;
        Ok(FileStat {
            len: metadata.len(),
            mtime: metadata.modified().ok(),
            is_symlink: metadata.file_type().is_symlink(),
            is_regular: metadata.file_type().is_file(),
            dev: metadata.dev(),
            ino: metadata.ino(),
            nlink: metadata.nlink(),
        })
    }

    fn read(
        &self,
        path: &Path,
        ceiling: u64,
        expected: FileIdentity,
    ) -> Result<String, RepoFileError> {
        // Type checked off `metadata` **before** the open — `fs_util`'s order,
        // for `fs_util`'s reason: a FIFO whose open blocks for a writer must
        // never be opened at all, and no flag on the open can undo that. It
        // follows, which is fine here: it decides nothing, and the two checks
        // below decide everything.
        if !std::fs::metadata(path).map_err(map_io)?.is_file() {
            return Err(RepoFileError::NotRegular);
        }
        // `O_NOFOLLOW`, the `transcript::writer` discipline (REQ-611 BR-9): a
        // symlink at the final component is refused by the kernel rather than
        // followed. `ELOOP` is the answer it gives, `EMLINK` on the rare
        // platform that spells it that way (`install::is_symlink_refusal`), and
        // both are mapped to the *same* verdict the `lstat` reaches so one
        // refusal has one sentence.
        let handle = OpenOptions::new()
            .read(true)
            .custom_flags(libc::O_NOFOLLOW)
            .open(path)
            .map_err(map_open)?;
        // And the half `O_NOFOLLOW` cannot do: the caller's `lstat` was of
        // `<root>/TETON.md` as spelled, this open was of the **canonical** path
        // `ToolContext::resolve` answered, and canonicalizing dereferences a
        // final-component symlink — so by the time the flag sees the path there
        // is no link left at it to refuse. The identity spans both syscalls
        // instead: `(dev, ino)` off the entry `stat`, compared against an
        // `fstat` of this very descriptor.
        //
        // Identity, **not** length. Length is not identity, and comparing it
        // refused the wrong things in both directions: an in-place edit between
        // the two syscalls keeps the inode and is a file the entry rule already
        // judged, while a same-size replacement is a different file wearing the
        // same number.
        let opened = handle.metadata().map_err(map_io)?;
        if !opened.is_file() {
            return Err(RepoFileError::NotRegular);
        }
        if opened.dev() != expected.dev || opened.ino() != expected.ino {
            return Err(RepoFileError::Changed);
        }
        let mut bytes = Vec::new();
        // `take` as well as the ceiling argument, so a file that grows under the
        // read stays bounded. `read_to_end` and then a conversion rather than
        // `read_to_string`, so "not UTF-8" is a verdict rather than an inference
        // from a flattened `io::Error`.
        (&handle)
            .take(ceiling)
            .read_to_end(&mut bytes)
            .map_err(map_io)?;
        decode_at_ceiling(bytes, ceiling)
    }
}

/// `bytes` as text, forgiving **only** a codepoint the read ceiling cut in half.
///
/// `take(ceiling)` cuts on a byte, and a byte is not a character: a perfectly
/// valid `TETON.md` whose 65,536th byte is the middle of an em dash would
/// otherwise be reported [`RepoFileError::NotUtf8`] — the daemon calling the
/// repository's own file corrupt because of where the daemon chose to stop.
///
/// Two conditions, together, and neither alone:
///
/// - **the buffer is exactly at the ceiling**, so this is a file the read cut
///   rather than a file that ended;
/// - **the tail is a genuinely incomplete sequence** — `error_len().is_none()`,
///   which is `str`'s own way of saying "these bytes are a valid *prefix* and
///   more of them would have finished it". Position is not enough: three `0xFF`
///   bytes are also the last three bytes of the buffer, and `0xFF` is not the
///   start of any UTF-8 sequence, so a file that ends in them is not a cut
///   codepoint — it is not text. Verified as well as positioned, because the
///   positional half alone would have recovered exactly that file.
///
/// Anything else is `NotUtf8`, unchanged: a file with a stray `0x80` in the
/// middle is not text, and saying so is the honest answer.
///
/// The recovery keeps the **valid prefix** and never `from_utf8_lossy`: a
/// replacement character would put bytes in the prompt that are not in the file,
/// which is exactly what [`RepoContextFile::text`] must not contain.
fn decode_at_ceiling(bytes: Vec<u8>, ceiling: u64) -> Result<String, RepoFileError> {
    let error = match String::from_utf8(bytes) {
        Ok(text) => return Ok(text),
        Err(error) => error,
    };
    let valid_up_to = error.utf8_error().valid_up_to();
    // `None` means "the input ended in the middle of a sequence that was still
    // valid so far" — precisely the state a byte-aligned cut leaves behind.
    // `Some(n)` means those `n` bytes are wrong wherever they sit.
    let incomplete = error.utf8_error().error_len().is_none();
    let mut bytes = error.into_bytes();
    let at_ceiling = bytes.len() as u64 == ceiling;
    if !(at_ceiling && incomplete) {
        return Err(RepoFileError::NotUtf8);
    }
    debug_assert!(
        bytes.len() - valid_up_to <= 3,
        "an incomplete UTF-8 tail cannot be longer than three bytes"
    );
    bytes.truncate(valid_up_to);
    String::from_utf8(bytes).map_err(|_| RepoFileError::NotUtf8)
}

/// Map an `io::Error` to this module's named answer for an opening call.
///
/// `PermissionDenied` alone is [`RepoFileError::Denied`]; everything else that
/// is not a missing file is [`RepoFileError::Unavailable`], because the two lead
/// a user to different remedies and only one of them can be right.
fn map_io(error: std::io::Error) -> RepoFileError {
    match error.kind() {
        std::io::ErrorKind::NotFound => RepoFileError::NotFound,
        std::io::ErrorKind::PermissionDenied => RepoFileError::Denied,
        _ => RepoFileError::Unavailable,
    }
}

/// [`map_io`], plus the one answer only the `O_NOFOLLOW` open can give.
///
/// `libc::ELOOP` rather than either literal: the number differs by platform (62
/// on macOS, 40 on Linux) and `io::ErrorKind` has no stable value for it, so the
/// platform's own constant is the only spelling that is right on both.
///
/// `EMLINK` beside it because a rare platform reports the same condition that
/// way — the pair `install::is_symlink_refusal` already matches, matched here
/// the same way so one kernel answer does not reach a user as two different
/// sentences depending on which of this daemon's two `O_NOFOLLOW` opens hit it.
fn map_open(error: std::io::Error) -> RepoFileError {
    if matches!(error.raw_os_error(), Some(libc::ELOOP) | Some(libc::EMLINK)) {
        return RepoFileError::Symlink;
    }
    map_io(error)
}

/// A file that was read, and everything a later turn needs to know about it.
///
/// `text` is **stripped** ([`render::strip_for_prompt`]) and **not** truncated:
/// the cap is the route's, so the loader stores what it read and the renderer
/// cuts to whatever figure the route decided (ADR-5).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RepoContextFile {
    /// Which of the two names was read.
    pub source: RepoContextSource,
    /// The canonical path it was read from — the path the jail accepted, which
    /// is the path that was opened.
    pub path: PathBuf,
    /// The root-relative identity minted from that resolution: what a privacy
    /// boundary matches and what egress judges (BR-5).
    pub provenance: ProvenanceId,
    /// The file's text after [`render::strip_for_prompt`], before any cap.
    pub text: String,
    /// The size on disk, as `stat` reported it — the figure `/context` shows
    /// beside the resident bytes so a truncation is legible.
    pub bytes_on_disk: u64,
    /// The `stat` this copy was read under; BR-6's staleness key.
    pub key: FileStat,
}

/// What a session's repository notes are doing (System Model, BR-2).
///
/// Six states, the daemon-side twin of
/// [`RepoContextStateKind`] — which is the wire enum and carries no file. The
/// two that mean "the bytes are in the prompt" carry the file itself, because
/// every later seam (the block, the staleness check, the provenance union) is a
/// function of it; the rest carry only what a surface has to say.
///
/// [`Self::Loaded`] versus [`Self::Truncated`] is decided against
/// [`REPO_CONTEXT_MAX_BYTES`], the widest cap any route can ask for. A floored
/// route renders the same `Loaded` file as a truncated *block*, and the block's
/// [`RepoContextBlock::truncated`] is the route-aware answer — one derivation,
/// asked at the route (ADR-5).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RepoContextState {
    /// Read whole; every byte of it is resident.
    Loaded(RepoContextFile),
    /// Read, and longer than the cap: the block ends with the marker naming the
    /// cap and the bytes dropped.
    Truncated(RepoContextFile),
    /// No candidate at the session root — BR-1's normal case, one `stat` per
    /// candidate and no read.
    ///
    /// Also the answer for a root that is not a `project` (BR-1 reads only a
    /// project root) and for a file that is empty or nothing but whitespace.
    /// Both are "there are no notes here", which is what this state means; the
    /// wire enum has no narrower value and inventing one would put a distinction
    /// on a surface that has no remedy to offer for it.
    Absent,
    /// A privacy boundary covers the file's identity, so it was not made
    /// resident and — see [`RepoContext::load`] — was never read (ADR-2).
    WithheldBoundary {
        /// Which name is on disk. Named, because "which file" is the first
        /// question this state raises.
        source: RepoContextSource,
        /// Its size on disk, from the `stat` that reached it — see
        /// [`Self::bytes_on_disk`].
        ///
        /// Always `Some` here: the boundary verdict is taken after the entry
        /// rule, so a withheld file is always one whose `lstat` succeeded and
        /// said *regular file*. It is an `Option` because it shares the
        /// accessor with [`Self::Unreadable`], where it is not.
        bytes_on_disk: Option<u64>,
    },
    /// The switch is off, durably or for this session: the file was never
    /// opened, and no reader call was made (BR-2).
    WithheldOff,
    /// A file is there and could not be read.
    Unreadable {
        /// Which name is on disk.
        source: RepoContextSource,
        /// The daemon's own words for why.
        ///
        /// `&'static str` **is** the bound BR-1 asks for. The set is closed and
        /// harness-authored, so no path, no OS message and no repository byte
        /// can reach a surface through it — which is the failure mode a
        /// formatted `io::Error` would have.
        reason: &'static str,
        /// Its size on disk, when the `stat` that would have said got an answer
        /// **and** that answer was about a regular file — see
        /// [`Self::bytes_on_disk`].
        bytes_on_disk: Option<u64>,
    },
}

/// The reason for a symlinked entry ([`RepoContextState::Unreadable`]).
const REASON_SYMLINK: &str = "it is a symlink, and a symlinked entry is not followed";
/// The reason for an entry whose inode answers to a second name.
///
/// The symlink sentence's sibling: both name an entry that is a *reference* to
/// bytes somewhere else, and both are refused for that reason (see the module
/// docs). Worded as a fact about the file, because the remedy is the file's —
/// copy it, do not link it.
const REASON_HARDLINK: &str = "it is a hard link";
/// The reason for a file that stopped being the file that was `stat`ed.
///
/// Not [`REASON_NOT_REGULAR`]: what is at the path now may be an ordinary file,
/// and the honest sentence is that it moved under the read rather than that it
/// is the wrong kind of thing.
const REASON_CHANGED: &str = "it changed while it was being read";
/// The reason for an `EPERM`/TCC refusal.
const REASON_DENIED: &str = "it could not be opened (permission denied)";
/// The reason for a directory, FIFO, socket or device wearing the name.
const REASON_NOT_REGULAR: &str = "it is not a regular file";
/// The reason for bytes that are not text.
const REASON_NOT_UTF8: &str = "it is not valid UTF-8";
/// The reason for a filesystem that failed for some other reason entirely.
///
/// Neutral about cause on purpose: the daemon knows the read did not happen and
/// does not know why, and [`REASON_DENIED`] would send the user after a
/// permission that is not the problem. Worded as the *filesystem's* answer
/// rather than as "it could not be read", because the sentence this rides
/// inside already says that — `TETON.md could not be read — it is not resident
/// (the filesystem reported an error)`.
const REASON_UNAVAILABLE: &str = "the filesystem reported an error";
/// The reason for a candidate the jail will not resolve to a file under the
/// root — a root that no longer resolves, or a leaf with no root-relative
/// identity to mint.
const REASON_UNRESOLVED: &str = "it does not resolve to a file inside the session root";

impl RepoContextState {
    /// The wire state this one reports as (TASK-370's enum).
    #[must_use]
    pub fn kind(&self) -> RepoContextStateKind {
        match self {
            Self::Loaded(_) => RepoContextStateKind::Loaded,
            Self::Truncated(_) => RepoContextStateKind::Truncated,
            Self::Absent => RepoContextStateKind::Absent,
            Self::WithheldBoundary { .. } => RepoContextStateKind::WithheldBoundary,
            Self::WithheldOff => RepoContextStateKind::WithheldOff,
            Self::Unreadable { .. } => RepoContextStateKind::Unreadable,
        }
    }

    /// The file, for the two states that have one.
    #[must_use]
    pub fn file(&self) -> Option<&RepoContextFile> {
        match self {
            Self::Loaded(file) | Self::Truncated(file) => Some(file),
            _ => None,
        }
    }

    /// Which name is on disk, for every state that knows.
    ///
    /// `None` for [`Self::Absent`] and [`Self::WithheldOff`]: `off` never opened
    /// anything, so the daemon does not know which of the two names is there and
    /// must not imply that it does.
    #[must_use]
    pub fn source(&self) -> Option<RepoContextSource> {
        match self {
            Self::Loaded(file) | Self::Truncated(file) => Some(file.source),
            Self::WithheldBoundary { source, .. } | Self::Unreadable { source, .. } => {
                Some(*source)
            }
            Self::Absent | Self::WithheldOff => None,
        }
    }

    /// The file's size on disk, for every state that got an answer about it.
    ///
    /// **A withheld or unreadable file still has a size**, and every surface
    /// that reports one reads it here. Dropping it would make `/context` print
    /// `0 bytes on disk` for a file the user can see — a figure that is not
    /// merely uninformative but wrong, and the first thing they would check.
    ///
    /// `None` where the daemon genuinely does not know, and the set is exactly:
    ///
    /// - [`Self::Absent`] and [`Self::WithheldOff`] — nothing was `stat`ed, or
    ///   nothing is there;
    /// - the [`Self::Unreadable`] refusals whose `stat` **failed**, which had no
    ///   answer to keep;
    /// - the two whose `stat` succeeded about something that is not a regular
    ///   file — a symlinked entry, a directory. `lstat`'s `len` for those is the
    ///   target path's length or a directory block, not a file's size, and
    ///   reporting it as "bytes on disk" would be a number that means nothing
    ///   dressed as one that means something.
    #[must_use]
    pub fn bytes_on_disk(&self) -> Option<u64> {
        match self {
            Self::Loaded(file) | Self::Truncated(file) => Some(file.bytes_on_disk),
            Self::WithheldBoundary { bytes_on_disk, .. }
            | Self::Unreadable { bytes_on_disk, .. } => *bytes_on_disk,
            Self::Absent | Self::WithheldOff => None,
        }
    }

    /// Whether `other` says the same thing to a user as `self` — everything but
    /// the staleness key.
    ///
    /// **A `touch` is not news.** [`RefreshVerdict::Reload`]'s `always_store` is
    /// a fact about the *key*: an `mtime` that moved must be stored whatever the
    /// bytes turn out to be, or the re-read repeats on every turn thereafter.
    /// Whether the answer is worth **announcing** is a different question, and
    /// full equality cannot ask it — [`FileStat`] is a field of the state, so a
    /// bare `touch` compares unequal while nothing a client is shown has moved.
    /// This is the comparison the publish gate wants; `==` is the one the store
    /// wants, and they are deliberately not the same.
    ///
    /// A same-length edit *is* news and is caught here, because the file's
    /// `text` is compared: only the key is excluded, not the content.
    ///
    /// # Adding a field to [`RepoContextFile`]
    ///
    /// It belongs in the comparison below unless it is another fact about
    /// *when* the copy was read. The exhaustive destructuring is what makes the
    /// compiler ask.
    #[must_use]
    pub fn same_news(&self, other: &Self) -> bool {
        let (a, b) = match (self, other) {
            (Self::Loaded(a), Self::Loaded(b)) | (Self::Truncated(a), Self::Truncated(b)) => (a, b),
            // Every other pairing — including `Loaded` against `Truncated` —
            // carries no key to exclude, so full equality is the answer.
            _ => return self == other,
        };
        let RepoContextFile {
            source,
            path,
            provenance,
            text,
            bytes_on_disk,
            key: _,
        } = a;
        source == &b.source
            && path == &b.path
            && provenance == &b.provenance
            && text == &b.text
            && bytes_on_disk == &b.bytes_on_disk
    }
}

/// The loader (ADR-3): one file, one place, two entry points.
///
/// A namespace rather than a value. ADR-3 spells the second entry point
/// `refresh(&self, ..)`; it is [`Self::verdict`], taking the current state by
/// reference, because a wrapper struct holding a `RepoContextState` would be a
/// second place the session's state lives and the session record (TASK-373) is
/// the first — and it answers a verdict rather than a state, because the re-read
/// a stale key implies is a blocking call the caller has to place (see there).
pub struct RepoContext;

impl RepoContext {
    /// Read the session's repository notes, or name why they were not read
    /// (BR-1, BR-2, BR-5).
    ///
    /// The order of the gates is the requirement's, and each one is before the
    /// step it makes unnecessary:
    ///
    /// 1. **the switch** — off means *unopened* (BR-2), so this returns before
    ///    touching `files` at all;
    /// 2. **the root kind** — only a `project` root is read (BR-1), so a `home`
    ///    or `plain` root costs no `stat` either;
    /// 3. **one `stat` per candidate**, in [`CANDIDATE_NAMES`] order, on the path
    ///    as spelled — a missing file stops here, which is BR-1's normal case;
    /// 4. **the entry rule** — a symlink, a hardlink or a non-regular file is
    ///    named `Unreadable` and never opened;
    /// 5. **the jail and the mint** — [`ToolContext::resolve`], one call for both
    ///    halves of the identity (REQ-571 ADR-B);
    /// 6. **the boundary** — a covered identity is withheld **before** the read,
    ///    so a `local-only` file's bytes never enter the daemon's memory at all
    ///    (ADR-2 asks for the block to be withheld; not reading it is the
    ///    stronger property and costs nothing);
    /// 7. **the read**, bounded by [`REPO_CONTEXT_READ_CEILING_BYTES`] and
    ///    checked against the [`FileIdentity`] step 3 answered, so the bytes
    ///    that come back are the inode step 4 judged.
    ///
    /// A present-but-empty (or whitespace-only) file is [`RepoContextState::Absent`]
    /// and does **not** fall through to the next candidate. The contest is decided
    /// on existence at step 3; a second rule that re-opened it on content would
    /// mean a repository could not turn its notes off by emptying the file it
    /// already has.
    pub fn load(
        root: &ProbedRoot,
        boundaries: &BoundaryMatcher<'_>,
        enabled: bool,
        files: &dyn RepoFileReader,
    ) -> RepoContextState {
        if !enabled {
            return RepoContextState::WithheldOff;
        }
        if root.view.kind != RootKind::Project {
            return RepoContextState::Absent;
        }
        let jail = ToolContext::for_root(root);
        for (name, source) in CANDIDATES {
            let spelled = root.path.join(name);
            let key = match files.stat(&spelled) {
                Ok(key) => key,
                Err(RepoFileError::NotFound) => continue,
                // No `key`, so no size to report: the `stat` is what would have
                // said, and it is what failed.
                Err(other) => return unreadable(source, reason_for(other), None),
            };
            // Past this point the `stat` answered, and from the two checks below
            // onward it answered about a *regular file* — which is the condition
            // under which `key.len` is a size worth showing anyone.
            if key.is_symlink {
                return unreadable(source, REASON_SYMLINK, None);
            }
            if !key.is_regular {
                return unreadable(source, REASON_NOT_REGULAR, None);
            }
            let on_disk = Some(key.len);
            // A hardlink is a regular file and not a symlink, so neither check
            // above sees it and `O_NOFOLLOW` has nothing to refuse — see the
            // module docs. `lstat` already counted the names; this is the whole
            // of the refusal. The size is reported because it is a real regular
            // file's real size, unlike a symlink's `lstat` length.
            if key.nlink > 1 {
                return unreadable(source, REASON_HARDLINK, on_disk);
            }
            // The jail's own resolution, and the identity it mints — never a
            // second parse of the same path (LESSON-494, LESSON-623).
            let Ok(resolved) = jail.resolve(name) else {
                return unreadable(source, REASON_UNRESOLVED, on_disk);
            };
            if boundaries
                .match_path(resolved.provenance.as_str())
                .is_some()
            {
                return RepoContextState::WithheldBoundary {
                    source,
                    // The one figure a withheld state may carry, and it is not
                    // the file's content: BR-5 withholds the *bytes*, and a size
                    // the user can read off `ls` is what makes the withholding
                    // legible rather than mysterious.
                    bytes_on_disk: on_disk,
                };
            }
            // The **identity** the `stat` above answered travels into the read,
            // not the path it answered about: `resolved.path` is canonical, and
            // canonicalizing is what dereferences a symlink planted since the
            // `stat`. This is the seam that makes the entry rule hold end to
            // end (see the module docs).
            let text = match files.read(
                &resolved.path,
                REPO_CONTEXT_READ_CEILING_BYTES,
                key.identity(),
            ) {
                Ok(text) => text,
                Err(other) => return unreadable(source, reason_for(other), on_disk),
            };
            // Stripped here as well as in the renderer (BR-4, ADR-4: before the
            // cap is measured). The pass is deletion-only and idempotent, so the
            // second one over the same text is a no-op; doing it here is what
            // makes the `Loaded`/`Truncated` decision below a decision about
            // bytes that will actually reach the prompt.
            let text = render::strip_for_prompt(&text).into_owned();
            if text.trim().is_empty() {
                return RepoContextState::Absent;
            }
            let file = RepoContextFile {
                source,
                path: resolved.path,
                provenance: resolved.provenance,
                text,
                bytes_on_disk: key.len,
                key,
            };
            // Asked of the renderer rather than by comparing lengths here: the
            // cap is measured on the sanitized text the block carries, and one
            // derivation of "does this fit" is the whole reason `truncated` is a
            // field on the block (ADR-5).
            return if RepoContextBlock::render(&file, REPO_CONTEXT_MAX_BYTES).truncated {
                RepoContextState::Truncated(file)
            } else {
                RepoContextState::Loaded(file)
            };
        }
        RepoContextState::Absent
    }

    /// BR-6's start-of-turn staleness check: what the session's state has to
    /// become, or [`RefreshVerdict::Unchanged`] when it has not.
    ///
    /// `Unchanged` is the answer that has to be cheap, because it is the answer
    /// on every turn of every session: one `stat` and a glob match, no read, no
    /// allocation of the file's bytes. It is returned only when **both** halves
    /// of the verdict still hold — the `mtime`/`len` key is identical *and* the
    /// boundary set still does not cover the identity (OQ-4: a boundary added
    /// mid-session drops the block at the next prompt).
    ///
    /// A state with no stored key ([`RepoContextState::Absent`],
    /// `WithheldBoundary`, `Unreadable`) asks the caller to re-run [`Self::load`]
    /// and to report only a *difference*. That is one or two `stat`s and still no
    /// read — none of those paths reaches the read — and it is what lets a
    /// `TETON.md` created mid-session become resident at the next prompt.
    ///
    /// **A newly created `TETON.md` beside a loaded `AGENTS.md` waits for the
    /// next `session/create` or `/cd`.** Seeing it would cost a second `stat` on
    /// every turn of every session carrying a fallback file, and ADR-3's budget
    /// for this check is one.
    ///
    /// # The key is `mtime` + `len`, and `mtime` has a granularity
    ///
    /// Two edits inside one filesystem timestamp tick that leave the file the
    /// same length are indistinguishable here, and the second one is not
    /// resident until something else moves the key. HFS+ stamps whole seconds;
    /// APFS and ext4 are nanosecond-resolution and do not have the problem in
    /// practice. It is the same key `read`'s own staleness check uses, and the
    /// alternative — hashing up to 64 KiB on every turn of every session — is
    /// far past ADR-3's budget of one `stat` for the quiet answer. A user who
    /// hits it is one `/cd .` (or one more edit) away from the new bytes.
    ///
    /// # It decides; it does not re-read
    ///
    /// The answer is a [`RefreshVerdict`], and the re-load a
    /// [`RefreshVerdict::Reload`] names is the **caller's** to run. The two
    /// halves cost different things and therefore run in different places: the
    /// decision is one `stat` and a glob match, taken inline on the turn path,
    /// while the re-load can open and read 64 KiB of a user-controlled path and
    /// on macOS raise a TCC dialog that blocks for as long as the user takes to
    /// answer it — so the caller takes `block_in_place_if_multithread` for that
    /// branch and nothing else (BUG-184's rule).
    ///
    /// There is deliberately **no** composed `refresh(..) -> Option<..>` beside
    /// this. It had exactly one caller, that caller now needs the halves apart,
    /// and a composition nothing in production runs is a composition its own
    /// test agrees with while production drifts (LESSON-451). The four lines
    /// that compose it live at the call site, where the blocking wrap is.
    ///
    /// # The gates before the key comparison
    ///
    /// They are the ones a session can walk out from under mid-flight:
    ///
    /// - **the switch** (BR-2) — off means unopened, on every path in here;
    /// - **the root kind** — a session whose root stopped being a `project`
    ///   re-derives, because BR-1 reads only a project root and the stored file
    ///   was read under one that was;
    /// - **the root itself** — a stored file that is no longer *at* the
    ///   probed root is a file from the repository the session has left, and
    ///   `/repo/sub`'s notes are not `/repo`'s. The
    ///   `stat` below would happily find a same-named file at the new root and
    ///   compare it against the old one's key, which is how a `/cd` that raced
    ///   the rebuild would keep the old repository's bytes resident.
    pub fn verdict(
        current: &RepoContextState,
        root: &ProbedRoot,
        boundaries: &BoundaryMatcher<'_>,
        enabled: bool,
        files: &dyn RepoFileReader,
    ) -> RefreshVerdict {
        if !enabled {
            // Off means unopened, on every path into this function.
            return match current {
                RepoContextState::WithheldOff => RefreshVerdict::Unchanged,
                _ => RefreshVerdict::Settled(RepoContextState::WithheldOff),
            };
        }
        let (RepoContextState::Loaded(file) | RepoContextState::Truncated(file)) = current else {
            return RefreshVerdict::Reload {
                always_store: false,
            };
        };
        // The two facts about the *root* that the stored key cannot speak for.
        // Both fall through to a full re-load, which answers `Absent` for a
        // non-project root and re-reads at the root the session now stands on.
        if root.view.kind != RootKind::Project || !under_root(&file.path, &root.path) {
            return RefreshVerdict::Reload {
                always_store: false,
            };
        }
        let spelled = root.path.join(file_name(file.source));
        match files.stat(&spelled) {
            // `mtime.is_some()` is part of the equality on purpose: two `None`s
            // compare equal, and a filesystem that reports no timestamp would
            // otherwise pin the first copy read for the life of the session.
            Ok(key) if key == file.key && key.mtime.is_some() => {
                if boundaries.match_path(file.provenance.as_str()).is_some() {
                    RefreshVerdict::Settled(RepoContextState::WithheldBoundary {
                        source: file.source,
                        bytes_on_disk: Some(key.len),
                    })
                } else {
                    RefreshVerdict::Unchanged
                }
            }
            // Any change to the key — and any `stat` that now fails — is a full
            // re-load, so the state that comes back carries the new key. Compare
            // the *states* and a `touch` with no edit would leave the stale key
            // stored and re-read on every turn thereafter, which is what
            // `always_store` is for.
            //
            // The re-load `stat`s again rather than being handed the key just
            // read, and that second syscall buys the whole gate chain: a file
            // that became a symlink, a non-regular file or a boundary-covered
            // one between two turns is judged by exactly the rules that judged
            // it at `session/create`. One `stat` is the budget for the *quiet*
            // turn (ADR-3); a turn where the notes actually changed is already
            // paying for a read.
            _ => RefreshVerdict::Reload { always_store: true },
        }
    }
}

/// What [`RepoContext::verdict`] decided, with the expensive half named rather
/// than taken.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RefreshVerdict {
    /// The stored state still holds: one `stat`, no read, nothing to publish.
    Unchanged,
    /// A new state, reached **without** opening anything — the switch went off,
    /// or a boundary came to cover a file that was already resident.
    Settled(RepoContextState),
    /// The gates have to run again, and doing so may open the file.
    ///
    /// `always_store` when the stored **key** moved: the re-load's answer
    /// replaces the stored state even when the two compare equal, so a `touch`
    /// with no edit does not leave the stale key behind to be re-read on every
    /// turn thereafter.
    Reload {
        /// See above.
        always_store: bool,
    },
}

/// Whether `path` — a canonical path the jail resolved — is a file **directly
/// at** `root` as the session spells it.
///
/// The parent must *equal* the root, not merely start with it. BR-1 reads one
/// file at one place, and the place is the root itself: a session at
/// `/repo/sub` holding `/repo/sub/TETON.md` that `/cd`s to `/repo` is standing
/// somewhere else, and `starts_with` — the shape this replaced — called that
/// "still under the root". The `stat` below then found `/repo/TETON.md`,
/// compared it against the key of a *different* file, and answered `Unchanged`
/// whenever the two happened to share an `mtime` and a length: the parent
/// repository's notes never became resident.
///
/// [`ToolContext::resolve`] canonicalizes the root before it resolves, so the
/// stored path's parent is `root.canonicalize()` and **not** necessarily `root`
/// as written: `/tmp` is `/private/tmp` on macOS, and a root reached through any
/// symlink has the same shape. The textual comparison is tried first and answers
/// for every root that is already canonical, which is the ordinary one; the
/// `realpath` behind it runs only for a root spelled through a link, and only to
/// save that turn a full re-read.
fn under_root(path: &Path, root: &Path) -> bool {
    let Some(parent) = path.parent() else {
        return false;
    };
    parent == root
        || root
            .canonicalize()
            .is_ok_and(|canonical| parent == canonical)
}

/// One spelling of the `Unreadable` construction, so every refusal above is one
/// line and the reason set stays visibly closed.
fn unreadable(
    source: RepoContextSource,
    reason: &'static str,
    bytes_on_disk: Option<u64>,
) -> RepoContextState {
    RepoContextState::Unreadable {
        source,
        reason,
        bytes_on_disk,
    }
}

/// The daemon's sentence for a seam error — one `match`, so a reader-side answer
/// and the words a user sees cannot come apart.
///
/// [`RepoFileError::NotFound`] is [`REASON_UNRESOLVED`] rather than a refusal of
/// its own: the only call that reaches this with it is the *read*, whose path
/// was resolved by the jail moments earlier, so a file that is now missing is a
/// file that stopped resolving.
fn reason_for(error: RepoFileError) -> &'static str {
    match error {
        RepoFileError::NotFound => REASON_UNRESOLVED,
        RepoFileError::Denied => REASON_DENIED,
        RepoFileError::NotRegular => REASON_NOT_REGULAR,
        RepoFileError::NotUtf8 => REASON_NOT_UTF8,
        RepoFileError::Symlink => REASON_SYMLINK,
        RepoFileError::Changed => REASON_CHANGED,
        RepoFileError::Unavailable => REASON_UNAVAILABLE,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeMap;
    use std::sync::Mutex;
    use teton_core::entities::{BoundaryMode, PrivacyBoundary};
    use teton_protocol::methods::SessionRoot;

    /// What the fixture serves for one planted path.
    struct Planted {
        stat: Result<FileStat, RepoFileError>,
        read: Result<String, RepoFileError>,
    }

    /// The identity every [`Planted::regular`] file wears.
    ///
    /// Fixed rather than minted per fixture, and that is load-bearing: the
    /// staleness check compares whole [`FileStat`]s, so a counter here would
    /// make two identical plantings of one file compare unequal and turn every
    /// `Unchanged` leg below into a re-load. A test that wants a *mismatch*
    /// says so ([`Planted::with_identity`]).
    const PLANTED_IDENTITY: FileIdentity = FileIdentity { dev: 1, ino: 2 };

    impl Planted {
        /// A regular file of `text`, with `mtime` as the staleness key's stamp.
        fn regular(text: &str, mtime_secs: u64) -> Self {
            Self {
                stat: Ok(FileStat {
                    len: text.len() as u64,
                    mtime: Some(
                        SystemTime::UNIX_EPOCH + std::time::Duration::from_secs(mtime_secs),
                    ),
                    is_symlink: false,
                    is_regular: true,
                    dev: PLANTED_IDENTITY.dev,
                    ino: PLANTED_IDENTITY.ino,
                    nlink: 1,
                }),
                read: Ok(text.to_owned()),
            }
        }

        /// The same file, `stat`ing as `nlink` names — a hardlink at 2.
        fn with_links(mut self, nlink: u64) -> Self {
            if let Ok(stat) = &mut self.stat {
                stat.nlink = nlink;
            }
            self
        }

        /// A symlinked entry: `lstat` says link, and the read would succeed —
        /// which is the point. A loader that skipped the entry rule would get an
        /// answer rather than a refusal, so this fixture can tell the two apart.
        fn symlink() -> Self {
            Self {
                stat: Ok(FileStat {
                    len: 12,
                    mtime: Some(SystemTime::UNIX_EPOCH),
                    is_symlink: true,
                    is_regular: false,
                    dev: PLANTED_IDENTITY.dev,
                    ino: PLANTED_IDENTITY.ino,
                    nlink: 1,
                }),
                read: Ok("followed it\n".to_owned()),
            }
        }

        /// An `EPERM`/TCC refusal at the `stat`.
        fn denied() -> Self {
            Self {
                stat: Err(RepoFileError::Denied),
                read: Err(RepoFileError::Denied),
            }
        }
    }

    /// A [`RepoFileReader`] that serves planted answers and records every call.
    ///
    /// The recording is compared for **equality** rather than containment, the
    /// `DirLister` rule: a reach claim cannot be passed by a loader that also
    /// went somewhere else.
    struct Recorded {
        files: BTreeMap<PathBuf, Planted>,
        calls: Mutex<Vec<String>>,
        served: Mutex<u64>,
    }

    impl Recorded {
        fn new(files: Vec<(PathBuf, Planted)>) -> Self {
            Self {
                files: files.into_iter().collect(),
                calls: Mutex::new(Vec::new()),
                served: Mutex::new(0),
            }
        }

        /// The recorded calls, with the root's own prefix stripped so an
        /// assertion reads as the relative paths BR-1 talks about.
        fn calls(&self, root: &Path) -> Vec<String> {
            self.calls
                .lock()
                .unwrap()
                .iter()
                .map(|call| call.replace(&format!("{}/", root.display()), ""))
                .collect()
        }

        fn forget(&self) {
            self.calls.lock().unwrap().clear();
        }

        fn served(&self) -> u64 {
            *self.served.lock().unwrap()
        }
    }

    impl RepoFileReader for Recorded {
        fn stat(&self, path: &Path) -> Result<FileStat, RepoFileError> {
            self.calls
                .lock()
                .unwrap()
                .push(format!("stat {}", path.display()));
            match self.files.get(path) {
                Some(planted) => planted.stat,
                None => Err(RepoFileError::NotFound),
            }
        }

        fn read(
            &self,
            path: &Path,
            ceiling: u64,
            expected: FileIdentity,
        ) -> Result<String, RepoFileError> {
            self.calls
                .lock()
                .unwrap()
                .push(format!("read {}", path.display()));
            let planted = match self.files.get(path) {
                Some(planted) => planted,
                None => return Err(RepoFileError::NotFound),
            };
            // The seam's *other* contract, honoured by the double for the same
            // reason the ceiling is: a loader that stopped carrying the entry's
            // identity forward would be handed bytes here rather than a refusal,
            // and the fixture would agree with a production reader that does not.
            if planted.stat.is_ok_and(|stat| stat.identity() != expected) {
                return Err(RepoFileError::Changed);
            }
            let text = planted.read.clone()?;
            // The seam's contract, honoured by the double: at most `ceiling`
            // bytes leave it, and the count is what the ceiling test asserts on.
            let mut end = usize::try_from(ceiling)
                .unwrap_or(usize::MAX)
                .min(text.len());
            while !text.is_char_boundary(end) {
                end -= 1;
            }
            *self.served.lock().unwrap() += end as u64;
            Ok(text[..end].to_owned())
        }
    }

    /// A real, canonical directory to stand the session on, and the probed root
    /// naming it.
    ///
    /// Real because [`ToolContext::resolve`] is the production resolver and
    /// resolves against the filesystem (see the module docs: scripting it would
    /// be scripting the check). Canonical because the loader `stat`s the path as
    /// spelled and reads the path as resolved, and a test that wants to assert
    /// on one set of strings needs those two to be the same spelling.
    ///
    /// The kind is set directly rather than probed, so the fixture does not
    /// depend on which markers `session_root::probe` looks for today.
    fn project_root(tag: &str, kind: RootKind) -> (PathBuf, ProbedRoot) {
        use std::sync::atomic::{AtomicU64, Ordering};
        static NEXT: AtomicU64 = AtomicU64::new(0);
        let dir = std::env::temp_dir().join(format!(
            "teton-repo-context-{tag}-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let path = std::fs::canonicalize(&dir).unwrap();
        let probed = ProbedRoot {
            path: path.clone(),
            view: SessionRoot {
                display: "~/repo".to_owned(),
                kind,
                project_name: Some("repo".to_owned()),
                vcs_branch: None,
            },
        };
        (dir, probed)
    }

    /// No boundaries at all — the ordinary session.
    fn no_boundaries() -> Vec<PrivacyBoundary> {
        Vec::new()
    }

    /// BR-1: the contest is `TETON.md` then `AGENTS.md`, decided at the first
    /// `stat`, and nothing at all is opened under a root that is not a project.
    ///
    /// Mutation: reversing [`CANDIDATES`] loads the `AGENTS.md` text and fails
    /// the source and text assertions; deleting the `kind != Project` guard
    /// makes the three non-project legs record two `stat`s each and load the
    /// file, failing both the state and the emptiness assertions.
    #[test]
    fn only_a_project_root_is_read_and_teton_md_wins_over_agents_md() {
        // The two spellings of one fact, checked before anything leans on them.
        for (name, source) in CANDIDATES {
            assert_eq!(file_name(source), name, "the name/source pairing drifted");
        }

        let (dir, root) = project_root("contest", RootKind::Project);
        let planted = || {
            vec![
                (
                    root.path.join("TETON.md"),
                    Planted::regular("teton notes\n", 10),
                ),
                (
                    root.path.join("AGENTS.md"),
                    Planted::regular("agents notes\n", 10),
                ),
            ]
        };
        let boundaries = no_boundaries();
        let matcher = BoundaryMatcher::new(&boundaries).unwrap();

        let files = Recorded::new(planted());
        let state = RepoContext::load(&root, &matcher, true, &files);
        let file = state.file().expect("a project root with a TETON.md loads");
        assert_eq!(state.kind(), RepoContextStateKind::Loaded);
        assert_eq!(file.source, RepoContextSource::TetonMd);
        assert_eq!(file.text, "teton notes\n");
        assert_eq!(file.provenance.as_str(), "TETON.md");
        assert_eq!(
            files.calls(&root.path),
            vec!["stat TETON.md".to_owned(), "read TETON.md".to_owned()],
            "the winner's stat and read are the only calls — AGENTS.md is never asked about"
        );

        // The fallback is reached only when the first name is not there.
        let files = Recorded::new(vec![(
            root.path.join("AGENTS.md"),
            Planted::regular("agents notes\n", 10),
        )]);
        let state = RepoContext::load(&root, &matcher, true, &files);
        assert_eq!(state.source(), Some(RepoContextSource::AgentsMd));
        assert_eq!(state.file().unwrap().text, "agents notes\n");

        // BR-1: only a project root is read, even with both files present.
        for kind in [RootKind::Home, RootKind::FilesystemRoot, RootKind::Plain] {
            let (_, other) = project_root("kind", kind);
            let files = Recorded::new(vec![
                (other.path.join("TETON.md"), Planted::regular("x\n", 10)),
                (other.path.join("AGENTS.md"), Planted::regular("y\n", 10)),
            ]);
            let state = RepoContext::load(&other, &matcher, true, &files);
            assert_eq!(state, RepoContextState::Absent, "{kind:?} was read");
            assert!(
                files.calls(&other.path).is_empty(),
                "{kind:?} cost a filesystem call: {:?}",
                files.calls(&other.path)
            );
            std::fs::remove_dir_all(&other.path).ok();
        }
        std::fs::remove_dir_all(&dir).ok();
    }

    /// BR-1: a symlinked entry and an `EPERM` are named rather than silent, and
    /// a missing file costs exactly one `stat` per candidate and no read.
    ///
    /// Mutation: dropping the `key.is_symlink` refusal turns the first leg into
    /// a `Loaded` carrying `followed it`, failing both the state and the
    /// no-read assertion; mapping a `stat` error to `continue` turns the second
    /// leg into `Absent`.
    #[test]
    fn a_symlinked_entry_and_an_eperm_are_named_unreadable_and_a_missing_file_is_absent_after_one_stat(
    ) {
        let (dir, root) = project_root("refusals", RootKind::Project);
        let boundaries = no_boundaries();
        let matcher = BoundaryMatcher::new(&boundaries).unwrap();

        let files = Recorded::new(vec![(root.path.join("TETON.md"), Planted::symlink())]);
        assert_eq!(
            RepoContext::load(&root, &matcher, true, &files),
            RepoContextState::Unreadable {
                source: RepoContextSource::TetonMd,
                reason: REASON_SYMLINK,
                // A symlink's `lstat` length is its target path, not a file
                // size, so the state reports none.
                bytes_on_disk: None,
            }
        );
        assert_eq!(
            files.calls(&root.path),
            vec!["stat TETON.md".to_owned()],
            "a symlinked entry was followed"
        );

        // **Verify (HIGH).** A hardlinked entry is refused by the entry rule and
        // never opened — the property `RealFiles`' own leg cannot assert,
        // because only a recording seam can say a read did not happen.
        let files = Recorded::new(vec![(
            root.path.join("TETON.md"),
            Planted::regular("linked bytes\n", 10).with_links(2),
        )]);
        assert_eq!(
            RepoContext::load(&root, &matcher, true, &files),
            RepoContextState::Unreadable {
                source: RepoContextSource::TetonMd,
                reason: REASON_HARDLINK,
                bytes_on_disk: Some("linked bytes\n".len() as u64),
            }
        );
        assert_eq!(
            files.calls(&root.path),
            vec!["stat TETON.md".to_owned()],
            "a hardlinked entry was opened"
        );

        let files = Recorded::new(vec![(root.path.join("TETON.md"), Planted::denied())]);
        assert_eq!(
            RepoContext::load(&root, &matcher, true, &files),
            RepoContextState::Unreadable {
                source: RepoContextSource::TetonMd,
                reason: REASON_DENIED,
                // The `stat` itself failed, so there is no size to report.
                bytes_on_disk: None,
            }
        );
        assert_eq!(files.calls(&root.path), vec!["stat TETON.md".to_owned()]);

        // The normal case: one stat per candidate, in order, and no read.
        let files = Recorded::new(Vec::new());
        assert_eq!(
            RepoContext::load(&root, &matcher, true, &files),
            RepoContextState::Absent
        );
        assert_eq!(
            files.calls(&root.path),
            vec!["stat TETON.md".to_owned(), "stat AGENTS.md".to_owned()],
        );

        // An empty file is not notes, and does not reopen the contest.
        let files = Recorded::new(vec![
            (root.path.join("TETON.md"), Planted::regular("  \n\t\n", 10)),
            (root.path.join("AGENTS.md"), Planted::regular("notes\n", 10)),
        ]);
        assert_eq!(
            RepoContext::load(&root, &matcher, true, &files),
            RepoContextState::Absent
        );
        std::fs::remove_dir_all(&dir).ok();
    }

    /// BR-3: a file far past the ceiling is read to
    /// [`REPO_CONTEXT_READ_CEILING_BYTES`] and no further — the fixture counts
    /// the bytes it served.
    ///
    /// Mutation: passing `u64::MAX` (or the file's own length) as the ceiling
    /// serves 10,485,760 bytes and fails the first assertion; dropping the
    /// `take` in [`RealFiles::read`] is the production twin of the same slip and
    /// is why the seam's contract is asserted here rather than the loader's
    /// stored length alone.
    #[test]
    fn the_read_stops_at_the_read_ceiling() {
        let (dir, root) = project_root("ceiling", RootKind::Project);
        let boundaries = no_boundaries();
        let matcher = BoundaryMatcher::new(&boundaries).unwrap();

        // 10 MiB of 64-byte lines.
        let line = format!("{}\n", "g".repeat(63));
        let huge = line.repeat(10 * 1024 * 1024 / 64);
        assert_eq!(huge.len(), 10 * 1024 * 1024, "the fixture is not 10 MiB");
        let files = Recorded::new(vec![(
            root.path.join("TETON.md"),
            Planted::regular(&huge, 10),
        )]);

        let state = RepoContext::load(&root, &matcher, true, &files);
        assert_eq!(
            files.served(),
            REPO_CONTEXT_READ_CEILING_BYTES,
            "the read did not stop at the ceiling"
        );
        assert_eq!(state.kind(), RepoContextStateKind::Truncated);
        let file = state.file().unwrap();
        assert_eq!(file.text.len() as u64, REPO_CONTEXT_READ_CEILING_BYTES);
        assert_eq!(
            file.bytes_on_disk,
            10 * 1024 * 1024,
            "the size on disk is the file's, not the read's"
        );
        std::fs::remove_dir_all(&dir).ok();
    }

    /// BR-5: a boundary-covered identity is withheld **and never read**; an
    /// uncovered one carries the identity `ProvenanceId::from_resolved` mints
    /// for the path that was opened; and the switch off costs zero calls.
    ///
    /// Mutation: moving the boundary check after the read keeps the state right
    /// and fails the recorded-calls assertion — which is the assertion that
    /// matters, since the failure being guarded against is the file's bytes
    /// entering the daemon at all. Making `load` mint from `root.path.join(name)`
    /// instead of the resolved path passes on this fixture and is why the
    /// oracle below is `from_resolved` on the canonical pair rather than a
    /// literal.
    #[test]
    fn a_boundary_covered_file_is_withheld_and_an_uncovered_one_mints_its_identity() {
        let (dir, root) = project_root("boundary", RootKind::Project);
        let planted = || {
            vec![(
                root.path.join("TETON.md"),
                Planted::regular("secret layout\n", 10),
            )]
        };

        let covered = vec![PrivacyBoundary::user("TETON.md", BoundaryMode::LocalOnly)];
        let matcher = BoundaryMatcher::new(&covered).unwrap();
        let files = Recorded::new(planted());
        assert_eq!(
            RepoContext::load(&root, &matcher, true, &files),
            RepoContextState::WithheldBoundary {
                source: RepoContextSource::TetonMd,
                // BR-3/verify: a withheld file still has a size, and it is the
                // `stat`'s — the one figure a state that read nothing can give.
                bytes_on_disk: Some("secret layout\n".len() as u64),
            }
        );
        assert_eq!(
            files.calls(&root.path),
            vec!["stat TETON.md".to_owned()],
            "a covered file was read"
        );

        let open = no_boundaries();
        let matcher = BoundaryMatcher::new(&open).unwrap();
        let files = Recorded::new(planted());
        let state = RepoContext::load(&root, &matcher, true, &files);
        let file = state.file().expect("an uncovered file loads");
        assert_eq!(
            file.provenance,
            ProvenanceId::from_resolved(&root.path, &root.path.join("TETON.md")).unwrap(),
        );

        // BR-2: off is unopened.
        let files = Recorded::new(planted());
        assert_eq!(
            RepoContext::load(&root, &matcher, false, &files),
            RepoContextState::WithheldOff
        );
        assert!(
            files.calls(&root.path).is_empty(),
            "the switch off still touched the filesystem: {:?}",
            files.calls(&root.path)
        );
        std::fs::remove_dir_all(&dir).ok();
    }

    /// BR-6: the staleness check re-reads only when the `mtime`/`len` key moved
    /// or the boundary verdict changed, and its quiet answer costs one `stat`.
    ///
    /// Driven through [`RepoContext::verdict`] and — where the verdict asks for
    /// one — [`RepoContext::load`], which is the pair the turn path runs; the
    /// local `settle` below is those two lines and nothing else, so it cannot
    /// hide a branch production does not have.
    ///
    /// Mutation: dropping `key == file.key` from the guard answers `Unchanged`
    /// forever and fails the edited leg; dropping the boundary re-check answers
    /// `Unchanged` on the covered leg and leaves a `local-only` file resident,
    /// which is OQ-4's hole; deleting the root-kind or at-the-root gate makes
    /// the two `/cd` legs answer `Unchanged` and keep the departed repository's
    /// bytes.
    ///
    /// **Verify (MINOR 4):** restoring `path.starts_with(root)` in
    /// [`under_root`] makes the `/repo/sub` → `/repo` leg answer `Unchanged`
    /// and leave the subdirectory's file resident at the parent root.
    /// **Verify (MINOR 3):** making [`RepoContextState::same_news`] full
    /// equality makes the `touch` leg fail — a re-read that changed nothing a
    /// client is shown reads as news, and the publish gate above it announces
    /// it on every touched turn.
    #[test]
    fn refresh_reads_only_when_mtime_len_or_verdict_changed() {
        let (dir, root) = project_root("refresh", RootKind::Project);
        let open = no_boundaries();
        let matcher = BoundaryMatcher::new(&open).unwrap();
        let path = root.path.join("TETON.md");

        /// The turn path's own composition: decide, then re-load if the verdict
        /// asked for one. `None` is "the stored state still holds".
        fn settle(
            current: &RepoContextState,
            root: &ProbedRoot,
            matcher: &BoundaryMatcher<'_>,
            enabled: bool,
            files: &dyn RepoFileReader,
        ) -> Option<RepoContextState> {
            match RepoContext::verdict(current, root, matcher, enabled, files) {
                RefreshVerdict::Unchanged => None,
                RefreshVerdict::Settled(state) => Some(state),
                RefreshVerdict::Reload { always_store } => {
                    let fresh = RepoContext::load(root, matcher, enabled, files);
                    (always_store || fresh != *current).then_some(fresh)
                }
            }
        }

        let files = Recorded::new(vec![(path.clone(), Planted::regular("first\n", 10))]);
        let loaded = RepoContext::load(&root, &matcher, true, &files);
        assert_eq!(loaded.file().unwrap().text, "first\n");

        // Unchanged: one stat, no read, no new state.
        files.forget();
        assert_eq!(
            RepoContext::verdict(&loaded, &root, &matcher, true, &files),
            RefreshVerdict::Unchanged
        );
        assert_eq!(files.calls(&root.path), vec!["stat TETON.md".to_owned()]);

        // Edited: the key moved, so the bytes are re-read — and the verdict says
        // *store it either way*, so a `touch` with no edit cannot leave the old
        // key behind to be re-read on every turn after it.
        let files = Recorded::new(vec![(path.clone(), Planted::regular("second\n", 11))]);
        assert_eq!(
            RepoContext::verdict(&loaded, &root, &matcher, true, &files),
            RefreshVerdict::Reload { always_store: true }
        );
        let fresh =
            settle(&loaded, &root, &matcher, true, &files).expect("an edited file is a new state");
        assert_eq!(fresh.file().unwrap().text, "second\n");
        assert_eq!(
            files.calls(&root.path),
            vec![
                // The verdict's own stat, then the composition's second one —
                // the assertion above ran the first, and `settle` runs the
                // whole pair again. What matters is the shape: the re-load
                // `stat`s for itself, which is what re-runs the entry rule and
                // the jail on a file that may have changed into something else.
                "stat TETON.md".to_owned(),
                "stat TETON.md".to_owned(),
                "stat TETON.md".to_owned(),
                "read TETON.md".to_owned()
            ],
        );

        // Same bytes, new boundary: withheld without a read (OQ-4).
        let covered = vec![PrivacyBoundary::user("*.md", BoundaryMode::LocalOnly)];
        let matcher_covered = BoundaryMatcher::new(&covered).unwrap();
        let files = Recorded::new(vec![(path.clone(), Planted::regular("first\n", 10))]);
        assert_eq!(
            settle(&loaded, &root, &matcher_covered, true, &files),
            Some(RepoContextState::WithheldBoundary {
                source: RepoContextSource::TetonMd,
                bytes_on_disk: Some("first\n".len() as u64),
            })
        );
        assert_eq!(files.calls(&root.path), vec!["stat TETON.md".to_owned()]);

        // The switch, both ways, with no filesystem call on the way down.
        let files = Recorded::new(vec![(path.clone(), Planted::regular("first\n", 10))]);
        assert_eq!(
            settle(&loaded, &root, &matcher, false, &files),
            Some(RepoContextState::WithheldOff)
        );
        assert!(files.calls(&root.path).is_empty());
        assert_eq!(
            settle(
                &RepoContextState::WithheldOff,
                &root,
                &matcher,
                false,
                &files
            ),
            None
        );
        let back_on = settle(
            &RepoContextState::WithheldOff,
            &root,
            &matcher,
            true,
            &files,
        )
        .expect("switching on re-loads at once");
        assert_eq!(back_on.file().unwrap().text, "first\n");

        // A state with no stored key: unchanged is still `None`, and still no read.
        let files = Recorded::new(Vec::new());
        assert_eq!(
            settle(&RepoContextState::Absent, &root, &matcher, true, &files),
            None
        );
        assert_eq!(
            files.calls(&root.path),
            vec!["stat TETON.md".to_owned(), "stat AGENTS.md".to_owned()],
        );

        // Verify (minor i): the root the session stands on is re-checked, and a
        // stored file that belongs to a root the session has left never wins the
        // key comparison. Both fall through to a full re-load.
        let (other_dir, other) = project_root("refresh-moved", RootKind::Project);
        let files = Recorded::new(vec![(
            other.path.join("TETON.md"),
            // Byte-identical to the loaded copy, key and all: only the *root*
            // differs, which is precisely what the stat comparison cannot see.
            Planted::regular("first\n", 10),
        )]);
        assert_eq!(
            RepoContext::verdict(&loaded, &other, &matcher, true, &files),
            RefreshVerdict::Reload {
                always_store: false
            },
            "a file under the root the session left was compared against the new root's"
        );
        // **Verify (MINOR 4).** A `/cd` *up* one level, to a root that is a
        // prefix of the one the session left. `starts_with` — the shape this
        // replaced — called `/repo/sub/TETON.md` "still under `/repo`", and the
        // `stat` below then compared `/repo/TETON.md` against the key of a
        // file in the subdirectory. The two are byte-identical here, which is
        // the case the old spelling could not tell apart: the parent
        // repository's own notes never became resident.
        let nested = dir.join("sub");
        std::fs::create_dir_all(&nested).unwrap();
        let nested = std::fs::canonicalize(&nested).unwrap();
        let inner = ProbedRoot {
            path: nested.clone(),
            view: root.view.clone(),
        };
        let files = Recorded::new(vec![(
            nested.join("TETON.md"),
            Planted::regular("first\n", 10),
        )]);
        let inner_loaded = RepoContext::load(&inner, &matcher, true, &files);
        assert_eq!(
            inner_loaded.file().unwrap().path,
            nested.join("TETON.md"),
            "the subdirectory fixture did not load"
        );
        let files = Recorded::new(vec![(
            // Same name, same key, same bytes — a different file, one directory up.
            root.path.join("TETON.md"),
            Planted::regular("first\n", 10),
        )]);
        assert_eq!(
            RepoContext::verdict(&inner_loaded, &root, &matcher, true, &files),
            RefreshVerdict::Reload {
                always_store: false
            },
            "a `/cd` from `/repo/sub` to `/repo` kept the subdirectory's notes"
        );
        assert_eq!(
            settle(&inner_loaded, &root, &matcher, true, &files)
                .expect("the parent root's own file is a new state")
                .file()
                .unwrap()
                .path,
            root.path.join("TETON.md"),
        );
        std::fs::remove_dir_all(&nested).ok();

        // **Verify (MINOR 3).** A bare `touch` moves the key and nothing else.
        // The verdict says *store it* — otherwise the stale key is re-read on
        // every turn after it — and `same_news` says there is nothing to
        // announce, which is the distinction the publish gate turns on.
        let touched = Recorded::new(vec![(path.clone(), Planted::regular("first\n", 99))]);
        assert_eq!(
            RepoContext::verdict(&loaded, &root, &matcher, true, &touched),
            RefreshVerdict::Reload { always_store: true }
        );
        let fresh = RepoContext::load(&root, &matcher, true, &touched);
        assert_ne!(fresh, loaded, "the key must move, or the leg is vacuous");
        assert!(
            fresh.same_news(&loaded),
            "a `touch` with no edit reads as news: {fresh:?}"
        );
        // And an edit that keeps the length is news, so the exclusion is of the
        // key alone and not of the content.
        let edited = Recorded::new(vec![(path.clone(), Planted::regular("FIRST\n", 10))]);
        let edited = RepoContext::load(&root, &matcher, true, &edited);
        assert!(
            !edited.same_news(&loaded),
            "a same-length edit was suppressed as a `touch`"
        );

        let (home_dir, home) = project_root("refresh-home", RootKind::Home);
        assert_eq!(
            RepoContext::verdict(&loaded, &home, &matcher, true, &files),
            RefreshVerdict::Reload {
                always_store: false
            },
            "a root that stopped being a project kept its notes"
        );
        assert_eq!(
            settle(&loaded, &home, &matcher, true, &files),
            Some(RepoContextState::Absent),
            "BR-1: a non-project root carries no notes"
        );
        std::fs::remove_dir_all(&other_dir).ok();
        std::fs::remove_dir_all(&home_dir).ok();
        std::fs::remove_dir_all(&dir).ok();
    }

    /// BR-4: the cap is measured on the **stripped** text, so a file of exactly
    /// [`REPO_CONTEXT_MAX_BYTES`] printable bytes plus 500 NULs is `Loaded`
    /// whole rather than `Truncated`.
    ///
    /// Mutation: storing the raw text instead of `strip_for_prompt`'s answer
    /// makes the state `Truncated` and the stored length 8,692.
    #[test]
    fn a_file_of_printable_bytes_and_control_characters_loads_whole_at_the_cap() {
        let (dir, root) = project_root("strip", RootKind::Project);
        let open = no_boundaries();
        let matcher = BoundaryMatcher::new(&open).unwrap();

        let line = format!("{}\n", "p".repeat(63));
        let mut text = line.repeat(REPO_CONTEXT_MAX_BYTES / 64);
        assert_eq!(text.len(), REPO_CONTEXT_MAX_BYTES);
        text.push_str(&"\0".repeat(500));

        let files = Recorded::new(vec![(
            root.path.join("TETON.md"),
            Planted::regular(&text, 10),
        )]);
        let state = RepoContext::load(&root, &matcher, true, &files);
        assert_eq!(state.kind(), RepoContextStateKind::Loaded);
        assert_eq!(state.file().unwrap().text.len(), REPO_CONTEXT_MAX_BYTES);
        assert_eq!(
            state.file().unwrap().bytes_on_disk as usize,
            REPO_CONTEXT_MAX_BYTES + 500,
            "the size on disk counts the bytes that were stripped"
        );
        std::fs::remove_dir_all(&dir).ok();
    }

    // -----------------------------------------------------------------------
    // `RealFiles` — the three claims only the real filesystem can settle
    // -----------------------------------------------------------------------

    /// A real, empty directory to plant real files in.
    ///
    /// [`project_root`] above needs a `ProbedRoot`; these three tests drive
    /// [`RealFiles`] directly and want only the directory.
    fn scratch(tag: &str) -> PathBuf {
        use std::sync::atomic::{AtomicU64, Ordering};
        static NEXT: AtomicU64 = AtomicU64::new(0);
        let dir = std::env::temp_dir().join(format!(
            "teton-real-files-{tag}-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::canonicalize(&dir).unwrap()
    }

    /// The identity the entry `stat` answers for `path` — what
    /// [`RepoContext::load`] carries into the read.
    fn identity_of(path: &Path) -> FileIdentity {
        RealFiles
            .stat(path)
            .expect("the fixture's own file stats")
            .identity()
    }

    /// **Verify (MAJOR 2).** A codepoint the read ceiling cuts in half is a cut,
    /// not a corrupt file: the valid prefix is kept. Anything else that is not
    /// UTF-8 is still [`RepoFileError::NotUtf8`].
    ///
    /// Driven against [`RealFiles`] on a real file rather than through the
    /// [`Recorded`] double, and that is the whole point of the test: the double
    /// walks back to a character boundary itself before it answers, so it hides
    /// the defect it would be asserting about. The ceiling is a *parameter* of
    /// the seam, so a 64-byte one exercises the same arithmetic as 65,536
    /// without a 64 KiB fixture.
    ///
    /// Mutation, run and observed: deleting the recovery from
    /// [`decode_at_ceiling`] — `String::from_utf8(bytes).map_err(|_| NotUtf8)` —
    /// fails the first leg with `Err(NotUtf8)` on a file that is valid UTF-8
    /// throughout. Widening the guard to drop `at_ceiling` fails the third and
    /// fourth legs by returning a silently shortened file where the honest
    /// answer is a refusal. **Verify (MINOR 1):** restoring the positional test
    /// `bytes.len() - valid_up_to <= 3` in place of `error_len().is_none()`
    /// fails the last leg, which is a file ending in three `0xFF` at the
    /// ceiling — three trailing bytes, and not one of them the start of a UTF-8
    /// sequence.
    #[test]
    fn a_codepoint_straddling_the_read_ceiling_keeps_the_valid_prefix() {
        let dir = scratch("utf8-ceiling");
        const CEILING: u64 = 64;

        // An em dash whose first byte is the ceiling's last: three bytes, one
        // inside and two past.
        let straddling = dir.join("straddle.md");
        std::fs::write(&straddling, format!("{}\u{2014}tail\n", "a".repeat(63))).unwrap();
        assert_eq!(
            RealFiles.read(&straddling, CEILING, identity_of(&straddling)),
            Ok("a".repeat(63)),
            "the read stopped mid-codepoint and called the file corrupt"
        );

        // The same file read past its end is whole, so the fixture is not a
        // file that was broken to begin with.
        assert_eq!(
            RealFiles.read(&straddling, 4_096, identity_of(&straddling)),
            Ok(format!("{}\u{2014}tail\n", "a".repeat(63)))
        );

        // A genuinely non-UTF-8 file, shorter than the ceiling: still refused.
        let corrupt = dir.join("corrupt.md");
        std::fs::write(&corrupt, [b'o', b'k', 0xFF, b'\n']).unwrap();
        assert_eq!(
            RealFiles.read(&corrupt, CEILING, identity_of(&corrupt)),
            Err(RepoFileError::NotUtf8),
            "a file that is not text was quietly truncated instead of refused"
        );

        // At the ceiling, but the invalid bytes are nowhere near the cut: the
        // recovery must not fire, because this file is not text.
        let mut deep = vec![b'x'; 64];
        deep[20] = 0xFF;
        deep.extend_from_slice(&[b'y'; 64]);
        let deep_path = dir.join("deep.md");
        std::fs::write(&deep_path, &deep).unwrap();
        assert_eq!(
            RealFiles.read(&deep_path, CEILING, identity_of(&deep_path)),
            Err(RepoFileError::NotUtf8)
        );

        // **MINOR 1.** Three `0xFF` at the production ceiling: positionally
        // indistinguishable from a cut four-byte codepoint — the last three
        // bytes of a buffer that is exactly `REPO_CONTEXT_READ_CEILING_BYTES`
        // long — and not text at all. `0xFF` is not a lead byte in any UTF-8
        // encoding, so `error_len()` names it a real error rather than an
        // incomplete tail, and the file is refused.
        let mut ff = vec![b'z'; usize::try_from(REPO_CONTEXT_READ_CEILING_BYTES).unwrap() - 3];
        ff.extend_from_slice(&[0xFF, 0xFF, 0xFF]);
        ff.extend_from_slice(b"more after the ceiling\n");
        let ff_path = dir.join("trailing-ff.md");
        std::fs::write(&ff_path, &ff).unwrap();
        assert_eq!(
            RealFiles.read(
                &ff_path,
                REPO_CONTEXT_READ_CEILING_BYTES,
                identity_of(&ff_path)
            ),
            Err(RepoFileError::NotUtf8),
            "three bytes that can never start a codepoint were recovered as a \
             codepoint the ceiling cut in half"
        );

        std::fs::remove_dir_all(&dir).ok();
    }

    /// A real [`ProbedRoot`] naming an existing directory, for the tests below
    /// that drive the whole of [`RepoContext::load`] against the real
    /// filesystem.
    fn probed_at(path: &Path) -> ProbedRoot {
        ProbedRoot {
            path: path.to_path_buf(),
            view: SessionRoot {
                display: "~/repo".to_owned(),
                kind: RootKind::Project,
                project_name: Some("repo".to_owned()),
                vcs_branch: None,
            },
        }
    }

    /// **Verify (HIGH).** A symlinked `TETON.md` is refused through the **whole
    /// of `load`** against the real filesystem, and a read handed an identity
    /// that is not the file's own is `Changed`.
    ///
    /// # The window this closes, and why the earlier version did not close it
    ///
    /// The first fix put `O_NOFOLLOW` on the open and compared the opened
    /// handle's *length* against a pre-open `metadata`. Neither reaches the
    /// hazard. `load` opens `ToolContext::resolve`'s answer, and `resolve`
    /// canonicalizes — so a symlink planted after the entry `lstat` is already
    /// dereferenced before `O_NOFOLLOW` sees the path, and the flag refuses
    /// nothing. Length is not identity either: a replacement of the same size
    /// passes, and an in-place edit — which is not an attack — fails.
    ///
    /// So the identity travels. Both halves are asserted here because they fail
    /// separately:
    ///
    /// 1. the **loader**, end to end on a real tree, with the link planted
    ///    *before* `load` runs so the entry `lstat` is the syscall that sees it;
    /// 2. the **seam**, called directly with an identity that is not the file's,
    ///    which is the state a same-UID process leaves behind by replacing the
    ///    entry between the `stat` and the open. No fixture can hold that race
    ///    open, and handing `read` the wrong identity is exactly what the race
    ///    produces.
    ///
    /// # Mutation, run and observed
    ///
    /// | change | result |
    /// |---|---|
    /// | drop the `(dev, ino)` check | leg 2 is `Ok("the target's own bytes\n")` — the daemon reading bytes it never `stat`ed. This is also what the length comparison it replaced did, because both lengths were read *inside* `read` and so spanned microseconds rather than the caller's window |
    /// | drop `.custom_flags(libc::O_NOFOLLOW)` | leg 3 is `Err(Changed)` where `Err(Symlink)` is owed — the identity catches the swap, and the user is told the wrong thing about it |
    #[test]
    fn a_symlinked_entry_is_refused_through_load_and_a_swapped_inode_is_changed() {
        let dir = scratch("nofollow");
        let boundaries = no_boundaries();
        let matcher = BoundaryMatcher::new(&boundaries).unwrap();
        let root = probed_at(&dir);

        let target = dir.join("target.md");
        std::fs::write(&target, "the target's own bytes\n").unwrap();
        let link = dir.join("TETON.md");
        std::os::unix::fs::symlink(&target, &link).unwrap();

        // Leg 1 — the loader, whole, on a real tree. The link is planted before
        // `load` runs, so the entry `lstat` is what sees it.
        assert_eq!(
            RepoContext::load(&root, &matcher, true, &RealFiles),
            RepoContextState::Unreadable {
                source: RepoContextSource::TetonMd,
                reason: REASON_SYMLINK,
                bytes_on_disk: None,
            },
            "a symlinked `TETON.md` was followed by the loader"
        );

        // Leg 2 — the seam, handed an identity that is not this file's. The
        // *path* resolves to an ordinary regular file and the open succeeds;
        // only the `fstat` comparison can refuse it.
        let elsewhere = dir.join("elsewhere.md");
        std::fs::write(&elsewhere, "somebody else's bytes\n").unwrap();
        assert_eq!(
            RealFiles.read(
                &target,
                REPO_CONTEXT_READ_CEILING_BYTES,
                identity_of(&elsewhere),
            ),
            Err(RepoFileError::Changed),
            "the read answered about an inode it was not asked about"
        );

        // Leg 3 — `O_NOFOLLOW` still refuses a link handed to the seam directly,
        // which is the case where the path was *not* canonicalized first.
        assert_eq!(
            RealFiles.read(&link, REPO_CONTEXT_READ_CEILING_BYTES, identity_of(&link)),
            Err(RepoFileError::Symlink),
            "the open followed a symlink"
        );
        // Non-vacuity: the same reader reads the target at its own name with its
        // own identity, so the three refusals above are about the fixtures'
        // shapes and not about the reader.
        assert_eq!(
            RealFiles.read(
                &target,
                REPO_CONTEXT_READ_CEILING_BYTES,
                identity_of(&target),
            ),
            Ok("the target's own bytes\n".to_owned())
        );
        // And the loader's own answer for a symlinked candidate is the same
        // sentence, from the other end of the pair.
        assert_eq!(reason_for(RepoFileError::Symlink), REASON_SYMLINK);
        assert_eq!(reason_for(RepoFileError::Changed), REASON_CHANGED);

        std::fs::remove_dir_all(&dir).ok();
    }

    /// **Verify (HIGH).** A hardlinked `TETON.md` is refused by the entry rule,
    /// and an **in-place edit** between the entry `stat` and the read is not.
    ///
    /// The two belong in one test because they are the two directions the
    /// identity check can be got wrong in, and a fix for either one alone
    /// produces a plausible-looking build:
    ///
    /// - a hardlink is a regular file, is not a symlink, and *is* the inode it
    ///   points at — so every other check here passes it, and only `nlink`
    ///   refuses it. A user who can plant one at the session root can make any
    ///   file they own resident;
    /// - an in-place edit keeps `(dev, ino)` and changes the bytes and the
    ///   length. It is the ordinary case — somebody saved their `TETON.md`
    ///   while a turn was starting — and the length comparison this replaced
    ///   refused it, which is a legitimate file the daemon would have called
    ///   unreadable.
    ///
    /// # Mutation, run and observed
    ///
    /// | change | result |
    /// |---|---|
    /// | delete the `key.nlink > 1` refusal | leg 1 loads `the linked bytes\n`, and the recorded-double leg in `a_symlinked_entry_and_an_eperm_…` fails beside it |
    /// | add `len` to [`FileIdentity`] and compare it | leg 2 fails: the rewritten file's identity no longer matches the one taken before the edit, so an ordinary save is refused as a race |
    #[test]
    fn a_hardlink_is_refused_and_an_in_place_edit_is_read() {
        let dir = scratch("hardlink");
        let boundaries = no_boundaries();
        let matcher = BoundaryMatcher::new(&boundaries).unwrap();
        let root = probed_at(&dir);

        // Leg 1 — a second name for one inode, planted at the candidate.
        let outside = dir.join("outside.md");
        std::fs::write(&outside, "the linked bytes\n").unwrap();
        let linked = dir.join("TETON.md");
        std::fs::hard_link(&outside, &linked).unwrap();
        assert_eq!(
            RealFiles.stat(&linked).expect("the hardlink stats").nlink,
            2,
            "the fixture is not a hardlink, so the refusal below is vacuous"
        );
        assert_eq!(
            RepoContext::load(&root, &matcher, true, &RealFiles),
            RepoContextState::Unreadable {
                source: RepoContextSource::TetonMd,
                reason: REASON_HARDLINK,
                // A hardlink is a regular file, so its `lstat` length is a real
                // size and is reported — unlike a symlink's.
                bytes_on_disk: Some("the linked bytes\n".len() as u64),
            },
            "a hardlinked `TETON.md` made a file outside the root resident"
        );

        // Leg 2 — the same inode, rewritten between the `stat` and the read.
        // The identity is taken first, exactly as `load` takes it, and the file
        // is then rewritten in place before the read runs.
        std::fs::remove_file(&linked).unwrap();
        let edited = dir.join("TETON.md");
        std::fs::write(&edited, "before the edit\n").unwrap();
        let identity = identity_of(&edited);
        std::fs::write(&edited, "after the edit, and rather longer than before\n").unwrap();
        assert_eq!(
            identity_of(&edited),
            identity,
            "the fixture replaced the inode instead of rewriting it, which is \
             the other case entirely"
        );
        assert_eq!(
            RealFiles.read(&edited, REPO_CONTEXT_READ_CEILING_BYTES, identity),
            Ok("after the edit, and rather longer than before\n".to_owned()),
            "an ordinary in-place edit was refused as a race"
        );

        std::fs::remove_dir_all(&dir).ok();
    }

    /// **Verify (MINOR iv).** A permission is a permission and everything else
    /// is not: the two lead a user to different remedies, and only one of them
    /// can be right.
    ///
    /// Asserted on [`map_io`] against synthesized `io::Error`s rather than on a
    /// tree, because the input that matters — an `EIO`, a device that went away
    /// — is not one a test can reliably provoke, and the mapping *is* the fix.
    ///
    /// Mutation: restoring `_ => RepoFileError::Denied` fails the third leg,
    /// and with it the sentence a user is shown for a file they own and can
    /// `cat`.
    #[test]
    fn only_a_permission_error_is_denied_and_every_other_one_is_named_neutrally() {
        use std::io::{Error, ErrorKind};
        assert_eq!(
            map_io(Error::from(ErrorKind::NotFound)),
            RepoFileError::NotFound
        );
        assert_eq!(
            map_io(Error::from(ErrorKind::PermissionDenied)),
            RepoFileError::Denied
        );
        for kind in [
            ErrorKind::Other,
            ErrorKind::BrokenPipe,
            ErrorKind::InvalidData,
            ErrorKind::TimedOut,
        ] {
            assert_eq!(
                map_io(Error::from(kind)),
                RepoFileError::Unavailable,
                "{kind:?} was reported as a permission the user has"
            );
        }

        // The two sentences are different sentences, which is the point of the
        // split — and every reason in the closed set is distinct, so no two
        // verdicts can reach a user as one.
        assert_ne!(REASON_DENIED, REASON_UNAVAILABLE);
        let reasons = [
            RepoFileError::NotFound,
            RepoFileError::Denied,
            RepoFileError::NotRegular,
            RepoFileError::NotUtf8,
            RepoFileError::Symlink,
            RepoFileError::Changed,
            RepoFileError::Unavailable,
        ]
        .map(reason_for);
        let unique: std::collections::BTreeSet<&str> = reasons.iter().copied().collect();
        assert_eq!(
            unique.len(),
            reasons.len(),
            "two seam errors share one sentence: {reasons:?}"
        );
        // The entry rule's own refusal is not a seam error and still must not
        // reach a user as one of the seam's sentences.
        assert!(
            !reasons.contains(&REASON_HARDLINK),
            "the hardlink refusal shares a sentence with a seam error: {reasons:?}"
        );
    }
}
