//! **AC-4's scoping half (BR-7)** — a duty's egress is scoped by *the content it
//! sends*, not by the turn it happens during.
//!
//! ## What this file adds that the per-duty tests do not
//!
//! Every duty already ships the AC-4 pair at its own layer: a remotely-bound
//! duty over boundary content sends **zero** bytes, paired with the same route
//! over clean content that genuinely does send
//! (`harness::{digest,triage,shell_duty,title,compact}::tests`). Those prove the
//! guard holds. They cannot tell **turn-level** scoping from **content-level**
//! scoping, because in all of them the duty's content is the only content there
//! is — a passing turn-scoped implementation would look identical.
//!
//! So the discriminating fixture is the one below: a conversation whose *wider
//! context* carries boundary-protected material, and a duty whose *own* content
//! is clean. Under BR-7 the duty sends. Under turn-level scoping it would be
//! refused. The two legs are asserted on the same route, the same boundaries and
//! the same prompt, differing in exactly one argument — the provenance the call
//! site computed.
//!
//! ## Two duties cannot participate, each for a structural reason
//!
//! Recorded rather than quietly omitted, because "four of the five" would
//! otherwise read as an oversight:
//!
//! - **`compact` sends the conversation.** Its content *is* the turn's wider
//!   context (`context_provenance(self)`), so there is no narrower scope for it
//!   to demonstrate. BR-7 still holds for it — it is scoped by what it sends —
//!   but for `compact` that is everything, which is why a boundary-bearing
//!   conversation refuses compaction outright. That is asserted below as its own
//!   claim rather than folded into the scoping table.
//! - **`shell` output has only one provenance.** Every command result is
//!   [`ToolProvenance::Unknown`], which the choke point fail-closes on before it
//!   looks at a glob, so a `shell` duty on a machine with **any** boundary
//!   configured is refused whatever the rest of the turn contains. Its AC-4 pair
//!   therefore differs in *boundaries* rather than in provenance — the disclosed
//!   limitation in `harness::shell_duty`'s module docs, asserted here too so the
//!   claim is in the test suite and not only in prose.

use std::sync::{Arc, Mutex};
use teton_core::effort::{EffortLevel, ResolvedEffort};

use async_trait::async_trait;
use futures::stream;
use serde_json::json;

use teton_core::entities::{BoundaryMode, PrivacyBoundary};
use teton_core::ProvenanceId;
use teton_providers::transport::{HttpMethod, TransportError, TransportRequest, TransportResponse};
use teton_providers::{
    CapabilityProfile, Provider, ProviderError, StopReason, TokenUsage, Transport, TurnCompletion,
    TurnEvent, TurnRequest, TurnStream,
};

use tetond::egress::{Egress, NoopSink, Provenance};
use tetond::harness::context::{summarize_if_large, APPROX_BYTES_PER_TOKEN};

/// Mint the identity of a fixture file (REQ-571 ADR-A).
///
/// The provenance channel accepts only a [`ProvenanceId`], and an integration
/// test cannot reach the crate-internal fixture helper, so each test binary
/// states its own. A fixture naming a path that is not an identity is a broken
/// fixture, hence the panic.
fn source_id(path: &str) -> ProvenanceId {
    ProvenanceId::claimed(path).expect("fixture path must be a provenance id")
}

use tetond::harness::{
    context_provenance, title, ContextManager, DutyKind, DutyRoute, ToolDuties, ToolOutcome,
    ToolProvenance, ToolRegistry, COMPACT_DUTY, DIGEST_DUTY, SHELL_DUTY, TITLE_DUTY, TRIAGE_DUTY,
};

/// The boundary file's content. It exists nowhere else, so its appearance in a
/// captured payload is a leak and its absence is the guard holding.
const SECRET: &str = "sk-live-DO-NOT-LEAK-duty-egress-Qv7";

