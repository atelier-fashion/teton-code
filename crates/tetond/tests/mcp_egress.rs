//! AC-9 — MCP servers as egress-gated tool providers (ADR-003, BR-1).
//!
//! AC-9 has three limbs and this harness proves each with the real machinery, not
//! code inspection:
//!
//! 1. **Tools appear + execute under permission prompts.** A registered mock
//!    server's tools surface in the harness [`ToolRegistry`] under their
//!    `mcp__<server>__<tool>` names and are authorized through the same
//!    [`PermissionGate`] as any built-in tool — asked by default, run only on an
//!    allow.
//! 2. **`local-only` content never reaches a remote MCP server.** A remote
//!    (HTTP) server's `tools/call` flows through the *real* [`Egress`] choke point
//!    in front of a capture transport; a call whose arguments reference a
//!    boundary path is blocked before a byte leaves and raises a `privacy_block`,
//!    exactly as the AC-5 model-call test requires — same egress-capture
//!    verification.
//! 3. **The local/remote asymmetry.** A *local* stdio server may read a
//!    `local-only` file (nothing left the machine), but the result that enters
//!    context carries that file's provenance, so a *later* remote turn assembling
//!    it is caught at egress — the content cannot be laundered off the machine.
//!
//! A fourth test proves the lifecycle guarantee: one server crashing degrades only
//! its own tools; another server and the session keep working. A fifth drives a
//! *real* stdio subprocess end-to-end so the stdio transport is exercised for
//! real, not only through mocks.

use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use serde_json::{json, Value};

use teton_core::entities::{BoundaryMode, PrivacyBoundary};
use teton_protocol::events::{Event, PermissionRequest, PrivacyAction, PrivacyBlock};
use teton_protocol::methods::PermissionOutcome;
use teton_protocol::{ProviderId, SessionId};
use teton_providers::transport::{
    ByteStream, HttpMethod, Transport, TransportError, TransportRequest, TransportResponse,
};

use tetond::broadcast::EventBus;
use tetond::egress::provenance::assembled_provenance;
use tetond::egress::{Egress, EgressContext, EgressError, PrivacyEventSink, Provenance};
use tetond::harness::permissions::{
    PendingPermissions, PermissionConfig, PermissionDecision, PermissionGate, PermissionPolicy,
};
use tetond::harness::tools::mcp::{register_mcp_tools, result_context_block};
use tetond::harness::tools::{ToolContext, ToolRegistry};
use tetond::mcp::{
    EgressGate, McpConnection, McpError, McpRegistry, McpServerConfig, McpToolResult, McpTransport,
};

/// A secret that must never appear in captured egress.
const SECRET_ENV: &str = "API_KEY=sk-live-DO-NOT-LEAK-mcp-xyz";

fn local_only_boundaries() -> Vec<PrivacyBoundary> {
    vec![PrivacyBoundary {
        path_glob: "secrets/**".to_owned(),
        mode: BoundaryMode::LocalOnly,
    }]
}

// ---------------------------------------------------------------------------
// Mock MCP connection + connector (for the non-egress limbs)
// ---------------------------------------------------------------------------

/// A scripted MCP connection: canned handshake/list, and a `tools/call` that
/// echoes back a marker so a test can assert the call ran.
struct MockConnection {
    server: String,
}

#[async_trait]
impl McpConnection for MockConnection {
    async fn call(
        &self,
        method: &str,
        params: Value,
        _provenance: &Provenance,
    ) -> Result<Value, McpError> {
        Ok(match method {
            "initialize" => json!({ "serverInfo": { "name": self.server, "version": "1" } }),
            "tools/list" => json!({
                "tools": [
                    {
                        "name": "lookup",
                        "description": "look something up",
                        "inputSchema": { "type": "object", "properties": { "q": {"type":"string"} } }
                    }
                ]
            }),
            "tools/call" => {
                let name = params.get("name").and_then(Value::as_str).unwrap_or("");
                json!({
                    "content": [ { "type": "text", "text": format!("ran {name}") } ],
                    "isError": false
                })
            }
            _ => Value::Null,
        })
    }

