//! REQ-611 — the daemon-side transcript sink: one writer per daemon, one file
//! per session (architecture ADR-1, ADR-2, ADR-3, ADR-8).
//!
//! # The shape, and why each piece is that shape
//!
//! [`TranscriptSink`] is a **handle**: a bounded [`tokio::sync::mpsc`] sender
//! (4096 records) plus a small shared table of per-session facts. Behind it, one
//! writer owns every open file and drains the channel.
//!
//! * **Producers never block and never fail.** [`TranscriptSink::record`]
//!   returns `()` (ADR-8) and reaches the channel with `try_send` only. The bus
//!   tap TASK-363 installs calls it *under the bus mutex*, so anything that
//!   could wait there would stall every publisher in the daemon (LESSON-518).
//!   A full channel is not an error and not an eviction (LESSON-513): the record
//!   is counted as dropped and the count becomes a `transcript_gap` line in
//!   front of the next record that lands (BR-5).
//! * **The writer is an OS thread, not a tokio task.** It does blocking file
//!   I/O; on a runtime worker that would block the executor, and
//!   `spawn_blocking` per record would trade a thread hop for every line of
//!   streamed model text. `Receiver::blocking_recv` on a dedicated thread is the
//!   shape that makes BR-5's promise — *a slow disk delays the file, not the
//!   session* — a property of the plumbing rather than of the disk being fast.
//! * **One channel for the bus tap and the in-process hand-offs** (ADR-2). A
//!   `tool_result` handed in from the turn path and the `session_update`
//!   envelope that preceded it arrive at one queue, so the file's `n` order is
//!   the order the turn produced them.
//!
//! # Who owns what (ADR-3)
//!
//! The sink owns per-session transcript state; the session registry owns none of
//! it. The split *inside* the sink is deliberate too: the shared table holds
//! only cheap facts (`enabled`, path, counts, degraded reason) behind a mutex
//! that is never held across I/O, while the file handles live in the writer
//! thread's own map. A `File` under a lock that an RPC handler reads is a lock
//! held for the length of a disk write.
//!
//! # What this module does not do
//!
//! No bus, no session registry, no runtime. TASK-363 installs the tap, calls
//! [`TranscriptSink::session_created`] from the registry, publishes
//! `transcript_state` from the [`SinkHooks`] callback, and hands the turn path
//! its `record` calls.

pub mod record;
pub mod retention;
pub mod writer;

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::SystemTime;

use teton_core::config::TranscriptConfig;
use teton_protocol::events::{EventEnvelope, TranscriptStateReason};
use teton_protocol::SessionId;
use tokio::sync::{mpsc, oneshot};

pub use record::{
    CloseReason, Opened, PermissionDecided, PromptSubmitted, Record, ToolCallInput, ToolResult,
};
pub use retention::{prune, PruneReport};
pub use writer::{Refused, Writer};

use writer::Faults;

/// How many records the sink's channel holds (ADR-1).
///
/// Sized in **records, not bytes**, and the architecture's risk register says
/// so: a burst of mebibyte tool results can hold tens of mebibytes in flight,
/// because BR-12's truncation happens in the writer so that the rule has one
/// home. A byte-budgeted channel is the follow-up if that is ever measured.
pub const CHANNEL_CAPACITY: usize = 4_096;

/// What the sink needs to know to write files (ADR-4).
///
/// Built from the `[transcript]` table plus the resolved data directory, which
/// is [`TranscriptConfig::effective_dir`]'s job — this struct holds the
/// *resolved* directory so nothing downstream re-derives a path.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SinkConfig {
    /// Where transcripts are written.
    pub dir: PathBuf,
    /// Days a file is kept; `0` never prunes (BR-13).
    pub retain_days: u32,
    /// The per-field content budget before truncation (BR-12).
    pub max_record_bytes: usize,
    /// The daemon version, recorded in `transcript_opened`.
    pub daemon_version: String,
    /// The `[privacy] redact` posture at open, recorded in `transcript_opened`
    /// (BR-11). It does **not** gate a write; it tells a reader what the egress
    /// side was doing while this file was written.
    pub redact: bool,
}

impl SinkConfig {
    /// Resolve a `[transcript]` table against a data directory.
    #[must_use]
    pub fn new(
        transcript: &TranscriptConfig,
        data_dir: &Path,
        redact: bool,
        daemon_version: String,
    ) -> Self {
        Self {
            dir: transcript.effective_dir(data_dir),
            retain_days: transcript.retain_days,
            max_record_bytes: transcript.max_record_bytes,
            daemon_version,
            redact,
        }
    }
}

/// A session the sink should start tracking (ADR-3).
///
/// A named bundle rather than four positional arguments, which is the house rule
/// for a parameter cluster (`turn_context.rs`, and `suppression_ratchet.rs`
/// refuses new unnamed ones).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NewSession {
    /// The session.
    pub session_id: SessionId,
    /// Its root, in display form, recorded in `transcript_opened`.
    pub root: String,
    /// Whether it records from the start — the `[transcript] enabled` default
    /// read at session creation (BR-2).
    pub enabled: bool,
    /// The bus sequence number at creation, so `transcript_opened` can say where
    /// in the daemon-wide numbering this file begins.
    pub seq_at_open: u64,
}

/// Why a session's transcript stopped without being asked to (BR-6, ADR-8).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Degradation {
    /// The session it happened to.
    pub session_id: SessionId,
    /// `write_failure` or `dir_refused` — the two the user did not choose.
    pub reason: TranscriptStateReason,
    /// One sentence naming what went wrong, for the session line and
    /// `/transcript` status.
    pub detail: String,
}

/// What the sink calls when it stops recording on its own (ADR-8).
///
/// A callback rather than a bus handle because this module has no bus: TASK-363
/// installs one that publishes `transcript_state { enabled: false, reason }` and
/// prints the session's one line. It fires **exactly once** per session — a
/// notice that repeats is one users learn to read past (LESSON-513).
pub struct SinkHooks {
    /// Called once, the first time a session's transcript degrades.
    pub on_degraded: Box<dyn Fn(&Degradation) + Send + Sync>,
}

impl Default for SinkHooks {
    fn default() -> Self {
        Self {
            on_degraded: Box::new(|_| {}),
        }
    }
}

impl std::fmt::Debug for SinkHooks {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("SinkHooks { on_degraded: <fn> }")
    }
}

