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
    /// The egress choke point refused the request because its content provenance
    /// intersected a `local-only` privacy boundary (BR-1). This is **not** a
    /// network fault and must never be retried: no connection was attempted, and
    /// the authoritative `privacy_block` event has already fired at the choke
    /// point. Distinct from [`TransportError::Connect`] precisely so the daemon
    /// can reroute the turn to the local tier rather than retry the blocked
    /// provider (REQ-544 M-1).
    #[error("egress refused: content is under a local-only privacy boundary")]
    PrivacyBlocked,
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
