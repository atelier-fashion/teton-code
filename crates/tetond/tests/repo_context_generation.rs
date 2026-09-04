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

use teton_core::boundary::BoundaryMatcher;
use teton_core::config::Config;
use teton_core::effort::{EffortLevel, ResolvedEffort};
use teton_core::entities::{BoundaryMode, PrivacyBoundary};
use teton_protocol::events::{Event, GenerationOutcome as Stage, RepoContextGeneration};
use teton_protocol::methods::{RepoContextStateKind, RootKind, SessionRoot};
use teton_protocol::{Category, SessionId, Tier};
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
use tetond::harness::tools::walk::WalkBudget;
use tetond::harness::{Duty, DutyRoute, SessionEvents, DRAFT_DUTY};
use tetond::repo_context::evidence::EvidenceBudget;
use tetond::repo_context::generate::{
    self, ConsentGiven, GenerationContext, GenerationOutcome, Reason, Stage as FailStage,
};
use tetond::repo_context::{RealFiles, RepoContextBlock, RepoContextState, REPO_CONTEXT_MAX_BYTES};
use tetond::session_root::ProbedRoot;

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
    route: &'a dyn Fn() -> DutyRoute,
    budget: EvidenceBudget,
    walk: WalkBudget,
    force: bool,
}

impl<'a> Run<'a> {
    fn new(route: &'a dyn Fn() -> DutyRoute) -> Self {
        Self {
            boundaries: &[],
            route,
            // Roomy: every fixture's evidence is a few hundred bytes, so a cut is
            // something a test asks for rather than something it stumbles into.
            budget: EvidenceBudget::new(64 * 1024),
            walk: WalkBudget::default(),
            force: false,
        }
    }
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
        ConsentGiven::granted(),
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
///    the `Load` row's `notes_exist` assertion fails;
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
                // Not even the root's own line fits, so the body is empty.
                budget: EvidenceBudget::new(0),
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
    // The file is written whole and successfully, and the switch is off — so the
    // loader answers `WithheldOff` and the pipeline unlinks what it wrote. This is
    // the one failure `write.rs` cannot see, and the one AC-9 is most about: a
    // file left here is a file the next session loads as if the repository had
    // authored it.
    {
        let mut fx = Fixture::new("not-loaded");
        fx.config.context.repo_file = false;
        let mut sub = fx.subscribe();
        let (_seen, route) = answers_well();
        let outcome = generate(&fx, Run::new(&route)).await;
        assert_failed(
            &outcome,
            FailStage::Load,
            &Reason::NotLoaded(RepoContextStateKind::WithheldOff),
        );
        assert!(
            !fx.notes_exist(),
            "a file the loader refuses is removed, not left for the next session"
        );
        assert_failed_news(&mut sub, &fx);
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
                budget: EvidenceBudget::new(120),
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
fn assert_failed_news(sub: &mut Subscription, fx: &Fixture) {
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
