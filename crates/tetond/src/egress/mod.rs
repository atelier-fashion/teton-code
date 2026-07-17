//! The single egress choke point (architecture D-2).
//!
//! Every byte that leaves this machine for a remote provider — or, per ADR-003,
//! a remote MCP server — passes through here. This is the *only* place in the
//! whole workspace that constructs a real HTTP client ([`HttpTransport`], built
//! on `reqwest`); a CI deny-check ([`deny_http_client`]) fails the build if any
//! other crate grows an HTTP-client dependency. Because the provider crate
//! carries no client of its own and only ever holds a `&dyn Transport`, "an
//! adapter cannot reach the network except through egress" is a compile-time
//! property, not a review convention.
//!
//! Three responsibilities converge here:
//! - **BR-1 (privacy boundary)** — a request whose content provenance intersects
//!   a `local-only` boundary is blocked before a single byte leaves; a
//!   `privacy_block` event is emitted with the offending path, the provider, and
//!   the action taken. Enforcement is provenance-based, not string-scanning; see
//!   [`provenance`] and [`inspector`].
//! - **BR-2 (cost)** — every *allowed* remote call is the hook where TASK-008
//!   records a `CostRecord`. That seam is marked in [`Egress::send`].
//! - **BR-7 (credentials)** — the adapter never sees a secret; the transport
//!   attaches the resolved credential header ([`HttpTransport::with_injected_headers`]).
//!
//! ## How provenance reaches egress
//!
//! The [`Transport`] trait (owned by `teton-providers`, and deliberately not
//! modified by this crate) carries only method/url/headers/body — it has no
//! channel for provenance, because provenance is a property of the *assembled
//! context*, not of the raw request bytes. So the daemon's turn loop calls the
//! richer [`Egress::send`], passing the [`Provenance`] of the context it
//! assembled for that turn. For the adapter seam, [`Egress::scoped`] hands back a
//! [`TurnTransport`]: a `Transport` whose `execute` runs the same guard against a
//! baked-in per-turn provenance, so an adapter that only knows `&dyn Transport`
//! is still enforced.
//!
//! ## Residual limit (documented per the task, to be carried to spec Assumptions)
//!
//! Provenance tracking is honest about *derived* content: a summary, snippet, or
//! tool result computed from a `local-only` file inherits that file's provenance
//! and is blocked ([`ContextBlock::derive`]). What it does **not** cover is a
//! model-generated **paraphrase of boundary content emitted in a LATER turn**:
//! once a local model has read a secret and the operator later routes a turn to a
//! remote provider, text the model writes from its own memory carries no file
//! provenance, so this layer cannot recognize it. Closing that gap needs
//! turn-to-turn taint propagation through model output (out of scope for the
//! MVP). Until then the mitigation is routing discipline — a session that has
//! touched `local-only` content stays on the local tier — which the router
//! enforces above this layer.

pub mod inspector;
pub mod provenance;

use std::sync::Arc;

use async_trait::async_trait;
use futures::StreamExt;

use teton_core::boundary::BoundaryMatcher;
use teton_core::entities::PrivacyBoundary;
use teton_protocol::events::{Event, PrivacyAction, PrivacyBlock};
use teton_protocol::{ProviderId, SessionId};
use teton_providers::transport::{
    ByteStream, HttpMethod, Transport, TransportError, TransportRequest, TransportResponse,
};

use crate::broadcast::EventBus;
use crate::cost::{CostAttribution, CostMeter};

pub use inspector::{inspect, Inspection, Violation};
pub use provenance::{assembled_provenance, ContextBlock, Provenance};

