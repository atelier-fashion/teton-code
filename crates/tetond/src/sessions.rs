//! The session registry — the daemon's authoritative list of sessions.
//!
//! Sessions live here, in daemon-owned shared state, not in any client
//! connection. They therefore outlive the clients that create them (BR-4): a
//! client can disconnect and reconnect, or a second client can attach, and the
//! session list stays identical for everyone. This module is the skeleton's
//! session store; prompt-turn and phase-gate machinery land in later tasks.
//!
//! # The conversation lives here too (REQ-567)
//!
//! A session's [`Conversation`] — the ordered blocks the harness retained
//! across every completed turn — is canonical session state, the thing ACP says
//! a session *is*, so it belongs in the session store rather than in a sixth
//! per-session side-map beside the runtime's cross-cutting ones (`session_taint`,
//! `session_gates`, `session_user_urls`). Three properties make it safe to keep
//! it behind the same `std::sync::Mutex` as the summaries:
//!
//! - Every critical section is a vector move or a clone; **the lock is never
//!   held across a turn or an `.await`** (LESSON-448). A turn's exclusivity is
//!   carried by a [`TurnClaim`] — a flag on the record, released on drop — not
//!   by a held lock.
//! - A commit is a **whole-vector replacement** (BR-6): there is no partial
//!   write for a failed turn to leave behind.
//! - The store is keyed by session and read only by that session's dispatch, so
//!   BR-2's isolation is structural rather than checked.

use std::fmt;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use teton_core::session_root::CwdRefusal;
use teton_protocol::methods::{RepoContextStateKind, SessionSummary};
use teton_protocol::{Phase, SessionId, SessionMode, TurnId};

use crate::harness::context::{ContextBlock, RetainedContext};
use crate::repo_context::RepoContextState;
use crate::skills::SkillRegistry;

/// One session as the registry holds it: what clients see, plus what only the
/// registry needs to know.
///
/// [`SessionSummary`] is a **wire type** — it is what `session/list` returns — so
/// bookkeeping that exists to stop the daemon spending money twice does not
/// belong on it. `title_attempted` is exactly that: a fact about what the daemon
/// has already tried, not a fact about the session a client is looking at.
struct SessionRecord {
    summary: SessionSummary,
    /// Whether this session's **one** `title` duty attempt has been claimed
    /// (REQ-561 TASK-062).
    ///
    /// Separate from `summary.title.is_some()` on purpose, and the separation is
    /// the whole cost argument. "Name a session that has no title" and "name a
    /// session that has not been named yet" differ only on the sessions where
    /// the duty *failed* — and on those, the first rule fires again on every
    /// subsequent turn, which turns the cheapest category in the table into a
    /// per-turn model call precisely on the machines where the calls are already
    /// not working. So the attempt is spent when it is made, not when it
    /// succeeds.
    title_attempted: bool,
    /// What this session has said so far (REQ-567 BR-1) — replayed into the next
    /// turn's [`ContextManager`](crate::harness::context::ContextManager) and
    /// replaced wholesale when that turn completes.
    conversation: Conversation,
    /// The turn currently holding this session, if any (REQ-567 BR-5).
    ///
    /// Named rather than a bare `bool` because the refusal has to say *which*
    /// turn is in flight: a busy error that cannot name its cause is the generic
    /// turn failure LESSON-456 is about.
    in_flight_turn: Option<TurnId>,
    /// The `/name` commands this session dispatches (REQ-585 BR-1, ADR-1).
    ///
    /// A **snapshot** of four directory listings, built when the session is
    /// created and rebuilt when its root moves — never re-derived per turn and
    /// never watched (see [`SkillRegistry`]). It lives here, beside the root it
    /// was derived from, because half of it *is* a fact about that root: a
    /// project skill is `<session-root>/.claude/…`, so a registry stored
    /// anywhere else would be a second thing to move on `/cd`.
    ///
    /// Behind an [`Arc`] so a query — `skills/list`, and the turn that expands a
    /// skill — clones a pointer rather than every registered body under the
    /// registry lock (LESSON-448's rule at a smaller scale: a lock held across a
    /// deep copy is a lock every other session waits on).
    skills: Arc<SkillRegistry>,
    /// The repository's own notes, as this session last read them (REQ-612
    /// BR-1, ADR-3).
    ///
    /// Beside [`Self::skills`] because it is the same kind of fact and moves at
    /// the same three moments: a **snapshot** derived from the root the session
    /// stands on, taken at `session/create`, retaken when the root moves, and —
    /// unlike the skill registry — re-checked at the start of each prompt turn
    /// against one `stat` (BR-6). Storing it anywhere else would be a second
    /// thing to move on `/cd`.
    ///
    /// [`RepoContextState::Absent`] until the first load, which is the honest
    /// reading of a record nobody has derived yet: no file is resident, and the
    /// prompt is byte-identical to a build without this feature.
    ///
    /// Behind an [`Arc`] for [`Self::skills`]'s reason at a smaller scale — the
    /// state carries up to 64 KiB of file text, and a turn reading it under the
    /// registry lock would hold that lock for the copy.
    repo_context: Arc<RepoContextState>,
    /// The last `repo_context_state` this session **published**, as the triple
    /// a client actually renders (REQ-612 BR-3, AC-3).
    ///
    /// Separate from [`Self::repo_context`] because the two answer different
    /// questions, and conflating them let a truncation go out in silence. The
    /// stored state is the *file*, classified once at the widest cap any route
    /// can ask for; what a client is told is the file **as this route rendered
    /// it**, and a narrower route cap renders the same unchanged file as a truncated
    /// block. So a session that moves from an 8,192-cap route to a 4,096-cap one
    /// has a stored state that did not move and news that did.
    ///
    /// The triple is `(state, truncated, resident_bytes)` — the three fields a
    /// route can move without the file moving; `source`, `bytes_on_disk` and
    /// `reason` are facts about the file, and a change to one of *those* is
    /// announced through [`SessionRegistry::claim_repo_context_publish`]'s
    /// `always`.
    ///
    /// Seeded rather than `Option`: see the constructor.
    repo_context_published: (RepoContextStateKind, bool, u64),
    /// This session's `/context on|off`, or `None` while it follows the durable
    /// `[context] repo_file` default (REQ-612 BR-2).
    ///
    /// Three values rather than two, and the third is what makes the two
    /// switches composable: `None` is "nobody has said anything about *this*
    /// session", so a `config/set` that flips the durable default reaches every
    /// session that has not overridden it. A `bool` seeded from the config at
    /// create would freeze each session at the value the config held that
    /// moment, which is the second store REQ-611 ASSUME-017 keeps this feature
    /// free of.
    ///
    /// Never persisted: `/context off` is a fact about one session and writes
    /// nothing to `config.toml` (BR-2).
    context_switch: Option<bool>,
    /// Where this session stands on writing the notes file it does **not**
    /// have (REQ-613 BR-1, ADR-1).
    ///
    /// Beside [`Self::repo_context`] because it is derived from it and moves at
    /// the same two lifecycle moments, and on the record rather than on the
    /// daemon because "has *this session* prompted yet" is a session fact: the
    /// runtime's `turn_counter` is daemon-wide and mints ids, so it cannot
    /// answer it (REQ-613 architecture, finding 1).
    generation: GenerationState,
    /// The root [`Self::generation`] was last decided **at**, or `None` before
    /// anything decided it.
    ///
    /// BR-1's cadence is once per session *per root*, and the two halves need
    /// different treatment: a decision made at this root is never re-armed —
    /// a decline is a decline for the rest of the session — while a `/cd` into
    /// a different project is a new question and must re-arm even after one.
    /// Comparing the root is what tells those apart, and it is the only thing
    /// that can: every site that re-derives the notes calls one function
    /// ([`SessionRegistry::arm_generation`]), and `/context on` reaches it too.
    generation_root: Option<PathBuf>,
}

/// Where a session stands on writing the repository notes it does not have
/// (REQ-613 BR-1, architecture ADR-1).
///
/// Six states, and every one of them is terminal but the first two: `Pending`
/// is a question waiting for the turn that will ask it, `Offered` is that turn
/// holding the claim, and the other four are the four ways the question is
/// over. Nothing here is written anywhere — a declined offer is session-scoped
/// by construction, because Teton never remembers a permission answer across
/// sessions, and the durable ways to stop the offer are `[context] generate =
/// never` and an existing (even empty) file.
///
/// Deliberately **not** [`teton_protocol::events::GenerationOutcome`], which is
/// the ten-word vocabulary the wire carries for the *stages* of one run. This
/// is what the record holds between runs, and folding the two would put nine
/// values on a record where six are reachable and would make `walking` a state
/// a session can be left in.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GenerationState {
    /// The root's notes are absent, nothing has suppressed the offer, and the
    /// next prompt turn is the one that raises it (BR-1).
    Pending,
    /// A turn has **claimed** the offer and is running it. The claim is what
    /// makes the hook fire on one turn rather than on every turn of a session
    /// whose answer never arrived ([`SessionRegistry::claim_generation`]).
    Offered,
    /// A human said no. Not raised again for this root in this session.
    Declined,
    /// A file was written and loaded (BR-7).
    Generated,
    /// A stage failed, no file was left behind, and the offer is not re-raised
    /// this session (BR-9).
    Failed,
    /// Nothing was asked and nothing ran: a file (or an `AGENTS.md`, or an
    /// empty one) is already there, the notes are switched off, the root is not
    /// a project, `[context] generate = never`, or the level forbids the write.
    Suppressed,
}

impl GenerationState {
    /// Whether this state is one a fresh derivation may replace.
    ///
    /// The two that are not: a human's answer and a run's outcome. Both mean
    /// *this root's* question is settled for this session (BR-1), and a
    /// re-derivation at the same root — `/context off` then `/context on`, say
    /// — must not turn a decline back into a prompt.
    const fn rearmable(self) -> bool {
        matches!(self, Self::Pending | Self::Suppressed)
    }
}

/// One session's conversation: the ordered blocks the harness retained, turn
/// after turn (REQ-567 BR-1).
///
/// The blocks are stored **exactly as the harness kept them** — post containment
/// cut (BUG-147), post compaction — with the per-block role and egress
/// provenance the [`ContextBlock`] already carries. There is no parallel
/// conversation type: a second spelling of "who said this and where did it come
/// from" is how one of them ends up laxer than the other at the egress choke
/// point (BR-3).
///
/// **The system head is never in here.** Heads are rebuilt per prompt from the
/// current tools and route, and a fossilized head replayed under a fresh one
/// would put two system prompts in the context and make a mid-session head
/// change unrepresentable — which is exactly what BR-7's cache-independence
/// asks for. [`ContextManager::into_retained`](crate::harness::context::ContextManager::into_retained)
/// excludes it by construction: the head was never a block.
///
/// It wraps a [`RetainedContext`] rather than a bare block vector because two
/// facts about a conversation are not inside its blocks and must cross the
/// boundary with them: whether history has been dropped (so the truncation note
/// survives the turn that cut) and the egress provenance of what was dropped (so
/// a truncated-away `local-only` read still pins the session — BR-3).
///
/// ## Invariant: a stored conversation fits the budget (BR-4)
///
/// Every turn that writes here does so through
/// [`CarriedTurn`](crate::carry::CarriedTurn) — the sole production caller of
/// [`Self::from_retained`]'s two commit paths — and it truncates to the turn's
/// byte/token budget immediately before handing the vector over. No path can
/// leave an oversized vector here to be replayed into the next prompt. That
/// matters because a conversation is *replayed*, not
/// re-derived: an over-budget vector stored once would be replayed into a prompt
/// the engine refuses, and a refused turn never commits (BR-6) — the session
/// would wedge on its own history rather than degrade to compaction.
///
/// It is an invariant of the store rather than of the turn loop because the
/// loop's own gate runs at the top of each iteration and a cancelled turn can be
/// dropped between two of them. The budget itself is per-route, so this says
/// "fit the budget of the turn that wrote it", not "fit some global constant".
#[derive(Debug, Clone, Default)]
pub struct Conversation {
    retained: RetainedContext,
}

