//! JSON-RPC 2.0 framing.
//!
//! The generic envelopes ([`Request`], [`Response`], [`Notification`]) are
//! parameterised over their payload type so the typed methods in
//! [`crate::methods`] and events in [`crate::events`] plug straight in. This
//! module owns only framing and error-code vocabulary — no transport.

use serde::{Deserialize, Serialize};
use serde_json::Value;

/// The only JSON-RPC version this protocol speaks.
pub const JSONRPC_VERSION: &str = "2.0";

/// Marker for the mandatory `"jsonrpc": "2.0"` member.
///
/// Serializes to the literal `"2.0"` and rejects any other value on the wire,
/// so a mismatched framing version fails fast at deserialize time.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct JsonRpcV2;

impl Serialize for JsonRpcV2 {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(JSONRPC_VERSION)
    }
}

impl<'de> Deserialize<'de> for JsonRpcV2 {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let raw = String::deserialize(deserializer)?;
        if raw == JSONRPC_VERSION {
            Ok(Self)
        } else {
            Err(serde::de::Error::custom(format!(
                "unsupported jsonrpc version {raw:?}, expected {JSONRPC_VERSION:?}"
            )))
        }
    }
}

/// A JSON-RPC request/response correlation id.
///
/// The spec permits string or number ids; the daemon always issues concrete
/// numeric ids. The [`Id::Null`] form is reserved for the one case the spec
/// mandates it: a response to a request whose id could not be determined (a parse
/// error, or a missing/malformed `id`). Using `null` there — rather than a
/// sentinel like `0` — means two such error responses cannot collide with each
/// other or with a real pending request id (REQ-544 minor).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum Id {
    /// Numeric id (the daemon's default — a monotonic counter).
    Number(i64),
    /// String id (accepted for clients that prefer opaque tokens).
    Str(String),
    /// The JSON `null` id — only for an error response to an unidentifiable
    /// request (never issued for a real call). Serializes to `null`. Listed last
    /// so a numeric/string id never matches it.
    Null,
}

impl From<i64> for Id {
    fn from(value: i64) -> Self {
        Self::Number(value)
    }
}

impl From<&str> for Id {
    fn from(value: &str) -> Self {
        Self::Str(value.to_owned())
    }
}

/// A JSON-RPC request: a method call that expects a matching [`Response`].
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Request<P> {
    /// Framing version marker; always `"2.0"`.
    pub jsonrpc: JsonRpcV2,
    /// Correlation id echoed back in the response.
    pub id: Id,
    /// Method name (see [`crate::methods`]).
    pub method: String,
    /// Typed parameters for `method`.
    pub params: P,
}

impl<P> Request<P> {
    /// Builds a request with the framing marker filled in.
    pub fn new(id: Id, method: impl Into<String>, params: P) -> Self {
        Self {
            jsonrpc: JsonRpcV2,
            id,
            method: method.into(),
            params,
        }
    }
}

/// A JSON-RPC notification: a fire-and-forget message with no id and no reply.
///
/// The daemon broadcasts events as notifications (see [`crate::events`]).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Notification<P> {
    /// Framing version marker; always `"2.0"`.
    pub jsonrpc: JsonRpcV2,
    /// Method name.
    pub method: String,
    /// Typed parameters for `method`.
    pub params: P,
}

impl<P> Notification<P> {
    /// Builds a notification with the framing marker filled in.
    pub fn new(method: impl Into<String>, params: P) -> Self {
        Self {
            jsonrpc: JsonRpcV2,
            method: method.into(),
            params,
        }
    }
}

/// A JSON-RPC response: exactly one of `result` / `error` is present.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Response<R> {
    /// Framing version marker; always `"2.0"`.
    pub jsonrpc: JsonRpcV2,
    /// Correlation id copied from the originating [`Request`].
    pub id: Id,
    /// Success payload; `None` when `error` is set.
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub result: Option<R>,
    /// Failure payload; `None` when `result` is set.
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub error: Option<RpcError>,
}

impl<R> Response<R> {
    /// A successful response carrying `result`.
    pub fn success(id: Id, result: R) -> Self {
        Self {
            jsonrpc: JsonRpcV2,
            id,
            result: Some(result),
            error: None,
        }
    }

