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
- **Declared identity over derived identity** — a provider states the model it
  calls; pricing, routing, and attribution all consume that declaration. No
  subsystem re-derives an identifier from another subsystem's table, and an
  absent identifier stays `None` rather than becoming a plausible literal
  (REQ-557 ADR-A/ADR-D).
- **Dispatch on purpose, not on lifecycle position** — what a call is *for*
  (classify, summarize, edit, critique) decides which model serves it. Lifecycle
  phase remains an attribution and gating fact, never a routing input. A call site
  that knows its own purpose states it; only genuine ambiguity is classified, and
  only into a type that cannot name a purpose the call site already knew
  (REQ-558 ADR-C/ADR-D).
- **A privacy pin asserts a property, never a name** — a category or route pinned
  to the local tier is pinned by a type with no configurable counterpart, and
  resolves through a derivation that yields an id only when it is genuinely
  engine-backed. Asserting an id (`provider_id == "local"`) is not asserting
  locality: a provider registered under that id can be a remote endpoint
  (REQ-558, BUG-156, LESSON-485).
- **A gate decides on the parse the executor will use** — any check about a
  destination (allowlist, consent prompt, audit record) runs on the *same*
  parse the client that issues the request will produce, and the executor is
  handed that parse's re-serialization rather than the original bytes. Two
  parsers agreeing on ordinary input is not a property; the adversarial
  spellings are where they diverge, and a gate on a different parse is
  bypassable rather than merely weaker (REQ-563, LESSON-494).
- **A remembered grant is scoped by its key** — a permission answer attaches to
  its key, not to the sentence the user read, so the key carries every
  dimension the user was deciding about. Graded capabilities derive the key
  from the level (`permission_key_for(tier)`), and durable consent is stored in
  the same shape as the question — a per-level list, never a boolean that fans
  out (REQ-563, LESSON-495).
- **Egress owns every destination, not just every provider** — the choke point
  carries non-provider consumers too (the REQ-563 web lookup), so address-class
  refusal, redirect bounds, scan gates, timeouts, and one-row-one-event
  accounting are properties of the seam rather than of each caller. A tool that
  reaches the network is handed transport; it never constructs one, and the
  tree-wide `deny_http_client` check keeps that mechanical.
- **Policy is pure, mechanism is gated** — when a subsystem's interesting logic
  sits behind a non-default cargo feature CI never compiles, the *decision* is
  extracted into a feature-free module over plain data and the gated module is
  left holding only FFI. Otherwise the subtlest code in the tree ships with the
  least coverage. The test double must consume that same extracted policy, not
  a reimplementation — a double with its own copy of the rule tests only that
  two implementations share each other's bugs (REQ-564, LESSON-499).
- **A lifetime extension re-asserts its invariants at the seam** — when a
  value outlives the scope that enforced its invariants (a per-turn context
  becoming a per-session conversation), every invariant either travels with
  the value or is re-evaluated at one commit seam every writing path shares —
  success, error, and abort alike — and facts are recorded where they were
  known, never re-derived downstream of the knowledge (REQ-567, LESSON-501).
- **A foreign handle that is both borrowed and `!Send` wants a thread** — those
  two constraints together are the callee describing its ownership model, not
  two obstacles to route around with `unsafe impl Send` and a lifetime erasure.
  Give the parent and child one owned thread and a channel; the borrow becomes
  an ordinary stack borrow, `Send` stops being a question, and drop order is
  compiler-checked rather than commented (REQ-564, LESSON-498).
- **A mutually-exclusive wire shape is a return type, not a pair of flags** —
  when two request fields cannot both be sent because a provider answers 400 on
  both, encode the exclusion as an enum whose variants *are* the outcomes, and
  match on it with no wildcard arm. The illegal state stops being representable
  instead of being defended by a test that has to keep passing, and adding a
  third shape becomes a compile error at every adapter rather than a silent
  omission at one. The same posture as ADR-009's frame containment, applied
  outbound (REQ-559 ADR-A).