/// A failure at the egress choke point.
///
/// Every variant is content-free by construction — it carries at most a
/// config-authored path, a provider id, an action, or a transport failure class.
/// That is what makes error and telemetry paths safe under BR-1: an `EgressError`
/// may be logged, serialized, or surfaced to a client without leaking a byte of
/// boundary content.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum EgressError {
    /// A `local-only` boundary was about to be crossed; the request was refused
    /// before any network activity. The offending `path` is safe to surface (it
    /// is the boundary glob's target, not file content).
    #[error(
        "privacy boundary blocked egress of `{path}` to provider `{provider_id}` ({action:?})"
    )]
    PrivacyBlocked {
        /// Repo-relative path of the boundary source that would have leaked.
        path: String,
        /// Provider the content would have reached.
        provider_id: ProviderId,
        /// What the choke point did instead.
        action: PrivacyAction,
    },
    /// The underlying transport failed (before any HTTP status was known).
    #[error("transport error: {0}")]
    Transport(#[from] TransportError),
    /// The configured privacy boundaries did not compile. Egress refuses
    /// fail-closed rather than send content it cannot classify.
    #[error("privacy boundaries failed to compile; egress refused fail-closed")]
    BoundaryCompile,
    /// The real HTTP client could not be initialized.
    #[error("failed to initialize the HTTP client")]
    ClientInit,
}

impl EgressError {
    /// Collapse to a [`TransportError`] for the object-safe `Transport` seam.
    ///
    /// A privacy block manifests to an adapter as a refusal to connect — which is
    /// literally true, no connection is attempted — while the authoritative,
    /// typed signal (this error) and the `privacy_block` event carry the real
    /// reason. Adapters must not treat this as a provider fault to retry.
    #[must_use]
    pub fn into_transport_error(self) -> TransportError {
        match self {
            EgressError::Transport(t) => t,
            EgressError::PrivacyBlocked { .. } | EgressError::ClientInit => TransportError::Connect,
            EgressError::BoundaryCompile => TransportError::Io,
        }
    }
}

/// A sink for `privacy_block` events emitted by the choke point.
///
/// Abstracted so the choke point does not depend on the concrete daemon event
/// bus (and so tests can capture emitted events). The daemon wires its
/// [`EventBus`]; a [`NoopSink`] is available where events are irrelevant.
pub trait PrivacyEventSink: Send + Sync {
    /// Publish a `privacy_block` event, scoped to `session_id` when known.
    fn privacy_block(&self, session_id: Option<SessionId>, block: PrivacyBlock);
}

/// The production sink: broadcast to attached clients over the daemon event bus.
impl PrivacyEventSink for EventBus {
    fn privacy_block(&self, session_id: Option<SessionId>, block: PrivacyBlock) {
        self.publish(session_id, Event::PrivacyBlock(block));
    }
}

/// A sink that drops events — for contexts with no subscribers.
#[derive(Debug, Clone, Copy, Default)]
pub struct NoopSink;

impl PrivacyEventSink for NoopSink {
    fn privacy_block(&self, _session_id: Option<SessionId>, _block: PrivacyBlock) {}
}

/// Per-call context the choke point needs but the [`TransportRequest`] cannot
/// carry: which provider the call targets, which session it belongs to, and the
/// action to record if it is blocked.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EgressContext {
    /// Provider the request targets (named in any `privacy_block` event).
    pub provider_id: ProviderId,
    /// Owning session, when the call is session-scoped.
    pub session_id: Option<SessionId>,
    /// The action a block should report. Defaults to
    /// [`PrivacyAction::ReroutedToLocal`]: the remote call is prevented and the
    /// turn is to be served on the local tier.
    pub block_action: PrivacyAction,
    /// Billing attribution for this call (TASK-008, BR-2). When present *and* the
    /// choke point holds a cost meter, an allowed forward is recorded as one
    /// `CostRecord`; when absent, the call forwards unmetered (e.g. a
    /// non-billable probe). Carries the phase and model the choke point cannot
    /// otherwise know.
    pub cost: Option<CostAttribution>,
}

impl EgressContext {
    /// Context for `provider_id`, defaulting the block action to
    /// [`PrivacyAction::ReroutedToLocal`] and no session scope.
    #[must_use]
    pub fn new(provider_id: impl Into<ProviderId>) -> Self {
        Self {
            provider_id: provider_id.into(),
            session_id: None,
            block_action: PrivacyAction::ReroutedToLocal,
            cost: None,
        }
    }