/// What `/transcript` reports for one session (BR-15, ADR-6).
///
/// The path is in here and deliberately **not** in the `transcript_state` event:
/// state is news, location is answered on the asking connection only.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct TranscriptStatus {
    /// The effective state — the config default, then any `/transcript`
    /// override, then `false` if the sink degraded.
    pub enabled: bool,
    /// The file, once opened.
    pub path: Option<PathBuf>,
    /// Records written for this session.
    pub records: u64,
    /// Records dropped for this session, cumulative (BR-5).
    pub dropped: u64,
    /// Why writing stopped, if it did (BR-6).
    pub degraded: Option<String>,
}

/// The per-session facts the handle and the writer share.
///
/// Atomics for everything on the hot path, so [`TranscriptSink::record`] can
/// decide whether to send without waiting on anything the writer holds.
#[derive(Debug, Default)]
struct Slot {
    /// The effective toggle.
    enabled: AtomicBool,
    /// Records dropped and **not yet** turned into a `transcript_gap`. The
    /// writer swaps this to zero as it writes the gap.
    pending_drops: AtomicU64,
    /// Every drop this session has had, for `/transcript` status.
    total_drops: AtomicU64,
    /// The last `n` written.
    records: AtomicU64,
    /// The facts nobody reads on the hot path.
    detail: Mutex<SlotDetail>,
}

/// [`Slot`]'s cold half.
#[derive(Debug, Default, Clone)]
struct SlotDetail {
    /// The session root, for `transcript_opened`.
    root: String,
    /// The bus seq at open, for `transcript_opened`.
    seq_at_open: u64,
    /// The file, once the writer has opened it.
    path: Option<PathBuf>,
    /// Why writing stopped (BR-6).
    degraded: Option<String>,
}

/// The table both halves of the sink read.
#[derive(Debug, Default)]
struct Shared {
    /// How many sessions are currently recording.
    ///
    /// The fast path: with nothing recording, [`TranscriptSink::record`] returns
    /// on one relaxed load and never touches the map. BR-1's "zero overhead"
    /// is about a daemon that never constructs a sink at all; this is what makes
    /// a *constructed* sink with nothing switched on cost the same.
    recording: AtomicUsize,
    /// Per-session state, keyed by [`SessionId`] (ADR-3).
    slots: Mutex<HashMap<SessionId, Arc<Slot>>>,
}

impl Shared {
    /// The slot for `session_id`, if the sink knows the session.
    ///
    /// A session it does not know is one [`TranscriptSink::session_created`] was
    /// never called for, and BR-7 says its records are dropped rather than
    /// written to a file invented for them.
    fn slot(&self, session_id: &SessionId) -> Option<Arc<Slot>> {
        self.slots
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .get(session_id)
            .map(Arc::clone)
    }
}

/// What travels down the sink's one channel.
///
/// Control messages share the record channel so that ordering is total: a
/// `SetEnabled { enabled: false }` cannot overtake the records that preceded it
/// and close the file early.
enum Message {
    /// Start tracking a session (and open its file if it records).
    Created(NewSession),
    /// One record for one session.
    Record {
        /// The session the record belongs to.
        session_id: SessionId,
        /// The record.
        record: Box<Record>,
        /// Drops counted **before this record was enqueued** (BR-5).
        ///
        /// Carried on the message rather than read off the shared slot when the
        /// writer gets here, because the drop's *position* is the whole point: a
        /// count read at write time would attribute drops to a hole ahead of
        /// records that were already queued when they happened, and the gap
        /// would land in the wrong place in a file whose ordering is its value.
        drops_before: u64,
    },
    /// `/transcript on` / `off` for one session (BR-2).
    SetEnabled {
        /// The session.
        session_id: SessionId,
        /// Its new effective state.
        enabled: bool,
        /// The bus seq at the switch, for `transcript_resumed`.
        seq: u64,
    },
    /// The session ended.
    Closed {
        /// The session.
        session_id: SessionId,
        /// Why.
        reason: CloseReason,
        /// Drops counted before the session was forgotten, so the last thing in
        /// the file before `transcript_closed` is an honest gap record (BR-5).
        drops_before: u64,
    },
    /// Answer when everything queued ahead of this has been written.
    Flush(oneshot::Sender<()>),
    /// Close every open file and stop (AC-18).
    Shutdown(oneshot::Sender<()>),
}

/// The daemon's transcript sink (ADR-1, ADR-3).
///
/// Cheap to clone: every clone is another handle onto the same channel and the
/// same shared table.
#[derive(Debug, Clone)]
pub struct TranscriptSink {
    tx: mpsc::Sender<Message>,
    shared: Arc<Shared>,
}

impl TranscriptSink {
    /// Start a sink and its writer thread.
    #[must_use]
    pub fn spawn(config: SinkConfig) -> Self {
        Self::spawn_with(config, SinkHooks::default())
    }

    /// Start a sink whose degradations reach `hooks` (ADR-8).
    #[must_use]
    pub fn spawn_with(config: SinkConfig, hooks: SinkHooks) -> Self {
        Self::spawn_inner(config, hooks, Faults::default())
    }

    /// Start a sink whose writers share `faults`, so a test can arm a write
    /// failure before the file exists.
    #[cfg(test)]
    fn spawn_faulty(config: SinkConfig, hooks: SinkHooks, faults: Faults) -> Self {
        Self::spawn_inner(config, hooks, faults)
    }

    fn spawn_inner(config: SinkConfig, hooks: SinkHooks, faults: Faults) -> Self {
        let (tx, rx) = mpsc::channel(CHANNEL_CAPACITY);
        let shared = Arc::new(Shared::default());
        let sink = Self {
            tx,
            shared: Arc::clone(&shared),
        };
        let thread = std::thread::Builder::new()
            .name("teton-transcript".to_owned())
            .spawn(move || {
                WriterTask {
                    config,
                    hooks,
                    faults,
                    shared,
                    files: HashMap::new(),
                }
                .run(rx);
            });
        if let Err(err) = thread {
            // The receiver was moved into the closure that failed to spawn, so
            // it is already dropped and every `try_send` below fails — which the
            // producers count as drops. Loud rather than fatal: a daemon that
            // cannot spawn a thread has larger problems than its transcript, and
            // BR-6's posture is that the recording stops, not the session.
            eprintln!(
                "transcript: writer thread could not start ({err}); no transcript will be written"
            );
        }
        sink
    }

