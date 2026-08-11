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
    /// Receive every session's events, not just this connection's own
    /// (REQ-568 BR-5). An explicit opt-in the daemon logs at handshake, so a
    /// monitor's existence is observable rather than inferred from traffic.
    ///
    /// Defaulted, and `false` is the *accurate* default rather than a
    /// placeholder: a client that omits the key predates REQ-568 and asked for
    /// nothing, so it must attach as an ordinary client rather than fail the
    /// handshake on a field it has never heard of.
    #[serde(default)]
    pub monitor: bool,
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

/// Which half of a version-incompatible pairing is the older build.
///
/// The two cases need *opposite* remedies — restart the daemon versus upgrade
/// the client — so a surface that renders one sentence for both is telling half
/// its users to do the wrong thing. Derived here, from the numbers alone, so the
/// daemon's log line and the CLI's terminal line cannot disagree about who is
/// behind.
///
/// The sentences themselves deliberately live at the surfaces: this crate is
/// transport-free and knows nothing about `brew`, launchd, or how a particular
/// client is installed.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum VersionSkew {
    /// The daemon speaks only versions below everything the client offers — an
    /// older daemon still running after the binaries on disk were upgraded.
    /// This is the common one: the socket path is stable across releases
    /// (ADR-007), so a new CLI finds the old daemon and attaches to it.
    DaemonIsOlder,
    /// The daemon speaks only versions above everything the client offers — a
    /// stale client against a current daemon.
    ClientIsOlder,
}

impl VersionSkew {
    /// Classifies two disjoint ranges. `None` when they overlap (no skew) or
    /// when either range is itself inverted (`min > max`), which is a malformed
    /// advertisement rather than a version difference.
    #[must_use]
    pub fn classify(
        client_min: ProtocolVersion,
        client_max: ProtocolVersion,
        daemon_min: ProtocolVersion,
        daemon_max: ProtocolVersion,
    ) -> Option<Self> {
        if client_min > client_max || daemon_min > daemon_max {
            return None;
        }
        if daemon_max < client_min {
            Some(VersionSkew::DaemonIsOlder)
        } else if daemon_min > client_max {
            Some(VersionSkew::ClientIsOlder)
        } else {
            None
        }
    }

    /// A neutral clause naming who is behind, with no installer-specific advice.
    /// Surfaces that know how the binaries are managed append their own remedy.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            VersionSkew::DaemonIsOlder => "the running daemon is the older build",
            VersionSkew::ClientIsOlder => "this client is the older build",
        }
    }
}

/// The four advertised bounds, recovered from a wire
/// [`error_code::UNSUPPORTED_PROTOCOL_VERSION`] response.
///
/// [`HandshakeError::to_rpc_error`] puts them in the error's `data` member
/// precisely so the rejected client can say something better than the daemon's
/// own generic sentence — and the released daemons this build has to explain
/// itself to already emit exactly this shape, which is what makes the remedy
/// reachable without changing them.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct VersionMismatch {
    /// Lowest version the client offered.
    pub client_min: ProtocolVersion,
    /// Highest version the client offered.
    pub client_max: ProtocolVersion,
    /// Lowest version the daemon supports.
    pub daemon_min: ProtocolVersion,
    /// Highest version the daemon supports.
    pub daemon_max: ProtocolVersion,
}

impl VersionMismatch {
    /// Reads the bounds out of an `UNSUPPORTED_PROTOCOL_VERSION` error.
    ///
    /// `None` for any other error code, and for a matching code whose `data` is
    /// absent or malformed — a caller still knows it is a version problem from
    /// the code, and must not present a fabricated range.
    #[must_use]
    pub fn from_rpc_error(err: &RpcError) -> Option<Self> {
        if err.code != error_code::UNSUPPORTED_PROTOCOL_VERSION {
            return None;
        }
        let data = err.data.as_ref()?;
        let field = |name: &str| {
            data.get(name)
                .and_then(serde_json::Value::as_u64)
                .and_then(|v| u32::try_from(v).ok())
                .map(ProtocolVersion)
        };
        Some(Self {
            client_min: field("client_min")?,
            client_max: field("client_max")?,
            daemon_min: field("daemon_min")?,
            daemon_max: field("daemon_max")?,
        })
    }

    /// Which side is behind, or `None` if these bounds in fact overlap.
    #[must_use]
    pub fn skew(&self) -> Option<VersionSkew> {
        VersionSkew::classify(
            self.client_min,
            self.client_max,
            self.daemon_min,
            self.daemon_max,
        )
    }
}