    /// Scope the context to `session_id`.
    #[must_use]
    pub fn with_session(mut self, session_id: impl Into<SessionId>) -> Self {
        self.session_id = Some(session_id.into());
        self
    }

    /// Override the action reported on a block.
    #[must_use]
    pub fn with_action(mut self, action: PrivacyAction) -> Self {
        self.block_action = action;
        self
    }

    /// Attach billing attribution so an allowed forward is recorded as a
    /// `CostRecord` (TASK-008, BR-2).
    #[must_use]
    pub fn with_cost(mut self, attribution: CostAttribution) -> Self {
        self.cost = Some(attribution);
        self
    }
}

/// The egress choke point: a privacy/cost guard wrapping an inner [`Transport`].
///
/// In production `T` is [`HttpTransport`]; tests substitute a capturing transport
/// so the egress-capture harness can assert that no boundary content ever reaches
/// the wire. The guard is `T`-generic precisely so the same enforcement runs in
/// front of the real client and the test double.
pub struct Egress<T: Transport> {
    inner: T,
    boundaries: Vec<PrivacyBoundary>,
    sink: Arc<dyn PrivacyEventSink>,
    cost: Option<Arc<dyn CostMeter>>,
}

impl<T: Transport> Egress<T> {
    /// Build a choke point over `inner`, enforcing `boundaries`, emitting
    /// `privacy_block` events through `sink`.
    #[must_use]
    pub fn new(
        inner: T,
        boundaries: Vec<PrivacyBoundary>,
        sink: Arc<dyn PrivacyEventSink>,
    ) -> Self {
        Self {
            inner,
            boundaries,
            sink,
            cost: None,
        }
    }

    /// Install the cost meter (TASK-008): every allowed forward carrying a
    /// [`CostAttribution`] is recorded as one `CostRecord` (BR-2). Additive —
    /// a choke point without a meter forwards exactly as before.
    #[must_use]
    pub fn with_cost_meter(mut self, meter: Arc<dyn CostMeter>) -> Self {
        self.cost = Some(meter);
        self
    }

    /// The configured boundaries (read-only).
    #[must_use]
    pub fn boundaries(&self) -> &[PrivacyBoundary] {
        &self.boundaries
    }

    /// Dispatch `request` for the context assembled with `provenance`.
    ///
    /// This is the choke point's primary API and the daemon's turn loop calls it.
    /// If `provenance` intersects a boundary, the request is refused before any
    /// network activity: a `privacy_block` event is emitted and
    /// [`EgressError::PrivacyBlocked`] is returned. The `request` — whose body may
    /// contain boundary bytes — is dropped, never forwarded, never logged.
    /// Otherwise the request is forwarded to the inner transport.
    pub async fn send(
        &self,
        request: TransportRequest,
        provenance: &Provenance,
        ctx: &EgressContext,
    ) -> Result<TransportResponse, EgressError> {
        // Fast path: content from no file, or no boundaries configured, can never
        // intersect a boundary — skip building a matcher entirely.
        if !provenance.is_empty() && !self.boundaries.is_empty() {
            let matcher =
                BoundaryMatcher::new(&self.boundaries).map_err(|_| EgressError::BoundaryCompile)?;
            if let Inspection::Blocked(violation) = inspect(provenance, &matcher, ctx.block_action)
            {
                let block = PrivacyBlock {
                    path: violation.path.clone(),
                    provider_id: ctx.provider_id.clone(),
                    action: violation.action,
                };
                self.sink.privacy_block(ctx.session_id.clone(), block);
                return Err(EgressError::PrivacyBlocked {
                    path: violation.path,
                    provider_id: ctx.provider_id.clone(),
                    action: violation.action,
                });
            }
        }

        // Cleared. TASK-008 wraps this forward to record a CostRecord (BR-2) from
        // the streamed usage — the single point where every remote call is billed.
        let response = self
            .inner
            .execute(request)
            .await
            .map_err(EgressError::Transport)?;
        // Bill the call iff the caller attached attribution and a meter is
        // installed; the meter wraps the response so recording happens from the
        // streamed usage when the body drains.
        match (&self.cost, &ctx.cost) {
            (Some(meter), Some(attribution)) => Ok(meter.meter_response(
                response,
                ctx.session_id.clone(),
                ctx.provider_id.clone(),
                attribution.clone(),
            )),
            _ => Ok(response),
        }
    }

