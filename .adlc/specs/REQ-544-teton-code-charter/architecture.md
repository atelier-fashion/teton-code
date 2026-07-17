# REQ-544 — Architecture: Teton Code MVP

Parent decisions ADR-001 (Rust), ADR-002 (bespoke JSON-RPC over UDS,
ACP-informed), ADR-003 (MCP consumption) live in
`.adlc/context/architecture.md` and are not restated here. This document covers
the REQ-level design: crate responsibilities, the build decomposition, and the
decisions made at breakdown time.

## Approach

Build the MVP as a Cargo workspace with a strict dependency direction:

```
teton-protocol ──┐
                 ├──> tetond (daemon binary)
teton-core ──────┤        ▲ UDS/JSON-RPC
  ▲              │        │
  │              └──> teton (CLI binary)
teton-providers, teton-inference (depend on core, never on binaries)
```

- `teton-core` — pure domain logic: entities (Session, RoutingPolicy,
  ModelProvider, PrivacyBoundary, CostRecord), config schema, policy
  evaluation. **No I/O** — everything table-driven-testable.
- `teton-protocol` — client↔daemon message types + event envelopes (ADR-002),
  version handshake. Serde types only; shared by both binaries and mirrored in
  TS later.
- `teton-providers` — provider trait + Anthropic and OpenAI-compatible
  adapters, capability profiles, fallback logic.
- `teton-inference` — llama.cpp embedding, hardware probe, model download,
  micro-benchmark, memory-pressure watcher.
- `tetond` — composition root: UDS server, session registry, event broadcast,
  agent harness (tool-use loop + permission model), the **single egress
  module** (privacy enforcement + cost recording live here and nowhere else),
  router, MCP client.
- `teton` — thin CLI client rendering protocol streams.

## Key decisions (REQ-level)

### D-1: Single-REQ task tree, tiered delivery, stacked PRs allowed

The charter's 9 ACs decompose into 14 tasks in 5 dependency tiers under this
one REQ rather than child REQs. Rationale: tasks + dependency tiers are the
toolkit's parallelism mechanism (worktrees per task); the repo is pre-alpha
with no production to protect, so a large integration branch is acceptable.
Mitigation for PR size: implementation MAY land tiers as stacked PRs onto an
integration branch (`feat/REQ-544-mvp`), merging to `main` per tier — decided
at implementation time, not mandated here.

### D-2: Egress choke point is a module boundary, not a convention

BR-1 (privacy) and BR-2 (cost attribution) are enforced in one `egress` module
in `tetond`; `teton-providers` adapters are **constructed around a transport
handed to them by egress** — an adapter physically cannot make an HTTP call
that bypasses boundary checks and cost recording. The MCP client (ADR-003)
uses the same transport. Egress-capture tests (AC-5, AC-9) mock this
transport.

### D-3: Harness lands local-first

The agent harness (tool loop, permissions, verification) is built first
against the local model only (offline AC-1 path); remote-model wiring arrives
with the router task. This keeps the harness task single-session-sized and
gives an offline demo at the end of tier 3.

### D-4: Phase enum is the routing contract

`Phase = {spec, architect, implement, review, io, freeform}` lives in
`teton-core` and is the shared vocabulary of the router (BR-5), cost ledger
(BR-2), and structured mode (AC-3). Structured mode is a state machine over
this enum with artifact gates; freeform is the degenerate single-phase case —
one code path, no special-casing (mirrors BR-3).

## Task breakdown — dependency tiers

| Tier | Tasks |
|---|---|
| 0 | TASK-001 workspace+CI |
| 1 | TASK-002 protocol crate · TASK-003 core domain types |
| 2 | TASK-004 daemon skeleton · TASK-005 provider adapters · TASK-006 local inference |
| 3 | TASK-007 privacy egress · TASK-008 cost ledger · TASK-009 agent harness |
| 4 | TASK-010 router · TASK-011 MCP client |
| 5 | TASK-012 CLI · TASK-013 structured mode · TASK-014 acceptance suite |

Tasks within a tier are independent (parallel by default); every task ≤3
dependencies; DAG verified acyclic.

## AC coverage map

| AC | Covered by |
|---|---|
| AC-1 zero-config first session | 006, 009, 012 |
| AC-2 two remote providers | 005, 012 |
| AC-3 structured phase routing | 010, 013 |
| AC-4 cost meter | 008, 012 |
| AC-5 privacy boundary | 007 (+014) |
| AC-6 multi-client daemon | 004 |
| AC-7 provider degradation | 005 (+010) |
| AC-8 hardware probe/benchmark | 006 |
| AC-9 MCP + egress | 011 (+014) |

TASK-014 exercises all nine end-to-end.

## Proposed additions to `.adlc/context/architecture.md`

None — ADR-001/002/003 already recorded there this session; D-1..D-4 are
REQ-scoped and stay here. If D-2 (egress-owned transport) proves out, promote
it to a context-level pattern at `/wrapup`.