    /// Track a session, opening its file when it records from the start (BR-2).
    pub fn session_created(&self, session: NewSession) {
        let slot = Arc::new(Slot::default());
        slot.enabled.store(session.enabled, Ordering::Relaxed);
        {
            let mut detail = slot
                .detail
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            detail.root.clone_from(&session.root);
            detail.seq_at_open = session.seq_at_open;
        }
        let replaced = self
            .shared
            .slots
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .insert(session.session_id.clone(), slot);
        if let Some(previous) = replaced {
            if previous.enabled.swap(false, Ordering::Relaxed) {
                self.shared.recording.fetch_sub(1, Ordering::Relaxed);
            }
        }
        if session.enabled {
            self.shared.recording.fetch_add(1, Ordering::Relaxed);
        }
        let session_id = session.session_id.clone();
        self.control(&session_id, Message::Created(session));
    }

    /// Record one thing for one session. Fire and forget (ADR-8).
    ///
    /// Returns `()`, and that is the point: a turn must not die because a disk
    /// is full, and a `Result` here would make "never fails the turn" a property
    /// every call site had to remember rather than a property of the type
    /// (BR-6, LESSON-505).
    pub fn record(&self, session_id: &SessionId, record: Record) {
        if self.shared.recording.load(Ordering::Relaxed) == 0 {
            return;
        }
        let Some(slot) = self.shared.slot(session_id) else {
            return;
        };
        if !slot.enabled.load(Ordering::Relaxed) {
            return;
        }
        // Claim the drops counted so far and pin them to this record's position
        // in the queue. If the send then fails, they go back with this record
        // added to them — the count is never lost and never moves earlier.
        let drops_before = slot.pending_drops.swap(0, Ordering::Relaxed);
        let message = Message::Record {
            session_id: session_id.clone(),
            record: Box::new(record),
            drops_before,
        };
        if self.tx.try_send(message).is_err() {
            slot.pending_drops
                .fetch_add(drops_before.saturating_add(1), Ordering::Relaxed);
            slot.total_drops.fetch_add(1, Ordering::Relaxed);
        }
    }

    /// Report that `count` records never reached the sink (BR-5, ADR-1).
    ///
    /// The bus tap calls this when its own `try_send` fails, and
    /// [`TranscriptSink::record`] calls it for the same reason. The next record
    /// that lands is preceded by one `transcript_gap { dropped: count }`, so the
    /// per-file `n` never has a hole — the drop is *counted*, which is the whole
    /// difference between this and an evicted subscriber (LESSON-513).
    pub fn dropped(&self, session_id: &SessionId, count: u64) {
        if count == 0 {
            return;
        }
        if let Some(slot) = self.shared.slot(session_id) {
            slot.pending_drops.fetch_add(count, Ordering::Relaxed);
            slot.total_drops.fetch_add(count, Ordering::Relaxed);
        }
    }

    /// Switch a session's transcript on or off (BR-2).
    ///
    /// Session-lifetime and never persisted: `config.toml` is untouched by this
    /// call. Turning one on after an `off` resumes the **same** file with `n`
    /// continuing (AC-4).
    ///
    /// **A degraded session does not come back on.** BR-6 says the sink stops
    /// for that session, and [`TranscriptStatus::enabled`] is defined as the
    /// *effective* state — so honouring a `/transcript on` here would set a flag
    /// the writer will refuse to act on and leave `/transcript` telling the user
    /// they are recording when no byte will ever be written. The refusal is
    /// silent at this layer; TASK-363's handler is the surface that says why.
    pub fn set_enabled(&self, session_id: &SessionId, enabled: bool, seq: u64) {
        let Some(slot) = self.shared.slot(session_id) else {
            return;
        };
        if enabled
            && slot
                .detail
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .degraded
                .is_some()
        {
            return;
        }
        if slot.enabled.swap(enabled, Ordering::Relaxed) != enabled {
            if enabled {
                self.shared.recording.fetch_add(1, Ordering::Relaxed);
            } else {
                self.shared.recording.fetch_sub(1, Ordering::Relaxed);
            }
        }
        self.control(
            session_id,
            Message::SetEnabled {
                session_id: session_id.clone(),
                enabled,
                seq,
            },
        );
    }

    /// Close a session's transcript and forget the session (ADR-3).
    pub fn session_closed(&self, session_id: &SessionId, reason: CloseReason) {
        let removed = self
            .shared
            .slots
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .remove(session_id);
        let drops_before = match removed {
            Some(slot) => {
                if slot.enabled.swap(false, Ordering::Relaxed) {
                    self.shared.recording.fetch_sub(1, Ordering::Relaxed);
                }
                slot.pending_drops.swap(0, Ordering::Relaxed)
            }
            None => 0,
        };
        self.control(
            session_id,
            Message::Closed {
                session_id: session_id.clone(),
                reason,
                drops_before,
            },
        );
    }

    /// What `/transcript` reports for this session (BR-15).
    #[must_use]
    pub fn status(&self, session_id: &SessionId) -> Option<TranscriptStatus> {
        let slot = self.shared.slot(session_id)?;
        let detail = slot
            .detail
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone();
        Some(TranscriptStatus {
            enabled: slot.enabled.load(Ordering::Relaxed),
            path: detail.path,
            records: slot.records.load(Ordering::Relaxed),
            dropped: slot.total_drops.load(Ordering::Relaxed),
            degraded: detail.degraded,
        })
    }

    /// Wait until everything queued so far has been written.
    ///
    /// The synchronisation point a caller needs before *reading* the file or
    /// reporting a path: the writer is a separate thread, so a record handed
    /// over a microsecond ago may not be on disk yet. `await`s rather than
    /// `try_send`s, because unlike a record this is allowed to wait.
    pub async fn flush(&self) {
        let (tx, rx) = oneshot::channel();
        if self.tx.send(Message::Flush(tx)).await.is_ok() {
            let _ = rx.await;
        }
    }

    /// Close every open transcript with `daemon_shutdown` and stop the writer
    /// (AC-18).
    pub async fn shutdown(&self) {
        let (tx, rx) = oneshot::channel();
        if self.tx.send(Message::Shutdown(tx)).await.is_ok() {
            let _ = rx.await;
        }
    }