impl Conversation {
    /// The conversation a completed turn's context is (REQ-567 D-1).
    #[must_use]
    pub fn from_retained(retained: RetainedContext) -> Self {
        Self { retained }
    }

    /// The blocks, in the order they happened.
    #[must_use]
    pub fn blocks(&self) -> &[ContextBlock] {
        self.retained.blocks()
    }

    /// What the next turn replays into its manager: blocks, truncation flag, and
    /// dropped provenance together.
    #[must_use]
    pub fn into_retained(self) -> RetainedContext {
        self.retained
    }

    /// The same, borrowed — for a caller that only needs to look.
    #[must_use]
    pub fn retained(&self) -> &RetainedContext {
        &self.retained
    }

    /// How many blocks the conversation holds — the count `context_cleared`
    /// reports (BR-8).
    #[must_use]
    pub fn len(&self) -> usize {
        self.retained.blocks().len()
    }

    /// Whether this session has retained anything yet.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.retained.blocks().is_empty()
    }
}

/// The refusal a second concurrent turn on one session gets (REQ-567 BR-5, D-3).
///
/// Typed, and it names the turn already running: BR-5 requires that a refusal
/// "names its cause truthfully rather than surfacing as a generic turn error",
/// which is LESSON-456's rule applied to a state a client can legitimately hit
/// and legitimately retry.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum TurnClaimError {
    /// Another turn holds this session. The conversation is linear by refusing,
    /// not by interleaving (D-3).
    #[error(
        "session {session_id} is already running turn {turn_id}; \
         one session runs one turn at a time — retry when it finishes"
    )]
    InFlight {
        /// The session that is busy.
        session_id: SessionId,
        /// The turn holding it.
        turn_id: TurnId,
    },
    /// There is no such session to claim.
    ///
    /// A second variant rather than a granted claim over nothing: a claim that
    /// guards no record would let a turn run — and later commit — against a
    /// session the registry does not have, which is a silent no-op dressed as a
    /// turn. Callers reach this only if a session vanishes between lookup and
    /// claim, which nothing does today.
    #[error("no such session {session_id}")]
    NoSuchSession {
        /// The id that matched no record.
        session_id: SessionId,
    },
}

/// Exclusive hold on one session's conversation for the length of one turn
/// (REQ-567 BR-5, D-3).
///
/// **Released by `Drop`, never by an explicit call** — the discipline
/// [`crate::lifetime`] and [`crate::model_consent`]'s `InFlightGuard` already
/// keep, because there are three ways a turn's future can stop existing and only
/// one of them runs code the turn wrote. A turn can panic and unwind; a turn can
/// return; and a turn whose client disconnects is **drained** for
/// `TURN_DRAIN_TIMEOUT` and aborted only if it outlasts that (`server.rs`,
/// REQ-565) — the abort drops the future where it stands. Any of the three
/// leaking a claim would wedge the session so that every later prompt on it is
/// refused for a turn that no longer exists.
///
/// It holds a registry *handle*, not a lock guard: the mutex is taken for the
/// two vector writes and released immediately, so nothing here blocks the async
/// path for the length of a turn (LESSON-448).
pub struct TurnClaim {
    sessions: Arc<Mutex<Vec<SessionRecord>>>,
    session_id: SessionId,
    turn_id: TurnId,
}

impl TurnClaim {
    /// The turn this claim was taken for.
    #[must_use]
    pub fn turn_id(&self) -> &TurnId {
        &self.turn_id
    }

    /// The session it holds.
    #[must_use]
    pub fn session_id(&self) -> &SessionId {
        &self.session_id
    }
}

/// Prints what the claim *is* — which session, which turn — and not the
/// registry handle it releases into.
///
/// Deriving would print every session's conversation through the handle, which
/// is prompt text and tool output in whatever log the claim was formatted into
/// (conventions: no file content or prompt text in logs).
impl fmt::Debug for TurnClaim {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("TurnClaim")
            .field("session_id", &self.session_id)
            .field("turn_id", &self.turn_id)
            .finish_non_exhaustive()
    }
}

impl Drop for TurnClaim {
    fn drop(&mut self) {
        let Ok(mut sessions) = self.sessions.lock() else {
            // A poisoned registry means some other critical section panicked;
            // there is nothing useful to release into and unwinding here would
            // replace that panic with this one.
            return;
        };
        let Some(record) = sessions
            .iter_mut()
            .find(|record| record.summary.session_id == self.session_id)
        else {
            return;
        };
        // Only ever release *this* claim. Two claims cannot coexist today, so
        // the comparison is a guard against a future that changes that rather
        // than a live case — but "the guard released whatever it found" is the
        // shape that turns such a change into a silent double-admission.
        if record.in_flight_turn.as_ref() == Some(&self.turn_id) {
            record.in_flight_turn = None;
        }
    }
}

/// How much entropy backs one session id (REQ-569 ADR-H): 128 bits, the same
/// width the rest of the industry gives an opaque resource name.
const SESSION_ID_ENTROPY_BYTES: usize = 16;

/// Crockford's base32 alphabet, lowercased.
///
/// Base32 rather than hex because 128 bits is 26 characters here against hex's
/// 32, and these ids are read by humans in daemon logs and typed back on a CLI.
/// Crockford's variant specifically: it drops `i`, `l`, `o`, and `u`, so an id
/// read off a log line cannot be transcribed into a *different valid-looking*
/// id by the usual one/ell and zero/oh confusions.
const SESSION_ID_ALPHABET: [u8; 32] = *b"0123456789abcdefghjkmnpqrstvwxyz";

/// The number of base32 characters 128 bits encodes to: 25 full 5-bit groups
/// plus 3 leftover bits, which take a 26th.
const SESSION_ID_BODY_LEN: usize = 26;

/// The prefix every session id carries.
///
/// Kept from the old `sess-{n}` scheme deliberately: logs, error strings, and
/// the CLI's session handling all read better when an id is self-describing,
/// and the prefix costs nothing — the entropy is entirely in what follows it.
const SESSION_ID_PREFIX: &str = "sess-";

/// Whether `session_id` is short enough to be one this daemon could have minted
/// (REQ-569 verify, F9).
///
/// A wire `session_id` is otherwise bounded only by the frame cap — about four
/// megabytes — and `session/attach` stores it verbatim as a key in the grant
/// registry when a consent is granted, so an unbounded id is an unbounded
/// allocation keyed to a connection.
///
/// **Length only, deliberately.** Validating the *alphabet* would make the
/// refusal depend on the id's shape in a way an attacker can probe, and it would
/// couple a wire gate to a minting detail that ADR-H is explicit must confer no
/// authorization. This is a well-formedness bound on an untrusted string, not a
/// second access check: every id of a plausible length still draws exactly the
/// refusal the grant rules give it, whether it names a live session or nothing
/// at all (BR-8 — no existence oracle).
#[must_use]
pub fn within_minted_length(session_id: &SessionId) -> bool {
    session_id.0.len() <= SESSION_ID_PREFIX.len() + SESSION_ID_BODY_LEN
}

/// Whether `cwd` may be a session's root — the **one** validator behind
/// `session/create`'s `cwd` and `session/set_cwd` (REQ-583 BR-6/BR-7, ADR-4).
///
/// The cwd becomes the session's tool jail (BUG-147), so a relative or
/// nonexistent one is refused up front — jailing tools to a directory that is
/// not there reproduces the every-tool-fails session BUG-147 fixed. Absolute
/// after the client's own resolution (`~` expansion, relative-to-shell joining),
/// exists, is a directory; nothing is canonicalized here (the jail canonicalizes
/// per call).
///
/// A refusal **names the path** (BR-6): the sentence is what the CLI prints
/// before any session output, and "must be an absolute path" without the path
/// sends the user back to guess which of their spellings the daemon saw. The
/// path is the caller's own argument echoed back to that caller alone, never
/// published. The sentence itself is [`CwdRefusal`]'s, a pure type in
/// `teton-core` so the CLI's fail-fast for a `--cwd` that names no directory
/// constructs the same value instead of retyping it; the I/O that reaches the
/// verdict lives here alone.
///
/// It lives here — with the registry that stores the path — rather than in the
/// server or the runtime because both of them call it: the server validates a
/// `session/create` before the registry is touched, and the runtime validates a
/// `session/set_cwd` *after* it holds the turn claim, so a busy session says
/// `SESSION_BUSY` before it says anything about the path.
///
/// # Errors
///
/// The typed reason ([`CwdRefusal`]); its `Display` is the wire message.
pub fn validate_session_cwd(cwd: &Path) -> Result<(), CwdRefusal> {
    if !cwd.is_absolute() {
        return Err(CwdRefusal::NotAbsolute(cwd.to_path_buf()));
    }
    if !cwd.is_dir() {
        return Err(CwdRefusal::NotADirectory(cwd.to_path_buf()));
    }
    Ok(())
}

/// Why [`SessionRegistry::create`] refused (REQ-569 verify, F9).
///
/// Two failures with **opposite owners**, which is why they stopped being one
/// `&'static str`. `MissingPhase` is the caller's params — they asked for a
/// structured session and named no phase, and the remedy is to send different
/// params. `NoEntropy` is the daemon's machine failing to supply randomness; no
/// parameter the caller could have sent would have changed it, and reporting it
/// as a params error sends the user off editing a request that was never wrong.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SessionCreateError {
    /// A structured session was requested without a starting phase.
    MissingPhase,
    /// The OS entropy source would not mint a session id.
    NoEntropy,
}

impl SessionCreateError {
    /// The sentence the client is given. Carries no path and no content
    /// (conventions: privacy in error messages).
    #[must_use]
    pub fn message(self) -> &'static str {
        match self {
            Self::MissingPhase => "structured session requires a starting phase",
            Self::NoEntropy => "cannot mint a session id: the OS entropy source is unavailable",
        }
    }
}

