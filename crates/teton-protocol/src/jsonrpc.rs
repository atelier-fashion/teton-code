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
/// The spec permits string or number ids; we model both and forbid the
/// `null` form (the daemon always issues concrete ids).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum Id {
    /// Numeric id (the daemon's default — a monotonic counter).
    Number(i64),
    /// String id (accepted for clients that prefer opaque tokens).
    Str(String),
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
/// application errors start at [`SERVER_ERROR_START`] and count downward.
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

    /// The client and daemon share no compatible protocol version
    /// (see [`crate::handshake`]).
    pub const UNSUPPORTED_PROTOCOL_VERSION: i64 = -32000;
    /// The referenced session id is unknown to the daemon.
    pub const UNKNOWN_SESSION: i64 = -32001;
    /// The referenced provider id is not configured.
    pub const UNKNOWN_PROVIDER: i64 = -32002;
    /// A configuration mutation was rejected (e.g. a raw key in `auth_ref`, BR-7).
    pub const CONFIG_REJECTED: i64 = -32003;
}

#[cfg(test)]
mod tests {
    use super::*;

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
        ] {
            assert!(code <= error_code::SERVER_ERROR_START);
            assert!(code > -32100);
        }
    }
}
