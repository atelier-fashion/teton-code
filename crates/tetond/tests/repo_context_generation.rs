//! **The generation pipeline, end to end after consent** (REQ-613 TASK-385,
//! BR-4/BR-5/BR-7/BR-9, AC-7/AC-9, architecture ADR-6).
//!
//! `repo_context::generate::run` joins five acts the daemon already knew how to
//! do one at a time — walk a tree, run a duty, bound an answer, write a file,
//! load a file. Each act ships its own test at its own layer (`evidence`'s walk,
//! `draft`'s prompt and bounding, `write`'s no-clobber open, REQ-612's loader).
//! What none of them can show is the **join**, which is what this file is: the
//! duty gets the evidence's provenance and nothing the boundary covered, the one
//! call lands as one named cost row, the file that is written is the file that
//! loads, and every way a stage can fail ends with a typed answer and an empty
//! directory.
//!
//! ## The seam is the duty seam's own
//!
//! `DutyRoute::Serves` takes an `Arc<dyn Duty>`, so a fake duty is a route like
//! any other — the same shape `duty_matrix.rs` and `duty_egress.rs` use, one
//! level up. Two fixtures are needed and both are here: a **recording** duty for
//! the claims about what the pipeline handed over (BR-4, BR-9), and a **real
//! remote route** over `Egress` with a real in-memory `CostLedger` for the claim
//! about what the call cost (BR-5) — because a cost row is written by the choke
//! point when the metered body drains, and a fake that never reaches a transport
//! would let the ledger assertion pass over a call nobody made.
//!
//! ## Non-vacuity (LESSON-485)
//!
//! Every failure row asserts the *absence* of a file, which is a claim an
//! entirely broken pipeline also satisfies. So each of them is paired with the
//! working run at the top of the same test — the same fixture, the same root,
//! differing in one argument — which really does leave a `TETON.md` behind.

use std::io::ErrorKind;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use futures::{stream, StreamExt};

use serde_json::{json, Value};

use teton_core::boundary::BoundaryMatcher;
use teton_core::config::{Config, GenerateMode};
use teton_core::effort::{EffortLevel, ResolvedEffort};
use teton_core::entities::{BoundaryMode, PrivacyBoundary};
use teton_protocol::events::{
    Event, GenerationOutcome as Stage, PermissionRequest, PermissionSubject, RepoContextGeneration,
};
use teton_protocol::methods::{
    ConfigUpdate, ContextAction, PermissionOutcome, ProviderConfig, RepoContextGenerateMode,
    RepoContextOrigin, RepoContextStateKind, RootKind, SessionContextParams, SessionContextResult,
    SessionPermissionsParams, SessionRoot, SessionSetCwdParams, TierBindingConfig,
};
use teton_protocol::permissions::PermissionLevel;
use teton_protocol::{
    Category, Phase as ProtoPhase, ProviderId, ProviderKind as ProtoProviderKind, SessionId,
    SessionMode, Tier, Tier as ProtoTier,
};
use teton_providers::transport::{
    ByteStream, HttpMethod, Transport, TransportError, TransportRequest, TransportResponse,
};
use teton_providers::{
    CapabilityProfile, Provider, ProviderError, StopReason, TokenUsage, TurnCompletion, TurnEvent,
    TurnRequest, TurnStream,
};

use tetond::broadcast::{EventBus, Subscription};
use tetond::cost::{CostLedger, NoopCostSink, PriceTable};
use tetond::egress::{Egress, NoopSink, Provenance};
use tetond::grants::{ConnectionId, GrantRegistry};
use tetond::harness::permissions::{
    AddressedPermissionDelivery, PendingPermissions, PermissionGate,
};
use tetond::harness::tools::walk::WalkBudget;
use tetond::harness::{Duty, DutyRoute, SessionEvents, DRAFT_DUTY};
use tetond::repo_context::evidence::{self, EvidenceBudget, WalkStop};
use tetond::repo_context::generate::{
    self, ConsentGiven, DraftBudget, GenerationContext, GenerationOutcome, Offer, OfferOutcome,
    Reason, Stage as FailStage, Suppression, DRAFT_RESERVED_BYTES,
};
use tetond::repo_context::{RealFiles, RepoContextBlock, RepoContextState, REPO_CONTEXT_MAX_BYTES};
use tetond::runtime::{ClientPresence, DaemonRuntime};
use tetond::session_root::ProbedRoot;
use tetond::sessions::{GenerationState, SessionRegistry};

/// A marker that exists only inside `Cargo.toml`, so its appearance in a prompt
/// the duty was handed is a leak and its absence is the exclusion holding
/// (LESSON-432: assert on the bytes, not on the code path).
const MANIFEST_MARKER: &str = "sk-live-DO-NOT-DRAFT-req613-Jk4";

/// The session every fixture runs as.
const SESSION: &str = "sess-generation";

/// A well-formed answer from the draft duty: the five sections, in order.
const GOOD_DRAFT: &str = "## Purpose\nA sample repository.\n\n## Layout\n`src/` holds the \
                          binary.\n\n## Build & test\n`cargo test`.\n\n## Conventions\nNone \
                          stated.\n\n## Where to look\n`src/main.rs`.\n";

// ===========================================================================
// The fixture: a real project root, a bus, and a session emitter
// ===========================================================================

/// A planted project, canonical, with one member of each evidence class.
///
/// Real rather than in-memory: the walker under the pipeline is the production
/// walker and lists the real filesystem, and the write is a real `create_new`
/// with `O_NOFOLLOW`. A double with its own idea of what exists could disagree
/// with what the walk found, and the disagreement would look like a pass.
struct Fixture {
    dir: PathBuf,
    root: ProbedRoot,
    bus: Arc<EventBus>,
    events: SessionEvents,
    config: Config,
}

impl Fixture {
    fn new(tag: &str) -> Self {
        static NEXT: AtomicU64 = AtomicU64::new(0);
        let dir = std::env::temp_dir().join(format!(
            "teton-generation-{tag}-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        std::fs::create_dir_all(dir.join("src")).unwrap();
        let dir = std::fs::canonicalize(&dir).unwrap();
        std::fs::write(
            dir.join("Cargo.toml"),
            format!("[package]\nname = \"sample\"\n# {MANIFEST_MARKER}\n"),
        )
        .unwrap();
        std::fs::write(dir.join("README.md"), "# Sample\n\nA planted repository.\n").unwrap();
        std::fs::write(dir.join("src/main.rs"), "fn main() { println!(\"hi\"); }\n").unwrap();

        let bus = Arc::new(EventBus::new());
        let events = SessionEvents::new(Arc::clone(&bus), SessionId::from(SESSION));
        Self {
            root: ProbedRoot {
                path: dir.clone(),
                view: SessionRoot {
                    display: "~/sample".to_owned(),
                    kind: RootKind::Project,
                    project_name: Some("sample".to_owned()),
                    vcs_branch: None,
                },
            },
            dir,
            bus,
            events,
            config: Config::default(),
        }
    }

    fn notes(&self) -> PathBuf {
        self.dir.join("TETON.md")
    }

    /// Whether a `TETON.md` — of any kind, following nothing — is at the root.
    fn notes_exist(&self) -> bool {
        std::fs::symlink_metadata(self.notes()).is_ok()
    }

    fn subscribe(&self) -> Subscription {
        self.bus.subscribe(256)
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        // A read-only root would otherwise defeat the cleanup of the very test
        // that made it one.
        set_dir_mode(&self.dir, 0o755);
        std::fs::remove_dir_all(&self.dir).ok();
    }
}

fn set_dir_mode(dir: &Path, mode: u32) {
    use std::os::unix::fs::PermissionsExt as _;
    std::fs::set_permissions(dir, std::fs::Permissions::from_mode(mode)).ok();
}

/// Everything `run` needs, with the one budget and the one route the caller
/// varies.
///
/// A helper rather than nine fields spelled per test, for `GenerationContext`'s
/// own reason: the tests below differ in `force`, in the route, and in at most
/// one other argument, and a hand-written context per test is a test that
/// accidentally differs in a tenth thing.
struct Run<'a> {
    boundaries: &'a [PrivacyBoundary],
    /// `Send + Sync` because production holds this closure across the gate's
    /// `await` on a spawned task (`GenerationContext::route`); every fixture
    /// below is already both.
    route: &'a (dyn Fn() -> DutyRoute + Send + Sync),
    budget: DraftBudget,
    walk: WalkBudget,
    force: bool,
}

impl<'a> Run<'a> {
    fn new(route: &'a (dyn Fn() -> DutyRoute + Send + Sync)) -> Self {
        Self {
            boundaries: &[],
            route,
            // Roomy: every fixture's evidence is a few hundred bytes, so a cut is
            // something a test asks for rather than something it stumbles into.
            // Spelled as a *window* and put through the production derivation,
            // so a leg that wants a narrow body says how narrow the route was.
            budget: generate::evidence_budget_for(64 * 1024),
            walk: WalkBudget::default(),
            force: false,
        }
    }
}

/// A [`DraftBudget`] whose evidence body may spend exactly `body_bytes`, over a
/// route wide enough to have afforded it.
///
/// The derivation's own arithmetic rather than a hand-built struct: a leg that
/// asked for a body of *n* bytes is asking about the assembly, and one that
/// asked for a route of *n* bytes is asking about the window — keeping the two
/// spellings apart is what makes `WindowTooNarrow`'s leg below a different
/// claim from `NothingToDraft`'s.
fn body_budget(body_bytes: usize) -> DraftBudget {
    generate::evidence_budget_for(DRAFT_RESERVED_BYTES + body_bytes)
}

async fn generate(fx: &Fixture, run: Run<'_>) -> GenerationOutcome {
    let matcher = BoundaryMatcher::new(run.boundaries).expect("the fixture globs compile");
    generate::run(
        GenerationContext {
            root: &fx.root,
            reader: &RealFiles,
            boundaries: &matcher,
            budget: run.budget,
            walk: run.walk,
            route: run.route,
            events: &fx.events,
            tier: Tier::Think,
            config: &fx.config,
        },
        // The fixture's root is canonical (`Fixture::new` canonicalizes before
        // it plants anything), so the directory consent is minted for and the
        // directory the session stands on are one. The case where they are not
        // is `offer_and_run`'s, and is pinned there.
        ConsentGiven::granted(&fx.dir),
        run.force,
    )
    .await
}

// ===========================================================================
// The fake duty
// ===========================================================================

/// What the recording duty was handed, once per call.
type Seen = Arc<Mutex<Vec<(String, Provenance)>>>;

/// The draft duty, faked: it records the prompt and the provenance it was given
/// and answers with whatever the fixture scripted.
///
/// It implements the seam's own trait, so the pipeline drives it through
/// `DutyRoute::perform` — the deadline, the announcement and the error shape are
/// all still the seam's. What is faked is the model, and only the model.
struct Recording {
    seen: Seen,
    answer: Result<String, String>,
}

#[async_trait]
impl Duty for Recording {
    fn category(&self) -> Category {
        DRAFT_DUTY.category()
    }

    fn ceiling_bytes(&self) -> usize {
        DRAFT_DUTY.ceiling_bytes()
    }

    async fn perform(&self, prompt: &str, provenance: &Provenance) -> Result<String, String> {
        self.seen
            .lock()
            .expect("recorder poisoned")
            .push((prompt.to_owned(), provenance.clone()));
        self.answer.clone()
    }
}

/// A route served by [`Recording`].
fn recording_route(seen: &Seen, answer: Result<String, String>) -> DutyRoute {
    DutyRoute::Serves {
        provider_id: "fake-think".to_owned(),
        duty: Arc::new(Recording {
            seen: Arc::clone(seen),
            answer,
        }),
        // No routing decision was made, so there is nothing to announce — the
        // same `None` the transport-free offline entry point builds.
        announce: None,
    }
}

/// A recording route that answers well, and the recorder to read it back from.
fn answers_well() -> (Seen, impl Fn() -> DutyRoute) {
    let seen: Seen = Arc::default();
    let handle = Arc::clone(&seen);
    (seen, move || {
        recording_route(&handle, Ok(GOOD_DRAFT.to_owned()))
    })
}

// ===========================================================================
// A real remote route: the cost row's fixture
// ===========================================================================

/// A transport that answers with an Anthropic-shaped SSE body carrying a known
/// `(input, output)` usage, so a recorded row can be tied to a known call.
#[derive(Clone)]
struct ScriptedTransport {
    usage: (u64, u64),
}

#[async_trait]
impl Transport for ScriptedTransport {
    async fn execute(
        &self,
        _request: TransportRequest,
    ) -> Result<TransportResponse, TransportError> {
        let (input, output) = self.usage;
        Ok(TransportResponse {
            location: None,
            status: 200,
            body: anthropic_body(input, output),
        })
    }
}

fn anthropic_body(input: u64, output: u64) -> ByteStream {
    let s = format!(
        "event: message_start\n\
         data: {{\"message\":{{\"usage\":{{\"input_tokens\":{input},\"output_tokens\":1}}}}}}\n\n\
         event: message_delta\n\
         data: {{\"usage\":{{\"output_tokens\":{output}}}}}\n\n\
         event: message_stop\ndata: {{}}\n\n"
    );
    Box::pin(futures::stream::once(async move { Ok(s.into_bytes()) }))
}

/// What the stand-in provider does when the duty calls it.
#[derive(Clone)]
enum Answer {
    /// Put the request on the wire, drain the response, then stream the reply.
    Says(String),
    /// Refuse the request as larger than the window — the "over-window" leg of
    /// BR-9, and the one provider error that carries **no** `FailureClass`, so
    /// nothing about it can move a provider's health.
    OverWindow,
}

/// A provider that actually reaches the transport it is handed, and **drains the
/// response body**.
///
/// The draining is load-bearing, not tidiness: the choke point meters a call when
/// its body is consumed, so a provider that dropped the response would leave the
/// ledger empty and BR-5's assertion passing over a call that was never billed.
struct WireProvider {
    answer: Answer,
}

#[async_trait]
impl Provider for WireProvider {
    fn id(&self) -> &str {
        "anthropic"
    }