/// Mint one session id: `sess-` plus 128 bits of OS entropy in base32
/// (REQ-569 BR-8, ADR-H).
///
/// ## This is defense in depth, and nothing may treat it as more
///
/// BR-8 is explicit that **ids are names and grants are credentials**. Nothing
/// in the daemon may key an authorization decision on an id being hard to
/// guess: an attacker who learns an id — from a log, a screen, a shell history —
/// must still be refused by the grant checks, exactly as they were when ids were
/// `sess-0`. What this buys is narrower: a blind guesser no longer enumerates
/// the session namespace by counting, so the guessing surface stops being a
/// dozen names and starts being 2^128.
///
/// ## Why a failure to mint is an error and not a fallback
///
/// The one thing this must never do is quietly degrade to a predictable id when
/// the entropy source is unavailable. A fallback counter would reintroduce the
/// enumerable namespace precisely on the machines where nobody is watching, and
/// it would do so silently. So an entropy failure refuses the `session/create`
/// through the `Result` the caller already handles. On every platform the daemon
/// supports this is a `getentropy(2)`/`getrandom(2)` call that does not fail.
///
/// # Errors
///
/// Returns `Err(())` when the OS entropy source is unavailable; the caller
/// classifies it (see [`SessionCreateError::NoEntropy`]).
fn mint_session_id() -> Result<SessionId, ()> {
    let mut entropy = [0u8; SESSION_ID_ENTROPY_BYTES];
    getrandom::getrandom(&mut entropy).map_err(|_| ())?;

    let mut id = String::with_capacity(SESSION_ID_PREFIX.len() + SESSION_ID_BODY_LEN);
    id.push_str(SESSION_ID_PREFIX);

    // A plain 8-bits-in, 5-bits-out accumulator. The final group is left-padded
    // with zero bits, so the last character carries 3 bits of entropy rather
    // than 5 — 128 bits total, as counted above.
    let mut accumulator: u32 = 0;
    let mut bits: u32 = 0;
    for byte in entropy {
        accumulator = (accumulator << 8) | u32::from(byte);
        bits += 8;
        while bits >= 5 {
            bits -= 5;
            let index = usize::try_from((accumulator >> bits) & 0b1_1111).expect("5 bits fit");
            id.push(char::from(SESSION_ID_ALPHABET[index]));
        }
    }
    if bits > 0 {
        let index = usize::try_from((accumulator << (5 - bits)) & 0b1_1111).expect("5 bits fit");
        id.push(char::from(SESSION_ID_ALPHABET[index]));
    }

    Ok(SessionId::from(id))
}

/// A thread-safe registry of live sessions, newest tracked last.
///
/// **`Clone` yields another handle to the *same* registry**, not a copy of it —
/// the state is behind an `Arc`, in the shape a `tokio::sync` primitive or a
/// `reqwest::Client` uses. That exists so work which outlives a request handler
/// can still write back: the `title` duty (REQ-561 TASK-062) is detached from
/// the turn that triggers it precisely so the turn does not wait on it, and a
/// detached task needs an owned `'static` handle rather than a borrow of the
/// daemon's field.
#[derive(Clone)]
pub struct SessionRegistry {
    sessions: Arc<Mutex<Vec<SessionRecord>>>,
}

impl SessionRegistry {
    /// An empty registry.
    #[must_use]
    pub fn new() -> Self {
        Self {
            sessions: Arc::new(Mutex::new(Vec::new())),
        }
    }

    /// Creates a session and returns its summary.
    ///
    /// Structured sessions are pinned to a phase; freeform sessions carry no
    /// phase regardless of any phase passed in (BR-3). `cwd` is the client's
    /// working directory — the session's tool jail (BUG-147); `None` falls back
    /// to the daemon's env-derived root at turn time.
    ///
    /// # Errors
    ///
    /// [`SessionCreateError::MissingPhase`] when a structured session is
    /// requested without a starting phase (the protocol requires one), or
    /// [`SessionCreateError::NoEntropy`] when the OS entropy source cannot mint
    /// an id (see [`mint_session_id`]).
    pub fn create(
        &self,
        mode: SessionMode,
        phase: Option<Phase>,
        cwd: Option<PathBuf>,
    ) -> Result<SessionSummary, SessionCreateError> {
        let phase = match mode {
            SessionMode::Structured => match phase {
                Some(phase) => Some(phase),
                None => return Err(SessionCreateError::MissingPhase),
            },
            SessionMode::Freeform => None,
        };

        let summary = SessionSummary {
            session_id: mint_session_id().map_err(|()| SessionCreateError::NoEntropy)?,
            mode,
            phase,
            title: None,
            cwd,
        };
        self.sessions
            .lock()
            .expect("session registry mutex poisoned")
            .push(SessionRecord {
                summary: summary.clone(),
                title_attempted: false,
                conversation: Conversation::default(),
                in_flight_turn: None,
                // Empty until the caller discovers one ([`Self::set_skills`]).
                // The build needs the session's *probed* root — its path and
                // its `RootKind` — which is the server's to derive and not this
                // registry's to know; nothing can observe the gap, because the
                // id being returned here is the first anyone learns of the
                // session, and reaching it needs that id.
                skills: Arc::new(SkillRegistry::default()),
                // Absent until the caller loads one ([`Self::set_repo_context`]),
                // for the reason above it: the load needs the session's probed
                // root, its boundary set and the durable switch, and this
                // registry holds none of the three. Nothing can observe the gap
                // — the id being returned here is the first anyone learns of the
                // session.
                repo_context: Arc::new(RepoContextState::Absent),
                // Seeded with `absent` rather than left unstated, because
                // `absent` is what silence *means* on this event: a client that
                // has heard nothing correctly renders no notes line, which is
                // the same thing an `absent` event would tell it. Leaving this
                // unset would make the first turn of every session in every
                // directory without a `TETON.md` publish an `absent` nobody
                // asked for — the line LESSON-513 is about.
                repo_context_published: (RepoContextStateKind::Absent, false, 0),
                // No session has said anything about itself yet, so every one of
                // them follows `[context] repo_file` until someone types
                // `/context`.
                context_switch: None,
                // `Suppressed` until the caller derives the notes
                // ([`Self::arm_generation`]), for `repo_context`'s reason above
                // it: the derivation needs the probed root, and this registry
                // does not have one. The fail-**closed** value, deliberately —
                // a session whose notes were never derived offers nothing, so a
                // create path that forgot to derive them writes no file rather
                // than writing one into a directory nobody probed.
                generation: GenerationState::Suppressed,
                generation_root: None,
            });
        Ok(summary)
    }

    /// Every live session, newest first.
    #[must_use]
    pub fn list(&self) -> Vec<SessionSummary> {
        self.sessions
            .lock()
            .expect("session registry mutex poisoned")
            .iter()
            .rev()
            .map(|record| record.summary.clone())
            .collect()
    }

    /// Whether `id` names a live session.
    ///
    /// [`Self::get`] without the summary clone. The distinction earns its own
    /// method because the one caller (BUG-166's announcement gate,
    /// `server::refuse_commit_without_session_access`) runs on a refusal path
    /// an unattached peer can drive at will — the answer is a bound on what
    /// that peer's calls may allocate, so the check itself should allocate
    /// nothing.
    #[must_use]
    pub fn contains(&self, id: &SessionId) -> bool {
        self.sessions
            .lock()
            .expect("session registry mutex poisoned")
            .iter()
            .any(|record| &record.summary.session_id == id)
    }

    /// Looks up a session by id.
    #[must_use]
    pub fn get(&self, id: &SessionId) -> Option<SessionSummary> {
        self.sessions
            .lock()
            .expect("session registry mutex poisoned")
            .iter()
            .find(|record| &record.summary.session_id == id)
            .map(|record| record.summary.clone())
    }

    /// Claim this session's **one** `title` duty attempt (REQ-561 TASK-062).
    ///
    /// Returns `true` at most once per session, and only while the session has
    /// no title. The caller runs the duty exactly when this says `true`; every
    /// later turn of the same session gets `false` and issues no model call,
    /// which is how AC-6's "requested exactly once" is a property of the
    /// registry rather than of every call site remembering.
    ///
    /// ## The claim is taken *before* the call, not after it
    ///
    /// A guard keyed only on `title.is_none()` would re-fire on every turn of
    /// every session whose duty failed — an unresolvable binding, a local engine
    /// that will not load, a model that answered with nothing. That is the
    /// cheapest category in the table becoming a per-turn model call on exactly
    /// the machines where the calls are not working. So the attempt is spent
    /// here, at claim time; a failed title leaves the session unnamed, which is
    /// the state every session was in before this REQ (BR-3).
    ///
    /// ## Why it is one method and not a read plus a write
    ///
    /// Two turns of one session can be in flight at once (each `session/prompt`
    /// runs on its own task), and a `has_title()` followed by a `mark()` would
    /// let both read `false` and both call the model. The check and the mark
    /// happen under one lock, so the claim is genuinely exclusive.
    pub fn claim_title(&self, id: &SessionId) -> bool {
        let mut sessions = self
            .sessions
            .lock()
            .expect("session registry mutex poisoned");
        let Some(record) = sessions
            .iter_mut()
            .find(|record| &record.summary.session_id == id)
        else {
            return false;
        };
        if record.title_attempted || record.summary.title.is_some() {
            return false;
        }
        record.title_attempted = true;
        true
    }

    /// Give the session named by `id` its title, if it does not already have one.
    ///
    /// Returns whether the title landed. **An existing title is never
    /// overwritten** (REQ-561 BR-9): a session is a thing a person has already
    /// learned to recognize by name, and a second derivation that renamed it
    /// would move it in their list for no reason they asked for. The guard is
    /// keyed on `title.is_none()` and lives here rather than at the call site,
    /// so a future second caller inherits it instead of having to remember it.
    ///
    /// A `false` return is also what keeps `session_titled` honest: the caller
    /// publishes only when the title actually landed, so the event stream
    /// carries at most one naming per session (AC-15).
    pub fn set_title(&self, id: &SessionId, title: &str) -> bool {
        let mut sessions = self
            .sessions
            .lock()
            .expect("session registry mutex poisoned");
        let Some(record) = sessions
            .iter_mut()
            .find(|record| &record.summary.session_id == id)
        else {
            return false;
        };
        if record.summary.title.is_some() {
            return false;
        }
        record.summary.title = Some(title.to_owned());
        true
    }

    /// Move this session's root to `cwd` (`session/set_cwd`, REQ-583 BR-7).
    ///
    /// The path is the **only** stored fact about a session's root (ADR-1):
    /// kind, display, project name and branch are re-derived from it at every
    /// use, so rewriting it here moves every consumer — the next turn's jail,
    /// its environment block, the `/cd` line — with no second source of truth to
    /// keep in step. Stored as given, like [`Self::create`]'s `cwd`: the caller
    /// has already validated it ([`validate_session_cwd`]) and the jail
    /// canonicalizes per call.
    ///
    /// Unconditional, unlike [`Self::set_title`]'s once-only guard: a root moves
    /// as often as the user asks. `false` only for a session the registry does
    /// not have. The caller holds the turn claim across this and the clear that
    /// follows it, so a turn in flight cannot see its jail move underneath it.
    pub fn set_cwd(&self, id: &SessionId, cwd: PathBuf) -> bool {
        let mut sessions = self
            .sessions
            .lock()
            .expect("session registry mutex poisoned");
        let Some(record) = sessions
            .iter_mut()
            .find(|record| &record.summary.session_id == id)
        else {
            return false;
        };
        record.summary.cwd = Some(cwd);
        true
    }