    /// A per-turn, provenance-scoped [`Transport`] view for the adapter seam.
    ///
    /// Hand the returned `&dyn Transport` to an adapter: its `execute` runs the
    /// same guard as [`Egress::send`] against `provenance`, so an adapter that
    /// only knows `&dyn Transport` cannot bypass BR-1.
    #[must_use]
    pub fn scoped(&self, provenance: Provenance, ctx: EgressContext) -> TurnTransport<'_, T> {
        TurnTransport {
            egress: self,
            provenance,
            ctx,
        }
    }
}

/// A provenance-scoped `Transport` produced by [`Egress::scoped`].
///
/// Bridges the object-safe [`Transport`] seam (which cannot carry provenance) to
/// the guarded [`Egress::send`] by baking in the current turn's provenance. A
/// block still emits its `privacy_block` event; the adapter observes a
/// transport-level refusal (see [`EgressError::into_transport_error`]).
pub struct TurnTransport<'a, T: Transport> {
    egress: &'a Egress<T>,
    provenance: Provenance,
    ctx: EgressContext,
}

#[async_trait]
impl<T: Transport> Transport for TurnTransport<'_, T> {
    async fn execute(
        &self,
        request: TransportRequest,
    ) -> Result<TransportResponse, TransportError> {
        self.egress
            .send(request, &self.provenance, &self.ctx)
            .await
            .map_err(EgressError::into_transport_error)
    }
}

/// The real, network-backed [`Transport`] — the sole HTTP client in the tree.
///
/// Wraps a single `reqwest::Client` (connection-pooled, cheap to clone). Adapters
/// never construct this; the daemon builds one and hands it to [`Egress`]. Auth
/// is attached here, not by adapters (BR-7): resolved credential headers passed
/// to [`HttpTransport::with_injected_headers`] are added to every outbound
/// request on top of the adapter's protocol headers.
#[derive(Debug, Clone)]
pub struct HttpTransport {
    client: reqwest::Client,
    injected_headers: Vec<(String, String)>,
}

impl HttpTransport {
    /// Build a transport with a fresh HTTP client and no injected headers.
    pub fn new() -> Result<Self, EgressError> {
        Self::with_injected_headers(Vec::new())
    }

    /// Build a transport whose client injects `injected_headers` (e.g. a resolved
    /// authorization header, BR-7) into every request.
    pub fn with_injected_headers(
        injected_headers: Vec<(String, String)>,
    ) -> Result<Self, EgressError> {
        let client = reqwest::Client::builder()
            .build()
            .map_err(|_| EgressError::ClientInit)?;
        Ok(Self {
            client,
            injected_headers,
        })
    }
}

#[async_trait]
impl Transport for HttpTransport {
    async fn execute(
        &self,
        request: TransportRequest,
    ) -> Result<TransportResponse, TransportError> {
        let method = match request.method {
            HttpMethod::Post => reqwest::Method::POST,
        };
        let mut builder = self.client.request(method, &request.url);
        for (name, value) in request.headers.iter().chain(self.injected_headers.iter()) {
            builder = builder.header(name.as_str(), value.as_str());
        }
        let response = builder
            .body(request.body)
            .send()
            .await
            .map_err(|e| classify_reqwest_error(&e))?;

        let status = response.status().as_u16();
        let body: ByteStream = Box::pin(response.bytes_stream().map(|chunk| {
            chunk
                .map(|b| b.to_vec())
                .map_err(|e| classify_reqwest_error(&e))
        }));
        Ok(TransportResponse { status, body })
    }
}

