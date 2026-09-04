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
//! failure at any of them leaves no file.
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
use std::path::PathBuf;
use std::time::SystemTime;

use teton_core::boundary::BoundaryMatcher;
use teton_core::config::Config;
use teton_core::session_root::bounded_field;
use teton_protocol::events::{self, RepoContextGeneration};
use teton_protocol::methods::RepoContextStateKind;
use teton_protocol::Tier;

use crate::harness::digest::tool_result_provenance;
use crate::harness::draft::{bound_answer, build_prompt_from_evidence};
use crate::harness::duty::DutyRoute;
use crate::harness::tools::walk::WalkBudget;
use crate::harness::SessionEvents;
use crate::session_root::ProbedRoot;
use crate::transcript::record::rfc3339_utc;

use super::evidence::{self, Evidence, EvidenceBudget, WalkStop};
use super::render::generated_header;
use super::write;
use super::{RepoContext, RepoContextState, RepoFileReader};

/// How wide the event's `reason` may be, in characters.
///
/// The event's own doc calls the field repository-adjacent content and asks for
/// `bounded_field`; this is the budget it is bounded to. Generous enough for a
/// duty's own sentence (`the 'draft' category resolves to 'frontier', which is
/// not a configured provider.`) and far short of anything a repository could make
/// a monitor's line out of.
const REASON_MAX_CHARS: usize = 200;

/// The human said yes — or said it by asking (`/context init`).
///
/// A witness, and its only job is to make [`run`] unreachable from a path that
/// never asked: a function taking `bool` is a function some later caller passes
/// `true` to for convenience, and what would be waved through is a walk of the
/// user's repository followed by a frontier model call.
///
/// It is deliberately *not* proof of a particular gate answer — the gate lives
/// with the session (ADR-2) and this module cannot see it. What
/// [`Self::granted`] asserts is that the caller stood at a place where consent
/// was settled: an accepted prompt, a `full`-level session, `generate = always`,
/// or the user's own `/context init`.
#[derive(Debug, Clone, Copy)]
pub struct ConsentGiven(());

impl ConsentGiven {
    /// Mint the witness. Call this only where the answer is actually in hand.
    #[must_use]
    pub fn granted() -> Self {
        Self(())
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
    /// resident (BR-7). Carries what the loader answered instead.
    NotLoaded(RepoContextStateKind),
}

impl Reason {
    /// The event's `reason` line: this build's own words for the fact, short
    /// enough to render beside a stage name.
    ///
    /// The broadcast half, not the user's remedy. The remedy sentence belongs to
    /// the surface that has the typed [`Reason`] in hand.
    #[must_use]
    fn as_news(&self) -> String {
        match self {
            Reason::NothingToDraft => "the walk found nothing to draft from".to_owned(),
            Reason::Duty(sentence) => sentence.clone(),
            Reason::EmptyDraft => "the draft came back empty".to_owned(),
            Reason::AlreadyExists => "a TETON.md is already there".to_owned(),
            Reason::Symlink => "the path is a symlink".to_owned(),
            Reason::Io(kind) => format!("the write failed: {kind}"),
            Reason::NotLoaded(kind) => format!(
                "the file was written and read back as {}",
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
    /// A stage failed and **no file was left behind** (BR-9).
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
pub struct GenerationContext<'a> {
    /// The session's probed root: one probe, so the jail the walk runs under, the
    /// directory the file is written into and the root the loader reads at are
    /// one directory spelled once.
    pub root: &'a ProbedRoot,
    /// REQ-612's file-reader seam, shared by the gatherer and the loader.
    pub reader: &'a dyn RepoFileReader,
    /// The session's compiled boundary set — what the gatherer excludes by and
    /// what the loader withholds by.
    pub boundaries: &'a BoundaryMatcher<'a>,
    /// The evidence body's byte budget, derived by the caller from the draft
    /// route's own window (ADR-3: only the caller knows the route).
    pub budget: EvidenceBudget,
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
    pub route: &'a dyn Fn() -> DutyRoute,
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
/// # Every exit is one of four, and three of them leave no file
///
/// A stop the walk's budget imposed is **not** one of them: a partial listing is
/// still a listing, it is written into the tree the model is shown and into the
/// header the reader meets, and refusing to draft from it would turn a large
/// repository into a repository that can never have notes (BR-9).
pub async fn run(
    ctx: GenerationContext<'_>,
    consent: ConsentGiven,
    force: bool,
) -> GenerationOutcome {
    // The witness has no fields to read: what it asserts is that this call
    // happened at all. Bound rather than ignored in the signature so that the
    // parameter cannot be quietly dropped by a later edit.
    let ConsentGiven(()) = consent;

    // --- walking ---------------------------------------------------------
    publish(
        &ctx,
        events::GenerationOutcome::Walking,
        &Progress::default(),
    );
    let evidence = evidence::gather_with_walk_budget(
        ctx.root,
        ctx.reader,
        ctx.boundaries,
        ctx.budget,
        ctx.walk,
    );
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
    let written = if force {
        write::replace(&ctx.root.path, &body)
    } else {
        write::write_new(&ctx.root.path, &body)
    };
    let written = match written {
        Ok(written) => written,
        Err(failure) => return fail(&ctx, &drafted, Stage::Write, reason_for(failure)),
    };

    // --- loading ---------------------------------------------------------
    //
    // REQ-612's loader on the file just written, same run, same rules an authored
    // file is read by (BR-7). Anything short of `Loaded` is a failure *and* an
    // unlink: a file the loader will not make resident is a file the next session
    // meets as if the repository had written it.
    let state = RepoContext::load(
        ctx.root,
        ctx.boundaries,
        ctx.config.context.repo_file,
        ctx.reader,
    );
    if !matches!(state, RepoContextState::Loaded(_)) {
        write::remove(&written.path);
        let kind = state.kind();
        return fail(&ctx, &drafted, Stage::Load, Reason::NotLoaded(kind));
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
    ctx.events.repo_context_generation(RepoContextGeneration {
        outcome,
        root: ctx.root.view.display.clone(),
        entries: progress.entries.map(|entries| entries as u64),
        excluded: progress.excluded.map(|excluded| excluded as u32),
        draft_bytes: progress.draft_bytes.map(|bytes| bytes as u64),
        tier: Some(ctx.tier),
        reason: reason.map(|reason| bounded_field(&reason.as_news(), REASON_MAX_CHARS)),
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
            Reason::Duty("x".repeat(4_000)),
            Reason::EmptyDraft,
            Reason::AlreadyExists,
            Reason::Symlink,
            Reason::Io(ErrorKind::PermissionDenied),
            Reason::NotLoaded(RepoContextStateKind::Truncated),
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
}
