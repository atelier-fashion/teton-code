//! The generation pipeline (REQ-613 ADR-6): gather → draft → bound → write →
//! load, with one event per stage and a typed failure at every one of them.
//!
//! ## What is here and what is TASK-386's
//!
//! [`run`] is the pipeline *after* consent: it takes a [`ConsentGiven`] witness
//! and does the five acts. Everything in front of it — the config
//! short-circuits, the permission gate, the session record's `GenerationState` —
//! is the caller's, and lives with the session that owns that state. The split is
//! ADR-6's own: one function is what lets the first-turn hook and `/context init`
//! be one code path with two flags, and keeping the *offer* out of it is what
//! lets this half be driven by a test that never builds a `DaemonRuntime`.
//!
//! ## Five acts, and the seam each one already had
//!
//! Nothing here is new machinery. [`evidence::gather`] is the one walk (ADR-3),
//! [`DutyRoute::perform`] is the shared duty seam (ADR-4), [`bound_answer`] is
//! REQ-612's cutter spending REQ-612's cap on the header first,
//! [`write::write_new`]/[`write::replace`] are the no-clobber and `--force` writes
//! (ADR-5), and [`RepoContext::load`] is REQ-612's loader unchanged. What this
//! module owns is the **order**, the **event per stage**, and the rule that a
//! failure at any of them leaves no file — with the one exception [`run`]'s doc
//! names, where removing the file would destroy the one it replaced.
//!
//! ## The cost row is the duty's, and there is exactly one (BR-5)
//!
//! Nothing here writes to the ledger. A remotely-routed duty meters itself at the
//! egress choke point, attributed with its own [`Category::Draft`](teton_protocol::Category::Draft)
//! — that is what routing it through the seam *buys*. A second write here would
//! be a second row for one call, and `/cost` would show a repository's notes
//! costing twice what they cost.
//!
//! ## A failure is facts, not a sentence (LESSON-557)
//!
//! [`run`] answers with a [`Stage`] and a [`Reason`]; the sentence naming the
//! remedy (`/context init`) is composed at the surface that renders it. The one
//! string this module composes is the event's own bounded `reason`, which is
//! broadcast news rather than the user's remedy — and it is bounded and
//! neutralised on the way out, because a failure reason is repository-adjacent
//! content that can carry a path the repository chose.
//!
//! ## Provider health is not touched, anywhere on this path
//!
//! A duty failure — a provider error, a privacy block at the choke point, a
//! window the prompt did not fit — is reported to the call site as a sentence and
//! to nothing else (REQ-561's rule, kept). This module holds no health handle, so
//! BR-9's "the provider's health is unchanged" is a property of what is *absent*
//! here rather than of a branch that remembers not to fire.

use std::io::ErrorKind;
use std::path::{Path, PathBuf};
use std::time::SystemTime;

use teton_core::boundary::BoundaryMatcher;
use teton_core::config::{Config, GenerateMode};
use teton_core::session_root::bounded_field;
use teton_protocol::events::{self, RepoContextGeneration};
use teton_protocol::methods::RepoContextStateKind;
use teton_protocol::Tier;

use crate::grants::ConnectionId;
use crate::harness::digest::tool_result_provenance;
use crate::harness::draft::{bound_answer, build_prompt_from_evidence, DRAFT_OUTPUT_MAX_BYTES};
use crate::harness::duty::DutyRoute;
use crate::harness::permissions::{
    repo_context_generation_key, GenerationConsent, PermissionGate, TrustRoot,
};
use crate::harness::tools::skill::durable_trust_root_name;
use crate::harness::tools::walk::WalkBudget;
use crate::harness::SessionEvents;
use crate::session_root::ProbedRoot;
use crate::transcript::record::rfc3339_utc;

use super::evidence::{self, Evidence, EvidenceBudget, WalkStop};
use super::render::generated_header;
use super::write;
use super::{RepoContext, RepoContextState, RepoFileReader, CANDIDATE_NAMES};

/// How wide the event's `reason` may be, in characters.
///
/// The event's own doc calls the field repository-adjacent content and asks for
/// `bounded_field`; this is the budget it is bounded to. Generous enough for a
/// duty's own sentence (`the 'draft' category resolves to 'frontier', which is
/// not a configured provider.`) and far short of anything a repository could make
/// a monitor's line out of.
const REASON_MAX_CHARS: usize = 200;

/// The human said yes — or said it by asking (`/context init`) — **about this
/// directory**.
///
/// A witness, and its first job is to make [`run`] unreachable from a path that
/// never asked: a function taking `bool` is a function some later caller passes
/// `true` to for convenience, and what would be waved through is a walk of the
/// user's repository followed by a frontier model call.
///
/// It is deliberately *not* proof of a particular gate answer — the gate lives
/// with the session (ADR-2) and this module cannot see it. What
/// [`Self::granted`] asserts is that the caller stood at a place where consent
/// was settled: an accepted prompt, a `full`-level session, `generate = always`,
/// or the user's own `/context init`.
///
/// # It carries the directory, because the question named one
///
/// The consent key is minted from the **canonical** root
/// ([`durable_trust_root_name`]), so a root reached through a symlinked parent
/// is one remembered answer however it was spelled. A write that then used the
/// *spelled* path would put the file somewhere the answer was never given
/// about — two directories for one question — and would defeat the key the
/// moment the two spellings resolve differently. So the witness carries the
/// directory consent was settled for and [`run`] writes there: the fact has one
/// home, and it is the one that minted the key.
#[derive(Debug, Clone, Copy)]
pub struct ConsentGiven<'a>(&'a Path);

impl<'a> ConsentGiven<'a> {
    /// Mint the witness for `target`. Call this only where the answer is
    /// actually in hand, and hand it the directory that answer was about.
    #[must_use]
    pub fn granted(target: &'a Path) -> Self {
        Self(target)
    }

    /// The directory the notes are written into.
    #[must_use]
    pub fn target(self) -> &'a Path {
        self.0
    }
}

/// Which act of the pipeline a failure happened in (BR-9).
///
/// Four, not five: bounding cannot fail — it strips and cuts, and its worst
/// answer is a short file — so a model answer with nothing in it is a `Draft`
/// failure, reported where the answer came from rather than where it was
/// measured.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Stage {
    /// The evidence walk (ADR-3).
    Walk,
    /// The draft duty: routing it, performing it, and having an answer at the
    /// end (ADR-4).
    Draft,
    /// Putting the bytes on disk (ADR-5).
    Write,
    /// Reading them back with REQ-612's loader (BR-7).
    Load,
}

impl Stage {
    /// The stage's name in a sentence a person reads.
    #[must_use]
    pub fn label(self) -> &'static str {
        match self {
            Stage::Walk => "walking the repository",
            Stage::Draft => "drafting the notes",
            Stage::Write => "writing the file",
            Stage::Load => "reading the file back",
        }
    }
}