/// Map a `reqwest` failure to the transport's closed error taxonomy. Note this
/// classifies the *failure class* only — no URL, header, or body content is
/// carried into the error (BR-1 / conventions: nothing content-bearing in logs).
fn classify_reqwest_error(error: &reqwest::Error) -> TransportError {
    if error.is_timeout() {
        TransportError::Timeout
    } else if error.is_connect() {
        TransportError::Connect
    } else {
        TransportError::Io
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    /// A `Transport` that records what it was asked to send and returns an empty
    /// 200 — the unit-level stand-in for the network. (The integration harness in
    /// `tests/egress_capture.rs` uses a fuller version.) The record lives behind a
    /// shared `Arc` so a test can hold a handle after the transport is moved into
    /// the [`Egress`].
    #[derive(Default, Clone)]
    struct CaptureTransport {
        sent: Arc<Mutex<Vec<TransportRequest>>>,
    }

    #[async_trait]
    impl Transport for CaptureTransport {
        async fn execute(
            &self,
            request: TransportRequest,
        ) -> Result<TransportResponse, TransportError> {
            self.sent.lock().unwrap().push(request);
            Ok(TransportResponse {
                status: 200,
                body: Box::pin(futures::stream::empty()),
            })
        }
    }

    #[derive(Default)]
    struct CapturingSink {
        events: Mutex<Vec<PrivacyBlock>>,
    }

    impl PrivacyEventSink for CapturingSink {
        fn privacy_block(&self, _session_id: Option<SessionId>, block: PrivacyBlock) {
            self.events.lock().unwrap().push(block);
        }
    }

    fn boundaries() -> Vec<PrivacyBoundary> {
        use teton_core::entities::BoundaryMode;
        vec![PrivacyBoundary {
            path_glob: "secrets/**".to_owned(),
            mode: BoundaryMode::LocalOnly,
        }]
    }

    fn a_request(body: &str) -> TransportRequest {
        TransportRequest {
            method: HttpMethod::Post,
            url: "https://api.example.com/v1/messages".to_owned(),
            headers: vec![("content-type".to_owned(), "application/json".to_owned())],
            body: body.as_bytes().to_vec(),
        }
    }

    #[tokio::test]
    async fn a_clean_request_reaches_the_inner_transport() {
        let inner = CaptureTransport::default();
        let sent = inner.sent.clone();
        let egress = Egress::new(inner, boundaries(), Arc::new(CapturingSink::default()));
        let prov = Provenance::tainted_by("src/main.rs");
        let ctx = EgressContext::new("anthropic");
        let resp = egress
            .send(a_request("public code"), &prov, &ctx)
            .await
            .expect("clean request allowed");
        assert_eq!(resp.status, 200);
        // The inner transport received exactly the forwarded request.
        let forwarded = sent.lock().unwrap();
        assert_eq!(forwarded.len(), 1);
        assert_eq!(forwarded[0].body, b"public code");
    }

    #[tokio::test]
    async fn boundary_provenance_is_blocked_and_never_forwarded() {
        let egress = Egress::new(
            CaptureTransport::default(),
            boundaries(),
            Arc::new(CapturingSink::default()),
        );
        let prov = Provenance::tainted_by("secrets/prod.env");
        let ctx = EgressContext::new("anthropic");
        let err = egress
            .send(a_request("API_KEY=super-secret-xyzzy"), &prov, &ctx)
            .await
            .expect_err("must be blocked");
        match err {
            EgressError::PrivacyBlocked {
                path,
                provider_id,
                action,
            } => {
                assert_eq!(path, "secrets/prod.env");
                assert_eq!(provider_id, ProviderId::from("anthropic"));
                assert_eq!(action, PrivacyAction::ReroutedToLocal);
            }
            other => panic!("unexpected error: {other:?}"),
        }
    }

    #[tokio::test]
    async fn a_block_emits_a_privacy_block_event() {
        let sink = Arc::new(CapturingSink::default());
        let egress = Egress::new(CaptureTransport::default(), boundaries(), sink.clone());
        let prov = Provenance::tainted_by("secrets/prod.env");
        let ctx = EgressContext::new("deepseek").with_session("sess-1");
        let _ = egress.send(a_request("secret"), &prov, &ctx).await;
        let events = sink.events.lock().unwrap();
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].path, "secrets/prod.env");
        assert_eq!(events[0].provider_id, ProviderId::from("deepseek"));
        assert_eq!(events[0].action, PrivacyAction::ReroutedToLocal);
    }

    #[tokio::test]
    async fn the_scoped_transport_enforces_the_boundary() {
        let sink = Arc::new(CapturingSink::default());
        let egress = Egress::new(CaptureTransport::default(), boundaries(), sink.clone());
        let scoped = egress.scoped(
            Provenance::tainted_by("secrets/prod.env"),
            EgressContext::new("anthropic"),
        );
        // Adapter-style call through the object-safe seam.
        let err = scoped
            .execute(a_request("secret"))
            .await
            .expect_err("scoped transport must refuse");
        assert_eq!(err, TransportError::Connect);
        // The event still fired even though the adapter only saw a transport error.
        assert_eq!(sink.events.lock().unwrap().len(), 1);
    }

    #[tokio::test]
    async fn empty_provenance_is_always_allowed() {
        let egress = Egress::new(
            CaptureTransport::default(),
            boundaries(),
            Arc::new(CapturingSink::default()),
        );
        let ctx = EgressContext::new("anthropic");
        let resp = egress
            .send(a_request("system prompt only"), &Provenance::empty(), &ctx)
            .await
            .expect("empty provenance allowed");
        assert_eq!(resp.status, 200);
    }

    #[test]
    fn egress_error_display_carries_no_content() {
        // AC-4: the typed error is safe to log — path + provider + action only.
        let err = EgressError::PrivacyBlocked {
            path: "secrets/prod.env".to_owned(),
            provider_id: ProviderId::from("anthropic"),
            action: PrivacyAction::ReroutedToLocal,
        };
        let rendered = err.to_string();
        assert!(rendered.contains("secrets/prod.env"));
        assert!(rendered.contains("anthropic"));
        // Never any content bytes.
        assert!(!rendered.contains("API_KEY"));
    }

    #[test]
    fn privacy_blocked_maps_to_a_connect_refusal_for_adapters() {
        let err = EgressError::PrivacyBlocked {
            path: "secrets/x".to_owned(),
            provider_id: ProviderId::from("p"),
            action: PrivacyAction::ReroutedToLocal,
        };
        assert_eq!(err.into_transport_error(), TransportError::Connect);
    }
}