/// The boundary-protected path every fixture below taints its context with.
const SECRET_PATH: &str = "secrets/prod.env";

fn boundaries() -> Vec<PrivacyBoundary> {
    vec![PrivacyBoundary {
        path_glob: "secrets/**".to_owned(),
        mode: BoundaryMode::LocalOnly,
        origin: Default::default(),
    }]
}

// ---------------------------------------------------------------------------
// The wire
// ---------------------------------------------------------------------------

#[derive(Default, Clone)]
struct CaptureTransport {
    sent: Arc<Mutex<Vec<Vec<u8>>>>,
}

#[async_trait]
impl Transport for CaptureTransport {
    async fn execute(
        &self,
        request: TransportRequest,
    ) -> Result<TransportResponse, TransportError> {
        self.sent
            .lock()
            .expect("capture poisoned")
            .push(request.body);
        Ok(TransportResponse {
            location: None,
            status: 200,
            body: Box::pin(stream::empty()),
        })
    }
}

struct WireProvider {
    reply: String,
}

#[async_trait]
impl Provider for WireProvider {
    fn id(&self) -> &str {
        "frontier"
    }

    fn capabilities(&self) -> CapabilityProfile {
        CapabilityProfile::default()
    }

    async fn stream_turn(
        &self,
        request: TurnRequest,
        transport: &dyn Transport,
    ) -> Result<TurnStream, ProviderError> {
        let body = serde_json::to_vec(&request).map_err(|e| ProviderError::Build(e.to_string()))?;
        transport
            .execute(TransportRequest {
                method: HttpMethod::Post,
                url: "https://api.example.com/v1/chat/completions".to_owned(),
                headers: Vec::new(),
                body,
            })
            .await
            .map_err(|err| match err {
                TransportError::PrivacyBlocked(detail) => ProviderError::PrivacyBlocked(detail),
                _ => ProviderError::Transport,
            })?;
        Ok(Box::pin(stream::iter(vec![
            Ok(TurnEvent::TextDelta(self.reply.clone())),
            Ok(TurnEvent::Completed(TurnCompletion {
                usage: TokenUsage::default(),
                stop_reason: StopReason::EndTurn,
            })),
        ])))
    }
}

/// A remote route for `kind`, over a capturing transport, with `boundaries`
/// enforced at the choke point.
fn remote(
    kind: DutyKind,
    boundaries: Vec<PrivacyBoundary>,
    reply: &str,
) -> (DutyRoute, Arc<Mutex<Vec<Vec<u8>>>>) {
    let transport = CaptureTransport::default();
    let sent = Arc::clone(&transport.sent);
    let route = DutyRoute::remote(
        kind,
        "frontier",
        Box::new(WireProvider {
            reply: reply.to_owned(),
        }),
        Egress::new(transport, boundaries, Arc::new(NoopSink)),
        "claude-opus-4",
        "sess-scope",
        ResolvedEffort::effort(EffortLevel::High),
    );
    (route, sent)
}

fn wire(sent: &Arc<Mutex<Vec<Vec<u8>>>>) -> String {
    sent.lock()
        .expect("capture poisoned")
        .iter()
        .map(|body| String::from_utf8_lossy(body).into_owned())
        .collect()
}

// ---------------------------------------------------------------------------
// The turn whose wider context is boundary-bearing
// ---------------------------------------------------------------------------

/// A conversation that has already read a `local-only` file: the state in which
/// the *turn* may not go remote, and in which BR-7 says a duty over clean
/// content still may.
fn boundary_bearing_turn() -> ContextManager {
    let mut ctx = ContextManager::new("sys", 1_000_000);
    ctx.push_user("Read the production config and then find the parser.");
    ctx.push_tool_result_prov(
        "read",
        ToolProvenance::path(source_id(SECRET_PATH)),
        format!("API_KEY={SECRET}"),
    );
    ctx
}