/// Renders an inclusive range the way a person reads it: a single number when
/// the build speaks exactly one version, `a–b` when it spans several.
#[must_use]
pub fn format_range(min: ProtocolVersion, max: ProtocolVersion) -> String {
    if min == max {
        min.to_string()
    } else {
        format!("{min}–{max}")
    }
}

/// A *build*-version difference between a client and the daemon it just
/// successfully attached to (REQ-565 BR-6).
///
/// # Why this is not [`VersionSkew`]
///
/// [`VersionSkew`] classifies **protocol** ranges, and it only exists on a path
/// that has already *failed*: the ranges are disjoint, the handshake is
/// refused, and the client is told to restart something. That check is silent
/// precisely when the harm REQ-565 cites occurs — a v0.1.12 daemon still
/// serving four hours after v0.1.13 was installed. Two adjacent releases almost
/// always speak the same protocol version, so negotiation succeeds and nothing
/// is said, while every command is answered by the old binary.
///
/// So this is a different question with a different answer shape: the handshake
/// **succeeded**, both halves work, and the user is being told a true thing
/// about which build answered — a notice, never an error (BR-6).
///
/// Carries both versions because a warning that names only one of them cannot
/// be acted on.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BuildSkew {
    /// The version the client is built at.
    pub client_version: String,
    /// The version the answering daemon reports.
    pub daemon_version: String,
}

/// Classifies a build-version difference, or `None` when the two agree.
///
/// Deliberately a plain string inequality rather than a semver comparison. The
/// remedy is the same in both directions — end every session so the next start
/// runs whatever is on disk now — so ordering the two versions would add a
/// parse that can fail (on a `-dev` suffix, a git describe, a locally built
/// binary) in exchange for a distinction nothing consumes. Any difference is
/// worth one line; no difference is worth none.
///
/// Whitespace is trimmed so a trailing newline picked up from a version file
/// cannot masquerade as a skew.
#[must_use]
pub fn build_skew(client_version: &str, daemon_version: &str) -> Option<BuildSkew> {
    let client = client_version.trim();
    let daemon = daemon_version.trim();
    if client == daemon {
        return None;
    }
    Some(BuildSkew {
        client_version: client.to_owned(),
        daemon_version: daemon.to_owned(),
    })
}

