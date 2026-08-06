//! The `digest` category's resolved route: what actually serves the harness's
//! tool-result summarization duty (REQ-558 BR-1, BR-2, TASK-054).
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
//! [`super::turn_loop`]'s fold point, and this module's whole API takes a route
//! that was already resolved from `Category::Digest`. There is no `&str` → route
//! path here and no prompt parameter on anything.
//!
//! ## Two implementations, one seam (mirroring `completion.rs`)
//!
//! - [`LocalDigester`] holds an [`Engine`] and no transport, so egress is
//!   impossible on that path by construction — exactly
//!   [`LocalEngineSource`](super::completion::LocalEngineSource)'s posture.
//! - [`RemoteDigester`] reaches the network **only** through the provenance-scoped
//!   `&dyn Transport` that [`Egress::scoped`] produces, so a digest of a
//!   `local-only` file is refused before a byte leaves and is billed as one
//!   `CostRecord` when it is allowed (BR-1/BR-2).
//!
//! ## The direction of change, stated plainly
//!
//! Before this task, summarization was **always local**: file content never left
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

use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use futures::StreamExt;

use teton_inference::{Engine, GenParams};
use teton_protocol::{Category, ProviderId, SessionId};
use teton_providers::{Message, Provider, Role, Transport, TurnEvent, TurnRequest};

use crate::cost::CostAttribution;
use crate::egress::{Egress, EgressContext, Provenance};

use super::context::ToolProvenance;
use super::render::render_duty;

/// The egress [`Provenance`] of one tool result.
///
/// The bridge between the harness's per-tool provenance and the choke point's
/// content provenance, in **one** function: `context_provenance` folds the whole
/// conversation through it and [`RemoteDigester`] scopes a single result through
/// it. A tool whose touched files are unknowable (`shell`) produces
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

/// Something that can serve one `digest` call.
///
/// `Send + Sync` because the turn loop holds the route across awaits and drives
/// it from any task. The trait is deliberately **narrow**: one prompt in, one
/// summary or one error message out. It cannot report success without producing
/// text, and it cannot decline — an implementation that cannot serve returns an
/// `Err`, which the caller turns into the bounded degraded means rather than into
/// silence (LESSON-447).
#[async_trait]
pub trait Digester: Send + Sync {
    /// Summarize `prompt`, whose embedded content came from `provenance`.
    ///
    /// `provenance` is the egress provenance of the *content being sent*, not of
    /// the conversation. A local implementation ignores it and has no transport
    /// to use it with; a remote one MUST scope its egress by it.
    ///
    /// # Errors
    /// Returns the failure as a broadcast-safe sentence — an engine error, a
    /// provider error, or a refusal at the choke point. Never the model's own
    /// output and never the content.
    async fn digest(&self, prompt: &str, provenance: &ToolProvenance) -> Result<String, String>;
}

/// The `digest` category, resolved for this turn.
///
/// Two states, and the second is not an error to be swallowed: an unresolvable
/// `digest` binding is a **routing failure with a reason**, and the reason is the
/// resolver's own sentence (BR-6 — this type mints no explanation of its own).
/// [`summarize_if_large`](super::context::summarize_if_large) reports it on the
/// outcome and still bounds its input.
pub enum DigestRoute {
    /// Resolved: `provider_id` serves the duty through `digester`.
    Serves {
        /// The provider the `digest` category resolved to. Carried so the failure
        /// log — and a test — can say *where* a digest went, rather than only
        /// that one happened.
        provider_id: String,
        /// What performs the call.
        digester: Box<dyn Digester>,
    },
    /// Unresolved: nothing can serve `digest` for this turn.
    Unresolved {
        /// The resolver's sentence, verbatim.
        reason: String,
    },
}

impl DigestRoute {
    /// A route served by the local tier's `engine`.
    #[must_use]
    pub fn local(provider_id: impl Into<String>, engine: Arc<Mutex<dyn Engine>>) -> Self {
        DigestRoute::Serves {
            provider_id: provider_id.into(),
            digester: Box::new(LocalDigester { engine }),
        }
    }

    /// A route served by a remote `provider`, reaching the network only through
    /// `egress`.
    #[must_use]
    pub fn remote<T: Transport + 'static>(
        provider_id: impl Into<String>,
        provider: Box<dyn Provider>,
        egress: Egress<T>,
        model: impl Into<String>,
        session_id: impl Into<SessionId>,
    ) -> Self {
        let provider_id = provider_id.into();
        DigestRoute::Serves {
            digester: Box::new(RemoteDigester {
                provider,
                egress,
                provider_id: ProviderId::from(provider_id.clone()),
                model: model.into(),
                session_id: session_id.into(),
            }),
            provider_id,
        }
    }

    /// A route that resolved to nothing, explained by `reason`.
    #[must_use]
    pub fn unresolved(reason: impl Into<String>) -> Self {
        DigestRoute::Unresolved {
            reason: reason.into(),
        }
    }

    /// The provider serving `digest` this turn, or `None` when it is unresolved.
    #[must_use]
    pub fn provider(&self) -> Option<&str> {
        match self {
            DigestRoute::Serves { provider_id, .. } => Some(provider_id),
            DigestRoute::Unresolved { .. } => None,
        }
    }
}

/// The local tier serving `digest`: an [`Engine`] handle and **no transport**.
///
/// The absence is the guarantee, not an omission — this struct has no field a
/// network call could be made through, which is why
/// [`Digester::digest`]'s `provenance` argument is ignorable here without a
/// boundary check. Adding one would be a guard placed where it is convenient
/// rather than where the decision is made (LESSON-484); the decision is
/// "which route", and it was already made.
struct LocalDigester {
    engine: Arc<Mutex<dyn Engine>>,
}