/// The control leg every scoping assertion is measured against: this route, with
/// these boundaries, **is** refused when it is handed the turn's own provenance.
///
/// Without it "the duty sent" is equally consistent with a boundary that was
/// never armed (LESSON-485). Returns the refusal sentence so the caller can show
/// it was the choke point talking.
async fn turn_scoped_would_refuse(route: &DutyRoute, turn: &ContextManager) -> String {
    let turn_provenance = context_provenance(turn);
    assert!(
        turn_provenance.sources().any(|p| p == SECRET_PATH),
        "the fixture turn must genuinely carry boundary-protected material, or the \
         scoping claim below has nothing to be narrower than"
    );
    let err = route
        .perform("any duty prompt", &turn_provenance)
        .await
        .expect_err("the turn's own provenance must be refused by this very route");
    assert!(err.contains("privacy boundary"), "{err}");
    err
}

// ===========================================================================
// BR-7 — the scope is the content sent, not the turn
// ===========================================================================

/// **`digest`.** An oversized result read from a clean file is summarized
/// remotely while the same conversation holds a `local-only` read.
#[tokio::test]
async fn a_digest_of_clean_content_sends_inside_a_boundary_bearing_turn() {
    let turn = boundary_bearing_turn();
    let (route, sent) = remote(DIGEST_DUTY, boundaries(), "three functions and a constant");

    // Control: turn-level scoping would refuse this exact route.
    turn_scoped_would_refuse(&route, &turn).await;
    assert!(
        wire(&sent).is_empty(),
        "the refused call must have sent nothing"
    );

    // The claim: the call site computes the provenance of the *result*, which
    // came out of `src/lib.rs`, and that is what scopes the egress.
    let oversized: String = std::iter::repeat_n("pub fn helper() {}\n".to_owned(), 400).collect();
    let out = summarize_if_large(
        &route,
        "read",
        &oversized,
        64,
        64 * APPROX_BYTES_PER_TOKEN,
        &ToolProvenance::path(source_id("src/lib.rs")),
    )
    .await;

    assert_eq!(out.engine_error, None, "the clean digest must have served");
    assert!(
        out.text.contains("three functions and a constant"),
        "{}",
        out.text
    );
    assert!(
        wire(&sent).contains("Summarize the following"),
        "the duty prompt never reached the transport, so nothing was scoped"
    );
    assert!(
        !wire(&sent).contains(SECRET),
        "BR-7 VIOLATION: {}",
        wire(&sent)
    );
}

/// **`triage`.** A `grep` whose matches all come from clean files is ranked
/// remotely while the same conversation holds a `local-only` read.
///
/// Driven through `GrepTool::refine` rather than through `rank_matches`, because
/// the thing being asserted is that the *call site* reads the provenance off the
/// tool's own outcome — the narrower value — and never off the conversation.
#[tokio::test]
async fn a_triage_of_clean_matches_sends_inside_a_boundary_bearing_turn() {
    let turn = boundary_bearing_turn();
    let (route, sent) = remote(TRIAGE_DUTY, boundaries(), "2 1");

    turn_scoped_would_refuse(&route, &turn).await;
    assert!(wire(&sent).is_empty());

    let registry = ToolRegistry::with_builtins();
    let spare = DutyRoute::unresolved("not the duty under test");
    let duties = ToolDuties {
        triage: &route,
        shell: &spare,
    };
    let content = "src/a.rs:12: fn parse_one()\nsrc/b.rs:9: fn parse_two()";
    let refined = registry
        .refine(
            "grep",
            &json!({ "pattern": "parse" }),
            "find the parser",
            &duties,
            // The match count travels on the outcome, not in the text
            // (REQ-561 verify M3): two matches, so the ranking is worth making.
            ToolOutcome::ok(content)
                .with_paths([source_id("src/a.rs"), source_id("src/b.rs")])
                .measuring(2),
        )
        .await;

    assert_eq!(
        refined.duty_error, None,
        "the clean triage must have served"
    );
    assert!(
        refined
            .outcome
            .content
            .starts_with("[triage: 2 of 2 matches"),
        "{}",
        refined.outcome.content
    );
    assert!(
        wire(&sent).contains("Rank them by how"),
        "the duty prompt never reached the transport, so nothing was scoped"
    );
    assert!(
        !wire(&sent).contains(SECRET),
        "BR-7 VIOLATION: {}",
        wire(&sent)
    );
}

