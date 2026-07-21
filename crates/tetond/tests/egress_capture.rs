//! AC-5 egress-capture harness (BR-1).
//!
//! The acceptance criterion is explicit that a privacy-boundary claim is proven
//! by *capturing outbound traffic*, not by reading code. So this test wires the
//! real [`Egress`] choke point in front of a **capture transport** — a mock
//! `Transport` that records every request it is asked to send instead of hitting
//! the network — drives a scripted session that reads both public and
//! `local-only` files, and asserts:
//!
//! 1. Zero bytes of any boundary file appear in *any* captured outbound payload.
//! 2. A deliberate attempt to route boundary content remotely produces a
//!    `privacy_block` event carrying the path, provider, and action.
//! 3. Provenance survives derivation: a *summary of* a boundary file is blocked
//!    even though it shares no bytes with the original.
//! 4. Error and event/telemetry surfaces carry no boundary content.
//!
//! The capture transport stands in for the network; because `teton-providers`
//! owns no HTTP client, in production the only thing behind the same seam is
//! `tetond`'s real client — so what this harness proves about the guard holds in
//! production too.

use std::sync::{Arc, Mutex};

use async_trait::async_trait;

use teton_core::entities::{BoundaryMode, PrivacyBoundary};
use teton_protocol::events::{Event, PrivacyAction, PrivacyBlock};
use teton_protocol::{ProviderId, SessionId};
use teton_providers::transport::{
    ByteStream, HttpMethod, Transport, TransportError, TransportRequest, TransportResponse,
};

use tetond::egress::provenance::{assembled_provenance, ContextBlock};
use tetond::egress::{Egress, EgressContext, EgressError, PrivacyEventSink, Provenance};

/// Secrets that must never appear in captured egress. Distinct markers so a leak
/// is unambiguous.
const SECRET_ENV: &str = "API_KEY=sk-live-DO-NOT-LEAK-abc123";
const SECRET_YAML: &str = "db_password: hunter2-NEVER-SHIP";

/// A `Transport` that records every request instead of sending it, and returns a
/// canned 200 with a short body stream. The record is shared so the test can
/// inspect it after the transport is moved into the [`Egress`].
#[derive(Default, Clone)]
struct CaptureTransport {
    sent: Arc<Mutex<Vec<TransportRequest>>>,
}

impl CaptureTransport {
    fn captured(&self) -> Vec<TransportRequest> {
        self.sent.lock().unwrap().clone()
    }
}

#[async_trait]
impl Transport for CaptureTransport {
    async fn execute(
        &self,
        request: TransportRequest,
    ) -> Result<TransportResponse, TransportError> {
        self.sent.lock().unwrap().push(request);
        let body: ByteStream = Box::pin(futures::stream::once(async {
            Ok(b"{\"ok\":true}".to_vec())
        }));
        Ok(TransportResponse { status: 200, body })
    }
}

/// Captures `privacy_block` events for assertion.
#[derive(Default)]
struct CapturingSink {
    events: Mutex<Vec<(Option<SessionId>, PrivacyBlock)>>,
}

impl CapturingSink {
    fn events(&self) -> Vec<(Option<SessionId>, PrivacyBlock)> {
        self.events.lock().unwrap().clone()
    }
}

impl PrivacyEventSink for CapturingSink {
    fn privacy_block(&self, session_id: Option<SessionId>, block: PrivacyBlock) {
        self.events.lock().unwrap().push((session_id, block));
    }
}

fn local_only_boundaries() -> Vec<PrivacyBoundary> {
    vec![PrivacyBoundary {
        path_glob: "secrets/**".to_owned(),
        mode: BoundaryMode::LocalOnly,
    }]
}