/// Why a stage failed, as **facts** (LESSON-557).
///
/// Each variant leads a user somewhere different — fix a root, change a policy
/// row, keep the file you already have, look at a permission — which is the whole
/// reason this is not a string. The surface that renders it composes the sentence
/// and names `/context init` as the remedy; nothing here does.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Reason {
    /// The walk produced no usable listing at all: a root that could not be
    /// canonicalized, or a byte budget too small for even the root's own line.
    /// Distinct from a budget *stop*, which is not a failure (BR-9).
    NothingToDraft,
    /// The draft route's context window is too narrow to carry any evidence at
    /// all: [`evidence_budget_for`] reserves the answer and the drafting
    /// instruction out of it first, and what is left is nothing.
    ///
    /// Distinct from [`Self::NothingToDraft`], which is a statement about the
    /// *repository* — and the reason this variant exists: a saturated budget
    /// walks the tree, finds it, cannot afford a byte of it, and would then
    /// report a repository with nothing in it. The remedy is a wider route, not
    /// a different directory.
    WindowTooNarrow {
        /// The window the route was measured against, in bytes.
        window_bytes: usize,
    },
    /// The duty's own sentence, verbatim — an unresolvable binding, a provider
    /// error, a refusal at the egress choke point, a prompt the route's window
    /// would not take, or the seam's deadline.
    ///
    /// A string because it *is* the resolver's or the seam's own words and this
    /// module mints no explanation of its own (the duty seam's rule). It is
    /// already broadcast-safe: never the model's output and never the content.
    Duty(String),
    /// The model answered and nothing but whitespace survived stripping — a
    /// file that would be a header and nothing else.
    EmptyDraft,
    /// A `TETON.md` is already there and `force` was not set (BR-6's no-clobber
    /// write, and AC-8's race between consent and write).
    AlreadyExists,
    /// The entry at the path is a symlink, which this build will not follow.
    Symlink,
    /// Anything else the filesystem said, by kind.
    Io(ErrorKind),
    /// The bytes were written whole and REQ-612's loader would not make them
    /// resident (BR-7). Carries what the loader answered instead, and what
    /// became of the file.
    NotLoaded {
        /// What the loader answered instead of `Loaded`.
        kind: RepoContextStateKind,
        /// Whether the bytes are still on disk.
        ///
        /// **The two doors part here, and only here.** A `write_new` that is
        /// then refused by the loader is unlinked: nothing was there before, so
        /// removing it restores the directory the run found. A `replace` has
        /// already renamed over the file it was given permission to replace —
        /// the old bytes are gone the instant the rename returns — so unlinking
        /// would leave the user with *neither* file, which is the one outcome
        /// `--force` cannot mean. It is carried rather than worded at the
        /// detection point because the surface that renders this is the surface
        /// that has to tell the user which of the two happened.
        left_in_place: bool,
    },
}

impl Reason {
    /// The event's `reason` line: this build's own words for the fact, short
    /// enough to render beside a stage name.
    ///
    /// The broadcast half, not the user's remedy. The remedy sentence belongs to
    /// the surface that has the typed [`Reason`] in hand.
    #[must_use]
    pub(crate) fn as_news(&self) -> String {
        match self {
            Reason::NothingToDraft => "the walk found nothing to draft from".to_owned(),
            Reason::WindowTooNarrow { window_bytes } => format!(
                "the draft route's window ({window_bytes} bytes) is too narrow to carry any \
                 evidence"
            ),
            Reason::Duty(sentence) => sentence.clone(),
            Reason::EmptyDraft => "the draft came back empty".to_owned(),
            Reason::AlreadyExists => "a TETON.md is already there".to_owned(),
            Reason::Symlink => "the path is a symlink".to_owned(),
            Reason::Io(kind) => format!("the write failed: {kind}"),
            // The second clause is the whole of BR-9's exception: every other
            // failure leaves no file, and a user owed that promise has to be
            // told the one case where it does not hold — the replacement is
            // there, unread, and `--force` will not un-replace what it replaced.
            Reason::NotLoaded {
                kind,
                left_in_place: false,
            } => format!(
                "the file was written and read back as {}",
                state_word(*kind)
            ),
            Reason::NotLoaded {
                kind,
                left_in_place: true,
            } => format!(
                "the replacement was written and read back as {}; it is left in place",
                state_word(*kind)
            ),
        }
    }
}

/// A [`RepoContextStateKind`]'s word in a sentence, without going through serde
/// for one noun.
fn state_word(kind: RepoContextStateKind) -> &'static str {
    match kind {
        RepoContextStateKind::Loaded => "loaded",
        RepoContextStateKind::Truncated => "truncated",
        RepoContextStateKind::Absent => "absent",
        RepoContextStateKind::WithheldBoundary => "withheld by a boundary",
        RepoContextStateKind::WithheldOff => "withheld by the switch",
        RepoContextStateKind::Unreadable => "unreadable",
    }
}

/// A `TETON.md` this run produced, and everything the caller has to store or say
/// about it.
#[derive(Debug, Clone)]
pub struct Generated {
    /// Where it was written — the writer's own spelling, never a second join.
    pub path: PathBuf,
    /// What REQ-612's loader made of it, the same run (BR-7). Always a
    /// [`RepoContextState::Loaded`]; anything else is a
    /// [`GenerationOutcome::Failed`] with the file removed.
    pub state: RepoContextState,
    /// Bytes on disk, header included.
    pub bytes: usize,
    /// The bounded draft's own bytes, header excluded — the figure the `drafted`
    /// event carries.
    pub draft_bytes: usize,
    /// The tier that served the draft (BR-5), as the header states it.
    pub tier: Tier,
    /// Filesystem entries the walk visited.
    pub entries: usize,
    /// Evidence files a boundary covered and the gatherer dropped before the
    /// call (BR-4).
    pub excluded: usize,
}

/// What one run of the pipeline did (ADR-6).
///
/// The daemon's own typed answer, and deliberately not
/// [`events::GenerationOutcome`], which is the ten-value *word* the wire carries:
/// this one has to hold the facts a failure is made of, and that enum is `Copy`
/// and fieldless by design. [`Self::wire`] maps one to the other, in one place,
/// so the news and the answer cannot come to disagree about what a run did.
#[derive(Debug, Clone)]
pub enum GenerationOutcome {
    /// A file was created where none existed.
    Written(Generated),
    /// An existing file was replaced — only `force` reaches this (BR-8).
    Replaced(Generated),
    /// A stage failed. **No file was left behind** (BR-9), with the one
    /// exception [`run`]'s doc names: a `force` run whose replacement would not
    /// load back leaves the replacement, because the file it replaced is
    /// already gone.
    Failed {
        /// Which act it failed in.
        stage: Stage,
        /// What went wrong, as facts.
        reason: Reason,
    },
}

impl GenerationOutcome {
    /// The wire word for this outcome.
    #[must_use]
    pub fn wire(&self) -> events::GenerationOutcome {
        match self {
            GenerationOutcome::Written(_) => events::GenerationOutcome::Written,
            GenerationOutcome::Replaced(_) => events::GenerationOutcome::Replaced,
            GenerationOutcome::Failed { .. } => events::GenerationOutcome::Failed,
        }
    }

    /// The file this run produced, or `None` for a failure.
    #[must_use]
    pub fn generated(&self) -> Option<&Generated> {
        match self {
            GenerationOutcome::Written(made) | GenerationOutcome::Replaced(made) => Some(made),
            GenerationOutcome::Failed { .. } => None,
        }
    }
}

