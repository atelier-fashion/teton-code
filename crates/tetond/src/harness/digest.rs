//! The `digest` duty: the harness's tool-result summarization (REQ-558 BR-1,
//! BR-2, TASK-054), expressed on the shared duty seam (REQ-561 BR-6, TASK-058).
//!
//! ## What this replaces
//!
//! [`summarize_if_large`](super::context::summarize_if_large) took an
//! `Option<Arc<Mutex<dyn Engine>>>` — the local engine, hardcoded. It did not
//! route at all: `digest` was a category the config could bind and the runtime
//! never read, which is BR-1's headline defect in miniature. Now the turn loop is
//! handed a **resolved route** and the duty runs wherever `digest` resolves —
//! through a per-category override, or through its `scan` tier binding.
//!
//! ## The category is tagged at the call site, not inferred (BR-2)
//!
//! `digest` is `harness_known`: `summarize_if_large` *is* summarizing, so nothing
//! reads the tool output to decide that it is a summarization. The tag lives at
//! [`super::turn_loop`]'s fold point, and the whole `digest` API takes a route
//! that was already resolved from `Category::Digest`. There is no `&str` → route
//! path here and no prompt parameter on anything.
//!
//! ## What is left in this module
//!
//! Almost nothing, and that is the point. REQ-558 built `digest` as a one-off —
//! its own route enum, its own trait, its own local/remote pair, its own egress
//! call. REQ-561 moved all five concerns onto the shared [`duty`](super::duty)
//! seam so that four more callers do not each grow a copy (BR-6, ADR-1). What is
//! genuinely `digest`-specific is what remains here:
//!
//! - [`DIGEST_DUTY`] — the category plus its output ceiling, the one `const`
//!   ADR-3 allows a category to carry.
//! - [`tool_result_provenance`] — the caller-side bridge from the harness's
//!   per-tool provenance to the egress choke point's content provenance
//!   (ADR-2: [`Duty::perform`](super::duty::Duty::perform) takes the merged
//!   [`Provenance`], so converting is the call site's job).
//!
//! The prompt and its output contract live with the call site that builds them,
//! in [`super::context`].
//!
//! ## The direction of change, stated plainly
//!
//! Before TASK-054, summarization was **always local**: file content never left
//! the machine for this purpose. A user who binds `scan` (or `digest` directly) to
//! a remote provider now sends tool output — file bodies, build logs — to that
//! provider. That is intended; `scan` is a remote-shaped tier and `digest` is a
//! `scan` duty. What holds the line is that the content is scoped by the *tool
//! result's own* provenance at the choke point, which is narrower than the turn's
//! context: a `read` of a boundary-protected file is blocked even though the rest
//! of the conversation is fine, and a `shell` result (whose touched files are
//! unknowable) fails closed whenever any boundary is configured.
//!
//! A blocked or failed remote digest does **not** silently fold the raw result:
//! it degrades to the mechanical truncation `summarize_if_large` already used for
//! engine failure (LESSON-447). See that function for the invariant.

use teton_protocol::Category;

use crate::egress::Provenance;

use super::context::{ToolProvenance, SUMMARIZER_INPUT_MAX_BYTES};
use super::duty::DutyKind;

/// Byte ceiling on what a remote `digest` will accumulate from its stream.
///
/// `max_tokens` on the request is a *request*; a provider that ignores it, or a
/// stream that does not terminate, would otherwise grow an unbounded buffer on
/// the one path whose entire job is to make its input smaller. The local duty
/// has no equivalent exposure — its engine returns a completed string under
/// `GenParams` — so the bound is enforced in the remote impl (LESSON-484: where
/// the decision is made) rather than at the call site.
///
/// Set to the summarizer's own **input** ceiling: a summary larger than the
/// text it summarizes is not a summary, so nothing legitimate is lost, and
/// reusing the constant means the two cannot drift into a state where the duty
/// can return more than `summarize_if_large` would ever have sent.
const DIGEST_OUTPUT_MAX_BYTES: usize = SUMMARIZER_INPUT_MAX_BYTES;

/// The `digest` duty on the shared seam: its category and its output ceiling.
///
/// One `const` per category, stated once and read by every construction site —
/// the resolver in [`crate::runtime`], the transport-free offline entry point in
/// [`super::turn_loop`], and the tests. Together with that resolver's one
/// category-naming line and the prompt in [`super::context`], this is the whole
/// of `digest`'s per-category surface (REQ-561 ADR-3).
pub const DIGEST_DUTY: DutyKind = DutyKind::new(Category::Digest, DIGEST_OUTPUT_MAX_BYTES);

