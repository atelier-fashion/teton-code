//! Protocol-version negotiation.
//!
//! The client opens the connection by advertising the inclusive range of
//! protocol versions it can speak; the daemon intersects that with its own
//! range and picks the highest common version, or rejects the connection.
//! ACP performs the analogous exchange in `initialize`.

use serde::{Deserialize, Serialize};

use crate::jsonrpc::{error_code, RpcError};
use crate::methods::RpcMethod;
use crate::{ClientKind, ProtocolVersion, PROTOCOL_VERSION_MAX, PROTOCOL_VERSION_MIN};

/// The opening request a client sends to attach. ACP equivalent: `initialize`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct HandshakeParams {
    /// Kind of client attaching.
    pub client_kind: ClientKind,
    /// Client name for diagnostics (e.g. `"teton-cli"`).
    pub client_name: String,
    /// Client build version (e.g. a crate version).
    pub client_version: String,
    /// Lowest protocol version the client can speak.
    pub protocol_min: ProtocolVersion,
    /// Highest protocol version the client can speak.
    pub protocol_max: ProtocolVersion,
}

/// The daemon's answer to a successful handshake.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct HandshakeResult {
    /// The single protocol version both sides will use for this connection.
    pub protocol_version: ProtocolVersion,
    /// Daemon name for diagnostics.
    pub daemon_name: String,
    /// Daemon build version.
    pub daemon_version: String,
    /// Opaque capability tokens the daemon advertises (forward-compatible).
    #[serde(default)]
    pub capabilities: Vec<String>,
}

impl RpcMethod for HandshakeParams {
    const METHOD: &'static str = "handshake";
    type Result = HandshakeResult;
}

/// A handshake could not be completed.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum HandshakeError {
    /// The client and daemon ranges do not overlap.
    #[error(
        "no mutually supported protocol version: client [{client_min}, {client_max}], \
         daemon [{daemon_min}, {daemon_max}]"
    )]
    IncompatibleVersion {
        /// Lowest version the client offered.
        client_min: ProtocolVersion,
        /// Highest version the client offered.
        client_max: ProtocolVersion,
        /// Lowest version the daemon supports.
        daemon_min: ProtocolVersion,
        /// Highest version the daemon supports.
        daemon_max: ProtocolVersion,
    },
}

impl HandshakeError {
    /// Maps the error onto a wire [`RpcError`] with an application error code.
    #[must_use]
    pub fn to_rpc_error(&self) -> RpcError {
        match self {
            HandshakeError::IncompatibleVersion {
                client_min,
                client_max,
                daemon_min,
                daemon_max,
            } => RpcError::new(error_code::UNSUPPORTED_PROTOCOL_VERSION, self.to_string())
                .with_data(serde_json::json!({
                    "client_min": client_min,
                    "client_max": client_max,
                    "daemon_min": daemon_min,
                    "daemon_max": daemon_max,
                })),
        }
    }
}

/// Negotiates a single protocol version from two inclusive ranges.
///
/// Picks the highest version in the intersection of
/// `[daemon_min, daemon_max]` and `[client_min, client_max]`, or returns
/// [`HandshakeError::IncompatibleVersion`] when the ranges are disjoint.
///
/// # Errors
///
/// Returns [`HandshakeError::IncompatibleVersion`] if the ranges do not overlap.
pub fn negotiate(
    daemon_min: ProtocolVersion,
    daemon_max: ProtocolVersion,
    client_min: ProtocolVersion,
    client_max: ProtocolVersion,
) -> Result<ProtocolVersion, HandshakeError> {
    let low = daemon_min.max(client_min);
    let high = daemon_max.min(client_max);
    if low > high {
        Err(HandshakeError::IncompatibleVersion {
            client_min,
            client_max,
            daemon_min,
            daemon_max,
        })
    } else {
        Ok(high)
    }
}

/// Negotiates against this build's supported range
/// ([`PROTOCOL_VERSION_MIN`]..=[`PROTOCOL_VERSION_MAX`]).
///
/// # Errors
///
/// Returns [`HandshakeError::IncompatibleVersion`] if the client's advertised
/// range does not overlap this build's supported range.
pub fn negotiate_from(params: &HandshakeParams) -> Result<ProtocolVersion, HandshakeError> {
    negotiate(
        PROTOCOL_VERSION_MIN,
        PROTOCOL_VERSION_MAX,
        params.protocol_min,
        params.protocol_max,
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde::de::DeserializeOwned;

    fn round_trip<T>(value: &T)
    where
        T: Serialize + DeserializeOwned + PartialEq + std::fmt::Debug,
    {
        let json = serde_json::to_string(value).unwrap();
        let back: T = serde_json::from_str(&json).unwrap();
        assert_eq!(&back, value);
    }

    fn params(min: u32, max: u32) -> HandshakeParams {
        HandshakeParams {
            client_kind: ClientKind::Cli,
            client_name: "teton-cli".to_owned(),
            client_version: "0.1.0".to_owned(),
            protocol_min: ProtocolVersion(min),
            protocol_max: ProtocolVersion(max),
        }
    }

    #[test]
    fn handshake_params_and_result_round_trip() {
        round_trip(&params(1, 1));
        round_trip(&HandshakeResult {
            protocol_version: ProtocolVersion(1),
            daemon_name: "tetond".to_owned(),
            daemon_version: "0.1.0".to_owned(),
            capabilities: vec!["structured_mode".to_owned(), "local_tier".to_owned()],
        });
    }

    #[test]
    fn handshake_binds_to_its_method_name() {
        assert_eq!(HandshakeParams::METHOD, "handshake");
    }

    #[test]
    fn negotiate_picks_highest_common_version() {
        // Daemon [1,3], client [2,5] → 3 is the highest shared version.
        let v = negotiate(
            ProtocolVersion(1),
            ProtocolVersion(3),
            ProtocolVersion(2),
            ProtocolVersion(5),
        )
        .unwrap();
        assert_eq!(v, ProtocolVersion(3));
    }

    #[test]
    fn negotiate_from_this_build_range_succeeds_for_overlapping_client() {
        let v = negotiate_from(&params(1, 4)).unwrap();
        assert_eq!(v, PROTOCOL_VERSION_MAX);
    }

    #[test]
    fn negotiate_rejects_disjoint_ranges_with_typed_error() {
        // Client only speaks [4,5]; this build speaks [1,1] → no overlap.
        let err = negotiate_from(&params(4, 5)).unwrap_err();
        assert_eq!(
            err,
            HandshakeError::IncompatibleVersion {
                client_min: ProtocolVersion(4),
                client_max: ProtocolVersion(5),
                daemon_min: PROTOCOL_VERSION_MIN,
                daemon_max: PROTOCOL_VERSION_MAX,
            }
        );
    }

    #[test]
    fn negotiate_rejects_when_client_is_older_than_daemon() {
        // Daemon [3,4], client [1,2] → disjoint on the low side.
        let err = negotiate(
            ProtocolVersion(3),
            ProtocolVersion(4),
            ProtocolVersion(1),
            ProtocolVersion(2),
        )
        .unwrap_err();
        assert!(matches!(err, HandshakeError::IncompatibleVersion { .. }));
    }

    #[test]
    fn incompatible_version_maps_to_app_error_code() {
        let err = negotiate_from(&params(9, 9)).unwrap_err();
        let rpc = err.to_rpc_error();
        assert_eq!(rpc.code, error_code::UNSUPPORTED_PROTOCOL_VERSION);
        assert!(rpc.data.is_some());
        // The wire error round-trips like any other RpcError.
        round_trip(&rpc);
    }
}