/// A handshake could not be completed.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum HandshakeError {
    /// The client and daemon ranges do not overlap.
    ///
    /// The message names the numbers *and* who is behind: this string is what a
    /// client too old to decode the error's `data` prints verbatim, so it is the
    /// only diagnosis some readers ever get.
    #[error(
        "no mutually supported protocol version: client speaks {}, daemon speaks {} — {}. \
         An upgrade that replaced the binaries on disk does not restart a daemon that is \
         already running.",
        format_range(*client_min, *client_max),
        format_range(*daemon_min, *daemon_max),
        VersionSkew::classify(*client_min, *client_max, *daemon_min, *daemon_max)
            .map_or("the advertised ranges do not overlap", VersionSkew::as_str)
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
            monitor: false,
        }
    }

    #[test]
    fn handshake_params_and_result_round_trip() {
        round_trip(&params(1, 1));
        // A declared monitor survives the wire too — a declaration that only
        // held on the sending side would be no declaration at all (BR-5).
        round_trip(&HandshakeParams {
            monitor: true,
            ..params(1, 1)
        });
        round_trip(&HandshakeResult {
            protocol_version: ProtocolVersion(1),
            daemon_name: "teton-code".to_owned(),
            daemon_version: "0.1.0".to_owned(),
            capabilities: vec!["structured_mode".to_owned(), "local_tier".to_owned()],
        });
    }

    /// A build that predates REQ-568 sends no `monitor` key at all, and the
    /// handshake is the one frame that cannot be allowed to fail on an
    /// unrecognized field — a client refused here never reaches a method that
    /// could explain itself. Written as the JSON such a client actually emits
    /// rather than as a re-serialized value, so the assertion runs through the
    /// real decode path instead of through this build's own output.
    #[test]
    fn a_handshake_that_predates_the_monitor_field_attaches_as_a_non_monitor() {
        let legacy = r#"{
            "client_kind": "cli",
            "client_name": "teton-cli",
            "client_version": "0.1.13",
            "protocol_min": 1,
            "protocol_max": 1
        }"#;
        let params: HandshakeParams =
            serde_json::from_str(legacy).expect("an older client's handshake must still decode");
        assert!(!params.monitor, "silence is not a monitor declaration");
        // The rest of the frame is unaffected — the field is additive.
        assert_eq!(params.client_kind, ClientKind::Cli);
        assert_eq!(params.protocol_max, ProtocolVersion(1));
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

    /// A released v1 daemon meeting this build is the pairing the whole version
    /// gate exists for: an upgrade replaced the binaries, nothing restarted the
    /// daemon, and the stable socket path (ADR-007) hands the new CLI straight
    /// to the old process. It must be refused *at the handshake*, before any
    /// method can return a shape this build cannot read.
    #[test]
    fn a_released_v1_daemon_is_refused_before_any_method_runs() {
        let err = negotiate(
            ProtocolVersion(1),
            ProtocolVersion(1),
            PROTOCOL_VERSION_MIN,
            PROTOCOL_VERSION_MAX,
        )
        .unwrap_err();
        let HandshakeError::IncompatibleVersion {
            client_min,
            client_max,
            daemon_min,
            daemon_max,
        } = err;
        assert_eq!(
            VersionSkew::classify(client_min, client_max, daemon_min, daemon_max),
            Some(VersionSkew::DaemonIsOlder)
        );
    }

    #[test]
    fn skew_names_whichever_side_is_behind_and_stays_silent_when_ranges_overlap() {
        let v = ProtocolVersion;
        assert_eq!(
            VersionSkew::classify(v(2), v(2), v(1), v(1)),
            Some(VersionSkew::DaemonIsOlder)
        );
        assert_eq!(
            VersionSkew::classify(v(1), v(1), v(2), v(2)),
            Some(VersionSkew::ClientIsOlder)
        );
        // Touching ranges overlap at a single version — negotiable, no skew.
        assert_eq!(VersionSkew::classify(v(1), v(2), v(2), v(3)), None);
        assert_eq!(VersionSkew::classify(v(1), v(5), v(1), v(5)), None);
        // An inverted advertisement is malformed, not a version difference —
        // reporting it as "the daemon is older" would send the reader off to
        // restart a daemon that is not the problem.
        assert_eq!(VersionSkew::classify(v(5), v(1), v(2), v(2)), None);
        assert_eq!(VersionSkew::classify(v(2), v(2), v(5), v(1)), None);
    }

    /// The client's remedy is only reachable if the bounds survive the trip
    /// through the wire error — including from a released daemon, which emits
    /// this same `data` shape and is the one build we cannot change.
    #[test]
    fn the_bounds_survive_the_round_trip_through_the_wire_error() {
        let err = negotiate(
            ProtocolVersion(1),
            ProtocolVersion(1),
            ProtocolVersion(2),
            ProtocolVersion(2),
        )
        .unwrap_err();
        let rpc = err.to_rpc_error();
        let recovered = VersionMismatch::from_rpc_error(&rpc).expect("bounds are in `data`");
        assert_eq!(
            recovered,
            VersionMismatch {
                client_min: ProtocolVersion(2),
                client_max: ProtocolVersion(2),
                daemon_min: ProtocolVersion(1),
                daemon_max: ProtocolVersion(1),
            }
        );
        assert_eq!(recovered.skew(), Some(VersionSkew::DaemonIsOlder));

        // Anything else, or a truncated payload, yields nothing rather than a
        // fabricated range.
        assert_eq!(
            VersionMismatch::from_rpc_error(&RpcError::new(
                error_code::UNKNOWN_SESSION,
                "no such session"
            )),
            None
        );
        assert_eq!(
            VersionMismatch::from_rpc_error(&RpcError::new(
                error_code::UNSUPPORTED_PROTOCOL_VERSION,
                "no data member"
            )),
            None
        );
        assert_eq!(
            VersionMismatch::from_rpc_error(
                &RpcError::new(error_code::UNSUPPORTED_PROTOCOL_VERSION, "partial")
                    .with_data(serde_json::json!({"client_min": 2, "client_max": 2}))
            ),
            None
        );
    }

    /// The daemon's own sentence is what a client too old to decode `data`
    /// prints verbatim, so it has to carry the diagnosis itself.
    #[test]
    fn the_rejection_sentence_names_the_versions_and_who_is_behind() {
        let err = negotiate(
            ProtocolVersion(1),
            ProtocolVersion(1),
            ProtocolVersion(2),
            ProtocolVersion(2),
        )
        .unwrap_err();
        let text = err.to_string();
        assert!(text.contains("client speaks 2"), "{text}");
        assert!(text.contains("daemon speaks 1"), "{text}");
        assert!(
            text.contains("the running daemon is the older build"),
            "{text}"
        );
        assert!(text.contains("does not restart a daemon"), "{text}");

        // The mirror image reads as the mirror image, not as a copy.
        let flipped = negotiate(
            ProtocolVersion(3),
            ProtocolVersion(4),
            ProtocolVersion(1),
            ProtocolVersion(2),
        )
        .unwrap_err();
        let flipped = flipped.to_string();
        assert!(flipped.contains("client speaks 1–2"), "{flipped}");
        assert!(flipped.contains("daemon speaks 3–4"), "{flipped}");
        assert!(
            flipped.contains("this client is the older build"),
            "{flipped}"
        );
    }

    #[test]
    fn a_single_version_range_reads_as_one_number() {
        assert_eq!(format_range(ProtocolVersion(2), ProtocolVersion(2)), "2");
        assert_eq!(format_range(ProtocolVersion(1), ProtocolVersion(3)), "1–3");
    }

    // -----------------------------------------------------------------------
    // Build skew — REQ-565 BR-6 / AC-7
    // -----------------------------------------------------------------------

    #[test]
    fn matching_builds_produce_no_skew() {
        assert_eq!(build_skew("0.1.13", "0.1.13"), None);
    }

    #[test]
    fn differing_builds_carry_both_versions() {
        let skew = build_skew("0.1.13", "0.1.12").expect("a differing build must be reported");
        assert_eq!(skew.client_version, "0.1.13");
        assert_eq!(skew.daemon_version, "0.1.12");
    }

    /// Direction does not change the verdict: an older *client* against a newer
    /// daemon is still worth a line, and the remedy is the same either way.
    #[test]
    fn skew_is_reported_in_both_directions() {
        assert!(build_skew("0.1.12", "0.1.13").is_some());
        assert!(build_skew("0.1.13", "0.1.12").is_some());
    }

    /// The whole reason this exists. The exact pairing REQ-565's Description
    /// cites — v0.1.12 daemon, v0.1.13 CLI — speaks one protocol version, so
    /// `negotiate` **succeeds** and `VersionSkew` says nothing. If the build
    /// check ever gets folded into the protocol check, this fails.
    #[test]
    fn a_same_protocol_build_difference_is_invisible_to_the_protocol_check() {
        // Both halves speak the same protocol range: negotiation succeeds…
        let negotiated = negotiate(
            PROTOCOL_VERSION_MIN,
            PROTOCOL_VERSION_MAX,
            PROTOCOL_VERSION_MIN,
            PROTOCOL_VERSION_MAX,
        )
        .expect("identical ranges must negotiate");
        assert_eq!(negotiated, PROTOCOL_VERSION_MAX);

        // …and the protocol-skew classifier is silent…
        assert_eq!(
            VersionSkew::classify(
                PROTOCOL_VERSION_MIN,
                PROTOCOL_VERSION_MAX,
                PROTOCOL_VERSION_MIN,
                PROTOCOL_VERSION_MAX
            ),
            None,
            "overlapping ranges must not be a protocol skew"
        );

        // …but the stale daemon is still stale, and this is what says so.
        assert!(
            build_skew("0.1.13", "0.1.12").is_some(),
            "the build check must catch what the protocol check cannot"
        );
    }

    /// A version string carrying stray whitespace is the same version.
    #[test]
    fn whitespace_is_not_a_skew() {
        assert_eq!(build_skew("0.1.13", "0.1.13\n"), None);
        assert_eq!(build_skew(" 0.1.13 ", "0.1.13"), None);
    }

    /// `HandshakeResult` already carries what the check needs, so BR-6 costs no
    /// protocol change — if this stops compiling, the wire type moved and the
    /// CLI's skew check lost its input.
    #[test]
    fn the_handshake_result_carries_the_daemon_version_the_check_needs() {
        let result = HandshakeResult {
            protocol_version: PROTOCOL_VERSION_MAX,
            daemon_name: "teton-code".to_owned(),
            daemon_version: "0.1.12".to_owned(),
            capabilities: vec![],
        };
        assert!(build_skew("0.1.13", &result.daemon_version).is_some());
    }
}