    /// A failed response carrying `error`.
    #[must_use]
    pub fn failure(id: Id, error: RpcError) -> Self {
        Self {
            jsonrpc: JsonRpcV2,
            id,
            result: None,
            error: Some(error),
        }
    }

    /// True when this response carries a `result` rather than an `error`.
    #[must_use]
    pub fn is_success(&self) -> bool {
        self.error.is_none()
    }
}

/// A JSON-RPC error object. Doubles as a [`std::error::Error`] via `thiserror`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, thiserror::Error)]
#[error("json-rpc error {code}: {message}")]
pub struct RpcError {
    /// Numeric error code (see the `error_code` constants).
    pub code: i64,
    /// Human-readable, machine-safe message. Never carries file content,
    /// prompt text, or credentials (conventions: privacy in error text).
    pub message: String,
    /// Optional structured detail.
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub data: Option<Value>,
}

impl RpcError {
    /// Builds an error with no `data` member.
    pub fn new(code: i64, message: impl Into<String>) -> Self {
        Self {
            code,
            message: message.into(),
            data: None,
        }
    }

    /// Attaches a structured `data` member.
    #[must_use]
    pub fn with_data(mut self, data: Value) -> Self {
        self.data = Some(data);
        self
    }
}

/// JSON-RPC error codes.
///
/// The standard range (`-32768..=-32000`) is reserved by the JSON-RPC spec;
/// application errors start at [`error_code::SERVER_ERROR_START`] and count
/// downward.
///
/// **Every** application code the protocol uses is declared here, in the one
/// `application_error_codes!` block below — including codes whose only producer
/// lives in another crate. One module owning the whole numbering is what makes
/// a collision impossible rather than merely tested for: a second module
/// picking "the next free code" can only ever be picking it against a snapshot
/// of this one.
pub mod error_code {
    /// Invalid JSON was received (spec-reserved).
    pub const PARSE_ERROR: i64 = -32700;
    /// The JSON is not a valid Request object (spec-reserved).
    pub const INVALID_REQUEST: i64 = -32600;
    /// The method does not exist (spec-reserved).
    pub const METHOD_NOT_FOUND: i64 = -32601;
    /// Invalid method parameters (spec-reserved).
    pub const INVALID_PARAMS: i64 = -32602;
    /// Internal JSON-RPC error (spec-reserved).
    pub const INTERNAL_ERROR: i64 = -32603;

    /// First application-defined error code. App errors occupy the
    /// implementation-defined server range and count down from here.
    pub const SERVER_ERROR_START: i64 = -32000;