    /// Replace this session's skill registry (REQ-585 BR-1, ADR-1).
    ///
    /// Called twice in a session's life and nowhere else: once when it is
    /// created, and again when its root moves (`session/set_cwd`), because half
    /// the registry is derived from that root. **Not** per turn and not per
    /// query — discovery is four directory listings and a file read per
    /// candidate, and a session that paid that on every prompt would be paying
    /// it while a user waits, for an answer that cannot have changed unless the
    /// root did. The consequence is stated where the type is
    /// ([`SkillRegistry`]): a file written after the session started is not
    /// picked up until the next `/cd`.
    ///
    /// Unconditional, like [`Self::set_cwd`] beside it: a root moves as often as
    /// the user asks, and each move re-derives this. `false` only for a session
    /// the registry does not have.
    pub fn set_skills(&self, id: &SessionId, skills: SkillRegistry) -> bool {
        let mut sessions = self
            .sessions
            .lock()
            .expect("session registry mutex poisoned");
        let Some(record) = sessions
            .iter_mut()
            .find(|record| &record.summary.session_id == id)
        else {
            return false;
        };
        record.skills = Arc::new(skills);
        true
    }

    /// This session's skill registry, as `skills/list` reports it and as a turn
    /// expanding a `/name` line reads it.
    ///
    /// A cloned [`Arc`], so the lock is held for a pointer bump rather than for
    /// a copy of every registered body (LESSON-448).
    ///
    /// A session the registry does not have answers **empty**, for
    /// [`Self::conversation_snapshot`]'s reason: a session that does not exist
    /// dispatches no commands, and an empty registry is exactly the state ADR-2
    /// already requires every consumer to handle — it is what a machine with no
    /// `~/.claude` has, and what a new client synthesizes from an old daemon's
    /// `METHOD_NOT_FOUND`.
    #[must_use]
    pub fn skills(&self, id: &SessionId) -> Arc<SkillRegistry> {
        self.sessions
            .lock()
            .expect("session registry mutex poisoned")
            .iter()
            .find(|record| &record.summary.session_id == id)
            .map(|record| Arc::clone(&record.skills))
            .unwrap_or_default()
    }

    /// Replace this session's repository notes (REQ-612 BR-1, ADR-3).
    ///
    /// Called from the **three** sites ADR-3 names and nowhere else: at
    /// `session/create`, inside `session/set_cwd` before the move is announced,
    /// and from the turn's `assemble` stage when BR-6's staleness check says the
    /// file moved. `/context on|off` reaches it through the first of those
    /// spellings, since a switch that changed is a state that changed.
    ///
    /// Unconditional, like [`Self::set_skills`] beside it: the file is re-read
    /// as often as its key moves. `false` only for a session the registry does
    /// not have.
    ///
    /// **The publish is the caller's, and it happens after this returns.** The
    /// event bus is not to be touched while this mutex is held — the
    /// `set_session_cwd` discipline, and the reason `store_session_repo_context`
    /// separates the store from the announcement.
    pub fn set_repo_context(&self, id: &SessionId, state: RepoContextState) -> bool {
        let mut sessions = self
            .sessions
            .lock()
            .expect("session registry mutex poisoned");
        let Some(record) = sessions
            .iter_mut()
            .find(|record| &record.summary.session_id == id)
        else {
            return false;
        };
        record.repo_context = Arc::new(state);
        true
    }

    /// This session's repository notes, as the next turn's staleness check
    /// compares them and as `/context` reports them.
    ///
    /// A cloned [`Arc`], so the lock is held for a pointer bump rather than for
    /// a copy of the file's text (LESSON-448) — the same trade
    /// [`Self::skills`] makes.
    ///
    /// A session the registry does not have answers [`RepoContextState::Absent`],
    /// for [`Self::skills`]'s reason: a session that does not exist carries no
    /// notes, and `Absent` is a state every consumer already handles — it is
    /// what a session at a `home` root has.
    #[must_use]
    pub fn repo_context(&self, id: &SessionId) -> Arc<RepoContextState> {
        self.sessions
            .lock()
            .expect("session registry mutex poisoned")
            .iter()
            .find(|record| &record.summary.session_id == id)
            .map_or_else(
                || Arc::new(RepoContextState::Absent),
                |record| Arc::clone(&record.repo_context),
            )
    }

    /// Claim the right to publish `triple` as this session's
    /// `repo_context_state`, and record it (REQ-612 BR-3, AC-3).
    ///
    /// `true` when the caller should announce — which is when the triple differs
    /// from the last one published for this session, **or** when `always` says
    /// the stored state itself moved. `false` when a client has already been
    /// told exactly this, and the news would be a duplicate line on every prompt
    /// of a session whose notes have not moved.
    ///
    /// # Two reasons to publish, and both are needed
    ///
    /// `always` is the *file* moving: a `/cd` between two repositories whose
    /// notes happen to be the same size renders an identical triple and is
    /// still a different file, so the client is owed the line.
    ///
    /// The triple is the *render* moving. The turn path renders at the route's
    /// own cap, so a session that reroutes from an 8,192-cap route to a
    /// narrower-cap one has a stored state that did not change and a truncation the
    /// user has not been told about — which is exactly the silence BR-3 forbids.
    /// Gating the publish on [`Self::set_repo_context`]'s `false` (the shape
    /// this replaced) could not see that, because nothing about the *file*
    /// moved.
    ///
    /// The triple is `(state, truncated, resident_bytes)` — the three fields a
    /// route can move on its own. `source`, `bytes_on_disk` and `reason` are
    /// facts about the file and are covered by `always`.
    ///
    /// # The record moves only when the announcement does
    ///
    /// A triple stored without being published would let a later, identical
    /// render be suppressed against a line no client ever saw. So this writes
    /// exactly when it returns `true`, read and write under one lock, so two
    /// turns of one session cannot both decide they are the change. The publish
    /// itself is the caller's and happens after this returns, outside the mutex
    /// — the [`Self::set_repo_context`] discipline.
    ///
    /// `false` for a session the registry does not have: a session that is gone
    /// has no client to tell.
    pub fn claim_repo_context_publish(
        &self,
        id: &SessionId,
        triple: (RepoContextStateKind, bool, u64),
        always: bool,
    ) -> bool {
        let mut sessions = self
            .sessions
            .lock()
            .expect("session registry mutex poisoned");
        let Some(record) = sessions
            .iter_mut()
            .find(|record| &record.summary.session_id == id)
        else {
            return false;
        };
        if !always && record.repo_context_published == triple {
            return false;
        }
        record.repo_context_published = triple;
        true
    }

    /// Replace this session's repository notes **and** claim the right to
    /// announce `triple` for them, under one lock (REQ-612 BR-1/BR-3).
    ///
    /// [`Self::set_repo_context`] followed by
    /// [`Self::claim_repo_context_publish`] is the same two writes and is
    /// **not** the same operation. Between the two calls the mutex is free, so
    /// two concurrent `/cd`s can interleave: A stores its state, B stores its
    /// state and claims its triple, then A claims *its* triple over B's. The
    /// registry is then holding B's file beside A's published record, and the
    /// next turn measures its news against a line no client was ever sent —
    /// which is the exact failure the published record exists to prevent.
    ///
    /// So the lifecycle sites take this instead, and it is the only shape that
    /// makes "the state and the line about it move together" a property of the
    /// registry rather than of two calls staying adjacent.
    ///
    /// The claim is unconditional here — this is only reached once the caller
    /// has established that the **state itself** moved, which is `always` by
    /// construction; the turn path's gate is the conditional one, because a
    /// route can move the render without moving the file.
    ///
    /// `false` for a session the registry does not have: nothing is stored and
    /// there is no client to tell. The publish is still the caller's and still
    /// happens after this returns, outside the mutex (LESSON-448).
    pub fn store_and_claim_repo_context(
        &self,
        id: &SessionId,
        state: RepoContextState,
        triple: (RepoContextStateKind, bool, u64),
    ) -> bool {
        let mut sessions = self
            .sessions
            .lock()
            .expect("session registry mutex poisoned");
        let Some(record) = sessions
            .iter_mut()
            .find(|record| &record.summary.session_id == id)
        else {
            return false;
        };
        record.repo_context = Arc::new(state);
        record.repo_context_published = triple;
        true
    }

    /// Settle this session's generation state for the root it now stands on
    /// (REQ-613 BR-1, ADR-1).
    ///
    /// Called from the **one** derivation both lifecycle sites funnel through
    /// (`DaemonRuntime::store_session_repo_context`), so `session/create`,
    /// `/cd` and `/context on|off` cannot come to disagree about when the offer
    /// is armed.
    ///
    /// # Two rules, and the root is what tells them apart
    ///
    /// A **different** root is a different question: BR-1 says a `/cd` into
    /// another project raises the offer again, even for a session that declined
    /// at the last one, so the state is replaced whatever it held.
    ///
    /// The **same** root is the same question, and only a state nobody has
    /// answered may move ([`GenerationState::rearmable`]). Without that, a
    /// `/context off` followed by `/context on` — two calls that re-derive the
    /// notes and land back on `Absent` — would turn a decline the user gave
    /// five minutes ago back into a prompt, and BR-1's "not raised again for
    /// that root in that session" would hold only for sessions that never
    /// touched the switch.
    ///
    /// `false` for a session the registry does not have: there is no record to
    /// arm and nobody to ask.
    pub fn arm_generation(&self, id: &SessionId, root: &Path, state: GenerationState) -> bool {
        let mut sessions = self
            .sessions
            .lock()
            .expect("session registry mutex poisoned");
        let Some(record) = sessions
            .iter_mut()
            .find(|record| &record.summary.session_id == id)
        else {
            return false;
        };
        let same_root = record.generation_root.as_deref() == Some(root);
        if !same_root || record.generation.rearmable() {
            record.generation = state;
            record.generation_root = Some(root.to_path_buf());
        }
        true
    }

    /// Claim the right to raise this session's generation offer, moving
    /// [`GenerationState::Pending`] to [`GenerationState::Offered`] (BR-1).
    ///
    /// `true` for the **one** turn that takes the claim; every other turn — and
    /// every later iteration of the same turn's tool loop — reads a state that
    /// is no longer `Pending` and raises nothing. Read and write under one
    /// lock, for [`Self::claim_title`]'s reason: two concurrent turns of one
    /// session must not both decide they are the one asking.
    ///
    /// The claim is taken **before** the offer, not after it succeeds. A guard
    /// keyed on "has this session generated yet" would re-raise the offer on
    /// every prompt of a session whose answer never came — the per-turn model
    /// call `claim_title` exists to prevent, with a permission prompt in front
    /// of it.
    ///
    /// The caller **must** store a terminal state when the run finishes
    /// ([`Self::set_generation`]); a claim that is never settled leaves the
    /// session at `Offered`, which raises nothing again and is the fail-closed
    /// end of that mistake.
    pub fn claim_generation(&self, id: &SessionId) -> bool {
        let mut sessions = self
            .sessions
            .lock()
            .expect("session registry mutex poisoned");
        let Some(record) = sessions
            .iter_mut()
            .find(|record| &record.summary.session_id == id)
        else {
            return false;
        };
        if record.generation != GenerationState::Pending {
            return false;
        }
        record.generation = GenerationState::Offered;
        true
    }

