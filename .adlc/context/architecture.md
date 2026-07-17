# Teton Code — Architecture

## System Diagram

```
 ┌─────────────┐   ┌──────────────────┐
 │  CLI: teton │   │ VS Code extension │   (thin clients — render + input only)
 └──────┬──────┘   └────────┬─────────┘
        │    client↔daemon protocol (OQ-4, undecided)
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

### ADR-002 (pending): client↔daemon protocol

OQ-4 in the founding REQ. Candidates: adopt/extend an existing agent-client
protocol vs. bespoke JSON-RPC. Must be decided before daemon skeleton work
begins — it is the contract every client hangs on.