/// Simulate the daemon's context-assembly step: serialize a set of context
/// blocks into a request body and compute the request's provenance from them.
fn assemble(url: &str, blocks: &[ContextBlock]) -> (TransportRequest, Provenance) {
    let joined = blocks
        .iter()
        .map(ContextBlock::content)
        .collect::<Vec<_>>()
        .join("\n");
    let request = TransportRequest {
        method: HttpMethod::Post,
        url: url.to_owned(),
        headers: vec![("content-type".to_owned(), "application/json".to_owned())],
        body: joined.into_bytes(),
    };
    (request, assembled_provenance(blocks))
}

fn contains_bytes(haystack: &[u8], needle: &str) -> bool {
    haystack
        .windows(needle.len())
        .any(|w| w == needle.as_bytes())
}

/// AC-5, the whole criterion: a scripted session that touches `local-only` files
/// completes with zero boundary bytes in captured egress, and the deliberate
/// remote-routing attempt raises a `privacy_block`.
#[tokio::test]
async fn scripted_session_leaks_zero_boundary_bytes_and_blocks_deliberate_egress() {
    let capture = CaptureTransport::default();
    let sink = Arc::new(CapturingSink::default());
    let egress = Egress::new(capture.clone(), local_only_boundaries(), sink.clone());
    let ctx = EgressContext::new("anthropic").with_session("sess-42");

    // Turn 1 — an ordinary turn over public files only. Must be allowed and reach
    // the (captured) wire.
    let (req, prov) = assemble(
        "https://api.anthropic.com/v1/messages",
        &[
            ContextBlock::synthetic("You are Teton Code."),
            ContextBlock::from_file("src/main.rs", "fn main() { println!(\"hi\"); }"),
            ContextBlock::from_file("README.md", "# Teton Code"),
        ],
    );
    let r1 = egress.send(req, &prov, &ctx).await;
    assert!(r1.is_ok(), "public turn must be allowed");

    // Turn 2 — DELIBERATE violation: the assembled context includes a read of a
    // `local-only` file. Must be blocked before any byte leaves.
    let (req, prov) = assemble(
        "https://api.anthropic.com/v1/messages",
        &[
            ContextBlock::synthetic("You are Teton Code."),
            ContextBlock::from_file("src/main.rs", "fn main() {}"),
            ContextBlock::from_file("secrets/prod.env", SECRET_ENV),
        ],
    );
    let r2 = egress.send(req, &prov, &ctx).await;
    match r2 {
        Err(EgressError::PrivacyBlocked {
            path,
            provider_id,
            action,
        }) => {
            assert_eq!(path, "secrets/prod.env");
            assert_eq!(provider_id, ProviderId::from("anthropic"));
            assert_eq!(action, PrivacyAction::ReroutedToLocal);
        }
        other => panic!("turn 2 must be a privacy block, got {other:?}"),
    }

    // Turn 3 — provenance survives DERIVATION: a summary of a boundary file.
    let secret_block = ContextBlock::from_file("secrets/config.yaml", SECRET_YAML);
    let summary = secret_block.derive("Summary: this file holds the production DB credentials.");
    assert!(
        !summary.content().contains("hunter2"),
        "the summary must genuinely not quote the secret"
    );
    let (req, prov) = assemble(
        "https://api.anthropic.com/v1/messages",
        &[ContextBlock::synthetic("You are Teton Code."), summary],
    );
    let r3 = egress.send(req, &prov, &ctx).await;
    assert!(
        matches!(r3, Err(EgressError::PrivacyBlocked { ref path, .. }) if path == "secrets/config.yaml"),
        "a summary of a boundary file must itself be blocked, got {r3:?}"
    );

    // --- Assertions over the whole session ---

    // (1) Exactly one request reached the wire (turn 1); turns 2 and 3 never did.
    let captured = capture.captured();
    assert_eq!(captured.len(), 1, "only the clean turn may be forwarded");

    // (2) Zero boundary bytes in ANY captured payload — the core AC-5 property.
    for req in &captured {
        assert!(
            !contains_bytes(&req.body, SECRET_ENV),
            "SECRET_ENV leaked into egress"
        );
        assert!(
            !contains_bytes(&req.body, SECRET_YAML),
            "SECRET_YAML leaked into egress"
        );
        assert!(
            !contains_bytes(&req.body, "hunter2"),
            "derived secret fragment leaked into egress"
        );
    }
    // Positive control: the allowed turn's public content did go out.
    assert!(contains_bytes(&captured[0].body, "fn main()"));

    // (3) Two privacy_block events, one per blocked turn, correctly attributed.
    let events = sink.events();
    assert_eq!(events.len(), 2, "one privacy_block per blocked turn");
    let paths: Vec<&str> = events.iter().map(|(_, b)| b.path.as_str()).collect();
    assert!(paths.contains(&"secrets/prod.env"));
    assert!(paths.contains(&"secrets/config.yaml"));
    for (session_id, block) in &events {
        assert_eq!(session_id.as_ref(), Some(&SessionId::from("sess-42")));
        assert_eq!(block.provider_id, ProviderId::from("anthropic"));
        assert_eq!(block.action, PrivacyAction::ReroutedToLocal);
    }
}