    fn capabilities(&self) -> CapabilityProfile {
        CapabilityProfile::default()
    }

    async fn stream_turn(
        &self,
        request: TurnRequest,
        transport: &dyn Transport,
    ) -> Result<TurnStream, ProviderError> {
        let reply = match &self.answer {
            Answer::Says(reply) => reply.clone(),
            // Before the transport, deliberately: a window refusal is decided on
            // the request's size, so nothing is sent and the leg is
            // distinguishable from a privacy block by captured bytes.
            Answer::OverWindow => {
                return Err(ProviderError::ContextLengthExceeded {
                    provider_id: "anthropic".to_owned(),
                })
            }
        };
        let body = serde_json::to_vec(&request).map_err(|e| ProviderError::Build(e.to_string()))?;
        let response = transport
            .execute(TransportRequest {
                method: HttpMethod::Post,
                url: "https://api.anthropic.com/v1/messages".to_owned(),
                headers: Vec::new(),
                body,
            })
            .await
            .map_err(|err| match err {
                TransportError::PrivacyBlocked(detail) => ProviderError::PrivacyBlocked(detail),
                _ => ProviderError::Transport,
            })?;
        let mut body = response.body;
        while let Some(chunk) = body.next().await {
            chunk.map_err(|_| ProviderError::Transport)?;
        }
        Ok(Box::pin(stream::iter(vec![
            Ok(TurnEvent::TextDelta(reply)),
            Ok(TurnEvent::Completed(TurnCompletion {
                usage: TokenUsage::default(),
                stop_reason: StopReason::EndTurn,
            })),
        ])))
    }
}

/// A genuinely remote draft route: the real `Egress`, the real duty seam, a
/// scripted wire.
fn remote_route(
    answer: Answer,
    boundaries: Vec<PrivacyBoundary>,
    ledger: Option<Arc<CostLedger>>,
    usage: (u64, u64),
) -> DutyRoute {
    let egress = Egress::new(ScriptedTransport { usage }, boundaries, Arc::new(NoopSink));
    let egress = match ledger {
        Some(ledger) => egress.with_cost_meter(ledger),
        None => egress,
    };
    DutyRoute::remote(
        DRAFT_DUTY,
        "anthropic",
        Box::new(WireProvider { answer }),
        egress,
        "claude-opus-5",
        SESSION,
        ResolvedEffort::effort(EffortLevel::High),
    )
}

// ===========================================================================
// Reading the bus
// ===========================================================================

/// Every event published so far, in order. `try_recv` rather than a timed
/// `recv`: publishing is synchronous, so a caller that knows the run has finished
/// drains deterministically (LESSON-450).
fn published(sub: &mut Subscription) -> Vec<Event> {
    let mut out = Vec::new();
    while let Some(envelope) = sub.try_recv() {
        assert_eq!(
            envelope.session_id.as_ref().map(|id| id.0.as_str()),
            Some(SESSION),
            "every stage is attributed to the session that ran it"
        );
        out.push(envelope.event);
    }
    out
}

/// Just the generation stages, in order.
fn stages(sub: &mut Subscription) -> Vec<RepoContextGeneration> {
    published(sub)
        .into_iter()
        .filter_map(|event| match event {
            Event::RepoContextGeneration(news) => Some(news),
            _ => None,
        })
        .collect()
}

/// The stage names, in order — the shape most assertions want.
fn names(stages: &[RepoContextGeneration]) -> Vec<Stage> {
    stages.iter().map(|stage| stage.outcome).collect()
}

// ===========================================================================
// BR-4 — the duty gets the evidence's provenance, and the exclusion rides out
// ===========================================================================

/// **BR-4.** The draft duty is handed the evidence's own `Sources` — never
/// `Unknown`, never a file a boundary covered — and the count of what was dropped
/// reaches the surface on the `drafted` event.
///
/// Two claims that have to be made together. The provenance is what the choke
/// point scopes the call by, so an over-broad one refuses a call that should go
/// and an under-broad one lets bytes out unjudged; and the exclusion is invisible
/// unless it is *counted*, because a file that was silently not read looks
/// exactly like a file that was not there.
///
/// The captured **bytes** are asserted alongside the provenance (LESSON-432): the
/// covered manifest's marker exists nowhere else in the fixture, so its absence
/// from the prompt is the exclusion holding rather than the code path being
/// walked.
///
/// **Mutations** (LESSON-441), all three run 2026-09-03 and restored:
/// 1. handing `Provenance::empty()` to `perform` in place of the evidence's —
///    the source-set assertion fails, and the privacy leg of
///    `every_stage_failure_is_typed_leaves_no_file_and_keeps_provider_health`
///    stops refusing;
/// 2. dropping `excluded` from the `Progress` the `drafted` event carries — the
///    `Some(1)` assertion fails;
/// 3. publishing `excluded: Some(0)` before the walk — the `walking` stage's
///    `None` assertion fails, which is the distinction the `Option` exists for.
#[tokio::test]
async fn the_draft_duty_receives_the_evidence_provenance_and_the_excluded_count_rides_the_event() {
    let fx = Fixture::new("provenance");
    let mut sub = fx.subscribe();
    let (seen, route) = answers_well();
    let boundaries = vec![PrivacyBoundary::user("Cargo.toml", BoundaryMode::LocalOnly)];

    let outcome = generate(
        &fx,
        Run {
            boundaries: &boundaries,
            ..Run::new(&route)
        },
    )
    .await;
    assert!(
        matches!(outcome, GenerationOutcome::Written(_)),
        "{outcome:?}"
    );

    // --- what the duty was handed -----------------------------------------
    let calls = seen.lock().unwrap().clone();
    assert_eq!(calls.len(), 1, "one draft is one model call");
    let (prompt, provenance) = &calls[0];

    assert!(
        !provenance.is_unknown(),
        "the draft's provenance is never Unknown (BR-4)"
    );
    let mut sources: Vec<&str> = provenance.sources().collect();
    sources.sort_unstable();
    assert_eq!(
        sources,
        vec!["README.md", "src/main.rs"],
        "the provenance is exactly the evidence files whose bytes are in the prompt"
    );
    assert!(
        !prompt.contains(MANIFEST_MARKER),
        "a covered file's bytes reached the draft call"
    );
    // Non-vacuity: the files that were *not* covered really did travel, so the
    // absence above is an exclusion and not an empty prompt.
    assert!(prompt.contains("A planted repository."), "{prompt}");
    assert!(prompt.contains("fn main()"), "{prompt}");

    // --- what the surface was told ----------------------------------------
    let stages = stages(&mut sub);
    assert_eq!(
        names(&stages),
        vec![Stage::Walking, Stage::Drafted, Stage::Written],
        "one event per stage, in the order the stages happen"
    );
    let walking = &stages[0];
    assert_eq!(walking.entries, None, "nothing is measured before the walk");
    assert_eq!(walking.excluded, None);
    let drafted = &stages[1];
    assert_eq!(
        drafted.excluded,
        Some(1),
        "the covered manifest was counted"
    );
    assert!(
        drafted.entries.is_some_and(|entries| entries >= 4),
        "the walk's entries ride the event: {:?}",
        drafted.entries
    );
    assert_eq!(drafted.tier, Some(Tier::Think));
    assert!(drafted.draft_bytes.is_some_and(|bytes| bytes > 0));
    assert_eq!(
        drafted.root, "~/sample",
        "the root's home-relative display, the same spelling the offer showed"
    );
}

// ===========================================================================
// BR-5 / AC-7 — one named cost row
// ===========================================================================

/// **BR-5, AC-7.** The draft call lands in the ledger as **exactly one** row,
/// carrying its own `draft` category and the provider that actually served it.
///
/// The whole point of routing the draft through the duty seam is that the seam
/// meters it: the pipeline writes nothing to the ledger, and a second write there
/// would show a repository's notes costing twice what they cost. So the assertion
/// is on the count as hard as on the contents.
///
/// The route here is genuinely remote — the real `Egress`, the real
/// `RemoteDuty`, a scripted Anthropic wire — because a cost row is written when
/// the metered body drains, and a fake duty would let this pass over a call
/// nobody made. The token counts are checked against the script for the same
/// reason: they are what proves the row came from *this* call.
///
/// **Mutations**, both run 2026-09-03 and restored:
/// 1. dropping `.with_category(..)` from `RemoteDuty`'s attribution — the row
///    arrives with `category: None` and `/cost` can no longer name the draft;
/// 2. calling `route.perform` twice in `run` — two rows, and the count fails.
#[tokio::test]
async fn one_cost_row_names_the_draft_category_and_the_serving_provider() {
    let fx = Fixture::new("cost");
    let ledger = Arc::new(
        CostLedger::open_in_memory(PriceTable::bundled(), Arc::new(NoopCostSink))
            .expect("open an in-memory ledger"),
    );
    let usage = (4_100u64, 900u64);
    let meter = Arc::clone(&ledger);
    let route = move || {
        remote_route(
            Answer::Says(GOOD_DRAFT.to_owned()),
            Vec::new(),
            Some(Arc::clone(&meter)),
            usage,
        )
    };

    let outcome = generate(&fx, Run::new(&route)).await;
    assert!(
        matches!(outcome, GenerationOutcome::Written(_)),
        "the remote leg must succeed, or the ledger claim is vacuous: {outcome:?}"
    );

    let rows = ledger.all_records().expect("read the ledger");
    assert_eq!(rows.len(), 1, "one draft is one cost row: {rows:?}");
    let row = &rows[0];
    assert_eq!(
        row.category,
        Some(Category::Draft),
        "the row must name the draft category, or `/cost` cannot show it (AC-7)"
    );
    assert_eq!(row.provider_id, "anthropic", "the route that served it");
    assert_eq!(row.model, "claude-opus-5");
    assert_eq!(row.session_id, SESSION);
    assert_eq!(row.phase, None, "a duty has no lifecycle position");
    // The counts tie the row to this call rather than to any call.
    assert_eq!((row.input_tokens, row.output_tokens), usage);
}

// ===========================================================================
// BR-7 — written, then loaded, the same run
// ===========================================================================

/// **BR-7.** After the write the daemon runs REQ-612's loader on the new file —
/// same cap, same frame, same provenance — and the state that comes back is
/// `Loaded` with the block's bytes equal to the file's rendered block.
///
/// The outcome is `Written`, which is the fact a caller turns into
/// `RepoContextOrigin::Generated` on the wire; the mapping is asserted here
/// rather than assumed, because a pipeline that answered `Failed` on a file it
/// had successfully written would still leave a loadable file behind and every
/// filesystem assertion would pass.
///
/// The header is checked as the file's **first line**, not merely as a substring:
/// a header written anywhere else is a header the next reader does not meet.
///
/// **Mutations**, both run 2026-09-03 and restored:
/// 1. returning the pre-write `RepoContextState` instead of re-loading — the
///    state is `Absent` and the `Loaded` assertion fails;
/// 2. writing the body without the header — the first-line assertion fails and
///    the file no longer says who wrote it.
#[tokio::test]
async fn a_written_file_is_loaded_the_same_run_with_origin_generated() {
    let fx = Fixture::new("loaded");
    let mut sub = fx.subscribe();
    let (_seen, route) = answers_well();

    let outcome = generate(&fx, Run::new(&route)).await;
    let GenerationOutcome::Written(made) = &outcome else {
        panic!("a clean run over an empty root writes: {outcome:?}");
    };
    assert_eq!(
        outcome.wire(),
        Stage::Written,
        "the word the wire carries, and what a caller maps to origin: generated"
    );

    // --- the file ----------------------------------------------------------
    assert_eq!(made.path, fx.notes());
    let on_disk = std::fs::read_to_string(fx.notes()).expect("the file is there");
    assert_eq!(on_disk.len(), made.bytes, "the reported size is the file's");
    assert!(on_disk.len() <= REPO_CONTEXT_MAX_BYTES);
    let first = on_disk.lines().next().expect("a non-empty file");
    assert!(
        first.starts_with("> Generated by Teton on ") && first.contains("(think tier)"),
        "the header is the file's first line: {first}"
    );
    assert!(
        first.contains("Edit freely"),
        "the header invites the edit that makes the file the user's: {first}"
    );
    assert!(on_disk.contains("## Where to look"), "{on_disk}");

    // --- the loader --------------------------------------------------------
    let RepoContextState::Loaded(file) = &made.state else {
        panic!("the written file must load, not {:?}", made.state.kind());
    };
    assert_eq!(file.provenance.as_str(), "TETON.md");
    assert_eq!(file.bytes_on_disk as usize, on_disk.len());
    let block = RepoContextBlock::render(file, REPO_CONTEXT_MAX_BYTES);
    assert!(
        !block.truncated,
        "a generated file is bounded so the loader never truncates it (BR-6)"
    );
    assert_eq!(
        block.resident_bytes,
        on_disk.len(),
        "the block's bytes are the file's bytes"
    );
    assert!(block.text.contains(first), "the header is resident too");

    // --- the news ----------------------------------------------------------
    let stages = stages(&mut sub);
    assert_eq!(
        names(&stages),
        vec![Stage::Walking, Stage::Drafted, Stage::Written]
    );
    let written = stages.last().unwrap();
    assert_eq!(written.tier, Some(Tier::Think));
    assert_eq!(
        written.reason, None,
        "a written file has nothing to explain"
    );
    assert_eq!(
        written.draft_bytes,
        Some(made.draft_bytes as u64),
        "the event and the answer report one figure"
    );
    assert!(
        made.draft_bytes < made.bytes,
        "draft_bytes excludes this build's own header line"
    );
}

// ===========================================================================
// BR-9 / AC-9 — every failure is typed, leaves no file, and touches no health
// ===========================================================================

/// **BR-9, AC-9.** A duty error, a privacy block, an over-window answer and a
/// write error each end the run with a typed `Failed` naming the stage, with no
/// file on disk and the provider's health untouched — and a walk that stopped on
/// its budget is **not** a failure at all.
///
/// One table rather than six tests, for `duty_matrix.rs`'s reason: what BR-9
/// claims is that the *shape* is the same however a stage fails, and a table is
/// what makes adding a seventh failure without its row impossible to do quietly.
///
/// **Health is asserted as an absence, over a real bus.** Nothing on this path
/// holds a health handle, so the claim is that no run — however it failed —
/// published a `provider_degraded`. Two of the rows are the two provider failures
/// that carry no `FailureClass` at all (`PrivacyBlocked`, `ContextLengthExceeded`),
/// which is where the guarantee actually lives.
///
/// **Non-vacuity.** The run at the top writes a real file over the same fixture,
/// so "no file" below is a fact about the failures rather than about a pipeline
/// that never writes anything.
///
/// **Mutations**, all five run 2026-09-03 and restored:
/// 1. dropping the `write::remove` on the load failure — the file survives and
///    the `Load` row's `notes_exist` assertion fails. **Re-run 2026-09-04**
///    (LESSON-598) because the row's fixture changed underneath it: the refusal
///    is now reached through a boundary rather than the durable switch, and
///    deleting the `if !force { write::remove(..) }` still fails with `a file
///    the loader refuses is removed, not left for the next session`;
/// 2. treating `evidence.stop.is_some()` as a walk failure — the budget-stop leg
///    at the end fails;
/// 3. answering `Stage::Failed` with no `reason` — the reason assertion fails on
///    every row;
/// 4. mapping `WriteFailure::Io` to `Reason::AlreadyExists` — the write row's
///    typed-reason assertion fails and a permission problem would be reported to
///    the user as a file they do not have;
/// 5. swapping `stop_phrase` and `cut_phrase` at the `generated_header` call
///    site — the budget-stop leg's ordering assertion fails, which is the one
///    place a run produces both omissions at once.
///
/// **Two more rows, two more mutations**, both run 2026-09-04 and restored,
/// recorded as **observed**:
/// 6. deleting the `budget.evidence.max_bytes == 0` short-circuit at the top of
///    `run` — the narrow-window row fails with `left: NothingToDraft, right:
///    WindowTooNarrow { window_bytes: 12287 }`. That *is* the defect: a route
///    one byte under the reserve saturates the evidence budget to zero, the
///    assembly fits nothing, and the user is told their repository has nothing
///    in it when the remedy is a wider route;
/// 7. unlinking unconditionally on the load failure (`if !force` removed) —
///    the `--force` row fails at `a replacement the loader refuses is left
///    where it landed: NotFound`, which is the user with neither their file nor
///    Teton's.
#[tokio::test]
async fn every_stage_failure_is_typed_leaves_no_file_and_keeps_provider_health() {
    // --- non-vacuity: the same fixture really does write --------------------
    {
        let fx = Fixture::new("nonvacuous");
        let (_seen, route) = answers_well();
        let outcome = generate(&fx, Run::new(&route)).await;
        assert!(
            matches!(outcome, GenerationOutcome::Written(_)),
            "{outcome:?}"
        );
        assert!(fx.notes_exist());
    }

    // --- the walk found nothing to draft from ------------------------------
    {
        let fx = Fixture::new("nothing");
        let mut sub = fx.subscribe();
        let (seen, route) = answers_well();
        let outcome = generate(
            &fx,
            Run {
                // A route wide enough to have carried evidence, and a body of
                // one byte: not even the root's own line fits, so the assembly
                // is empty. One byte rather than zero because a zero body is
                // `WindowTooNarrow`'s fact — the route — and this leg is about
                // the repository.
                budget: body_budget(1),
                ..Run::new(&route)
            },
        )
        .await;
        assert_failed(&outcome, FailStage::Walk, &Reason::NothingToDraft);
        assert!(!fx.notes_exist(), "a failed walk leaves no file");
        assert!(
            seen.lock().unwrap().is_empty(),
            "nothing was routed and no model was called"
        );
        assert_failed_news(&mut sub, &fx);
    }

    // --- the route's window could not carry any evidence -------------------
    //
    // A different fact from the leg above and a different remedy: the walk was
    // never the problem. `evidence_budget_for` reserves the answer and the
    // drafting instruction out of the window first, so a route narrower than
    // that reserve leaves a body of zero — and a build that let that through
    // would walk the tree, assemble nothing out of a saturated budget, and tell
    // the user their repository has nothing in it.
    {
        let fx = Fixture::new("narrow-window");
        let mut sub = fx.subscribe();
        let (seen, route) = answers_well();
        let narrow = DRAFT_RESERVED_BYTES - 1;
        let outcome = generate(
            &fx,
            Run {
                budget: generate::evidence_budget_for(narrow),
                ..Run::new(&route)
            },
        )
        .await;
        assert_failed(
            &outcome,
            FailStage::Walk,
            &Reason::WindowTooNarrow {
                window_bytes: narrow,
            },
        );
        assert!(!fx.notes_exist(), "a route too narrow leaves no file");
        assert!(
            seen.lock().unwrap().is_empty(),
            "nothing was routed and no model was called"
        );
        let reason = assert_failed_news(&mut sub, &fx);
        assert!(
            reason.contains(&narrow.to_string()) && reason.contains("window"),
            "the news names the window rather than blaming the repository: {reason}"
        );
    }

    // --- the duty failed ---------------------------------------------------
    {
        let fx = Fixture::new("duty-error");
        let mut sub = fx.subscribe();
        let sentence = "The 'draft' category resolves to 'frontier', which is not a configured \
                        provider.";
        let seen: Seen = Arc::default();
        let handle = Arc::clone(&seen);
        let route = move || recording_route(&handle, Err(sentence.to_owned()));
        let outcome = generate(&fx, Run::new(&route)).await;
        assert_failed(
            &outcome,
            FailStage::Draft,
            &Reason::Duty(sentence.to_owned()),
        );
        assert!(!fx.notes_exist(), "a failed draft leaves no file");
        assert_eq!(
            seen.lock().unwrap().len(),
            1,
            "the duty was genuinely asked"
        );
        assert_failed_news(&mut sub, &fx);
    }

    // --- the model answered nothing ---------------------------------------
    {
        let fx = Fixture::new("empty-draft");
        let mut sub = fx.subscribe();
        let seen: Seen = Arc::default();
        let handle = Arc::clone(&seen);
        // Whitespace and nothing else: the file this would produce is a header
        // alone, which REQ-612 would load and put in every later prompt.
        let route = move || recording_route(&handle, Ok("   \n\t\n".to_owned()));
        let outcome = generate(&fx, Run::new(&route)).await;
        assert_failed(&outcome, FailStage::Draft, &Reason::EmptyDraft);
        assert_eq!(seen.lock().unwrap().len(), 1, "the duty did answer");
        assert!(!fx.notes_exist(), "an empty draft leaves no file");
        assert_failed_news(&mut sub, &fx);
    }

    // --- the choke point refused the call ---------------------------------
    //
    // The session's own matcher covers nothing, so the gatherer excludes nothing
    // and the evidence's provenance reaches egress intact; the *route's*
    // boundaries cover it. That is ADR-4's own case: a covered source that
    // slipped exclusion is refused at the choke point like any other duty's
    // content, and is not a provider fault.
    {
        let fx = Fixture::new("privacy");
        let mut sub = fx.subscribe();
        let route = || {
            remote_route(
                Answer::Says(GOOD_DRAFT.to_owned()),
                vec![PrivacyBoundary::user("README.md", BoundaryMode::LocalOnly)],
                None,
                (0, 0),
            )
        };
        let outcome = generate(&fx, Run::new(&route)).await;
        let GenerationOutcome::Failed { stage, reason } = &outcome else {
            panic!("a refused call is a failure: {outcome:?}");
        };
        assert_eq!(*stage, FailStage::Draft);
        assert!(
            matches!(reason, Reason::Duty(_)),
            "a privacy block is the duty's own refusal, not a second path: {reason:?}"
        );
        assert!(!fx.notes_exist(), "a refused draft leaves no file");
        assert_failed_news(&mut sub, &fx);
    }

    // --- the prompt did not fit the window --------------------------------
    {
        let fx = Fixture::new("over-window");
        let mut sub = fx.subscribe();
        let route = || remote_route(Answer::OverWindow, Vec::new(), None, (0, 0));
        let outcome = generate(&fx, Run::new(&route)).await;
        let GenerationOutcome::Failed { stage, reason } = &outcome else {
            panic!("an over-window request is a failure: {outcome:?}");
        };
        assert_eq!(*stage, FailStage::Draft);
        assert!(
            matches!(reason, Reason::Duty(sentence) if sentence.contains("context window")),
            "{reason:?}"
        );
        assert!(!fx.notes_exist(), "an over-window draft leaves no file");
        assert_failed_news(&mut sub, &fx);
    }

    // --- the write failed --------------------------------------------------
    {
        let fx = Fixture::new("write-error");
        let mut sub = fx.subscribe();
        let (_seen, route) = answers_well();
        // `r-x`: the walk and the reads still work, and `create_new` does not.
        set_dir_mode(&fx.dir, 0o555);
        let outcome = generate(&fx, Run::new(&route)).await;
        set_dir_mode(&fx.dir, 0o755);
        assert_failed(
            &outcome,
            FailStage::Write,
            &Reason::Io(ErrorKind::PermissionDenied),
        );
        assert!(!fx.notes_exist(), "a failed write leaves no file");
        assert_failed_news(&mut sub, &fx);
    }

    // --- the loader would not read it back --------------------------------
    //
    // The file is written whole and successfully, and one of the session's own
    // privacy boundaries covers `TETON.md` — so REQ-612's loader answers
    // `WithheldBoundary` and the pipeline unlinks what it wrote. This is the one
    // failure `write.rs` cannot see, and the one AC-9 is most about: a file left
    // here is a file the next session loads as if the repository had authored
    // it.
    //
    // **A boundary rather than the durable switch**, which is what this leg used
    // to flip. `[context] repo_file = false` no longer reaches the pipeline at
    // all — `offer_and_run` refuses in front of the walk (`Suppression::
    // SwitchedOff`), which is what stops a switched-off machine paying for a
    // model call it will unlink — so a fixture built on it would be asserting
    // about a state the daemon cannot be in. The boundary reaches the same
    // load-back refusal through a mechanism that is still live.
    let covers_the_notes = vec![PrivacyBoundary::user("TETON.md", BoundaryMode::LocalOnly)];
    {
        let fx = Fixture::new("not-loaded");
        let mut sub = fx.subscribe();
        let (_seen, route) = answers_well();
        let outcome = generate(
            &fx,
            Run {
                boundaries: &covers_the_notes,
                ..Run::new(&route)
            },
        )
        .await;
        assert_failed(
            &outcome,
            FailStage::Load,
            &Reason::NotLoaded {
                kind: RepoContextStateKind::WithheldBoundary,
                left_in_place: false,
            },
        );
        assert!(
            !fx.notes_exist(),
            "a file the loader refuses is removed, not left for the next session"
        );
        assert_failed_news(&mut sub, &fx);
    }
    // --- …and on the `--force` door the replacement stays -------------------
    //
    // The one exception to "a failure leaves no file", and it is not a
    // softening: `write::replace` renames over the file it was given permission
    // to replace, so the old bytes are gone before the loader is asked. Unlinking
    // here would answer a *read* failure by destroying both files — the user
    // would be left with neither their notes nor Teton's. The run still fails,
    // and the reason says the replacement is where it landed.
    {
        let fx = Fixture::new("not-loaded-force");
        let mut sub = fx.subscribe();
        let (_seen, route) = answers_well();
        std::fs::write(fx.notes(), "# hand written\n").unwrap();
        let outcome = generate(
            &fx,
            Run {
                boundaries: &covers_the_notes,
                force: true,
                ..Run::new(&route)
            },
        )
        .await;
        assert_failed(
            &outcome,
            FailStage::Load,
            &Reason::NotLoaded {
                kind: RepoContextStateKind::WithheldBoundary,
                left_in_place: true,
            },
        );
        let on_disk = std::fs::read_to_string(fx.notes())
            .expect("a replacement the loader refuses is left where it landed");
        assert!(
            on_disk.starts_with("> Generated by Teton on ") && on_disk.contains("## Purpose"),
            "the file left in place is the new body, whole: {on_disk}"
        );
        let reason = assert_failed_news(&mut sub, &fx);
        assert!(
            reason.contains("left in place"),
            "the news says what became of the bytes, or a user reading `failed` \
             deletes a file they still have: {reason}"
        );
    }

    // --- and a budget stop with a usable tree is NOT a failure -------------
    {
        let fx = Fixture::new("walk-stop");
        let mut sub = fx.subscribe();
        let (seen, route) = answers_well();
        let outcome = generate(
            &fx,
            Run {
                // Two entries of a five-entry tree: the walk stops, and what it
                // handed over is still a listing. The byte budget is set just
                // past that listing, so this one run carries **both** omissions
                // and their order in the header is observable.
                walk: WalkBudget {
                    max_entries: 2,
                    ..WalkBudget::default()
                },
                budget: body_budget(120),
                ..Run::new(&route)
            },
        )
        .await;
        assert!(
            matches!(outcome, GenerationOutcome::Written(_)),
            "a partial listing is still a listing (BR-9): {outcome:?}"
        );
        assert!(fx.notes_exist());
        let header = std::fs::read_to_string(fx.notes()).unwrap();
        let header = header.lines().next().unwrap().to_owned();
        let stop_at = header
            .find("walk stopped at")
            .unwrap_or_else(|| panic!("the stop is never swallowed: {header}"));
        let cut_at = header
            .find("manifests left out")
            .unwrap_or_else(|| panic!("the cut is never swallowed: {header}"));
        assert!(
            stop_at < cut_at,
            "the header states the omissions in the order they happened: {header}"
        );
        let prompt = seen.lock().unwrap()[0].0.clone();
        assert!(
            prompt.contains("the listing is partial"),
            "the model is told the listing is partial: {prompt}"
        );
        assert_eq!(
            names(&stages(&mut sub)),
            vec![Stage::Walking, Stage::Drafted, Stage::Written]
        );
    }
}

/// The typed answer a failed run must give: the stage, the facts, and nothing
/// else.
fn assert_failed(outcome: &GenerationOutcome, stage: FailStage, reason: &Reason) {
    let GenerationOutcome::Failed {
        stage: got_stage,
        reason: got_reason,
    } = outcome
    else {
        panic!("expected a failure at {stage:?}, got {outcome:?}");
    };
    assert_eq!(*got_stage, stage);
    assert_eq!(got_reason, reason);
    assert!(outcome.generated().is_none());
    assert_eq!(outcome.wire(), Stage::Failed);
}

/// The news a failed run must publish: a `failed` stage with words on it, and no
/// provider-health event anywhere on the bus.
///
/// Answers with those words, for the one leg that has something further to say
/// about them — the rest ignore the return.
fn assert_failed_news(sub: &mut Subscription, fx: &Fixture) -> String {
    let events = published(sub);
    assert!(
        !events
            .iter()
            .any(|event| matches!(event, Event::ProviderDegraded(_))),
        "a failing draft must not move the provider's health (BR-9)"
    );
    let stages: Vec<&RepoContextGeneration> = events
        .iter()
        .filter_map(|event| match event {
            Event::RepoContextGeneration(news) => Some(news),
            _ => None,
        })
        .collect();
    let last = stages.last().expect("a run publishes at least one stage");
    assert_eq!(last.outcome, Stage::Failed, "{stages:?}");
    let reason = last.reason.as_deref().unwrap_or_default();
    assert!(!reason.trim().is_empty(), "a failure says why: {stages:?}");
    assert!(
        reason.chars().count() <= 200,
        "the reason is bounded on the way to the wire: {reason}"
    );
    assert_eq!(last.root, fx.root.view.display);
    reason.to_owned()
}

// ===========================================================================
// Both doors — `force` replaces, and without it nothing is clobbered
// ===========================================================================

/// **Both doors (BR-6, BR-8).** `run` with `force` replaces an existing file;
/// without it an existing `TETON.md` is `Failed { AlreadyExists }` and its bytes
/// are untouched.
///
/// The no-clobber answer is decided at the *write*, which is deliberate: it is
/// also AC-8's race — a `TETON.md` created between consent and the write — and a
/// pre-check would answer the easy case while leaving the race to clobber.
///
/// **Mutation**, run 2026-09-03 and restored: calling `write::replace`
/// unconditionally makes the first leg overwrite the planted file and both of its
/// assertions fail.
#[tokio::test]
async fn force_replaces_and_without_it_an_existing_file_is_refused_untouched() {
    let fx = Fixture::new("doors");
    let (_seen, route) = answers_well();
    std::fs::write(fx.notes(), "# hand written\n").unwrap();

    let outcome = generate(&fx, Run::new(&route)).await;
    assert_failed(&outcome, FailStage::Write, &Reason::AlreadyExists);
    assert_eq!(
        std::fs::read_to_string(fx.notes()).unwrap(),
        "# hand written\n",
        "a refused write changes nothing"
    );

    let outcome = generate(
        &fx,
        Run {
            force: true,
            ..Run::new(&route)
        },
    )
    .await;
    let GenerationOutcome::Replaced(made) = &outcome else {
        panic!("--force replaces: {outcome:?}");
    };
    assert_eq!(outcome.wire(), Stage::Replaced);
    let on_disk = std::fs::read_to_string(fx.notes()).unwrap();
    assert!(on_disk.starts_with("> Generated by Teton on "), "{on_disk}");
    assert!(on_disk.contains("## Purpose"), "{on_disk}");
    assert!(matches!(made.state, RepoContextState::Loaded(_)));
    // No scratch file survives the rename.
    let leftovers: Vec<PathBuf> = std::fs::read_dir(&fx.dir)
        .unwrap()
        .filter_map(|entry| entry.ok().map(|entry| entry.path()))
        .filter(|path| path.to_string_lossy().ends_with(".tmp"))
        .collect();
    assert!(leftovers.is_empty(), "{leftovers:?}");
}

// ===========================================================================
// The offer's own short-circuits (ADR-2), over the pipeline's fixture
// ===========================================================================

/// `offer_and_run` over a root of the caller's choosing, at a level that
/// settles without asking.
///
/// `full` rather than a scripted client, because neither leg below is about the
/// prompt: one is refused in front of the gate, and the other has to reach the
/// write. A gate at `full` answers `ByLevel(Allowed)` before any delivery is
/// attempted, so the only open question left in the run is the one the leg is
/// about — and a gate that stopped allowing would fail loudly rather than
/// quietly writing nothing.
async fn offer_at(
    fx: &Fixture,
    root: &ProbedRoot,
    run: Run<'_>,
    mode: GenerateMode,
    explicit: bool,
) -> OfferOutcome {
    let matcher = BoundaryMatcher::new(run.boundaries).expect("the fixture globs compile");
    let gate = PermissionGate::with_level(
        SessionId::from(SESSION),
        PermissionLevel::Full,
        Vec::new(),
        Arc::clone(&fx.bus),
        Arc::new(PendingPermissions::new()),
    );
    generate::offer_and_run(Offer {
        ctx: GenerationContext {
            root,
            reader: &RealFiles,
            boundaries: &matcher,
            budget: run.budget,
            walk: run.walk,
            route: run.route,
            events: &fx.events,
            tier: Tier::Think,
            config: &fx.config,
        },
        gate: &gate,
        // Someone could be asked — so a refusal below is the short-circuit's,
        // never `RefusedUnattended` standing in for it.
        addressee: Some(GrantRegistry::new().next_connection_id()),
        mode,
        explicit,
        force: run.force,
    })
    .await
}

/// A route that counts every time it is **resolved**, not merely every time it
/// answers.
///
/// The distinction is the whole of the claim below: a short-circuit that ran the
/// walk and then declined to draft would leave a `Recording`'s own counter at
/// zero and look identical to one that refused in front. What must not happen is
/// that a route is built at all.
fn counted_route(seen: &Seen, resolved: &Arc<AtomicU64>) -> impl Fn() -> DutyRoute + Send + Sync {
    let seen = Arc::clone(seen);
    let resolved = Arc::clone(resolved);
    move || {
        resolved.fetch_add(1, Ordering::SeqCst);
        recording_route(&seen, Ok(GOOD_DRAFT.to_owned()))
    }
}

/// **REQ-612 BR-2 / REQ-613 BR-7.** A machine with `[context] repo_file = false`
/// writes nothing and spends nothing — including through `/context init`, the
/// explicit door — and says which key answered.
///
/// # Why the explicit door does not outrank this one
///
/// `[context] generate = never` is a setting about the *offer*, so a user who
/// typed the command has said the thing it exists to stop Teton assuming. The
/// durable switch says the notes file may not be **opened** on this machine, and
/// the pipeline reads back what it writes (BR-7) under that very key: a run
/// under it walks the tree, spends a frontier model call, writes the file, is
/// refused by its own loader and unlinks it — ending with no file, no notes and
/// one billed call. Refusing in front is the same outcome for none of the cost,
/// which is what `generate_repo_context`'s doc already promised ("a machine that
/// says the file may not be opened refuses the write outright").
///
/// # Non-vacuity (LESSON-485)
///
/// "No file" is satisfied by a pipeline that never writes anything, so the leg
/// at the top is the same fixture, the same root and the same flags with the
/// switch **on**, and it really does write.
///
/// **Mutation** (LESSON-441), run 2026-09-04 and restored, recorded as
/// **observed**: replacing the short-circuit's condition with `false` — the
/// build this finding was raised against — gives `Ran(Failed { stage: Load,
/// reason: NotLoaded { kind: WithheldOff, left_in_place: false } })`. Every
/// clause of the finding is in that one line: the route was resolved, the tree
/// was walked, a frontier model call was spent, `TETON.md` was written, the
/// loader refused it under the very switch that was set, and the file was
/// unlinked — a machine that had said "never open this file" paying full price
/// to end where it started.
#[tokio::test]
async fn the_durable_switch_refuses_in_front_of_the_walk_even_for_an_explicit_init() {
    // --- non-vacuity: switched on, the same call writes ---------------------
    {
        let fx = Fixture::new("switch-on");
        let seen: Seen = Arc::default();
        let resolved = Arc::new(AtomicU64::new(0));
        let route = counted_route(&seen, &resolved);
        let outcome = offer_at(&fx, &fx.root, Run::new(&route), GenerateMode::Ask, true).await;
        assert!(
            matches!(outcome, OfferOutcome::Ran(GenerationOutcome::Written(_))),
            "{outcome:?}"
        );
        assert!(fx.notes_exist());
        assert_eq!(resolved.load(Ordering::SeqCst), 1, "one run is one route");
    }

    // --- switched off: nothing is resolved, walked, called or written -------
    let mut fx = Fixture::new("switch-off");
    fx.config.context.repo_file = false;
    let mut sub = fx.subscribe();
    let seen: Seen = Arc::default();
    let resolved = Arc::new(AtomicU64::new(0));
    let route = counted_route(&seen, &resolved);

    let outcome = offer_at(&fx, &fx.root, Run::new(&route), GenerateMode::Ask, true).await;
    assert!(
        matches!(outcome, OfferOutcome::Suppressed(Suppression::SwitchedOff)),
        "the durable switch is settled in front of the pipeline: {outcome:?}"
    );
    assert_eq!(
        resolved.load(Ordering::SeqCst),
        0,
        "no draft route was even resolved"
    );
    assert!(
        seen.lock().unwrap().is_empty(),
        "and no model was called: the whole point is the call that is not billed"
    );
    assert!(
        !fx.notes_exist(),
        "nothing was written, and nothing unlinked"
    );

    let heard = stages(&mut sub);
    assert_eq!(
        names(&heard),
        vec![Stage::Suppressed],
        "one line, and it is a suppression rather than a failure: nothing ran"
    );
    let reason = heard[0].reason.as_deref().unwrap_or_default();
    assert!(
        reason.contains("[context] repo_file = false") && reason.contains("teton context enable"),
        "the line names the key that answered and the way back: {reason}"
    );
}

/// **S5.** The notes land in the **canonical** directory — the one the consent
/// key was minted from — when the session's root is reached through a symlink.
///
/// The key is `repo_context:generate:<canonical root>`, so an answer given
/// about `/tmp/link` and an answer given about the directory it points at are
/// one remembered answer. A write that then used the session's *spelling* would
/// be honouring that answer in a directory it was never about — harmless while
/// the two resolve to one place, and a file in the wrong repository the day they
/// do not.
///
/// Both spellings show the file afterwards, which is why the assertion is on the
/// **path the run reports** and on `canonicalize` rather than on existence: a
/// build that wrote through the symlink puts the same bytes in the same place
/// and tells the caller a path that resolves somewhere it did not write from.
///
/// **Mutation** (LESSON-441), run 2026-09-04 and restored: writing to
/// `ctx.root.path` — the spelling this finding was raised against — fails here
/// with `left: …-symlinked-34731-0-link/TETON.md, right:
/// …-symlinked-34731-0/TETON.md`, the two directories one answer was given
/// about.
#[tokio::test]
async fn a_root_reached_through_a_symlink_writes_into_the_canonical_directory() {
    let fx = Fixture::new("symlinked");
    let link = fx.dir.with_file_name(format!(
        "{}-link",
        fx.dir.file_name().unwrap().to_string_lossy()
    ));
    std::os::unix::fs::symlink(&fx.dir, &link).expect("plant a symlinked spelling of the root");
    // Non-vacuity for the assertion below: the two spellings really are
    // different strings, and really are one directory.
    assert_ne!(link, fx.dir);
    assert_eq!(
        std::fs::canonicalize(&link).unwrap(),
        fx.dir,
        "the symlink resolves to the fixture's own root"
    );

    let spelled = ProbedRoot {
        path: link.clone(),
        view: fx.root.view.clone(),
    };
    let (_seen, route) = answers_well();
    let outcome = offer_at(&fx, &spelled, Run::new(&route), GenerateMode::Ask, false).await;

    let made = outcome
        .generated()
        .unwrap_or_else(|| panic!("the run must write, or the path claim is vacuous: {outcome:?}"));
    assert_eq!(
        made.path,
        fx.dir.join("TETON.md"),
        "the file is written into the directory the consent key was minted from, \
         not the one the session spells"
    );
    assert!(
        fx.notes_exist(),
        "and it really is there under the canonical spelling"
    );

    std::fs::remove_file(&link).ok();
}

// ===========================================================================
// BR-6 / AC-8 — the cap, and the file that appears after consent
// ===========================================================================

/// A duty that plants a `TETON.md` at the root and *then* answers well.
///
/// The one instrument that can produce AC-8's race. `perform` runs strictly
/// after consent (the caller holds the witness), strictly after the walk, and
/// strictly before the write — which is exactly the window in which a checkout,
/// another session, or the user's own editor can put a file where this run is
/// about to put one. Nothing outside the seam can plant a file in that window
/// without a second thread and a sleep.
struct RacesTheWrite {
    notes: PathBuf,
    contents: &'static str,
}

#[async_trait]
impl Duty for RacesTheWrite {
    fn category(&self) -> Category {
        DRAFT_DUTY.category()
    }

