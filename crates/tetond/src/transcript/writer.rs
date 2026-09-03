//! REQ-611 — one session's file: open, append, resume, close (BR-9, BR-12,
//! BR-14).
//!
//! # What a [`Writer`] owns
//!
//! One session's transcript file and its per-file `n`. `n` is minted here and
//! nowhere else: it is a property of the file, and a caller handing in a line
//! that already carried one would be a second minting site — which is exactly
//! how a counter grows a hole (BR-14). A [`Writer`] outlives a `/transcript
//! off`: [`Writer::close`] writes the closing record and drops the file handle
//! while keeping the path and the counter, so [`Writer::resume`] can append
//! `transcript_resumed` to the **same** file with `n` continuing (AC-4).
//!
//! # Owner-only, explicitly, on every platform (BR-9)
//!
//! Modes are set with `set_permissions` rather than left to `DirBuilder::mode`
//! and `OpenOptions::mode` alone, because both of those are masked by the
//! process umask and CI's ubuntu leg does not run the developer's umask. A
//! pre-existing directory or file that is wider than owner-only is **refused**
//! rather than tightened: BR-9 says a wider entry is not silently reused, and
//! the directory the daemon does not own is precisely the one whose contents it
//! should not be appending a session's prompts to. That is why
//! [`crate::auth::secure_socket_dir`] is used only for *creation* here — its
//! best-effort tightening of an existing directory is the right posture for a
//! socket and the wrong one for a record.
//!
//! # Durability
//!
//! `write_all` then `flush` per line. A crash can therefore leave at most one
//! partial trailing line, which BR-14 permits and the format documentation
//! states; no `fsync`, because a transcript is a user's record of their own
//! session rather than a ledger, and a per-record sync would put the disk's
//! latency on the writer thread for every chunk of streamed model text.

use std::fs::{File, OpenOptions};
use std::io::{self, Write as _};
use std::os::unix::fs::{OpenOptionsExt as _, PermissionsExt as _};
use std::path::{Path, PathBuf};
use std::time::SystemTime;

use teton_protocol::SessionId;

use super::record::{file_stamp_utc, CloseReason, Line, Opened, Record};

/// The mode bits that must be clear on a transcript file or directory (BR-9).
///
/// Group and other, all of them: a transcript holds every prompt and every tool
/// result, so "another user can read it" and "another user can traverse to it"
/// are the same refusal.
const NON_OWNER_BITS: u32 = 0o077;

/// The extension every transcript file carries.
pub const TRANSCRIPT_EXTENSION: &str = "jsonl";

/// Why a transcript could not be opened (BR-9, AC-11, AC-13).
///
/// Typed rather than an `io::Error` alone because the sink turns these into two
/// different stories for the user — `dir_refused` is a configuration the user
/// can fix, an I/O failure is a machine that is unwell — and a message composed
/// at the point of detection cannot be worded by the surface that renders it
/// (conventions: *compose the sentence where the facts are*).
#[derive(Debug, thiserror::Error)]
pub enum Refused {
    /// The directory or file exists and is wider than owner-only (BR-9).
    #[error(
        "`{path}` is wider than owner-only (mode {mode:04o}); transcripts are not written to it"
    )]
    Mode {
        /// The offending path.
        path: PathBuf,
        /// Its permission bits.
        mode: u32,
    },
    /// The directory or the file path is a symlink.
    ///
    /// Refused rather than followed for [`super::retention::prune`]'s reason: a
    /// symlink is a path the daemon did not choose, and appending a session's
    /// content through one writes it somewhere the user never named.
    #[error("`{path}` is a symlink; transcripts are not written through one")]
    Symlink {
        /// The offending path.
        path: PathBuf,
    },
    /// The path exists and is not the kind of thing it needs to be.
    #[error("`{path}` is not a {expected}")]
    NotA {
        /// The offending path.
        path: PathBuf,
        /// What was needed there — `directory` or `file`.
        expected: &'static str,
    },
    /// The directory could not be created, or the file could not be opened or
    /// written.
    #[error("`{path}` could not be opened for a transcript: {source}")]
    Io {
        /// The path the failure was about.
        path: PathBuf,
        /// The underlying failure.
        #[source]
        source: io::Error,
    },
}

