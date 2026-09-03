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
//! # What is deliberately not here
//!
//! No call into [`crate::projects::scan`] — `scan.rs`'s forbid list guards the
//! `session/create` derivation, and a one-file `stat` + `read` has no reason to
//! reach for a scanner. No watcher: BR-6's staleness check is [`RepoContext::refresh`],
//! run by the turn's `assemble` stage, and REQ-585 BR-1 already decided against
//! watching. No wiring: `HarnessConfig`, the session record and the runtime are
//! TASK-373/374's, and nothing in this module knows they exist.

pub mod render;

use std::io::Read;
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
/// comparison in [`RepoContext::refresh`] treats a missing timestamp as
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
    Denied,
    /// Not a regular file — a directory, FIFO, socket or device.
    NotRegular,
    /// Not valid UTF-8. Reported separately so the state names the reason
    /// rather than blaming a permission the user has.
    NotUtf8,
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
    /// bytes.
    ///
    /// Over-ceiling is **not** a refusal: a huge file is read to the ceiling and
    /// truncated with a marker, because the top of the file is the part a
    /// repository author puts first (ADR-5).
    ///
    /// # Errors
    ///
    /// [`RepoFileError`], one variant per user-visible sentence.
    fn read(&self, path: &Path, ceiling: u64) -> Result<String, RepoFileError>;
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
        })
    }

    fn read(&self, path: &Path, ceiling: u64) -> Result<String, RepoFileError> {
        // Type checked off `metadata` **before** the open — `fs_util`'s order,
        // for `fs_util`'s reason: a FIFO whose open blocks for a writer must
        // never be opened at all. The caller has already refused a symlinked
        // entry, so following here resolves nothing it has not judged.
        let metadata = std::fs::metadata(path).map_err(map_io)?;
        if !metadata.is_file() {
            return Err(RepoFileError::NotRegular);
        }
        let handle = std::fs::File::open(path).map_err(map_io)?;
        let mut bytes = Vec::new();
        // `take` as well as the ceiling argument, so a file that grows under the
        // read stays bounded. `read_to_end` and then a conversion rather than
        // `read_to_string`, so "not UTF-8" is a verdict rather than an inference
        // from a flattened `io::Error`.
        handle
            .take(ceiling)
            .read_to_end(&mut bytes)
            .map_err(|_| RepoFileError::Denied)?;
        String::from_utf8(bytes).map_err(|_| RepoFileError::NotUtf8)
    }
}

/// Map an `io::Error` to this module's two-value answer for an opening call.
fn map_io(error: std::io::Error) -> RepoFileError {
    match error.kind() {
        std::io::ErrorKind::NotFound => RepoFileError::NotFound,
        _ => RepoFileError::Denied,
    }
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
    },
}

/// The reason for a symlinked entry ([`RepoContextState::Unreadable`]).
const REASON_SYMLINK: &str = "it is a symlink, and a symlinked entry is not followed";
/// The reason for an `EPERM`/TCC refusal.
const REASON_DENIED: &str = "it could not be opened (permission denied)";
/// The reason for a directory, FIFO, socket or device wearing the name.
const REASON_NOT_REGULAR: &str = "it is not a regular file";
/// The reason for bytes that are not text.
const REASON_NOT_UTF8: &str = "it is not valid UTF-8";
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
            Self::WithheldBoundary { source } | Self::Unreadable { source, .. } => Some(*source),
            Self::Absent | Self::WithheldOff => None,
        }
    }
}

