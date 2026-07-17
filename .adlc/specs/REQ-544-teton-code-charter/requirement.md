---
id: REQ-544
title: "Teton Code — hybrid local/remote AI coding agent with workflow-aware model routing"
status: approved
deployable: true
created: 2026-07-17
updated: 2026-07-17
component: "agent/engine"
domain: "devtools"
stack: ["daemon", "cli", "llama.cpp", "vscode-extension", "llm-providers"]
concerns: ["cost", "privacy", "routing", "extensibility"]
tags: ["local-model", "byom", "adlc-routing", "cost-meter", "privacy-boundary", "claude-code-like"]
---

## Description

**Teton Code** — a standalone AI coding agent in the shape of Claude Code — an
agentic harness that reads, edits, and verifies code through tool-use loops — with
two differentiators no existing tool (Cline, Continue, Aider, OpenCode, Cursor)
combines:

1. **Ships with a slim local model** (zero-config first-run download, not bundled
   in the installer) that runs as a persistent daemon and handles the always-on
   cheap tier: routing/intent classification, file and diff summarization, grep
   triage, commit messages, secret redaction, and offline fallback.
2. **Workflow-aware model routing** built on the ADLC phase structure. Rather than
   guessing task difficulty from prompt text (the failure mode of generic
   routers), the harness routes by lifecycle phase: spec/architecture → frontier
   model, implementation → cheap/mid model executing from task-file artifacts,
   review → frontier, mechanical I/O → local. The ADLC artifacts (specs, task
   files) are what carry intelligence forward and make cheap models viable for
   the token-heavy implementation phase.