    /// Send a control message, counting a full channel as a drop for the
    /// session it concerned.
    ///
    /// Control messages are rare — one per session creation, switch and close —
    /// and losing one to a full channel needs a disk stalled behind 4096 records
    /// at that instant. It is counted rather than retried because a retry that
    /// waited would put the session registry behind the disk, which is the one
    /// thing this design refuses everywhere else.
    fn control(&self, session_id: &SessionId, message: Message) {
        if self.tx.try_send(message).is_err() {
            self.dropped(session_id, 1);
        }
    }
}

/// The sink **is** the bus tap (ADR-1).
///
/// One method, one `try_send`, and deliberately nothing else — `observe` runs
/// under the bus mutex, so every publisher in the daemon waits on whatever this
/// body does (LESSON-518). The envelope clone is the whole cost, and it is the
/// cost of recording the wire form verbatim: a record built by re-serializing
/// the event here would be this daemon's second rendering of it rather than the
/// bytes a fully attached client saw.
///
/// [`TranscriptSink::record`] answers the rest: it drops out on the recording
/// fast path when nothing is switched on, refuses a session it does not know,
/// and counts a full channel as a drop that becomes the next record's
/// `transcript_gap` (BR-5).
///
/// The `session_id` check is a second seam for BR-7, and it stays. `publish`
/// offers only session-scoped envelopes, and the writer refuses one scoped to
/// another session — three checks for one rule, because the rule is *whose
/// file this is* and each seam sees a different half of it (LESSON-502).
impl crate::broadcast::EventTap for TranscriptSink {
    fn observe(&self, envelope: &EventEnvelope) {
        let Some(session_id) = envelope.session_id.clone() else {
            return;
        };
        self.record(&session_id, Record::BusEnvelope(envelope.clone()));
    }
}

/// The writer thread's own state: the files, and nothing shared.
struct WriterTask {
    config: SinkConfig,
    hooks: SinkHooks,
    faults: Faults,
    shared: Arc<Shared>,
    files: HashMap<SessionId, Entry>,
}

/// One session's file, as the writer thread sees it.
struct Entry {
    /// `None` until the file is opened for the first time. A [`Writer`] survives
    /// a `/transcript off` so that a later `on` resumes the same file (AC-4).
    writer: Option<Writer>,
    /// When the session started, which names the file.
    started_at: SystemTime,
    /// The effective toggle **in channel order**.
    ///
    /// Distinct from the shared slot's flag on purpose. The handle's flag flips
    /// the instant `/transcript off` is called, while records enqueued a moment
    /// earlier are still in the queue; those belong in the file, because BR-2
    /// says the switch takes effect *at the next record*, not retroactively.
    /// This copy moves only when the `SetEnabled` message reaches its place in
    /// the queue, which is exactly that boundary.
    enabled: bool,
    /// Set on the first failure or refusal; every later record for this session
    /// is a no-op (BR-6, ADR-8).
    degraded: bool,
}

impl WriterTask {
    /// Drain the channel until it closes or a [`Message::Shutdown`] arrives.
    ///
    /// `blocking_recv` on a dedicated thread — see the module doc on why this is
    /// not a tokio task.
    fn run(mut self, mut rx: mpsc::Receiver<Message>) {
        // BR-13: prune at daemon start, before any file is opened.
        let _ = prune(&self.config.dir, self.config.retain_days, SystemTime::now());
        while let Some(message) = rx.blocking_recv() {
            match message {
                Message::Created(session) => self.on_created(&session),
                Message::Record {
                    session_id,
                    record,
                    drops_before,
                } => self.on_record(&session_id, *record, drops_before),
                Message::SetEnabled {
                    session_id,
                    enabled,
                    seq,
                } => self.on_set_enabled(&session_id, enabled, seq),
                Message::Closed {
                    session_id,
                    reason,
                    drops_before,
                } => self.on_closed(&session_id, reason, drops_before),
                Message::Flush(reply) => {
                    for entry in self.files.values_mut() {
                        if let Some(writer) = entry.writer.as_mut() {
                            let _ = writer.flush();
                        }
                    }
                    let _ = reply.send(());
                }
                Message::Shutdown(reply) => {
                    self.close_all(CloseReason::DaemonShutdown);
                    let _ = reply.send(());
                    return;
                }
            }
        }
        // Every handle was dropped without a shutdown — close what is open so a
        // reader is never left guessing whether a file was truncated.
        self.close_all(CloseReason::DaemonShutdown);
    }

    /// A session appeared. Open its file if it records from the start.
    fn on_created(&mut self, session: &NewSession) {
        self.files.insert(
            session.session_id.clone(),
            Entry {
                writer: None,
                started_at: SystemTime::now(),
                enabled: session.enabled,
                degraded: false,
            },
        );
        if session.enabled {
            self.ensure_open(&session.session_id, None);
        }
    }

    /// One record. Every reason not to write it is checked before the file is
    /// touched.
    fn on_record(&mut self, session_id: &SessionId, record: Record, drops_before: u64) {
        let Some(slot) = self.shared.slot(session_id) else {
            // BR-7: an unknown session's record is dropped, never written to a
            // file invented for it.
            return;
        };
        // Off in **both** views is off. The entry's flag is the channel-ordered
        // one and settles a record that raced a `/transcript off`; the slot's is
        // the fallback for the far rarer case of a `SetEnabled` message lost to
        // a full channel, where the entry's copy is the stale one.
        let disabled_here = self
            .files
            .get(session_id)
            .is_some_and(|entry| !entry.enabled);
        if disabled_here && !slot.enabled.load(Ordering::Relaxed) {
            return;
        }
        // BR-7: one file, one session. A bus envelope carries its own scope, and
        // an envelope scoped to another session — or to none, which is what a
        // daemon-scoped event looks like — is not this file's to hold.
        if matches!(record, Record::BusEnvelope(_)) && record.envelope_session() != Some(session_id)
        {
            return;
        }
        if !self.ensure_open(session_id, None) {
            return;
        }
        // Scoped so the writer's borrow of `self.files` ends before `degrade`
        // needs `&mut self`.
        let written = {
            let Some(writer) = self
                .files
                .get_mut(session_id)
                .and_then(|entry| entry.writer.as_mut())
            else {
                return;
            };
            if drops_before > 0 {
                writer.note_dropped(drops_before);
            }
            writer.append(&record)
        };
        match written {
            Ok(n) => slot.records.store(n, Ordering::Relaxed),
            Err(err) => self.degrade(
                session_id,
                TranscriptStateReason::WriteFailure,
                &format!("transcript write failed: {err}"),
            ),
        }
    }