/// The loader (ADR-3): one file, one place, two entry points.
///
/// A namespace rather than a value. ADR-3 spells the second entry point
/// `refresh(&self, ..)`; it takes the current state by reference instead,
/// because a wrapper struct holding a `RepoContextState` would be a second place
/// the session's state lives and the session record (TASK-373) is the first.
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
    /// 4. **the entry rule** — a symlink or a non-regular file is named
    ///    `Unreadable` and never opened;
    /// 5. **the jail and the mint** — [`ToolContext::resolve`], one call for both
    ///    halves of the identity (REQ-571 ADR-B);
    /// 6. **the boundary** — a covered identity is withheld **before** the read,
    ///    so a `local-only` file's bytes never enter the daemon's memory at all
    ///    (ADR-2 asks for the block to be withheld; not reading it is the
    ///    stronger property and costs nothing);
    /// 7. **the read**, bounded by [`REPO_CONTEXT_READ_CEILING_BYTES`].
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
                Err(_) => return unreadable(source, REASON_DENIED),
            };
            if key.is_symlink {
                return unreadable(source, REASON_SYMLINK);
            }
            if !key.is_regular {
                return unreadable(source, REASON_NOT_REGULAR);
            }
            // The jail's own resolution, and the identity it mints — never a
            // second parse of the same path (LESSON-494, LESSON-623).
            let Ok(resolved) = jail.resolve(name) else {
                return unreadable(source, REASON_UNRESOLVED);
            };
            if boundaries
                .match_path(resolved.provenance.as_str())
                .is_some()
            {
                return RepoContextState::WithheldBoundary { source };
            }
            let text = match files.read(&resolved.path, REPO_CONTEXT_READ_CEILING_BYTES) {
                Ok(text) => text,
                Err(RepoFileError::NotFound) => return unreadable(source, REASON_UNRESOLVED),
                Err(RepoFileError::NotRegular) => return unreadable(source, REASON_NOT_REGULAR),
                Err(RepoFileError::NotUtf8) => return unreadable(source, REASON_NOT_UTF8),
                Err(RepoFileError::Denied) => return unreadable(source, REASON_DENIED),
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

    /// BR-6's start-of-turn staleness check: `Some` when the session's state has
    /// to change, `None` when it has not.
    ///
    /// `None` is the answer that has to be cheap, because it is the answer on
    /// every turn of every session: one `stat` and a glob match, no read, no
    /// allocation of the file's bytes. It is returned only when **both** halves
    /// of the verdict still hold — the `mtime`/`len` key is identical *and* the
    /// boundary set still does not cover the identity (OQ-4: a boundary added
    /// mid-session drops the block at the next prompt).
    ///
    /// A state with no stored key ([`RepoContextState::Absent`],
    /// `WithheldBoundary`, `Unreadable`) re-runs [`Self::load`] and reports only
    /// a *difference*. That is one or two `stat`s and still no read — none of
    /// those paths reaches the read — and it is what lets a `TETON.md` created
    /// mid-session become resident at the next prompt.
    ///
    /// **A newly created `TETON.md` beside a loaded `AGENTS.md` waits for the
    /// next `session/create` or `/cd`.** Seeing it would cost a second `stat` on
    /// every turn of every session carrying a fallback file, and ADR-3's budget
    /// for this check is one.
    pub fn refresh(
        current: &RepoContextState,
        root: &ProbedRoot,
        boundaries: &BoundaryMatcher<'_>,
        enabled: bool,
        files: &dyn RepoFileReader,
    ) -> Option<RepoContextState> {
        if !enabled {
            // Off means unopened, on every path into this function.
            return match current {
                RepoContextState::WithheldOff => None,
                _ => Some(RepoContextState::WithheldOff),
            };
        }
        let (RepoContextState::Loaded(file) | RepoContextState::Truncated(file)) = current else {
            let fresh = Self::load(root, boundaries, enabled, files);
            return (fresh != *current).then_some(fresh);
        };
        let spelled = root.path.join(file_name(file.source));
        match files.stat(&spelled) {
            // `mtime.is_some()` is part of the equality on purpose: two `None`s
            // compare equal, and a filesystem that reports no timestamp would
            // otherwise pin the first copy read for the life of the session.
            Ok(key) if key == file.key && key.mtime.is_some() => {
                if boundaries.match_path(file.provenance.as_str()).is_some() {
                    Some(RepoContextState::WithheldBoundary {
                        source: file.source,
                    })
                } else {
                    None
                }
            }
            // Any change to the key — and any `stat` that now fails — is a full
            // re-load, so the state that comes back carries the new key. Compare
            // the *states* here and a `touch` with no edit would leave the stale
            // key stored and re-read on every turn thereafter.
            //
            // The re-load `stat`s again rather than being handed the key just
            // read, and that second syscall buys the whole gate chain: a file
            // that became a symlink, a non-regular file or a boundary-covered
            // one between two turns is judged by exactly the rules that judged
            // it at `session/create`. One `stat` is the budget for the *quiet*
            // turn (ADR-3); a turn where the notes actually changed is already
            // paying for a read.
            _ => Some(Self::load(root, boundaries, enabled, files)),
        }
    }
}