/// **`title`.** A session is named from a request that carries no
/// boundary-derived material, while the same conversation holds a `local-only`
/// read.
///
/// `title` is the sharpest case of the three: the content it sends is one
/// sentence the user typed, which is as narrow as a duty's scope ever gets, and
/// a turn-scoped implementation would leave every session in a
/// boundary-touching conversation permanently unnamed.
#[tokio::test]
async fn a_title_of_clean_request_text_sends_inside_a_boundary_bearing_turn() {
    let turn = boundary_bearing_turn();
    let (route, sent) = remote(TITLE_DUTY, boundaries(), "Retry the download client");

    turn_scoped_would_refuse(&route, &turn).await;
    assert!(wire(&sent).is_empty());

    let named = title::name_session(
        &route,
        "Add retry-with-backoff to the download client and cover it with tests.",
        &Provenance::empty(),
    )
    .await
    .expect("a clean request names the session remotely");

    assert_eq!(named, "Retry the download client");
    assert!(
        wire(&sent).contains("Give the session a short name"),
        "the duty prompt never reached the transport, so nothing was scoped"
    );
    assert!(
        !wire(&sent).contains(SECRET),
        "BR-7 VIOLATION: {}",
        wire(&sent)
    );
}

// ===========================================================================
// The two duties whose scope cannot be narrower than the turn
// ===========================================================================

/// **`compact` is the exception that proves the rule.** Its content *is* the
/// conversation, so BR-7's "scoped by what it sends" refuses it outright on a
/// boundary-bearing turn — and the deterministic gate still holds the budget.
///
/// This is not a gap in BR-7. It is BR-7 applied to a duty whose scope happens
/// to be everything, and it is the reason ADR-12's provenance inheritance
/// exists: a compaction that *did* run must not launder what it elided.
///
/// The non-vacuity pair is the same route and the same conversation with no
/// boundary configured, which genuinely sends the whole conversation off the
/// machine — so the refusal is the guard rather than a duty that never fires.
#[tokio::test]
async fn a_compaction_of_a_boundary_bearing_conversation_is_refused_whole() {
    /// Five blocks over a 4 KB budget, all read from the `local-only` file.
    fn pressured() -> ContextManager {
        let mut ctx = ContextManager::new("sys", 1_000_000).with_budget_bytes(4_000);
        for i in 0..5 {
            ctx.push_tool_result_prov(
                "read",
                ToolProvenance::path(source_id(SECRET_PATH)),
                format!("block {i} {SECRET} {}", "x".repeat(1_000)),
            );
        }
        assert!(ctx.under_compaction_pressure());
        ctx
    }
    // Three blocks rather than two, because the replacement paragraph re-enters
    // context inside the untrusted-data envelope (REQ-544 M-2) and the envelope
    // is real bytes: against this fixture's deliberately tiny 4 KB budget,
    // forgetting two no longer fits — which is the over-budget rejection working,
    // not the refusal under test.
    const ANSWER: &str = "FORGET: 1 2 3\nSUMMARY: the agent read the production config.";

    // NON-VACUITY, first: with no boundary configured this very route, this very
    // conversation, genuinely sends.
    let mut open = pressured();
    let (open_route, open_sent) = remote(COMPACT_DUTY, Vec::new(), ANSWER);
    let served = open.compact_if_pressured(&open_route).await;
    assert!(!served.degraded, "{served:?}");
    assert_eq!(served.dropped_blocks, 3);
    assert!(
        wire(&open_sent).contains("Below is a numbered list of the blocks"),
        "the compaction prompt never reached the transport"
    );

    // THE CLAIM: add the boundary and the same compaction is refused before a
    // byte leaves — because what it sends is the conversation itself.
    let mut guarded = pressured();
    let before = guarded.blocks().len();
    let (route, sent) = remote(COMPACT_DUTY, boundaries(), ANSWER);
    let outcome = guarded.compact_if_pressured(&route).await;

    assert!(outcome.degraded, "a refused compaction is a degraded one");
    assert_eq!(outcome.dropped_blocks, 0, "and it applies nothing in part");
    assert!(
        outcome
            .reason
            .as_deref()
            .unwrap_or_default()
            .contains("privacy boundary"),
        "{:?}",
        outcome.reason
    );
    assert!(
        wire(&sent).is_empty(),
        "boundary content reached the transport"
    );
    assert!(!wire(&sent).contains(SECRET));
    assert_eq!(guarded.blocks().len(), before, "nothing was applied");

    // And BR-4's structural half: the budget is still met, by the gate rather
    // than by the duty.
    let _ = guarded.truncate_to_budget();
    assert!(guarded.estimated_bytes() <= 4_000);
}