    async fn notify(&self, _method: &str, _params: Value) -> Result<(), McpError> {
        Ok(())
    }
}

/// A connector handing out [`MockConnection`]s; a named server can be scripted to
/// fail its connection (to prove crash isolation).
#[derive(Default)]
struct MockConnector {
    broken: Mutex<Vec<String>>,
}

impl MockConnector {
    fn breaks(server: &str) -> Self {
        Self {
            broken: Mutex::new(vec![server.to_owned()]),
        }
    }
}

#[async_trait]
impl tetond::mcp::McpConnector for MockConnector {
    async fn connect(&self, config: &McpServerConfig) -> Result<Arc<dyn McpConnection>, McpError> {
        if self.broken.lock().unwrap().contains(&config.id) {
            return Err(McpError::Startup(format!("{}: mock down", config.id)));
        }
        Ok(Arc::new(MockConnection {
            server: config.id.clone(),
        }))
    }
}

fn stdio_cfg(id: &str) -> McpServerConfig {
    McpServerConfig {
        id: id.to_owned(),
        transport: McpTransport::Stdio {
            command: "unused-by-mock".to_owned(),
            args: vec![],
            env: std::collections::BTreeMap::new(),
        },
    }
}

// ---------------------------------------------------------------------------
// Capture transport + sink for the egress limb (mirrors egress_capture.rs)
// ---------------------------------------------------------------------------

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
        // A minimal valid JSON-RPC tools/call response.
        let body: ByteStream = Box::pin(futures::stream::once(async {
            Ok(
                br#"{"jsonrpc":"2.0","id":1,"result":{"content":[{"type":"text","text":"ok"}],"isError":false}}"#
                    .to_vec(),
            )
        }));
        Ok(TransportResponse { status: 200, body })
    }
}

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

fn contains_bytes(haystack: &[u8], needle: &str) -> bool {
    haystack
        .windows(needle.len())
        .any(|w| w == needle.as_bytes())
}

// ---------------------------------------------------------------------------
// AC-9.1 — registered tools appear and execute behind permission prompts
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn registered_mcp_tools_appear_and_run_under_permission_prompts() {
    let connector = Arc::new(MockConnector::default());
    let registry = Arc::new(McpRegistry::new(connector, vec![stdio_cfg("kb")]));

    // The server's tools surface in the harness tool set, namespaced.
    let mut tools = ToolRegistry::with_builtins();
    let registered = register_mcp_tools(
        &mut tools,
        Arc::clone(&registry),
        tokio::runtime::Handle::current(),
    )
    .await;
    assert_eq!(registered, vec!["mcp__kb__lookup".to_owned()]);
    assert!(tools.names().contains(&"mcp__kb__lookup"));

    // An MCP tool is authorized by the same gate as a built-in tool. With the
    // coding defaults it is `ask` (not on the read-only allowlist), so a
    // permission_request goes out and the tool runs only on an allow.
    let bus = Arc::new(EventBus::new());
    let pending = Arc::new(PendingPermissions::new());
    let gate = PermissionGate::new(
        SessionId::from("sess-mcp"),
        PermissionConfig::coding_defaults(),
        Arc::clone(&bus),
        Arc::clone(&pending),
    );
    let mut sub = bus.subscribe(16);

    let decide = gate.authorize("mcp__kb__lookup", Some("look something up".to_owned()));
    let drive = async {
        let env = sub.recv().await.unwrap();
        match env.event {
            Event::PermissionRequest(PermissionRequest {
                request_id,
                tool_name,
                ..
            }) => {
                assert_eq!(tool_name, "mcp__kb__lookup");
                assert!(pending.resolve(
                    &request_id,
                    PermissionOutcome::Selected {
                        option_id: "allow_once".to_owned()
                    }
                ));
            }
            other => panic!("expected permission_request, got {other:?}"),
        }
    };
    let (decision, ()) = tokio::join!(decide, drive);
    assert_eq!(decision, PermissionDecision::Allowed);

    // Once allowed, the namespaced tool executes through the full bridge — the
    // registry's `Tool::run` crossing sync→async — and its result comes back
    // wrapped in untrusted-content framing (never as executable instructions).
    let tool_ctx = ToolContext::new(std::env::temp_dir());
    let outcome = tools.dispatch("mcp__kb__lookup", &tool_ctx, &json!({ "q": "teton" }));
    assert!(!outcome.is_error, "{}", outcome.content);
    assert!(outcome.content.contains("ran lookup"));
    assert!(outcome.content.contains("trust=\"untrusted\""));
}

