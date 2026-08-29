//! The MCP JSON-RPC protocol client and its two transports.
//!
//! MCP is JSON-RPC 2.0. This module speaks the three methods a tool-provider
//! consumer needs — `initialize`, `tools/list`, `tools/call` — over a
//! [`McpConnection`] seam with two implementations:
//!
//! - [`StdioConnection`] — a **local** server: spawn the configured subprocess and
//!   exchange newline-delimited JSON-RPC over its stdio. No egress.
//! - [`HttpConnection`] — a **remote** server: POST JSON-RPC through the single
//!   [`crate::egress`] choke point ([`EgressGate`]), so a `tools/call` carrying
//!   `local-only` provenance is blocked before a byte leaves the machine (BR-1).
//!
//! The [`McpConnection`] seam is what makes the registry and bridge testable
//! without real subprocesses or sockets, and what lets the same [`McpClient`]
//! protocol logic drive either transport.
//!
//! ## Provenance
//!
//! Every [`McpClient::call_tool`] computes the [`Provenance`] of the call from its
//! path-shaped arguments ([`call_provenance`]). For a remote server that
//! provenance rides into egress; for a local server the bridge reuses it to tag
//! the result that enters context. Either way, a boundary path a tool touched is
//! never laundered to a remote provider (BR-1).

use std::collections::{BTreeMap, BTreeSet};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use futures::StreamExt;
use serde_json::{json, Value};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::process::{Child, ChildStdin, ChildStdout};
use tokio::sync::Mutex;

use teton_core::ProvenanceId;
use teton_protocol::events::ProvenanceRejection;
use teton_protocol::{ProviderId, SessionId};
use teton_providers::transport::{
    BlockDetail, ByteStream, HttpMethod, TransportRequest, TransportResponse,
};

use crate::egress::{
    rejection_reason, sanitize_reported_source, Egress, EgressContext, EgressError, Provenance,
};
use teton_providers::transport::Transport;

/// The MCP protocol revision Teton Code advertises in `initialize`.
const MCP_PROTOCOL_VERSION: &str = "2024-11-05";

/// Default per-request deadline. A server that does not answer in time is treated
/// as unhealthy (crash/hang), so a wedged tool provider can never hang the loop.
const DEFAULT_REQUEST_TIMEOUT: Duration = Duration::from_secs(30);

/// A failure talking to an MCP server.
///
/// Every variant is **content-free by construction** (BR-1 / conventions): it
/// carries at most a method name, a server id, a config-authored boundary path, or
/// a failure class — never a byte of MCP request/response payload. So an
/// `McpError` may be logged, surfaced to a client, or folded into a tool result
/// without leaking boundary content.
#[derive(Debug, thiserror::Error)]
pub enum McpError {
    /// A tool name did not match the `mcp__<server>__<tool>` namespace.
    #[error("`{0}` is not a namespaced MCP tool (expected `mcp__<server>__<tool>`)")]
    NotNamespaced(String),
    /// The configured server could not be spawned/connected.
    #[error("MCP server `{0}` failed to start")]
    Startup(String),
    /// The transport failed (write/read/connect), i.e. the connection is lost.
    #[error("MCP server `{0}` transport error")]
    Transport(String),
    /// The server did not answer within the deadline.
    #[error("MCP server `{0}` timed out")]
    Timeout(String),
    /// The server closed the connection (process exited / EOF).
    #[error("MCP server `{0}` closed the connection")]
    Closed(String),
    /// The server spoke malformed JSON-RPC or an unexpected shape.
    #[error("MCP server `{0}` returned a malformed protocol message")]
    Protocol(String),
    /// The server answered the request with a JSON-RPC `error`. The message names
    /// the server and the JSON-RPC error *code* only — never the error `data`.
    #[error("MCP server `{server_id}` returned JSON-RPC error {code}")]
    Server {
        /// Server that produced the error.
        server_id: String,
        /// JSON-RPC error code.
        code: i64,
    },
    /// A call to a remote server was refused at the egress choke point — by the
    /// provenance inspection (BR-1) or by the REQ-562 redaction scan.
    ///
    /// `detail` is which of the two, and for the scan, which way it went. An MCP
    /// tool call is remote egress like any other (ADR-003), so all three of the
    /// choke point's refusals reach here — and a redaction block reported as
    /// "privacy boundary blocked …" sends the user hunting for a `local-only`
    /// glob that does not exist. Worse still is telling someone whose scan
    /// *could not run* that something was found: those are different problems
    /// with different fixes, which is the rule REQ-562 BR-3 exists to keep.
    #[error("{}", mcp_block_sentence(*.detail, .path, .server_id))]
    PrivacyBlocked {
        /// Repo-relative boundary path that would have leaked, or — for a
        /// redaction block — the non-secret locus naming the outbound payload.
        path: String,
        /// The remote MCP server the content would have reached.
        server_id: String,
        /// Which inspection refused it (REQ-562 BR-3). [`BlockDetail`] is the
        /// vocabulary that crosses the `teton-providers` seam: enough to choose
        /// the right sentence, and structurally unable to carry payload content
        /// — it has no fields.
        detail: BlockDetail,
    },
}

/// The sentence [`McpError::PrivacyBlocked`] renders, one per cause
/// (REQ-562 BR-3).
///
/// Three refusals, three sentences — the same posture as the daemon's
/// `refusal_clause` and the choke point's own `privacy_blocked_sentence`, and
/// for the same reason: a noun swapped inside one sentence drifts into claiming
/// the scan found something when it never ran.
///
/// The boundary wording is REQ-544's, preserved verbatim — it is the sentence
/// already in every log and every existing test.
///
/// Nothing here can interpolate payload content. `path` is a config-authored
/// glob target or the fixed locus phrase, and [`BlockDetail`] has no fields at
/// all, so BR-6 holds by the shape of the inputs rather than by care here.
fn mcp_block_sentence(detail: BlockDetail, path: &str, server_id: &str) -> String {
    match detail {
        BlockDetail::Boundary => {
            format!(
                "privacy boundary blocked an MCP call touching `{path}` to server `{server_id}`"
            )
        }
        BlockDetail::Redaction => format!(
            "the redaction scan found sensitive content in an MCP call to server \
             `{server_id}` ({path}), so it was refused"
        ),
        BlockDetail::ScanUnavailable => format!(
            "the redaction scan could not run on an MCP call to server `{server_id}` \
             ({path}), so it was refused unscanned"
        ),
    }
}

impl McpError {
    /// Whether this error means the underlying connection is dead and the server
    /// should be marked degraded and reconnected on next demand (crash/restart).
    ///
    /// A per-call fault ([`McpError::Server`], [`McpError::Protocol`],
    /// [`McpError::PrivacyBlocked`], [`McpError::NotNamespaced`]) does **not** kill
    /// the connection — only transport-level loss does.
    #[must_use]
    pub fn is_connection_lost(&self) -> bool {
        matches!(
            self,
            McpError::Startup(_)
                | McpError::Transport(_)
                | McpError::Timeout(_)
                | McpError::Closed(_)
        )
    }
}