- **A required field with no `Default` is how "every call states X" is enforced**
  — a rule of the form "no call path may omit X" is a guard that holds only until
  someone adds the next call path (LESSON-443). Making X a required struct field
  whose type has no `Default` turns "the path forgot" into a compile error, and
  leaves the test to check that the value is *honest* rather than that it exists
  (REQ-559 ADR-B).
- **An unknown endpoint's default capability is the intersection, not the
  superset** — for a provider you cannot identify, the permissive default sends
  values it will reject, and the rejection path lands back on the vendor default
  the feature existed to override. Default to the narrowest set every known
  target accepts, and let the user widen it by declaring one. The failure the
  generous default reintroduces is usually the one the feature was written to fix
  (REQ-559 ADR-E).
- **Remembering a refusal is not retrying it** — a rule against silent retries
  forbids making a failing request again and hoping; it does not forbid declining
  to make it. A refusal observed at runtime is remembered *session-scoped*, keyed
  by what the user configured, never persisted, and never allowed to mutate the
  *declared* capability — persisting a capability conclusion drawn from one HTTP
  status is sniffing, and it outlives the condition that produced it. The
  condition that makes it a degradation rather than a hidden downgrade is that
  the surface reports it (REQ-559 ADR-F).
- **A named preset classifies the open set by its default, never by a list** —
  when a setting names a posture over an existing per-item table, the items it
  enumerates are only the *closed, first-party* ones; everything else — every
  MCP tool, every tool added tomorrow — falls to the preset's default policy,
  which is the preset's answer to "something I do not recognise". Enumerating
  the mutating side is impossible (those names are server-supplied) and
  enumerating it badly fails open. One function turns the name into the table,
  and the legacy constructors delegate to it rather than holding a second copy
  (REQ-560 ADR-A/ADR-B, LESSON-456).
- **A remembered answer never outranks the rule that decides whether to ask** —
  a session grant answers a question; if the current policy would not have asked
  it, the grant is not consulted. Evaluating the policy first is what makes a
  tightened setting take effect immediately and a loosened one restore the
  earlier answer, and it keeps a stale grant from surviving a posture the user
  has since changed (REQ-560 BR-5).
- **A gated surface splits into pure content and gated bytes** — when a feature
  renders only behind a TTY (or any other gate CI does not cross), its *content*
  is a pure function of state, unit-tested with the gate out of the way, and only
  the bytes that reach the terminal stay gated. Otherwise the gate that hides the
  feature from users hides it from the test suite too, and the feature ships
  unverified (REQ-556, REQ-560 BR-8, LESSON-481).
- **Enablement is collection at the edge, commitment at the core** — a guided
  setup flow holds no server-side step state: clients collect and buffer
  answers (input buffering is not session state), the daemon exposes
  plan/preview/commit endpoints, validation is the startup validator run on a
  candidate, the preview is digest-bound to the commit so what the user
  confirmed is what is written, and the commit point is the config swap every
  consumer already reads per turn. Secrets are written by the surface that
  collected them, by reference everywhere else, with a displaced-state-aware
  undo (REQ-572 ADR-1/ADR-2/ADR-3, LESSON-514). The pattern now has two
  instances — `/web setup` (REQ-572) and `/provider setup` (REQ-579) — and a
  third should be built by copying the trio, not by generalising `config/set`:
  the preview-then-digest-bound-commit is what makes the flow safe, and
  `config/set` has neither a preview nor a digest, and persists one update per
  call, so a multi-row change through it is not atomic (REQ-579 ADR-1). When
  the flow's success depends on the *model* saying something, guarantee it at
  the surface instead — a small local model transfers data reliably and
  directives unreliably (REQ-579 ADR-9, LESSON-532).
- **A pre-authorization publish is attacker-paced** — any event published
  before an authorization gate answers needs the caller-chosen-id length
  bound AND a per-connection budget, and read-only endpoints prefer silent
  refusal over announcement (REQ-572 verify, LESSON-513; the
  `may_announce_grant` precedent generalized).
- **Adapter degradation** — providers with weak tool-calling get a reduced
  harness profile (smaller tool set, shorter loops, mandatory verification)
  rather than the full loop (BR-6).