/// Everything [`run`] needs that it cannot derive, gathered at the three call
/// sites that have it (ADR-6).
///
/// A wrapper rather than nine arguments, for the reason the turn path's own
/// context types exist: the first-turn hook, `/context init` and
/// `/context init --force` differ in exactly two flags, and a nine-argument call
/// repeated three times is three chances for two of them to differ in a tenth
/// thing nobody notices.
///
/// **The session id is not a field.** [`SessionEvents`] already carries the
/// session these events are attributed to, and a second copy here would be a
/// second spelling of one identity for the five publishing sites to drift on —
/// the same reason `SessionEvents` owns the attribution in the first place.
/// The draft route's window, and what the evidence body may spend of it
/// (ADR-3).
///
/// **Both halves, because the subtraction can take the whole of it.** The
/// evidence share is the window less the answer and the drafting instruction,
/// and on a route narrower than that reserve it saturates to zero — at which
/// point the share alone can no longer say what happened. A body of zero bytes
/// and a window of 8,000 bytes are two different facts with two different
/// remedies, and [`Reason::WindowTooNarrow`] can only tell them apart if the
/// window travelled with the share.
///
/// Not a second field on [`EvidenceBudget`]: that type is the *assembly's*
/// bound and is spent by the gatherer, which has no business knowing what the
/// route's window was.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DraftBudget {
    /// The route's whole context budget, in bytes, as the router answered it.
    pub window_bytes: usize,
    /// What the evidence body may spend of it.
    pub evidence: EvidenceBudget,
}

pub struct GenerationContext<'a> {
    /// The session's probed root: one probe, so the jail the walk runs under, the
    /// directory the file is written into and the root the loader reads at are
    /// one directory spelled once.
    pub root: &'a ProbedRoot,
    /// REQ-612's file-reader seam, shared by the gatherer and the loader.
    ///
    /// `Send + Sync` because this context is held across the gate's `await` and
    /// the duty's, inside a task the connection spawns — the trait object's own
    /// bounds are what the compiler checks there, and the runtime's seam
    /// (`Arc<dyn RepoFileReader + Send + Sync>`) already satisfies them.
    pub reader: &'a (dyn RepoFileReader + Send + Sync),
    /// The session's compiled boundary set — what the gatherer excludes by and
    /// what the loader withholds by.
    pub boundaries: &'a BoundaryMatcher<'a>,
    /// The draft route's window and the evidence body's share of it, derived by
    /// the caller (ADR-3: only the caller knows the route).
    pub budget: DraftBudget,
    /// The walk's own budget — REQ-583's type, passed through.
    ///
    /// Not a second home for the bound (ADR-3's objection was to putting
    /// `max_entries` on [`EvidenceBudget`], where a walk's cost would acquire a
    /// second owner): production hands [`WalkBudget::default`] and the numbers
    /// stay REQ-583's. It rides here for
    /// [`ToolContext::with_walk_budget`](crate::harness::tools::ToolContext::with_walk_budget)'s
    /// reason, one layer up — BR-9's "a budget stop with a usable tree is not a
    /// failure" has to be provable, and proving it through the default would mean
    /// planting a hundred thousand files.
    pub walk: WalkBudget,
    /// The draft route, resolved on demand.
    ///
    /// A resolver and not a resolved route, so that **nothing is routed until the
    /// evidence is in hand**: a run that fails at the walk announces no
    /// `route_decided` for a duty that never ran.
    /// `Send + Sync` for [`Self::reader`]'s reason: the closure is borrowed
    /// across an `await` on a spawned task.
    pub route: &'a (dyn Fn() -> DutyRoute + Send + Sync),
    /// Where every stage event goes, and whose session id they are attributed to.
    pub events: &'a SessionEvents,
    /// The tier serving the draft, for the header line and the event (BR-5).
    ///
    /// The caller's, because the caller is what resolved the route: deriving it
    /// here would mean asking the router a second time and getting a second
    /// answer if a policy row moved in between.
    pub tier: Tier,
    /// The session's config snapshot. Read for `[context] repo_file`, the switch
    /// the loader obeys — a session with the notes switched off may still be
    /// asked to write them, and the state it gets back has to say so rather than
    /// pretend the file loaded.
    pub config: &'a Config,
}