    /// Record what a generation run ended as (REQ-613 BR-1, BR-9).
    ///
    /// Unconditional, unlike [`Self::arm_generation`]: this is the answer to the
    /// question that method armed, and the caller has just finished asking it.
    /// `false` only for a session the registry does not have.
    pub fn set_generation(&self, id: &SessionId, state: GenerationState) -> bool {
        let mut sessions = self
            .sessions
            .lock()
            .expect("session registry mutex poisoned");
        let Some(record) = sessions
            .iter_mut()
            .find(|record| &record.summary.session_id == id)
        else {
            return false;
        };
        record.generation = state;
        true
    }

    /// Where this session stands on the offer.
    ///
    /// A session the registry does not have answers
    /// [`GenerationState::Suppressed`], which is the fail-closed reading and the
    /// same one the constructor seeds: a session that does not exist is not
    /// owed a prompt and has no working tree to write into.
    #[must_use]
    pub fn generation(&self, id: &SessionId) -> GenerationState {
        self.sessions
            .lock()
            .expect("session registry mutex poisoned")
            .iter()
            .find(|record| &record.summary.session_id == id)
            .map_or(GenerationState::Suppressed, |record| record.generation)
    }

    /// Set this session's `/context` switch (REQ-612 BR-2).
    ///
    /// Session-scoped and never persisted: the durable half is
    /// `[context] repo_file` through `config/set`, and the two are composed —
    /// not merged — by the reader, so this write cannot be mistaken for a
    /// machine-wide one. `false` only for a session the registry does not have.
    ///
    /// There is no verb that clears it back to `None`. A session that has been
    /// switched has an opinion for the rest of its life, which is what makes
    /// `/context off` survive a `config/set` that turns the default on.
    pub fn set_context_switch(&self, id: &SessionId, enabled: bool) -> bool {
        let mut sessions = self
            .sessions
            .lock()
            .expect("session registry mutex poisoned");
        let Some(record) = sessions
            .iter_mut()
            .find(|record| &record.summary.session_id == id)
        else {
            return false;
        };
        record.context_switch = Some(enabled);
        true
    }

    /// This session's `/context` switch, or `None` while it follows the durable
    /// default.
    ///
    /// A session the registry does not have answers `None`, which is the same
    /// answer as "has not been switched" and is the honest one: nobody has
    /// switched a session that does not exist.
    #[must_use]
    pub fn context_switch(&self, id: &SessionId) -> Option<bool> {
        self.sessions
            .lock()
            .expect("session registry mutex poisoned")
            .iter()
            .find(|record| &record.summary.session_id == id)
            .and_then(|record| record.context_switch)
    }

    /// This session's retained conversation, as the next turn replays it
    /// (REQ-567 BR-1).
    ///
    /// A clone rather than a borrow, deliberately: the caller replays these
    /// blocks into a `ContextManager` and then runs a whole turn against it, and
    /// handing out a borrow would mean handing out the lock for that long
    /// (LESSON-448). The copy costs one pass over the retained blocks per prompt,
    /// against a turn that is about to spend seconds in a model.
    ///
    /// A session the registry does not have snapshots as **empty**, which is the
    /// honest answer — a conversation that does not exist has said nothing — and
    /// keeps the caller's seeding path free of a "session vanished" branch it
    /// cannot act on anyway.
    #[must_use]
    pub fn conversation_snapshot(&self, id: &SessionId) -> Conversation {
        self.sessions
            .lock()
            .expect("session registry mutex poisoned")
            .iter()
            .find(|record| &record.summary.session_id == id)
            .map(|record| record.conversation.clone())
            .unwrap_or_default()
    }

    /// Replace this session's conversation with `blocks` — **the whole vector**,
    /// which is the atomic unit of REQ-567 BR-6.
    ///
    /// ## Why replacement and never an append
    ///
    /// What the turn's manager holds when the turn ends *is* the retained view:
    /// the model text as the containment cut kept it (BUG-147), the blocks a
    /// mid-turn compaction rewrote (BR-4), the tool results as they folded in.
    /// Committing that vector is a move, not a re-derivation — so the store can
    /// never disagree with the harness about what the conversation is. An append
    /// API would have to be told which blocks are new, and a compaction that
    /// rewrote the history it is appending to has no such answer.
    ///
    /// ## Rollback is by omission, not by undo
    ///
    /// This is the **only** writer, so a turn that fails simply never calls it
    /// and the pre-turn vector stands untouched (BR-6/AC-5). There is no partial
    /// state to roll back because there was never a partial write; the atomicity
    /// is a property of the API's shape rather than of a call site remembering to
    /// restore something.
    ///
    /// A commit for a session the registry does not have stores nothing — there
    /// is no record to hold it, and inventing one would resurrect a session
    /// whose id nothing will ever look up again.
    ///
    /// # Panics
    ///
    /// If the registry mutex is poisoned. [`Self::try_commit_conversation`] is
    /// the non-panicking twin the drop path uses.
    pub fn commit_conversation(&self, id: &SessionId, retained: RetainedContext) {
        let mut sessions = self
            .sessions
            .lock()
            .expect("session registry mutex poisoned");
        Self::write_conversation(&mut sessions, id, retained);
    }

    /// The same commit, on a path that must not panic (REQ-567 verify).
    ///
    /// `Drop` runs during unwinding, and a panic raised inside a drop that is
    /// itself running because of a panic aborts the process — the whole daemon,
    /// every other session with it. So the drop-path commit takes the lock with
    /// [`PoisonError::into_inner`](std::sync::PoisonError::into_inner) rather
    /// than `expect`. That is sound for *this* write specifically: it is a
    /// whole-vector replacement of one field, so there is no half-updated
    /// invariant a poisoned lock could be protecting — the very property BR-6
    /// already rests on.
    ///
    /// The explicit path keeps `expect`: a poisoned registry there is a bug to
    /// surface loudly, on a stack where surfacing it is safe.
    pub fn try_commit_conversation(&self, id: &SessionId, retained: RetainedContext) {
        let mut sessions = match self.sessions.lock() {
            Ok(sessions) => sessions,
            Err(poisoned) => poisoned.into_inner(),
        };
        Self::write_conversation(&mut sessions, id, retained);
    }

    /// The one write both commit paths perform.
    fn write_conversation(
        sessions: &mut [SessionRecord],
        id: &SessionId,
        retained: RetainedContext,
    ) {
        if let Some(record) = sessions
            .iter_mut()
            .find(|record| &record.summary.session_id == id)
        {
            record.conversation = Conversation::from_retained(retained);
        }
    }

    /// Empty this session's conversation and report how many blocks went
    /// (REQ-567 BR-8).
    ///
    /// The count is the `context_cleared` payload — a clear that reported
    /// nothing would leave the user unable to tell "cleared a long session" from
    /// "cleared a session that was already empty", and those are the two things
    /// they are most likely to want to know.
    ///
    /// **Only the conversation** (OQ-4): session taint, the user-pasted-URL set,
    /// and remembered permission grants all survive, because a routinely-typed
    /// clear must never silently widen egress or consent (LESSON-495).
    pub fn clear_conversation(&self, id: &SessionId) -> usize {
        let mut sessions = self
            .sessions
            .lock()
            .expect("session registry mutex poisoned");
        let Some(record) = sessions
            .iter_mut()
            .find(|record| &record.summary.session_id == id)
        else {
            return 0;
        };
        let dropped = record.conversation.len();
        record.conversation = Conversation::default();
        dropped
    }

    /// Claim this session for `turn_id`, or refuse and name the turn already
    /// running (REQ-567 BR-5, D-3).
    ///
    /// Two `session/prompt` calls on one session can be in flight at once — each
    /// runs on its own task — and both replaying the same snapshot and then both
    /// committing would fork the conversation: the second commit would erase the
    /// first turn's blocks wholesale. This is the gate that makes the transcript
    /// linear, and it refuses rather than queues (D-3): a refusal is immediate
    /// and retryable where a queue is silent and unbounded, and the CLI prompter
    /// is sequential, so a single well-behaved client never sees it.
    ///
    /// ## The check and the mark are one lock, for `claim_title`'s reason
    ///
    /// A `is_busy()` followed by a `mark_busy()` would let both turns read
    /// "free" and both proceed — the exact race this exists to close. So the
    /// read and the write happen under one lock and the claim is genuinely
    /// exclusive.
    ///
    /// ## The lock is not what is held for the turn
    ///
    /// The returned [`TurnClaim`] holds a flag on the record, released on drop,
    /// while the mutex is released before this function returns. Holding the
    /// registry lock across a turn would block every other session's `list`,
    /// `get`, and title claim on one model call (LESSON-448).
    ///
    /// # Errors
    ///
    /// [`TurnClaimError::InFlight`] when another turn holds the session, and
    /// [`TurnClaimError::NoSuchSession`] when there is no record to claim.
    pub fn try_begin_turn(
        &self,
        id: &SessionId,
        turn_id: &TurnId,
    ) -> Result<TurnClaim, TurnClaimError> {
        let mut sessions = self
            .sessions
            .lock()
            .expect("session registry mutex poisoned");
        let Some(record) = sessions
            .iter_mut()
            .find(|record| &record.summary.session_id == id)
        else {
            return Err(TurnClaimError::NoSuchSession {
                session_id: id.clone(),
            });
        };
        if let Some(in_flight) = &record.in_flight_turn {
            return Err(TurnClaimError::InFlight {
                session_id: id.clone(),
                turn_id: in_flight.clone(),
            });
        }
        record.in_flight_turn = Some(turn_id.clone());
        Ok(TurnClaim {
            sessions: Arc::clone(&self.sessions),
            session_id: id.clone(),
            turn_id: turn_id.clone(),
        })
    }

    /// Number of live sessions.
    #[must_use]
    pub fn count(&self) -> usize {
        self.sessions
            .lock()
            .expect("session registry mutex poisoned")
            .len()
    }

    /// Whether the registry holds no sessions.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.count() == 0
    }
}

impl Default for SessionRegistry {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::fixture_id;

    #[test]
    fn structured_session_requires_a_phase() {
        let reg = SessionRegistry::new();
        assert!(reg.create(SessionMode::Structured, None, None).is_err());

        let s = reg
            .create(SessionMode::Structured, Some(Phase::Spec), None)
            .unwrap();
        assert_eq!(s.mode, SessionMode::Structured);
        assert_eq!(s.phase, Some(Phase::Spec));
    }

    #[test]
    fn freeform_session_never_carries_a_phase() {
        let reg = SessionRegistry::new();
        let s = reg
            .create(SessionMode::Freeform, Some(Phase::Spec), None)
            .unwrap();
        assert_eq!(s.phase, None);
    }