/// One tool advertised by an MCP server (`tools/list` entry).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct McpTool {
    /// The server-local tool name (not yet namespaced).
    pub name: String,
    /// Model-facing description.
    pub description: String,
    /// JSON Schema for the tool's arguments.
    pub input_schema: Value,
}

impl McpTool {
    /// Parse one `tools/list` entry. A missing description or schema degrades to
    /// sensible defaults rather than failing the whole listing.
    fn from_value(value: &Value) -> Result<Self, ()> {
        let name = value.get("name").and_then(Value::as_str).ok_or(())?;
        Ok(Self {
            name: name.to_owned(),
            description: value
                .get("description")
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_owned(),
            input_schema: value
                .get("inputSchema")
                .cloned()
                .unwrap_or_else(|| json!({"type": "object"})),
        })
    }
}

/// The result of a `tools/call`: the joined text content and the error flag.
///
/// MCP returns content as a list of typed parts; the MVP flattens the `text`
/// parts (deferring non-text content with the rest of resources/prompts) and
/// preserves the server's `isError` signal so the harness can tell a tool failure
/// from a success.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct McpToolResult {
    /// The joined text content the server returned.
    pub text: String,
    /// Whether the server flagged the call as an error.
    pub is_error: bool,
}

impl McpToolResult {
    /// Parse a `tools/call` result object (`{content: [...], isError: bool}`).
    fn from_value(value: &Value) -> Result<Self, McpError> {
        let is_error = value
            .get("isError")
            .and_then(Value::as_bool)
            .unwrap_or(false);
        let mut text = String::new();
        if let Some(parts) = value.get("content").and_then(Value::as_array) {
            for part in parts {
                if let Some(t) = part.get("text").and_then(Value::as_str) {
                    if !text.is_empty() {
                        text.push('\n');
                    }
                    text.push_str(t);
                }
            }
        }
        Ok(Self { text, is_error })
    }
}

/// Server identity reported by `initialize`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct McpServerInfo {
    /// Human-facing server name.
    pub name: String,
    /// Server version string.
    pub version: String,
}

// ---------------------------------------------------------------------------
// The connection seam
// ---------------------------------------------------------------------------

/// The low-level JSON-RPC seam an [`McpClient`] drives.
///
/// A request/response `call` and a fire-and-forget `notify`. `provenance` on
/// `call` is the provenance of any content in `params` that would leave the
/// machine: empty for handshake calls, the tool-argument provenance for a
/// `tools/call`. The [`HttpConnection`] enforces it at egress; the
/// [`StdioConnection`] ignores it (local, no egress).
///
/// Implemented by the two real transports and by test doubles — which is what
/// keeps the registry and bridge testable without real processes or sockets.
#[async_trait]
pub trait McpConnection: Send + Sync {
    /// Send a JSON-RPC request and await its result value (the `result` field,
    /// already unwrapped from the envelope).
    async fn call(
        &self,
        method: &str,
        params: Value,
        provenance: &Provenance,
    ) -> Result<Value, McpError>;

    /// Send a JSON-RPC notification (no id, no response).
    async fn notify(&self, method: &str, params: Value) -> Result<(), McpError>;
}

/// The protocol client: `initialize`, `tools/list`, `tools/call` over any
/// [`McpConnection`].
pub struct McpClient {
    server_id: String,
    conn: Arc<dyn McpConnection>,
}

impl McpClient {
    /// A client for `server_id` over `conn`.
    #[must_use]
    pub fn new(server_id: impl Into<String>, conn: Arc<dyn McpConnection>) -> Self {
        Self {
            server_id: server_id.into(),
            conn,
        }
    }

    /// The server id this client talks to.
    #[must_use]
    pub fn server_id(&self) -> &str {
        &self.server_id
    }

    /// Perform the MCP handshake: send `initialize`, then the
    /// `notifications/initialized` acknowledgement. Handshake content is
    /// synthetic (no file provenance), so it never touches a boundary.
    pub async fn initialize(&self) -> Result<McpServerInfo, McpError> {
        let params = json!({
            "protocolVersion": MCP_PROTOCOL_VERSION,
            "capabilities": {},
            "clientInfo": { "name": "teton-code", "version": crate::version() },
        });
        let result = self
            .conn
            .call("initialize", params, &Provenance::empty())
            .await?;
        let info = result.get("serverInfo");
        let server_info = McpServerInfo {
            name: info
                .and_then(|i| i.get("name"))
                .and_then(Value::as_str)
                .unwrap_or(&self.server_id)
                .to_owned(),
            version: info
                .and_then(|i| i.get("version"))
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_owned(),
        };
        self.conn
            .notify("notifications/initialized", json!({}))
            .await?;
        Ok(server_info)
    }

    /// List the server's tools (`tools/list`). Entries that do not parse are
    /// skipped rather than failing the whole listing.
    pub async fn list_tools(&self) -> Result<Vec<McpTool>, McpError> {
        let result = self
            .conn
            .call("tools/list", json!({}), &Provenance::empty())
            .await?;
        let mut tools = Vec::new();
        if let Some(items) = result.get("tools").and_then(Value::as_array) {
            for item in items {
                if let Ok(tool) = McpTool::from_value(item) {
                    tools.push(tool);
                }
            }
        }
        Ok(tools)
    }

    /// Call a tool (`tools/call`). The call's [`Provenance`] is computed from its
    /// path-shaped arguments ([`call_provenance`]) and rides into the connection —
    /// so a remote call touching a `local-only` path is blocked at egress (BR-1).
    pub async fn call_tool(&self, tool: &str, arguments: Value) -> Result<McpToolResult, McpError> {
        let provenance = call_provenance(&arguments);
        let params = json!({ "name": tool, "arguments": arguments });
        let result = self.conn.call("tools/call", params, &provenance).await?;
        McpToolResult::from_value(&result)
    }
}

// ---------------------------------------------------------------------------
// Tool namespacing
// ---------------------------------------------------------------------------

/// The delimiter between the `mcp` prefix, the server id, and the tool name.
const NAMESPACE_SEP: &str = "__";

/// Build the namespaced tool name `mcp__<server>__<tool>` the harness exposes.
#[must_use]
pub fn namespaced_tool_name(server_id: &str, tool: &str) -> String {
    format!("mcp{NAMESPACE_SEP}{server_id}{NAMESPACE_SEP}{tool}")
}

/// Split a namespaced tool name back into `(server_id, tool)`.
///
/// Returns `None` if `name` is not `mcp__<server>__<tool>`. The server id may not
/// contain the separator; the tool name may (only the first two `__` boundaries
/// are significant), so a tool named `list__all` on server `fs` round-trips.
#[must_use]
pub fn parse_namespaced_tool_name(name: &str) -> Option<(&str, &str)> {
    let rest = name.strip_prefix("mcp")?.strip_prefix(NAMESPACE_SEP)?;
    let sep = rest.find(NAMESPACE_SEP)?;
    let server = &rest[..sep];
    let tool = &rest[sep + NAMESPACE_SEP.len()..];
    if server.is_empty() || tool.is_empty() {
        return None;
    }
    Some((server, tool))
}