/// **`shell`'s pair differs in boundaries, not in provenance** — the disclosed
/// limitation, asserted.
///
/// A command's output cannot be attributed to files, so it is always
/// [`ToolProvenance::Unknown`] and there is no "clean shell output" to pair a
/// refusal against. The honest pairing is the one below: the same route and the
/// same output, sending on a machine with no boundary and refused on a machine
/// with one. That is the whole of the difference, and it is what makes the
/// refusal a property of the boundary rather than of the fixture.
#[tokio::test]
async fn a_shell_duty_sends_only_on_a_machine_with_no_boundary_configured() {
    const FAILED: &str = "$ cargo build\n(exit 101)\n[stderr] error[E0308]: mismatched types";
    const REPLY: &str = "the build failed on a type mismatch.";

    async fn interpret(route: &DutyRoute) -> tetond::harness::RefinedOutcome {
        let registry = ToolRegistry::with_builtins();
        let spare = DutyRoute::unresolved("not the duty under test");
        let duties = ToolDuties {
            triage: &spare,
            shell: route,
        };
        registry
            .refine(
                "shell",
                &json!({ "command": "cargo build" }),
                "fix the build",
                &duties,
                // `measuring` says a command really ran: the `shell` duty fires
                // only on results a spawned command produced (REQ-561 verify).
                ToolOutcome::error(FAILED)
                    .with_unknown_provenance()
                    .measuring(FAILED.chars().count()),
            )
            .await
    }

    // NON-VACUITY: no boundary configured, so unknown provenance is not
    // fail-closed and the duty genuinely sends.
    let (open_route, open_sent) = remote(SHELL_DUTY, Vec::new(), REPLY);
    let served = interpret(&open_route).await;
    assert_eq!(served.duty_error, None);
    assert!(
        served.outcome.content.contains("[shell: the build failed"),
        "{}",
        served.outcome.content
    );
    assert!(wire(&open_sent).contains("Below is one shell command"));

    // THE CLAIM: one boundary anywhere on the machine, and the same call is
    // refused — and the command's own output still reaches the model (BR-3).
    let (route, sent) = remote(SHELL_DUTY, boundaries(), REPLY);
    let refused = interpret(&route).await;
    assert!(
        refused
            .duty_error
            .as_deref()
            .unwrap_or_default()
            .contains("privacy boundary"),
        "{:?}",
        refused.duty_error
    );
    assert_eq!(
        refused.outcome.content, FAILED,
        "the tool's own result must ride through unchanged (BR-3)"
    );
    assert!(
        wire(&sent).is_empty(),
        "unattributable output reached the transport"
    );
}
