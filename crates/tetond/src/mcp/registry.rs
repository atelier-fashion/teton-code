//! Config-declared MCP servers: connect-on-demand lifecycle, health, and the
//! `mcp__<server>__<tool>` namespace.
//!
//! A user registers servers in config; the registry connects to each **on
//! demand** (the first time its tools are needed), caches the live connection,
//! and tracks its [`ServerHealth`]. The load-bearing lifecycle property is
//! **fault isolation**: a server that fails to start, times out, or crashes
//! mid-call is marked [`ServerHealth::Degraded`] and its tools drop out of the
//! set — but every *other* server, and the session itself, keep going. A degraded
//! server is retried (restarted) the next time one of its tools is called, so a
//! transient crash self-heals without operator action.
//!
//! The [`McpConnector`] seam abstracts "how a config becomes a live connection",
//! so the registry is unit-testable with mock connections — no real subprocess or
//! socket — while production wires [`DefaultConnector`] (stdio subprocess or
//! egress-gated HTTP).

use std::collections::{BTreeMap, HashMap};
use std::sync::Arc;

use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use tokio::sync::Mutex;

use teton_protocol::SessionId;

use super::client::{
    namespaced_tool_name, parse_namespaced_tool_name, EgressGate, HttpConnection, McpClient,
    McpConnection, McpError, McpTool, McpToolResult, StdioConnection,
};

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
/// These types are serde-ready so they can be lifted into the top-level config
/// document; wiring them into `teton_core::config::Config` is a follow-up outside
/// this task's crate scope.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct McpServerConfig {
    /// Stable, unique server id — the `<server>` in `mcp__<server>__<tool>`.
    pub id: String,
    /// How to reach the server.
    pub transport: McpTransport,
}

/// The health of a registered server.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ServerHealth {
    /// Not yet connected (no tool of this server has been needed).
    Unconnected,
    /// Connected and serving.
    Healthy,
    /// Failed to start / crashed / timed out. The reason is content-free (BR-1).
    /// Its tools drop out until a retry reconnects it.
    Degraded(String),
}

/// Turns an [`McpServerConfig`] into a live [`McpConnection`].
///
/// The seam that keeps the registry testable: production uses
/// [`DefaultConnector`]; tests inject a mock that returns scripted connections.
#[async_trait]
pub trait McpConnector: Send + Sync {
    /// Establish a fresh connection to `config`'s server.
    async fn connect(&self, config: &McpServerConfig) -> Result<Arc<dyn McpConnection>, McpError>;
}

/// The production connector: spawns a stdio subprocess, or builds an
/// egress-gated HTTP connection.
pub struct DefaultConnector {
    egress: Arc<dyn EgressGate>,
    session_id: Option<SessionId>,
}

impl DefaultConnector {
    /// A connector whose HTTP servers send through `egress`, scoped to
    /// `session_id` for privacy-block attribution.
    #[must_use]
    pub fn new(egress: Arc<dyn EgressGate>, session_id: Option<SessionId>) -> Self {
        Self { egress, session_id }
    }
}

#[async_trait]
impl McpConnector for DefaultConnector {
    async fn connect(&self, config: &McpServerConfig) -> Result<Arc<dyn McpConnection>, McpError> {
        match &config.transport {
            McpTransport::Stdio { command, args, env } => {
                let conn = StdioConnection::spawn(&config.id, command, args, env)?;
                Ok(Arc::new(conn))
            }
            McpTransport::Http { endpoint } => {
                let mut conn = HttpConnection::new(&config.id, endpoint, Arc::clone(&self.egress));
                if let Some(session) = &self.session_id {
                    conn = conn.with_session(session.clone());
                }
                Ok(Arc::new(conn))
            }
        }
    }
}

/// A tool discovered on a server, ready to be namespaced and bridged.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DiscoveredTool {
    /// The server that advertises the tool.
    pub server_id: String,
    /// The tool itself (server-local name, description, schema).
    pub tool: McpTool,
}

impl DiscoveredTool {
    /// The `mcp__<server>__<tool>` name this tool is exposed under.
    #[must_use]
    pub fn namespaced_name(&self) -> String {
        namespaced_tool_name(&self.server_id, &self.tool.name)
    }
}

/// Per-server lifecycle state.
struct ServerState {
    config: McpServerConfig,
    client: Option<Arc<McpClient>>,
    health: ServerHealth,
}

/// The registry of configured MCP servers.
pub struct McpRegistry {
    connector: Arc<dyn McpConnector>,
    servers: Mutex<HashMap<String, ServerState>>,
    order: Vec<String>,
}