/// The egress [`Provenance`] of one tool result.
///
/// The bridge between the harness's per-tool provenance and the choke point's
/// content provenance, in **one** function: `context_provenance` folds the whole
/// conversation through it and [`summarize_if_large`](super::context::summarize_if_large)
/// scopes a single result through it before handing the duty the merged form
/// (ADR-2). A tool whose touched files are unknowable (`shell`) produces
/// [`Provenance::unknown`], which egress fail-closes.
#[must_use]
pub fn tool_result_provenance(provenance: &ToolProvenance) -> Provenance {
    match provenance {
        ToolProvenance::Sources(paths) => {
            let mut prov = Provenance::empty();
            for path in paths {
                prov.merge(&Provenance::tainted_by(path.clone()));
            }
            prov
        }
        ToolProvenance::Unknown => Provenance::unknown(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use std::collections::BTreeSet;
    use std::sync::{Arc, Mutex};

    use async_trait::async_trait;
    use futures::stream;
    use teton_core::entities::{BoundaryMode, PrivacyBoundary};
    use teton_inference::{Engine, MockEngine};
    use teton_protocol::events::RouteDecided;
    use teton_protocol::{ProviderId, SessionId, Tier};
    use teton_providers::transport::{
        HttpMethod, TransportError, TransportRequest, TransportResponse,
    };
    use teton_providers::{
        CapabilityProfile, Provider, ProviderError, StopReason, TokenUsage, Transport,
        TurnCompletion, TurnEvent, TurnRequest, TurnStream,
    };

    use crate::egress::{Egress, NoopSink};
    use crate::harness::context::{summarize_if_large, SummarizeOutcome};
    use crate::harness::duty::DutyRoute;

    /// A transport that records every request body it is asked to send.
    ///
    /// This is the instrument the boundary claim is made against: "no boundary
    /// content left the machine" is a statement about bytes on a wire, not about
    /// a return value, so it is asserted here rather than on the outcome.
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
                status: 200,
                body: Box::pin(stream::empty()),
            })
        }
    }

    /// A provider that actually puts its request on the wire before answering.
    ///
    /// The smallest adapter that can *leak*: it serializes the prompt into the
    /// transport it was handed. A test that bypassed egress would therefore show
    /// the prompt in the capture, which is what makes the boundary assertion
    /// non-vacuous.
    struct WireProvider {
        reply: String,
        /// How many times the reply delta is streamed. `1` is an ordinary
        /// provider; anything larger is one ignoring its token budget.
        repeat: usize,
    }

    #[async_trait]
    impl Provider for WireProvider {
        fn id(&self) -> &str {
            "wire"
        }
        fn capabilities(&self) -> CapabilityProfile {
            CapabilityProfile::default()
        }
        async fn stream_turn(
            &self,
            request: TurnRequest,
            transport: &dyn Transport,
        ) -> Result<TurnStream, ProviderError> {
            let body =
                serde_json::to_vec(&request).map_err(|e| ProviderError::Build(e.to_string()))?;
            transport
                .execute(TransportRequest {
                    method: HttpMethod::Post,
                    url: "https://api.example.com/v1/chat/completions".to_owned(),
                    headers: Vec::new(),
                    body,
                })
                .await
                .map_err(|err| match err {
                    TransportError::PrivacyBlocked => ProviderError::PrivacyBlocked,
                    _ => ProviderError::Transport,
                })?;
            // `repeat` models a provider that ignores `max_tokens`: the same
            // delta over and over, which is what an unbounded accumulator would
            // happily grow on.
            let mut events: Vec<Result<TurnEvent, ProviderError>> = (0..self.repeat)
                .map(|_| Ok(TurnEvent::TextDelta(self.reply.clone())))
                .collect();
            events.push(Ok(TurnEvent::Completed(TurnCompletion {
                usage: TokenUsage::default(),
                stop_reason: StopReason::EndTurn,
            })));
            Ok(Box::pin(stream::iter(events)))
        }
    }

    fn boundaries() -> Vec<PrivacyBoundary> {
        vec![PrivacyBoundary {
            path_glob: "secrets/**".to_owned(),
            mode: BoundaryMode::LocalOnly,
        }]
    }

    /// A remote `digest` route over a capturing transport, with `boundaries`
    /// enforced at the choke point.
    fn remote_route(
        boundaries: Vec<PrivacyBoundary>,
        reply: &str,
    ) -> (DutyRoute, Arc<Mutex<Vec<Vec<u8>>>>) {
        let transport = CaptureTransport::default();
        let sent = Arc::clone(&transport.sent);
        let egress = Egress::new(transport, boundaries, Arc::new(NoopSink));
        let route = DutyRoute::remote(
            DIGEST_DUTY,
            "frontier",
            Box::new(WireProvider {
                reply: reply.to_owned(),
                repeat: 1,
            }),
            egress,
            "claude-opus-4",
            "sess-1",
        );
        (route, sent)
    }

    /// Everything the capturing transport was asked to send, as one string.
    fn wire(sent: &Arc<Mutex<Vec<Vec<u8>>>>) -> String {
        sent.lock()
            .expect("capture poisoned")
            .iter()
            .map(|body| String::from_utf8_lossy(body).into_owned())
            .collect()
    }

    /// **A provider that ignores `max_tokens` cannot grow the digest buffer
    /// without limit.**
    ///
    /// `max_tokens` on the request is a request. The remote digester used to
    /// accumulate every `TextDelta` into a `String` with no ceiling — on the one
    /// path whose entire purpose is to make its input *smaller*, and driven by a
    /// third party. A provider that streams forever, or simply ignores the
    /// budget, would grow that buffer until the process died.
    ///
    /// Bounded harness-side, at the summarizer's own input ceiling: a summary
    /// larger than the text it summarizes is not a summary, so the bound costs
    /// nothing legitimate.
    #[tokio::test]
    async fn a_remote_digest_is_bounded_however_much_the_provider_streams() {
        // 4 KiB per delta × 64 deltas = 256 KiB offered, 16 KiB accepted.
        let chunk = "x".repeat(4096);
        let transport = CaptureTransport::default();
        let egress = Egress::new(transport, Vec::new(), Arc::new(NoopSink));
        let route = DutyRoute::remote(
            DIGEST_DUTY,
            "frontier",
            Box::new(WireProvider {
                reply: chunk.clone(),
                repeat: 64,
            }),
            egress,
            "claude-opus-4",
            "sess-1",
        );

        let out =
            summarize_if_large(&route, "read", &oversized(), 50, &ToolProvenance::none()).await;

        // `summarize_if_large` prefixes its own `[summarized … elided]` header,
        // so the summary itself is what the bound governs. The header is short
        // and fixed; measuring it in is what the allowance below is for.
        let summary = out
            .text
            .split_once('\n')
            .expect("the harness header is one line")
            .1;
        assert!(
            summary.len() <= DIGEST_OUTPUT_MAX_BYTES,
            "a digest accepted {} bytes from a provider ignoring its token \
             budget; the ceiling is {DIGEST_OUTPUT_MAX_BYTES}",
            summary.len()
        );
        // Non-vacuity, both directions: the provider really did offer far more
        // than the bound (so the cap was exercised), and the digest really did
        // accumulate up to it (so this is a cap and not a refusal).
        assert!(
            chunk.len() * 64 > DIGEST_OUTPUT_MAX_BYTES * 4,
            "the fixture must offer well over the bound"
        );
        assert!(
            summary.len() > DIGEST_OUTPUT_MAX_BYTES / 2,
            "the cap must let a real summary through: {} bytes",
            summary.len()
        );
        assert!(out.engine_error.is_none(), "{:?}", out.engine_error);
    }

    /// An oversized tool result: over both the token and the byte trigger.
    fn oversized() -> String {
        "word ".repeat(500)
    }

    // -- the remote leg ------------------------------------------------------

    /// A `digest` bound to a remote provider genuinely runs there — the prompt
    /// reaches the wire and the provider's answer comes back as the summary.
    ///
    /// This is the non-vacuity half of the boundary test below: without it,
    /// "nothing leaked" would be trivially true of a route that never sends.
    #[tokio::test]
    async fn a_remotely_bound_digest_sends_the_duty_prompt_and_returns_its_summary() {
        let (route, sent) = remote_route(boundaries(), "REMOTE SUMMARY");
        let text = oversized();

        let out = summarize_if_large(
            &route,
            "read",
            &text,
            50,
            &ToolProvenance::path("src/main.rs"),
        )
        .await;

        assert_eq!(out.engine_error, None, "the remote digest served the duty");
        assert!(out.text.contains("REMOTE SUMMARY"), "{}", out.text);
        assert!(out.text.contains("summarized read output"));
        let wire = wire(&sent);
        assert!(
            wire.contains("Summarize the following"),
            "the duty prompt never reached the transport"
        );
    }

    /// **The boundary interaction, asserted by capture.**
    ///
    /// `digest` is bound remotely and the tool result came from a `local-only`
    /// file. The choke point refuses before a byte leaves; the duty degrades to
    /// mechanical truncation and says why. Nothing about this is inspection: the
    /// content is asserted absent from the wire, and the previous test proves
    /// this same route *does* send when the content is clean.
    #[tokio::test]
    async fn a_local_only_tool_result_is_never_sent_to_a_remote_digest() {
        let (route, sent) = remote_route(boundaries(), "REMOTE SUMMARY");
        let text = format!("{} API_KEY=super-secret-xyzzy", oversized());

        let out = summarize_if_large(
            &route,
            "read",
            &text,
            50,
            &ToolProvenance::path("secrets/prod.env"),
        )
        .await;

        assert!(
            wire(&sent).is_empty(),
            "boundary content reached the transport"
        );
        assert!(!wire(&sent).contains("super-secret-xyzzy"));
        // And the duty still bounded its input rather than folding it raw.
        assert!(out.text.contains("truncated mechanically"), "{}", out.text);
        assert!(out.text.len() < text.len());
        let err = out.engine_error.expect("the refusal must be reported");
        assert!(err.contains("privacy boundary"), "{err}");
    }

    /// A `shell` result's touched files are unknowable, so a remote digest of one
    /// fails closed whenever any boundary is configured — the same posture the
    /// turn path takes, applied to the duty.
    #[tokio::test]
    async fn an_unknown_provenance_result_fails_closed_at_a_remote_digest() {
        let (route, sent) = remote_route(boundaries(), "REMOTE SUMMARY");
        let text = oversized();

        let out = summarize_if_large(&route, "shell", &text, 50, &ToolProvenance::Unknown).await;

        assert!(wire(&sent).is_empty(), "unknown provenance was forwarded");
        assert!(out.text.contains("truncated mechanically"));
        assert!(out.engine_error.is_some());
    }

    /// With no boundaries configured, the same unknown-provenance result goes —
    /// fail-closed is scoped to "a boundary exists", exactly as `Egress::send`
    /// defines it. Pins that this module reuses that rule rather than inventing
    /// a stricter or looser one of its own.
    #[tokio::test]
    async fn without_boundaries_an_unknown_provenance_result_is_still_digested() {
        let (route, sent) = remote_route(Vec::new(), "REMOTE SUMMARY");
        let out =
            summarize_if_large(&route, "shell", &oversized(), 50, &ToolProvenance::Unknown).await;
        assert!(out.text.contains("REMOTE SUMMARY"));
        assert!(!wire(&sent).is_empty());
    }

    // -- the local leg -------------------------------------------------------

    /// The local route has no transport at all, so a `local-only` tool result is
    /// summarized normally — there is nothing for a boundary to guard against.
    #[tokio::test]
    async fn a_local_digest_summarizes_boundary_content_because_it_cannot_send_it() {
        let engine: Arc<Mutex<dyn Engine>> =
            Arc::new(Mutex::new(MockEngine::with_response("mock", "CONDENSED")));
        let route = DutyRoute::local(DIGEST_DUTY, "local", engine);

        let out = summarize_if_large(
            &route,
            "read",
            &oversized(),
            50,
            &ToolProvenance::path("secrets/prod.env"),
        )
        .await;

        assert_eq!(out.engine_error, None);
        assert!(out.text.contains("CONDENSED"));
        assert_eq!(route.provider(), Some("local"));
    }

    // -- the announcement, at the real call site (REQ-561 BR-2) --------------

    /// **The threshold is what decides whether `route_decided` fires.**
    ///
    /// One route, two tool results, one difference between them: size. The
    /// oversized one crosses `summarize_if_large`'s threshold and reaches the
    /// duty, so it announces; the small one returns before the duty is touched,
    /// so it announces nothing.
    ///
    /// Written as a pair in one test on purpose (LESSON-485). Split apart, the
    /// negative half is satisfied by an emitter that never emits and the
    /// positive half by one that emits at resolution — it is only the two
    /// against the *same route* that pin "announced iff performed". And it is
    /// asserted through `summarize_if_large` rather than `DutyRoute::perform`
    /// because the production call site is what decides whether a duty runs.
    #[tokio::test]
    async fn a_digest_announces_its_route_only_when_the_result_is_big_enough_to_digest() {
        use teton_protocol::events::Event;

        use crate::broadcast::EventBus;

        let engine: Arc<Mutex<dyn Engine>> =
            Arc::new(Mutex::new(MockEngine::with_response("mock", "CONDENSED")));
        let bus = Arc::new(EventBus::new());
        let mut sub = bus.subscribe(16);
        let route = DutyRoute::local(DIGEST_DUTY, "local", engine).announcing(
            &bus,
            Some(SessionId::from("sess")),
            Some(RouteDecided {
                category: Some(Category::Digest),
                tier: Some(Tier::Scan),
                phase: None,
                provider_id: ProviderId::from("local"),
                model: None,
                reason: "Routing the 'digest' category to 'local' through its 'scan' tier \
                         binding."
                    .to_owned(),
            }),
        );

        let announced = |sub: &mut crate::broadcast::Subscription| -> Vec<RouteDecided> {
            std::iter::from_fn(|| sub.try_recv())
                .filter_map(|env| match env.event {
                    Event::RouteDecided(rd) => Some(rd),
                    _ => None,
                })
                .collect()
        };

        // Under the threshold: returned untouched, so no duty ran and nothing is
        // announced.
        let small =
            summarize_if_large(&route, "read", "short output", 100, &ToolProvenance::none()).await;
        assert_eq!(
            small.text, "short output",
            "the fixture must not be digested"
        );
        assert!(
            announced(&mut sub).is_empty(),
            "a result too small to digest announces a routed model call that \
             never happened"
        );

        // Over it: the duty runs, and now the route is on the wire.
        let big =
            summarize_if_large(&route, "read", &oversized(), 50, &ToolProvenance::none()).await;
        assert!(big.text.contains("CONDENSED"), "{}", big.text);
        let decided = announced(&mut sub);
        assert_eq!(decided.len(), 1, "{decided:?}");
        assert_eq!(decided[0].category, Some(Category::Digest));
        assert_eq!(decided[0].tier, Some(Tier::Scan));
        assert_eq!(decided[0].provider_id.0, "local");
        assert!(!decided[0].reason.is_empty());
    }

    // -- the provenance bridge ----------------------------------------------

    #[test]
    fn tool_provenance_maps_onto_egress_provenance() {
        assert!(tool_result_provenance(&ToolProvenance::none()).is_empty());
        assert!(tool_result_provenance(&ToolProvenance::Unknown).is_unknown());

        let two = ToolProvenance::Sources(BTreeSet::from(["a.rs".to_owned(), "b.rs".to_owned()]));
        let prov = tool_result_provenance(&two);
        assert!(prov.contains("a.rs") && prov.contains("b.rs"));
        assert!(!prov.is_unknown());
    }

    // -- the unresolved leg (LESSON-447) ------------------------------------

    /// **AC-4.** An unresolvable `digest` binding is a routing failure, and the
    /// invariant the duty guards — nothing oversized enters context — survives it
    /// by the same degraded means an engine failure gets. `Err(_) => input` on a
    /// function whose job is to shrink its input is the shape LESSON-447 forbids.
    #[tokio::test]
    async fn an_unresolved_digest_still_bounds_an_oversized_result() {
        let route = DutyRoute::unresolved(
            "The 'digest' category inherits the 'scan' tier, which is not bound to any \
             provider.",
        );
        let text = "word ".repeat(50_000); // 250 KB
        let threshold_tokens = 100;

        let SummarizeOutcome {
            text: out,
            engine_error,
        } = summarize_if_large(
            &route,
            "read",
            &text,
            threshold_tokens,
            &ToolProvenance::none(),
        )
        .await;

        // The head only: a fold that leaked the raw result is a quarter-megabyte
        // of `word `, and dumping it buries the finding it is meant to report.
        assert!(
            out.contains("truncated mechanically"),
            "no mechanical fold; the outcome opened with: {}",
            &out[..out.len().min(120)]
        );
        assert!(
            out.len() <= threshold_tokens * super::super::context::APPROX_BYTES_PER_TOKEN + 256,
            "the routing failure folded {} bytes — the raw result leaked through",
            out.len()
        );
        let err = engine_error.expect("a routing failure must be reported, never swallowed");
        assert!(err.contains("not bound to any provider"), "{err}");
        assert_eq!(route.provider(), None);
    }

    /// An unresolved route leaves an *under*-threshold result exactly alone: the
    /// duty only ever acts on what it exists to bound.
    #[tokio::test]
    async fn an_unresolved_digest_does_not_touch_a_small_result() {
        let route = DutyRoute::unresolved("nothing is bound");
        let out =
            summarize_if_large(&route, "read", "short output", 100, &ToolProvenance::none()).await;
        assert_eq!(out.text, "short output");
        assert_eq!(out.engine_error, None);
    }
}