/// Write-failure injection for this module's own tests (BR-6, ADR-8).
///
/// **Two arms, one call site.** Under `cfg(test)` a unit test arms an exact
/// number of failures with [`Faults::arm`]. In any build, the
/// `TETON_TRANSCRIPT_SEAM=write_fail_after:<n>` seam (REQ-611 AC-10) may arm
/// "the `n+1`th append fails" through [`Faults::failing_after`] — but only
/// where `runtime::engine::test_seams_enabled` says seams are honoured, so a
/// shipped daemon that reaches this code carries `fail_after: None` and the
/// branch is never taken. A seam can only *deny* a write, never record one.
///
/// A count rather than a flag: BR-6 requires the sink to *attempt* one
/// `transcript_closed { write_failure }` after the failure that degraded it, so
/// a test that arms exactly one failure can assert that the closing record
/// actually landed.
#[derive(Debug, Clone, Default)]
pub(crate) struct Faults {
    #[cfg(test)]
    remaining: std::sync::Arc<std::sync::atomic::AtomicU64>,
    /// The `write_fail_after:<n>` seam: appends seen so far, and the count
    /// after which every further append fails. `None` outside a seam.
    fail_after: Option<std::sync::Arc<(std::sync::atomic::AtomicU64, u64)>>,
}

impl Faults {
    /// Arm the runtime seam: the first `n` appends succeed, every later one fails.
    #[must_use]
    pub(crate) fn failing_after(n: u64) -> Self {
        Self {
            #[cfg(test)]
            remaining: Default::default(),
            fail_after: Some(std::sync::Arc::new((
                std::sync::atomic::AtomicU64::new(0),
                n,
            ))),
        }
    }

    /// Whether this append fails: the runtime seam first, then a unit test's
    /// armed count.
    fn take(&self) -> bool {
        use std::sync::atomic::Ordering;
        if let Some(armed) = &self.fail_after {
            let seen = armed.0.fetch_add(1, Ordering::Relaxed) + 1;
            if seen > armed.1 {
                return true;
            }
        }
        self.take_armed()
    }

    /// Consume one armed failure, if any remain.
    #[cfg(test)]
    fn take_armed(&self) -> bool {
        use std::sync::atomic::Ordering;
        self.remaining
            .fetch_update(Ordering::Relaxed, Ordering::Relaxed, |remaining| {
                remaining.checked_sub(1)
            })
            .is_ok()
    }

    /// No unit-test failure is ever injected in a shipped build.
    #[cfg(not(test))]
    fn take_armed(&self) -> bool {
        false
    }

    /// Arm `count` write failures.
    #[cfg(test)]
    pub(crate) fn arm(&self, count: u64) {
        self.remaining
            .store(count, std::sync::atomic::Ordering::Relaxed);
    }
}

/// One session's append-only JSONL transcript.
#[derive(Debug)]
pub struct Writer {
    /// The file, as `<dir>/<start-utc>-<session_id>.jsonl`.
    path: PathBuf,
    /// The session every line of this file belongs to.
    session_id: SessionId,
    /// The last `n` written. The next record takes `n + 1`.
    n: u64,
    /// `max_record_bytes`, carried from the `transcript_opened` record so every
    /// line in a file is measured against the budget that file declared.
    budget: usize,
    /// Records dropped before they reached the file, not yet accounted for by a
    /// `transcript_gap` (BR-5).
    pending_gap: u64,
    /// The last bus `seq` written, so a gap record can say what it follows.
    last_seq: Option<u64>,
    /// The open handle, or `None` between a [`Writer::close`] and a
    /// [`Writer::resume`].
    file: Option<File>,
    /// Test-only write-failure injection; see [`Faults`].
    faults: Faults,
}