    fn ceiling_bytes(&self) -> usize {
        DRAFT_DUTY.ceiling_bytes()
    }

    async fn perform(&self, _prompt: &str, _provenance: &Provenance) -> Result<String, String> {
        std::fs::write(&self.notes, self.contents).expect("the racing writer plants its file");
        Ok(GOOD_DRAFT.to_owned())
    }
}

/// An answer of exactly `REPO_CONTEXT_MAX_BYTES + 2,000` bytes: the five
/// sections, then one-byte lines to the target length.
///
/// **The padding is one byte wide on purpose.** The bounding cut keeps whole
/// lines, so a wider line would land the file at "the last line boundary at or
/// under the cap" — a number that depends on the header this build happens to
/// compose, and computing it here would be letting the subject state its own
/// expectation (LESSON-569). With every padding byte a newline there is a line
/// boundary at every offset, so the file lands on the cap **exactly**, whatever
/// the header's length turns out to be, and the assertion can be the flat
/// constant REQ-612 published.
fn answer_of_cap_plus_2000() -> String {
    let mut answer = GOOD_DRAFT.to_owned();
    let target = REPO_CONTEXT_MAX_BYTES + 2_000;
    assert!(
        answer.len() < target,
        "the fixture prefix is already too long"
    );
    while answer.len() < target {
        answer.push('\n');
    }
    assert_eq!(answer.len(), target);
    answer
}

/// **BR-6, AC-8.** An answer of the cap plus 2,000 bytes is written at
/// **exactly** REQ-612's cap — the header counted inside it — and the loader
/// reports `Loaded` rather than `Truncated`; and a `TETON.md` that appears
/// between consent and the write stops the write with
/// `Failed { AlreadyExists }`, one line, and the racing file untouched.
///
/// # Why the two halves are one test
///
/// They are the same claim about the same act, taken from its two ends. The
/// cap half says what the write puts on disk when it happens; the race half
/// says when it does not happen at all. Split apart, each is satisfiable by a
/// build the other rejects — a pipeline that never writes passes the race half,
/// and one that clobbers passes the cap half — and the cap half is what makes
/// the race half non-vacuous, because it writes a real file over the same
/// fixture shape a few lines earlier (LESSON-485).
///
/// # The race is decided at the write, not by a pre-check
///
/// Nothing here plants the file before `run` is called: `RacesTheWrite` plants
/// it from *inside* the duty, which is the window BR-6 is about. A build that
/// answered the no-clobber question by `stat`ing the root before the walk would
/// pass `force_replaces_and_without_it_an_existing_file_is_refused_untouched`
/// and clobber here.
///
/// **Mutations** (LESSON-441), both run 2026-09-03 and restored, recorded as
/// **observed** — and the first is why they are recorded that way:
/// 1. `bound_answer` charging the cap without the header
///    (`DRAFT_OUTPUT_MAX_BYTES.saturating_sub(0)`). The prediction was "the
///    file is header + cap and the length assertion fails"; what actually
///    happens is `Failed { stage: Load, reason: NotLoaded(Truncated) }` — the
///    over-cap file is written, REQ-612's loader answers `Truncated`, and `run`
///    unlinks it. So the assertion that fires is the outcome one, *an over-long
///    answer is bounded, not refused*, and the visible damage is a repository
///    that ends a generation with no notes at all. That the failure lands there
///    rather than on the byte count is the reason the outcome is asserted
///    before the length;
/// 2. `run` calling `write::replace` unconditionally in place of
///    `write::write_new` — the race leg gives `expected a failure at Write, got
///    Written(..)`, and the file that won the race is gone.
/// Mutation run 2026-09-03 (orchestrator, after two agent stalls): `if force` → `if true` at
/// `generate.rs:432` (the replace path taken unconditionally) — the race leg fails at the
/// `Failed { AlreadyExists }` assertion because the file created between consent and write is
/// replaced. Restored byte-identically.
#[tokio::test]
async fn a_file_created_between_consent_and_write_stops_the_write_and_a_long_answer_lands_at_the_cap(
) {
    // --- the cap: an answer 2,000 bytes over it lands exactly on it ---------
    {
        let fx = Fixture::new("at-the-cap");
        let seen: Seen = Arc::default();
        let handle = Arc::clone(&seen);
        let answer = answer_of_cap_plus_2000();
        let route = move || recording_route(&handle, Ok(answer.clone()));

        let outcome = generate(&fx, Run::new(&route)).await;
        let GenerationOutcome::Written(made) = &outcome else {
            panic!("an over-long answer is bounded, not refused: {outcome:?}");
        };
        assert_eq!(seen.lock().unwrap().len(), 1, "one draft is one model call");

        let on_disk = std::fs::read_to_string(fx.notes()).expect("the file is there");
        assert_eq!(
            on_disk.len(),
            REPO_CONTEXT_MAX_BYTES,
            "the written file lands exactly on REQ-612's cap, header included (AC-8)"
        );
        assert_eq!(made.bytes, REPO_CONTEXT_MAX_BYTES, "and it reports that");
        let first = on_disk.lines().next().expect("a non-empty file");
        assert!(
            first.starts_with("> Generated by Teton on ") && first.contains("(think tier)"),
            "the header is still the first line of a file cut to the cap: {first}"
        );
        assert!(
            made.draft_bytes < REPO_CONTEXT_MAX_BYTES,
            "the header is charged inside the cap, so the draft's share is smaller: {}",
            made.draft_bytes
        );

        // --- and the loader takes it whole ---------------------------------
        let RepoContextState::Loaded(file) = &made.state else {
            panic!(
                "a file bounded to the cap must load whole, not {:?} (AC-8)",
                made.state.kind()
            );
        };
        assert_eq!(file.text.len(), REPO_CONTEXT_MAX_BYTES);
        let block = RepoContextBlock::render(file, REPO_CONTEXT_MAX_BYTES);
        assert!(
            !block.truncated,
            "`loaded`, not `truncated`: a generated file is bounded before it is written"
        );
        assert!(block.text.contains(first), "the header is resident too");
    }

    // --- the race: a file that appears after consent stops the write --------
    {
        let fx = Fixture::new("race");
        let mut sub = fx.subscribe();
        let racer = "# Won the race\n\nWritten by someone else while Teton was drafting.\n";
        let notes = fx.notes();
        let route = move || DutyRoute::Serves {
            provider_id: "fake-think".to_owned(),
            duty: Arc::new(RacesTheWrite {
                notes: notes.clone(),
                contents: racer,
            }),
            announce: None,
        };

        assert!(!fx.notes_exist(), "the race starts with an empty root");
        let outcome = generate(&fx, Run::new(&route)).await;
        assert_failed(&outcome, FailStage::Write, &Reason::AlreadyExists);
        assert_eq!(
            std::fs::read_to_string(fx.notes()).unwrap(),
            racer,
            "the file that won the race is not clobbered (BR-6)"
        );

        // --- one line, and nothing else ------------------------------------
        let events = published(&mut sub);
        assert!(
            !events
                .iter()
                .any(|event| matches!(event, Event::ProviderDegraded(_))),
            "losing a race is not the provider's fault (BR-9)"
        );
        let heard: Vec<RepoContextGeneration> = events
            .into_iter()
            .filter_map(|event| match event {
                Event::RepoContextGeneration(news) => Some(news),
                _ => None,
            })
            .collect();
        assert_eq!(
            names(&heard),
            vec![Stage::Walking, Stage::Drafted, Stage::Failed],
            "the run got as far as the draft and stopped at the write"
        );
        let failed = heard.last().expect("a run publishes its last stage");
        assert_eq!(
            failed.reason.as_deref(),
            Some("a TETON.md is already there"),
            "one line, and it names the fact rather than an errno"
        );
        assert_eq!(
            heard
                .iter()
                .filter(|stage| stage.outcome == Stage::Failed)
                .count(),
            1,
            "one line, not two: {heard:?}"
        );
    }
}

// ===========================================================================
// BR-3 / AC-4 — the real walker, over a real deep tree
// ===========================================================================

/// Plant a six-level tree under `dir` and answer how many entries it adds.
///
/// Six levels because that is the depth at which a listing stops being a
/// formality: `crates/tetond/src/harness/tools/walk.rs` is five, and a
/// `src/main/java/com/example/app` layout is six. A walker that quietly bounded
/// its depth would still list every fixture in this file correctly.
fn plant_six_levels(dir: &Path) -> usize {
    let leaf = dir.join("d1/d2/d3/d4/d5/d6");
    std::fs::create_dir_all(&leaf).unwrap();
    // Two extensions in one directory, so the per-directory profile has
    // something to order; the rest are one file per level, so the count is
    // arithmetic rather than a guess.
    std::fs::write(dir.join("d1/a.rs"), "pub fn a() {}\n").unwrap();
    std::fs::write(dir.join("d1/a.md"), "# a\n").unwrap();
    std::fs::write(dir.join("d1/d2/b.rs"), "pub fn b() {}\n").unwrap();
    std::fs::write(dir.join("d1/d2/d3/c.py"), "def c(): pass\n").unwrap();
    std::fs::write(dir.join("d1/d2/d3/d4/d.py"), "def d(): pass\n").unwrap();
    std::fs::write(dir.join("d1/d2/d3/d4/d5/e.md"), "# e\n").unwrap();
    std::fs::write(leaf.join("leaf.rs"), "pub fn leaf() {}\n").unwrap();
    // Six directories and seven files.
    13
}

/// **BR-3, AC-4.** The production walker, over a real six-level tree in a real
/// temporary directory, lists it **to its leaves** with the per-directory
/// extension profile — and the same gatherer under an injected [`WalkBudget`]
/// stops, says so in the evidence it hands the model, and says so again in the
/// header of the file that gets written.
///
/// # Why a real directory and the real walker
///
/// Every other walk claim in this REQ is made against a planted listing or the
/// three-file fixture at the top of this file, and neither can fail the way a
/// real walk can: depth is where a recursive walker breaks, and a fixture whose
/// deepest path is `src/main.rs` cannot tell a walker that descends from one
/// that lists two levels and stops. So this one plants six levels on the real
/// filesystem and asserts the **leaf's own line**, profile included.
///
/// # The stop is injected, not provoked
///
/// AC-4's budget-stop half is reached through
/// [`evidence::gather_with_walk_budget`] rather than by planting 100,001 files:
/// the seam exists for exactly this, and a test that planted the real budget's
/// worth of entries would trade ten seconds of CI for the same assertion. The
/// pipeline leg beneath it re-runs the whole of `run` under the same budget,
/// because "the stop is stated" is a claim about the **file's header** and the
/// gatherer cannot make it.
///
/// **Mutations** (LESSON-441), both run 2026-09-03 and restored, recorded as
/// **observed**:
/// 1. `gather_with_walk_budget` building its jail with `WalkBudget::default()`
///    instead of the budget it was handed — the injected-budget leg fails with
///    `left: None, right: Some(Entries(5))`, and AC-4's stop becomes untestable
///    without a hundred thousand files;
/// 2. `tree_section` rendering at `Some(2)` instead of the assembly's own depth
///    — the sixth level's line disappears (the body prints `.`, `d1/` and
///    `src/` and stops), which is the difference between a listing and a
///    summary.
/// Mutation run 2026-09-03 (orchestrator): `stop_phrase(evidence.stop).as_deref()` → `None` at
/// `generate.rs:412` (the walk stop not written into the header) — fails at the header assertion.
/// Restored byte-identically.
#[tokio::test]
async fn the_real_walker_lists_a_deep_tempdir_and_stops_at_an_injected_budget() {
    let fx = Fixture::new("deep");
    let planted = plant_six_levels(&fx.dir);
    // The fixture's own three files and its `src/` directory.
    let entries = planted + 4;
    let open: [PrivacyBoundary; 0] = [];
    let matcher = BoundaryMatcher::new(&open).expect("an empty set compiles");
    let roomy = EvidenceBudget::new(64 * 1024);

    // --- listed to its leaves ----------------------------------------------
    let whole = evidence::gather(&fx.root, &RealFiles, &matcher, roomy);
    assert_eq!(whole.stop, None, "the default budget walks this tree whole");
    assert_eq!(whole.cut, None, "and 64 KiB is room for its listing");
    assert_eq!(
        whole.entries, entries,
        "every planted entry was handed over: {}",
        whole.body
    );
    assert!(
        whole.body.contains("d1/d2/d3/d4/d5/d6/ — 1 file (.rs 1)"),
        "the sixth level is listed with its own profile: {}",
        whole.body
    );
    assert!(
        whole.body.contains("d1/ — 2 files (.md 1, .rs 1)"),
        "a directory's line counts its files by extension: {}",
        whole.body
    );

    // --- and the same gatherer stops on an injected budget ------------------
    let budget = WalkBudget {
        max_entries: 5,
        ..WalkBudget::default()
    };
    let stopped = evidence::gather_with_walk_budget(&fx.root, &RealFiles, &matcher, roomy, budget);
    assert_eq!(
        stopped.stop,
        Some(WalkStop::Entries(5)),
        "the injected budget is the one the walk obeys (AC-4)"
    );
    assert_eq!(stopped.entries, 5, "and it stopped at it, not past it");
    assert!(
        stopped.body.contains("the listing is partial"),
        "the model is told what it is looking at: {}",
        stopped.body
    );

    // --- the stop reaches the file's header --------------------------------
    let (_seen, route) = answers_well();
    let outcome = generate(
        &fx,
        Run {
            walk: budget,
            ..Run::new(&route)
        },
    )
    .await;
    assert!(
        matches!(outcome, GenerationOutcome::Written(_)),
        "a partial listing is still a listing (BR-9): {outcome:?}"
    );
    let on_disk = std::fs::read_to_string(fx.notes()).unwrap();
    let header = on_disk.lines().next().expect("a non-empty file").to_owned();
    assert!(
        header.contains("walk stopped at 5 entries"),
        "the budget stop is written into the header, never swallowed (BR-3): {header}"
    );
    assert!(
        !header.contains("cut at depth"),
        "and nothing else was omitted, so the header claims nothing else: {header}"
    );
}

// ===========================================================================
// TASK-386 — the offer in front of the pipeline, from inside a real turn
// ===========================================================================
//
// Everything above drives `generate::run` with consent already in hand. What
// follows drives the **daemon**: `DaemonRuntime::run_prompt_turn` and
// `session_context`, over a real socket to a mock vendor, with a real
// `PermissionGate` answered by a scripted client.
//
// That is the only instrument that can settle BR-1 and BR-2. "The offer is
// raised once per session per root, on the first prompt turn" is a claim about
// *when* the daemon asks, and a test that called `offer_and_run` directly would
// pass with the hook deleted from `assemble_harness` — which is the whole of
// what this task wired.

/// A planted project root: a marker so it probes as `project`, and the evidence
/// classes a draft is written from.
struct Project {
    root: PathBuf,
}

impl Project {
    fn new(tag: &str) -> Self {
        static SEQ: AtomicU64 = AtomicU64::new(0);
        let root = PathBuf::from("/tmp").join(format!(
            "tg{tag}{:x}{:x}",
            std::process::id() & 0xffff,
            SEQ.fetch_add(1, Ordering::SeqCst)
        ));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(root.join("src")).unwrap();
        std::fs::write(root.join("Cargo.toml"), "[package]\nname = \"planted\"\n").unwrap();
        std::fs::write(root.join("README.md"), "# Planted\n\nA fixture.\n").unwrap();
        std::fs::write(root.join("src/main.rs"), "fn main() {}\n").unwrap();
        Self { root }
    }