    /// Declares the application-defined error codes, and derives [`ALL`] from
    /// the same declarations. The distinctness guard runs over `ALL`, so a code
    /// added below is covered the moment it is written — there is no second
    /// list of "codes to check" that can fall behind this one.
    macro_rules! application_error_codes {
        ($( $(#[$attr:meta])* $name:ident = $code:literal; )+) => {
            $( $(#[$attr])* pub const $name: i64 = $code; )+

            /// Every application-defined error code as a `(name, code)` pair,
            /// generated from the declarations above. Used by the guard test
            /// that proves no two of them share a number, and available to any
            /// caller that needs to reason over the whole set.
            pub const ALL: &[(&str, i64)] = &[$( (stringify!($name), $name) ),+];
        };
    }

    application_error_codes! {
        /// The client and daemon share no compatible protocol version
        /// (see [`crate::handshake`]).
        UNSUPPORTED_PROTOCOL_VERSION = -32000;
        /// The referenced session id is unknown to the daemon.
        UNKNOWN_SESSION = -32001;
        /// The referenced provider id is not configured.
        UNKNOWN_PROVIDER = -32002;
        /// A configuration mutation was rejected (e.g. a raw key in `auth_ref`, BR-7).
        CONFIG_REJECTED = -32003;
        /// A turn could not complete because its content is under a `local-only`
        /// privacy boundary (BR-1) and no local tier was available to serve it after
        /// the remote egress was blocked (REQ-544 M-1). A specific, content-free
        /// privacy signal — never a generic `INTERNAL_ERROR`.
        PRIVACY_BLOCKED = -32004;
        /// A turn arrived while the local tier was still coming up — its weights are
        /// downloading, or they are installed and the daemon is loading and
        /// benchmarking them — and nothing else could serve it.
        ///
        /// Split out of [`UNKNOWN_PROVIDER`] because it is the one unserved-turn
        /// state that **resolves on its own**: the user has nothing to fix and
        /// nothing to answer, only to wait. A client renders it as a waiting notice
        /// rather than as a failure; every other unserved-turn cause — declined,
        /// unanswered, below the RAM floor, load failed, no loader — needs an action
        /// and keeps [`UNKNOWN_PROVIDER`].
        TIER_WARMING = -32005;
        /// The daemon evicted this client's event subscription because it fell
        /// too far behind the event stream (`tetond::broadcast`).
        ///
        /// Reaches the client as the payload of a `subscription/lagged`
        /// notification, not as a response error — nothing was requested and so
        /// nothing failed. It is numbered here anyway: a client that keys off
        /// the numeric code alone must never confuse an evicted subscription
        /// with a turn blocked by a privacy boundary.
        SUBSCRIPTION_LAGGED = -32006;
        /// The daemon has committed to exiting and will not serve a new session
        /// (REQ-565 BR-3).
        ///
        /// The *second* arm of the connect-vs-shutdown race. A daemon whose last
        /// client left decides to exit under the same lock that admits clients,
        /// so a handshake arriving after that decision is refused here rather
        /// than accepted into a session the daemon will not serve.
        ///
        /// This is a **retryable** condition and the only handshake error that
        /// is: the client's remedy is to autostart a successor and connect to
        /// it, not to report a failure. It is deliberately distinct from
        /// [`UNSUPPORTED_PROTOCOL_VERSION`], which is *not* retryable — spawning
        /// a new daemon cannot fix a stale binary on disk, so a client that
        /// folded the two together would spin.
        DAEMON_SHUTTING_DOWN = -32007;
        /// Another turn is already running on this session, so this prompt was
        /// refused rather than interleaved into the same conversation
        /// (REQ-567 BR-5, architecture D-3).
        ///
        /// The **second** transient code, numbered for [`TIER_WARMING`]'s
        /// reason: it resolves without the user doing anything — the turn
        /// holding the session finishes — so the remedy is to retry, not to fix
        /// something. A concurrent prompt is a state a client can legitimately
        /// reach and legitimately retry from, and reporting it as an
        /// [`INTERNAL_ERROR`] would tell the user their turn broke when the
        /// daemon knew exactly why it refused (LESSON-456).
        ///
        /// The accompanying message names the in-flight turn, because BR-5
        /// requires a refusal to name its cause rather than surface as a
        /// generic turn failure. The daemon classifies here; the client renders
        /// it as a waiting notice, exactly as it does `TIER_WARMING` (BUG-152).
        SESSION_BUSY = -32008;
        /// A mutating method — `session/prompt`, `session/clear` — named a
        /// session this connection is not attached to, and was refused before
        /// it reached the runtime (REQ-568 BR-4).
        ///
        /// Distinct from [`UNKNOWN_SESSION`], which says the daemon has no
        /// such session at all: this says the session is simply not this
        /// connection's to drive, and the remedy is `session/attach` rather
        /// than creating a session or retrying. Attachment is the single grant
        /// seam for session access — a mutating method carries no implicit
        /// grant of its own — so a client that folded the two codes together
        /// would tell a user their session had vanished when it is only
        /// someone else's.
        NOT_ATTACHED = -32009;
        /// The connection's process descends from this daemon's own process
        /// tree — or could not be shown not to — so it may never attach to a
        /// session or declare `monitor` (REQ-569 BR-4, ADR-A).
        ///
        /// **The terminal refusal.** Unlike [`NOT_GRANTED`], this one has no
        /// remedy and no consent path: there is nothing the connection can ask
        /// for, nothing a user can approve, and no retry that changes the
        /// answer. It keys on kernel-attested process ancestry rather than on
        /// what the connection happens to do, so it holds for a tool child, an
        /// MCP server subprocess, and any future daemon-spawned process that
        /// links the client crate.
        ///
        /// Distinct from [`NOT_ATTACHED`] in *which* question was asked:
        /// `NOT_ATTACHED` answers "may this connection drive a session it never
        /// attached to" and names `session/attach` as the remedy; this answers
        /// "may this connection attach at all", and the answer is no. A client
        /// that folded them together would advise a daemon child to attach —
        /// the one thing it can never do.
        ATTACH_FORBIDDEN = -32010;
        /// The connection holds no grant, and none was sought (REQ-569
        /// BR-1/BR-2).
        ///
        /// Two things answer it, and neither is `session/attach`'s ordinary
        /// ungranted path — that one raises a consent request and ends in
        /// [`CONSENT_DENIED`] or [`CONSENT_TIMEOUT`] instead:
        ///
        /// - **A `monitor` declaration without a monitor-scope grant.**
        ///   Terminal, like [`ATTACH_FORBIDDEN`]: REQ-569's verify pass removed
        ///   the consent path to `monitor` (it was mintable by one peer holding
        ///   two connections), so there is nothing to ask for and no retry that
        ///   changes the answer.
        /// - **A connection with too many consent requests already
        ///   outstanding.** Remediable, and the only remedy is time: it stops
        ///   applying as the caller's own pending prompts resolve.
        ///
        /// Distinct from [`UNKNOWN_SESSION`]: it is issued *before* the session
        /// registry is consulted and is identical for a session that exists and
        /// one that does not, precisely so it cannot be read as an existence
        /// oracle.
        ///
        /// Distinct from [`NOT_ATTACHED`] by which door was closed:
        /// `NOT_ATTACHED` is refused at a *mutating* method and its remedy is
        /// `session/attach`; this is refused at the handshake or at
        /// `session/attach` itself, so the remedy `NOT_ATTACHED` names is not
        /// available here.
        NOT_GRANTED = -32011;
        /// A user was asked whether to grant this connection the session (or
        /// `monitor`) and answered **no** (REQ-569 BR-5/BR-7).
        ///
        /// BR-5's third distinct reason, and the difference from
        /// [`NOT_GRANTED`] is *whether anyone was asked*: `NOT_GRANTED` says no
        /// grant existed and none could be sought, while this says the question
        /// was put to a user and the user declined. A client folding them
        /// together would tell someone their request was never seen when it was
        /// seen and refused — and would go on to retry a request a human just
        /// turned down.
        ///
        /// Mints nothing. A denied decision leaves the grant registry exactly
        /// as it found it (BR-7, LESSON-501 — the decision travels with the
        /// grant).
        CONSENT_DENIED = -32012;
        /// A consent request went unanswered for the daemon's bounded window
        /// and resolved to denied (REQ-569 BR-7, AC-6).
        ///
        /// BR-5's fourth reason, and the one that is **not** a decision: no
        /// user said no, no user said anything. It is distinct from
        /// [`CONSENT_DENIED`] because it is the one refusal on this seam that a
        /// plain retry can legitimately fix — the prompt may have been rendered
        /// on a surface nobody was looking at — whereas retrying past a denial
        /// is asking a user who already answered to answer again.
        ///
        /// Fail-closed by construction: the window elapsing *is* the refusal,
        /// so a daemon that crashed mid-consent, a client that never rendered
        /// the prompt, and a user who walked away all end in the same place,
        /// holding nothing.
        CONSENT_TIMEOUT = -32013;
        /// An approval that would mint a grant arrived with no verified human
        /// presence behind it (REQ-570 BR-1, BR-3).
        ///
        /// **This is the refusal that closes the REQ-569 self-approval
        /// residual.** REQ-569 routes a consent prompt back to the requesting
        /// connection when nothing is attached to the target session — correct
        /// for a user reopening their own CLI, and for a headless same-UID
        /// process it meant the "consent" was self-issued with no human
        /// involved. The routing arm survives, because refusing it would break
        /// resume; what changed is that its answer now mints nothing without a
        /// presence check the daemon itself performed.
        ///
        /// Distinct from [`CONSENT_DENIED`] by *who* refused: a denial is a
        /// human saying no, this is the daemon declining to believe a human was
        /// there at all. A client that folded them together would tell a user
        /// they had declined something they never saw.
        ///
        /// Distinct from [`ATTESTATION_UNAVAILABLE`] by whether asking was even
        /// possible: this says nobody proved presence, that one says nobody
        /// *could*.
        ATTESTATION_REQUIRED = -32014;
        /// A human was asked to prove presence and did not authenticate
        /// (REQ-570 BR-7; `LAError -1`).
        ///
        /// A decision-shaped ending, like [`CONSENT_DENIED`]: someone was at
        /// the machine and the check did not pass. Kept apart from
        /// [`ATTESTATION_CANCELLED`] because the remedies differ — a failed
        /// fingerprint is worth retrying, a deliberate dismissal is not.
        ATTESTATION_FAILED = -32015;
        /// The presence prompt was dismissed — by the user, the system, or the
        /// daemon itself (REQ-570 BR-7; `LAError -2 / -4 / -9`).
        ///
        /// `-9` (`appCancel`) is the arm the BR-12 spike actually observed, and
        /// is what the daemon's own timeout produces when it takes a stale
        /// dialog down.
        ATTESTATION_CANCELLED = -32016;
        /// The presence prompt went unanswered for its bounded window
        /// (REQ-570 BR-7).
        ///
        /// [`CONSENT_TIMEOUT`]'s counterpart one layer in: that one is nobody
        /// answering the *consent* question, this is nobody answering the
        /// *presence* one. Both fail closed; a client renders them the same way
        /// and they stay numbered apart so an operator can tell which surface
        /// was ignored.
        ATTESTATION_TIMEOUT = -32017;
        /// No usable presence mechanism exists here, so cross-session attach is
        /// refused outright (REQ-570 BR-8, BR-11).
        ///
        /// **Terminal, like [`ATTACH_FORBIDDEN`]** — no retry changes it, and
        /// the accompanying message names the specific cause rather than
        /// failing generically (AC-7b). The case this exists for is headless
        /// Linux: the BR-12 spike confirmed a polkit authority can be on the
        /// bus and still answer "no agent is available", and that the textual
        /// agent fallback needs a `/dev/tty` neither a headless host nor the VS
        /// Code extension has.
        ///
        /// It is a **refusal**, never a fall-through. A platform without a
        /// mechanism must not quietly return to the self-approval residual this
        /// REQ exists to close — that is the whole of BR-11.
        ATTESTATION_UNAVAILABLE = -32018;
        /// A monitor-scope request was answered by the connection that raised
        /// it (REQ-570 BR-5).
        ///
        /// `monitor` is sight of every session on the machine, so the approver
        /// must never be the requester **under any routing arm**. REQ-569's
        /// verify pass found the previous monitor path was mintable by one
        /// attacker holding two connections; attestation is what breaks that
        /// attack, and this check remains as a structural invariant with its own
        /// regression test rather than a property merely avoided by
        /// construction (LESSON-502).
        SELF_APPROVAL_REFUSED = -32019;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_application_error_code_is_distinct() {
        // Structural, not enumerated: it runs over `error_code::ALL`, which the
        // declaration macro generates from the constants themselves. A code
        // added later is covered without anyone remembering to extend a list —
        // a hand-maintained roster of what to check is the same shape of bug as
        // the collision it is meant to catch.
        for (i, (name_a, a)) in error_code::ALL.iter().enumerate() {
            for (name_b, b) in &error_code::ALL[i + 1..] {
                assert_ne!(a, b, "{name_a} and {name_b} both use code {a}");
            }
        }
    }

    #[test]
    fn every_application_error_code_sits_in_the_server_range() {
        // The JSON-RPC spec reserves -32768..=-32000 and carves out
        // -32099..=-32000 for implementation-defined server errors. Codes count
        // down from `SERVER_ERROR_START`; drifting outside that window would
        // collide with the spec's own vocabulary rather than with a sibling.
        for (name, code) in error_code::ALL {
            assert!(
                (error_code::SERVER_ERROR_START - 99..=error_code::SERVER_ERROR_START)
                    .contains(code),
                "{name} ({code}) is outside the implementation-defined server range"
            );
        }
    }

    #[test]
    fn jsonrpc_marker_round_trips_and_rejects_other_versions() {
        assert_eq!(serde_json::to_string(&JsonRpcV2).unwrap(), "\"2.0\"");
        let ok: JsonRpcV2 = serde_json::from_str("\"2.0\"").unwrap();
        assert_eq!(ok, JsonRpcV2);
        let bad: Result<JsonRpcV2, _> = serde_json::from_str("\"1.0\"");
        assert!(bad.is_err());
    }

    #[test]
    fn id_accepts_number_and_string() {
        let n: Id = serde_json::from_str("7").unwrap();
        assert_eq!(n, Id::Number(7));
        let s: Id = serde_json::from_str("\"abc\"").unwrap();
        assert_eq!(s, Id::Str("abc".to_owned()));
        assert_eq!(serde_json::to_string(&Id::Number(7)).unwrap(), "7");
        assert_eq!(serde_json::to_string(&Id::from("abc")).unwrap(), "\"abc\"");
    }

    #[test]
    fn null_id_round_trips_and_is_distinct() {
        // The spec-mandated null id for an unidentifiable request serializes to
        // `null` and parses back — and never equals a real numeric/string id, so
        // two parse-error responses can't collide (REQ-544 minor).
        assert_eq!(serde_json::to_string(&Id::Null).unwrap(), "null");
        let back: Id = serde_json::from_str("null").unwrap();
        assert_eq!(back, Id::Null);
        assert_ne!(Id::Null, Id::Number(0));
        // A numeric id must still parse as Number, not accidentally as Null.
        assert_eq!(serde_json::from_str::<Id>("0").unwrap(), Id::Number(0));

        // It round-trips inside a full error response, too.
        let resp = Response::<Value>::failure(Id::Null, RpcError::new(-32700, "invalid json"));
        let json = serde_json::to_string(&resp).unwrap();
        assert!(json.contains("\"id\":null"), "{json}");
        let back: Response<Value> = serde_json::from_str(&json).unwrap();
        assert_eq!(back.id, Id::Null);
    }

    #[test]
    fn request_round_trips() {
        let req = Request::new(Id::Number(1), "session/list", serde_json::json!({}));
        let json = serde_json::to_string(&req).unwrap();
        let back: Request<Value> = serde_json::from_str(&json).unwrap();
        assert_eq!(back, req);
        assert!(json.contains("\"jsonrpc\":\"2.0\""));
    }

    #[test]
    fn notification_round_trips() {
        let note = Notification::new("event", serde_json::json!({"k": 1}));
        let json = serde_json::to_string(&note).unwrap();
        let back: Notification<Value> = serde_json::from_str(&json).unwrap();
        assert_eq!(back, note);
    }

    #[test]
    fn success_response_omits_error_member() {
        let resp = Response::success(Id::Number(1), 42_u32);
        let json = serde_json::to_string(&resp).unwrap();
        assert!(!json.contains("error"));
        assert!(resp.is_success());
        let back: Response<u32> = serde_json::from_str(&json).unwrap();
        assert_eq!(back, resp);
    }

    #[test]
    fn failure_response_omits_result_member() {
        let resp: Response<u32> = Response::failure(
            Id::Number(1),
            RpcError::new(error_code::UNKNOWN_SESSION, "no such session"),
        );
        let json = serde_json::to_string(&resp).unwrap();
        assert!(!json.contains("result"));
        assert!(!resp.is_success());
        let back: Response<u32> = serde_json::from_str(&json).unwrap();
        assert_eq!(back, resp);
    }

    #[test]
    fn rpc_error_round_trips_with_data() {
        let err = RpcError::new(error_code::CONFIG_REJECTED, "rejected")
            .with_data(serde_json::json!({"field": "auth_ref"}));
        let json = serde_json::to_string(&err).unwrap();
        let back: RpcError = serde_json::from_str(&json).unwrap();
        assert_eq!(back, err);
    }

    #[test]
    fn app_error_codes_are_below_the_reserved_boundary() {
        assert_eq!(error_code::SERVER_ERROR_START, -32000);
        // App codes occupy the implementation-defined server-error range,
        // counting down from the start. The loop binding keeps each comparison
        // a runtime check rather than a const-folded assertion.
        for code in [
            error_code::UNSUPPORTED_PROTOCOL_VERSION,
            error_code::UNKNOWN_SESSION,
            error_code::UNKNOWN_PROVIDER,
            error_code::CONFIG_REJECTED,
            error_code::PRIVACY_BLOCKED,
        ] {
            assert!(code <= error_code::SERVER_ERROR_START);
            assert!(code > -32100);
        }
    }
}