    #[test]
    fn list_is_newest_first_and_get_finds_by_id() {
        let reg = SessionRegistry::new();
        assert!(reg.is_empty());

        let a = reg.create(SessionMode::Freeform, None, None).unwrap();
        let b = reg.create(SessionMode::Freeform, None, None).unwrap();

        let list = reg.list();
        assert_eq!(list.len(), 2);
        assert_eq!(list[0].session_id, b.session_id);
        assert_eq!(list[1].session_id, a.session_id);

        assert_eq!(reg.get(&a.session_id).unwrap().session_id, a.session_id);
        assert!(reg.get(&SessionId::from("does-not-exist")).is_none());
        assert_eq!(reg.count(), 2);
    }

    #[test]
    fn a_session_remembers_its_cwd() {
        // BUG-147: the cwd is the session's tool jail — it must survive from
        // session/create through get() to the prompt turn.
        let reg = SessionRegistry::new();
        let s = reg
            .create(
                SessionMode::Freeform,
                None,
                Some(PathBuf::from("/Users/dev/my-repo")),
            )
            .unwrap();
        assert_eq!(
            s.cwd.as_deref(),
            Some(std::path::Path::new("/Users/dev/my-repo"))
        );
        assert_eq!(
            reg.get(&s.session_id).unwrap().cwd,
            Some(PathBuf::from("/Users/dev/my-repo"))
        );
        // A client that sends none stores none (daemon-root fallback applies).
        let bare = reg.create(SessionMode::Freeform, None, None).unwrap();
        assert_eq!(bare.cwd, None);
    }

    /// REQ-583 BR-7: `set_cwd` rewrites the one stored root fact in place, for
    /// a session that had one and for one that was on the daemon fallback, and
    /// refuses a session the registry does not have.
    #[test]
    fn a_session_cwd_can_be_moved_and_a_ghost_cannot() {
        let reg = SessionRegistry::new();
        let s = reg
            .create(
                SessionMode::Freeform,
                None,
                Some(PathBuf::from("/Users/dev/my-repo")),
            )
            .unwrap();
        assert!(reg.set_cwd(&s.session_id, PathBuf::from("/Users/dev/other")));
        assert_eq!(
            reg.get(&s.session_id).unwrap().cwd,
            Some(PathBuf::from("/Users/dev/other")),
            "the stored path must be the new one — the next turn's probe reads it"
        );
        // Unconditional: a second move lands too (unlike a title, a root moves
        // as often as the user asks).
        assert!(reg.set_cwd(&s.session_id, PathBuf::from("/Users/dev/third")));
        assert_eq!(
            reg.get(&s.session_id).unwrap().cwd,
            Some(PathBuf::from("/Users/dev/third"))
        );

        // A session that sent no cwd (fallback root) acquires one.
        let bare = reg.create(SessionMode::Freeform, None, None).unwrap();
        assert!(reg.set_cwd(&bare.session_id, PathBuf::from("/Users/dev/late")));
        assert_eq!(
            reg.get(&bare.session_id).unwrap().cwd,
            Some(PathBuf::from("/Users/dev/late"))
        );

        // The other session is untouched — the move is keyed by id.
        assert_eq!(
            reg.get(&s.session_id).unwrap().cwd,
            Some(PathBuf::from("/Users/dev/third"))
        );
        assert!(
            !reg.set_cwd(&SessionId::from("sess-ghost"), PathBuf::from("/x")),
            "a session the registry never had cannot be moved"
        );
    }

    /// A throwaway tree holding one `commands/<name>.md`, removed on drop.
    ///
    /// [`SkillRegistry`] has exactly two constructors — `Default` and
    /// [`crate::skills::discover`] — and deliberately no test-only builder
    /// (LESSON-544: a fixture that reaches past the constructor leaves the
    /// constructor unguarded). So a registry a test can tell apart from another
    /// one is a real directory with a real file in it.
    struct SkillTree {
        root: PathBuf,
    }

    impl SkillTree {
        fn holding(name: &str) -> Self {
            use std::sync::atomic::{AtomicUsize, Ordering};
            static SEQ: AtomicUsize = AtomicUsize::new(0);
            let seq = SEQ.fetch_add(1, Ordering::SeqCst);
            let root = PathBuf::from("/tmp")
                .join(format!("tsess{:x}{seq:x}", std::process::id() & 0xffff));
            let commands = root.join(".claude").join("commands");
            std::fs::create_dir_all(&commands).unwrap();
            std::fs::write(commands.join(format!("{name}.md")), "body\n").unwrap();
            Self { root }
        }

        fn registry(&self) -> SkillRegistry {
            crate::skills::discover(
                None,
                &self.root,
                teton_protocol::methods::RootKind::Project,
                &crate::skills::RealFs,
            )
        }
    }