/// CI deny-check: the egress choke point must be the workspace's *only* HTTP
/// client (BR-1). Runs under `cargo test --workspace` — the same command CI's
/// test step invokes — so a regression fails the build, not just review.
#[cfg(test)]
mod deny_http_client {
    use std::path::{Path, PathBuf};

    /// Crate names that mean "this crate can open a network connection itself".
    /// Only `tetond` (this crate) is permitted any of them.
    const HTTP_CLIENT_CRATES: &[&str] = &[
        "reqwest",
        "hyper",
        "hyper-util",
        "isahc",
        "ureq",
        "curl",
        "surf",
        "attohttpc",
        "http-client",
        "actix-web",
        "awc",
    ];

    /// `<workspace>/crates`, derived from this crate's manifest dir.
    fn crates_dir() -> PathBuf {
        Path::new(env!("CARGO_MANIFEST_DIR"))
            .parent()
            .expect("tetond lives under <workspace>/crates")
            .to_path_buf()
    }

    /// The crate names a manifest lists as *shipping* dependencies — `[dependencies]`
    /// and `[build-dependencies]`, inline or sub-table form. Comments and
    /// `[dev-dependencies]` are ignored (test-only clients cannot ship). This is a
    /// deliberately small hand parser so the check needs no toml dependency; it
    /// covers the manifest shapes this workspace actually uses.
    fn shipping_dependency_names(manifest: &str) -> Vec<String> {
        let mut names = Vec::new();
        let mut in_deps = false;
        for raw in manifest.lines() {
            let line = match raw.find('#') {
                Some(i) => &raw[..i],
                None => raw,
            };
            let line = line.trim();
            if line.is_empty() {
                continue;
            }
            if let (Some(true), Some(true)) = (
                line.chars().next().map(|c| c == '['),
                line.chars().last().map(|c| c == ']'),
            ) {
                let section = line[1..line.len() - 1].trim();
                in_deps = false;
                if section == "dependencies" || section == "build-dependencies" {
                    in_deps = true;
                } else if let Some(crate_name) = section
                    .strip_prefix("dependencies.")
                    .or_else(|| section.strip_prefix("build-dependencies."))
                {
                    // Sub-table form `[dependencies.foo]` names one crate; its body
                    // lines are that crate's fields, not new dependencies.
                    names.push(crate_name.trim().to_owned());
                }
                continue;
            }
            if in_deps {
                let key = line
                    .split(['=', '.', ' ', '\t'])
                    .next()
                    .unwrap_or("")
                    .trim();
                if !key.is_empty() {
                    names.push(key.to_owned());
                }
            }
        }
        names
    }

