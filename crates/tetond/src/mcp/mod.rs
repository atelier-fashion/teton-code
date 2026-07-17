//! MCP (Model Context Protocol) consumption — user-registered servers as tool
//! providers (ADR-003).
//!
//! Teton Code consumes MCP servers: a user registers a server (a local `stdio`
//! subprocess or a remote streamable-HTTP endpoint) and its tools become
//! available to agent sessions under the same permission model and privacy
//! egress rules as the built-in tools. MCP is the agent↔tools protocol; it does
//! not compete with ADR-002's client↔daemon protocol.
//!
//! ## The load-bearing asymmetry (BR-1)
//!
//! - A **remote** MCP server is egress: every `tools/call` to it flows through the
//!   single [`crate::egress`] choke point ([`client::HttpConnection`]), so content
//!   under a `local-only` boundary can never reach it — the call is refused before
//!   a byte leaves, exactly as a remote model call would be.
//! - A **local** stdio MCP server is *not* egress — it may read `local-only` files
//!   because nothing leaves the machine. But its **results** entering context must
//!   still carry the provenance of any boundary path the call referenced, so that
//!   a *later* remote turn cannot launder that content off the machine through the
//!   model's context. The bridge tags results accordingly
//!   ([`crate::harness::tools::mcp`]).
//!
//! ## Module map
//! - [`client`] — the MCP JSON-RPC protocol client (`initialize`, `tools/list`,
//!   `tools/call`) over both the stdio and streamable-HTTP transports.
//! - [`registry`] — config-declared servers, connect-on-demand lifecycle, health,
//!   crash-degrades-only-its-own-tools, and the `mcp__<server>__<tool>` namespace.
//!
//! ## MVP scope
//!
//! Tools only. MCP *resources* and *prompts* are deferred (to be recorded in the
//! spec's Out of Scope at wrapup).

pub mod client;
pub mod registry;

pub use client::{
    call_provenance, namespaced_tool_name, parse_namespaced_tool_name, EgressGate, HttpConnection,
    McpClient, McpConnection, McpError, McpServerInfo, McpTool, McpToolResult, StdioConnection,
};
pub use registry::{
    DefaultConnector, DiscoveredTool, McpConnector, McpRegistry, McpServerConfig, McpTransport,
    ServerHealth,
};