/// Gather, draft, bound, write and load, publishing a stage event at each
/// (ADR-6, BR-4, BR-5, BR-7, BR-9).
///
/// `force` chooses the door: [`write::replace`]'s atomic rename over an existing
/// file, or [`write::write_new`]'s create-new refusal. Without it a `TETON.md`
/// that is there — planted before the run, or created between consent and the
/// write — is [`Reason::AlreadyExists`] and nothing on disk changes.
///
/// # A failure leaves no file, with one named exception
///
/// Every stage that fails ends with the directory as this run found it — except
/// a load-back refusal on the `force` door, where the replacement has already
/// landed on top of the file it replaced and removing it would destroy both
/// (see [`Reason::NotLoaded`]'s `left_in_place`).
///
/// A stop the walk's budget imposed is **not** a failure at all: a partial
/// listing is still a listing, it is written into the tree the model is shown
/// and into the header the reader meets, and refusing to draft from it would
/// turn a large repository into a repository that can never have notes (BR-9).
pub async fn run(
    ctx: GenerationContext<'_>,
    consent: ConsentGiven<'_>,
    force: bool,
) -> GenerationOutcome {
    // The witness asserts that this call happened at all, and names the
    // directory the answer was given about — which is where the bytes go.
    let target = consent.target();

    // --- walking ---------------------------------------------------------
    publish(
        &ctx,
        events::GenerationOutcome::Walking,
        &Progress::default(),
    );
    // Before the walk, because a route that cannot carry a byte of evidence
    // makes the walk pointless and its answer misleading: the assembly would
    // spend a saturated budget, hand back an empty body, and the run would
    // report a repository with nothing in it (BR-9's `NothingToDraft` is a fact
    // about the tree). Published under `Stage::Walk` all the same — it is the
    // walk's own budget that is missing.
    if ctx.budget.evidence.max_bytes == 0 {
        return fail(
            &ctx,
            &Progress::default(),
            Stage::Walk,
            Reason::WindowTooNarrow {
                window_bytes: ctx.budget.window_bytes,
            },
        );
    }
    // BUG-184's rule: the gatherer is synchronous and unbounded in wall-clock
    // terms up to REQ-583's budget — up to 100,000 entries or 10 seconds of
    // `read_dir`, `stat` and read on user-controlled paths, and on macOS a root
    // under `~/Documents` can raise a TCC dialog that blocks the syscall for as
    // long as the user takes to answer it. This runs on the connection's
    // spawned task, so the wait must not be taken on an async worker — the same
    // wrapping `load_repo_context` takes one act earlier, for the same syscalls.
    let evidence = crate::runtime::block_in_place_if_multithread(|| {
        evidence::gather_with_walk_budget(
            ctx.root,
            ctx.reader,
            ctx.boundaries,
            ctx.budget.evidence,
            ctx.walk,
        )
    });
    let walked = Progress {
        entries: Some(evidence.entries),
        excluded: Some(evidence.excluded),
        ..Progress::default()
    };
    if evidence.body.trim().is_empty() {
        return fail(&ctx, &walked, Stage::Walk, Reason::NothingToDraft);
    }

    // --- drafting --------------------------------------------------------
    //
    // The provenance is the evidence's own `Sources`, bridged by the one function
    // that bridges a harness provenance to the choke point's (BR-4). Never
    // `Unknown`: the gatherer mints an identity for every file it read, and a
    // covered one was dropped before it was `stat`ed.
    let prompt = build_prompt_from_evidence(&evidence);
    let provenance = tool_result_provenance(&evidence.provenance);
    let answer = match (ctx.route)().perform(&prompt, &provenance).await {
        Ok(answer) => answer,
        Err(reason) => return fail(&ctx, &walked, Stage::Draft, Reason::Duty(reason)),
    };

    // --- bounding --------------------------------------------------------
    //
    // The header first and charged to the cap, so "the file is at most 8,192
    // bytes" is a statement about the file rather than about the model's share of
    // it (ADR-5, AC-8).
    let header = generated_header(
        ctx.tier.as_str(),
        &today(),
        stop_phrase(evidence.stop).as_deref(),
        cut_phrase(&evidence).as_deref(),
    );
    let body = bound_answer(&answer, &header);
    // The model's own share, measured past the header this build put in front of
    // it. `trim`, not `is_empty`: an answer of three spaces and a newline
    // survives stripping and is not notes — and the file it would produce is a
    // header alone, which REQ-612 would load and put in every later prompt.
    let draft = &body[header_prefix_len(&header).min(body.len())..];
    if draft.trim().is_empty() {
        return fail(&ctx, &walked, Stage::Draft, Reason::EmptyDraft);
    }
    let draft_bytes = draft.len();
    let drafted = Progress {
        draft_bytes: Some(draft_bytes),
        ..walked
    };
    publish(&ctx, events::GenerationOutcome::Drafted, &drafted);

    // --- writing ---------------------------------------------------------
    //
    // Into the directory consent was settled for, never the session's own
    // spelling of it: the two are the same directory reached two ways, and the
    // permission key was minted from this one.
    let written = if force {
        write::replace(target, &body)
    } else {
        write::write_new(target, &body)
    };
    let written = match written {
        Ok(written) => written,
        Err(failure) => return fail(&ctx, &drafted, Stage::Write, reason_for(failure)),
    };

    // --- loading ---------------------------------------------------------
    //
    // REQ-612's loader on the file just written, same run, same rules an authored
    // file is read by (BR-7). Anything short of `Loaded` is a failure, and on the
    // no-clobber door an unlink with it: a file the loader will not make resident
    // is a file the next session meets as if the repository had written it, and
    // nothing was there before this run to restore.
    //
    // **`--force` is the exception, and it is not a softening of BR-9.**
    // `write::replace` renames over the old file, so by the time the loader
    // answers there is exactly one file at the path and the bytes it displaced
    // are gone. Unlinking here would leave the user with neither their file nor
    // its replacement — destroying, on a *read* failure, the very thing they
    // gave permission to replace. So the replacement stays and the reason says
    // so.
    let state = RepoContext::load(
        ctx.root,
        ctx.boundaries,
        ctx.config.context.repo_file,
        ctx.reader,
    );
    if !matches!(state, RepoContextState::Loaded(_)) {
        if !force {
            write::remove(&written.path);
        }
        let kind = state.kind();
        return fail(
            &ctx,
            &drafted,
            Stage::Load,
            Reason::NotLoaded {
                kind,
                left_in_place: force,
            },
        );
    }

    let made = Generated {
        path: written.path,
        state,
        bytes: written.bytes,
        draft_bytes,
        tier: ctx.tier,
        entries: evidence.entries,
        excluded: evidence.excluded,
    };
    let outcome = if force {
        GenerationOutcome::Replaced(made)
    } else {
        GenerationOutcome::Written(made)
    };
    publish(&ctx, outcome.wire(), &drafted);
    outcome
}

/// What is known about the run so far, in the four measurements the event
/// carries.
///
/// Every one is an [`Option`] for the event's own reason: most stages are reached
/// *before* the figure exists, and a `0` would be a measurement — "the walk found
/// nothing" — where "nothing has been measured" is the truth.
#[derive(Debug, Clone, Copy, Default)]
struct Progress {
    entries: Option<usize>,
    excluded: Option<usize>,
    draft_bytes: Option<usize>,
}

/// Publish one stage.
fn publish(ctx: &GenerationContext<'_>, outcome: events::GenerationOutcome, progress: &Progress) {
    emit(ctx, outcome, progress, None);
}

/// Publish a `failed` stage and answer with the typed failure — the one exit
/// that must never leave the news and the answer disagreeing about what happened.
fn fail(
    ctx: &GenerationContext<'_>,
    progress: &Progress,
    stage: Stage,
    reason: Reason,
) -> GenerationOutcome {
    emit(
        ctx,
        events::GenerationOutcome::Failed,
        progress,
        Some(&reason),
    );
    GenerationOutcome::Failed { stage, reason }
}

/// The one place a [`RepoContextGeneration`] is built.
///
/// The root's *display* — home-relative and already bounded by the probe, the
/// same spelling the offer showed the human — never the absolute path a monitor
/// has no business learning.
///
/// `tier` is present from the **first** stage, unlike the three measurements
/// beside it, and that is not an oversight: the event's contract is "once one has
/// been chosen", and it was — the caller resolved the draft route before this
/// function ran, which is how it derived the evidence budget. The measurements
/// are absent early because nothing has measured them yet; the tier is not in
/// that position.
fn emit(
    ctx: &GenerationContext<'_>,
    outcome: events::GenerationOutcome,
    progress: &Progress,
    reason: Option<&Reason>,
) {
    emit_news(
        ctx,
        outcome,
        progress,
        reason.map(Reason::as_news).as_deref(),
    );
}

/// [`emit`] for a stage whose words are not a [`Reason`]'s — the offer's own
/// three (ADR-2's short-circuits), which happen in front of the pipeline and so
/// in front of anything a stage could fail at.
fn emit_reason(
    ctx: &GenerationContext<'_>,
    outcome: events::GenerationOutcome,
    progress: &Progress,
    reason: &str,
) {
    emit_news(ctx, outcome, progress, Some(reason));
}

/// The one construction, bounded and neutralised on the way out for the reason
/// the event's own doc gives: a reason is repository-adjacent content and can
/// carry a path the repository chose.
fn emit_news(
    ctx: &GenerationContext<'_>,
    outcome: events::GenerationOutcome,
    progress: &Progress,
    news: Option<&str>,
) {
    ctx.events.repo_context_generation(RepoContextGeneration {
        outcome,
        root: ctx.root.view.display.clone(),
        entries: progress.entries.map(|entries| entries as u64),
        excluded: progress.excluded.map(|excluded| excluded as u32),
        draft_bytes: progress.draft_bytes.map(|bytes| bytes as u64),
        tier: Some(ctx.tier),
        reason: news.map(|news| bounded_field(news, REASON_MAX_CHARS)),
    });
}

/// This module's named answer for a [`write::WriteFailure`].
///
/// A translation and not a re-derivation: the writer already decided what the
/// filesystem said, and a second classification here would be a second opinion
/// about one `errno`.
fn reason_for(failure: write::WriteFailure) -> Reason {
    match failure {
        write::WriteFailure::AlreadyExists => Reason::AlreadyExists,
        write::WriteFailure::Symlink => Reason::Symlink,
        write::WriteFailure::Io(kind) => Reason::Io(kind),
    }
}

/// The header's share of [`bound_answer`]'s output, so the `drafted` event
/// reports the *model's* bytes.
///
/// Spelled from the same rule `bound_answer` applies — the header, plus the
/// newline it is given when it has none — because the event's contract is that
/// `draft_bytes` excludes this build's own line.
fn header_prefix_len(header: &str) -> usize {
    if header.is_empty() || header.ends_with('\n') {
        header.len()
    } else {
        header.len() + 1
    }
}

