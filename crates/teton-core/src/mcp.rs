//! MCP (Model Context Protocol) server declarations (ADR-003).
//!
//! A user registers MCP servers as tool providers — a local `stdio` subprocess
//! or a remote streamable-HTTP endpoint — and their tools become available to
//! agent sessions under the same permission model and privacy egress rules as the
//! built-in tools.
//!
//! These types live in `teton-core` (the pure-data layer) because they are part
//! of the main [`crate::config::Config`] document (the `[[mcp_server]]` table,
//! AC-9): a server registers in one place alongside providers, routing, and
//! privacy boundaries. The daemon (`tetond`) turns each declaration into a live
//! connection and enforces the lifecycle, permission gate, and BR-1 egress
//! asymmetry around it — this crate holds no I/O.

use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

/// How a configured MCP server is reached.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum McpTransport {
    /// A local subprocess speaking JSON-RPC over its stdio. Not egress.
    Stdio {
        /// The executable to spawn.
        command: String,
        /// Arguments passed to it.
        #[serde(default)]
        args: Vec<String>,
        /// Extra environment variables this server declares. The child otherwise
        /// gets only a minimal base environment (PATH/HOME/locale essentials) —
        /// the daemon's provider keys are **never** inherited (REQ-544 MED-2,
        /// BR-7). Declared vars are layered on top of that base.
        #[serde(default)]
        env: BTreeMap<String, String>,
    },
    /// A remote streamable-HTTP endpoint. Every call flows through egress (BR-1).
    Http {
        /// Absolute endpoint URL.
        endpoint: String,
    },
}

impl McpTransport {
    /// Whether this transport reaches off the machine (and therefore flows through
    /// the egress choke point).
    #[must_use]
    pub fn is_remote(&self) -> bool {
        matches!(self, McpTransport::Http { .. })
    }
}

/// A user-declared MCP server (System Model: an MCP tool provider, ADR-003).
///
/// Part of the main config document: it deserializes from a `[[mcp_server]]`
/// table (or a JSON object with the same shape, used by the daemon's test
/// override seam). [`crate::config::Config::validate`] enforces unique ids and
/// the transport-specific required fields.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct McpServerConfig {
    /// Stable, unique server id — the `<server>` in `mcp__<server>__<tool>`.
    pub id: String,
    /// How to reach the server.
    pub transport: McpTransport,
    /// Whether this server is declared **trusted** (REQ-544 security).
    ///
    /// Defaults to `false` — a conservative, privacy-first default. A local
    /// (`stdio`) server that is NOT trusted has its tool results tagged with
    /// `Unknown` egress provenance (fail-closed, exactly like `shell`),
    /// because a black-box subprocess can read a `local-only` file via an opaque,
    /// non-path argument the daemon cannot see — so its result cannot be proven
    /// public and must not be laundered to a remote provider on a later turn.
    /// Setting `trusted = true` keeps the precise, argument-derived provenance
    /// (`call_provenance`) for that server's results. A remote (`http`) server
    /// already routes every call through the egress choke point, so this flag does
    /// not change its behavior (see [`Self::opaque_provenance`]).
    #[serde(default)]
    pub trusted: bool,
}

impl McpServerConfig {
    /// Whether this server's tool results must carry **`Unknown`** (fail-closed)
    /// provenance rather than argument-derived provenance (REQ-544 security).
    ///
    /// True only for a local (`stdio`) server that is not declared `trusted`: its
    /// touched files cannot be derived from opaque arguments, so — like `shell` —
    /// its result taints the session to the local tier. A `trusted` server, or any
    /// remote (`http`) server (already egress-gated), keeps precise provenance.
    #[must_use]
    pub fn opaque_provenance(&self) -> bool {
        !self.trusted && !self.transport.is_remote()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stdio_is_not_remote_http_is() {
        let stdio = McpTransport::Stdio {
            command: "mcp-server-filesystem".to_owned(),
            args: vec![],
            env: BTreeMap::new(),
        };
        assert!(!stdio.is_remote());
        let http = McpTransport::Http {
            endpoint: "https://mcp.example.com/rpc".to_owned(),
        };
        assert!(http.is_remote());
    }

    #[test]
    fn server_config_round_trips_through_toml() {
        // The `[[mcp_server]]` shape: a scalar id plus a nested transport table
        // with a `kind` tag — proving the enum lifts cleanly into the config
        // document (AC-9).
        for cfg in [
            McpServerConfig {
                id: "fs".to_owned(),
                transport: McpTransport::Stdio {
                    command: "mcp-server-filesystem".to_owned(),
                    args: vec!["--root".to_owned(), ".".to_owned()],
                    env: BTreeMap::from([("MCP_LOG".to_owned(), "info".to_owned())]),
                },
                trusted: true,
            },
            McpServerConfig {
                id: "remote".to_owned(),
                transport: McpTransport::Http {
                    endpoint: "https://mcp.example.com/rpc".to_owned(),
                },
                trusted: false,
            },
        ] {
            let toml_text = toml::to_string(&cfg).expect("serialize");
            let back: McpServerConfig = toml::from_str(&toml_text).expect("deserialize");
            assert_eq!(cfg, back, "round-trip mismatch; toml was:\n{toml_text}");
        }
    }

    #[test]
    fn trusted_defaults_to_false_when_omitted() {
        // REQ-544 security: the privacy-first default. A `[[mcp_server]]` table that
        // omits `trusted` deserializes as untrusted.
        let toml_text = "\
id = \"fs\"
[transport]
kind = \"stdio\"
command = \"mcp-server-filesystem\"
";
        let cfg: McpServerConfig = toml::from_str(toml_text).expect("deserialize");
        assert!(
            !cfg.trusted,
            "trusted must default to false (privacy-first)"
        );
    }

    #[test]
    fn only_an_untrusted_stdio_server_has_opaque_provenance() {
        // REQ-544 security: fail-closed provenance applies to a local, untrusted
        // stdio server; a trusted stdio server or any http server keeps precise
        // arg-derived provenance.
        let stdio = |trusted| McpServerConfig {
            id: "fs".to_owned(),
            transport: McpTransport::Stdio {
                command: "x".to_owned(),
                args: vec![],
                env: BTreeMap::new(),
            },
            trusted,
        };
        assert!(
            stdio(false).opaque_provenance(),
            "untrusted stdio is fail-closed"
        );
        assert!(
            !stdio(true).opaque_provenance(),
            "trusted stdio keeps provenance"
        );

        let http = |trusted| McpServerConfig {
            id: "remote".to_owned(),
            transport: McpTransport::Http {
                endpoint: "https://mcp.example.com/rpc".to_owned(),
            },
            trusted,
        };
        assert!(
            !http(false).opaque_provenance(),
            "http is egress-gated, not opaque"
        );
        assert!(!http(true).opaque_provenance());
    }
}