- **A durable document is edited, never re-uttered** — a writer that persists
  user-authored configuration applies its semantic delta (diff of the caller's
  pre-mutation state vs its candidate, never a diff against the document) to
  the document as it exists on disk, and validates the exact bytes it will
  write — so comments, key order, and unknown keys are not collateral of an
  unrelated save, hand-edit drift at untouched keys survives, and an
  unparseable document is a loud refusal rather than a silent rewrite. An
  element-wise edit inside an array re-establishes the element's *identity*
  (its natural key) before writing; when identity cannot be established,
  wholesale replacement is strictly safer than a positional guess (REQ-574
  ADR-1/ADR-4, LESSON-522, LESSON-456).
- **Graceful absence** — the local tier disables itself below the hardware
  floor or under memory pressure rather than degrading the machine (BR-8/BR-9).
- **Suggestion data is daemon-owned, typed, and seam-pinned** — a list clients
  display (search backends; the model catalog before it) is one daemon factory
  returning protocol types, carried on an existing stateless RPC as an
  additive field. Contract tests enumerate the typed source, never another
  crate's source text; prose copies (bundled guide, README rows) are CI-gated
  against it bidirectionally; and cross-binary parity is asserted on the bytes
  that cross the seam in e2e, not by twin goldens that each verify their own
  author. Corollary: a seam that gains a sanitizer takes over every legitimate
  use of the alphabet it destroys — styling moves inside the seam, applied
  after defusing (REQ-573, LESSON-517). Second instance: the provider recipe
  catalog (REQ-577), which adds two rules — a fact a catalog ships about an
  external system is verified against **both halves of its contract**, the
  third party's and the product's own consuming seam, with a seam test
  crossing the two (LESSON-523); and the cap-exempt tool set admits members
  only for a stated, doc-commented rationale distinct per tool (web = user
  opt-in; teton_docs = self-serving product knowledge), so the exempt set
  stays a checked rule rather than a dumping ground (LESSON-524).
- **A forgiving input surface normalizes at the write seam and echoes what
  it stored** — the persisted value is always the literal contract value
  (an absolute request URL), so every downstream consumer (validation,
  adapters, egress origin-binding) stays verbatim; composition never runs at
  request time, the seam is gated by the same shape check its predicates
  presuppose, and the echo renders the host the request will actually reach
  (REQ-578, LESSON-528, LESSON-529). Corollary: a pure, I/O-free slice of
  daemon-side logic may be linked into a thin client when duplicating it
  would be the drift risk — the crate's manifest states the widened
  consumer set, and the thin-client rule is preserved by the purity, not by
  the dependency direction (REQ-578 ADR-1).