/// The walk's stop as the header states it, or `None` when it did not stop.
fn stop_phrase(stop: Option<WalkStop>) -> Option<String> {
    match stop? {
        WalkStop::Entries(entries) => Some(format!("walk stopped at {entries} entries")),
        WalkStop::WallClock(elapsed) => {
            Some(format!("walk stopped after {:.1} s", elapsed.as_secs_f64()))
        }
    }
}

/// The assembly's cut as the header states it, or `None` when nothing was cut.
///
/// A tree cut names its depth, which is ADR-5's own example line; every other
/// class is dropped whole and names itself.
fn cut_phrase(evidence: &Evidence) -> Option<String> {
    let cut = evidence.cut?;
    Some(match cut.depth {
        Some(depth) => format!("{} cut at depth {depth}", cut.class.label()),
        None => format!("{} left out", cut.class.label()),
    })
}

// ===========================================================================
// The offer in front of the pipeline (ADR-1, ADR-2, ADR-6)
// ===========================================================================

/// What the evidence body may spend of the draft route's window, given the
/// window in bytes (ADR-3).
///
/// The route's budget less the two things the same request has to carry: this
/// build's own drafting instruction, and room for the answer — which is
/// [`DRAFT_OUTPUT_MAX_BYTES`] exactly, because the answer *is* the file and the
/// file is capped there.
///
/// Read off a budget the caller already holds and never re-derived
/// (`Router::budget_for`'s rule): the caller resolves the draft route once and
/// asks the router for that provider's pair, so the evidence and the call that
/// spends it are measured against one window.
///
/// The window rides back beside the share it produced: a route narrower than
/// [`DRAFT_RESERVED_BYTES`] leaves nothing, and the share alone cannot then say
/// whether the repository was empty or the route was ([`Reason::WindowTooNarrow`]).
#[must_use]
pub fn evidence_budget_for(window_bytes: usize) -> DraftBudget {
    DraftBudget {
        window_bytes,
        evidence: EvidenceBudget::new(window_bytes.saturating_sub(DRAFT_RESERVED_BYTES)),
    }
}

/// What the request has to carry before a byte of evidence is added: the
/// answer's reservation and the drafting instruction.
///
/// Named rather than spelled twice, because it is the threshold
/// [`Reason::WindowTooNarrow`] is about — a window at or under this leaves the
/// evidence body nothing at all.
pub const DRAFT_RESERVED_BYTES: usize = DRAFT_OUTPUT_MAX_BYTES + DRAFT_PROMPT_BYTES;

/// What the drafting instruction costs before a byte of evidence is added.
///
/// [`build_prompt`](crate::harness::draft::build_prompt)'s fixed text is about
/// 1,300 bytes, plus the per-member headings it writes around the evidence and
/// whatever chat template the route wraps it in. Rounded generously upward: the
/// cost of over-reserving is a slightly shorter tree, and the cost of
/// under-reserving is a request the window refuses.
const DRAFT_PROMPT_BYTES: usize = 4_096;

/// The notes file already at `root` — its name and its size — or `None` when
/// neither of REQ-612's two candidate names is there (BR-1, BR-6).
///
/// **`RepoContextState` cannot answer this, and that is the reason this
/// exists.** An empty or whitespace-only `TETON.md` loads as
/// [`RepoContextState::Absent`] by REQ-612's own rule, and BR-1 counts exactly
/// that file as *present* — it is the documented way to stop the offer. A
/// caller that armed the offer on `Absent` alone would prompt the user to write
/// a file over the empty one they left there on purpose.
///
/// Any answer but "not found" counts as present, symlinks and directories
/// included: this decides whether Teton may write, and the only safe reading of
/// a `stat` that would not answer is that something is there.
///
/// Goes through the injected [`RepoFileReader`], so a switched-off session's
/// "off means unopened" stays a property of the code — the caller checks the
/// switch before it asks.
/// The size is an [`Option`] for the event's own reason: a `stat` that failed
/// answered *something is there* and nothing about how big it is, and a `0`
/// beside a name the user can `ls` would be a measurement — "an empty file is
/// in your way" — where "the size is not known" is the truth.
#[must_use]
pub fn notes_present(
    root: &ProbedRoot,
    reader: &dyn RepoFileReader,
) -> Option<(&'static str, Option<u64>)> {
    CANDIDATE_NAMES.iter().find_map(|name| {
        match reader.stat(&root.path.join(name)) {
            Ok(key) => Some((*name, Some(key.len))),
            Err(super::RepoFileError::NotFound) => None,
            // A `stat` that failed for any other reason answered *something* is
            // there. Fail closed: refusing to write is recoverable, clobbering
            // is not — and the size is unknown rather than zero.
            Err(_) => Some((*name, None)),
        }
    })
}

/// Why an offer was never put to anybody (ADR-2).
///
/// Facts rather than a sentence, for [`Reason`]'s reason — each of these sends
/// the user somewhere different: a config key, a permission level, a root that
/// does not resolve.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Suppression {
    /// `[context] generate = never`. Never reached by `/context init`, which is
    /// the user's explicit act and outranks the setting (BR-8).
    Never,
    /// `[context] repo_file = false` — the durable switch REQ-612's loader
    /// obeys.
    ///
    /// **Reached by `/context init` too**, unlike [`Self::Never`], and the
    /// difference is which question the setting answers. `generate` is a setting
    /// about the *offer*, so a user who typed the command has said the thing it
    /// exists to stop Teton assuming. `repo_file = false` says the file may not
    /// be **opened** on this machine, and the pipeline reads back what it writes
    /// (BR-7): a run under it walks the tree, spends a frontier model call,
    /// writes the file, is refused by its own loader and unlinks it. Refusing in
    /// front is the same answer for none of the cost.
    SwitchedOff,
    /// A notes file is already at the root and `force` was not set — the
    /// no-clobber rule answered by a `stat`, in front of the walk (BR-6).
    ///
    /// **A suppression and not a [`GenerationOutcome::Failed`]**, though the two
    /// carried the same sentence once: nothing ran. No tree was walked, no model
    /// was called, nothing was written and nothing failed — and the session
    /// record agrees, because `Suppressed` is what the arming already stores for
    /// this exact fact. The genuine write-time race keeps
    /// [`Reason::AlreadyExists`], which is a failure: that one drafted first.
    AlreadyPresent {
        /// The name found — an `AGENTS.md` stops this run as surely as a
        /// `TETON.md`, and naming the one this build would have written sends
        /// the user to the wrong file.
        name: &'static str,
        /// Its size, when the `stat` answered ([`notes_present`]).
        bytes: Option<u64>,
    },
    /// The level forbids the write, so **no prompt was drawn** (`plan`).
    /// Decided in front of the gate on LESSON-524's rule inverted — do not ask
    /// what you will deny — and carrying the gate's own denial sentence, so the
    /// daemon's and the client's account of one refusal cannot drift.
    DeniedLevel(String),
    /// The session root has no canonical name, so there is no key to ask or
    /// remember an answer under (REQ-591 BR-6's fail-closed rule, one question
    /// over).
    NoDurableRoot,
    /// The session's privacy boundaries do not compile, so nothing the walk
    /// gathered could be judged against them. Unreachable from a shipped path —
    /// `Config::validate` refuses an invalid glob at load and at `config/set` —
    /// and fail-closed regardless: an unjudgeable set reads nothing and sends
    /// nothing.
    Unconfigured,
}

