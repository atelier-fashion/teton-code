# REQ-568 — Architecture: Session-scoped event delivery and bounded request frames

## Approach

Four seams, all in existing files — no new modules, no wire-format changes
beyond one additive handshake field and one new error code.

1. **Per-connection session state** (`handle_client`, server.rs): a
   `ConnState` — `attached: Arc<RwLock<HashSet<SessionId>>>` plus an immutable
   `monitor: bool` fixed at handshake. It lives alongside the existing
   per-connection locals (`handshaked`, `client_guard`, `fence`). The dispatch
   path mutates `attached` (create auto-attaches the creator, attach inserts on
   success); the forwarder task holds a clone and reads it per envelope.
   `session/clear` does NOT detach — attachment is connection-lifetime, a
   transcript clear is content-lifetime.

2. **Filter at the forwarding seam** (`forward_events`, server.rs): deliver an
   envelope iff `session_id.is_none()` (daemon-scoped) OR `monitor` OR
   `attached.contains(sid)`. The decision is a pure function
   (`should_forward(envelope_session, attached, monitor) -> bool`) with a
   table-driven unit test, called from the forwarder loop ("policy is pure,
   mechanism is gated").

3. **Attachment gate inside the dispatch seam** (server.rs): `dispatch` (and
   `spawn_prompt_turn`, which bypasses `dispatch`) gain a `&ConnState`
   parameter; `session/prompt` and `session/clear` refuse with `NOT_ATTACHED`
   before touching the runtime. The gate is below `handle_client` so the
   direct-RPC tests exercise it — a gate above `dispatch` would be the BUG-155
   CLI-only mistake (LESSON-484).

4. **Bounded frames** (`handle_client` read loop, server.rs): the read goes
   through `(&mut reader).take(MAX_FRAME)` so the line buffer is *incapable*
   of exceeding the cap — a post-read `line.len()` check would bound nothing,
   the memory is already spent. Limit-hit (buffer full, no trailing newline)
   → best-effort `INVALID_PARAMS` with null id, then close the connection.

## Key decisions

### ADR-A: Filter in the forwarder, not the bus; skipped envelopes advance the fence

The `EventBus` stays connection-agnostic (`publish(Option<SessionId>, Event)`
unchanged); per-connection state never enters broadcast.rs. `forward_events`
counts every envelope it drains — sent or skipped — into the `forwarded`
watermark, exactly as the existing serialization-failure arm already does
("counted even when serialization failed: the event will never be sent, so
nothing should wait on it"). `EventFence::sync` waits for
`forwarded >= delivered`, where `delivered` counts envelopes queued into this
connection's subscription channel; since the forwarder processes everything
delivered, the invariant is untouched and BR-7 holds with zero fence changes.
**Consequence**: envelope `seq` is assigned bus-side at publish, so a filtered
client observes monotonic but non-contiguous `seq`. Nothing may assume
contiguity; the scoping test pins gap tolerance.

### ADR-B: `NOT_ATTACHED` (-32009) is distinct from `UNKNOWN_SESSION` (-32001)

Prompt/clear against an existing-but-unattached session → `NOT_ATTACHED`;
against a nonexistent session → `UNKNOWN_SESSION` (unchanged). Distinct codes
per the BUG-152 rule (one classifier, daemon-side; clients render from codes).
No session-existence oracle is created: `session/list` already enumerates
sessions to any handshaked client (REQ-569 OQ-4 owns tightening that). The
code lands in the `application_error_codes!` macro so the existing uniqueness
guard covers it.

### ADR-C: Monitor is a handshake-time declaration, immutable per connection

`HandshakeParams` gains `monitor: bool` with `#[serde(default)]` (the
`SessionCreateParams.cwd` optional-field pattern) — old clients omit it,
deserialize to `false`, and keep today's behavior minus the leak (they create
→ auto-attach → see their own session). No capability negotiation: filtering
is unconditional in the daemon, never advertised or adapted per client.
Immutability keeps the forwarder free of a second shared mutable; a client
that wants to stop monitoring reconnects. BR-5 observability: the daemon logs
the declaration at handshake (client name/kind + "monitor").
`negotiate_from` and version admission are untouched.

### ADR-D: MAX_FRAME = 4 MiB; refuse-then-close, no resync

Measurement basis: the largest legitimate frame is `session/prompt` carrying
pasted text; observed prompts sit well under 100 KiB and every other method is
sub-KiB, so 4 MiB is ~40× headroom while bounding per-connection buffer
memory. Resolves spec OQ-2. On limit-hit the daemon sends the refusal
(best-effort `try_send`) and tears the connection down rather than resyncing
to the next newline (resolves OQ-3): a legitimate client never hits the cap,
an attacker reconnects either way, and discard-until-newline is complexity
with no honest beneficiary. AC-5's "fresh connection still serves" documents
the recovery path.

### ADR-E: CLI renders only its own session (defense in depth, not a control)

The CLI pump filters envelopes against `Context.session_id` before calling
`render_event`; `render_event` stays a pure function with no filtering inside.
This is AC-8 defense-in-depth atop the daemon filter (BR-3) — never a
substitute. Daemon-scoped envelopes (`session_id: None` — model lifecycle,
daemon lifetime) still render.

## Data model changes

None persistent. Protocol: `HandshakeParams.monitor` (additive, defaulted),
error code `-32009 NOT_ATTACHED`. `EventEnvelope` unchanged.

## Deliberate test-contract change

`multi_client.rs::two_clients_share_sessions_and_daemon_survives_client_exit`
today asserts client B receives client A's `phase_transition` — the leak
encoded as a feature. It is rewritten: B still shares the session *registry*
(list) and the daemon still survives client exit, but B sees A's events only
after `session/attach` (or with `monitor: true`). This is a planned BR-8
carve-out: multi-client *event* flows change by design; single-client flows
are byte-identical.

## Publisher audit (spec assumption discharge)

All 32 `publish(...)` call sites enumerated. Every `None`-scoped publish is
genuinely daemon-scoped (DaemonLifetime, ModelLifecycle,
ModelSelectionProposed/Decided, DaemonClientAttach, install progress); every
session-output-bearing event passes `Some(session_id)` (runtime, harness,
router, cost, egress, permissions). No publish site needs re-scoping.

## Task graph

```
Tier 1 (parallel, disjoint files):
  TASK-097 protocol: monitor field + NOT_ATTACHED code   [teton-protocol]
  TASK-100 daemon: MAX_FRAME bounded reader              [tetond server.rs + tests/frame_cap.rs]
  TASK-101 cli: own-session render filter                [teton]
Tier 2:
  TASK-098 daemon: ConnState + forwarder filter          [tetond server.rs + tests/multi_client.rs]  deps: 097
Tier 3:
  TASK-099 daemon: attachment gate on prompt/clear       [tetond server.rs]                          deps: 098
Tier 4:
  TASK-102 integration: scoping e2e + fence variants     [tetond tests + e2e harness]                deps: 099, 100, 101
```

Tier-parallel tasks touch disjoint files by construction (097: protocol crate;
100: server.rs read loop + own test file; 101: teton crate) — 098 and 100 both
touch server.rs but sit in different tiers, sequenced.