- **A transient refusal becomes a wait, at the classifier — not a retry, at
  the client** — when a refusal's own code says "this ends by itself"
  (BUG-152's `TIER_WARMING`), the daemon holds the request on the state
  transition that ends it and announces the hold as a typed event, rather
  than the client re-sending on a heuristic. The hold reads the *same* typed
  classification the refusal renders (`LocalTierState`), sits before anything
  the request would otherwise spend (tools, head, conversation, title claim),
  wakes on every gate transition and re-reads rather than trusting the wake,
  and ends early when the issuing client leaves — a held request has no
  paid-for work to protect, unlike a started one (REQ-565's drain rule holds
  past the hold). Corollary: a shared claim reused by two phases (the M-2
  install claim, taken by both the download and the load) is read *with* the
  state that tells the phases apart, or every reader calls the second phase
  by the first one's name (REQ-580 ADR-1..4).
- **A user-invoked probe reuses the call path it probes, and names its
  outcome from facts the product owns** — a connection test that took a
  shortcut (a `/models` list, a HEAD) would prove reachability of an endpoint
  the product never POSTs to; the test is the real request, minimal, typed on
  the way back (one value per fact — a redirect that is not a completion and a
  deadline that elapsed are their own outcomes, not prose inside
  `unreachable`), and it moves the same health the router reads. Reasons are
  composed from status, dial host, configured model and the credential
  *reference*, never a third party's body or header. Corollaries from verify:
  a probe is a **billed call**, so at teardown it drains like a turn and holds
  a lifetime claim like a turn (or `_exit` beats the drain and the row is
  lost); a fixed small request is not a turn, so it carries its own deadline;
  and a consent preview renders an endpoint through the one shared masking
  renderer, or it prints the userinfo it exists to protect (REQ-581 ADR-1..6,
  LESSON-535).
- **A message for one connection is routed, never published** — a catch-up,
  a replay, a "so this client learns X" is addressed to the connection that
  attached; it goes out on that connection's own outbound as a routed frame
  (seq from the bus, behind the response it follows), not through
  `EventBus::publish`, whose daemon-scoped fan-out passes every subscriber. The
  bus is for news; a per-connection frame on it is a leak of whatever the
  frame carries. When a routed path is added, audit every existing publish for
  an audience of one — the handshake's lifecycle replay stood as a broadcast
  for fifteen REQs after REQ-569 made routing possible (BUG-177, LESSON-536).
- **A second surface for a command is a second call site of one grammar and
  one renderer** — a session command that mirrors a shell command rebuilds
  the shell's argv and parses it with the shell's own parser, then runs the
  same `<sub>_on(conn, ctx)` body the shell wrapper runs; recognition of a
  typed shell line walks the parser's own tree to the subcommand path and
  dispatches through the same table; the nudge that points a model's shell
  recipe at the session spelling is derived from that table. Corollaries from
  verify: parse *before* any gate so `--help` is never refused; a classifier
  that resolves to every row inherits every row's argument grammar and gate
  posture, so validate with the one grammar first; and a session command that
  reads a secret confirms before it reads, because the entry loop and the
  dialogue prompter share one stdin buffer and a multi-line paste answers the
  next question (REQ-582 ADR-1/2/6, LESSON-537; REQ-555 BR-4 generalized).
- **The block a tool-call turn pushes ends with the call, whichever source
  produced it** — the local tier's reply is cut right after the call it
  parsed; a remote provider's structured call is rendered by the loop onto
  the prose in the reply grammar the system prompt teaches
  (`{"tool": …, "arguments": …}`) before the block is pushed. So the
  transcript always says what the assistant did, an assistant turn is never
  empty (every remote provider answers 400 to one), and OQ-1's cancellation
  trim cuts the *trailing* call and nothing ahead of it. `prepare()` enforces
  the wire-shape rules — user-first, alternating, **no empty message** — at
  the seam that builds the sequence, because more than one writer can produce
  an empty block (BUG-178, LESSON-538).
- **A session's ground is a derived value with one probe and three consumers**
  — the jail root's kind, display and project facts are derived from the
  stored path at every use (turn, `session/create`, `/cd`), never cached; the
  prompt states them as data (one bounded line, byte-bounded only where it is
  resident), the tools enforce them (jail refusals name the root, walkers read
  the kind), and the client renders what the daemon derived. A turn re-reads
  the path **after** taking its claim — state snapshotted before the claim is a
  hint, not a fact (REQ-583 ADR-1/ADR-2/ADR-4, LESSON-539, LESSON-473).
- **A walker's harness lines wear one prefix and are peeled by one recogniser
  that knows every writer's shape** — every non-match line a search tool
  appends starts `... (` and is authored in `walk.rs` (trailers, cap notices);
  the duty that ranks matches strips exactly those shapes, so authoring a new
  harness line is a two-sided change (writer + recogniser, pinned by a test
  that enumerates every writer) and a new line is never ranked as a match by
  accident. The walk itself is one policy — budget, skip set (never nameable),
  home-top-level prune (nameable), bundle suffixes — consumed by every walker,
  and a zero-budget fixture holds one entry because listing order is the
  filesystem's, not the test's (REQ-583 ADR-3, LESSON-540).
- **A resident fact is bought with reference data, never with the ceiling** —
  the third instance of the redact.rs rule: the environment block (a fact a
  model cannot learn by calling a tool) was paid for by moving the guide's
  `[web]` key reference into the `teton_docs web` topic, with the floor and
  the 9 KiB overhead untouched; the task that measures a composed artifact
  runs after every task that writes to it (REQ-583 ADR-2, ASSUME-008,
  LESSON-541, LESSON-491).
- **A per-route fact is derived once, where the route is decided, and every
  surface reads that value** — the context budget joins effort as the second
  instance (`RouteBudget` beside `ResolvedEffort`): one pure function over
  plain data (`harness/budget.rs`), one caller (`Router::budget_for`), and the
  result stamped into the route so `route_decided`, `/verbose`, `/doctor`, the
  `context_pressure` event and every refusal read the same number and the same
  bound rather than each deriving one. A fact that changes when a turn is
  **rerouted mid-turn** is re-derived and re-applied before the next model
  call, and the change is published as news, not applied in silence
  (REQ-586 ADR-1/ADR-2/ADR-3, LESSON-456).
- **Three consequences of the route-aware budget the next author must hold**
  (REQ-586 verify): (a) the **router reads `[privacy] redact`** — an egress
  fact reaching routing, correct because one per-turn `Config` feeds both
  `build_router(…).with_redact_scan` and `redaction_gate`, so a bound and the
  gate it anticipates cannot disagree; re-reading the config for one of them
  would break that, so don't. (b) The system prompt's byte overhead is now a
  **production input to a user-visible budget** — `REDACT_BODY_OVERHEAD_BYTES`
  stopped being test-only, so *adding a tool description raises the overhead
  and shrinks every redact-scanning route's context by the same bytes*; it
  belongs beside the "measure the composed artifact last" rule above. (c) The
  context budget is the **only per-turn input-token bound that exists** —
  there is no spend cap; with 1M-token windows shipping, one prompt can carry
  ≈25M input tokens across its iterations, `context_budget_cap` is the sole
  knob, and a notice (not a cap) is what fires when a big window is recorded.
- **A fact that crosses a seam is tested on both sides and once across** — a
  renderer test that builds the wire value by hand proves the consumer and
  says nothing about the line that produces it. Four producers shipped
  unguarded in one REQ and each survived the whole suite under mutation; the
  grep that finds them is "a test constructing a wire type with a struct
  literal" (LESSON-544). Its sibling: a rule worth enforcing — one home per
  fact, one call per paired decision — needs a **test**, because a checklist
  recorded in a task file has no schedule and no owner (LESSON-545,
  LESSON-546).
- **User-authored prompt text is a first-class provenance source** — prompt
  text carried no file provenance at all until a `/`-command's expansion had to
  pin its turn exactly as a `read` of the same file would. `Provenance::User`
  therefore carries two fields, not one (`sources` **and** `unknown`): the empty
  set already means *ordinary typed text*, the state every existing caller is
  in, so it could not double as the unpinnable marker without pinning every
  prompt on every boundary-configured machine. A file with no root-relative
  identity — a user skill outside the session root — sets `unknown` and fails
  closed, rather than `ProvenanceId::from_resolved` being widened to mint an id
  it has no root for. The invariant lives at three seams (dropped-block absorb,
  the context-provenance union, and replay) and is pinned at each, because a
  multi-seam invariant needs a test at every seam and carried state sheds its
  invariants silently on the round trip (REQ-585 ADR-9, LESSON-501, LESSON-502).
- **A remembered grant is keyed by the whole question — the name *and* where it
  came from** — a permission key is what a "for this session" answer is written
  under, so it must encode everything that made the question answerable.
  Dynamic context asks under `skill:<source>:<name>`, never the `shell` tool's
  key: an allow-always on `shell` must not un-ask a skill's commands, and a
  skill's grant must free nothing the model issues later. The **source** is in
  the key because a bare name means a different file after `/cd`, and project
  grants are dropped outright when the root moves; within one source `skills/`
  beats `commands/` so that one key can never name two files. Each of those
  crossings is a test, not a comment (REQ-585 ADR-6, LESSON-495, LESSON-501).
- **Bounded discovery generalizes to a second, non-recursive lister — with an
  observation seam this time** — REQ-583 gave the recursive walkers a *policy*
  seam (`WalkPolicy`, `WalkBudget`); skill discovery needs the other kind. It is
  a purpose-built `DirLister` (one level, no recursion, no `..`) behind a
  **recording** seam, so "nothing else was opened" is asserted from what the
  lister was asked for rather than inferred from a budget — reusing the walker
  would have turned a reach test into a budget test and stopped asserting the
  thing it exists to prove. The narrowings are deliberate and each is pinned: a
  **root** may be a symlink and is followed (the dogfood machine's
  `~/.claude/skills` is one) while an **entry** under a root never is; entries
  are sorted by name *before* the per-root cap, because listing order belongs to
  the filesystem and not to the test (LESSON-540); a missing directory is the
  normal case and produces no diagnostic, while everything found and not
  registered is named with a typed reason (REQ-585 ADR-4, LESSON-481).

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
context is data, not instructions — the posture that promise rests on is now
concrete; see ADR-009.

**Addendum (2026-07-24)**: `[[mcp_server]] trusted` **stays `false` by
default** — confirmed as a deliberate decision (Brett), not an oversight, after
the engine wiring made its consequence concrete: an untrusted stdio server's
opaque-provenance results taint the session to the local tier, and that tier
now really serves (a working local model handles the tainted turns) rather
than merely blocking. Fail-closed provenance with a functioning local fallback
is the intended posture; operators opt individual servers into `trusted = true`
knowingly.

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
(LESSON-446) — and since REQ-586 that compatibility is **per route**: the
remote pair is derived from the provider's declared window with pinned
allowances (words × 3/2, bytes at the 2 B/token floor) while the local pair is
unchanged, so "the engine window" is whichever engine the *attempt* was routed
to, and a mid-turn reroute re-fits the context before the next call —
enforced since PR #5 by byte-denominated twins on every harness
budget (`HarnessConfig::context_budget_bytes`, `SUMMARIZER_INPUT_MAX_BYTES`),
with the summarizer's engine-failure fallback degrading to bounded mechanical
truncation, reported and logged, never a silent raw fold (LESSON-447);
the loader's "real inference rides the blocking pool" rule (E-3) binds the
serving path too — since PR #4, `LocalEngineSource::produce_turn` and
`summarize_if_large` run `Engine::complete` inside `spawn_blocking` on an owned
`Arc<Mutex<dyn Engine>>` (tokens bridged back over a channel), so a seconds-long
local completion can never park a tokio worker and stall unrelated RPCs
(LESSON-448, pinned by `tests/nonblocking_inference.rs`);
default/CI builds compile none of this and keep the loaderless
honest-`disabled` behavior.

### ADR-006: Distribution is a Homebrew tap of prebuilt binaries, formula templated in-repo (2026-07-26)

**Decision**: users install with one command —
`brew install atelier-fashion/tap/teton` — from `atelier-fashion/homebrew-tap`,
which serves prebuilt per-platform tarballs from GitHub Releases. The formula's
source of truth is `packaging/homebrew/teton.rb.tmpl` in THIS repo; the release
workflow renders it with the tag and the artifacts' real sha256s and pushes it.
The tap is a publish target, never hand-edited. `brew services` runs the
daemon binary (`teton-code` since ADR-007) under launchd.

**Rationale**: a source-build formula would reimpose the Rust + cmake burden the
one-command install exists to remove; homebrew-core needs notability the project
does not yet have. Keeping the template here puts formula changes through this
repo's PR review. The install stays small because REQ-547's consent flow fetches
weights on first run rather than bundling them.

**Consequences**: releases are tag-driven and gated — tag/`Cargo.toml` version
agreement, a per-target smoke that asserts the DECISION 3 seam refusal on the
shipped binary, install + `brew services` verification before the tap is
pushed, and a tap-wide concurrency group so two releases cannot race the
formula backwards. The x86_64-darwin leg is cross-compiled and Rosetta-smoked
(GitHub retired Intel macOS runners), recorded at that strength rather than as
native verification. Two hardening items are deliberately deferred and recorded
in REQ-548: build-provenance attestation, and environment-gated secrets for the
tap token and the GCP credentials.

### ADR-007: The daemon ships as `teton-code`; the crate and runtime paths do not follow the rename (2026-07-31)

**Decision**: the shipped daemon executable is `teton-code` — a `[[bin]]`
target rename only (REQ-549, PR #12). Three names deliberately do NOT follow:
the crate stays `tetond` (so `tetond::` imports and `--features tetond/llama`
are untouched), the runtime rendezvous filenames stay `tetond.sock`/`.lock`/
`.log`, and the protocol shape is unchanged (only the handshake's
`daemon_name` value moved).

**Rationale**: macOS attributes permission dialogs — Keychain, network — to
the requesting process's executable filename, and the daemon is what resolves
`keychain://` references at call time (REQ-544 BR-7). "tetond" read as a typo
at the moment the OS asks the user for trust (LESSON-457). The crate name is
invisible to users and renaming it would churn every import and build
invocation for zero user-facing gain. The socket/lock filenames are an
upgrade-compatibility contract: keeping them stable means a newly-installed
CLI finds an already-running old daemon rather than racing a second daemon
against it (the single-instance flock spans versions only if the lock path
does).

**Consequences**: every surface naming the executable agrees on `teton-code`
(daemon self-reports, CLI autostart, packaging, release scripts, tests, docs).
Users see one Keychain re-prompt after upgrading, because ACL grants bind to
executable identity; without a stable code-signing identity every rebuild
re-prompts anyway — recorded as REQ-549 OQ-2, to be combined with REQ-548's
deferred provenance/signing work. Renaming the runtime filenames later is a
separate migration (REQ-549 OQ-1) that must solve cross-version
single-instancing before it ships. The interactive startup UX added alongside
(skyline banner, framed entry prompt) renders strictly through the existing
`Surface`/`Prompter` seams and is TTY-gated, so non-interactive output remains
byte-identical — the future ratatui front-end inherits both by implementing
the same seams.


### ADR-008: Releases are signed, attested, and environment-gated; gates prove exactly what they claim (2026-08-01)

**Decision**: macOS release binaries are Developer ID signed (Team
545BU9G9D6) inside the build job via an ephemeral keychain (p12 removed
seconds after import); all release artifacts — three tarballs AND
checksums.txt — are attested with `actions/attest-build-provenance` and
re-verified post-publish with `gh attestation verify --signer-workflow
atelier-fashion/teton-code/.github/workflows/release.yml` plus a same-run
sha256 cross-check, in a seam-testable batch script; a failed gate blocks
`bump-formula` through the needs-graph. Credential-bearing jobs declare
GitHub environments (release-signing, tap-publish, site-deploy; rules
`main` + `v*.*.*`), and every action in a credential-bearing job is
SHA-pinned. Gate exit taxonomy: 0 PASS / 65 FAILED (bytes are bad —
unforgeable by tool absence, crash, or signal death) / 75 UNCHECKED.

**Rationale**: REQ-550. Signing stabilizes executable identity so Keychain
grants survive upgrades (ADR-007's residual); attestation binds published
bytes to this repo's release workflow, not merely to the repo; environment
gating removes the any-ref-readable tap token. Every gate has known-bad
selftest fixtures proving it goes red (LESSON-454), driven through
tool-override seams that refuse to run in CI (LESSON-460 governs fixture
fidelity).

**Consequences**: the first signed release triggers each user's last
Keychain re-prompt. The 2026-08-01 accepted risk (unlocked keychain
reachable by third-party build scripts during `cargo build`) was CLOSED by
REQ-551 (2026-08-03): the identity now exists only for import → sign →
early-destroy, every credential step scrubs the forward-flowing environment
channels (BASH_ENV, exported functions, PATH — LESSON-463) and uses absolute
tool paths, and all of it is self-asserted by in-suite workflow mutants
(selftest 407 cases; LESSON-464). Residual: a persistent background process
planted during the build is runner-level compromise, out of threat model —
job isolation is the future fix if it ever enters scope. Post-merge human
steps live in docs/release-runbook.md §11 (one green release, then delete
the repo-level tap token, then the negative probe). AC-6 (grant survival)
is first observable at the second signed release.

### ADR-009: Frame is frame in both directions; sanitize where the frame is written (2026-08-04)

**Decision**: the harness's prompt-injection posture is **structural
containment at three layers**, not a textual request to the model. Every layer
holds in *both* directions — what the model may not emit is exactly what
content may not introduce — and each is enforced at the code that authors the
frame it guards, never at a downstream pass over an already-flattened string.

| Layer | Frame | Input guard (content may not introduce) | Output guard (model may not emit) |
|---|---|---|---|
| Tokenizer | `<\|im_start\|>`, `<\|…\|>` by shape, `<tool_call>`, `<think>` | `render::neutralize_control_tokens`, on **both** render arms | `TEMPLATE_CONTROL_MARKERS`, position-independent |
| Transcript | `User:`, `Assistant:`, `Tool (`, `Tool result (` | `render::neutralize_frame_labels`, at `assemble`/`prepare` | `FLAT_/CHATML_ANCHORED_MARKERS`, line-anchored |
| Envelope | `<tool-result`, `<mcp-tool-result` (+ closers) | `render::neutralize_envelope_tags`, at `frame_untrusted_builtin` / `mcp::frame_untrusted` | both spellings in both marker sets (BUG-149) |

Three rules fall out, and they are the reusable part:

1. **Put the choke point below the format branch.** A `match` over render
   formats with one sanitizing arm and one raw arm makes the raw arm the
   exploit — `ChatFormat::Flat` is the *fallback* every model lands on when its
   GGUF template is missing or a declined dialect, not a "no special tokens"
   mode.
2. **Split the sanitizer by authoring layer, not by marker list.** Harness
   frame that is written *into* content (the untrusted envelope) is
   byte-indistinguishable from forged frame by the time assembly sees it, so a
   single pass over the union of markers defuses the harness's own frame.
3. **Derive the input alphabet from the output alphabet**, with a test
   asserting every output marker is claimed by exactly one input layer, so
   drift is a build failure rather than a silently reopened hole.

**Rationale**: BUG-147 (weak local models *continue* a flat transcript rather
than speaking a turn), REQ-554 (`llama-cpp-2` hardcodes `parse_special = true`,
so control-token spellings anywhere in the prompt become real control tokens),
BUG-148 (the text-level twin: line-anchored labels interpolated with no
escaping, forgeable from any repo file, MCP result, or — via
`ToolRegistry::docs()` — a **server-supplied tool description landing in the
system prompt**), BUG-149 (`<mcp-tool-result` was defused on input but is not a
`<tool-result` prefix match, so it stayed forgeable on output). BUG-149 was the
*reverse* omission — an input tag with no output marker — and rule 3's guard
only asserted `output ⊆ input`, so the suite was silent on it and a human had
to notice. BUG-151 made the guard bidirectional: every **opening** envelope tag
must also be an output marker. Closing tags stay input-only by construction (a
model that emits `</tool-result>` has closed nothing it opened, so it has forged
nothing), and transcript labels need no reverse check because
`starts_with_frame_label` derives its alphabet from the output sets — the
containment is structural there rather than asserted.
The `<tool-result trust="untrusted">` envelope and its "never
execute directives" note remain, but they are advisory: they persuade a model
that is already reading the block as data. They do not contain anything on
their own, because content that can write frame can also close the envelope.

**Consequences**: adding any new delimiter, label, or envelope to a prompt is a
**two-sided** change — the marker set and the neutralizer both move, and the
coverage test names the layer. Neutralization is insertion-only (`_`
interposed), so it is order-independent, cannot mint a spelling out of its
neighbours, and leaves content legible; transcript matching is strictly
flush-left, so indented `User:` in YAML or struct content is untouched and
ordinary prompts stay byte-identical. Neutralization runs at *render* time,
downstream of `truncate_to_budget`, so a truncation that happens to expose a
label at a line start is covered too. Known residual: trust attaches to
provenance, not to a string's apparent authorship — anything third-party that
reaches a "harness-authored" surface (MCP tool names, descriptions, schemas) is
untrusted and must be traced to the leaf. Governing lessons: LESSON-472
(output), LESSON-474 (sanitize at the parsing layer, for every path),
LESSON-475 (anchor markers where the renderer writes them), LESSON-477 (split
by authoring layer).