    #[test]
    fn only_tetond_may_depend_on_an_http_client() {
        let dir = crates_dir();
        let mut offenders = Vec::new();
        for entry in std::fs::read_dir(&dir).expect("read crates dir") {
            let entry = entry.expect("dir entry");
            let name = entry.file_name().to_string_lossy().into_owned();
            if name == "tetond" {
                continue; // the one crate allowed to hold the client
            }
            let manifest_path = entry.path().join("Cargo.toml");
            if !manifest_path.exists() {
                continue;
            }
            let manifest = std::fs::read_to_string(&manifest_path).expect("read manifest");
            for dep in shipping_dependency_names(&manifest) {
                if HTTP_CLIENT_CRATES.contains(&dep.as_str()) {
                    offenders.push(format!("crate `{name}` declares HTTP client `{dep}`"));
                }
            }
        }
        assert!(
            offenders.is_empty(),
            "BR-1 requires the egress choke point be the ONLY HTTP client in the \
             workspace. Move network access behind `tetond`'s egress. Offenders: {offenders:?}"
        );
    }

    #[test]
    fn tetond_itself_declares_the_sole_http_client() {
        // Positive control: proves the client is actually wired here and that the
        // deny-list would fire if `tetond` were not the skipped crate.
        let manifest_path = Path::new(env!("CARGO_MANIFEST_DIR")).join("Cargo.toml");
        let manifest = std::fs::read_to_string(manifest_path).expect("read tetond manifest");
        let names = shipping_dependency_names(&manifest);
        assert!(
            names
                .iter()
                .any(|n| HTTP_CLIENT_CRATES.contains(&n.as_str())),
            "tetond must construct the sole HTTP client (reqwest)"
        );
    }

    #[test]
    fn parser_finds_inline_dependencies_but_ignores_comments_and_dev_deps() {
        let manifest = "\
[package]
name = \"x\"

[dependencies]
serde = \"1\"
# reqwest = \"0.12\"  <- a comment, must be ignored
teton-core = { path = \"../teton-core\" }

[dependencies.tokio]
version = \"1\"
features = [\"macros\"]

[dev-dependencies]
reqwest = \"0.12\"
";
        let names = shipping_dependency_names(manifest);
        assert!(names.contains(&"serde".to_owned()));
        assert!(names.contains(&"teton-core".to_owned()));
        assert!(names.contains(&"tokio".to_owned()));
        // The commented and dev-only reqwest must NOT appear.
        assert!(!names.contains(&"reqwest".to_owned()));
    }

    #[test]
    fn parser_catches_a_planted_shipping_client() {
        let manifest = "[dependencies]\nserde = \"1\"\nhyper = { version = \"1\" }\n";
        let names = shipping_dependency_names(manifest);
        assert!(names.contains(&"hyper".to_owned()));
    }
}
