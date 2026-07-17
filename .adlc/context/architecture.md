# Teton Code — Architecture

## System Diagram

```
 ┌─────────────┐   ┌──────────────────┐
 │  CLI: teton │   │ VS Code extension │   (thin clients — render + input only)
 └──────┬──────┘   └────────┬─────────┘
        │  bespoke JSON-RPC over Unix socket (ADR-002)
        ▼                   ▼
 ┌──────────────────────────────────────────────┐
 │              tetond (Rust daemon)            │
 │  ┌─────────┐ ┌──────────┐ ┌──────────────┐   │
 │  │ Session │ │  Router  │ │ Cost ledger  │   │
 │  │  state  │ │ (phase → │ │ (CostRecord  │   │
 │  │ + ADLC  │ │  policy) │ │  per call)   │   │
 │  └─────────┘ └────┬─────┘ └──────────────┘   │
 │                   │                          │
 │        ┌──────────┴───────────┐              │
 │        ▼                      ▼              │
 │  ┌───────────┐    ┌────────────────────┐     │
 │  │  Local    │    │  Single egress     │     │
 │  │ inference │    │  point (privacy    │     │
 │  │(llama.cpp)│    │  boundary enforce) │     │
 │  └───────────┘    └─────────┬──────────┘     │
 └─────────────────────────────┼────────────────┘
                               ▼
                 ┌───────────────────────────┐
                 │ Provider adapters:        │
                 │ Anthropic / OpenAI-compat │
                 │ (DeepSeek, Kimi, Ollama…) │
                 └───────────────────────────┘
```

## Layers

- **Clients** — thin, stateless renderers. Hold no session state the daemon
  lacks (surface-parity rule, BR-4). CLI first; extension second.
- **Daemon (`tetond`)** — all differentiating logic: session/phase state,
  routing policy, cost accounting, privacy enforcement, provider adapters,
  local-model lifecycle (probe → download → benchmark → runtime pressure
  adaptation).
- **Egress** — every remote call flows through one choke point where privacy
  boundaries (BR-1) and cost recording (BR-2) are enforced. No adapter may
  bypass it.

## Key Patterns

- **Engine/surface separation** — protocol-first; any new editor client is a
  rendering exercise, not an agent reimplementation.
- **Workflow-aware routing** — phase (spec/architect/implement/review/io)
  determines model tier via a user-visible policy table; never per-prompt
  heuristics in structured mode (BR-5).
- **Adapter degradation** — providers with weak tool-calling get a reduced
  harness profile (smaller tool set, shorter loops, mandatory verification)
  rather than the full loop (BR-6).
- **Graceful absence** — the local tier disables itself below the hardware
  floor or under memory pressure rather than degrading the machine (BR-8/BR-9).

## ADRs

### ADR-001: Daemon and CLI in Rust (2026-07-17)

**Decision**: implement `tetond` and the `teton` CLI in Rust (single Cargo
workspace).

**Rationale**: first-class llama.cpp embedding (llama-cpp-2 bindings or vendored
build), single static binary per platform (zero-runtime install, critical for
AC-1's zero-config promise), memory safety for a long-running daemon holding
model weights, and ecosystem precedent for performance-critical devtools.

**Alternatives rejected**: Go (easier daemon ergonomics but cgo friction with
llama.cpp); TypeScript/Bun (fastest iteration, weakest fit for embedding
inference and shipping a lean daemon).

**Consequences**: slower initial velocity than Go/TS; extension (TS) will talk
to the daemon over the protocol rather than sharing code — which the
engine/surface split requires anyway.

### ADR-002: Bespoke JSON-RPC protocol, ACP-informed, over Unix domain socket (2026-07-17)

**Decision**: the client↔daemon protocol is a bespoke JSON-RPC 2.0 protocol
over a Unix domain socket, with an event-subscription model (clients subscribe
to session/event streams; the daemon broadcasts). Message vocabulary borrows
ACP's terms wherever the concepts overlap (session, prompt turn,
permission-request, diff semantics) so a future ACP compatibility shim — a thin
stdio↔socket adapter process — stays cheap. Protocol types live in the
`teton-protocol` crate, shared by daemon and CLI and mirrored in TypeScript for
the extension.

**Rationale**: ACP's structural model is "editor spawns agent as owned
subprocess over stdio," which inverts our architecture — a persistent shared
daemon that multiple clients attach to and detach from, with sessions that
outlive any client (BR-4). Our differentiating surfaces (`route_decided`,
`privacy_block`, `cost_recorded`, model download/benchmark progress) have no
ACP vocabulary. Bespoke gives an exact fit; borrowing ACP vocabulary preserves
the ecosystem option (Zed, Neovim, Emacs speak ACP) without contorting the
daemon around a subprocess model it doesn't have.

**Alternatives rejected**: stock ACP (subprocess model mismatch,
single-client assumption); raw stdio per-client agents (no shared daemon, no
shared local model); gRPC (heavier toolchain, worse fit for extension-side
TypeScript, no ACP affinity).

**Consequences**: all editor integrations are first-party work until the ACP
shim exists; protocol versioning, socket auth (filesystem permissions +
peer-credential check), and backpressure are ours to design — to be specified
in the protocol child REQ at decomposition time.

### ADR-003: MCP server consumption is first-class (2026-07-17)

**Decision**: Teton Code consumes MCP (Model Context Protocol) servers as tool
providers — users can register MCP servers and their tools become available to
agent sessions, subject to the same permission model and privacy egress rules
as built-in tools.

**Rationale**: MCP is the de-facto standard for agent tooling; users arrive
with existing MCP servers and expect them to work. Note the role split: MCP is
agent↔tools; ADR-002's protocol is client↔daemon. They do not compete.

**Consequences**: tool calls to remote MCP servers are egress and MUST flow
through the privacy boundary choke point (BR-1) — content under a `local-only`
boundary never reaches a remote MCP server. Tool-result content entering
context is data, not instructions (prompt-injection posture to be detailed in
the harness child REQ).