/// AC-4: neither the typed error nor the serialized event carries boundary
/// content — the paths that surface a block to logs, clients, and telemetry are
/// content-free.
#[tokio::test]
async fn error_and_event_paths_exclude_boundary_content() {
    let sink = Arc::new(CapturingSink::default());
    let egress = Egress::new(
        CaptureTransport::default(),
        local_only_boundaries(),
        sink.clone(),
    );
    let ctx = EgressContext::new("anthropic");

    let (req, prov) = assemble(
        "https://api.anthropic.com/v1/messages",
        &[ContextBlock::from_file("secrets/prod.env", SECRET_ENV)],
    );
    let err = egress.send(req, &prov, &ctx).await.expect_err("must block");

    // The typed error renders with only path + provider + action.
    let rendered = err.to_string();
    assert!(rendered.contains("secrets/prod.env"));
    assert!(!rendered.contains("sk-live"));
    assert!(!rendered.contains("API_KEY"));

    // The event serializes (the shape that travels to clients / telemetry) with
    // no secret bytes. Serialize the tagged `Event` — the wire form a subscriber
    // actually receives.
    let (_, block) = sink.events().pop().expect("one event");
    let json = serde_json::to_string(&Event::PrivacyBlock(block)).expect("serialize event");
    assert!(json.contains("secrets/prod.env"));
    assert!(json.contains("privacy_block"));
    assert!(!json.contains("sk-live"));
    assert!(!json.contains("API_KEY"));
}

/// The adapter-facing scoped transport enforces the same boundary through the
/// object-safe `Transport` seam an adapter actually holds.
#[tokio::test]
async fn adapter_seam_is_enforced() {
    let capture = CaptureTransport::default();
    let sink = Arc::new(CapturingSink::default());
    let egress = Egress::new(capture.clone(), local_only_boundaries(), sink.clone());

    let scoped = egress.scoped(
        Provenance::tainted_by("secrets/prod.env"),
        EgressContext::new("anthropic"),
    );
    let request = TransportRequest {
        method: HttpMethod::Post,
        url: "https://api.anthropic.com/v1/messages".to_owned(),
        headers: vec![],
        body: SECRET_ENV.as_bytes().to_vec(),
    };
    // An adapter only sees `&dyn Transport`; the block manifests as the
    // dedicated, non-retryable `PrivacyBlocked` signal (REQ-544 M-1) — never a
    // connect refusal that could be misclassified as transient and retried — and
    // the request never reaches the wire.
    let err = Transport::execute(&scoped, request)
        .await
        .expect_err("scoped transport must refuse");
    assert_eq!(err, TransportError::PrivacyBlocked);
    assert!(capture.captured().is_empty(), "nothing may reach the wire");
    assert_eq!(sink.events().len(), 1, "the block still emitted its event");
}