    /// `/transcript on` or `off` for one session (AC-3, AC-4).
    fn on_set_enabled(&mut self, session_id: &SessionId, enabled: bool, seq: u64) {
        if let Some(entry) = self.files.get_mut(session_id) {
            entry.enabled = enabled;
        }
        if enabled {
            self.ensure_open(session_id, Some(seq));
            return;
        }
        let pending = self.take_pending_drops(session_id);
        let closed = {
            let Some(writer) = self
                .files
                .get_mut(session_id)
                .and_then(|entry| entry.writer.as_mut())
            else {
                return;
            };
            writer.note_dropped(pending);
            writer.close(CloseReason::SessionCommand)
        };
        if let Err(err) = closed {
            self.degrade(
                session_id,
                TranscriptStateReason::WriteFailure,
                &format!("transcript write failed: {err}"),
            );
        } else {
            self.publish_records(session_id);
        }
    }

    /// The session ended: close the file and forget it.
    fn on_closed(&mut self, session_id: &SessionId, reason: CloseReason, drops_before: u64) {
        if let Some(mut entry) = self.files.remove(session_id) {
            if let Some(writer) = entry.writer.as_mut() {
                writer.note_dropped(drops_before);
                let _ = writer.close(reason);
            }
        }
    }

    /// Close every open transcript, e.g. at daemon shutdown (AC-18).
    ///
    /// A run of drops still uncounted goes into the file as a `transcript_gap`
    /// ahead of the closing record: a hole the daemon knew about and did not
    /// write down would be exactly the silent gap BR-5 forbids.
    fn close_all(&mut self, reason: CloseReason) {
        let sessions: Vec<SessionId> = self.files.keys().cloned().collect();
        for session_id in sessions {
            let pending = self.take_pending_drops(&session_id);
            let Some(writer) = self
                .files
                .get_mut(&session_id)
                .and_then(|entry| entry.writer.as_mut())
            else {
                continue;
            };
            writer.note_dropped(pending);
            let _ = writer.close(reason);
            let _ = writer.flush();
        }
    }

    /// Take this session's counted-but-unwritten drops (BR-5).
    fn take_pending_drops(&self, session_id: &SessionId) -> u64 {
        self.shared
            .slot(session_id)
            .map_or(0, |slot| slot.pending_drops.swap(0, Ordering::Relaxed))
    }