// ---------------------------------------------------------------------------
// Provenance from tool arguments
// ---------------------------------------------------------------------------

/// Compute the [`Provenance`] of an MCP tool call from its arguments.
///
/// A black-box MCP server does not tell us which files it touched, so we treat
/// every path-shaped argument value as a source the call references. Two signals:
/// a value under a path-like key (`path`, `file`, `dir`, …), and any string value
/// that carries a path separator. Over-tagging a non-boundary path is harmless —
/// egress only blocks *boundary* sources — while under-tagging a boundary path
/// would break BR-1, so the bias is deliberately toward tagging.
///
/// This is the same provenance the remote path sends to egress and the local path
/// stamps onto the result block, so a boundary path a tool touched is caught in
/// either direction.
#[must_use]
pub fn call_provenance(arguments: &Value) -> Provenance {
    call_provenance_detailed(arguments).provenance
}

/// An argument value that looked like a path but could not be minted into a
/// [`ProvenanceId`] (REQ-571 ADR-D).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RejectedSource {
    /// The offending argument value, already sanitized for reporting.
    pub source: String,
    /// Which canonical-form rule it broke.
    pub reason: ProvenanceRejection,
}

/// An MCP call's [`Provenance`] together with the assertions that were
/// **refused** while computing it.
///
/// The two halves answer different questions and only one of them can be
/// recovered later. The provenance says what the call may touch; the rejections
/// say what it *claimed* to touch in a form the daemon could not verify — and
/// once a rejection has tainted the provenance `Unknown`, that taint is
/// indistinguishable from a `shell` result's. Keeping the rejections beside the
/// provenance is what lets the caller report which assertion caused it, on the
/// event a user actually sees (LESSON-505).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CallProvenance {
    /// The provenance the call carries.
    pub provenance: Provenance,
    /// Path-shaped argument values that could not be minted, in traversal
    /// order.
    pub rejected: Vec<RejectedSource>,
}

/// [`call_provenance`], keeping the refused assertions rather than only their
/// effect.
///
/// One traversal backs both entry points, so the set of values treated as paths
/// and the set reported as refused cannot drift into disagreeing.
#[must_use]
pub fn call_provenance_detailed(arguments: &Value) -> CallProvenance {
    let mut out = CallProvenance {
        provenance: Provenance::empty(),
        rejected: Vec::new(),
    };
    collect_paths(None, arguments, &mut out);
    out
}

/// Argument keys whose string values are treated as paths regardless of shape.
const PATH_KEYS: &[&str] = &[
    "path",
    "paths",
    "file",
    "files",
    "filepath",
    "filename",
    "dir",
    "directory",
    "cwd",
    "root",
    "source",
    "target",
];

/// Recursively collect path-shaped string values into `prov`. `key` is the object
/// key the current `value` sits under, when any.
///
/// ## An un-mintable claim fails toward taint (REQ-571 ADR-A/ADR-D)
///
/// [`ProvenanceId::claimed`] is the constructor for a path a third party
/// *asserted* — a remote MCP server names what it touched and nothing on this
/// machine can confirm it. When the assertion cannot be minted at all (it is
/// absolute, or retains a `..`), the honest answer is not "there is no source
/// here": an absolute argument may well name a boundary file (`/repo/secrets/x`),
/// and before this it was tagged verbatim and then silently matched no
/// repo-relative glob — the BR-2 hole, on the MCP path. So the whole call's
/// provenance is marked **unknown**, which egress fail-closes exactly as it does
/// a `shell` result. Dropping the value instead would be failing open.
///
/// This bites only when a boundary is configured — the inspector runs nowhere
/// else — and the cost of the conservative answer is a session pinned local, the
/// same posture REQ-544 C-1 already takes.
///
/// ## And the taint is reported, not only taken (REQ-571 TASK-122)
///
/// The refusal is recorded in [`CallProvenance::rejected`] as well as folded
/// into the unknown bit, because the two are not the same information: once the
/// provenance is `Unknown` it says only "something here was unattributable",
/// which is what a `shell` result says too. Whoever is holding the tool name —
/// [`McpRegistry::call_tool`](crate::mcp::McpRegistry::call_tool) — turns the
/// record into the `provenance_rejected` event, so a session that goes local
/// says *which* assertion did it rather than leaving the user to guess.
fn collect_paths(key: Option<&str>, value: &Value, out: &mut CallProvenance) {
    match value {
        Value::String(s) => {
            let key_is_path =
                key.is_some_and(|k| PATH_KEYS.contains(&k.to_ascii_lowercase().as_str()));
            if key_is_path || looks_like_path(s) {
                match ProvenanceId::claimed(s) {
                    Ok(id) => out.provenance.merge(&Provenance::tainted_by(id)),
                    Err(e) => {
                        out.provenance.mark_unknown();
                        out.rejected.push(RejectedSource {
                            source: sanitize_reported_source(s),
                            reason: rejection_reason(&e),
                        });
                    }
                }
            }
        }
        Value::Array(items) => {
            for item in items {
                collect_paths(key, item, out);
            }
        }
        Value::Object(map) => {
            for (k, v) in map {
                collect_paths(Some(k), v, out);
            }
        }
        _ => {}
    }
}

/// Whether a string is shaped like a repo-relative filesystem path: non-empty, no
/// whitespace, bounded length, carries a path separator, and is not a URL (a
/// `scheme://` value is a remote reference, not a local file).
fn looks_like_path(s: &str) -> bool {
    if s.is_empty() || s.len() > 4096 {
        return false;
    }
    if s.chars().any(char::is_whitespace) {
        return false;
    }
    if s.contains("://") {
        return false;
    }
    s.contains('/') || s.contains('\\')
}

// ---------------------------------------------------------------------------
// The egress seam for the HTTP transport
// ---------------------------------------------------------------------------

/// The egress capability the [`HttpConnection`] needs, object-safe so the
/// connection is not generic over the concrete transport `T`.
///
/// Implemented by [`Egress`] for every `T: Transport`, so a remote MCP call rides
/// the exact same choke point a remote *model* call does — one place, one BR-1
/// enforcement.
#[async_trait]
pub trait EgressGate: Send + Sync {
    /// Dispatch `request` under `provenance`; a boundary intersection is refused
    /// before any network activity (see [`Egress::send`]).
    async fn send_request(
        &self,
        request: TransportRequest,
        provenance: &Provenance,
        ctx: &EgressContext,
    ) -> Result<TransportResponse, EgressError>;
}

#[async_trait]
impl<T: Transport> EgressGate for Egress<T> {
    async fn send_request(
        &self,
        request: TransportRequest,
        provenance: &Provenance,
        ctx: &EgressContext,
    ) -> Result<TransportResponse, EgressError> {
        self.send(request, provenance, ctx).await
    }
}