/// What one **offer** did — the pipeline's outcome, or the reason there was no
/// pipeline (ADR-6).
///
/// A wrapper around [`GenerationOutcome`] rather than four more variants on it:
/// that enum answers "what did the run do", and every arm of it has walked a
/// tree. These three say a run never started, which is a different question and
/// the one the session record stores.
#[derive(Debug, Clone)]
pub enum OfferOutcome {
    /// Nobody was asked and nothing ran.
    Suppressed(Suppression),
    /// A **human** said no. Session-scoped, written nowhere (BR-1).
    Declined,
    /// Nobody could be asked: a client that refuses without reading a line, one
    /// that does not know the subject, or no addressable connection at all
    /// (BR-2's unattended rule).
    RefusedUnattended,
    /// Consent was in hand and [`run`] ran.
    Ran(GenerationOutcome),
}

impl OfferOutcome {
    /// The wire word for this outcome — the one mapping, so the news and the
    /// answer cannot come to disagree about what an offer did.
    #[must_use]
    pub fn wire(&self) -> events::GenerationOutcome {
        match self {
            OfferOutcome::Suppressed(Suppression::DeniedLevel(_)) => {
                events::GenerationOutcome::DeniedLevel
            }
            OfferOutcome::Suppressed(_) => events::GenerationOutcome::Suppressed,
            OfferOutcome::Declined => events::GenerationOutcome::Declined,
            OfferOutcome::RefusedUnattended => events::GenerationOutcome::RefusedUnattended,
            OfferOutcome::Ran(outcome) => outcome.wire(),
        }
    }

    /// The file this offer produced, or `None` when nothing was written.
    #[must_use]
    pub fn generated(&self) -> Option<&Generated> {
        match self {
            OfferOutcome::Ran(outcome) => outcome.generated(),
            _ => None,
        }
    }
}

/// Everything the offer needs that [`run`] does not (ADR-2).
///
/// The pipeline's context plus the four facts that decide whether it runs at
/// all: the gate, who may answer it, what the config says, and which door this
/// is.
pub struct Offer<'a> {
    /// What [`run`] will be given once consent is settled.
    pub ctx: GenerationContext<'a>,
    /// The session's gate — the same one every other consent this session gives
    /// goes through, so an answer here expires with the root like the others
    /// (ADR-2).
    pub gate: &'a PermissionGate,
    /// The connection that may answer, when there is one. `None` is a session
    /// nobody can be asked through — an internal driver or a fixture — and is
    /// spelled with the gate's own word rather than a second one.
    pub addressee: Option<ConnectionId>,
    /// `[context] generate`, read from the caller's config snapshot.
    pub mode: GenerateMode,
    /// Whether the user asked for this by name (`/context init`).
    ///
    /// One flag, one effect: `never` is a setting about the *offer*, and a user
    /// who typed the command has said the thing the setting exists to stop
    /// Teton assuming (BR-8). It relaxes nothing else — `plan` still refuses,
    /// and the gate is still asked.
    pub explicit: bool,
    /// Whether an existing file may be replaced (`--force`, BR-8). Rides the
    /// subject, so the human sees which question is on screen.
    pub force: bool,
}

/// Decide whether Teton may write this repository's missing notes, and — if so
/// — write them (ADR-1, ADR-2, ADR-6, BR-1, BR-2, BR-8).
///
/// The **one** path both doors take. The first-turn hook and `/context init`
/// differ in [`Offer::explicit`] and [`Offer::force`] and in nothing else, which
/// is what makes AC-8's "the same bytes come out of both doors" true by
/// construction rather than by test.
///
/// # Five things are settled before the gate, and each for its own reason
///
/// 1. **`generate = never`** — the config's answer, and asking a human a
///    question their config already answered is the offer this feature is most
///    likely to be uninstalled over. `/context init` skips this one.
/// 2. **`repo_file = false`** — the durable switch, and the one short-circuit
///    `/context init` does *not* skip: the pipeline reads back what it writes
///    (BR-7) with this very key, so a run under it can only end by unlinking
///    the file it paid a model call for ([`Suppression::SwitchedOff`]).
/// 3. **A root with no canonical name** — no key, so no answer could be
///    remembered against the directory it was given about (LESSON-495). Fail
///    closed rather than proceed under a name that names nothing.
/// 4. **A file already there** — the no-clobber rule reaching the offer, so a
///    file that appeared between the arming and this turn costs a `stat` rather
///    than a walk, a frontier model call and a refused write. The write is
///    still where the race is decided ([`write::write_new`]); this is the easy
///    case, taken early — and it is a *suppression*, because nothing ran.
/// 5. **`plan`** — LESSON-524 inverted: do not draw a prompt for an act the
///    level will refuse. The gate would answer [`GenerationConsent::Denied`] to
///    a caller that skipped this, so the two cannot disagree; what the
///    short-circuit buys is that no human is shown a question with no yes in it.
///
/// `generate = always` is settled there too, in the other direction: it
/// *answers* the question the prompt would ask, at every level but `plan`
/// (BR-2), so the gate is not consulted at all and the event says the config
/// answered rather than a person.
pub async fn offer_and_run(offer: Offer<'_>) -> OfferOutcome {
    let Offer {
        ctx,
        gate,
        addressee,
        mode,
        explicit,
        force,
    } = offer;

    // --- the config's own answer (BR-2, BR-8) ------------------------------
    if mode == GenerateMode::Never && !explicit {
        return suppressed(&ctx, Suppression::Never);
    }

    // --- the durable switch (REQ-612 BR-2, BR-7) ---------------------------
    //
    // Read here rather than at the load-back, which is the only place it used
    // to be read: `run` loads with this same key, so a machine with the notes
    // switched off would walk the tree, spend a frontier model call, write the
    // file, be refused by its own loader and unlink it — paying for a file it
    // was never going to keep. `explicit` does not relax it, for the reason
    // `Suppression::SwitchedOff` gives.
    if !ctx.config.context.repo_file {
        return suppressed(&ctx, Suppression::SwitchedOff);
    }

    // --- a root that will not canonicalise (ADR-2, REQ-591 BR-6) -----------
    //
    // Resolved here, at the moment the write would happen, which is the one
    // moment it is a statement about the directory being written into. The
    // display is the probe's — the same spelling the event carries and the
    // prompt shows — so the human and the monitor see one name for one root
    // while the key is minted from the other.
    //
    // The resolved path is kept, not just the name minted from it: it is the
    // directory the question is asked about, so it is the directory the answer
    // is honoured in (see [`ConsentGiven`]).
    let resolved = std::fs::canonicalize(&ctx.root.path).ok();
    let durable = resolved.as_deref().map(durable_trust_root_name);
    let trust = TrustRoot {
        display: &ctx.root.view.display,
        durable: durable.as_deref(),
    };
    let (Some(key), Some(target)) = (repo_context_generation_key(trust), resolved.as_deref())
    else {
        return suppressed(&ctx, Suppression::NoDurableRoot);
    };

    // --- a file that is already there (BR-6, AC-8's easy half) -------------
    if !force {
        if let Some((name, bytes)) = notes_present(ctx.root, ctx.reader) {
            return suppressed(&ctx, Suppression::AlreadyPresent { name, bytes });
        }
    }

    // --- the level (ADR-2, LESSON-524 inverted) ----------------------------
    if let Some(note) = gate.repo_context_generation_denial_note(&key) {
        return suppressed(&ctx, Suppression::DeniedLevel(note));
    }

    // --- the human, or the config standing in for one ----------------------
    if mode == GenerateMode::Always {
        // BR-2's own words: `always` answers the question the prompt would ask,
        // at every level but `plan` — which the denial note above has already
        // taken. So the gate is not consulted, including at `full`, where its
        // answer would be the same one; and the news says the config answered,
        // because a user reading a written file they were never asked about is
        // owed the setting's name.
        emit_reason(
            &ctx,
            events::GenerationOutcome::Offered,
            &Progress::default(),
            "[context] generate = always",
        );
    } else {
        publish(
            &ctx,
            events::GenerationOutcome::Offered,
            &Progress::default(),
        );
        let consent = match addressee {
            Some(connection) => {
                gate.authorize_repo_context_generation(&key, trust, force, connection)
                    .await
            }
            // No addressable connection — an internal driver, or a fixture. The
            // question cannot be *put* to anyone, which is the gate's own
            // fail-closed answer, so it is spelled with the gate's word rather
            // than with a second one (`accept_invocation` says the same thing at
            // its own door).
            None => GenerationConsent::RefusedUnattended,
        };
        match consent {
            GenerationConsent::Allowed { .. } => {}
            GenerationConsent::Declined => {
                publish(
                    &ctx,
                    events::GenerationOutcome::Declined,
                    &Progress::default(),
                );
                return OfferOutcome::Declined;
            }
            GenerationConsent::RefusedUnattended => {
                publish(
                    &ctx,
                    events::GenerationOutcome::RefusedUnattended,
                    &Progress::default(),
                );
                return OfferOutcome::RefusedUnattended;
            }
            // The level, answered at the gate rather than in front of it. Not
            // reachable through the note above, and kept because a gate and a
            // short-circuit that disagreed would be two answers to one
            // question — this is the arm that makes the gate's the one that
            // counts.
            GenerationConsent::Denied => {
                return suppressed(
                    &ctx,
                    Suppression::DeniedLevel(
                        gate.repo_context_generation_denial_note(&key)
                            .unwrap_or_else(|| "the session's permission level forbids it".into()),
                    ),
                );
            }
        }
    }

    OfferOutcome::Ran(run(ctx, ConsentGiven::granted(target), force).await)
}