/// A denied prompt means the MCP tool must not run — the same gate contract as a
/// built-in tool.
#[tokio::test]
async fn a_denied_prompt_blocks_the_mcp_tool() {
    let mut cfg = PermissionConfig::coding_defaults();
    cfg.set("mcp__kb__lookup", PermissionPolicy::Deny);
    let bus = Arc::new(EventBus::new());
    let pending = Arc::new(PendingPermissions::new());
    let gate = PermissionGate::new(SessionId::from("s"), cfg, bus, pending);
    assert_eq!(
        gate.authorize("mcp__kb__lookup", None).await,
        PermissionDecision::Denied
    );
}

// ---------------------------------------------------------------------------
// AC-9.2 — local-only content never reaches a REMOTE MCP server (egress capture)
// ---------------------------------------------------------------------------

#[tokio::test]
async fn a_remote_mcp_call_touching_a_boundary_path_is_blocked_at_egress() {
    let capture = CaptureTransport::default();
    let sink = Arc::new(CapturingSink::default());
    let egress = Arc::new(Egress::new(
        capture.clone(),
        local_only_boundaries(),
        sink.clone(),
    ));

    // A remote (HTTP) MCP server, reached through the real egress choke point.
    let registry = McpRegistry::with_egress(
        Arc::clone(&egress) as Arc<dyn EgressGate>,
        Some(SessionId::from("sess-remote")),
        vec![McpServerConfig {
            id: "remote".to_owned(),
            transport: McpTransport::Http {
                endpoint: "https://mcp.example.com/rpc".to_owned(),
            },
        }],
    );

    // A clean call (no boundary path) reaches the captured wire.
    let clean = registry
        .call_tool("mcp__remote__lookup", json!({ "q": "public question" }))
        .await;
    assert!(clean.is_ok(), "a boundary-free remote call is allowed");

    // A call whose arguments reference a `local-only` path is refused before a
    // byte leaves, and surfaces as a privacy block.
    let blocked = registry
        .call_tool(
            "mcp__remote__lookup",
            json!({ "path": "secrets/prod.env", "content": SECRET_ENV }),
        )
        .await;
    match blocked {
        Err(McpError::PrivacyBlocked { path, server_id }) => {
            assert_eq!(path, "secrets/prod.env");
            assert_eq!(server_id, "remote");
        }
        other => panic!("expected a privacy block, got {other:?}"),
    }

    // Egress capture: the boundary path and its content never appear in ANY
    // captured payload — the blocked `tools/call` was refused before a byte left.
    // (Handshake `initialize`/`initialized` and the clean call are captured; the
    // blocked call is not — so it is its absence, not a fixed count, that matters.)
    let captured = capture.captured();
    for req in &captured {
        assert!(
            !contains_bytes(&req.body, SECRET_ENV),
            "boundary content leaked into a remote MCP call"
        );
        assert!(
            !contains_bytes(&req.body, "secrets/prod.env"),
            "the blocked call's boundary path reached the wire"
        );
    }
    // Positive control: the clean call's public content did go out.
    assert!(
        captured
            .iter()
            .any(|req| contains_bytes(&req.body, "public question")),
        "the clean call should have been forwarded"
    );

    // A privacy_block event was emitted, attributed to the MCP server + session.
    let events = sink.events();
    assert_eq!(events.len(), 1);
    let (session, block) = &events[0];
    assert_eq!(session.as_ref(), Some(&SessionId::from("sess-remote")));
    assert_eq!(block.path, "secrets/prod.env");
    assert_eq!(block.provider_id, ProviderId::from("remote"));
    assert_eq!(block.action, PrivacyAction::ReroutedToLocal);
}