    fn notes(&self) -> PathBuf {
        self.root.join("TETON.md")
    }

    fn notes_exist(&self) -> bool {
        std::fs::symlink_metadata(self.notes()).is_ok()
    }

    fn write(&self, rel: &str, contents: &str) {
        std::fs::write(self.root.join(rel), contents).unwrap();
    }
}

impl Drop for Project {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.root);
    }
}

/// A single-threaded mock OpenAI-compatible vendor on a real socket.
///
/// Real rather than a `Transport` double, for `repo_context.rs`'s reason: the
/// claim is about the bytes a **turn** put on the wire, and the system prompt is
/// assembled several layers above any seam a double could stand in for. A copy
/// of the shape three other integration binaries carry, because integration test
/// binaries share nothing.
struct Vendor {
    endpoint: String,
    bodies: Arc<Mutex<Vec<String>>>,
    script: Arc<Mutex<std::collections::VecDeque<String>>>,
}

impl Vendor {
    fn start() -> Self {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind a mock vendor");
        let addr = listener.local_addr().expect("mock vendor address");
        let bodies: Arc<Mutex<Vec<String>>> = Arc::default();
        let script: Arc<Mutex<std::collections::VecDeque<String>>> = Arc::default();
        let captured = Arc::clone(&bodies);
        let scripted = Arc::clone(&script);
        std::thread::spawn(move || {
            use std::io::{Read as _, Write as _};
            for stream in listener.incoming() {
                let Ok(mut stream) = stream else { break };
                // Read by the request's own framing, never by a heuristic: a
                // short read is legal at any point in a stream (LESSON-540).
                let mut raw = Vec::new();
                let mut buf = [0u8; 65_536];
                let mut want: Option<usize> = None;
                while let Ok(read) = stream.read(&mut buf) {
                    if read == 0 {
                        break;
                    }
                    raw.extend_from_slice(&buf[..read]);
                    if want.is_none() {
                        if let Some(end) = raw.windows(4).position(|w| w == b"\r\n\r\n") {
                            let head = String::from_utf8_lossy(&raw[..end]).to_ascii_lowercase();
                            let len = head
                                .lines()
                                .find_map(|line| line.strip_prefix("content-length:"))
                                .and_then(|value| value.trim().parse::<usize>().ok())
                                .unwrap_or(0);
                            want = Some(end + 4 + len);
                        }
                    }
                    if want.is_some_and(|total| raw.len() >= total) {
                        break;
                    }
                }
                captured
                    .lock()
                    .unwrap()
                    .push(String::from_utf8_lossy(&raw).into_owned());
                let body = scripted
                    .lock()
                    .unwrap()
                    .pop_front()
                    .unwrap_or_else(|| openai_turn(GOOD_DRAFT, None));
                let response = format!(
                    "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\n\
                     Content-Length: {}\r\nConnection: close\r\n\r\n{body}",
                    body.len()
                );
                let _ = stream.write_all(response.as_bytes());
                let _ = stream.flush();
            }
        });
        Self {
            endpoint: format!("http://{addr}/v1/chat/completions"),
            bodies,
            script,
        }
    }