/// Publish a suppression and answer with it — the one place the two are built
/// together, so a stage nobody was told about cannot be returned.
fn suppressed(ctx: &GenerationContext<'_>, why: Suppression) -> OfferOutcome {
    let reason = suppression_news(&why);
    let outcome = OfferOutcome::Suppressed(why);
    emit_reason(ctx, outcome.wire(), &Progress::default(), &reason);
    outcome
}

/// A [`Suppression`]'s own words — the news half of LESSON-557 for the offer's
/// short-circuits, in one place because [`announce_unconfigured`] publishes one
/// of them from a call site that never built a [`GenerationContext`].
fn suppression_news(why: &Suppression) -> String {
    match why {
        Suppression::Never => {
            "`[context] generate = never` — `/context init` still writes one".to_owned()
        }
        // The key by its name and the command that flips it: this one refuses
        // the explicit door too, so a user who typed `/context init` and got
        // nothing is owed the setting that answered and the way to change it.
        Suppression::SwitchedOff => "`[context] repo_file = false` — the notes are switched off \
                                     durably, so nothing is written; `teton context enable` turns \
                                     them back on"
            .to_owned(),
        Suppression::AlreadyPresent { name, bytes } => match bytes {
            Some(bytes) => {
                format!("a {name} of {bytes} bytes is already there; `--force` replaces it")
            }
            // The `stat` would not answer, so there is no size to state — and
            // a `0` here would tell the user their file is empty.
            None => format!("a {name} is already there; `--force` replaces it"),
        },
        Suppression::DeniedLevel(note) => note.clone(),
        Suppression::NoDurableRoot => {
            "this root has no canonical name, so there is nothing to ask about".to_owned()
        }
        Suppression::Unconfigured => {
            "the configured privacy boundaries do not compile, so nothing was read".to_owned()
        }
    }
}

/// Publish [`Suppression::Unconfigured`] for a caller that has no
/// [`GenerationContext`] to publish it from, and answer with it.
///
/// The boundary set is what a [`GenerationContext`] is *built* around — the
/// gatherer excludes by it and the loader withholds by it — so the one caller
/// that discovers it will not compile discovers it before there is a context to
/// hand [`suppressed`]. Without this it returned silently, and a suppression
/// nobody was told about is the one shape [`suppressed`] exists to prevent: the
/// session record would say `Suppressed` and no line on the bus would say why.
///
/// Everything the event needs is here except the two things that do not exist
/// yet: nothing has been measured, and no route has been resolved, so the tier
/// is absent rather than guessed. The root is its **display** — home-relative
/// and already bounded by the probe — never the absolute path, which is
/// [`emit_news`]'s rule and the whole of the news/location split.
#[must_use]
pub fn announce_unconfigured(events: &SessionEvents, root: &ProbedRoot) -> OfferOutcome {
    let why = Suppression::Unconfigured;
    let news = suppression_news(&why);
    events.repo_context_generation(RepoContextGeneration {
        outcome: events::GenerationOutcome::Suppressed,
        root: root.view.display.clone(),
        entries: None,
        excluded: None,
        draft_bytes: None,
        tier: None,
        reason: Some(bounded_field(&news, REASON_MAX_CHARS)),
    });
    OfferOutcome::Suppressed(why)
}

/// Today, UTC, as `YYYY-MM-DD`.
///
/// The transcript's calendar rather than a second one (LESSON-456): `rfc3339_utc`
/// is exact for every proleptic-Gregorian date and is already the daemon's one
/// answer to "what day is it", so a header and a transcript written in the same
/// second cannot disagree about the date. The first ten characters of an RFC 3339
/// stamp are its date, by the format's own definition.
fn today() -> String {
    let mut stamp = rfc3339_utc(SystemTime::now());
    stamp.truncate(10);
    stamp
}

#[cfg(test)]
mod tests {
    use super::*;

    use crate::repo_context::evidence::{Cut, EvidenceClass};
    use crate::repo_context::render::GENERATED_HEADER_MAX_BYTES;

    /// An [`Evidence`] with `cut` and nothing else — the two phrase helpers read
    /// no other field.
    fn evidence_cut(cut: Option<Cut>) -> Evidence {
        Evidence {
            cut,
            ..Evidence::empty()
        }
    }