/// One spelling of the `Unreadable` construction, so every refusal above is one
/// line and the reason set stays visibly closed.
fn unreadable(source: RepoContextSource, reason: &'static str) -> RepoContextState {
    RepoContextState::Unreadable { source, reason }
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
                }),
                read: Ok(text.to_owned()),
            }
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

        fn read(&self, path: &Path, ceiling: u64) -> Result<String, RepoFileError> {
            self.calls
                .lock()
                .unwrap()
                .push(format!("read {}", path.display()));
            let planted = match self.files.get(path) {
                Some(planted) => planted,
                None => return Err(RepoFileError::NotFound),
            };
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
            }
        );
        assert_eq!(
            files.calls(&root.path),
            vec!["stat TETON.md".to_owned()],
            "a symlinked entry was followed"
        );

        let files = Recorded::new(vec![(root.path.join("TETON.md"), Planted::denied())]);
        assert_eq!(
            RepoContext::load(&root, &matcher, true, &files),
            RepoContextState::Unreadable {
                source: RepoContextSource::TetonMd,
                reason: REASON_DENIED,
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

    /// BR-6: `refresh` reads only when the `mtime`/`len` key moved or the
    /// boundary verdict changed, and its `None` costs one `stat`.
    ///
    /// Mutation: dropping `key == file.key` from the guard returns `None`
    /// forever and fails the edited leg; dropping the boundary re-check returns
    /// `None` on the covered leg and leaves a `local-only` file resident, which
    /// is OQ-4's hole.
    #[test]
    fn refresh_reads_only_when_mtime_len_or_verdict_changed() {
        let (dir, root) = project_root("refresh", RootKind::Project);
        let open = no_boundaries();
        let matcher = BoundaryMatcher::new(&open).unwrap();
        let path = root.path.join("TETON.md");

        let files = Recorded::new(vec![(path.clone(), Planted::regular("first\n", 10))]);
        let loaded = RepoContext::load(&root, &matcher, true, &files);
        assert_eq!(loaded.file().unwrap().text, "first\n");

        // Unchanged: one stat, no read, no new state.
        files.forget();
        assert_eq!(
            RepoContext::refresh(&loaded, &root, &matcher, true, &files),
            None
        );
        assert_eq!(files.calls(&root.path), vec!["stat TETON.md".to_owned()]);

        // Edited: the key moved, so the bytes are re-read.
        let files = Recorded::new(vec![(path.clone(), Planted::regular("second\n", 11))]);
        let fresh = RepoContext::refresh(&loaded, &root, &matcher, true, &files)
            .expect("an edited file is a new state");
        assert_eq!(fresh.file().unwrap().text, "second\n");
        assert_eq!(
            files.calls(&root.path),
            vec![
                // The comparison `stat`, then the re-load's own — the second one
                // is what re-runs the entry rule and the jail on a file that may
                // have changed into something else entirely.
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
            RepoContext::refresh(&loaded, &root, &matcher_covered, true, &files),
            Some(RepoContextState::WithheldBoundary {
                source: RepoContextSource::TetonMd,
            })
        );
        assert_eq!(files.calls(&root.path), vec!["stat TETON.md".to_owned()]);

        // The switch, both ways, with no filesystem call on the way down.
        let files = Recorded::new(vec![(path.clone(), Planted::regular("first\n", 10))]);
        assert_eq!(
            RepoContext::refresh(&loaded, &root, &matcher, false, &files),
            Some(RepoContextState::WithheldOff)
        );
        assert!(files.calls(&root.path).is_empty());
        assert_eq!(
            RepoContext::refresh(
                &RepoContextState::WithheldOff,
                &root,
                &matcher,
                false,
                &files
            ),
            None
        );
        let back_on = RepoContext::refresh(
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
            RepoContext::refresh(&RepoContextState::Absent, &root, &matcher, true, &files),
            None
        );
        assert_eq!(
            files.calls(&root.path),
            vec!["stat TETON.md".to_owned(), "stat AGENTS.md".to_owned()],
        );
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
}