    /// Queue one reply. The queue is consumed in order, so a fixture that
    /// expects a draft queues the draft first: the offer runs inside `assemble`,
    /// ahead of the turn's own call.
    fn will_answer(&self, content: &str, tool: Option<(&str, &str, &str)>) {
        self.script
            .lock()
            .unwrap()
            .push_back(openai_turn(content, tool));
    }

    /// Every request body the vendor was handed, parsed out of the raw HTTP.
    fn requests(&self) -> Vec<Value> {
        self.bodies
            .lock()
            .unwrap()
            .iter()
            .filter_map(|raw| {
                let (_, body) = raw.split_once("\r\n\r\n")?;
                serde_json::from_str(body).ok()
            })
            .collect()
    }

    /// The system prompt of every captured request, in order — parsed as a
    /// *value*, because the claims below are about the bytes the daemon
    /// assembled and `\n` on the wire is two characters.
    fn systems(&self) -> Vec<String> {
        self.requests()
            .iter()
            .filter_map(|request| {
                request["messages"]
                    .as_array()?
                    .iter()
                    .find(|message| message["role"] == json!("system"))
                    .map(|message| message["content"].as_str().unwrap_or_default().to_owned())
            })
            .collect()
    }

    /// How many requests carried a **draft** prompt — the duty's own instruction
    /// is unmistakable and belongs to no other call.
    fn draft_calls(&self) -> usize {
        self.requests()
            .iter()
            .filter(|request| {
                serde_json::to_string(request).is_ok_and(|body| {
                    body.contains("You are writing the repository notes for a project")
                })
            })
            .count()
    }
}