    /// **The header's optional facts are the walk's own, in the walk's own
    /// order.**
    ///
    /// ADR-5's example line is `(think tier; tree cut at depth 6)`, and both
    /// halves of that parenthesis are composed here — a stop and a cut are
    /// different omissions, and a reader who meets both should meet them in the
    /// order they happened.
    ///
    /// Mutation (LESSON-441), run 2026-09-03 and restored: composing the phrases
    /// in the other order — `cut` where `stop` goes — fails the golden line
    /// below. The *call site*'s own ordering is a separate claim and is pinned
    /// where a run produces both omissions at once
    /// (`tests/repo_context_generation.rs::every_stage_failure_is_typed_leaves_no_file_and_keeps_provider_health`).
    #[test]
    fn the_header_states_the_stop_then_the_cut_and_stays_under_its_bound() {
        assert_eq!(stop_phrase(None), None);
        assert_eq!(
            stop_phrase(Some(WalkStop::Entries(100_000))).as_deref(),
            Some("walk stopped at 100000 entries")
        );
        assert_eq!(
            stop_phrase(Some(WalkStop::WallClock(std::time::Duration::from_millis(
                10_250
            ))))
            .as_deref(),
            Some("walk stopped after 10.2 s")
        );

        assert_eq!(cut_phrase(&evidence_cut(None)), None);
        assert_eq!(
            cut_phrase(&evidence_cut(Some(Cut {
                class: EvidenceClass::Tree,
                depth: Some(6),
            })))
            .as_deref(),
            Some("tree cut at depth 6")
        );
        assert_eq!(
            cut_phrase(&evidence_cut(Some(Cut {
                class: EvidenceClass::Readme,
                depth: None,
            })))
            .as_deref(),
            Some("README left out")
        );

        let line = generated_header(
            Tier::Think.as_str(),
            "2026-09-03",
            stop_phrase(Some(WalkStop::Entries(100_000))).as_deref(),
            cut_phrase(&evidence_cut(Some(Cut {
                class: EvidenceClass::Tree,
                depth: Some(6),
            })))
            .as_deref(),
        );
        assert_eq!(
            line,
            "> Generated by Teton on 2026-09-03 (think tier; walk stopped at 100000 entries; \
             tree cut at depth 6). Edit freely — Teton reads this file at every session start."
        );
        assert!(line.len() <= GENERATED_HEADER_MAX_BYTES);
    }

    /// **`draft_bytes` is the model's bytes, never this build's header line.**
    ///
    /// The event's contract, and the arithmetic it rests on is `bound_answer`'s:
    /// the header goes in first and is given a newline when it has none, so the
    /// figure the event reports has to subtract exactly that much and no more.
    ///
    /// Mutation, run 2026-09-03 and restored: returning `header.len()`
    /// unconditionally reports one byte too many for every real header (they
    /// never end in a newline) and fails the first case here.
    #[test]
    fn the_drafted_figure_excludes_the_header_and_the_newline_it_is_given() {
        let header = "> Generated by Teton";
        let body = bound_answer("## Purpose\nA repository.\n", header);
        assert_eq!(header_prefix_len(header), header.len() + 1);
        assert_eq!(
            body.len() - header_prefix_len(header),
            "## Purpose\nA repository.\n".len()
        );
        assert_eq!(header_prefix_len("with a newline\n"), 15);
        assert_eq!(header_prefix_len(""), 0);
    }

    /// **The date is the transcript's calendar, spelled `YYYY-MM-DD`.**
    ///
    /// One calendar, not two: the shape is asserted rather than the value, since
    /// the only wrong answer this can give is a differently-shaped one.
    #[test]
    fn the_header_date_is_ten_characters_of_iso_calendar() {
        let date = today();
        assert_eq!(date.len(), 10, "{date}");
        let (year, rest) = date.split_at(4);
        assert!(year.chars().all(|c| c.is_ascii_digit()), "{date}");
        assert!(rest.starts_with('-'), "{date}");
        assert_eq!(date.matches('-').count(), 2, "{date}");
    }

    /// **Every typed reason has words, and they are bounded on the way out.**
    ///
    /// The news half of LESSON-557: the daemon carries facts to the surface *and*
    /// says something short on the bus, and neither is the other's leftovers. A
    /// reason with no words would reach a monitor as a `failed` with nothing
    /// beside it.
    #[test]
    fn every_reason_has_bounded_news() {
        let reasons = [
            Reason::NothingToDraft,
            Reason::WindowTooNarrow { window_bytes: 900 },
            Reason::Duty("x".repeat(4_000)),
            Reason::EmptyDraft,
            Reason::AlreadyExists,
            Reason::Symlink,
            Reason::Io(ErrorKind::PermissionDenied),
            Reason::NotLoaded {
                kind: RepoContextStateKind::Truncated,
                left_in_place: false,
            },
            Reason::NotLoaded {
                kind: RepoContextStateKind::Truncated,
                left_in_place: true,
            },
        ];
        for reason in &reasons {
            let news = bounded_field(&reason.as_news(), REASON_MAX_CHARS);
            assert!(!news.trim().is_empty(), "{reason:?} has no news");
            assert!(
                news.chars().count() <= REASON_MAX_CHARS,
                "{reason:?} is unbounded"
            );
        }
    }

    /// **Every suppression has words too, and the two that name a fact state
    /// it.**
    ///
    /// [`suppression_news`] is the offer's half of the same rule: four of these
    /// are reached with nothing else on the bus, so a suppression with no words
    /// is a `suppressed` line a client renders as "no offer to write TETON.md —"
    /// and stops. The two assertions past the loop are the facts a user acts on:
    /// the durable switch's key and the command that flips it, and the size of
    /// the file that is in the way — the substring
    /// `crates/teton/tests/cli_e2e.rs`'s AC-10 leg reads off stdout.
    ///
    /// Mutation (LESSON-441), run 2026-09-04 and restored: wording
    /// `AlreadyPresent`'s unknown-size arm with `{bytes:?}` in place of the
    /// no-size sentence puts `None` in the line; the `"None"` assertion below
    /// fails, which is the `0`-that-was-never-measured this `Option` exists to
    /// prevent.
    #[test]
    fn every_suppression_has_bounded_news_naming_its_own_fact() {
        let whys = [
            Suppression::Never,
            Suppression::SwitchedOff,
            Suppression::AlreadyPresent {
                name: "TETON.md",
                bytes: Some(412),
            },
            Suppression::AlreadyPresent {
                name: "AGENTS.md",
                bytes: None,
            },
            Suppression::DeniedLevel("the session's permission level forbids it".to_owned()),
            Suppression::NoDurableRoot,
            Suppression::Unconfigured,
        ];
        for why in &whys {
            let news = bounded_field(&suppression_news(why), REASON_MAX_CHARS);
            assert!(!news.trim().is_empty(), "{why:?} has no news");
            assert!(
                news.chars().count() <= REASON_MAX_CHARS,
                "{why:?} is unbounded: {news}"
            );
        }

        let switched = suppression_news(&Suppression::SwitchedOff);
        assert!(
            switched.contains("[context] repo_file = false")
                && switched.contains("teton context enable"),
            "the durable refusal names the key and the way back: {switched}"
        );

        let sized = suppression_news(&Suppression::AlreadyPresent {
            name: "TETON.md",
            bytes: Some(412),
        });
        assert_eq!(
            sized, "a TETON.md of 412 bytes is already there; `--force` replaces it",
            "AC-10 reads `412 bytes is already there` off this line"
        );
        let no_size = suppression_news(&Suppression::AlreadyPresent {
            name: "AGENTS.md",
            bytes: None,
        });
        assert_eq!(
            no_size, "a AGENTS.md is already there; `--force` replaces it",
            "an unanswered `stat` states no size rather than a zero"
        );
        assert!(!no_size.contains("None"), "{no_size}");
    }
}