// ---------------------------------------------------------------------------
// HTTP (streamable-HTTP) transport — remote, egress-gated
// ---------------------------------------------------------------------------

/// A remote MCP server reached over streamable-HTTP, **through egress**.
///
/// Each JSON-RPC message is POSTed via the [`EgressGate`], so a `tools/call`
/// whose provenance intersects a `local-only` boundary is blocked before a byte
/// leaves the machine and surfaces as [`McpError::PrivacyBlocked`] (BR-1). The
/// connection holds no HTTP client of its own — that is egress's alone.
pub struct HttpConnection {
    egress: Arc<dyn EgressGate>,
    endpoint: String,
    server_id: String,
    session_id: Option<SessionId>,
    next_id: AtomicU64,
}

impl HttpConnection {
    /// A connection to `endpoint` for `server_id`, sending through `egress`.
    #[must_use]
    pub fn new(
        server_id: impl Into<String>,
        endpoint: impl Into<String>,
        egress: Arc<dyn EgressGate>,
    ) -> Self {
        Self {
            egress,
            endpoint: endpoint.into(),
            server_id: server_id.into(),
            session_id: None,
            next_id: AtomicU64::new(1),
        }
    }

    /// Scope the connection's egress events/blocks to `session_id`.
    #[must_use]
    pub fn with_session(mut self, session_id: impl Into<SessionId>) -> Self {
        self.session_id = Some(session_id.into());
        self
    }

    fn egress_context(&self) -> EgressContext {
        let mut ctx = EgressContext::new(ProviderId::from(self.server_id.as_str()));
        if let Some(session) = &self.session_id {
            ctx = ctx.with_session(session.clone());
        }
        ctx
    }

    async fn post(&self, message: &Value, provenance: &Provenance) -> Result<Vec<u8>, McpError> {
        let body = serde_json::to_vec(message).map_err(|e| McpError::Protocol(format!("{e}")))?;
        let request = TransportRequest {
            method: HttpMethod::Post,
            url: self.endpoint.clone(),
            // Auth is attached by egress, not here (BR-7). Accept both response
            // shapes streamable-HTTP may use.
            headers: vec![
                ("content-type".to_owned(), "application/json".to_owned()),
                (
                    "accept".to_owned(),
                    "application/json, text/event-stream".to_owned(),
                ),
            ],
            body,
        };
        let response = self
            .egress
            .send_request(request, provenance, &self.egress_context())
            .await
            .map_err(|e| self.map_egress_error(e))?;
        collect_body(response.body).await
    }

    /// Project an [`EgressError`] onto this module's vocabulary.
    ///
    /// The cause is carried across as a [`BlockDetail`] — the same projection
    /// [`EgressError::into_transport_error`] performs, and the same three
    /// values, because the alternative is this seam reporting every refusal as
    /// a boundary block (REQ-562 BR-3). The finding's kind and span stop here:
    /// the `privacy_block` event already carries them straight from the choke
    /// point, and a byte range has no business travelling through a tool result.
    fn map_egress_error(&self, err: EgressError) -> McpError {
        match err {
            EgressError::PrivacyBlocked { ref path, .. } => McpError::PrivacyBlocked {
                path: path.clone(),
                server_id: self.server_id.clone(),
                detail: match err.into_transport_error() {
                    teton_providers::transport::TransportError::PrivacyBlocked(detail) => detail,
                    // Unreachable: `into_transport_error` maps every
                    // `PrivacyBlocked` to a `PrivacyBlocked`. Named rather than
                    // `unreachable!` so a future re-mapping degrades to the
                    // *stricter* reading — a scan that could not run — instead
                    // of panicking the daemon on a tool call.
                    _ => BlockDetail::ScanUnavailable,
                },
            },
            other => McpError::Transport(other.to_string()),
        }
    }
}

#[async_trait]
impl McpConnection for HttpConnection {
    async fn call(
        &self,
        method: &str,
        params: Value,
        provenance: &Provenance,
    ) -> Result<Value, McpError> {
        let id = self.next_id.fetch_add(1, Ordering::SeqCst);
        let message = json!({
            "jsonrpc": "2.0",
            "id": id,
            "method": method,
            "params": params,
        });
        let bytes = self.post(&message, provenance).await?;
        let response = extract_jsonrpc(&bytes, &self.server_id)?;
        rpc_result(&response, &self.server_id)
    }

