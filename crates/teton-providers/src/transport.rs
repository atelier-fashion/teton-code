//! The [`Transport`] indirection (architecture D-2).
//!
//! This is THE load-bearing decision of the provider layer. Adapters never
//! construct an HTTP client; they build a [`TransportRequest`] and hand it to a
//! `Transport`. `tetond`'s single egress choke point is the only implementor,
//! so every remote call — regardless of which adapter made it — passes through
//! one place where the privacy boundary (BR-1) is enforced, cost is recorded
//! (BR-2), and credentials are attached (BR-7). Because the trait is the *only*
//! way an adapter can reach the network, and this crate carries no HTTP client
//! dependency, "an adapter cannot bypass egress" is a compile-time property, not
//! a code-review hope.

use async_trait::async_trait;
use futures::Stream;
use std::pin::Pin;

/// HTTP method for a transport request. Chat/completions is always `POST`;
/// modeled as an enum so the contract is explicit rather than a bare string.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HttpMethod {
    /// HTTP POST.
    Post,
}

/// A fully-formed request for the transport to execute. The adapter fills in the
/// URL, protocol headers, and serialized body; the transport adds authentication
/// and performs the network call.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TransportRequest {
    /// HTTP method.
    pub method: HttpMethod,
    /// Absolute request URL (provider endpoint).
    pub url: String,
    /// Protocol headers (content-type, accept, provider version, …). The
    /// transport is responsible for adding the credential header — adapters
    /// never see a secret (BR-7).
    pub headers: Vec<(String, String)>,
    /// Serialized request body.
    pub body: Vec<u8>,
}

/// A stream of raw response byte chunks, as they arrive off the wire.
pub type ByteStream = Pin<Box<dyn Stream<Item = Result<Vec<u8>, TransportError>> + Send>>;

/// The transport's response: a status code plus a streaming body. The status is
/// surfaced so the adapter can classify 4xx / 5xx without reading the body.
pub struct TransportResponse {
    /// HTTP status code.
    pub status: u16,
    /// Streaming response body.
    pub body: ByteStream,
}

impl std::fmt::Debug for TransportResponse {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("TransportResponse")
            .field("status", &self.status)
            .field("body", &"<stream>")
            .finish()
    }
}

/// Which inspection inside the egress choke point refused a payload, in the
/// only vocabulary this seam can carry.
///
/// ## Why this exists rather than the daemon's own value
///
/// The authoritative one is `teton_protocol::events::BlockCause`, and it says
/// more: a redaction block there carries the finding's kind and byte span. It
/// cannot come through here. This crate performs no I/O and declares no
/// protocol dependency — the manifest comment at the top of `Cargo.toml` is the
/// load-bearing constraint that keeps the single choke point enforceable — so a
/// `BlockCause` crossing into it would be a new dependency edge bought for a
/// sentence.
///
/// So the cause crosses as three values and nothing more: **enough to choose
/// the right sentence, not enough to say anything about the payload**. That the
/// span does not fit through this seam is a feature — the `privacy_block` event
/// is where a locatable finding belongs, and it goes there directly from the
/// choke point.
///
/// ## No `Default`, deliberately
///
/// There is exactly one construction site (`tetond`'s
/// `EgressError::into_transport_error`), which maps a real cause. A default
/// would let a later call site write `PrivacyBlocked(…default…)` and silently
/// report a redaction block as a boundary — the precise untruth REQ-562 BR-3
/// exists to prevent.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BlockDetail {
    /// Content provenance intersected a declared `local-only` boundary — the
    /// block REQ-544 shipped.
    Boundary,
    /// The redaction scan (REQ-562) found something in the outbound payload.
    Redaction,
    /// The redaction scan **could not run** and the payload was blocked
    /// unscanned (REQ-562 BR-3, fail closed). This says nothing about what the
    /// payload contains, because nothing looked.
    ScanUnavailable,
}

impl std::fmt::Display for BlockDetail {
    /// The **log** clause, for the transport and provider error Displays.
    ///
    /// Not the user's sentence: a surface that has a person reading it composes
    /// its own wording from the value (the daemon's turn-failure sentence and
    /// the CLI's `privacy_block` line both do). What is fixed here is the rule
    /// those surfaces also follow — the scan-unavailable clause says the scan
    /// could not run, never that it found something, and no clause can carry
    /// payload content because this type has no field to carry it in.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let clause = match self {
            // Unchanged from REQ-544: this is the wording already in every log.
            BlockDetail::Boundary => "content is under a local-only privacy boundary",
            BlockDetail::Redaction => "the redaction scan found sensitive content",
            BlockDetail::ScanUnavailable => "the redaction scan could not run",
        };
        f.write_str(clause)
    }
}

/// A transport-level failure — i.e. one that occurs before any HTTP status is
/// known. HTTP 4xx/5xx are *not* transport errors; they arrive as a
/// [`TransportResponse::status`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum TransportError {
    /// The request timed out opening or reading the response.
    #[error("transport timed out")]
    Timeout,
    /// The connection could not be established.
    #[error("transport failed to connect")]
    Connect,
    /// A lower-level I/O error while reading the stream.
    #[error("transport I/O error")]
    Io,
    /// The egress choke point refused the request — because content provenance
    /// intersected a `local-only` privacy boundary (BR-1), or because the
    /// REQ-562 redaction scan blocked it. This is **not** a network fault and
    /// must never be retried: no connection was attempted, and the
    /// authoritative `privacy_block` event has already fired at the choke point.
    /// Distinct from [`TransportError::Connect`] precisely so the daemon can
    /// reroute the turn to the local tier rather than retry the blocked provider
    /// (REQ-544 M-1).
    ///
    /// It carries a [`BlockDetail`] because *which* inspection refused it is
    /// what the daemon's turn-failure sentence has to say (REQ-562 BR-3): a
    /// scan that could not run and a scan that found a credential are different
    /// problems with different fixes, and a single sentence for both sends the
    /// user looking for the wrong thing.
    #[error("egress refused: {0}")]
    PrivacyBlocked(BlockDetail),
}

/// The one seam through which adapters reach the network (D-2). Implemented by
/// `tetond`'s egress module; adapters only ever hold a `&dyn Transport`.
#[async_trait]
pub trait Transport: Send + Sync {
    /// Execute `request`, attaching authentication, and return the streaming
    /// response. A transport-level failure (timeout, connect) is an `Err`; an
    /// HTTP error status is a successful `Ok` with a 4xx/5xx
    /// [`TransportResponse::status`].
    async fn execute(&self, request: TransportRequest)
        -> Result<TransportResponse, TransportError>;
}
