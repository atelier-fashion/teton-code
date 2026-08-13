//! A refused provenance assertion reaches the user (REQ-571 ADR-D, AC-5/AC-14).
//!
//! ADR-D has two halves and they are refused in two different places, which is
//! the thing this suite is arranged around:
//!
//! 1. **At mint time.** [`ProvenanceId::claimed`] is the constructor for a path
//!    a *third party* asserted — a remote MCP server names what it touched and
//!    nothing on this machine can confirm it. An assertion that is absolute or
//!    `..`-bearing cannot be minted at all, and dropping it would fail **open**
//!    on exactly the value that could name a boundary file (`/repo/secrets/x`),
//!    so the whole call's provenance is marked unknown instead. That is the
//!    refusal production actually takes, and until this task it was silent.
//! 2. **At inspection time.** The egress choke point re-checks every source's
//!    string form ahead of boundary matching and fails closed on one that is not
//!    canonical, whether or not a boundary is configured. By construction that
//!    guard is unreachable — see below.
//!
//! Both report `provenance_rejected`, and the claim under test is **delivery**,
//! not emission: every assertion here reads the event off a real
//! [`EventBus`] subscription, because an audit signal that reaches only daemon
//! stderr is a weak control against a same-uid adversary (LESSON-505). A test
//! that asserted on a capturing sink would pass just as happily against a
//! daemon whose events never leave the process.
//!
//! ## Why the unreachable guard is tested at all (LESSON-508)
//!
//! After ADR-A a `Provenance` holds `ProvenanceId`s, and the one validator both
//! of that type's constructors run refuses every spelling the guard looks for.
//! So the guard is dead code by construction — and a redundant guard with no
//! test is one refactor from being deleted as noise, taking with it the only
//! thing standing between a future third-party-asserted source and a boundary
//! match that silently passes. `ProvenanceId::unvalidated_for_test` is the seam
//! that makes the malformed value constructible; it exists only in test builds
//! (a `teton-core` feature switched on by `tetond`'s dev-dependency), so daemon
//! code that reached for it would not compile.

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use serde_json::{json, Value};

use teton_core::entities::{BoundaryMode, PrivacyBoundary};
use teton_core::ProvenanceId;
use teton_protocol::events::{Event, ProvenanceRejected, ProvenanceRejection};
use teton_protocol::SessionId;
use teton_providers::transport::{
    ByteStream, HttpMethod, Transport, TransportError, TransportRequest, TransportResponse,
};

use tetond::broadcast::{EventBus, Subscription};
use tetond::egress::{Egress, EgressContext, EgressError, PrivacyEventSink, Provenance};
use tetond::mcp::{
    EgressGate, McpConnection, McpConnector, McpError, McpRegistry, McpServerConfig, McpTransport,
};

/// The boundary every test configures, so a refusal has something to be
/// fail-closed *about*.
fn local_only_boundaries() -> Vec<PrivacyBoundary> {
    vec![PrivacyBoundary {
        path_glob: "secrets/**".to_owned(),
        mode: BoundaryMode::LocalOnly,
    }]
}

// ---------------------------------------------------------------------------
// Event capture — subscribe before driving, then drain with a deadline
// ---------------------------------------------------------------------------

/// Drain every `provenance_rejected` currently queued for `sub`.
///
/// The idiom is subscribe-before-drive plus a bounded `recv`: the bus is a
/// bounded per-subscriber channel and `publish` is synchronous, so by the time
/// the driving call has returned, anything it published is already queued. The
/// timeout is what ends the drain, not what waits for the event.
async fn drain_rejections(sub: &mut Subscription) -> Vec<ProvenanceRejected> {
    let mut out = Vec::new();
    while let Ok(Some(env)) = tokio::time::timeout(Duration::from_millis(50), sub.recv()).await {
        if let Event::ProvenanceRejected(pr) = env.event {
            out.push(pr);
        }
    }
    out
}

// ---------------------------------------------------------------------------
// A remote MCP server, through the real egress choke point
// ---------------------------------------------------------------------------

/// Records every request it was asked to send and answers with a minimal valid
/// JSON-RPC `tools/call` result.
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
            Ok(
                br#"{"jsonrpc":"2.0","id":1,"result":{"content":[{"type":"text","text":"ok"}],"isError":false}}"#
                    .to_vec(),
            )
        }));
        Ok(TransportResponse {
            status: 200,
            location: None,
            body,
        })
    }
}

fn http_cfg(id: &str) -> McpServerConfig {
    McpServerConfig {
        id: id.to_owned(),
        transport: McpTransport::Http {
            endpoint: "https://mcp.example.com/rpc".to_owned(),
        },
        trusted: false,
    }
}

// ---------------------------------------------------------------------------
// A local stdio MCP server, which never touches egress
// ---------------------------------------------------------------------------

/// A scripted local connection: canned handshake/list and an echoing
/// `tools/call`. It is deliberately *successful* — the point of the local leg is
/// that a call which succeeds end to end still reports its refused assertion.
struct MockConnection;