// ---------------------------------------------------------------------------
// AC-9.3 — the local/remote asymmetry: a LOCAL server may read a boundary file,
// but its result cannot be laundered to a remote provider on a later turn.
// ---------------------------------------------------------------------------

#[tokio::test]
async fn a_local_mcp_result_reading_a_boundary_path_is_blocked_from_later_remote_egress() {
    // A local stdio MCP server ran a read of a `local-only` file. That was fine —
    // nothing left the machine. The result the bridge produces for context is
    // tagged with the file's provenance.
    let result = McpToolResult {
        text: SECRET_ENV.to_owned(),
        is_error: false,
    };
    let block = result_context_block(
        "mcp__local_fs__read_file",
        &json!({ "path": "secrets/prod.env" }),
        &result,
    );
    assert!(block.provenance().contains("secrets/prod.env"));

    // Now a LATER turn assembles that result block into a remote request. The
    // real egress choke point must block it — the secret cannot be laundered off
    // the machine just because a local tool produced it. (The bridge and egress
    // share the same `ContextBlock`/`Provenance` types, so the block flows
    // straight into context assembly.)
    let capture = CaptureTransport::default();
    let sink = Arc::new(CapturingSink::default());
    let egress = Egress::new(capture.clone(), local_only_boundaries(), sink.clone());

    let provenance = assembled_provenance(std::slice::from_ref(&block));
    let request = TransportRequest {
        method: HttpMethod::Post,
        url: "https://api.anthropic.com/v1/messages".to_owned(),
        headers: vec![],
        body: block.content().as_bytes().to_vec(),
    };
    let ctx = EgressContext::new("anthropic").with_session("sess-later");
    let result = egress.send(request, &provenance, &ctx).await;
    assert!(
        matches!(result, Err(EgressError::PrivacyBlocked { ref path, .. }) if path == "secrets/prod.env"),
        "a local MCP result reading a boundary file must not reach a remote provider, got {result:?}"
    );
    assert!(capture.captured().is_empty(), "nothing may reach the wire");
}

/// REQ-544 C-1 (H-1 laundering): a local MCP result that read a boundary path
/// passed under a **non-`path`** argument key must still be tagged and blocked —
/// the old narrow "`path` argument only" tagging let this leak with empty
/// provenance.
#[tokio::test]
async fn a_local_mcp_result_reading_a_boundary_path_under_an_arbitrary_key_is_blocked() {
    // The server was called with the boundary path under `resource` — neither a
    // path-like key nor a `path` arg. `call_provenance` still catches it because
    // the value is path-shaped, so the result block carries that provenance.
    let result = McpToolResult {
        text: SECRET_ENV.to_owned(),
        is_error: false,
    };
    let block = result_context_block(
        "mcp__local_fs__fetch",
        &json!({ "resource": "secrets/prod.env" }),
        &result,
    );
    assert!(
        block.provenance().contains("secrets/prod.env"),
        "a boundary path under an arbitrary key must still be tagged"
    );

    // A later remote turn assembling that block is blocked at the real egress.
    let capture = CaptureTransport::default();
    let sink = Arc::new(CapturingSink::default());
    let egress = Egress::new(capture.clone(), local_only_boundaries(), sink.clone());

    let provenance = assembled_provenance(std::slice::from_ref(&block));
    let request = TransportRequest {
        method: HttpMethod::Post,
        url: "https://api.anthropic.com/v1/messages".to_owned(),
        headers: vec![],
        body: block.content().as_bytes().to_vec(),
    };
    let ctx = EgressContext::new("anthropic").with_session("sess-h1");
    let result = egress.send(request, &provenance, &ctx).await;
    assert!(
        matches!(result, Err(EgressError::PrivacyBlocked { ref path, .. }) if path == "secrets/prod.env"),
        "an MCP result reading a non-`path`-keyed boundary path must be blocked, got {result:?}"
    );
    assert!(capture.captured().is_empty(), "nothing may reach the wire");
    assert_eq!(sink.events().len(), 1, "exactly one privacy_block");
}

