# REQ-572 — Architecture: Capability-aware refusals and guided in-session enablement

## Approach

Two halves, sharing one new pure-policy seam.

**The shared seam: one capability-state derivation.** A new feature-free
function in `teton-core` derives the web capability state from the same inputs
that already govern tool exposure:

```rust
pub enum WebCapabilityState {
    /// tier > off — the tool registers; carries the tier.
    Ready(WebTier),
    /// no [web] table / tier = off — available but off (the observed failure).
    OffAvailable,
    /// tier permits search structurally, but the search leg cannot serve:
    /// carries the named missing piece (no local model per REQ-563 BR-14 /
    /// product decision 1b — search blocks per query with a notice).
    SearchUnavailable { reason: SearchGap },
}
```

`register_web_tool`'s existing predicate (`tier == Off` → not registered,
REQ-563 BR-1/D-1) is re-expressed as a consumer of this function, so the
refusal text, the status surface, the setup flow, and actual tool exposure all
read **one** classifier (spec BR-3; LESSON-496's registration-vs-exposure
split, LESSON-456's one-classifier rule). Note: `tier = "search"` with no
`search_endpoint` cannot occur at runtime — `Config::validate()` refuses that
document at load (`WebSearchTierWithoutEndpoint`) — so the runtime state enum
carries no "partially configured" variant it could never observe; the
partially-configured experience lives at **preview time** in the setup flow,
where a candidate config is validated before it is ever written.

**Half 1 — capability-aware refusals.** `build_system_prompt` already emits
`WEB_OPT_IN_CLAUSE` when the web tool is absent (REQ-563 BR-6, keyed on
exposure). This REQ upgrades that single clause into per-state clauses driven
by `WebCapabilityState`, names the new `/web setup` command as the enablement
path, extends the BUG-160 bundled guide (`self_config.md`) with the `[web]`
surface (today it covers only providers — the same LESSON-493 hole one
capability over), and adds the once-per-conversation dedup instruction
(REQ-567's conversation carry gives the model its history; the instruction
tells it to compress repeat offers to one line). The system prompt is rebuilt
every turn, so no cache invalidation exists to get wrong.

**Half 2 — guided enablement: collect at the edge, commit at the core.** The
flow is three stateless daemon endpoints plus client-local collection:

```
CLI (/web setup, TTY-gated)                 tetond
────────────────────────────                ─────────────────────────────
web/setup_plan ────────────────────────────▶ capability state, what search
◀──────────────────────────────────────────  needs, current [web] summary
collect answers locally:
  tier (menu; search marked
  unavailable+reason when the
  local model is absent)
  endpoint (text)
  key (echo-off, memory only)
web/setup_preview {tier,endpoint,…} ───────▶ build candidate Config, run the
◀──────────────────────────────────────────  SAME Config::validate() startup
  exact [web] TOML + host from the           uses; host from the executor's
  executor's parse + warnings                parse (reqwest::Url, LESSON-494)
render preview; user confirms (default-no)
keychain.store("web-search", key) → ref      (secret never crosses the socket)
web/setup_commit {…, key_ref} ─────────────▶ re-validate candidate, atomic
◀──────────────────────────────────────────  write (persist_web_tier pattern),
  applied                                    swap in-memory Config, publish
on commit error: keychain.delete(entry)      WebSetupCompleted
```

Why this shape instead of a daemon-held multi-step flow state machine (the
spec's System Model sketch): the daemon already rebuilds the tool registry
**per turn** from the live config mutex (`build_tools`, runtime.rs), so the
only state that matters is the config itself — a server-side flow registry
would add exactly the pending-state surface BUG-161/BUG-162 taught us to fear
(cross-session ids, bystander answers) to guard state that has no reason to
exist. Unsubmitted answers are input buffering, which every client already
does with the prompt line; surface parity (ADR-002 BR-4) is preserved because
the daemon remains the sole authority on validation, preview content, and the
commit. See ADR-1 and the spec-mapping table below.

## Settled Open Questions (product decisions recorded)

- **OQ-1 (bystander sessions):** better than the spec's minimum, for free —
  `build_tools` clones the live config per turn, so **every** session picks up
  the enabled capability on its next turn after commit. The
  `WebSetupCompleted` event stays session-scoped to the committing session;
  bystanders see the state in their status surface. BR-13 taint restrictions
  in a bystander session are unaffected (taint is session state, not config).
- **OQ-2 (post-setup lookup offer):** no auto-fired lookup. The completion
  notice states the capability is live and that the next web-needing question
  will raise the ordinary Ask consent. Rationale: BR-13 forbids the flow
  performing egress; an auto-offered lookup is new egress machinery for a
  sentence's worth of UX.
- **OQ-3 (command spelling):** `/web setup` — joins the existing `/web allow`
  / `/web refresh` family in `slash.rs`; a capability-generic `/setup`
  namespace can arrive with the provider flow (out of scope here) without
  breaking this spelling.

## Wire changes (all additive, protocol stays v2)

- **Methods** (session-scoped, `may_drive`-gated like `web/override`):
  `web/setup_plan`, `web/setup_preview`, `web/setup_commit`.
- **Events**: `WebSetupCompleted { tiers, config_path }` (session-scoped;
  LESSON-505: an event in front of a human, not a log line);
  `WebSetupRejected { origin }` published when a **commit** arrives from a
  connection that fails the gate, at most once per connection (spec BR-4/AC-4
  defense-in-depth; the preview's silence and the budget are the recorded
  deviation in the spec-mapping table below).
- **Error code**: `WEB_SETUP_INVALID = -32020` (next free after
  `SELF_APPROVAL_REFUSED`) — a candidate config failing validation at
  preview/commit, message carrying the validator's own sentence. Gate failures
  reuse `NOT_ATTACHED` (-32009) — the existing classifier for that state —
  plus the rejection event.
- **`ConfigSnapshot`**: additive `web_capability` field (`#[serde(default)]`)
  so the status surface renders capability state without a new round-trip.
- Capability **states** travel as a typed enum in `web/setup_plan`'s result
  and in `ConfigSnapshot` (spec BR-10's "distinct on the wire" — a typed field,
  not prose the client re-parses).

## Key decisions

### ADR-1: Stateless setup endpoints; collection lives at the client edge

**Decision**: no server-held per-session setup flow state. Three read/write
endpoints (`plan` / `preview` / `commit`); the CLI collects answers locally
and the daemon validates and commits atomically.

**Rationale**: (a) the per-turn `build_tools` rebuild means config **is** the
state — a flow registry would be a second copy; (b) every pending-prompt
registry we have shipped grew a cross-session or bystander-answer bug
(BUG-161, BUG-162, LESSON-503/504) — the cheapest such surface is the one that
does not exist; (c) abort semantics collapse to "the client stops asking"
(spec BR-11 trivially holds for config; keychain cleanup is client-side, which
is also where the write happened).

**Spec mapping** (deviations recorded, intent preserved):

| Spec item | As specced | As designed | Why the intent holds |
|---|---|---|---|
| Entities: SetupFlow (daemon, session-scoped) | daemon state machine | client-local collection + stateless endpoints | BR-5's motive is thin clients + id hygiene; daemon keeps sole authority over validation/preview/commit, and the id-collision surface BR-5 guards against now has zero instances |
| Events: setup_started / setup_step / setup_aborted | daemon events | not emitted — no daemon state changes before commit | BR-14 requires announcing **completions and rejections**, which remain events; steps with no daemon effect have nothing to announce (LESSON-505 is about state changes) |
| AC-11 concurrent flows | id isolation across flows | no shared flow state; commits serialize on the config mutex | the failure mode AC-11 hunts (cross-answered steps) is structurally unrepresentable; the AC's event-delivery leg is still asserted at the client |
| BR-4 / AC-4 rejection announcement | `web_setup_rejected` published for **both** a refused preview and a refused commit | published for a refused **commit only**, at most **once per (connection, session)**, and only when the named session **exists** | *As-built, changed during the verify pass and re-keyed by BUG-166 — sources: the 6-agent review of this REQ's implementation; the post-merge security re-verification.* Both legs of the spec's version were a same-UID **write primitive**: `session/list` is ungated, so any peer can enumerate session ids, and a refused preview — which writes nothing else at all — turned each call into a line published into a stranger's transcript, at whatever rate the caller liked. The preview therefore refuses silently, on `web/setup_plan`'s own cry-wolf rationale (a notice that fires on demand is one users read past, which costs the real one its attention), and the commit's notice is budgeted like `session_grant_minted` (`ConnState::may_announce_grant`). The verify pass keyed that budget on the connection alone and spent it before delivery; BUG-166 found both halves wrong — one refusal aimed at a session id naming *nothing* burned the connection's only notice on an audience of zero (silencing every real notice it owed afterwards), and a connection refused on session A then B announced only into A though B's user is a different reader. The budget is now keyed per (connection, session) and spent only for sessions the registry answers for, which also keeps phantom envelopes wearing attacker-chosen ids out of monitor streams and keeps the budget set bounded by daemon-minted ids. No arrears figure, unlike the grant budget's, because a suppressed repeat here is a byte-identical duplicate to the identical audience. **The intent holds**: BR-4's subject is something trying to *change* this session's capability, and each targeted session's own user now hears about each offending connection exactly once. Enforcement is untouched — every refused preview and every refused commit past the budget is still `NOT_ATTACHED`; only the announcement is bounded. Pinned by `server.rs`'s `a_refused_preview_is_silent_while_a_refused_commit_is_not`, `a_connection_announces_at_most_one_setup_rejection_per_session`, `a_refusal_against_a_second_session_announces_into_that_session_too`, and `a_nonexistent_session_buys_no_notice_and_burns_no_budget`. |

### ADR-2: The commit re-runs the startup path on a candidate, then swaps

**Decision**: `commit_web_setup` builds a candidate `Config` (current config
clone + new `[web]` table), runs the **same** `Config::validate()` the daemon
runs at startup, serializes with `Config::to_toml()`, writes via the
`persist_web_tier` atomic pattern, and only then swaps the daemon's in-memory
config mutex. Preview and commit derive their `[web]` rendering from the same
candidate serialization, so what the user confirmed is byte-wise what is
written (spec BR-7).

**Rationale**: spec BR-8 demands re-derivation, not in-place patching
(LESSON-501: carried state sheds invariants); reusing the startup validator is
what makes "the same load/validate path" literal rather than aspirational. The
config-mutex swap is the one commit seam every downstream consumer already
reads per turn.

### ADR-3: The secret's whole lifecycle stays in the client process

**Decision**: the search key is collected echo-off into memory, written to the
OS keychain by the CLI's existing `Keychain::store` (service `teton`, account
`web-search` → `keychain://teton/web-search`), and only the **reference**
appears in `web/setup_commit` params. A new `Keychain::delete` removes the
entry when the commit it was written for fails or is abandoned between store
and commit.

**Rationale**: the keychain is already asymmetric by design — CLI writes
(`teton provider add`), daemon reads at call time (REQ-544 BR-7 / ADR-007
binds the grant to the daemon's executable identity). Sending the secret over
the socket to let the daemon write it would create a second holder and a new
wire exposure for zero capability.

**Accepted residual (recorded honestly)**: a hard kill in the milliseconds
between `store` and the commit RPC can orphan a keychain entry. The window is
store→commit, not the whole flow (the store deliberately happens **after** the
user confirms the preview); an orphaned `teton/web-search` entry is inert
(no config references it) and is overwritten by the next completed flow. AC-6
is asserted at every user-reachable abort point; the kill-window residual is
this paragraph.

**Second accepted residual (added by the verify fix pass)**: a **transport**
failure during the commit RPC — socket closed, daemon died mid-call — leaves
the commit's outcome unknown: the write may or may not have landed. On that
path the flow deliberately performs **no** keychain undo (deleting would break
the setup in the landed case; restoring would un-rotate a key the landed
config now expects) and instead renders one honest notice naming the
`web-search` account, both possibilities, and `/web setup` as the check. When
the commit did *not* land, the stored entry is therefore orphaned — a
deliberate divergence from AC-6's letter on this one path, accepted for the
same reason as the kill window above: the ambiguous state licenses no
mutation, and an honest notice beats a clever guess. Pinned by
`a_commit_that_never_answered_leaves_the_keychain_alone_and_says_so`.

### Security-review finding 7 — disposition (added by REQ-575, 2026-08-14)

**Finding.** A same-UID process that breaks the ancestry chain with `setsid`
handshakes as `NotDescendant`, mints its own session via `session/create`
(auto-attach satisfies `may_drive`), and calls `web/setup_commit` — rewriting
`config.toml` and live-swapping the in-memory `[web]` config for every session
with no human in the loop. REQ-570's `refuse_unattested_commitment` guards
exactly this class for `model/confirm`/`model/set` (BR-10(b): daemon-wide
commitments need presence attestation) but was not applied to
`web/setup_commit` when REQ-572 added it.

**Disposition — CLOSED for `web/setup_commit` by REQ-575.** The commit is now the
third BR-10(b) commitment: it runs the same `refuse_unattested_commitment` check,
degrading (not refusing) where no presence mechanism exists, and moved off the
reader-loop `dispatch` onto the `blocks_on_a_human` task so the prompt cannot
stall the connection. Recorded at parity with `model/set`: real on presence
builds, stated-but-degraded elsewhere; it does **not** close the REQ-569 ADR-A
ancestry escape (BR-10(b) is the compensating control).

**Tracked residual — `config/set`.** REQ-575's validation surfaced that
`config/set` (`RegisterProvider` = an egress endpoint, `SetPrivacyBoundary` = the
privacy boundary itself) is a *larger* daemon-wide config writer still gated at
layer (a) only. It is the same class of finding, tracked in **REQ-576** (it
reverses a documented BUG-162 decision, so it takes its own spec/review). The
consent-path `persist_web_tier` is a documented low-severity residual (raise-only
within an already-configured `[web]` table), folded into REQ-576's scope.

### ADR-4: `capability_dead_end` fires where the daemon can actually see it

**Decision**: the event is emitted at the two daemon-observable dead ends —
the unserved-turn path when routing wanted a remote tier and none is
configured (`unserved_turn_error`), and the web tool's tier-gap refusals
(tool called above its granted/configured tier). A prose-only refusal by the
model with no tool call (web fully off — the tool does not exist) emits no
event; the per-state prompt clause **is** the mitigation there, pinned by the
AC-1 prompt tests.

**Rationale**: detecting that model-authored prose "was" a capability refusal
would be a second classifier over text (LESSON-456's exact warning). The spec's
Events row is refined to the honest set; AC-2's event assertion binds to the
remote-tier case, which is fully daemon-visible.

## Data model changes

None persistent. `WebConfig` is unchanged (the flow writes existing fields).
No `setup_in_progress` flag anywhere — there is no flow state to guard.

## Interaction with in-flight work

The refusal-wording investigation (chip session on the BR-6 opt-in gap) may
land changes to `WEB_OPT_IN_CLAUSE` / `turn_loop.rs` before this REQ merges.
Phase 7/8 rebase resolves textually; the per-state clause structure here
subsumes a strengthened single clause, and the AC-1 prompt-pin tests are
written against clause **content**, not exact bytes. BUG-165 (merged,
`d093ede`) makes `search_auth` a header template — the setup flow collects it
as an optional advanced answer and the AC-8 contract tests cover the shipped
suggestions (SearxNG keyless with `?format=json`; Brave/Kagi via their
`search_auth` templates).

## Proposed additions to `.adlc/context/architecture.md` (applied at wrapup)

- **Enablement is collection at the edge, commitment at the core** — a guided
  setup flow holds no server-side step state: clients collect and buffer
  answers (input buffering is not session state), the daemon exposes
  plan/preview/commit endpoints, validation is the startup validator run on a
  candidate, and the commit point is the config swap every consumer already
  reads. Secrets are written by the surface that collected them, by reference
  everywhere else (REQ-572 ADR-1/ADR-2/ADR-3).

## Task graph

```
TASK-127 protocol types ──┬─▶ TASK-129 runtime plan/preview/commit ─▶ TASK-130 server dispatch ─┐
TASK-128 core capability ─┤                                                                     ├─▶ TASK-132 CLI flow ─▶ TASK-133 tests ─▶ TASK-134 docs
  state (pure policy)     └─▶ TASK-131 prompt & exposure ───────────────────────────────────────┘
```

Tier 1: TASK-127, TASK-128 (parallel). Tier 2: TASK-129, then TASK-130 ∥
TASK-131. Tier 3: TASK-132. Tier 4: TASK-133. Tier 5: TASK-134.