    /// Make sure this session has an open file, opening or resuming as needed.
    ///
    /// Returns whether the file is open afterwards. A refusal or an I/O failure
    /// degrades the session once (BR-6, AC-11) and returns `false`.
    fn ensure_open(&mut self, session_id: &SessionId, resume_seq: Option<u64>) -> bool {
        // A `Created` message lost to a full channel would otherwise cost the
        // session its whole transcript; the shared slot is authoritative and is
        // never lost, so the entry is rebuilt from it here.
        if !self.files.contains_key(session_id) {
            self.files.insert(
                session_id.clone(),
                Entry {
                    writer: None,
                    started_at: SystemTime::now(),
                    enabled: true,
                    degraded: false,
                },
            );
        }
        // The entry's cheap facts are snapshotted so that no borrow of
        // `self.files` outlives this block: both `degrade` and
        // `publish_records` want `self` back.
        let (degraded, opened_before, started_at) = {
            let entry = self
                .files
                .get_mut(session_id)
                .expect("the entry was just inserted if it was missing");
            (entry.degraded, entry.writer.is_some(), entry.started_at)
        };
        if degraded {
            return false;
        }
        if opened_before {
            // A closed-but-known file: `/transcript on` after an `off` appends
            // `transcript_resumed` to the same file with `n` continuing (AC-4).
            let resumed = {
                let writer = self
                    .files
                    .get_mut(session_id)
                    .and_then(|entry| entry.writer.as_mut())
                    .expect("`opened_before` says this session has a writer");
                if writer.is_open() {
                    return true;
                }
                writer.resume(resume_seq.unwrap_or(0))
            };
            return match resumed {
                Ok(()) => {
                    self.publish_records(session_id);
                    true
                }
                Err(err) => {
                    self.degrade(
                        session_id,
                        TranscriptStateReason::WriteFailure,
                        &format!("transcript could not be resumed: {err}"),
                    );
                    false
                }
            };
        }

        // BR-13: prune at every transcript open, not only at daemon start.
        let _ = prune(&self.config.dir, self.config.retain_days, SystemTime::now());

        let Some(slot) = self.shared.slot(session_id) else {
            return false;
        };
        let (root, seq_at_open) = {
            let detail = slot
                .detail
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            (
                detail.root.clone(),
                resume_seq.unwrap_or(detail.seq_at_open),
            )
        };
        let opened = Opened {
            daemon_version: self.config.daemon_version.clone(),
            root,
            redact: self.config.redact,
            max_record_bytes: self.config.max_record_bytes,
            seq_at_open,
        };
        match Writer::open(&self.config.dir, session_id, started_at, opened) {
            Ok(mut writer) => {
                writer.set_faults(self.faults.clone());
                let path = writer.path().to_path_buf();
                let records = writer.records();
                if let Some(entry) = self.files.get_mut(session_id) {
                    entry.writer = Some(writer);
                }
                let mut detail = slot
                    .detail
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner);
                detail.path = Some(path);
                drop(detail);
                slot.records.store(records, Ordering::Relaxed);
                true
            }
            Err(refused) => {
                self.degrade(
                    session_id,
                    TranscriptStateReason::DirRefused,
                    &refused.to_string(),
                );
                false
            }
        }
    }

    /// Copy the writer's `n` into the shared table, so `/transcript` status can
    /// read it without touching the file.
    fn publish_records(&self, session_id: &SessionId) {
        let Some(entry) = self.files.get(session_id) else {
            return;
        };
        let Some(writer) = entry.writer.as_ref() else {
            return;
        };
        if let Some(slot) = self.shared.slot(session_id) {
            slot.records.store(writer.records(), Ordering::Relaxed);
        }
    }

    /// Stop recording this session, once, and say so (BR-6, ADR-8).
    ///
    /// The order is the one BR-6 states: mark it, attempt one
    /// `transcript_closed { write_failure }` where a file exists, turn the
    /// effective state off, then fire the hook. Guarded by `entry.degraded`, so
    /// the callback fires exactly once however many records follow.
    fn degrade(&mut self, session_id: &SessionId, reason: TranscriptStateReason, detail: &str) {
        // A session that was refused its directory before an entry existed still
        // needs one, so that a second refusal finds `degraded` already set.
        if !self.files.contains_key(session_id) {
            self.files.insert(
                session_id.clone(),
                Entry {
                    writer: None,
                    started_at: SystemTime::now(),
                    enabled: true,
                    degraded: false,
                },
            );
        }
        let already = {
            let entry = self
                .files
                .get_mut(session_id)
                .expect("the entry was just inserted if it was missing");
            let already = entry.degraded;
            entry.degraded = true;
            if !already {
                if let Some(writer) = entry.writer.as_mut() {
                    let _ = writer.close(CloseReason::WriteFailure);
                }
            }
            already
        };
        if already {
            return;
        }
        if let Some(slot) = self.shared.slot(session_id) {
            if slot.enabled.swap(false, Ordering::Relaxed) {
                self.shared.recording.fetch_sub(1, Ordering::Relaxed);
            }
            let mut cold = slot
                .detail
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            cold.degraded = Some(detail.to_owned());
        }
        (self.hooks.on_degraded)(&Degradation {
            session_id: session_id.clone(),
            reason,
            detail: detail.to_owned(),
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::Value;
    use std::os::unix::fs::PermissionsExt as _;
    use teton_protocol::events::{
        Event, EventEnvelope, SessionUpdate, SessionUpdatePayload, ToolCallStatus,
    };
    use teton_protocol::methods::PromptBlock;
    use teton_protocol::TurnId;

    /// A scratch directory no other test in this process collides with; see
    /// `writer::tests::scratch`.
    fn scratch(tag: &str) -> PathBuf {
        static NEXT: AtomicU64 = AtomicU64::new(0);
        let dir = std::env::temp_dir().join(format!(
            "teton-transcript-sink-{}-{tag}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        let _ = std::fs::remove_dir_all(&dir);
        dir
    }

    fn config(dir: &Path) -> SinkConfig {
        SinkConfig {
            dir: dir.to_path_buf(),
            retain_days: 30,
            max_record_bytes: 65_536,
            daemon_version: "0.1.28".to_owned(),
            redact: false,
        }
    }

    fn session(suffix: char) -> SessionId {
        SessionId::from(format!("sess-0123456789abcdefghjkmnpqr{suffix}"))
    }

    fn new_session(session_id: &SessionId, enabled: bool) -> NewSession {
        NewSession {
            session_id: session_id.clone(),
            root: "/repo".to_owned(),
            enabled,
            seq_at_open: 7,
        }
    }

    fn a_prompt(text: &str) -> Record {
        Record::PromptSubmitted(PromptSubmitted {
            turn_id: TurnId::from("turn-1"),
            prompt: vec![PromptBlock::Text {
                text: text.to_owned(),
            }],
            skill: None,
        })
    }

    fn a_result(text: &str) -> Record {
        Record::ToolResult(ToolResult {
            tool_call_id: "call-1".to_owned(),
            status: ToolCallStatus::Completed,
            output: text.to_owned(),
        })
    }

    fn an_envelope(seq: u64, session_id: Option<SessionId>, text: &str) -> Record {
        Record::BusEnvelope(EventEnvelope::new(
            seq,
            session_id,
            Event::SessionUpdate(SessionUpdate {
                update: SessionUpdatePayload::AgentMessageChunk {
                    text: text.to_owned(),
                },
            }),
        ))
    }

    /// Every line of the one file in `dir`, parsed.
    fn lines_at(path: &Path) -> Vec<Value> {
        std::fs::read_to_string(path)
            .expect("read transcript")
            .lines()
            .map(|line| serde_json::from_str(line).expect("standalone JSON"))
            .collect()
    }

    fn kinds(lines: &[Value]) -> Vec<&str> {
        lines
            .iter()
            .map(|line| line["kind"].as_str().expect("kind"))
            .collect()
    }

    /// `n` runs 1..len with no holes.
    fn assert_contiguous(lines: &[Value]) {
        for (index, line) in lines.iter().enumerate() {
            let n = u64::try_from(index + 1).expect("small index");
            assert_eq!(line["n"], n, "n must be contiguous from 1: {line}");
        }
    }

    /// BR-5 — a run of drops becomes exactly one `transcript_gap` written before
    /// the next record, and `n` stays contiguous.
    ///
    /// The drops are reported through the public [`TranscriptSink::dropped`],
    /// which is the same call the bus tap makes when its `try_send` fails, so
    /// this exercises the sink half of BR-5 rather than a private counter.
    ///
    /// **Shown to fail** (mutation, restored): moving `writer.note_dropped` in
    /// `WriterTask::on_record` to *after* the `writer.append` call makes this
    /// red — `the gap is written before the record that follows the hole`,
    /// because the gap then lands at n=4 behind the record at n=3.
    #[tokio::test]
    async fn a_dropped_run_becomes_one_gap_record_and_n_stays_contiguous() {
        let dir = scratch("gap");
        let sink = TranscriptSink::spawn(config(&dir));
        let id = session('s');
        sink.session_created(new_session(&id, true));
        sink.record(&id, a_prompt("first"));

        // Three records the sink never saw — the shape a full channel leaves.
        sink.dropped(&id, 3);

        sink.record(&id, a_result("after the hole"));
        sink.record(&id, a_result("and one more"));
        sink.flush().await;

        let path = sink
            .status(&id)
            .and_then(|status| status.path)
            .expect("the file is open");
        let lines = lines_at(&path);
        assert_eq!(
            kinds(&lines),
            vec![
                "transcript_opened",
                "prompt_submitted",
                "transcript_gap",
                "tool_result",
                "tool_result",
            ],
            "the gap is written before the record that follows the hole"
        );
        assert_eq!(lines[2]["dropped"], 3, "one gap names the whole run");
        assert_eq!(
            lines[2]["seq_before"],
            Value::Null,
            "no bus record preceded the hole, so there is no seq to name"
        );
        assert_contiguous(&lines);
        assert_eq!(
            sink.status(&id).expect("status").dropped,
            3,
            "/transcript status reports the cumulative drop count"
        );

        sink.shutdown().await;
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// BR-6 / ADR-8 — the first write failure degrades the session once, and
    /// never says so twice.
    ///
    /// Exactly one failure is injected, so the `transcript_closed {
    /// write_failure }` BR-6 asks for actually lands and can be asserted rather
    /// than inferred. The claims are the halves of BR-6 that break separately:
    /// the reason is visible in status, the file says why it stopped, no later
    /// record reaches it — including after a `/transcript on` aimed at the
    /// degraded session — and the announcement happens once.
    ///
    /// # The mutations, run and counted
    ///
    /// "Announced once" is **over-determined**, and the conventions say to write
    /// the number down rather than assume the assertion guards it
    /// (LESSON-569, LESSON-598). Each gate was inverted on its own:
    ///
    /// | inverted | what fails |
    /// |---|---|
    /// | `if already { return; }` in `degrade` | **nothing** — the gate below never lets `degrade` be reached twice |
    /// | the `degraded` early return in `ensure_open` | this test, on `the file stops at the closing record and takes nothing after it`: three more lines appear, `transcript_resumed` among them |
    /// | `slot.enabled.swap(false, …)` in `degrade` | this test, on `a degraded session is no longer recording` |
    ///
    /// So the gate with teeth is `ensure_open`'s `degraded` check, and the
    /// once-guard is a backstop behind it rather than the thing this count
    /// measures. Naming that is the point: a later refactor that deletes the
    /// backstop will see a green suite, and this table is what tells the reader
    /// the suite is not the evidence they want.
    #[tokio::test]
    async fn first_write_failure_degrades_once_and_never_again() {
        let dir = scratch("degrade");
        let faults = Faults::default();
        let announced = Arc::new(AtomicUsize::new(0));
        let seen = Arc::clone(&announced);
        let reasons = Arc::new(Mutex::new(Vec::new()));
        let recorded = Arc::clone(&reasons);
        let sink = TranscriptSink::spawn_faulty(
            config(&dir),
            SinkHooks {
                on_degraded: Box::new(move |degradation| {
                    seen.fetch_add(1, Ordering::Relaxed);
                    recorded
                        .lock()
                        .expect("hook mutex")
                        .push(degradation.reason);
                }),
            },
            faults.clone(),
        );
        let id = session('s');
        sink.session_created(new_session(&id, true));
        sink.record(&id, a_prompt("before the failure"));
        sink.flush().await;

        faults.arm(1);
        sink.record(&id, a_result("this write fails"));
        // Every later record is a no-op returning `()`.
        sink.record(&id, a_result("never written"));
        sink.record(&id, a_prompt("nor this"));
        // The realistic second door: a user who sees the notice and types
        // `/transcript on` again. A degraded session does not reopen, and the
        // notice is not repeated at them.
        sink.set_enabled(&id, true, 77);
        sink.record(&id, a_prompt("nor after a /transcript on"));
        sink.flush().await;

        let status = sink.status(&id).expect("status");
        assert!(!status.enabled, "a degraded session is no longer recording");
        let degraded = status.degraded.expect("status names the degraded reason");
        assert!(
            degraded.contains("injected transcript write failure"),
            "the reason names what went wrong: {degraded}"
        );
        assert_eq!(
            announced.load(Ordering::Relaxed),
            1,
            "on_degraded fires exactly once"
        );
        assert_eq!(
            *reasons.lock().expect("hook mutex"),
            vec![TranscriptStateReason::WriteFailure],
            "the announcement carries the write_failure reason"
        );

        let lines = lines_at(&status.path.expect("the file was opened"));
        assert_eq!(
            kinds(&lines),
            vec!["transcript_opened", "prompt_submitted", "transcript_closed"],
            "the file stops at the closing record and takes nothing after it"
        );
        assert_eq!(lines[2]["reason"], "write_failure");
        assert_contiguous(&lines);

        sink.shutdown().await;
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// BR-7 — two sessions recording at once produce two files with no shared
    /// lines, and a record for a session the sink never met produces no file at
    /// all.
    ///
    /// The adversarial legs are the second and third: an envelope *scoped to
    /// another session* handed in under this session's id must not be written
    /// (that is what a mis-wired tap would do), and an unknown session must not
    /// cause a file to be invented for it — the failure that would put one
    /// user's session in a file another user may share.
    #[tokio::test]
    async fn two_sessions_two_files_no_crosstalk() {
        let dir = scratch("crosstalk");
        let sink = TranscriptSink::spawn(config(&dir));
        let alpha = session('a');
        let beta = session('b');
        let stranger = session('z');
        sink.session_created(new_session(&alpha, true));
        sink.session_created(new_session(&beta, true));

        sink.record(&alpha, a_prompt("alpha's prompt"));
        sink.record(&beta, a_prompt("beta's prompt"));
        sink.record(&alpha, an_envelope(11, Some(alpha.clone()), "alpha's text"));
        sink.record(&beta, an_envelope(12, Some(beta.clone()), "beta's text"));
        // Scoped to beta, handed in under alpha: not alpha's to hold.
        sink.record(&alpha, an_envelope(13, Some(beta.clone()), "beta's text"));
        // Daemon-scoped: belongs to no session, so to no file (BR-7).
        sink.record(&alpha, an_envelope(14, None, "daemon-wide"));
        // A session nothing ever created.
        sink.record(&stranger, a_prompt("who?"));
        sink.flush().await;

        let alpha_lines = lines_at(
            &sink
                .status(&alpha)
                .and_then(|status| status.path)
                .expect("alpha's file"),
        );
        let beta_lines = lines_at(
            &sink
                .status(&beta)
                .and_then(|status| status.path)
                .expect("beta's file"),
        );

        for line in &alpha_lines {
            assert_eq!(
                line["session_id"], alpha.0,
                "every line of alpha's file is alpha's"
            );
        }
        for line in &beta_lines {
            assert_eq!(line["session_id"], beta.0);
        }
        let alpha_text = serde_json::to_string(&alpha_lines).expect("render");
        assert!(
            !alpha_text.contains("beta's text") && !alpha_text.contains("daemon-wide"),
            "no other session's content and nothing daemon-scoped: {alpha_text}"
        );
        assert_eq!(kinds(&alpha_lines).len(), 3, "opened, prompt, one envelope");
        assert_contiguous(&alpha_lines);
        assert_contiguous(&beta_lines);

        assert!(
            sink.status(&stranger).is_none(),
            "a session the sink never met has no state"
        );
        let files: Vec<String> = std::fs::read_dir(&dir)
            .expect("read dir")
            .map(|entry| {
                entry
                    .expect("entry")
                    .file_name()
                    .to_string_lossy()
                    .into_owned()
            })
            .collect();
        assert_eq!(
            files.len(),
            2,
            "two sessions, two files, and none invented for a stranger: {files:?}"
        );

        sink.shutdown().await;
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// BR-1 / BR-2 — a session created with the transcript off writes nothing,
    /// and `/transcript on` opens the file from that point without backfilling.
    #[tokio::test]
    async fn a_session_starts_recording_only_when_it_is_switched_on() {
        let dir = scratch("switch-on");
        let sink = TranscriptSink::spawn(config(&dir));
        let id = session('s');
        sink.session_created(new_session(&id, false));
        sink.record(&id, a_prompt("before the switch"));
        sink.flush().await;

        assert!(!dir.exists(), "off means no directory and no file (BR-1)");
        let status = sink.status(&id).expect("status");
        assert!(!status.enabled);
        assert_eq!(status.path, None);

        sink.set_enabled(&id, true, 42);
        sink.record(&id, a_prompt("after the switch"));
        sink.flush().await;

        let path = sink
            .status(&id)
            .and_then(|status| status.path)
            .expect("the file opens on the switch");
        let lines = lines_at(&path);
        assert_eq!(kinds(&lines), vec!["transcript_opened", "prompt_submitted"]);
        assert_eq!(
            lines[0]["seq_at_open"], 42,
            "the file begins at the seq the switch named"
        );
        let text = serde_json::to_string(&lines).expect("render");
        assert!(
            !text.contains("before the switch"),
            "nothing is backfilled from before the switch: {text}"
        );

        sink.shutdown().await;
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// AC-4 — `/transcript off` closes, and a later `on` resumes the **same**
    /// file with `n` continuing.
    #[tokio::test]
    async fn off_then_on_resumes_the_same_file() {
        let dir = scratch("resume");
        let sink = TranscriptSink::spawn(config(&dir));
        let id = session('s');
        sink.session_created(new_session(&id, true));
        sink.record(&id, a_prompt("recorded"));
        sink.set_enabled(&id, false, 50);
        sink.record(&id, a_prompt("not recorded"));
        sink.flush().await;

        let first = sink
            .status(&id)
            .and_then(|status| status.path)
            .expect("the file exists");
        assert_eq!(
            kinds(&lines_at(&first)),
            vec!["transcript_opened", "prompt_submitted", "transcript_closed"]
        );

        sink.set_enabled(&id, true, 51);
        sink.record(&id, a_prompt("recorded again"));
        sink.flush().await;

        let second = sink
            .status(&id)
            .and_then(|status| status.path)
            .expect("still the same file");
        assert_eq!(second, first, "a resume appends to the same file");
        let lines = lines_at(&second);
        assert_eq!(
            kinds(&lines),
            vec![
                "transcript_opened",
                "prompt_submitted",
                "transcript_closed",
                "transcript_resumed",
                "prompt_submitted",
            ]
        );
        assert_eq!(lines[3]["seq_at_resume"], 51);
        assert_contiguous(&lines);

        sink.shutdown().await;
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// AC-11 / BR-9 — a transcript directory that exists wider than owner-only
    /// is refused at open, the session keeps running, and the refusal is
    /// announced once as `dir_refused`.
    #[tokio::test]
    async fn a_refused_directory_degrades_the_session_and_not_the_daemon() {
        let dir = scratch("refused");
        std::fs::create_dir_all(&dir).expect("scratch dir");
        std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o755)).expect("dir mode");

        let reasons = Arc::new(Mutex::new(Vec::new()));
        let recorded = Arc::clone(&reasons);
        let sink = TranscriptSink::spawn_with(
            config(&dir),
            SinkHooks {
                on_degraded: Box::new(move |degradation| {
                    recorded
                        .lock()
                        .expect("hook mutex")
                        .push(degradation.reason);
                }),
            },
        );
        let id = session('s');
        sink.session_created(new_session(&id, true));
        sink.record(&id, a_prompt("this session runs normally"));
        sink.flush().await;

        let status = sink.status(&id).expect("status");
        assert!(!status.enabled);
        assert_eq!(status.path, None, "no file was opened");
        assert!(
            status
                .degraded
                .as_deref()
                .is_some_and(|reason| reason.contains("wider than owner-only")),
            "the refusal states why: {:?}",
            status.degraded
        );
        assert_eq!(
            *reasons.lock().expect("hook mutex"),
            vec![TranscriptStateReason::DirRefused],
            "a refused directory is announced as dir_refused, not write_failure"
        );
        assert!(
            std::fs::read_dir(&dir).expect("listing").next().is_none(),
            "nothing is written into a refused directory"
        );

        sink.shutdown().await;
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// AC-18 — shutdown closes an open transcript with `daemon_shutdown`.
    #[tokio::test]
    async fn shutdown_closes_every_open_transcript() {
        let dir = scratch("shutdown");
        let sink = TranscriptSink::spawn(config(&dir));
        let id = session('s');
        sink.session_created(new_session(&id, true));
        sink.record(&id, a_prompt("mid-session"));
        sink.flush().await;
        let path = sink
            .status(&id)
            .and_then(|status| status.path)
            .expect("the file is open");

        sink.shutdown().await;

        let lines = lines_at(&path);
        assert_eq!(
            kinds(&lines),
            vec!["transcript_opened", "prompt_submitted", "transcript_closed"]
        );
        assert_eq!(lines[2]["reason"], "daemon_shutdown");
        assert_eq!(
            lines[2]["records"], 3,
            "the closing record states the final n"
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// ADR-3 — `session_closed` writes the closing record and forgets the
    /// session.
    #[tokio::test]
    async fn session_closed_writes_the_end_and_forgets_the_session() {
        let dir = scratch("closed");
        let sink = TranscriptSink::spawn(config(&dir));
        let id = session('s');
        sink.session_created(new_session(&id, true));
        sink.record(&id, a_prompt("only turn"));
        sink.flush().await;
        let path = sink
            .status(&id)
            .and_then(|status| status.path)
            .expect("the file is open");

        sink.session_closed(&id, CloseReason::SessionEnded);
        sink.flush().await;

        assert!(sink.status(&id).is_none(), "the session is forgotten");
        let lines = lines_at(&path);
        assert_eq!(lines[2]["kind"], "transcript_closed");
        assert_eq!(lines[2]["reason"], "session_ended");

        sink.shutdown().await;
        let _ = std::fs::remove_dir_all(&dir);
    }
}