/// One OpenAI-compatible streaming turn: a content delta, an optional tool call,
/// then usage and `[DONE]`.
fn openai_turn(content: &str, tool: Option<(&str, &str, &str)>) -> String {
    let mut s = String::new();
    let chunk = json!({ "choices": [{ "delta": { "content": content } }] });
    s.push_str(&format!("data: {chunk}\n\n"));
    if let Some((id, name, args)) = tool {
        let chunk = json!({
            "choices": [{
                "delta": { "tool_calls": [{
                    "index": 0,
                    "id": id,
                    "function": { "name": name, "arguments": args }
                }]}
            }]
        });
        s.push_str(&format!("data: {chunk}\n\n"));
        let finish = json!({ "choices": [{ "delta": {}, "finish_reason": "tool_calls" }] });
        s.push_str(&format!("data: {finish}\n\n"));
    } else {
        let finish = json!({ "choices": [{ "delta": {}, "finish_reason": "stop" }] });
        s.push_str(&format!("data: {finish}\n\n"));
    }
    let usage = json!({ "usage": { "prompt_tokens": 5, "completion_tokens": 2 } });
    s.push_str(&format!("data: {usage}\n\n"));
    s.push_str("data: [DONE]\n\n");
    s
}

/// A client that answers every addressed permission prompt from a script and
/// records what it was shown.
///
/// The recording is half the instrument: BR-2's claim is that the human sees
/// *which* question is on screen, so `--force`'s `replace: true` has to be
/// readable off the subject rather than inferred from the outcome.
struct Answerer {
    pending: Arc<PendingPermissions>,
    script: Mutex<std::collections::VecDeque<String>>,
    seen: Mutex<Vec<PermissionSubject>>,
}

impl Answerer {
    fn new(pending: Arc<PendingPermissions>, script: &[&str]) -> Arc<Self> {
        Arc::new(Self {
            pending,
            script: Mutex::new(script.iter().map(|s| (*s).to_owned()).collect()),
            seen: Mutex::default(),
        })
    }

    /// Every subject this client was shown, in order.
    fn seen(&self) -> Vec<PermissionSubject> {
        self.seen.lock().unwrap().clone()
    }

    /// Just the generation offers.
    fn offers(&self) -> Vec<PermissionSubject> {
        self.seen()
            .into_iter()
            .filter(|subject| matches!(subject, PermissionSubject::RepoContextGeneration { .. }))
            .collect()
    }
}