#[async_trait]
impl McpConnection for MockConnection {
    async fn call(
        &self,
        method: &str,
        _params: Value,
        _provenance: &Provenance,
    ) -> Result<Value, McpError> {
        Ok(match method {
            "initialize" => json!({ "serverInfo": { "name": "local_fs", "version": "1" } }),
            "tools/list" => json!({ "tools": [] }),
            "tools/call" => json!({
                "content": [ { "type": "text", "text": "ran" } ],
                "isError": false
            }),
            _ => Value::Null,
        })
    }

    async fn notify(&self, _method: &str, _params: Value) -> Result<(), McpError> {
        Ok(())
    }
}

struct MockConnector;

#[async_trait]
impl McpConnector for MockConnector {
    async fn connect(&self, _config: &McpServerConfig) -> Result<Arc<dyn McpConnection>, McpError> {
        Ok(Arc::new(MockConnection))
    }
}

fn stdio_cfg(id: &str) -> McpServerConfig {
    McpServerConfig {
        id: id.to_owned(),
        transport: McpTransport::Stdio {
            command: "unused-by-mock".to_owned(),
            args: vec![],
            env: BTreeMap::new(),
        },
        trusted: true,
    }
}

// ---------------------------------------------------------------------------
// AC-14 — the mint-time refusal, delivered to a subscribed client
// ---------------------------------------------------------------------------

/// AC-14. A remote MCP tool call whose argument asserts an **absolute** path,
/// with a boundary configured: the call is refused before a byte leaves, and the
/// `provenance_rejected` naming the argument is **delivered** to a client
/// subscribed to the bus.
///
/// This is the whole production chain, no seams stubbed: the real `Egress` in
/// front of a capture transport, the real `McpRegistry`, the real `EventBus` as
/// the sink, and the real `ProvenanceId::claimed` refusal in between.
#[tokio::test]
async fn a_remote_mcp_argument_asserting_an_absolute_path_is_reported_to_a_subscriber() {
    let bus = Arc::new(EventBus::new());
    let capture = CaptureTransport::default();
    let egress = Arc::new(Egress::new(
        capture.clone(),
        local_only_boundaries(),
        Arc::clone(&bus) as Arc<dyn PrivacyEventSink>,
    ));
    let registry = McpRegistry::with_egress(
        Arc::clone(&egress) as Arc<dyn EgressGate>,
        Some(SessionId::from("sess-remote")),
        vec![http_cfg("remote")],
    )
    .with_event_sink(Arc::clone(&bus) as Arc<dyn PrivacyEventSink>);

    // Subscribe BEFORE driving: the bus fans out to current subscribers only,
    // so a subscription taken afterwards would observe nothing and the test
    // would pass by missing the event rather than by finding it.
    let mut sub = bus.subscribe(256);

    let result = registry
        .call_tool(
            "mcp__remote__read_file",
            json!({ "path": "/repo/secrets/prod.env" }),
        )
        .await;

    // (1) Fail-closed: an assertion the daemon could not mint taints the call,
    // and an unknown-provenance call is refused whenever a boundary exists. The
    // reported locus is the content-free sentinel — there is no repo path to
    // name, which is precisely what was wrong with the argument.
    match result {
        Err(McpError::PrivacyBlocked { ref path, .. }) => {
            assert_eq!(path, "<unknown-provenance>");
        }
        other => panic!("expected a privacy block, got {other:?}"),
    }
    for req in capture.captured() {
        assert!(
            !String::from_utf8_lossy(&req.body).contains("secrets/prod.env"),
            "the refused assertion reached the wire"
        );
    }

    // (2) Delivered, not merely logged (AC-14, LESSON-505). The event names the
    // offending argument, the tool that carried it, and why it was refused —
    // enough for a user to fix the call rather than wonder why the session went
    // local.
    let rejections = drain_rejections(&mut sub).await;
    assert_eq!(rejections.len(), 1, "{rejections:?}");
    assert_eq!(rejections[0].source, "/repo/secrets/prod.env");
    assert_eq!(
        rejections[0].tool.as_deref(),
        Some("mcp__remote__read_file")
    );
    assert_eq!(rejections[0].reason, ProvenanceRejection::Absolute);
}

/// The same refusal on a **local** stdio server, whose call never goes near the
/// egress choke point.
///
/// This is why the report is published at the registry funnel rather than at
/// egress: a local server's refused assertion has no choke point to be noticed
/// at, and would otherwise surface — much later and much vaguer — as an
/// unknown-provenance block on whichever remote turn happened to assemble its
/// result. Here the call *succeeds*, and the refusal is still reported.
#[tokio::test]
async fn a_local_mcp_argument_is_reported_even_though_the_call_never_reaches_egress() {
    let bus = Arc::new(EventBus::new());
    let registry = McpRegistry::new(Arc::new(MockConnector), vec![stdio_cfg("local_fs")])
        .with_event_sink(Arc::clone(&bus) as Arc<dyn PrivacyEventSink>);

    let mut sub = bus.subscribe(256);

    let result = registry
        .call_tool(
            "mcp__local_fs__fetch",
            json!({ "resource": "../../etc/shadow" }),
        )
        .await;
    assert!(
        result.is_ok(),
        "a local call is not egress and is not blocked: {result:?}"
    );

    let rejections = drain_rejections(&mut sub).await;
    assert_eq!(rejections.len(), 1, "{rejections:?}");
    assert_eq!(rejections[0].source, "../../etc/shadow");
    assert_eq!(rejections[0].tool.as_deref(), Some("mcp__local_fs__fetch"));
    assert_eq!(rejections[0].reason, ProvenanceRejection::ParentTraversal);
}