#[async_trait]
impl Digester for LocalDigester {
    async fn digest(&self, prompt: &str, _provenance: &ToolProvenance) -> Result<String, String> {
        let engine = Arc::clone(&self.engine);
        let prompt = prompt.to_owned();
        // The completion runs on the blocking pool (E-3): with a real llama.cpp
        // engine a summary takes seconds, and this is awaited from the async turn
        // loop, where running it inline would park the tokio worker and stall
        // every other session's RPCs.
        let result = tokio::task::spawn_blocking(move || {
            let params = GenParams::default();
            let guard = engine.lock().expect("engine mutex poisoned");
            // REQ-554 BR-7: a duty prompt gets the same template treatment an
            // agent turn does. The format is read from the guard already held
            // here, inside the blocking task: taking a second lock on the async
            // path to ask the engine its format would park a tokio worker behind
            // whatever completion currently owns the mutex (LESSON-448).
            let format = guard.chat_format();
            let rendered = render_duty(format, &prompt);
            guard
                .complete(&rendered, &params, &mut |_| true)
                .map(|completion| completion.text)
        })
        .await;
        match result {
            Ok(Ok(text)) => Ok(text),
            Ok(Err(err)) => Err(err.to_string()),
            Err(_) => Err("the local summarization task did not complete".to_owned()),
        }
    }
}

/// A remote provider serving `digest`, through the single egress choke point.
///
/// The provider is handed only the provenance-scoped `&dyn Transport` that
/// [`Egress::scoped`] produces, so it cannot reach the network any other way: a
/// digest of boundary-protected content is refused before a byte leaves (BR-1),
/// and an allowed one is metered into a `CostRecord` attributed to
/// `Category::Digest` (BR-2) — so a user who binds `scan` remotely can see what
/// their summarization is costing.
struct RemoteDigester<T: Transport> {
    provider: Box<dyn Provider>,
    egress: Egress<T>,
    provider_id: ProviderId,
    model: String,
    session_id: SessionId,
}

#[async_trait]
impl<T: Transport> Digester for RemoteDigester<T> {
    async fn digest(&self, prompt: &str, provenance: &ToolProvenance) -> Result<String, String> {
        let request = TurnRequest {
            model: self.model.clone(),
            // A duty is one instruction, not a conversation: no system prompt to
            // inherit and — crucially — **no tools**. The summarizer must not be
            // able to emit a tool call that the loop would then have to decide
            // what to do with.
            system: None,
            messages: vec![Message {
                role: Role::User,
                content: prompt.to_owned(),
            }],
            tools: Vec::new(),
            max_tokens: GenParams::default().max_tokens,
        };
        // BR-2: a duty has no lifecycle position, so it attributes no phase — but
        // it does attribute its category, which is the whole point of routing it.
        let attribution = CostAttribution::new(self.model.clone()).with_category(Category::Digest);
        let ctx = EgressContext::new(self.provider_id.clone())
            .with_session(self.session_id.clone())
            .with_cost(attribution);
        // BR-1: the *tool result's own* provenance, not the turn's context. This
        // is the narrower and correct scope — only the tool output is being sent.
        let transport = self.egress.scoped(tool_result_provenance(provenance), ctx);

        let mut stream = self
            .provider
            .stream_turn(request, &transport)
            .await
            .map_err(|err| err.to_string())?;
        let mut text = String::new();
        while let Some(event) = stream.next().await {
            match event.map_err(|err| err.to_string())? {
                TurnEvent::TextDelta(delta) => text.push_str(&delta),
                // A duty was offered no tools, so a tool call is a provider
                // ignoring the request; drop it rather than fold it into a
                // summary. `Completed` carries usage, which the meter reads off
                // the stream at the choke point.
                TurnEvent::ToolCall(_) | TurnEvent::Completed(_) => {}
            }
        }
        Ok(text.trim().to_owned())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use std::collections::BTreeSet;

    use futures::stream;
    use teton_core::entities::{BoundaryMode, PrivacyBoundary};
    use teton_inference::MockEngine;
    use teton_providers::transport::{
        HttpMethod, TransportError, TransportRequest, TransportResponse,
    };
    use teton_providers::{
        CapabilityProfile, ProviderError, StopReason, TokenUsage, TurnCompletion, TurnStream,
    };

    use crate::egress::NoopSink;
    use crate::harness::context::{summarize_if_large, SummarizeOutcome};

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
            let events: Vec<Result<TurnEvent, ProviderError>> = vec![
                Ok(TurnEvent::TextDelta(self.reply.clone())),
                Ok(TurnEvent::Completed(TurnCompletion {
                    usage: TokenUsage::default(),
                    stop_reason: StopReason::EndTurn,
                })),
            ];
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
    ) -> (DigestRoute, Arc<Mutex<Vec<Vec<u8>>>>) {
        let transport = CaptureTransport::default();
        let sent = Arc::clone(&transport.sent);
        let egress = Egress::new(transport, boundaries, Arc::new(NoopSink));
        let route = DigestRoute::remote(
            "frontier",
            Box::new(WireProvider {
                reply: reply.to_owned(),
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
        let route = DigestRoute::local("local", engine);

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
        let route = DigestRoute::unresolved(
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
        let route = DigestRoute::unresolved("nothing is bound");
        let out =
            summarize_if_large(&route, "read", "short output", 100, &ToolProvenance::none()).await;
        assert_eq!(out.text, "short output");
        assert_eq!(out.engine_error, None);
    }
}