// ---------------------------------------------------------------------------
// Lifecycle — a crashing server degrades only its own tools; session continues.
// ---------------------------------------------------------------------------

#[tokio::test]
async fn a_crashing_server_degrades_only_its_own_tools() {
    let connector = Arc::new(MockConnector::breaks("down"));
    let registry = McpRegistry::new(connector, vec![stdio_cfg("down"), stdio_cfg("up")]);

    let discovered = registry.list_tools().await;
    let names: Vec<String> = discovered.iter().map(|d| d.namespaced_name()).collect();

    // The healthy server's tool is present; the broken one's is absent — the
    // session keeps its remaining tools rather than failing wholesale.
    assert!(names.contains(&"mcp__up__lookup".to_owned()));
    assert!(!names.iter().any(|n| n.starts_with("mcp__down__")));

    // A call to the healthy server still works.
    let ok = registry.call_tool("mcp__up__lookup", json!({})).await;
    assert!(ok.is_ok());
}

// ---------------------------------------------------------------------------
// The stdio transport, for real: spawn a subprocess and speak the protocol.
// ---------------------------------------------------------------------------

/// A tiny mock MCP server as a shell script, kept in lockstep with the client by
/// `read`ing one line per incoming message. The client uses sequential ids
/// (initialize=1, tools/list=2, tools/call=3; the `initialized` notification has
/// no id), which the script's hardcoded response ids match.
const MOCK_STDIO_SERVER: &str = r#"#!/bin/sh
read _initialize
printf '%s\n' '{"jsonrpc":"2.0","id":1,"result":{"protocolVersion":"2024-11-05","serverInfo":{"name":"stdio-mock","version":"0.1"},"capabilities":{"tools":{}}}}'
read _initialized
read _list
printf '%s\n' '{"jsonrpc":"2.0","id":2,"result":{"tools":[{"name":"echo","description":"echoes","inputSchema":{"type":"object"}}]}}'
read _call
printf '%s\n' '{"jsonrpc":"2.0","id":3,"result":{"content":[{"type":"text","text":"hello from a real subprocess"}],"isError":false}}'
"#;

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_real_stdio_subprocess_speaks_the_protocol() {
    use std::io::Write;
    use tetond::mcp::{McpClient, StdioConnection};

    // Write the mock server script to a temp file.
    let dir = std::env::temp_dir().join(format!(
        "teton-mcp-stdio-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    std::fs::create_dir_all(&dir).unwrap();
    let script = dir.join("server.sh");
    let mut f = std::fs::File::create(&script).unwrap();
    f.write_all(MOCK_STDIO_SERVER.as_bytes()).unwrap();
    drop(f);

    // Spawn `sh <script>` as the local server.
    let conn = StdioConnection::spawn(
        "stdio-mock",
        "sh",
        &[script.to_string_lossy().into_owned()],
        &std::collections::BTreeMap::new(),
    )
    .expect("spawn stdio mock")
    .with_request_timeout(std::time::Duration::from_secs(5));
    let client = McpClient::new("stdio-mock", Arc::new(conn));

    let info = client.initialize().await.expect("initialize");
    assert_eq!(info.name, "stdio-mock");

    let tools = client.list_tools().await.expect("list tools");
    assert_eq!(tools.len(), 1);
    assert_eq!(tools[0].name, "echo");

    let result = client
        .call_tool("echo", json!({ "text": "hi" }))
        .await
        .expect("call tool");
    assert_eq!(result.text, "hello from a real subprocess");
    assert!(!result.is_error);

    std::fs::remove_dir_all(&dir).ok();
}