impl McpRegistry {
    /// Build a registry over `configs`, connecting via `connector`.
    #[must_use]
    pub fn new(connector: Arc<dyn McpConnector>, configs: Vec<McpServerConfig>) -> Self {
        let mut servers = HashMap::with_capacity(configs.len());
        let mut order = Vec::with_capacity(configs.len());
        for config in configs {
            order.push(config.id.clone());
            servers.insert(
                config.id.clone(),
                ServerState {
                    config,
                    client: None,
                    health: ServerHealth::Unconnected,
                },
            );
        }
        Self {
            connector,
            servers: Mutex::new(servers),
            order,
        }
    }

    /// Build a registry whose servers connect through the production
    /// [`DefaultConnector`] over `egress`.
    #[must_use]
    pub fn with_egress(
        egress: Arc<dyn EgressGate>,
        session_id: Option<SessionId>,
        configs: Vec<McpServerConfig>,
    ) -> Self {
        Self::new(Arc::new(DefaultConnector::new(egress, session_id)), configs)
    }

    /// The configured server ids, in declaration order.
    #[must_use]
    pub fn server_ids(&self) -> &[String] {
        &self.order
    }

    /// The current health of `server_id`, or `None` if it is not configured.
    pub async fn health(&self, server_id: &str) -> Option<ServerHealth> {
        self.servers
            .lock()
            .await
            .get(server_id)
            .map(|s| s.health.clone())
    }

    /// Get the live client for `server_id`, connecting (or reconnecting after a
    /// crash) on demand. Marks the server [`ServerHealth::Degraded`] and returns
    /// the error on failure.
    async fn get_or_connect(&self, server_id: &str) -> Result<Arc<McpClient>, McpError> {
        let mut servers = self.servers.lock().await;
        let state = servers
            .get_mut(server_id)
            .ok_or_else(|| McpError::Startup(format!("{server_id}: not configured")))?;

        if let Some(client) = &state.client {
            return Ok(Arc::clone(client));
        }

        // First connect, or a restart after a crash dropped the cached client.
        let config = state.config.clone();
        let connect = self.connector.connect(&config).await;
        let conn = match connect {
            Ok(conn) => conn,
            Err(e) => {
                state.health = ServerHealth::Degraded(e.to_string());
                return Err(e);
            }
        };
        let client = Arc::new(McpClient::new(server_id, conn));
        if let Err(e) = client.initialize().await {
            state.health = ServerHealth::Degraded(e.to_string());
            return Err(e);
        }
        state.client = Some(Arc::clone(&client));
        state.health = ServerHealth::Healthy;
        Ok(client)
    }

    /// Drop the cached connection for `server_id` and mark it degraded, so the
    /// next call reconnects (restart-on-demand).
    async fn mark_degraded(&self, server_id: &str, reason: String) {
        if let Some(state) = self.servers.lock().await.get_mut(server_id) {
            state.client = None;
            state.health = ServerHealth::Degraded(reason);
        }
    }

    /// Discover the tools of every configured server, namespaced.
    ///
    /// A server that fails to connect or list is skipped and marked degraded — its
    /// tools drop out of the set, but every other server's tools are still
    /// returned (fault isolation). Never fails wholesale.
    pub async fn list_tools(&self) -> Vec<DiscoveredTool> {
        let mut discovered = Vec::new();
        for server_id in &self.order {
            let client = match self.get_or_connect(server_id).await {
                Ok(client) => client,
                Err(_) => continue, // already marked degraded
            };
            match client.list_tools().await {
                Ok(tools) => {
                    for tool in tools {
                        discovered.push(DiscoveredTool {
                            server_id: server_id.clone(),
                            tool,
                        });
                    }
                }
                Err(e) => {
                    if e.is_connection_lost() {
                        self.mark_degraded(server_id, e.to_string()).await;
                    }
                }
            }
        }
        discovered
    }