    impl Drop for SkillTree {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.root);
        }
    }

    /// REQ-585 BR-1: a session holds one skill registry, replaced wholesale
    /// when its root moves — the `/cd` shape, at the store.
    ///
    /// Replacement rather than a merge, for the reason the conversation is
    /// replaced rather than appended to: after a `/cd` the project half of the
    /// old registry names files under a root the session no longer stands on,
    /// and a merge would leave those rows dispatchable.
    #[test]
    fn a_sessions_skill_registry_is_stored_and_replaced_whole() {
        let before = SkillTree::holding("alpha");
        let after = SkillTree::holding("beta");
        let names = |registry: &SkillRegistry| -> Vec<String> {
            registry
                .skills()
                .iter()
                .map(|skill| skill.name.clone())
                .collect()
        };

        let reg = SessionRegistry::new();
        let session = reg
            .create(SessionMode::Freeform, None, Some(before.root.clone()))
            .unwrap();
        assert!(
            reg.skills(&session.session_id).is_empty(),
            "a freshly created session holds no registry until one is discovered \
             for it — the empty state ADR-2 already requires every consumer to \
             handle"
        );

        assert!(reg.set_skills(&session.session_id, before.registry()));
        assert_eq!(names(&reg.skills(&session.session_id)), vec!["alpha"]);

        assert!(reg.set_skills(&session.session_id, after.registry()));
        assert_eq!(
            names(&reg.skills(&session.session_id)),
            vec!["beta"],
            "the second discovery replaces the first whole: a row from the root \
             the session has left must not survive the move"
        );

        assert!(
            !reg.set_skills(&SessionId::from("sess-ghost"), before.registry()),
            "a session the registry never had holds nothing"
        );
        assert!(
            reg.skills(&SessionId::from("sess-ghost")).is_empty(),
            "and answers empty rather than inventing a record"
        );
    }

    /// REQ-583 BR-6: the one cwd validator refuses a relative path and a
    /// missing or non-directory path — typed, and **naming the path** in each
    /// refusal's one root-neutral sentence — and accepts a real directory.
    #[test]
    fn the_cwd_validator_names_the_path_in_every_refusal() {
        let relative = validate_session_cwd(std::path::Path::new("relative/dir"))
            .expect_err("a relative cwd is refused");
        assert_eq!(
            relative,
            CwdRefusal::NotAbsolute(PathBuf::from("relative/dir"))
        );
        assert_eq!(
            relative.to_string(),
            "path `relative/dir` must be an absolute path"
        );

        let missing = validate_session_cwd(std::path::Path::new("/nope-teton-sessions-test"))
            .expect_err("a nonexistent cwd is refused");
        assert_eq!(
            missing,
            CwdRefusal::NotADirectory(PathBuf::from("/nope-teton-sessions-test"))
        );
        assert_eq!(
            missing.to_string(),
            "path `/nope-teton-sessions-test` does not exist or is not a directory"
        );

        // A file, not a directory: same refusal, same shape.
        let dir = std::env::temp_dir().join(format!(
            "teton-sessions-root-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or(0)
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let file = dir.join("a-file");
        std::fs::write(&file, b"x").unwrap();
        let not_dir = validate_session_cwd(&file).expect_err("a file is not a directory");
        assert_eq!(
            not_dir.to_string(),
            format!(
                "path `{}` does not exist or is not a directory",
                file.display()
            )
        );
        for (refusal, named) in [
            (&relative, "relative/dir".to_owned()),
            (&missing, "/nope-teton-sessions-test".to_owned()),
            (&not_dir, file.display().to_string()),
        ] {
            // The path is the caller's own and may spell anything (a temp dir
            // under a `cwd`-named parent, say), so it is stripped before the
            // sentence itself is judged root-neutral.
            let text = refusal.to_string().replace(&named, "<path>");
            assert!(
                text.contains("`<path>`"),
                "the refusal must name the path: {text}"
            );
            assert!(
                !text.contains("cwd"),
                "wire jargon in a user-facing refusal: {text}"
            );
            assert!(!text.contains("session root"), "{text}");
        }

        assert_eq!(validate_session_cwd(&dir), Ok(()));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn session_ids_are_unique() {
        let reg = SessionRegistry::new();
        let a = reg.create(SessionMode::Freeform, None, None).unwrap();
        let b = reg.create(SessionMode::Freeform, None, None).unwrap();
        assert_ne!(a.session_id, b.session_id);
    }

    /// **REQ-569 BR-8 / ADR-H.** Every id is `sess-` plus 26 base32 characters
    /// drawn from Crockford's alphabet — so an id is well-formed by shape, and
    /// the shape itself excludes the `i`/`l`/`o`/`u` a transcription slip would
    /// produce.
    ///
    /// Asserted on the *format*, never on exact values: an id is 128 random
    /// bits, so a test that named one would be a test of the RNG.
    #[test]
    fn a_session_id_is_the_prefix_plus_26_base32_characters() {
        let reg = SessionRegistry::new();
        let s = reg.create(SessionMode::Freeform, None, None).unwrap();
        let id = s.session_id.to_string();

        let body = id
            .strip_prefix(SESSION_ID_PREFIX)
            .unwrap_or_else(|| panic!("an id must keep the `sess-` prefix logs read: {id}"));
        assert_eq!(
            body.len(),
            SESSION_ID_BODY_LEN,
            "128 bits is 26 base32 characters: {id}"
        );
        assert!(
            body.bytes().all(|c| SESSION_ID_ALPHABET.contains(&c)),
            "an id must be Crockford base32 — no i/l/o/u to mistype: {id}"
        );
    }

    /// **REQ-569 BR-8 / ADR-H, the property the change exists for.** Two
    /// sessions from one daemon are not sequentially related: knowing one tells
    /// you nothing about the next.
    ///
    /// The old scheme failed all three of these at once — `sess-0` and `sess-1`
    /// are all-digit, differ in a single position, and are the literal integers
    /// `0` and `1` — so each assertion below is a distinct way the counter could
    /// come back. The "differ in more than one position" check is probabilistic
    /// in principle and decided in practice: two independent 26-character base32
    /// strings agree in 25 of 26 positions with probability below 2^-120, which
    /// is far under this suite's real flake floor.
    ///
    /// It deliberately asserts nothing about *authorization*. Unguessability is
    /// defense in depth (ADR-H); the grants are the access control, and no test
    /// here should imply otherwise.
    #[test]
    fn session_ids_are_not_sequentially_related() {
        let reg = SessionRegistry::new();
        let ids: Vec<String> = (0..64)
            .map(|_| {
                reg.create(SessionMode::Freeform, None, None)
                    .unwrap()
                    .session_id
                    .to_string()
            })
            .collect();

        let unique: std::collections::HashSet<&String> = ids.iter().collect();
        assert_eq!(unique.len(), ids.len(), "64 sessions minted a repeated id");

        for (n, id) in ids.iter().enumerate() {
            let body = id.strip_prefix(SESSION_ID_PREFIX).unwrap();
            assert!(
                body.parse::<u128>().is_err(),
                "an all-digit body is a counter wearing a new name: {id}"
            );
            assert_ne!(
                id,
                &format!("{SESSION_ID_PREFIX}{n}"),
                "the sequential scheme is back"
            );
        }

        // Adjacency, at the character level: a counter's consecutive ids differ
        // in one place. Random ones differ nearly everywhere.
        for pair in ids.windows(2) {
            let differing = pair[0]
                .bytes()
                .zip(pair[1].bytes())
                .filter(|(a, b)| a != b)
                .count();
            assert!(
                differing > 1,
                "consecutive ids differ in {differing} position(s) — that is a \
                 counter, not entropy: {} then {}",
                pair[0],
                pair[1]
            );
        }
    }

    // -- the `title` duty's once-only guard (REQ-561 TASK-062) ---------------

    /// **AC-6 at its source.** The claim is granted exactly once, however many
    /// turns a session runs — and the second caller is refused *before* it can
    /// reach a model, which is what makes "requested once" a property of the
    /// registry rather than of every call site remembering.
    #[test]
    fn a_session_grants_its_title_attempt_exactly_once() {
        let reg = SessionRegistry::new();
        let s = reg.create(SessionMode::Freeform, None, None).unwrap();

        assert!(reg.claim_title(&s.session_id), "the first turn claims it");
        for turn in 2..=5 {
            assert!(
                !reg.claim_title(&s.session_id),
                "turn {turn} claimed a second title attempt"
            );
        }
    }

    /// **The cost trap, at the layer that decides it.** A claim that is spent and
    /// then *fails* leaves the session unnamed — and must not be handed back on
    /// the next turn. Keying the guard on `title.is_none()` alone would do
    /// exactly that, and would keep doing it for the life of the session.
    #[test]
    fn a_claim_that_produced_no_title_is_not_granted_again() {
        let reg = SessionRegistry::new();
        let s = reg.create(SessionMode::Freeform, None, None).unwrap();

        assert!(reg.claim_title(&s.session_id));
        // The duty failed: nothing was stored. The session is still untitled...
        assert_eq!(reg.get(&s.session_id).unwrap().title, None);
        // ...and that is not a reason to pay for another call.
        assert!(
            !reg.claim_title(&s.session_id),
            "a failed title must not re-fire on every subsequent turn"
        );
    }

    /// A session that already carries a title is never a candidate at all, so a
    /// restored or client-supplied name costs nothing to keep.
    #[test]
    fn a_session_that_already_has_a_title_claims_nothing() {
        let reg = SessionRegistry::new();
        let s = reg.create(SessionMode::Freeform, None, None).unwrap();
        assert!(reg.set_title(&s.session_id, "Already named"));

        assert!(
            !reg.claim_title(&s.session_id),
            "a titled session must not buy a naming call"
        );
    }

    /// **BR-9.** An existing title is never overwritten, and the caller can tell:
    /// the `false` return is what stops a second `session_titled` reaching the
    /// wire.
    #[test]
    fn an_existing_title_is_never_overwritten() {
        let reg = SessionRegistry::new();
        let s = reg.create(SessionMode::Freeform, None, None).unwrap();

        assert!(reg.set_title(&s.session_id, "Retry the download client"));
        assert!(!reg.set_title(&s.session_id, "Something else entirely"));
        assert_eq!(
            reg.get(&s.session_id).unwrap().title.as_deref(),
            Some("Retry the download client")
        );
        // And the stored title is what `session/list` shows, not a second field.
        assert_eq!(
            reg.list()[0].title.as_deref(),
            Some("Retry the download client")
        );
    }

    /// A session the registry never had claims nothing and stores nothing —
    /// there is no record to spend or to name.
    #[test]
    fn an_unknown_session_neither_claims_nor_stores_a_title() {
        let reg = SessionRegistry::new();
        let ghost = SessionId::from("never-created");
        assert!(!reg.claim_title(&ghost));
        assert!(!reg.set_title(&ghost, "A name for nothing"));
    }

    /// Two sessions are two claims: the guard is per session, not per daemon.
    #[test]
    fn each_session_gets_its_own_title_claim() {
        let reg = SessionRegistry::new();
        let a = reg.create(SessionMode::Freeform, None, None).unwrap();
        let b = reg.create(SessionMode::Freeform, None, None).unwrap();

        assert!(reg.claim_title(&a.session_id));
        assert!(reg.claim_title(&b.session_id));
        assert!(!reg.claim_title(&a.session_id));
    }

    // -- the session's conversation (REQ-567 TASK-092) ------------------------

    use crate::harness::context::{BlockRole, ContextManager, Provenance, ToolProvenance};

    fn user_block(text: &str) -> ContextBlock {
        ContextBlock {
            role: BlockRole::User,
            text: text.to_owned(),
            provenance: Provenance::user(),
        }
    }

    fn model_block(text: &str) -> ContextBlock {
        ContextBlock {
            role: BlockRole::Assistant,
            text: text.to_owned(),
            provenance: Provenance::Model,
        }
    }

    fn tool_block(tool: &str, path: &str, text: &str) -> ContextBlock {
        ContextBlock {
            role: BlockRole::Tool,
            text: text.to_owned(),
            provenance: Provenance::Tool {
                tool: tool.to_owned(),
                provenance: ToolProvenance::path(fixture_id(path)),
            },
        }
    }

    fn texts(blocks: &[ContextBlock]) -> Vec<&str> {
        blocks.iter().map(|b| b.text.as_str()).collect()
    }

    /// What a turn hands the store: blocks with nothing yet truncated and
    /// nothing yet forgotten. The two extra facts a real commit carries have
    /// their own tests below.
    fn retained(blocks: Vec<ContextBlock>) -> RetainedContext {
        RetainedContext::from_blocks(blocks)
    }

    /// **BR-1 at its source.** A session starts with nothing to say, and what a
    /// turn commits is exactly what the next turn snapshots — same blocks, same
    /// order, same roles and provenance.
    #[test]
    fn what_a_turn_commits_is_what_the_next_turn_snapshots() {
        let reg = SessionRegistry::new();
        let s = reg.create(SessionMode::Freeform, None, None).unwrap();
        assert!(
            reg.conversation_snapshot(&s.session_id).is_empty(),
            "a new session has said nothing"
        );

        let committed = vec![
            user_block("what does sessions.rs do?"),
            model_block("it is the daemon's session store"),
            tool_block("read", "crates/tetond/src/sessions.rs", "//! The session…"),
        ];
        reg.commit_conversation(&s.session_id, retained(committed.clone()));

        assert_eq!(reg.conversation_snapshot(&s.session_id).blocks(), committed);
    }

    /// **BR-6, both halves, in the one place that can hold them.** A turn mutates
    /// a snapshot it took — that is what a turn *is* — and none of it reaches the
    /// registry until the turn commits. A turn that fails never commits, so the
    /// next prompt sees the pre-turn conversation byte for byte (AC-5).
    ///
    /// And the commit that does land is a **whole-vector replacement**, not an
    /// append: it is the manager's post-turn view, which a mid-turn compaction is
    /// free to have rewritten.
    #[test]
    fn only_a_commit_changes_the_conversation_and_it_replaces_the_whole_vector() {
        let reg = SessionRegistry::new();
        let s = reg.create(SessionMode::Freeform, None, None).unwrap();
        let before = vec![user_block("prompt one"), model_block("answer one")];
        reg.commit_conversation(&s.session_id, retained(before.clone()));

        // A turn that errors: it read the conversation, grew it, and died.
        let mut failed_turn = reg.conversation_snapshot(&s.session_id).blocks().to_vec();
        failed_turn.push(user_block("prompt two"));
        failed_turn.push(tool_block("read", "a.rs", "half a file"));
        drop(failed_turn);

        assert_eq!(
            reg.conversation_snapshot(&s.session_id).blocks(),
            before,
            "a turn that never committed left blocks behind"
        );

        // A turn that completes, having compacted its history mid-turn: the
        // vector it commits does not contain the blocks it replaced.
        let compacted = vec![
            tool_block("compact", "a.rs", "[earlier conversation compacted]"),
            user_block("prompt two"),
            model_block("answer two"),
        ];
        reg.commit_conversation(&s.session_id, retained(compacted.clone()));

        assert_eq!(
            reg.conversation_snapshot(&s.session_id).blocks(),
            compacted,
            "a commit must replace the conversation, never extend it"
        );
    }

    /// **BR-3 / the truncation note, at the store.** A conversation is three
    /// facts, not one: the blocks, whether history was dropped, and the egress
    /// provenance of what was dropped. All three cross the registry, because a
    /// commit that carried only the vector would retract the honesty note on the
    /// next prompt and launder a truncated-away boundary read.
    #[test]
    fn a_commit_carries_the_two_facts_that_are_not_in_the_blocks() {
        let reg = SessionRegistry::new();
        let s = reg.create(SessionMode::Freeform, None, None).unwrap();

        // A turn that truncated a `local-only` read away, as its manager would
        // hand it over.
        let mut ctx = ContextManager::new("head", 1_000_000).with_budget_bytes(2_000);
        ctx.push_tool_result(
            "read",
            Some(fixture_id("secrets/prod.env")),
            "x".repeat(1_500),
        );
        ctx.push_user("x".repeat(1_500));
        ctx.push_user("and now this");
        let _ = ctx.truncate_to_budget();
        assert!(ctx.was_truncated(), "fixture: the turn must have truncated");

        reg.commit_conversation(&s.session_id, ctx.into_retained());

        let carried = reg.conversation_snapshot(&s.session_id);
        assert!(
            carried.retained().was_truncated(),
            "the next prompt would stop saying that history is missing"
        );
        assert!(
            carried
                .retained()
                .dropped_provenance()
                .sources()
                .contains(&fixture_id("secrets/prod.env")),
            "the next prompt would carry boundary-derived content with nothing \
             to scope it"
        );
        assert!(
            !carried.blocks().iter().any(|b| matches!(
                &b.provenance,
                Provenance::Tool {
                    provenance: ToolProvenance::Sources(paths),
                    ..
                } if paths.contains(&fixture_id("secrets/prod.env"))
            )),
            "fixture: the boundary block really was dropped, so the provenance \
             above can only have come from the accumulator"
        );

        // And a clear takes all three, not just the blocks: a cleared session
        // that still reported truncated history would print the note over an
        // empty conversation.
        assert!(reg.clear_conversation(&s.session_id) > 0);
        let cleared = reg.conversation_snapshot(&s.session_id);
        assert!(cleared.is_empty());
        assert!(!cleared.retained().was_truncated());
        assert!(cleared.retained().dropped_provenance().is_empty());
    }

    /// **BR-8.** Clearing empties the conversation and reports what went — the
    /// number `context_cleared` carries — and clearing again reports nothing,
    /// because nothing was there.
    #[test]
    fn clearing_empties_the_conversation_and_reports_what_went() {
        let reg = SessionRegistry::new();
        let s = reg.create(SessionMode::Freeform, None, None).unwrap();
        reg.commit_conversation(
            &s.session_id,
            retained(vec![
                user_block("one"),
                model_block("two"),
                tool_block("read", "a.rs", "three"),
            ]),
        );

        assert_eq!(reg.clear_conversation(&s.session_id), 3);
        assert!(reg.conversation_snapshot(&s.session_id).is_empty());
        assert_eq!(
            reg.clear_conversation(&s.session_id),
            0,
            "clearing an empty conversation drops nothing"
        );
    }

    /// **BR-2 / AC-12.** Two sessions interleaving their turns each carry only
    /// their own blocks. The isolation is the key, not a filter someone applies:
    /// a snapshot reads one record.
    #[test]
    fn interleaved_sessions_never_see_each_others_blocks() {
        let reg = SessionRegistry::new();
        let a = reg.create(SessionMode::Freeform, None, None).unwrap();
        let b = reg.create(SessionMode::Freeform, None, None).unwrap();

        reg.commit_conversation(&a.session_id, retained(vec![user_block("A's secret plan")]));
        reg.commit_conversation(
            &b.session_id,
            retained(vec![user_block("B's unrelated bug")]),
        );
        reg.commit_conversation(
            &a.session_id,
            retained(vec![
                user_block("A's secret plan"),
                model_block("A's answer"),
            ]),
        );

        assert_eq!(
            texts(reg.conversation_snapshot(&a.session_id).blocks()),
            ["A's secret plan", "A's answer"]
        );
        assert_eq!(
            texts(reg.conversation_snapshot(&b.session_id).blocks()),
            ["B's unrelated bug"]
        );
        // And clearing one session leaves the other's conversation alone.
        assert_eq!(reg.clear_conversation(&a.session_id), 2);
        assert_eq!(
            texts(reg.conversation_snapshot(&b.session_id).blocks()),
            ["B's unrelated bug"]
        );
    }

    /// **BR-5 / D-3.** One session runs one turn: the second claim is refused
    /// while the first is live, and the refusal names the turn holding it rather
    /// than surfacing as a generic failure (LESSON-456). When the claim drops —
    /// including on an aborted or panicking turn, which is the whole reason it is
    /// a guard — the session is admitted again.
    #[test]
    fn a_second_turn_is_refused_while_one_is_in_flight_and_admitted_after_it_drops() {
        let reg = SessionRegistry::new();
        let s = reg.create(SessionMode::Freeform, None, None).unwrap();
        let first = TurnId::from("turn-1");
        let second = TurnId::from("turn-2");

        let claim = reg.try_begin_turn(&s.session_id, &first).unwrap();
        assert_eq!(claim.turn_id(), &first);
        assert_eq!(claim.session_id(), &s.session_id);

        let refused = reg.try_begin_turn(&s.session_id, &second).unwrap_err();
        assert_eq!(
            refused,
            TurnClaimError::InFlight {
                session_id: s.session_id.clone(),
                turn_id: first.clone(),
            }
        );
        assert!(
            refused.to_string().contains("turn-1"),
            "the refusal must name the in-flight turn, not just report busy: {refused}"
        );

        drop(claim);
        let readmitted = reg
            .try_begin_turn(&s.session_id, &second)
            .expect("a released session must admit the next turn");
        assert_eq!(readmitted.turn_id(), &second);
    }

    /// The claim is per session, like the title claim: one busy session does not
    /// stop the daemon's other sessions from running turns (BR-2).
    #[test]
    fn each_session_is_claimed_independently() {
        let reg = SessionRegistry::new();
        let a = reg.create(SessionMode::Freeform, None, None).unwrap();
        let b = reg.create(SessionMode::Freeform, None, None).unwrap();

        let _a_turn = reg.try_begin_turn(&a.session_id, &TurnId::from("turn-1"));
        assert!(reg
            .try_begin_turn(&b.session_id, &TurnId::from("turn-2"))
            .is_ok());
        assert!(reg
            .try_begin_turn(&a.session_id, &TurnId::from("turn-3"))
            .is_err());
    }

    /// A session the registry never had carries no conversation and grants no
    /// claim — and says which of the two failures it is, rather than handing back
    /// a claim over nothing.
    #[test]
    fn an_unknown_session_carries_no_conversation_and_claims_no_turn() {
        let reg = SessionRegistry::new();
        let ghost = SessionId::from("never-created");

        assert!(reg.conversation_snapshot(&ghost).is_empty());
        reg.commit_conversation(&ghost, retained(vec![user_block("into the void")]));
        assert!(
            reg.conversation_snapshot(&ghost).is_empty(),
            "a commit must not resurrect a session that does not exist"
        );
        assert_eq!(reg.clear_conversation(&ghost), 0);
        assert_eq!(
            reg.try_begin_turn(&ghost, &TurnId::from("turn-1"))
                .unwrap_err(),
            TurnClaimError::NoSuchSession {
                session_id: ghost.clone(),
            }
        );
    }

    /// **Verify (MINOR 2).** The lifecycle sites' store and their claim are one
    /// operation, and the registry is what makes them one.
    ///
    /// # The hazard
    ///
    /// `set_repo_context` then `claim_repo_context_publish` is the same two
    /// writes with the mutex free in between, so two concurrent `/cd`s can
    /// interleave: A stores its state, B stores its state and claims its
    /// triple, A claims *its* triple over B's. The registry then holds B's file
    /// beside A's published record, and the next turn measures its news against
    /// a line no client was sent — the exact thing the record exists to prevent.
    ///
    /// # What is asserted here, and what is asserted next door
    ///
    /// This leg is the **contract**: one call writes both fields, shown by
    /// reading the state back and by a following claim of the same triple being
    /// refused as a duplicate. It is deterministic and it is not the whole
    /// claim — replacing the body with the two calls it replaced keeps it
    /// green, because the two writes still land.
    ///
    /// The atomicity is asserted where it can be: at the **call site**, by
    /// `runtime`'s `the_lifecycle_store_and_its_claim_are_one_registry_call`.
    /// A two-thread, 5,000-iteration race over the two-call form was written,
    /// run, and stayed green — the window is a lock release and re-acquire with
    /// no work in it — so shipping it would have been an assertion that cannot
    /// fail (LESSON-569). The source check next door can, and its mutation was
    /// observed.
    #[test]
    fn a_stored_repo_context_and_the_line_announced_about_it_move_together() {
        use crate::repo_context::RepoContextState;
        use teton_protocol::methods::RepoContextStateKind;

        let reg = SessionRegistry::new();
        let id = reg
            .create(SessionMode::Freeform, None, None)
            .expect("a freeform session needs no phase")
            .session_id;

        // One call, both writes.
        let triple = (RepoContextStateKind::Absent, false, 0);
        assert!(reg.store_and_claim_repo_context(&id, RepoContextState::Absent, triple));
        assert_eq!(*reg.repo_context(&id), RepoContextState::Absent);
        assert!(
            !reg.claim_repo_context_publish(&id, triple, false),
            "the triple was not recorded, so a later identical render would be \
             announced against a line no client saw"
        );

        // A session the registry does not have: nothing stored, nothing to tell.
        let ghost = SessionId::from("no-such-session");
        assert!(!reg.store_and_claim_repo_context(&ghost, RepoContextState::Absent, triple));

        // The pairing under contention is not asserted here — see the doc
        // above for why a race over this window cannot be made to fail on
        // demand, and where the atomicity is pinned instead.
    }

    /// **REQ-613 BR-1: once per session per root, and the root is what makes it
    /// "per root".**
    ///
    /// Three claims, and the middle one is the whole reason the record carries a
    /// root beside the state:
    ///
    /// 1. an armed offer is **claimed once** — the second turn of a session, and
    ///    the second iteration of one turn's tool loop, both read a state that
    ///    is no longer `Pending`;
    /// 2. a decision at **this** root survives a re-derivation of it, which is
    ///    what `/context off` then `/context on` is: two calls that land back on
    ///    `Absent` and would otherwise re-ask a question the user answered;
    /// 3. a **different** root re-arms whatever the last one held, because a
    ///    `/cd` into another project is another project's question.
    ///
    /// **Mutations** (LESSON-441), both run 2026-09-03 and restored: dropping
    /// the `rearmable` guard from `arm_generation` re-arms the decline in leg 2;
    /// dropping the `same_root` comparison leaves leg 3 at `Declined`, so a
    /// session that declined once would never be offered notes in any later
    /// repository.
    #[test]
    fn a_generation_offer_is_claimed_once_and_re_armed_only_by_a_new_root() {
        let reg = SessionRegistry::new();
        let id = reg
            .create(SessionMode::Freeform, None, None)
            .expect("a freeform session needs no phase")
            .session_id;
        let alpha = Path::new("/tmp/alpha");
        let beta = Path::new("/tmp/beta");

        // Fail closed until something derives the notes.
        assert_eq!(reg.generation(&id), GenerationState::Suppressed);
        assert!(!reg.claim_generation(&id), "nothing to claim");

        // (1) Armed, then claimed exactly once.
        assert!(reg.arm_generation(&id, alpha, GenerationState::Pending));
        assert!(reg.claim_generation(&id));
        assert_eq!(reg.generation(&id), GenerationState::Offered);
        assert!(
            !reg.claim_generation(&id),
            "a second turn — or a second tool-loop iteration — must not raise \
             the offer again"
        );

        // (2) The human answered; re-deriving the same root changes nothing.
        assert!(reg.set_generation(&id, GenerationState::Declined));
        assert!(reg.arm_generation(&id, alpha, GenerationState::Pending));
        assert_eq!(
            reg.generation(&id),
            GenerationState::Declined,
            "`/context on` at the root the user declined at must not re-ask"
        );
        assert!(!reg.claim_generation(&id));

        // (3) …and a different project is a different question.
        assert!(reg.arm_generation(&id, beta, GenerationState::Pending));
        assert_eq!(reg.generation(&id), GenerationState::Pending);
        assert!(reg.claim_generation(&id));

        // A suppressed root re-derives freely: nobody was asked anything.
        assert!(reg.set_generation(&id, GenerationState::Suppressed));
        assert!(reg.arm_generation(&id, beta, GenerationState::Pending));
        assert_eq!(reg.generation(&id), GenerationState::Pending);

        let ghost = SessionId::from("no-such-session");
        assert!(!reg.arm_generation(&ghost, alpha, GenerationState::Pending));
        assert!(!reg.set_generation(&ghost, GenerationState::Generated));
        assert!(!reg.claim_generation(&ghost));
        assert_eq!(reg.generation(&ghost), GenerationState::Suppressed);
    }
}