**Why**: the target user is a developer who wants to cut API spend 60–80% and
wants explicit control over which models do what — expressed as a legible policy
("architecture goes to Opus, implementation goes to DeepSeek, these folders never
leave my machine"), not regex rules. Privacy boundaries and a live cost meter make
both promises visible and auditable.

**Architecture stance (from spitball session)**: engine/surface separation. All
differentiating logic (router, ADLC state, privacy enforcement, cost accounting,
provider adapters) lives in a local daemon. Clients are thin: CLI first (fastest
path to dogfooding the router — the risky part), VS Code extension second (the
marketing surface where privacy badges, cost attribution, and the ADLC sidebar
land visually). The local model forces the daemon architecture anyway — weights
can't live in an extension host, and multiple editor windows must share one
inference process.

## System Model

### Entities

| Entity | Field | Type | Constraints |
|--------|-------|------|-------------|
| ModelProvider | id | string | required, unique |
| ModelProvider | kind | enum(local, openai-compatible, anthropic, custom) | required |
| ModelProvider | endpoint | string (URL) | required for remote kinds |
| ModelProvider | auth_ref | string | reference to OS keychain entry; raw keys never stored in config files |
| ModelProvider | capabilities | object | tool-call reliability tier, parallel-call support, max context; drives adapter degradation |
| RoutingPolicy | phase | enum(spec, architect, implement, review, io, freeform) | required |
| RoutingPolicy | provider_id | string | required, FK → ModelProvider |
| RoutingPolicy | fallback_id | string | optional; used on provider error/timeout |
| PrivacyBoundary | path_glob | string | required; repo-relative |
| PrivacyBoundary | mode | enum(local-only, redact-then-remote) | required; default local-only |
| Session | id | string | required, unique |
| Session | mode | enum(freeform, structured) | required; default freeform |
| Session | phase | enum or null | non-null only in structured mode |
| CostRecord | session_id, phase, provider_id, model, input_tokens, output_tokens, usd | mixed | one record per model call; usd computed from provider price table |
| TaskArtifact | req_id, phase, path | string | structured-mode ADLC artifacts (spec, architecture, task files) |

### Events

| Event | Trigger | Payload |
|-------|---------|---------|
| route_decided | Harness selects a model for a step | session, phase, provider, reason (policy rule that fired) |
| privacy_block | Content under a local-only boundary would be included in a remote call | path, provider, action taken (stripped / call re-routed to local) |
| phase_transition | Structured-mode gate passes | session, from_phase, to_phase, artifact refs |
| cost_recorded | Any model call completes | CostRecord |
| provider_degraded | Adapter falls back (tool-call failure, timeout) | provider, failure class, fallback used |
| daemon_client_attach | CLI or extension connects | client kind, protocol version |

### Permissions

| Action | Roles Allowed |
|--------|---------------|
| Modify RoutingPolicy / PrivacyBoundary | user only (interactive confirmation); never modifiable by model output or file content |
| Execute file edits / shell commands | harness permission model equivalent to Claude Code's (allowlist + prompt) |
| Read files under a PrivacyBoundary | local model tier only when mode=local-only |

## Business Rules

- [ ] BR-1: Content of any file matching a `local-only` PrivacyBoundary MUST never
      appear in any request to a remote provider — including embeddings,
      summaries derived verbatim, and error reports. Enforced in the daemon's
      single egress path, not in clients. This is a hard guarantee, not
      best-effort.
- [ ] BR-2: Every remote model call MUST produce a CostRecord attributed to
      (session, phase, provider, model). The cost meter is derived only from
      CostRecords — no estimated/unattributed spend is displayed as actual.
- [ ] BR-3: Freeform mode is the default experience; structured (ADLC) mode is
      opt-in and must never be required to perform an edit. The product converts
      users to structured mode with observed cost data, not gating.
- [ ] BR-4: Exactly one local-model daemon instance runs per machine; all clients
      (CLI sessions, editor windows) share it. Clients hold no session state that
      the daemon lacks (surface parity rule).
- [ ] BR-5: Routing decisions in structured mode are determined by RoutingPolicy
      (phase → provider), never by per-prompt heuristics. In freeform mode,
      heuristic routing is permitted but every decision still emits
      `route_decided` with its reason (control = legibility).
- [ ] BR-6: Any OpenAI-compatible endpoint can be registered as a provider with
      no code change. Providers with weak tool-calling get an adapter-degraded
      harness profile (reduced tool set, shorter loops, mandatory verification
      step) rather than the full loop.
- [ ] BR-7: API keys and tokens are stored only in the OS keychain; config files
      hold references. No credential ever appears in logs, CostRecords, or
      telemetry.
- [ ] BR-8: The local model tier must respond with visible-latency ≤ ~1s for its
      classification/summarization duties on target hardware (Apple Silicon
      baseline); if it can't, the router bypasses it rather than blocking the
      loop. Local-tier value is latency, not intelligence.
- [ ] BR-9: Local model selection is hardware-adaptive and measured, not static:
      a first-run probe (RAM, disk, GPU class) picks a candidate tier from a
      decision table; a post-download micro-benchmark validates it against the
      BR-8 latency duty and auto-steps-down on failure; the daemon monitors
      memory pressure at runtime and unloads/downgrades rather than swap-thrash.
      Machines below the floor run remote-only with the local tier cleanly
      absent. User-pinned model choice in config always overrides the probe.

## Acceptance Criteria

_MVP = daemon + CLI. Extension is phase 2 (see Out of Scope)._

- [ ] AC-1: Fresh install on macOS reaches a working first session with zero
      config: binary installs slim, local model downloads on first run with
      progress UI, and a freeform session can read/edit/verify a file using only
      the local model (offline demo path).
- [ ] AC-2: User can register at least two remote providers (one Anthropic, one
      arbitrary OpenAI-compatible endpoint) via CLI config and complete a
      freeform coding session routed to them.
- [ ] AC-3: In structured mode, a demo requirement flows spec → architect →
      implement → review with phase-based routing observable: `route_decided`
      events show frontier model on spec/architect/review and the configured
      cheap model on implement.
- [ ] AC-4: Cost meter: at session end the CLI reports total spend, per-phase
      attribution, and estimated savings vs. an all-frontier baseline for the
      same token volume.
- [ ] AC-5: Privacy boundary: with a directory marked `local-only`, a session
      that touches files inside it completes with zero remote calls containing
      that content, and a deliberate attempt to route such content remotely
      produces a visible `privacy_block` event. Verified by an egress-capture
      test (proxy or mock transport), not by code inspection.
- [ ] AC-6: Two clients (two CLI sessions) attach to one daemon concurrently and
      show consistent session lists; daemon survives client exit.
- [ ] AC-7: A provider with degraded tool-calling (simulated flaky adapter)
      triggers `provider_degraded` and the session completes via the fallback
      provider rather than failing.
- [ ] AC-8: First-run on a 16GB Apple Silicon machine selects a ≤3B model via
      the hardware probe, the post-download micro-benchmark reports first-token
      latency and tokens/sec, and a simulated benchmark failure (forced slow
      inference) triggers automatic step-down to the next smaller model. On a
      simulated <8GB machine, the local tier is disabled and freeform sessions
      run remote-only without error.
- [ ] AC-9: User can register an MCP server in config; its tools appear in a
      session and execute under the standard permission prompts, and a
      `local-only` boundary blocks boundary content from reaching a remote MCP
      server (same egress-capture verification as AC-5). (informed by ADR-003)

## External Dependencies

- llama.cpp (or MLX on Apple Silicon) for local inference; GGUF model
  distribution channel for first-run download.
- Candidate slim models to evaluate: Qwen coder small variants / small-MoE
  options; selection criteria are tokens/sec on laptop hardware and
  classification reliability, not benchmark coding scores.
- Provider APIs: Anthropic Messages API, OpenAI-compatible chat/completions
  (covers DeepSeek, Kimi, Ollama, vLLM, etc.).
- OS keychain integration (macOS Keychain first).
- MCP (Model Context Protocol) client support — user-registered MCP servers as
  tool providers (ADR-003); subject to the permission model and BR-1 egress
  enforcement.

## Assumptions

- The ADLC shape (phases, artifacts, gates) can be extracted into a generic,
  configurable form; the current toolkit's specifics (REQ counters, `.adlc/`
  layout, gate scripts) are personal conventions to be generalized, not shipped
  verbatim. The extraction will reveal which parts are load-bearing.