impl AddressedPermissionDelivery for Answerer {
    fn deliver(
        &self,
        connection: ConnectionId,
        _session_id: &SessionId,
        request: PermissionRequest,
    ) -> bool {
        if let Some(subject) = request.subject.clone() {
            self.seen.lock().unwrap().push(subject);
        }
        // An exhausted script dismisses rather than hangs: these tests count
        // prompts, and a prompt nobody answers is a wedged process rather than a
        // failed assertion.
        let outcome = match self.script.lock().unwrap().pop_front() {
            Some(option_id) => PermissionOutcome::Selected { option_id },
            None => PermissionOutcome::Cancelled,
        };
        self.pending
            .resolve_from(&request.request_id, outcome, connection)
    }
}

/// A daemon runtime with one mock provider, a session registry, and a client
/// that answers its prompts.
struct Wired {
    runtime: Arc<DaemonRuntime>,
    events: Arc<EventBus>,
    sessions: SessionRegistry,
    vendor: Vendor,
    connection: ConnectionId,
    answerer: Arc<Answerer>,
}

impl Wired {
    /// A runtime whose `scan`, `build` and `think` tiers all point at one mock
    /// vendor, answering permission prompts from `script`.
    ///
    /// `reflex` is deliberately left unbound: `route`, `redact` and `title` all
    /// hang off it and this machine has no local tier, so those duties resolve
    /// to nothing and cannot race the turn for a scripted reply
    /// (`repo_context.rs`'s fixture, for its reason).
    fn new(script: &[&str]) -> Self {
        let vendor = Vendor::start();
        let runtime = Arc::new(DaemonRuntime::minimal().with_default_boundaries_disabled());
        runtime
            .apply_config_update(ConfigUpdate::RegisterProvider(ProviderConfig {
                id: ProviderId::from("mock"),
                kind: ProtoProviderKind::OpenaiCompatible,
                endpoint: Some(vendor.endpoint.clone()),
                model: Some("mock-1".to_owned()),
                auth_ref: None,
                max_context: Some(128_000),
                context_budget_cap: None,
                allow_cleartext: None,
                floored_budget: None,
            }))
            .expect("registering a provider");
        for tier in [ProtoTier::Scan, ProtoTier::Build, ProtoTier::Think] {
            runtime
                .apply_config_update(ConfigUpdate::SetTierBinding(TierBindingConfig {
                    tier,
                    provider_id: ProviderId::from("mock"),
                    fallback_id: None,
                }))
                .expect("binding a tier");
        }
        let answerer = Answerer::new(Arc::clone(runtime.pending()), script);
        runtime.install_addressed_delivery(
            Arc::clone(&answerer) as Arc<dyn AddressedPermissionDelivery>
        );
        Self {
            runtime,
            events: Arc::new(EventBus::new()),
            sessions: SessionRegistry::new(),
            vendor,
            connection: GrantRegistry::new().next_connection_id(),
            answerer,
        }
    }

    /// A session rooted at `cwd`, with its notes and its generation state
    /// derived exactly as `session/create` derives them — the daemon's own
    /// function, so a fixture cannot drift into an agreeing re-implementation of
    /// the create path (LESSON-451).
    fn session_at(&self, cwd: &Path) -> SessionId {
        let id = self
            .sessions
            .create(
                SessionMode::Structured,
                Some(ProtoPhase::Implement),
                Some(cwd.to_path_buf()),
            )
            .expect("a structured session takes a phase")
            .session_id;
        let probed = self.runtime.session_root_for(Some(cwd));
        self.runtime
            .store_session_repo_context(&self.sessions, &id, &probed, &self.events);
        id
    }

    /// Move a session's root, through the daemon's own `/cd`.
    fn cd(&self, id: &SessionId, to: &Path) {
        self.runtime
            .set_session_cwd(
                &SessionSetCwdParams {
                    session_id: id.clone(),
                    cwd: to.to_path_buf(),
                    name_hint: None,
                },
                &self.sessions,
                &self.events,
                &tetond::skills::RealFs,
            )
            .expect("the fixture always moves to a real directory");
    }

    fn set_level(&self, id: &SessionId, level: PermissionLevel) {
        self.runtime.session_permissions(
            &SessionPermissionsParams {
                session_id: id.clone(),
                level: Some(level),
            },
            &self.events,
        );
    }

    async fn turn(&self, id: &SessionId, prompt: &str) {
        let cwd = self
            .sessions
            .get(id)
            .and_then(|s| s.cwd)
            .expect("the fixture always roots its sessions");
        let outcome = self
            .runtime
            .run_prompt_turn(
                &self.events,
                &self.sessions,
                id.clone(),
                SessionMode::Structured,
                Some(ProtoPhase::Implement),
                Some(cwd),
                prompt.to_owned(),
                None,
                Some(self.connection),
                ClientPresence::unwatched(),
            )
            .await;
        assert!(outcome.is_ok(), "the turn failed: {outcome:?}");
    }

    async fn context(&self, id: &SessionId, action: ContextAction) -> SessionContextResult {
        self.runtime
            .session_context(
                &SessionContextParams {
                    session_id: id.clone(),
                    action,
                },
                &self.sessions,
                &self.events,
                None,
                Some(self.connection),
            )
            .await
            .expect(
                "`session/context` answers every fixture session here: the id is \
                     live, no turn is in flight, and an RPC error would be a defect \
                     rather than an outcome this test is about",
            )
    }

    fn generation_state(&self, id: &SessionId) -> GenerationState {
        self.sessions.generation(id)
    }
}

/// Every `repo_context_generation` stage a subscription heard, in order.
///
/// Its own drain rather than [`stages`]: that one asserts the fixture's fixed
/// session id, and these sessions are minted by the registry.
fn offer_stages(sub: &mut Subscription) -> Vec<RepoContextGeneration> {
    let mut out = Vec::new();
    while let Some(envelope) = sub.try_recv() {
        assert!(
            envelope.session_id.is_some(),
            "every stage is attributed to the session that ran it: {:?}",
            envelope.event
        );
        if let Event::RepoContextGeneration(news) = envelope.event {
            out.push(news);
        }
    }
    out
}

/// The stage words, in order.
fn words(sub: &mut Subscription) -> Vec<Stage> {
    names(&offer_stages(sub))
}

// ---------------------------------------------------------------------------
// BR-1 / AC-1 — once per session per root, on the first prompt
// ---------------------------------------------------------------------------

/// **BR-1, AC-1.** The first prompt in a project with no notes raises **exactly
/// one** offer; accepting writes the file, loads it, and the *same turn's*
/// request carries the block; declining writes nothing and a second prompt
/// raises no second offer; a `/cd` into another notes-less project raises it
/// again; and an `AGENTS.md` or an empty `TETON.md` suppresses it with no prompt
/// at all.
///
/// # Why the whole rule is one test
///
/// Every clause here is a claim about the *same* counter — how many times the
/// daemon asked — and each is only meaningful beside the others. "Declining
/// raises no second offer" is equally consistent with a build that never asks,
/// and "a `/cd` asks again" is equally consistent with one that asks on every
/// turn. The accepted leg at the top is the non-vacuity for both (LESSON-479).
///
/// # The block assertion is on the bytes, not on the state
///
/// AC-1 says the turn that raised the offer proceeds *with the block resident*.
/// A stored `Loaded` state does not say that: the prompt is built after the
/// offer runs, so the claim is about the request the vendor was handed, and the
/// assertion is that its system prompt **ends with** the block REQ-612's own
/// renderer produces for the file on disk.
///
/// **Mutations** (LESSON-441), all four run 2026-09-04 and restored, recorded as
/// **observed**:
/// 1. `assemble_harness` keeps the pre-offer state instead of reading the stored
///    one back — the file is written and the turn's system prompt does not end
///    with the block (`the same turn's request body must end with the block`);
/// 2. dropping `claim_generation`'s `Pending` check — `left: 2, right: 1` on
///    the declined session's second prompt;
/// 3. arming on `RepoContextState::Absent` alone, without
///    `generate::notes_present` — `left: Pending, right: Suppressed` for the
///    empty `TETON.md` a user left there to stop exactly this;
/// 4. dropping the root comparison from `SessionRegistry::arm_generation` —
///    `left: Declined, right: Pending` after the `/cd`, so a session that
///    declined once would never be offered notes in any later repository.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_offer_is_raised_once_per_session_per_root_on_the_first_prompt_and_never_after_a_decline(
) {
    // --- accepted: written, loaded, and resident in the same turn -----------
    {
        let project = Project::new("accept");
        let wired = Wired::new(&["allow_once"]);
        let mut sub = wired.events.subscribe(256);
        wired.vendor.will_answer(GOOD_DRAFT, None);
        wired.vendor.will_answer("Understood.", None);

        let session = wired.session_at(&project.root);
        assert_eq!(wired.generation_state(&session), GenerationState::Pending);
        wired.turn(&session, "what is this repository?").await;

        assert_eq!(
            wired.answerer.offers().len(),
            1,
            "exactly one offer, on the first prompt: {:?}",
            wired.answerer.seen()
        );
        let PermissionSubject::RepoContextGeneration { path, replace, .. } =
            &wired.answerer.offers()[0]
        else {
            unreachable!("filtered above");
        };
        assert_eq!(path, "TETON.md", "the prompt names the path it will write");
        assert!(!replace, "nothing was there to replace");

        assert!(project.notes_exist(), "an accepted offer writes the file");
        let on_disk = std::fs::read_to_string(project.notes()).unwrap();
        assert!(on_disk.starts_with("> Generated by Teton on "), "{on_disk}");
        assert_eq!(wired.generation_state(&session), GenerationState::Generated);

        // …and the turn that raised it carried the block. The expected bytes
        // are REQ-612's own render of the stored state, so this cannot pass
        // against a block assembled some other way.
        let state = wired.sessions.repo_context(&session);
        let RepoContextState::Loaded(file) = &*state else {
            panic!("the written file must be resident: {:?}", state.kind());
        };
        let block = RepoContextBlock::render(file, REPO_CONTEXT_MAX_BYTES);
        let systems = wired.vendor.systems();
        let turn_system = systems.last().expect("the turn reached the vendor");
        assert!(
            turn_system.trim_end().ends_with(block.text.trim_end()),
            "the same turn's request body must end with the block: {turn_system}"
        );
        assert_eq!(wired.vendor.draft_calls(), 1, "one draft is one model call");

        let heard = words(&mut sub);
        assert_eq!(
            heard,
            vec![
                Stage::Offered,
                Stage::Walking,
                Stage::Drafted,
                Stage::Written
            ],
            "one event per stage, offer first"
        );
    }

    // --- declined: nothing written, and never asked twice -------------------
    {
        let project = Project::new("decline");
        let wired = Wired::new(&["reject_once"]);
        let mut sub = wired.events.subscribe(256);
        wired.vendor.will_answer("First.", None);
        wired.vendor.will_answer("Second.", None);

        let session = wired.session_at(&project.root);
        wired.turn(&session, "first prompt").await;
        assert!(!project.notes_exist(), "a declined offer writes nothing");
        assert_eq!(wired.generation_state(&session), GenerationState::Declined);
        assert_eq!(wired.vendor.draft_calls(), 0, "nothing was drafted");

        wired.turn(&session, "second prompt").await;
        assert_eq!(
            wired.answerer.offers().len(),
            1,
            "a declined offer is not raised again in this session"
        );
        assert!(!project.notes_exist());
        assert_eq!(
            words(&mut sub),
            vec![Stage::Offered, Stage::Declined],
            "one offer, one decline, and nothing on the second prompt"
        );

        // --- …and a `/cd` into another absent project asks again ------------
        let second = Project::new("cd-target");
        wired.cd(&session, &second.root);
        assert_eq!(
            wired.generation_state(&session),
            GenerationState::Pending,
            "a different root is a different question (BR-1)"
        );
        wired.vendor.will_answer(GOOD_DRAFT, None);
        wired.vendor.will_answer("Third.", None);
        wired.turn(&session, "third prompt").await;
        assert_eq!(
            wired.answerer.offers().len(),
            2,
            "the new root raised its own offer"
        );
        // The script is exhausted, so the second offer was dismissed rather than
        // accepted — which is the point: it was *asked*.
        assert!(!second.notes_exist(), "a dismissed offer writes nothing");
    }

    // --- an `AGENTS.md`, and an empty `TETON.md`: no offer at all -----------
    for (tag, name, contents) in [
        (
            "agents",
            "AGENTS.md",
            "# Agents\n\nSomeone described this.\n",
        ),
        ("empty", "TETON.md", "   \n"),
    ] {
        let project = Project::new(tag);
        project.write(name, contents);
        let wired = Wired::new(&["allow_once"]);
        let mut sub = wired.events.subscribe(256);
        wired.vendor.will_answer("Nothing to do.", None);

        let session = wired.session_at(&project.root);
        assert_eq!(
            wired.generation_state(&session),
            GenerationState::Suppressed,
            "{name} suppresses the offer before any turn runs"
        );
        wired.turn(&session, "a prompt").await;
        assert!(
            wired.answerer.offers().is_empty(),
            "{name} must raise no offer: {:?}",
            wired.answerer.seen()
        );
        assert_eq!(wired.vendor.draft_calls(), 0);
        assert!(
            words(&mut sub).is_empty(),
            "a suppressed-by-file root publishes no stage at all"
        );
    }
}