/// Non-vacuity for both legs above: a call whose path arguments all mint cleanly
/// reports nothing at all.
///
/// Without this, an implementation that reported on *every* MCP call would pass
/// the two tests above — and the event would be noise a user learns to ignore,
/// which is the same as not having it.
#[tokio::test]
async fn a_clean_mcp_call_reports_nothing() {
    let bus = Arc::new(EventBus::new());
    let registry = McpRegistry::new(Arc::new(MockConnector), vec![stdio_cfg("local_fs")])
        .with_event_sink(Arc::clone(&bus) as Arc<dyn PrivacyEventSink>);

    let mut sub = bus.subscribe(256);
    registry
        .call_tool(
            "mcp__local_fs__fetch",
            json!({ "path": "./src/main.rs", "q": "some words" }),
        )
        .await
        .expect("a clean local call runs");

    assert!(drain_rejections(&mut sub).await.is_empty());
}

// ---------------------------------------------------------------------------
// AC-5 — the redundant inspection guard, with NO boundary configured
// ---------------------------------------------------------------------------

/// AC-5, driven through the real choke point onto a real bus subscription.
///
/// Two claims the mint-time leg above cannot make. First, this is the
/// *inspection*-time guard: the malformed value is already inside a
/// `Provenance`, past every constructor that would have refused it. Second,
/// there is **no boundary configured** — the configuration in which every other
/// provenance, including an unknown one, is forwarded untouched.
///
/// The value under test cannot be built by any typed path in the daemon; that is
/// the LESSON-508 point restated as a test rather than a comment. See the module
/// docs.
#[tokio::test]
async fn the_inspection_guard_refuses_and_reports_with_no_boundary_configured() {
    for (source, reason) in [
        ("/repo/secrets/prod.env", ProvenanceRejection::Absolute),
        (
            "sub/../secrets/prod.env",
            ProvenanceRejection::ParentTraversal,
        ),
    ] {
        let bus = Arc::new(EventBus::new());
        let capture = CaptureTransport::default();
        let egress = Egress::new(
            capture.clone(),
            // No boundaries at all.
            Vec::new(),
            Arc::clone(&bus) as Arc<dyn PrivacyEventSink>,
        );
        let mut sub = bus.subscribe(256);

        let provenance = Provenance::tainted_by(ProvenanceId::unvalidated_for_test(source));
        let request = TransportRequest {
            method: HttpMethod::Post,
            url: "https://api.anthropic.com/v1/messages".to_owned(),
            headers: vec![],
            body: b"derived from a boundary file".to_vec(),
        };
        let ctx = EgressContext::new("anthropic").with_session("sess-guard");

        let err = egress
            .send(request, &provenance, &ctx)
            .await
            .expect_err("a malformed source must fail closed");
        assert!(
            matches!(err, EgressError::PrivacyBlocked { ref path, .. }
                if path == "<malformed-provenance>"),
            "{source:?}: {err:?}"
        );
        assert!(capture.captured().is_empty(), "{source:?} reached the wire");

        let rejections = drain_rejections(&mut sub).await;
        assert_eq!(rejections.len(), 1, "{source:?}: {rejections:?}");
        assert_eq!(rejections[0].source, source);
        assert_eq!(rejections[0].reason, reason);
        // The choke point sees an assembled provenance long after it left any
        // one tool, so it names none rather than guessing.
        assert_eq!(rejections[0].tool, None);
    }
}

/// Non-vacuity for AC-5: with the same empty boundary set, a canonical source is
/// forwarded and nothing is reported. Without this the assertions above would
/// hold just as well against an egress that blocked everything.
#[tokio::test]
async fn with_no_boundary_configured_a_canonical_source_still_reaches_the_wire() {
    let bus = Arc::new(EventBus::new());
    let capture = CaptureTransport::default();
    let egress = Egress::new(
        capture.clone(),
        Vec::new(),
        Arc::clone(&bus) as Arc<dyn PrivacyEventSink>,
    );
    let mut sub = bus.subscribe(256);

    let provenance = Provenance::tainted_by(ProvenanceId::claimed("secrets/prod.env").unwrap());
    let request = TransportRequest {
        method: HttpMethod::Post,
        url: "https://api.anthropic.com/v1/messages".to_owned(),
        headers: vec![],
        body: b"public code".to_vec(),
    };
    egress
        .send(request, &provenance, &EgressContext::new("anthropic"))
        .await
        .expect("no boundaries configured: nothing to block");

    assert_eq!(capture.captured().len(), 1);
    assert!(drain_rejections(&mut sub).await.is_empty());
}
