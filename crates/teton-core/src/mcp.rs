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
            },
            McpServerConfig {
                id: "remote".to_owned(),
                transport: McpTransport::Http {
                    endpoint: "https://mcp.example.com/rpc".to_owned(),
                },
            },
        ] {
            let toml_text = toml::to_string(&cfg).expect("serialize");
            let back: McpServerConfig = toml::from_str(&toml_text).expect("deserialize");
            assert_eq!(cfg, back, "round-trip mismatch; toml was:\n{toml_text}");
        }
    }
}