impl Writer {
    /// Open (creating) this session's transcript and write `transcript_opened`.
    ///
    /// The directory is created `0o700` when absent and **refused** when it
    /// exists wider than owner-only; the file is created `0o600` and refused on
    /// the same terms (BR-9, AC-13). The file name embeds the session id, which
    /// is what lets [`super::retention::prune`] recognise its own files without
    /// a manifest — session ids are names rather than credentials (REQ-569
    /// BR-8), so the name discloses nothing a `session/list` would not.
    ///
    /// The `transcript_opened` record is written here rather than by the caller
    /// so that a file cannot exist without one: AC-2 requires it to be the first
    /// record, and a two-call open would let a crash between them produce a file
    /// whose provenance nothing states.
    ///
    /// # Errors
    ///
    /// [`Refused`] when the directory or file is wider than owner-only, is a
    /// symlink, or cannot be created, opened or written.
    pub fn open(
        dir: &Path,
        session_id: &SessionId,
        started_at: SystemTime,
        opened: Opened,
    ) -> Result<Self, Refused> {
        prepare_dir(dir)?;
        let path = dir.join(format!(
            "{}-{session_id}.{TRANSCRIPT_EXTENSION}",
            file_stamp_utc(started_at)
        ));
        let budget = opened.max_record_bytes;
        let file = open_owner_only(&path)?;
        let mut writer = Self {
            path,
            session_id: session_id.clone(),
            n: 0,
            budget,
            pending_gap: 0,
            last_seq: None,
            file: Some(file),
            faults: Faults::default(),
        };
        writer.emit(&Record::Opened(opened)).map_err(|source| {
            let path = writer.path.clone();
            Refused::Io { path, source }
        })?;
        Ok(writer)
    }

    /// Reopen a closed transcript for append and write `transcript_resumed`
    /// (AC-4).
    ///
    /// The same file, the same `n` sequence: `/transcript off` then `on` inside
    /// one session is a pause in the record, not a second record. The mode
    /// checks run again, because the interval between the close and the resume
    /// is exactly when somebody could have widened the file.
    ///
    /// # Errors
    ///
    /// The `io::Error` from reopening or from the `transcript_resumed` write.
    pub fn resume(&mut self, seq_at_resume: u64) -> io::Result<()> {
        if self.file.is_none() {
            let file = open_owner_only(&self.path).map_err(io::Error::other)?;
            self.file = Some(file);
        }
        self.emit(&Record::Resumed { seq_at_resume }).map(|_| ())
    }

    /// Append one record, minting its `n`.
    ///
    /// A pending gap (see [`Writer::note_dropped`]) is written **first**, so a
    /// reader meets the count of what is missing before the record that follows
    /// the hole (BR-5).
    ///
    /// The task file spells this `append(Line)`; the [`Line`] is built inside
    /// because `n` is the writer's to mint — a `Line` handed in from outside
    /// could carry a stale one, and a hole in `n` is the one malformation BR-14
    /// does not permit.
    ///
    /// # Errors
    ///
    /// The `io::Error` from the write. The caller degrades the session on the
    /// first one (BR-6, ADR-8); `n` does not advance past a failed write.
    pub fn append(&mut self, record: &Record) -> io::Result<u64> {
        self.flush_gap(record.seq())?;
        self.emit(record)
    }

    /// Note that `count` records never reached the file (BR-5).
    ///
    /// The next successful [`Writer::append`] or [`Writer::close`] writes one
    /// `transcript_gap { dropped: count }` ahead of itself. Counting here rather
    /// than at the producer is what keeps "a hole in `n` never appears" a
    /// property of the file: the count and the counter are updated by the same
    /// code.
    pub fn note_dropped(&mut self, count: u64) {
        self.pending_gap = self.pending_gap.saturating_add(count);
    }