    async fn notify(&self, method: &str, params: Value) -> Result<(), McpError> {
        let message = json!({
            "jsonrpc": "2.0",
            "method": method,
            "params": params,
        });
        // A notification carries no id and no file content: empty provenance.
        self.post(&message, &Provenance::empty()).await?;
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// stdio transport — local, no egress
// ---------------------------------------------------------------------------

/// The env var names a spawned MCP server may inherit from the daemon — a
/// positive allowlist of the essentials a subprocess needs to run, and nothing
/// that could carry a credential (REQ-544 MED-2, BR-7). Provider API keys are
/// not on the list, so they are never passed on.
const MCP_BASE_ENV_ALLOW: &[&str] = &[
    "PATH", "HOME", "TMPDIR", "TZ", "TERM", "USER", "LOGNAME", "SHELL", "LANG", "LANGUAGE",
    "LC_ALL", "LC_CTYPE",
];

/// Compose the minimal environment a spawned MCP server receives: the allowlisted
/// essentials drawn from `daemon_vars`, then the per-server `declared` vars
/// layered on top (a declared var may override a base one). Nothing outside the
/// allowlist and the declared set is passed on — so the daemon's provider keys
/// never reach the child (REQ-544 MED-2).
///
/// `PATH` is on the allowlist, which means this inherits the daemon's own — and
/// under launchd that names no package-manager prefix, so an MCP server invoked
/// as `npx …` or any other brew-installed command cannot be launched at all
/// (BUG-174). It is floored before `declared` is layered on, so a server that
/// declares its own `PATH` still overrides it untouched.
///
/// Not pure — the floor asks the filesystem which prefixes exist here — but the
/// allowlist and override semantics remain testable without a subprocess, and
/// the floor's own decision is unit-tested in [`crate::env_path`].
fn compose_child_env<I>(
    daemon_vars: I,
    declared: &BTreeMap<String, String>,
) -> Vec<(String, String)>
where
    I: IntoIterator<Item = (String, String)>,
{
    // REQ-596: the composition itself moved to `crate::child_env`, shared with
    // the `shell` tool so the two spawn paths cannot drift apart again. This
    // stays as a named function because the *allowlist* is this path's own —
    // passing it as a parameter is what keeps a change made for the shell tool
    // from silently widening what a third-party MCP server inherits (BR-7.1).
    //
    // The credential set is deliberately **empty** here. BR-1 is a rule about
    // the `shell` child, and changing the MCP path is out of this REQ's scope;
    // an empty set is what makes the byte-identical guard below true rather than
    // aspirational. The MCP path already excludes provider keys by allowlist.
    crate::child_env::compose_child_env(daemon_vars, MCP_BASE_ENV_ALLOW, &BTreeSet::new(), declared)
}

/// A local MCP server spawned as a subprocess, speaking newline-delimited
/// JSON-RPC over its stdio.
///
/// This is **not** egress — a local server may read `local-only` files because
/// nothing leaves the machine. Its results still get provenance-tagged by the
/// bridge so a later remote turn cannot launder them. Reads are deadline-bounded,
/// so a wedged server surfaces as [`McpError::Timeout`] rather than hanging the
/// loop; the child is killed on drop.
pub struct StdioConnection {
    server_id: String,
    io: Mutex<StdioIo>,
    request_timeout: Duration,
}

/// The child process and its framed pipes, serialized behind one async mutex.
struct StdioIo {
    child: Child,
    stdin: ChildStdin,
    stdout: BufReader<ChildStdout>,
    next_id: u64,
}

impl StdioConnection {
    /// Spawn `command args…` as a local MCP server and connect to its stdio.
    ///
    /// The child gets a **minimal** environment (REQ-544 MED-2, BR-7): only the
    /// PATH/HOME/locale essentials from the daemon's environment, plus the
    /// per-server `env` the config declares. The daemon's provider API keys are
    /// **never** inherited — a third-party `npx`/`uvx` package cannot read them.
    /// (Since REQ-596 the `shell` tool composes its child the same way, through
    /// the same function; this path's allowlist was the model for it rather than
    /// the exception to it. An MCP server is a user-declared program, but it
    /// still has no business seeing provider credentials it did not declare.)
    ///
    /// # Errors
    /// Returns [`McpError::Startup`] if the process cannot be spawned or its pipes
    /// cannot be captured.
    pub fn spawn(
        server_id: impl Into<String>,
        command: &str,
        args: &[String],
        env: &BTreeMap<String, String>,
    ) -> Result<Self, McpError> {
        let server_id = server_id.into();
        let mut child = tokio::process::Command::new(command)
            .args(args)
            .env_clear()
            .envs(compose_child_env(std::env::vars(), env))
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::null())
            .kill_on_drop(true)
            .spawn()
            .map_err(|e| McpError::Startup(format!("{}: {}", server_id, e.kind())))?;

        let stdin = child
            .stdin
            .take()
            .ok_or_else(|| McpError::Startup(format!("{server_id}: no stdin pipe")))?;
        let stdout = child
            .stdout
            .take()
            .ok_or_else(|| McpError::Startup(format!("{server_id}: no stdout pipe")))?;

        Ok(Self {
            server_id,
            io: Mutex::new(StdioIo {
                child,
                stdin,
                stdout: BufReader::new(stdout),
                next_id: 1,
            }),
            request_timeout: DEFAULT_REQUEST_TIMEOUT,
        })
    }

    /// Override the per-request deadline (tests keep it short).
    #[must_use]
    pub fn with_request_timeout(mut self, timeout: Duration) -> Self {
        self.request_timeout = timeout;
        self
    }

    async fn write_line(
        io: &mut StdioIo,
        message: &Value,
        server_id: &str,
    ) -> Result<(), McpError> {
        let mut line =
            serde_json::to_vec(message).map_err(|e| McpError::Protocol(format!("{e}")))?;
        line.push(b'\n');
        io.stdin
            .write_all(&line)
            .await
            .map_err(|e| McpError::Transport(format!("{}: {}", server_id, e.kind())))?;
        io.stdin
            .flush()
            .await
            .map_err(|e| McpError::Transport(format!("{}: {}", server_id, e.kind())))?;
        Ok(())
    }
}

#[async_trait]
impl McpConnection for StdioConnection {
    async fn call(
        &self,
        method: &str,
        params: Value,
        _provenance: &Provenance,
    ) -> Result<Value, McpError> {
        let mut io = self.io.lock().await;
        let id = io.next_id;
        io.next_id += 1;

        let message = json!({
            "jsonrpc": "2.0",
            "id": id,
            "method": method,
            "params": params,
        });
        Self::write_line(&mut io, &message, &self.server_id).await?;

        // Read lines until the response with the matching id arrives, skipping
        // any server-initiated notifications/logs. The whole read is
        // deadline-bounded so a hung server cannot wedge the loop.
        let deadline = self.request_timeout;
        loop {
            let mut line = String::new();
            let read = tokio::time::timeout(deadline, io.stdout.read_line(&mut line)).await;
            let n = match read {
                Err(_) => return Err(McpError::Timeout(self.server_id.clone())),
                Ok(Ok(n)) => n,
                Ok(Err(e)) => {
                    return Err(McpError::Transport(format!(
                        "{}: {}",
                        self.server_id,
                        e.kind()
                    )))
                }
            };
            if n == 0 {
                return Err(McpError::Closed(self.server_id.clone()));
            }
            let trimmed = line.trim();
            if trimmed.is_empty() {
                continue;
            }
            let value: Value = serde_json::from_str(trimmed)
                .map_err(|_| McpError::Protocol(self.server_id.clone()))?;
            match value.get("id").and_then(Value::as_u64) {
                Some(resp_id) if resp_id == id => return rpc_result(&value, &self.server_id),
                // A notification or a response to a different id: keep reading.
                _ => continue,
            }
        }
    }