    /// Call a namespaced MCP tool.
    ///
    /// A connection-lost error degrades that server (and schedules a restart on
    /// next demand) but is returned to the caller as an ordinary error — the
    /// bridge folds it into a tool result the model sees, and the session
    /// continues. A per-call server error is returned without degrading the
    /// connection.
    ///
    /// # Errors
    /// [`McpError::NotNamespaced`] for a non-MCP name, or the underlying call
    /// error (transport, server, or [`McpError::PrivacyBlocked`]).
    pub async fn call_tool(
        &self,
        namespaced_name: &str,
        arguments: serde_json::Value,
    ) -> Result<McpToolResult, McpError> {
        let (server_id, tool) = parse_namespaced_tool_name(namespaced_name)
            .ok_or_else(|| McpError::NotNamespaced(namespaced_name.to_owned()))?;
        let (server_id, tool) = (server_id.to_owned(), tool.to_owned());

        let client = self.get_or_connect(&server_id).await?;
        match client.call_tool(&tool, arguments).await {
            Ok(result) => Ok(result),
            Err(e) => {
                if e.is_connection_lost() {
                    self.mark_degraded(&server_id, e.to_string()).await;
                }
                Err(e)
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::{json, Value};
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Mutex as StdMutex;

    use crate::egress::Provenance;

    /// A mock connection whose behavior is scripted per server: it can be healthy,
    /// or "crash" (return a connection-lost error) starting from a given call.
    struct MockConnection {
        server: String,
        /// Number of `tools/call`s served before the connection "crashes".
        calls_before_crash: Option<usize>,
        calls: AtomicUsize,
    }

    #[async_trait]
    impl McpConnection for MockConnection {
        async fn call(
            &self,
            method: &str,
            _params: Value,
            _provenance: &Provenance,
        ) -> Result<Value, McpError> {
            match method {
                "initialize" => {
                    Ok(json!({ "serverInfo": { "name": self.server, "version": "1" } }))
                }
                "tools/list" => Ok(json!({
                    "tools": [
                        { "name": "do_thing", "description": "does a thing", "inputSchema": {"type":"object"} }
                    ]
                })),
                "tools/call" => {
                    let n = self.calls.fetch_add(1, Ordering::SeqCst);
                    if let Some(limit) = self.calls_before_crash {
                        if n >= limit {
                            return Err(McpError::Closed(self.server.clone()));
                        }
                    }
                    Ok(json!({ "content": [ { "type": "text", "text": "ok" } ], "isError": false }))
                }
                _ => Ok(Value::Null),
            }
        }

        async fn notify(&self, _method: &str, _params: Value) -> Result<(), McpError> {
            Ok(())
        }
    }

    /// A connector that hands out mock connections and records how many times each
    /// server was connected (so a restart is observable).
    #[derive(Default)]
    struct MockConnector {
        /// server id -> (fail_to_connect, calls_before_crash)
        script: StdMutex<HashMap<String, (bool, Option<usize>)>>,
        connect_counts: StdMutex<HashMap<String, usize>>,
    }

    impl MockConnector {
        fn healthy(&self, server: &str) {
            self.script
                .lock()
                .unwrap()
                .insert(server.to_owned(), (false, None));
        }
        fn fails_to_connect(&self, server: &str) {
            self.script
                .lock()
                .unwrap()
                .insert(server.to_owned(), (true, None));
        }
        fn crashes_after(&self, server: &str, calls: usize) {
            self.script
                .lock()
                .unwrap()
                .insert(server.to_owned(), (false, Some(calls)));
        }
        fn connect_count(&self, server: &str) -> usize {
            self.connect_counts
                .lock()
                .unwrap()
                .get(server)
                .copied()
                .unwrap_or(0)
        }
    }

    #[async_trait]
    impl McpConnector for MockConnector {
        async fn connect(
            &self,
            config: &McpServerConfig,
        ) -> Result<Arc<dyn McpConnection>, McpError> {
            *self
                .connect_counts
                .lock()
                .unwrap()
                .entry(config.id.clone())
                .or_insert(0) += 1;
            let (fail, calls_before_crash) = self
                .script
                .lock()
                .unwrap()
                .get(&config.id)
                .copied()
                .unwrap_or((false, None));
            if fail {
                return Err(McpError::Startup(format!("{}: mock refused", config.id)));
            }
            Ok(Arc::new(MockConnection {
                server: config.id.clone(),
                calls_before_crash,
                calls: AtomicUsize::new(0),
            }))
        }
    }

    fn stdio_cfg(id: &str) -> McpServerConfig {
        McpServerConfig {
            id: id.to_owned(),
            transport: McpTransport::Stdio {
                command: "unused".to_owned(),
                args: vec![],
                env: BTreeMap::new(),
            },
        }
    }

    #[tokio::test]
    async fn tools_from_every_server_appear_namespaced() {
        let connector = Arc::new(MockConnector::default());
        connector.healthy("fs");
        connector.healthy("db");
        let registry = McpRegistry::new(connector, vec![stdio_cfg("fs"), stdio_cfg("db")]);

        let tools = registry.list_tools().await;
        let names: Vec<String> = tools.iter().map(DiscoveredTool::namespaced_name).collect();
        assert!(names.contains(&"mcp__fs__do_thing".to_owned()));
        assert!(names.contains(&"mcp__db__do_thing".to_owned()));
        assert_eq!(registry.health("fs").await, Some(ServerHealth::Healthy));
    }

    #[tokio::test]
    async fn one_server_failing_to_connect_degrades_only_itself() {
        let connector = Arc::new(MockConnector::default());
        connector.fails_to_connect("broken");
        connector.healthy("ok");
        let registry = McpRegistry::new(connector, vec![stdio_cfg("broken"), stdio_cfg("ok")]);

        let tools = registry.list_tools().await;
        let names: Vec<String> = tools.iter().map(DiscoveredTool::namespaced_name).collect();
        // The healthy server's tool is present; the broken server's is not.
        assert!(names.contains(&"mcp__ok__do_thing".to_owned()));
        assert!(!names.iter().any(|n| n.starts_with("mcp__broken__")));

        assert!(matches!(
            registry.health("broken").await,
            Some(ServerHealth::Degraded(_))
        ));
        assert_eq!(registry.health("ok").await, Some(ServerHealth::Healthy));
    }

    #[tokio::test]
    async fn a_crash_mid_call_degrades_the_server_but_the_call_returns_an_error() {
        let connector = Arc::new(MockConnector::default());
        connector.crashes_after("fs", 1); // first call ok, second crashes
        let registry = McpRegistry::new(connector, vec![stdio_cfg("fs")]);

        // First call succeeds.
        let ok = registry.call_tool("mcp__fs__do_thing", json!({})).await;
        assert!(ok.is_ok());
        assert_eq!(registry.health("fs").await, Some(ServerHealth::Healthy));

        // Second call crashes: the error surfaces (session continues) and the
        // server is degraded.
        let crashed = registry.call_tool("mcp__fs__do_thing", json!({})).await;
        assert!(crashed.is_err());
        assert!(matches!(
            registry.health("fs").await,
            Some(ServerHealth::Degraded(_))
        ));
    }

    #[tokio::test]
    async fn a_degraded_server_reconnects_on_the_next_call() {
        let connector = Arc::new(MockConnector::default());
        connector.crashes_after("fs", 1);
        let registry = McpRegistry::new(
            Arc::clone(&connector) as Arc<dyn McpConnector>,
            vec![stdio_cfg("fs")],
        );

        let _ = registry.call_tool("mcp__fs__do_thing", json!({})).await; // ok
        let _ = registry.call_tool("mcp__fs__do_thing", json!({})).await; // crash -> degraded
        assert_eq!(connector.connect_count("fs"), 1);

        // The next call reconnects (restart-on-demand): a fresh connection whose
        // crash counter resets, so this call succeeds again.
        let restarted = registry.call_tool("mcp__fs__do_thing", json!({})).await;
        assert!(restarted.is_ok(), "server should have restarted");
        assert_eq!(
            connector.connect_count("fs"),
            2,
            "a second connect happened"
        );
        assert_eq!(registry.health("fs").await, Some(ServerHealth::Healthy));
    }

    #[tokio::test]
    async fn calling_a_non_namespaced_tool_is_an_error() {
        let connector = Arc::new(MockConnector::default());
        let registry = McpRegistry::new(connector, vec![]);
        let err = registry.call_tool("read", json!({})).await.unwrap_err();
        assert!(matches!(err, McpError::NotNamespaced(_)));
    }

    #[test]
    fn server_config_round_trips_through_serde() {
        // serde-readiness (so these lift into the config document later); JSON
        // stands in for the eventual TOML — `toml` is not a tetond dependency.
        for cfg in [
            McpServerConfig {
                id: "fs".to_owned(),
                transport: McpTransport::Stdio {
                    command: "mcp-server-filesystem".to_owned(),
                    args: vec!["--root".to_owned(), ".".to_owned()],
                    env: BTreeMap::new(),
                },
            },
            McpServerConfig {
                id: "remote".to_owned(),
                transport: McpTransport::Http {
                    endpoint: "https://mcp.example.com/rpc".to_owned(),
                },
            },
        ] {
            let json = serde_json::to_string(&cfg).expect("serialize");
            let back: McpServerConfig = serde_json::from_str(&json).expect("deserialize");
            assert_eq!(cfg, back);
        }
    }
}