    /// Write `transcript_closed` and drop the file handle.
    ///
    /// Idempotent: closing an already-closed writer writes nothing. The path and
    /// `n` survive for a later [`Writer::resume`].
    ///
    /// # Errors
    ///
    /// The `io::Error` from the closing write. On the `write_failure` path the
    /// caller has already degraded the session, and this is the one attempt
    /// BR-6 asks for.
    pub fn close(&mut self, reason: CloseReason) -> io::Result<()> {
        if self.file.is_none() {
            return Ok(());
        }
        let gap = self.flush_gap(None);
        let records = self.n + 1;
        let result = self.emit(&Record::Closed { reason, records }).map(|_| ());
        self.file = None;
        gap.and(result)
    }

    /// Flush the file handle.
    ///
    /// Every record is already flushed as it is written; this exists for the
    /// shutdown path, which wants one explicit place to say "the bytes are with
    /// the kernel" (AC-18).
    ///
    /// # Errors
    ///
    /// The `io::Error` from the flush.
    pub fn flush(&mut self) -> io::Result<()> {
        match self.file.as_mut() {
            Some(file) => file.flush(),
            None => Ok(()),
        }
    }

    /// Where this session's transcript lives.
    #[must_use]
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// How many records the file holds — the current `n`.
    #[must_use]
    pub fn records(&self) -> u64 {
        self.n
    }

    /// Whether the file is currently open for append.
    #[must_use]
    pub fn is_open(&self) -> bool {
        self.file.is_some()
    }

    /// Adopt a shared fault injector.
    ///
    /// The sink hands one to every writer it opens so a test can arm a failure
    /// before the file exists. Outside `cfg(test)` [`Faults`] is empty and this
    /// assigns nothing.
    pub(crate) fn set_faults(&mut self, faults: Faults) {
        self.faults = faults;
    }

    /// Write the pending `transcript_gap`, if there is one.
    fn flush_gap(&mut self, seq_after: Option<u64>) -> io::Result<()> {
        if self.pending_gap == 0 {
            return Ok(());
        }
        let record = Record::Gap {
            dropped: self.pending_gap,
            seq_before: self.last_seq,
            seq_after,
        };
        self.emit(&record)?;
        self.pending_gap = 0;
        Ok(())
    }

    /// Render and write one line, advancing `n` only on success.
    fn emit(&mut self, record: &Record) -> io::Result<u64> {
        if self.faults.take() {
            return Err(io::Error::other("injected transcript write failure"));
        }
        let n = self.n + 1;
        let line = Line::render(record, &self.session_id, n, SystemTime::now(), self.budget);
        let mut bytes = serde_json::to_vec(&line).map_err(io::Error::other)?;
        bytes.push(b'\n');
        let Some(file) = self.file.as_mut() else {
            return Err(io::Error::new(
                io::ErrorKind::BrokenPipe,
                "transcript is closed",
            ));
        };
        file.write_all(&bytes)?;
        file.flush()?;
        self.n = n;
        if let Some(seq) = record.seq() {
            self.last_seq = Some(seq);
        }
        Ok(n)
    }
}

/// Create the transcript directory `0o700`, or refuse one that is wider (BR-9).
///
/// The pre-existing case is checked **before** anything is tightened. Tightening
/// first and checking after would turn "the daemon refuses a world-readable
/// transcript directory" into "the daemon silently fixes it", which AC-11 wants
/// the other way round: a directory the user pointed at and left open is a
/// configuration mistake they should be told about, not one to paper over.
fn prepare_dir(dir: &Path) -> Result<(), Refused> {
    match std::fs::symlink_metadata(dir) {
        Ok(meta) => {
            if meta.file_type().is_symlink() {
                return Err(Refused::Symlink {
                    path: dir.to_path_buf(),
                });
            }
            if !meta.is_dir() {
                return Err(Refused::NotA {
                    path: dir.to_path_buf(),
                    expected: "directory",
                });
            }
            let mode = meta.permissions().mode() & 0o777;
            if mode & NON_OWNER_BITS != 0 {
                return Err(Refused::Mode {
                    path: dir.to_path_buf(),
                    mode,
                });
            }
            Ok(())
        }
        Err(err) if err.kind() == io::ErrorKind::NotFound => {
            // The creation half of `secure_socket_dir`: every missing component
            // is created `0o700` from the start and the mode is then forced
            // regardless of the umask, so there is no window in which the
            // directory is traversable by another user.
            crate::auth::secure_socket_dir(dir).map_err(|source| Refused::Io {
                path: dir.to_path_buf(),
                source,
            })
        }
        Err(source) => Err(Refused::Io {
            path: dir.to_path_buf(),
            source,
        }),
    }
}