    async fn notify(&self, method: &str, params: Value) -> Result<(), McpError> {
        let mut io = self.io.lock().await;
        let message = json!({
            "jsonrpc": "2.0",
            "method": method,
            "params": params,
        });
        Self::write_line(&mut io, &message, &self.server_id).await
    }
}

impl StdioConnection {
    /// Whether the child process has already exited (a crash the registry treats
    /// as a degraded server).
    pub async fn has_exited(&self) -> bool {
        let mut io = self.io.lock().await;
        matches!(io.child.try_wait(), Ok(Some(_)))
    }
}

// ---------------------------------------------------------------------------
// Shared JSON-RPC helpers
// ---------------------------------------------------------------------------

/// Drain a streaming response body into a byte buffer.
async fn collect_body(mut body: ByteStream) -> Result<Vec<u8>, McpError> {
    let mut buf = Vec::new();
    while let Some(chunk) = body.next().await {
        let chunk = chunk.map_err(|e| McpError::Transport(e.to_string()))?;
        buf.extend_from_slice(&chunk);
    }
    Ok(buf)
}

/// Extract a single JSON-RPC response object from an HTTP response body.
///
/// Streamable-HTTP answers with either a bare `application/json` object or a
/// `text/event-stream` (SSE) whose `data:` line carries the object. Handle both:
/// prefer a direct parse, else scan `data:` payloads for the first JSON-RPC
/// object.
fn extract_jsonrpc(bytes: &[u8], server_id: &str) -> Result<Value, McpError> {
    let text = std::str::from_utf8(bytes).map_err(|_| McpError::Protocol(server_id.to_owned()))?;
    let trimmed = text.trim();
    if let Ok(value) = serde_json::from_str::<Value>(trimmed) {
        if value.is_object() {
            return Ok(value);
        }
    }
    // SSE: pull the JSON out of `data:` lines.
    for line in text.lines() {
        let line = line.trim_start();
        if let Some(payload) = line.strip_prefix("data:") {
            let payload = payload.trim();
            if payload.is_empty() || payload == "[DONE]" {
                continue;
            }
            if let Ok(value) = serde_json::from_str::<Value>(payload) {
                if value.is_object() {
                    return Ok(value);
                }
            }
        }
    }
    Err(McpError::Protocol(server_id.to_owned()))
}

/// Turn a JSON-RPC response envelope into its `result`, or an [`McpError`] for a
/// JSON-RPC `error`. The error message carries only the code, never `error.data`
/// (BR-1).
fn rpc_result(response: &Value, server_id: &str) -> Result<Value, McpError> {
    if let Some(error) = response.get("error").filter(|e| !e.is_null()) {
        let code = error.get("code").and_then(Value::as_i64).unwrap_or(0);
        return Err(McpError::Server {
            server_id: server_id.to_owned(),
            code,
        });
    }
    Ok(response.get("result").cloned().unwrap_or(Value::Null))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex as StdMutex;

    // ---- the block sentence (REQ-562 BR-3) ----

    /// **Three refusals, three sentences.** An MCP call crosses the same choke
    /// point a turn does, so all three of its refusals reach this seam — and
    /// the failure this replaced was not a wrong sentence but **one** sentence
    /// for every cause, which sent a user whose scan could not run hunting for
    /// a `local-only` glob that does not exist.
    ///
    /// The `unique.len()` assertion is what makes a collapsed clause fail; the
    /// per-row assertions are what keep the scan-unavailable wording from
    /// drifting into claiming a finding.
    #[test]
    fn the_three_block_causes_produce_three_distinct_mcp_sentences() {
        use std::collections::BTreeSet;

        let rendered: Vec<String> = [
            BlockDetail::Boundary,
            BlockDetail::Redaction,
            BlockDetail::ScanUnavailable,
        ]
        .into_iter()
        .map(|detail| {
            McpError::PrivacyBlocked {
                path: "secrets/prod.env".to_owned(),
                server_id: "kb".to_owned(),
                detail,
            }
            .to_string()
        })
        .collect();

        let unique: BTreeSet<&String> = rendered.iter().collect();
        assert_eq!(
            unique.len(),
            3,
            "the three causes must not share a sentence: {rendered:?}"
        );

        // REQ-544's wording, preserved: it is what is already in every log.
        assert!(
            rendered[0].contains("privacy boundary blocked an MCP call"),
            "{}",
            rendered[0]
        );
        assert!(
            rendered[1].contains("found sensitive content"),
            "{}",
            rendered[1]
        );
        // The rule BR-3 is about: a scan that never ran cannot have found
        // anything, and is not a boundary block either.
        assert!(rendered[2].contains("could not run"), "{}", rendered[2]);
        assert!(
            !rendered[2].contains("found"),
            "a scan that never ran claimed a finding: {}",
            rendered[2]
        );
        assert!(
            !rendered[2].contains("privacy boundary"),
            "a scan-unavailable block is not a boundary block: {}",
            rendered[2]
        );
        // Every sentence names the server, so an operator knows which one.
        for line in &rendered {
            assert!(line.contains("`kb`"), "{line}");
        }
    }

    // ---- namespacing ----

    #[test]
    fn namespacing_round_trips() {
        let name = namespaced_tool_name("fs", "read_file");
        assert_eq!(name, "mcp__fs__read_file");
        assert_eq!(parse_namespaced_tool_name(&name), Some(("fs", "read_file")));
    }

    #[test]
    fn a_tool_name_may_itself_contain_the_separator() {
        let name = namespaced_tool_name("fs", "list__all");
        assert_eq!(parse_namespaced_tool_name(&name), Some(("fs", "list__all")));
    }

    #[test]
    fn non_namespaced_names_are_rejected() {
        assert_eq!(parse_namespaced_tool_name("read"), None);
        assert_eq!(parse_namespaced_tool_name("mcp__fs"), None);
        assert_eq!(parse_namespaced_tool_name("mcp____tool"), None);
        assert_eq!(parse_namespaced_tool_name("mcp__srv__"), None);
    }

    // ---- provenance extraction ----

    #[test]
    fn provenance_from_a_path_argument() {
        let prov = call_provenance(&json!({ "path": "secrets/prod.env" }));
        assert!(prov.contains("secrets/prod.env"));
    }

    #[test]
    fn provenance_from_a_path_shaped_value_under_an_unknown_key() {
        // Even under a non-path key, a slash-bearing value is treated as a path so
        // a boundary source is never missed (BR-1 bias to tag).
        let prov = call_provenance(&json!({ "whatever": "secrets/leak.txt" }));
        assert!(prov.contains("secrets/leak.txt"));
    }

    #[test]
    fn provenance_recurses_and_ignores_non_paths() {
        let prov = call_provenance(&json!({
            "files": ["src/a.rs", "secrets/b.env"],
            "query": "just some words",
            "nested": { "file": "docs/c.md" },
            "url": "https://example.com/not-a-file",
        }));
        assert!(prov.contains("src/a.rs"));
        assert!(prov.contains("secrets/b.env"));
        assert!(prov.contains("docs/c.md"));
        // Prose and URLs are not paths.
        assert!(!prov.contains("just some words"));
        assert!(!prov.contains("https://example.com/not-a-file"));
    }

    #[test]
    fn arguments_with_no_paths_have_empty_provenance() {
        let prov = call_provenance(&json!({ "n": 3, "flag": true, "q": "hello" }));
        assert!(prov.is_empty());
    }

    // ---- refused assertions (REQ-571 ADR-D) ----

    /// The two halves of the fail-closed answer, asserted together because
    /// either alone is a bug: the call is tainted (so it cannot leak), *and*
    /// the refused assertion is kept (so the taint can be explained).
    #[test]
    fn an_un_mintable_argument_taints_the_call_and_is_recorded() {
        let out = call_provenance_detailed(&json!({ "path": "/repo/secrets/prod.env" }));
        assert!(
            out.provenance.is_unknown(),
            "an un-mintable claim must fail toward taint"
        );
        assert_eq!(
            out.rejected,
            vec![RejectedSource {
                source: "/repo/secrets/prod.env".to_owned(),
                reason: ProvenanceRejection::Absolute,
            }]
        );
    }

    /// Each refusal shape keeps its own reason, and a `..` claim is recorded
    /// rather than collapsed — the two are different problems for whoever reads
    /// the event.
    #[test]
    fn every_un_mintable_shape_is_recorded_with_its_reason() {
        let out = call_provenance_detailed(&json!({
            "path": "/etc/passwd",
            "resource": "../outside/x.env",
            "clean": "src/main.rs",
        }));
        let mut seen: Vec<(&str, ProvenanceRejection)> = out
            .rejected
            .iter()
            .map(|r| (r.source.as_str(), r.reason))
            .collect();
        seen.sort_unstable_by_key(|(s, _)| *s);
        assert_eq!(
            seen,
            [
                ("../outside/x.env", ProvenanceRejection::ParentTraversal),
                ("/etc/passwd", ProvenanceRejection::Absolute),
            ]
        );
        // The well-formed sibling is still tagged: one bad assertion does not
        // discard the provenance of the others.
        assert!(out.provenance.contains("src/main.rs"));
    }

    /// The recorded source is sanitized where it is recorded, not where it is
    /// rendered — a server chose this string, and every consumer downstream
    /// (event, log line, terminal) inherits the bound rather than repeating it.
    #[test]
    fn a_recorded_source_is_sanitized_and_bounded() {
        let hostile = format!("/{}\u{1b}[31m\n{}", "a".repeat(400), "b".repeat(400));
        let out = call_provenance_detailed(&json!({ "path": hostile }));
        let recorded = &out.rejected[0].source;
        assert!(recorded.len() <= 260, "unbounded: {} bytes", recorded.len());
        assert!(!recorded.contains('\u{1b}'));
        assert!(!recorded.contains('\n'));
        assert!(recorded.ends_with('…'), "truncation is visible: {recorded}");
    }

    /// Non-vacuity: a call whose every path-shaped argument mints cleanly
    /// records nothing, so the recording is a signal rather than noise every
    /// call carries.
    #[test]
    fn a_clean_call_records_no_rejection() {
        let out = call_provenance_detailed(&json!({
            "files": ["src/a.rs", "./docs/b.md"],
            "q": "words",
        }));
        assert!(out.rejected.is_empty());
        assert!(!out.provenance.is_unknown());
    }

    // ---- protocol parsing ----

    #[test]
    fn parses_a_tools_list_entry() {
        let tool = McpTool::from_value(&json!({
            "name": "echo",
            "description": "echoes input",
            "inputSchema": { "type": "object", "properties": {} }
        }))
        .unwrap();
        assert_eq!(tool.name, "echo");
        assert_eq!(tool.description, "echoes input");
    }

    #[test]
    fn a_tool_result_joins_text_parts_and_keeps_the_error_flag() {
        let result = McpToolResult::from_value(&json!({
            "content": [
                { "type": "text", "text": "line one" },
                { "type": "text", "text": "line two" }
            ],
            "isError": true
        }))
        .unwrap();
        assert_eq!(result.text, "line one\nline two");
        assert!(result.is_error);
    }

    #[test]
    fn a_jsonrpc_error_maps_to_a_server_error_carrying_only_the_code() {
        let response = json!({
            "jsonrpc": "2.0",
            "id": 1,
            "error": { "code": -32601, "message": "Method not found", "data": "secret detail" }
        });
        let err = rpc_result(&response, "fs").expect_err("must be an error");
        match err {
            McpError::Server { server_id, code } => {
                assert_eq!(server_id, "fs");
                assert_eq!(code, -32601);
            }
            other => panic!("unexpected: {other:?}"),
        }
        // The rendered error must never echo `error.data`.
        assert!(!err_to_string(&response).contains("secret detail"));
    }

    fn err_to_string(response: &Value) -> String {
        match rpc_result(response, "fs") {
            Err(e) => e.to_string(),
            Ok(_) => String::new(),
        }
    }

    #[test]
    fn extract_jsonrpc_handles_plain_json_and_sse() {
        let plain = br#"{"jsonrpc":"2.0","id":1,"result":{"ok":true}}"#;
        assert!(extract_jsonrpc(plain, "s").unwrap().get("result").is_some());

        let sse =
            b"event: message\ndata: {\"jsonrpc\":\"2.0\",\"id\":1,\"result\":{\"ok\":true}}\n\n";
        assert!(extract_jsonrpc(sse, "s").unwrap().get("result").is_some());
    }

    // ---- a mock connection for client-level tests ----

    /// A scripted [`McpConnection`] that records the provenance of each `call` and
    /// returns canned results, so the protocol client is testable with no I/O.
    #[derive(Default)]
    struct MockConnection {
        calls: StdMutex<Vec<(String, Provenance)>>,
    }

    #[async_trait]
    impl McpConnection for MockConnection {
        async fn call(
            &self,
            method: &str,
            _params: Value,
            provenance: &Provenance,
        ) -> Result<Value, McpError> {
            self.calls
                .lock()
                .unwrap()
                .push((method.to_owned(), provenance.clone()));
            let result = match method {
                "initialize" => json!({
                    "protocolVersion": MCP_PROTOCOL_VERSION,
                    "serverInfo": { "name": "mock", "version": "9.9" }
                }),
                "tools/list" => json!({
                    "tools": [
                        { "name": "echo", "description": "echoes", "inputSchema": {"type": "object"} }
                    ]
                }),
                "tools/call" => json!({
                    "content": [ { "type": "text", "text": "echoed" } ],
                    "isError": false
                }),
                _ => Value::Null,
            };
            Ok(result)
        }

        async fn notify(&self, _method: &str, _params: Value) -> Result<(), McpError> {
            Ok(())
        }
    }

    #[tokio::test]
    async fn client_drives_the_handshake_list_and_call() {
        let conn = Arc::new(MockConnection::default());
        let client = McpClient::new("mock", conn.clone());

        let info = client.initialize().await.unwrap();
        assert_eq!(info.name, "mock");
        assert_eq!(info.version, "9.9");

        let tools = client.list_tools().await.unwrap();
        assert_eq!(tools.len(), 1);
        assert_eq!(tools[0].name, "echo");

        let result = client
            .call_tool("echo", json!({ "path": "secrets/prod.env" }))
            .await
            .unwrap();
        assert_eq!(result.text, "echoed");
        assert!(!result.is_error);

        // The tools/call carried the boundary-path provenance derived from its
        // arguments; the handshake calls carried none.
        let calls = conn.calls.lock().unwrap();
        let call = calls.iter().find(|(m, _)| m == "tools/call").unwrap();
        assert!(call.1.contains("secrets/prod.env"));
        let init = calls.iter().find(|(m, _)| m == "initialize").unwrap();
        assert!(init.1.is_empty());
    }

    // ---- MED-2: a spawned MCP server gets a minimal, key-free environment ----

    #[test]
    fn compose_child_env_drops_provider_keys_and_keeps_essentials() {
        // REQ-544 MED-2: the daemon's provider key must not reach the child; the
        // essentials (and a declared per-server var) must.
        let daemon_vars = vec![
            ("PATH".to_owned(), "/usr/bin:/bin".to_owned()),
            ("HOME".to_owned(), "/home/x".to_owned()),
            (
                "ANTHROPIC_API_KEY".to_owned(),
                "sk-should-not-leak".to_owned(),
            ),
            ("OPENAI_API_KEY".to_owned(), "sk-also-secret".to_owned()),
            ("RANDOM_DAEMON_VAR".to_owned(), "nope".to_owned()),
        ];
        let mut declared = BTreeMap::new();
        declared.insert("MY_SERVER_SETTING".to_owned(), "on".to_owned());

        let env = compose_child_env(daemon_vars, &declared);
        let names: Vec<&str> = env.iter().map(|(k, _)| k.as_str()).collect();

        assert!(names.contains(&"PATH"));
        assert!(names.contains(&"HOME"));
        assert!(names.contains(&"MY_SERVER_SETTING"));
        // Provider keys and un-allowlisted daemon vars are gone.
        assert!(!names.contains(&"ANTHROPIC_API_KEY"));
        assert!(!names.contains(&"OPENAI_API_KEY"));
        assert!(!names.contains(&"RANDOM_DAEMON_VAR"));
        // And the secret value appears nowhere.
        assert!(!env.iter().any(|(_, v)| v.contains("should-not-leak")));
    }

    /// AC-4.1a (BR-7.1). `MCP_BASE_ENV_ALLOW`'s membership, asserted against a
    /// **literal** list rather than against itself — REQ-596 moved this path onto
    /// a composer shared with the `shell` tool, and the one thing that must not
    /// have happened is the shell's needs widening this constant. A name added
    /// here hands every third-party `npx`/`uvx` server the increment.
    #[test]
    fn req_596_did_not_widen_the_mcp_allowlist() {
        let expected = [
            "PATH", "HOME", "TMPDIR", "TZ", "TERM", "USER", "LOGNAME", "SHELL", "LANG", "LANGUAGE",
            "LC_ALL", "LC_CTYPE",
        ];
        assert_eq!(
            MCP_BASE_ENV_ALLOW, &expected,
            "widening this is a security regression in the path that was already correct; \
             the shell tool has its own constant (child_env::SHELL_ENV_ALLOW)"
        );
    }

    /// AC-4.1b. Sharing the composer must be provably free for the MCP path, not
    /// merely intended to be — so this pins the *whole composed result* for a
    /// fixed daemon environment and a fixed declared map, byte for byte, against
    /// a literal written out here.
    ///
    /// `PATH` is asserted separately because the floor asks the filesystem which
    /// package-manager prefixes exist, so its value is a property of the machine
    /// rather than of this function. Everything else is pinned exactly.
    #[test]
    fn the_composed_mcp_environment_is_byte_identical_to_its_pre_req_596_form() {
        let daemon_vars = vec![
            ("PATH".to_owned(), "/usr/bin:/bin".to_owned()),
            ("HOME".to_owned(), "/home/SENTINEL".to_owned()),
            ("TERM".to_owned(), "xterm-256color".to_owned()),
            ("LANG".to_owned(), "en_US.UTF-8".to_owned()),
            ("TMPDIR".to_owned(), "/tmp/SENTINEL".to_owned()),
            (
                "ANTHROPIC_API_KEY".to_owned(),
                "SENTINEL-not-a-key".to_owned(),
            ),
            ("RANDOM_DAEMON_VAR".to_owned(), "SENTINEL-nope".to_owned()),
        ];
        let declared = BTreeMap::from([
            ("MY_SERVER_SETTING".to_owned(), "on".to_owned()),
            ("TERM".to_owned(), "dumb".to_owned()),
        ]);

        let env = compose_child_env(daemon_vars, &declared);
        let without_path: Vec<(String, String)> =
            env.iter().filter(|(k, _)| k != "PATH").cloned().collect();

        let expected: Vec<(String, String)> = [
            ("HOME", "/home/SENTINEL"),
            ("LANG", "en_US.UTF-8"),
            ("MY_SERVER_SETTING", "on"),
            ("TERM", "dumb"),
            ("TMPDIR", "/tmp/SENTINEL"),
        ]
        .iter()
        .map(|(k, v)| ((*k).to_owned(), (*v).to_owned()))
        .collect();

        assert_eq!(
            without_path, expected,
            "REQ-596 shared the composer with the shell tool; the MCP path's composed \
             environment must be exactly what it was before"
        );
        assert!(
            env.iter().filter(|(k, _)| k == "PATH").count() == 1,
            "exactly one PATH, as the floor leaves behind"
        );
    }

    #[test]
    fn a_declared_var_overrides_a_base_var() {
        let daemon_vars = vec![("TERM".to_owned(), "xterm".to_owned())];
        let mut declared = BTreeMap::new();
        declared.insert("TERM".to_owned(), "dumb".to_owned());
        let env = compose_child_env(daemon_vars, &declared);
        assert_eq!(
            env.iter()
                .find(|(k, _)| k == "TERM")
                .map(|(_, v)| v.as_str()),
            Some("dumb")
        );
    }

    /// BUG-174: an MCP server invoked as `npx …` is unlaunchable when the
    /// daemon's inherited `PATH` names no package-manager prefix, which is
    /// exactly what launchd hands it.
    #[test]
    fn an_inherited_launchd_path_is_floored_for_the_child() {
        let daemon_vars = vec![("PATH".to_owned(), "/usr/bin:/bin".to_owned())];
        let env = compose_child_env(daemon_vars, &BTreeMap::new());
        let path = env
            .iter()
            .find(|(k, _)| k == "PATH")
            .map(|(_, v)| v.as_str())
            .expect("a child must always receive a PATH");
        assert!(path.starts_with("/usr/bin:/bin"), "{path}");
        // Whatever this machine actually has, the floor may only ever add.
        assert!(path.len() >= "/usr/bin:/bin".len(), "{path}");
    }

    /// A server that declares its own `PATH` has said what it wants; the floor
    /// is applied to the inherited value beneath it and must not survive on top.
    #[test]
    fn a_declared_path_overrides_the_floor_untouched() {
        let daemon_vars = vec![("PATH".to_owned(), "/usr/bin".to_owned())];
        let mut declared = BTreeMap::new();
        declared.insert("PATH".to_owned(), "/only/this".to_owned());
        let env = compose_child_env(daemon_vars, &declared);
        assert_eq!(
            env.iter()
                .find(|(k, _)| k == "PATH")
                .map(|(_, v)| v.as_str()),
            Some("/only/this")
        );
    }

    #[test]
    fn a_real_subprocess_does_not_receive_a_provider_key() {
        // The genuine article: apply the composed env to a real child and prove a
        // provider key present in the daemon-vars set is absent in the child.
        use std::process::Command;
        let path = std::env::var("PATH").unwrap_or_else(|_| "/usr/bin:/bin".to_owned());
        let daemon_vars = vec![
            ("PATH".to_owned(), path),
            (
                "ANTHROPIC_API_KEY".to_owned(),
                "sk-leak-me-XYZZY".to_owned(),
            ),
        ];
        let composed = compose_child_env(daemon_vars, &BTreeMap::new());
        let out = Command::new("/bin/sh")
            .arg("-c")
            .arg("printf 'start:%s:end' \"$ANTHROPIC_API_KEY\"")
            .env_clear()
            .envs(composed)
            .output()
            .expect("run child");
        let stdout = String::from_utf8_lossy(&out.stdout);
        assert_eq!(
            stdout, "start::end",
            "provider key leaked into the child env"
        );
        assert!(!stdout.contains("sk-leak-me-XYZZY"));
    }
}