- Well-specified task artifacts genuinely close most of the capability gap for
  mid-tier models on the implement phase (evidenced anecdotally by the user's
  pipeline-runner/task-implementer + Kimi delegation experience; to be validated
  with measured first-pass success rates during dogfooding).
- Target user is cost-conscious and terminal-comfortable; CLI-first does not
  meaningfully shrink the early-adopter pool.
- macOS/Apple Silicon is the first-class platform for MVP; Linux follows; Windows
  later.
- Daemon and CLI implemented in Rust (ADR-001 in
  `.adlc/context/architecture.md`); the VS Code extension (phase 2) is
  TypeScript talking to the daemon over the protocol.

## Open Questions

- [x] OQ-1: ~~Product name?~~ RESOLVED 2026-07-17: **Teton Code**. Domains
      tetoncode.ai / tetoncode.com / tetoncode.dev registered. CLI binary:
      `teton`. Follow-up: trademark search in software classes (9/42) before
      major brand investment; mountain-range metaphor (base camp = local daemon,
      summit = frontier model, routes = routing policy) reserved for branding.
- [ ] OQ-2: Open-source strategy — fully OSS engine with paid extension? OSS
      client + proprietary router? This shapes licensing, model choice, and
      community adapter contributions.
- [ ] OQ-3: Confirm the probe decision table (BR-9): proposed <8GB RAM →
      remote-only, 8–16GB → Qwen2.5-Coder 1.5B–3B, 16–32GB → 7B, 32GB+ →
      optional Qwen3-Coder-30B-A3B. Validate tiers and exact model picks during
      dogfooding benchmarks.
- [x] OQ-4: ~~Client↔daemon protocol?~~ RESOLVED 2026-07-17: bespoke JSON-RPC
      2.0 over Unix domain socket with event subscriptions, ACP-informed
      vocabulary, ACP compatibility shim deferred post-MVP (ADR-002 in
      `.adlc/context/architecture.md`).
- [ ] OQ-5: How does structured mode acquire ADLC artifacts in repos that don't
      have them — bundled generic templates, an in-product `/init` equivalent,
      or import of an existing `.adlc/`?
- [ ] OQ-6: Cost-savings baseline methodology for AC-4 — what counts as the
      honest "what this would have cost" comparator?
- [ ] OQ-7: Does `redact-then-remote` privacy mode ship in MVP, or is
      `local-only` the only boundary mode initially? (Redaction quality is a
      research problem; local-only is enforceable today.)

## Out of Scope

- VS Code extension (phase 2 — thin client over the same daemon protocol;
  explicitly deferred, not abandoned).
- JetBrains / Neovim / web clients.
- VS Code fork (rejected: maintenance treadmill; extension path chosen).
- Per-user fine-tuning of the local model on the user's repos (roadmap
  differentiator, not MVP; local RAG may partially substitute later).
- Windows support in MVP.
- Marketplace, billing, teams/enterprise admin, hosted anything — this is a
  local-first product in MVP.
- Automatic difficulty-based routing beyond the phase policy (no ML router in
  MVP; policy table + heuristics only).
- Shipping the user's personal ADLC toolkit verbatim (the generic extraction is
  the deliverable).

## Retrieved Context

No prior context retrieved — retrieval could not run: `/spec` was invoked outside
an initialized `.adlc/` project (no `context/`, no knowledge corpus), so Step 1.6
had no corpora to score. Re-run retrieval when this spec is formally filed after
`/init`.