/// Open `path` for append, creating it `0o600`, or refuse a wider one (BR-9,
/// AC-13).
///
/// `symlink_metadata` before the open, so a symlink at the path is refused
/// rather than followed, and a pre-existing `0o644` file is refused rather than
/// appended to. The mode is then forced with
/// [`crate::auth::secure_socket_permissions`] because `OpenOptions::mode` is
/// masked by the umask, which differs between a developer's machine and CI's
/// ubuntu leg.
fn open_owner_only(path: &Path) -> Result<File, Refused> {
    match std::fs::symlink_metadata(path) {
        Ok(meta) => {
            if meta.file_type().is_symlink() {
                return Err(Refused::Symlink {
                    path: path.to_path_buf(),
                });
            }
            if !meta.is_file() {
                return Err(Refused::NotA {
                    path: path.to_path_buf(),
                    expected: "file",
                });
            }
            let mode = meta.permissions().mode() & 0o777;
            if mode & NON_OWNER_BITS != 0 {
                return Err(Refused::Mode {
                    path: path.to_path_buf(),
                    mode,
                });
            }
        }
        Err(err) if err.kind() == io::ErrorKind::NotFound => {}
        Err(source) => {
            return Err(Refused::Io {
                path: path.to_path_buf(),
                source,
            })
        }
    }

    // `O_NOFOLLOW` closes the window between the `symlink_metadata` check above
    // and this open: a same-UID process that swaps a symlink in between would
    // otherwise redirect the transcript's bytes into any file the user owns
    // (verify finding, REQ-611 BR-9).
    let file = OpenOptions::new()
        .create(true)
        .append(true)
        .mode(0o600)
        .custom_flags(libc::O_NOFOLLOW)
        .open(path)
        .map_err(|source| Refused::Io {
            path: path.to_path_buf(),
            source,
        })?;
    crate::auth::secure_socket_permissions(path).map_err(|source| Refused::Io {
        path: path.to_path_buf(),
        source,
    })?;
    Ok(file)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::transcript::record::{PromptSubmitted, ToolResult};
    use serde_json::Value;
    use teton_protocol::events::ToolCallStatus;
    use teton_protocol::methods::PromptBlock;
    use teton_protocol::TurnId;

    /// A scratch directory no other test in this process collides with — the
    /// house pattern (`web::cache::tests::scratch_base`,
    /// `cost::ledger::tests::scratch_db`): a counter, not a timestamp, because
    /// two calls inside one clock tick return the same instant.
    fn scratch(tag: &str) -> PathBuf {
        use std::sync::atomic::{AtomicU64, Ordering};
        static NEXT: AtomicU64 = AtomicU64::new(0);
        let dir = std::env::temp_dir().join(format!(
            "teton-transcript-writer-{}-{tag}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        let _ = std::fs::remove_dir_all(&dir);
        dir
    }

    fn session() -> SessionId {
        SessionId::from("sess-0123456789abcdefghjkmnpqrs")
    }

    fn opened() -> Opened {
        Opened {
            daemon_version: "0.1.28".to_owned(),
            root: "/repo".to_owned(),
            redact: false,
            max_record_bytes: 65_536,
            seq_at_open: 12,
        }
    }

    fn at(secs: u64) -> SystemTime {
        std::time::UNIX_EPOCH + std::time::Duration::from_secs(secs)
    }

    /// The mode bits of `path`, without following a symlink.
    fn mode_of(path: &Path) -> u32 {
        std::fs::symlink_metadata(path)
            .expect("metadata")
            .permissions()
            .mode()
            & 0o777
    }

    /// Parse a transcript file into one JSON value per line.
    fn lines(path: &Path) -> Vec<Value> {
        std::fs::read_to_string(path)
            .expect("read transcript")
            .lines()
            .map(|line| serde_json::from_str(line).expect("every line is standalone JSON"))
            .collect()
    }

    /// AC-13 / BR-9 — the directory is `0o700` and the file `0o600` after
    /// `open`, whatever the umask.
    ///
    /// The umask is what makes this worth asserting: `DirBuilder::mode` and
    /// `OpenOptions::mode` are both masked by it, so a machine with a laxer
    /// umask than the developer's would create a `0o755` directory and this is
    /// the only place that would notice.
    #[test]
    fn open_creates_owner_only_dir_and_file() {
        let dir = scratch("modes");
        let writer = Writer::open(&dir, &session(), at(1_756_900_272), opened())
            .expect("a fresh directory opens");

        assert_eq!(mode_of(&dir), 0o700, "the transcript directory is 0700");
        assert_eq!(mode_of(writer.path()), 0o600, "the transcript file is 0600");
        assert_eq!(
            writer.path().file_name().and_then(|n| n.to_str()),
            Some("20250903T115112Z-sess-0123456789abcdefghjkmnpqrs.jsonl"),
            "the name is the stamp the prune pattern matches"
        );

        let lines = lines(writer.path());
        assert_eq!(lines.len(), 1, "open writes exactly one record");
        assert_eq!(lines[0]["kind"], "transcript_opened");
        assert_eq!(lines[0]["daemon_version"], "0.1.28");
        assert_eq!(lines[0]["root"], "/repo");
        assert_eq!(lines[0]["redact"], false);
        assert_eq!(lines[0]["max_record_bytes"], 65_536);
        assert_eq!(lines[0]["seq_at_open"], 12);
        assert_eq!(lines[0]["session_id"], "sess-0123456789abcdefghjkmnpqrs");

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// BR-9 / AC-13 — a pre-existing `0o644` file at the target path is refused
    /// and never appended to.
    ///
    /// The second assertion is the one with teeth: refusing and *then* writing
    /// anyway would still return an error, and a test that only checked the
    /// return value would pass. The planted content must survive byte for byte.
    ///
    /// **Shown to fail** (mutation, restored): dropping the `mode &
    /// NON_OWNER_BITS != 0` arm from [`open_owner_only`] makes this red on both
    /// legs — `a wider-than-owner-only file is refused` and then the appended
    /// bytes.
    #[test]
    fn open_refuses_a_wider_than_owner_only_file() {
        let dir = scratch("wide-file");
        std::fs::create_dir_all(&dir).expect("scratch dir");
        std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o700)).expect("dir mode");
        let path = dir.join("20250903T115112Z-sess-0123456789abcdefghjkmnpqrs.jsonl");
        std::fs::write(&path, b"planted\n").expect("plant a file");
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644)).expect("file mode");

        let refused = Writer::open(&dir, &session(), at(1_756_900_272), opened())
            .expect_err("a wider-than-owner-only file is refused");
        assert!(
            matches!(refused, Refused::Mode { mode: 0o644, .. }),
            "the refusal names the mode it found: {refused:?}"
        );
        assert_eq!(
            std::fs::read(&path).expect("planted file survives"),
            b"planted\n",
            "a refused file is not appended to"
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// BR-9 / AC-11 — a pre-existing directory wider than owner-only is refused
    /// rather than tightened.
    ///
    /// The distinction matters: [`crate::auth::secure_socket_dir`] tightens such
    /// a directory best-effort, which is right for a socket and wrong here.
    #[test]
    fn open_refuses_a_wider_than_owner_only_directory() {
        let dir = scratch("wide-dir");
        std::fs::create_dir_all(&dir).expect("scratch dir");
        std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o755)).expect("dir mode");

        let refused = Writer::open(&dir, &session(), at(1_756_900_272), opened())
            .expect_err("a world-readable transcript directory is refused");
        assert!(
            matches!(refused, Refused::Mode { mode: 0o755, .. }),
            "the refusal names the directory's mode: {refused:?}"
        );
        assert_eq!(
            mode_of(&dir),
            0o755,
            "a refused directory is left as the user set it, not silently tightened"
        );
        assert!(
            std::fs::read_dir(&dir).expect("listing").next().is_none(),
            "no transcript file is created in a refused directory"
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// AC-17 / BR-14 — every line parses on its own with a stock JSON parser and
    /// carries `n`, `ts`, `session_id` and `kind`; `n` runs from 1 with no holes
    /// across open, append, close, resume and close again.
    ///
    /// Parsed with `serde_json::from_str` per line and asserted on the raw
    /// values — no teton type is deserialized, because AC-17's claim is about
    /// what a reader with no teton code can do with the file.
    #[test]
    fn every_line_parses_standalone_and_n_is_contiguous() {
        let dir = scratch("contiguous");
        let mut writer = Writer::open(&dir, &session(), at(1_756_900_272), opened())
            .expect("a fresh directory opens");

        writer
            .append(&Record::PromptSubmitted(PromptSubmitted {
                turn_id: TurnId::from("turn-1"),
                prompt: vec![PromptBlock::Text {
                    text: "hello".to_owned(),
                }],
                skill: None,
            }))
            .expect("append a prompt");
        writer
            .append(&Record::ToolResult(ToolResult {
                tool_call_id: "call-1".to_owned(),
                status: ToolCallStatus::Completed,
                output: "ok".to_owned(),
            }))
            .expect("append a tool result");
        writer
            .close(CloseReason::SessionCommand)
            .expect("close on /transcript off");
        writer.resume(99).expect("resume on /transcript on");
        writer
            .close(CloseReason::SessionEnded)
            .expect("close at session end");

        let lines = lines(writer.path());
        let kinds: Vec<&str> = lines
            .iter()
            .map(|line| line["kind"].as_str().expect("every line has a kind"))
            .collect();
        assert_eq!(
            kinds,
            vec![
                "transcript_opened",
                "prompt_submitted",
                "tool_result",
                "transcript_closed",
                "transcript_resumed",
                "transcript_closed",
            ],
            "a close/resume pair stays in one file"
        );
        for (index, line) in lines.iter().enumerate() {
            let n = u64::try_from(index + 1).expect("small index");
            assert_eq!(line["n"], n, "n runs from 1 with no holes");
            assert_eq!(line["session_id"], "sess-0123456789abcdefghjkmnpqrs");
            assert!(
                line["ts"]
                    .as_str()
                    .is_some_and(|ts| ts.ends_with('Z') && ts.len() == 24),
                "every line carries an RFC 3339 UTC ts: {:?}",
                line["ts"]
            );
        }
        assert_eq!(
            lines[5]["records"], 6,
            "transcript_closed states the final n"
        );
        assert_eq!(writer.records(), 6);

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// BR-5 — a noted drop becomes one `transcript_gap` written *before* the
    /// next record, and the gap carries the seq either side of the hole.
    ///
    /// The writer half of the rule; the sink half (a producer's `try_send`
    /// failing) is `mod.rs`'s
    /// `a_dropped_run_becomes_one_gap_record_and_n_stays_contiguous`.
    #[test]
    fn a_noted_drop_is_written_before_the_next_record() {
        let dir = scratch("gap");
        let mut writer = Writer::open(&dir, &session(), at(1_756_900_272), opened())
            .expect("a fresh directory opens");

        writer.note_dropped(3);
        writer
            .append(&Record::ToolResult(ToolResult {
                tool_call_id: "call-1".to_owned(),
                status: ToolCallStatus::Completed,
                output: "ok".to_owned(),
            }))
            .expect("append after a drop");

        let lines = lines(writer.path());
        assert_eq!(lines[1]["kind"], "transcript_gap");
        assert_eq!(lines[1]["dropped"], 3);
        assert_eq!(lines[1]["n"], 2);
        assert_eq!(lines[2]["kind"], "tool_result");
        assert_eq!(lines[2]["n"], 3);

        let _ = std::fs::remove_dir_all(&dir);
    }
}