/// **The offer never runs mid-turn.** A two-iteration tool loop sees one offer,
/// one draft call, and one system prompt.
///
/// The rule REQ-612's own refresh obeys, one act further on: the system prompt
/// is fixed for the turn, so a second iteration must not be able to raise a
/// second offer — and the claim needs a turn that genuinely iterates, which is
/// why the first scripted reply calls a tool.
///
/// **What this pins, and what it cannot mutate.** The hook is outside the loop
/// *by position*, so there is no one-line change that moves it inside — which is
/// exactly why the guard is here: the next edit that reaches for the loop fails
/// both counts below. The claim's own mechanism is mutated where it can be, in
/// `SessionRegistry::claim_generation`
/// (`a_generation_offer_is_claimed_once_and_re_armed_only_by_a_new_root`).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_two_iteration_tool_loop_sees_no_second_offer() {
    let project = Project::new("loop");
    let wired = Wired::new(&["allow_once", "allow_once"]);
    wired.vendor.will_answer(GOOD_DRAFT, None);
    wired.vendor.will_answer(
        "Looking.",
        Some((
            "call-1",
            "read",
            &json!({ "path": "README.md" }).to_string(),
        )),
    );
    wired.vendor.will_answer("Read it.", None);

    let session = wired.session_at(&project.root);
    wired.turn(&session, "read the readme").await;

    assert_eq!(
        wired.answerer.offers().len(),
        1,
        "one turn is one offer, however many iterations it takes: {:?}",
        wired.answerer.seen()
    );
    assert_eq!(wired.vendor.draft_calls(), 1);
    // Three requests: the draft, then the turn's two iterations. The draft
    // carries no system message — a duty prompt is one user block — so the
    // *systems* below are exactly the loop's, which is what makes the equality
    // beneath them a statement about the turn rather than about the count.
    assert_eq!(
        wired.vendor.requests().len(),
        3,
        "the loop really did iterate"
    );
    let systems = wired.vendor.systems();
    assert_eq!(systems.len(), 2, "two iterations, two system prompts");
    assert_eq!(
        systems[1], systems[0],
        "the system prompt is fixed for the turn, offer included"
    );
    assert!(
        systems[0].contains("<repo-notes file=\"TETON.md\">"),
        "and it carried the block the offer produced: {}",
        systems[0]
    );
}

// ---------------------------------------------------------------------------
// BR-2 / BR-10 / AC-2 / AC-11 — the level table and the config's two postures
// ---------------------------------------------------------------------------

/// **BR-2, BR-10, AC-2, AC-11.** `plan` suppresses with one event and **no gate
/// call**; `full` writes with no prompt; `generate = always` writes with no
/// prompt at a level that would ask, and the event says the config answered;
/// `generate = never` suppresses, and `/context init` still writes.
///
/// Four postures, one test, because what they claim is one thing: *who* settled
/// the question. A build that asked in every case and a build that never asked
/// each satisfy half of this table, and only the four together pin the table
/// itself.
///
/// **Mutations** (LESSON-441), all three run 2026-09-04 and restored, recorded
/// as **observed** — and the first is why they are recorded that way:
/// 1. deleting the `plan` short-circuit gives `left: [Offered, DeniedLevel],
///    right: [DeniedLevel]`. The prediction was "a prompt is drawn"; what
///    actually happens is that the gate's own level table refuses without
///    drawing one, so the damage is an `offered` line a client renders for a
///    question that was never put to anybody. That is the event stream this
///    assertion is on, and it is why the assertion is on the *stages* rather
///    than only on the prompt count;
/// 2. reading `generate = always` as `ask` — the `always` leg's
///    "`always` answers the question the prompt would ask" fails with the
///    prompt it was shown;
/// 3. letting `generate = never` reach `/context init` — `left:
///    Some(Suppressed), right: Some(Written)`, and AC-11's explicit door is
///    gone.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn plan_suppresses_without_a_prompt_full_and_always_write_without_one_and_never_suppresses() {
    // --- `plan`: no prompt, one `denied_level`, no file ---------------------
    {
        let project = Project::new("plan");
        let wired = Wired::new(&["allow_once"]);
        let mut sub = wired.events.subscribe(256);
        wired.vendor.will_answer("Planning.", None);

        let session = wired.session_at(&project.root);
        wired.set_level(&session, PermissionLevel::Plan);
        wired.turn(&session, "a prompt").await;

        assert!(
            wired.answerer.seen().is_empty(),
            "`plan` draws no prompt for an act it will refuse: {:?}",
            wired.answerer.seen()
        );
        assert!(!project.notes_exist(), "`plan` never writes");
        assert_eq!(wired.vendor.draft_calls(), 0);
        let heard = offer_stages(&mut sub);
        assert_eq!(
            names(&heard),
            vec![Stage::DeniedLevel],
            "one event, and it names the level rather than a suppression"
        );
        assert!(
            heard[0]
                .reason
                .as_deref()
                .is_some_and(|reason| !reason.trim().is_empty()),
            "the level's own sentence rides the event: {heard:?}"
        );
        assert_eq!(
            wired.generation_state(&session),
            GenerationState::Suppressed
        );
    }

    // --- `full`: written, no prompt -----------------------------------------
    {
        let project = Project::new("full");
        let wired = Wired::new(&["allow_once"]);
        wired.vendor.will_answer(GOOD_DRAFT, None);
        wired.vendor.will_answer("Done.", None);

        let session = wired.session_at(&project.root);
        wired.set_level(&session, PermissionLevel::Full);
        wired.turn(&session, "a prompt").await;

        assert!(
            wired.answerer.seen().is_empty(),
            "`full` runs every mutation unprompted: {:?}",
            wired.answerer.seen()
        );
        assert!(project.notes_exist(), "`full` writes");
        assert_eq!(wired.vendor.draft_calls(), 1);
    }

    // --- `generate = always` at `guarded`: written, no prompt, and said so ---
    {
        let project = Project::new("always");
        let wired = Wired::new(&["allow_once"]);
        let mut sub = wired.events.subscribe(256);
        wired
            .runtime
            .apply_config_update(ConfigUpdate::SetRepoContextGenerate {
                mode: RepoContextGenerateMode::Always,
            })
            .expect("the durable posture is a config update");
        wired.vendor.will_answer(GOOD_DRAFT, None);
        wired.vendor.will_answer("Done.", None);

        let session = wired.session_at(&project.root);
        assert_eq!(
            wired
                .runtime
                .session_permissions(
                    &SessionPermissionsParams {
                        session_id: session.clone(),
                        level: None,
                    },
                    &wired.events,
                )
                .level,
            PermissionLevel::Guarded,
            "the leg is only meaningful at a level that would otherwise ask"
        );
        wired.turn(&session, "a prompt").await;

        assert!(
            wired.answerer.seen().is_empty(),
            "`always` answers the question the prompt would ask: {:?}",
            wired.answerer.seen()
        );
        assert!(project.notes_exist(), "`always` writes");
        let heard = offer_stages(&mut sub);
        assert_eq!(names(&heard)[0], Stage::Offered);
        assert!(
            heard[0]
                .reason
                .as_deref()
                .is_some_and(|reason| reason.contains("always")),
            "a user who was never asked is owed the setting's name: {heard:?}"
        );
    }

    // --- `generate = never`: no offer, and `/context init` still writes ------
    {
        let project = Project::new("never");
        let wired = Wired::new(&["allow_once"]);
        let mut sub = wired.events.subscribe(256);
        wired
            .runtime
            .apply_config_update(ConfigUpdate::SetRepoContextGenerate {
                mode: RepoContextGenerateMode::Never,
            })
            .expect("the durable posture is a config update");
        wired.vendor.will_answer("Nothing to do.", None);

        let session = wired.session_at(&project.root);
        wired.turn(&session, "a prompt").await;
        assert!(
            wired.answerer.seen().is_empty(),
            "`never` raises no offer: {:?}",
            wired.answerer.seen()
        );
        assert!(!project.notes_exist());
        assert_eq!(
            names(&offer_stages(&mut sub)),
            vec![Stage::Suppressed],
            "the suppression is stated, not silent"
        );

        // AC-11: the user's explicit act outranks the setting.
        wired.vendor.will_answer(GOOD_DRAFT, None);
        let answer = wired
            .context(&session, ContextAction::Init { force: false })
            .await;
        assert_eq!(
            answer.generation,
            Some(Stage::Written),
            "`/context init` writes even when `generate = never` (BR-8)"
        );
        assert!(project.notes_exist(), "the explicit door wrote the file");
        assert_eq!(answer.state, RepoContextStateKind::Loaded);
        assert_eq!(answer.origin, Some(RepoContextOrigin::Generated));
        assert_eq!(wired.answerer.offers().len(), 1, "and it asked, once");
    }
}

// ---------------------------------------------------------------------------
// BR-8 — the explicit door, and what `--force` asks
// ---------------------------------------------------------------------------

/// **BR-8.** `/context init` with a file already there refuses without touching
/// it, naming its size and the flag; with `--force` it asks a **different**
/// question — the subject says the file will be replaced — and, accepted,
/// replaces it.
///
/// The pair is the test. A refusal alone is equally consistent with a door that
/// never writes, and a replacement alone with one that never refuses; the two
/// legs run over the same file, differing in one flag.
///
/// **Mutations**, both run 2026-09-04 and restored: dropping the present-file
/// check in `offer_and_run` makes the first leg reach the gate and spend a model
/// call before the write refuses, failing "no model call, no prompt: the file is
/// answer enough"; passing `false` for `force` into
/// `authorize_repo_context_generation` makes the second leg's subject say
/// `replace: false`, which is the first leg's question wearing the second's
/// consequences.
///
/// **A third**, run 2026-09-04 and restored: answering the present-file
/// short-circuit with `Ran(Failed { stage: Write, reason: AlreadyExists })` —
/// the shape this finding was raised against — fails with `left: Some(Failed),
/// right: Some(Suppressed)`. The word matters twice over: nothing ran, so
/// `failed` is a claim about a write that was never attempted; and
/// `generation_state_for` would store `GenerationState::Failed` where
/// `arm_generation` stores `Suppressed` for the same fact about the same root.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn init_refuses_an_existing_file_without_force_and_asks_the_replace_question_with_it() {
    let project = Project::new("init");
    let authored = "# Hand written\n\nThe notes a user wrote themselves.\n";
    project.write("TETON.md", authored);
    let wired = Wired::new(&["allow_once"]);
    let mut sub = wired.events.subscribe(256);

    let session = wired.session_at(&project.root);
    assert_eq!(
        wired.generation_state(&session),
        GenerationState::Suppressed,
        "a repository with notes is not offered any"
    );

    // --- without `--force`: refused, untouched, and nobody was asked ---------
    let refused = wired
        .context(&session, ContextAction::Init { force: false })
        .await;
    // `suppressed`, not `failed`: nothing was walked, called, written or
    // unlinked, and the session record stores the same word the arming already
    // stored for this exact fact.
    assert_eq!(refused.generation, Some(Stage::Suppressed));
    assert_eq!(
        std::fs::read_to_string(project.notes()).unwrap(),
        authored,
        "a refused `init` changes nothing"
    );
    assert_eq!(
        refused.bytes_on_disk,
        Some(authored.len() as u64),
        "the answer names the size of the file it would not clobber (BR-8)"
    );
    assert!(
        wired.answerer.seen().is_empty(),
        "no model call, no prompt: the file is answer enough"
    );
    assert_eq!(wired.vendor.draft_calls(), 0);
    let heard = offer_stages(&mut sub);
    assert_eq!(names(&heard), vec![Stage::Suppressed]);
    let reason = heard[0].reason.as_deref().unwrap_or_default();
    assert!(
        reason.contains(&authored.len().to_string()) && reason.contains("--force"),
        "the news names the size and the flag: {reason}"
    );

    // --- with `--force`: a different question, and a replacement ------------
    wired.vendor.will_answer(GOOD_DRAFT, None);
    let replaced = wired
        .context(&session, ContextAction::Init { force: true })
        .await;
    assert_eq!(replaced.generation, Some(Stage::Replaced));
    let offers = wired.answerer.offers();
    assert_eq!(offers.len(), 1, "`--force` asks");
    let PermissionSubject::RepoContextGeneration { replace, path, .. } = &offers[0] else {
        unreachable!("filtered above");
    };
    assert!(
        *replace,
        "the human must see that this one overwrites: {:?}",
        offers[0]
    );
    assert_eq!(path, "TETON.md");

    let on_disk = std::fs::read_to_string(project.notes()).unwrap();
    assert!(on_disk.starts_with("> Generated by Teton on "), "{on_disk}");
    assert!(on_disk.contains("## Purpose"), "{on_disk}");
    assert_eq!(replaced.state, RepoContextStateKind::Loaded);
    assert_eq!(replaced.origin, Some(RepoContextOrigin::Generated));
    assert_eq!(
        wired.generation_state(&session),
        GenerationState::Generated,
        "the explicit door settles the session's question too"
    );
}
