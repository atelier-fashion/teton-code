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

### ADR-004: Local model weights are hosted on HuggingFace (2026-07-21)

**Decision**: GGUF artifacts are fetched directly from HuggingFace public repos
(`https://huggingface.co/<repo>/resolve/<commit-sha>/<file>.gguf`) rather than
self-hosted on `models.tetoncode.ai`. Catalog URLs pin an immutable commit SHA,
never a moving ref.

**Rationale**: zero infrastructure and zero bandwidth cost. Self-hosting the
large catalog entry (~18 GB) per download is not justifiable pre-alpha, and HF
is where these artifacts already live and are updated.

**Consequences**:
- HF `resolve` URLs 302-redirect to their CDN, so the model downloader needs a
  redirect-following client. It MUST be a **separate, credential-free client**
  from the provider/MCP egress client — the egress client's
  `redirect::Policy::none()` exists to stop a custom credential header
  (`x-api-key`, which reqwest does not strip cross-host) riding a redirect to
  an attacker-influenced host, and must not be relaxed. A model fetch carries no
  user content and no credential, so it is a distinct trust context.
- We inherit HF availability, rate limits (429/503 → backoff, reported
  distinctly from corruption), and repo/naming churn. Mitigated by pinning
  commit SHAs and by a configurable base URL (`HF_ENDPOINT`-style) that also
  serves firewalled/mirrored users and makes a future host move a config change
  rather than a release.
- `models.tetoncode.ai` stays available as a fallback mirror if HF becomes a
  real constraint.

**Alternatives rejected**: self-hosted CDN (control and stable URLs, but real
bandwidth cost and ops burden for a pre-alpha with no users); bundling weights
in the installer (rejected at charter time — a 4–8 GB installer).

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

### ADR-005: The large-band catalog entry trusts a third-party quantizer (2026-07-24)

**Decision**: the `large` band ships `unsloth/Qwen3-Coder-30B-A3B-Instruct-GGUF`
— a **third-party** quantization, not a first-party Qwen release — pinned to a
specific commit SHA with its LFS `lfs.oid` recorded as the catalog `sha256`. The
other three catalog entries are Qwen's own GGUF repos.

**Why a third party at all**: Qwen publishes no GGUF for Qwen3-Coder-30B-A3B —
`huggingface.co/Qwen/Qwen3-Coder-30B-A3B-Instruct-GGUF` does not exist. unsloth
is the most-used third-party quantizer for this model and ships the Q4_K_M as a
single file (the downloader fetches one URL). The alternatives were to drop the
`large` band entirely (leaving 32 GB+ machines with only the 7B `mid` tier) or
to quantize and self-host it (ops burden ADR-004 explicitly avoids pre-alpha).
Shipping the entry with an honest, bounded trust statement is the chosen middle.

**What the commit-SHA + digest pin does and does NOT cover**:
- **Covers — post-pin substitution.** Once pinned, the bytes cannot change under
  us: the URL names an immutable commit, the recorded `sha256` is that revision's
  `lfs.oid`, and BR-6 verifies the download against it. unsloth cannot swap the
  artifact for a fixed revision, and `refresh-catalog.py --check` fails loudly if
  the artifact at the pinned revision ever changes upstream.
- **Does NOT cover — fidelity at pin time.** The pin says nothing about whether
  the quantization was done *correctly or benignly* when it was produced. We are
  trusting unsloth's competence and good faith for the bytes as they stood at the
  pinned commit; the digest only makes that trust *stable*, not *unnecessary*.
- **Does NOT cover — the GGUF parser attack surface.** A GGUF is parsed by
  llama.cpp, whose loader has had memory-safety bugs (malformed tensor
  metadata/dimensions). A pinned digest guarantees we load the *same* bytes every
  time; it does not guarantee those bytes are safe to parse. This is a general
  property of loading any GGUF, sharpened for a third-party artifact whose
  producer we do not control. The daemon holds no additional sandbox around the
  loader today; that is a known, accepted residual risk for this entry.

**Re-adoption is deliberate, not incidental**: `refresh-catalog.py --update`
requires an explicit entry name (`--update <name>`). Re-resolving the unsloth
repo's `main` to a **new** commit — re-granting trust to bytes we have not seen —
is therefore a conscious, per-entry act, never a side effect of refreshing the
Qwen entries. The generated `models.toml` carries a `NOT an official Qwen repo`
comment on the entry so the trust boundary is visible at the point of use.

**Consequences**: revisit if Qwen (or another first party) publishes an official
GGUF for this model — prefer it. Any future move to sandbox the GGUF loader would
retire the parser-surface residual risk recorded here.

### ADR-006: A real engine enters only through the consent gate's post-verify loader (2026-07-24)

**Decision**: `tetond` constructs a real inference engine (`LlamaEngine`, behind
the non-default `llama` cargo feature) in exactly one way: the consent flow hands
digest-verified weights to a `LocalEngineLoader`, which loads on the blocking
pool, benchmarks against the BR-8 duty, and **stages** the engine per model; the
gate **commits** it into the daemon's model-tagged engine slot only after
re-checking that the model is still the recorded selection (abandoning it
otherwise), and only then publishes `ready`. The load phase holds the same
in-flight claim as the download. The only other engine source is the ungated
`TETON_LOCAL_SCRIPT` scripted stand-in, which is present from construction and
whose install outcomes never touch the tier gate (E-5).

**Rationale**: (a) unverified bytes must never reach the GGUF parser — ADR-005
accepts that parser as unsandboxed attack surface, so verification-before-load is
the compensating control, on the install path *and* on every startup (deep
digest, then load). (b) The load takes minutes, so its authorizing decision can
change mid-flight; stage → re-check → commit is what keeps a superseded flow from
making a stale engine live or evicting a successor's (LESSON-445). (c) `ready`
remains a fact: the tier opens on the slot's state, not a loader's claim, and a
failed load or missed duty publishes its reason (`EngineLoadFailed`) instead.
llama.cpp's process-global backend is initialized once per process and shared by
every engine the daemon ever loads; inputs are chunked/guarded so no C-side
assert is reachable (LESSON-444).

**Consequences**: every boot re-verifies and re-benchmarks before the tier opens
(~tens of seconds for large models — a caching policy is deferred); the harness's
context budgets and the engine window must be kept currency-compatible
(LESSON-446); default/CI builds compile none of this and keep the loaderless
honest-`disabled` behavior.
