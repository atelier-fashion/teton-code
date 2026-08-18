# REQ-581 — Architecture: A first-class provider connection test

## Approach

Copy the shape of a harness **duty** for the call, the shape of the REQ-579
**setup trio** for the RPC and its gates, and the shape of REQ-579's
**hand-off** for the session. Nothing here is a new network path, a new
consent primitive, or a new event style — the whole REQ is one daemon method
that performs the smallest existing remote call and reports it typed.

- **Daemon (`tetond/src/runtime.rs`)** — `DaemonRuntime::provider_test(events,
  session_id, provider_id) -> Result<ProviderTestResult, RpcError>`. It
  resolves the `[[providers]]` entry, refuses a `kind = "local"` provider with
  the tier's own state sentence (BR-8), builds the adapter and the
  credential-bound transport exactly as `run_one_attempt` does
  (`build_provider` + `build_remote_transport`), wraps the transport in
  `Egress::new(...).with_cost_meter(ledger)`, and streams **one** fixed
  `TurnRequest` — one user message, no system prompt, no tools, `max_tokens`
  at the small fixed floor — through `egress.scoped(Provenance::empty(), ctx)`
  with a `CostAttribution` marked `probe`. It measures wall time to the
  stream's end, reads usage off `Completed`, maps success or the typed
  `ProviderError` to a `ProviderTestOutcome`, moves the health map through the
  same two calls a turn uses (`HealthRecord::healthy()` /
  `health_record_after_failure`), publishes `Event::ProviderTested`
  session-scoped, and returns.
- **Server (`tetond/src/server.rs`)** — `provider/test` joins the own-task
  method list (the `blocks_on_a_human` branch — here it blocks on the network,
  same reason: never park the reader loop, LESSON-518). Gates in the setup
  trio's order: `refuse_unmintable_session_id`, then `may_drive` → silent
  `NOT_ATTACHED` in-response with **no** event (LESSON-513: a read/probe
  refusal announces nothing; only commits do). No presence attestation — this
  changes no config; the consent is the client-side confirm (BR-2) and the
  foreign-caller gate.
- **Protocol (`teton-protocol`)** — `ProviderTestParams { session_id,
  provider_id }`, `ProviderTestResult { provider_id, model, dial_host,
  outcome, health_after }`, `ProviderTestOutcome` (tagged enum, ADR-2),
  `Event::ProviderTested { provider_id, outcome, health_after }`
  (`provider_tested`), plus two additive fields: `CostRecord.probe: bool`
  (serde default) and `CostReportView.probe_calls: u64` (serde default).
- **Ledger (`tetond/src/cost`)** — a nullable `probe INTEGER` column via the
  existing `ADDITIVE_COLUMNS` migration; `CostAttribution` gains a `probe`
  flag the egress meter copies onto the row; `report()` counts probes.
- **CLI (`teton`)** — a new `provider_test_ui.rs` (one flow, one seam,
  REQ-579's `SetupIo` shape): preview from the `config/get` snapshot →
  `[y/N]` (or `--yes`) → `provider/test` → typed report lines. Two call
  sites: the `/provider test <id>` `COMMANDS` row and the `teton provider
  test <id>` clap subcommand (which opens a session first — the method is
  session-gated, and the cost row needs one). `teton cost` renders
  `probe_calls`. `render_event` gets a `ProviderTested` arm.
- **Session hand-off (`teton/src/session_ui.rs`, `tetond/src/harness/
  self_config.md`)** — one guide sentence naming `/provider test <id>`, pinned
  by the recipe-catalog contract test's `drift()` pattern; a second hand-off
  line keyed on a connection-question turn (ADR-4).
- **Tests** — `MockProvider` learns to answer an arbitrary status
  (`MockResponse::status(u16)`) so the e2e can drive 401/404/429; a new
  `tetond/tests/provider_test_flow.rs` covers reached/refused/unknown-model/
  rate-limited/unreachable/NOT_ATTACHED over the socket; runtime unit tests
  cover health movement and the ledger row; CLI unit tests cover the preview,
  the decline (zero calls), the report lines and the hand-off predicate.

## Data model changes

**Ledger (SQLite, `cost_records`)** — one additive nullable column:

| Column | Type | Meaning |
|---|---|---|
| `probe` | `INTEGER` (nullable) | `1` for a connection-test row; `NULL` for every turn and every row written before this REQ (the honest value: the concept did not exist) |

Migration is the existing `ADDITIVE_COLUMNS` path (`ALTER TABLE … ADD
COLUMN`, DDL, no row rewritten, append-only trigger untouched — REQ-564's
`cached_tokens` precedent).

**Protocol (additive)**:

| Type | Direction | Fields |
|---|---|---|
| `ProviderTestParams` | C→D | `session_id`, `provider_id` |
| `ProviderTestOutcome` | D→C | `reached { latency_ms, input_tokens, output_tokens, usd_micros: Option<i64> }` \| `refused { status, reason }` \| `unknown_model { status, reason }` \| `rate_limited { retry_after_secs: Option<u64> }` \| `server_error { status, reason }` \| `unreachable { reason }` — `#[serde(tag = "outcome", rename_all = "snake_case")]` |
| `ProviderTestResult` | D→C | `provider_id`, `model`, `dial_host` (the dial-time reading, LESSON-529), `outcome`, `health_after: ProviderHealth` |
| `Event::ProviderTested` | D→clients | `provider_id`, `outcome`, `health_after` — session scope from the envelope (the ProviderSetupCompleted precedent: no payload `session_id`) |
| `CostRecord.probe` | D→C | `bool`, `#[serde(default, skip_serializing_if = "not")]` — an old client reads the same bytes it always did |
| `CostReportView.probe_calls` | D→C | `u64`, `#[serde(default)]` |

`reason` in every failure variant is the **daemon's own sentence built from
the status and the dial host** — never a response body, never a header, never
the credential value (ADR-3). The credential *reference* may appear in the
`refused` reason (`keychain://teton/kimi`), which is what AC-2 asserts.

## API changes

- New method `provider/test` (params/result above). Errors: `INVALID_PARAMS`
  for an unknown provider id or a `kind = "local"` provider (the message
  carries the local tier's state sentence for the latter, BR-8);
  `NOT_ATTACHED` for a connection that may not drive the session;
  `CONFIG_REJECTED` when the credential reference cannot be resolved locally
  (nothing was sent — that is a config problem, not an outcome).
- New CLI surfaces: `/provider test <id>` (in-session) and `teton provider
  test <id> [--yes]` (shell). Both render through one module.
- `teton cost` gains one line when `probe_calls > 0`.

## Service layer

```
CLI (/provider test | teton provider test)
  └─ provider_test_ui::run(io, provider_id, auto_yes)
       ├─ config/get  → preview: id, kind, model, stored endpoint; "[y/N]"
       ├─ provider/test (session-scoped) ──────────────────────┐
       └─ render ProviderTestResult (typed) + health + routing  │
                                                                ▼
Daemon server: own-task handler → refuse_unmintable_session_id → may_drive
  └─ runtime.provider_test
       ├─ find provider; local kind → INVALID_PARAMS with tier state (BR-8)
       ├─ build_provider + build_remote_transport (auth_ref → header)
       ├─ Egress::new(transport, boundaries, events).with_cost_meter(ledger)
       ├─ stream_turn(fixed TurnRequest, egress.scoped(empty provenance, ctx{probe}))
       ├─ outcome ← Ok(usage, latency) | ProviderError (ADR-2 mapping)
       ├─ record_health(healthy | health_record_after_failure(class))
       └─ publish Event::ProviderTested (session-scoped); return result
```

## Key decisions

### ADR-1 — The probe is a duty-shaped call, written once beside `run_one_attempt`, not a `Duty`

`RemoteDuty::perform` is exactly the request shape wanted (one message, no
tools, fixed budget, `CostAttribution` + `EgressContext` + `egress.scoped`) —
but it flattens every failure to a `String`, and this REQ's whole value is the
*typed* failure. So `provider_test` builds the same request through the same
three constructors the turn path uses and keeps the `ProviderError`. It does
**not** run the turn loop (no tools, no context, no retries, no fallback: a
test that fell back to another provider would be testing the wrong one) and
does not attach the redaction gate (the payload is a constant sentence with no
user content — there is nothing to scan; documented at the call site so the
omission reads as a decision).

### ADR-2 — Outcomes are derived from status and transport class only; `Retry-After` is not carried in v1

`ProviderError::ClientError { status }` carries a status and nothing else, and
`TransportResponse` surfaces exactly one named header (`location`) — its own
doc names "just read one more header" as the surface it refuses to grow, and
~40 sites construct the struct literally. Carrying `Retry-After` for a
connection test is not worth that. Mapping (the classifier `FailureClass`
already draws these lines for retry/fallback; this names them for a person):

| Signal | Outcome |
|---|---|
| stream completed | `reached` (latency = request start → stream end; tokens from `Completed`; `usd_micros` from the price table when the model is priced) |
| 401, 403 | `refused` — "HTTP 401 from `<host>` — the vendor did not accept the credential at `<auth_ref>`" |
| 404 | `unknown_model` — the endpoint exists (registration validated it), so the missing thing is the model the config declares |
| 429 | `rate_limited { retry_after_secs: None }` — "rate limited by `<host>`; try again shortly" |
| other 4xx | `refused { status }` |
| 5xx | `server_error { status }` — the vendor answered and is failing |
| Timeout / Transport / MalformedResponse | `unreachable` — "could not reach `<host>`: <class>" |
| `EffortRefused` | cannot occur — the probe sends no effort field (`ResolvedEffort` omitted) |
| `PrivacyBlocked` | cannot occur — empty provenance, constant payload |

**Amends spec AC-3**: `rate_limited` carries `retry_after_secs` only when the
transport surfaces it, which v1's does not by design; the AC's "carrying 7" is
recorded as deferred with this ADR as the reason, and the report line says
"try again shortly". `unknown_model` on a 400 is *not* attempted (a 400 stays
`refused { 400 }`), per the spec's own assumption.

### ADR-3 — `reason` is composed by the daemon from status + dial host, never from the vendor's body

The adapters read an error body head today only to detect the effort
refusal, and `ProviderError` deliberately carries no body ("conventions.md
forbids content in error messages"). A vendor body can echo the request. The
test's sentences therefore name the status, the dial host (`canonical_host_and_
port_of`, LESSON-529's reading), the model from config, and the credential
*reference* — all daemon-owned facts. A user who wants the vendor's exact words
has the status to look them up; the product does not put a third party's
prose into its transcript.

### ADR-4 — The hand-off keys on the *turn* (prompt + tool activity), not only the reply, and prints its own one line

REQ-579's nudge keys on the reply reciting `teton provider add` — a recipe.
The observed failure mode here was different: the model *ran* `teton provider
list` / `teton policy show` through the `shell` tool and misread them; its
reply text recited little. So `SessionState` records, per turn, the user's
prompt (`begin_turn(prompt)`) and the tool calls the turn made (from the
`session_update` tool-call payloads it already renders). The predicate:

> the prompt reads as a connection question (`test|check|verify|working|
> connected|reach` ∧ `provider|connection|connectivity|api|<a registered
> provider id>`, case-insensitive) **and** the turn either recited a
> `teton provider|policy|doctor` diagnostic or ran the `shell` tool on a
> `teton …` command **and** the reply did not name `/provider test`

prints, once, `in this session, /provider test <id> makes one consented call
and reports what came back; that is the connection test.` — TTY only, after
the reply, through the same `hand_off_after_turn` entry (which now decides
between the two lines; the setup line's own predicate is unchanged). This is a
heuristic and is labelled one: AC-8b is claimed only after a live A/B
(LESSON-532), recorded in `docs/manual-verification.md`. The registered
provider ids come from the `config/get` snapshot the CLI already caches
(REQ-560), so "kimi" in the prompt counts without a hard-coded vendor list.

### ADR-5 — The shell subcommand opens a session

`provider/test` is session-gated (`may_drive`) so a foreign connection or a
tool-spawned `teton provider test … --yes` (a daemon descendant, excluded from
session access by REQ-569's ancestry gate) cannot make the user's provider
spend on their behalf. `teton provider test <id>` therefore creates a
freeform session, runs the same flow, and lets the session end with the
connection — which is also what gives the cost row a `session_id`. This is
the one place a "read-ish" subcommand opens a session, and it is because the
command is *not* a read: it sends.

## Test economy

| Layer | Proves |
|---|---|
| protocol unit | wire names, tagged outcome variants, additive `probe`/`probe_calls` round-trip and old-shape tolerance |
| tetond unit (`runtime::tests`) | outcome mapping table (each `ProviderError` → variant), health movement (`Unavailable` → `healthy` after `reached`; failure stamps cooldown), ledger row with `probe = 1` on `reached` and none on failure, local-kind refusal, event scope |
| tetond unit (`server::tests`) | foreign connection → `NOT_ATTACHED`, no event, `MockProvider` request count 0 |
| tetond e2e (`provider_test_flow.rs`) | reached (200 with usage) / 401 / 404 / 429 / closed port over the socket against the real daemon; `teton cost` counts one probe |
| teton unit | preview lines; `n` → zero RPCs; `--yes` skips; each outcome's report line; hand-off predicate table + non-TTY silence; `/help` row |
| contract | guide names `/provider test <id>` (`drift()` pin) |
| manual | AC-8b live A/B; one real `reached` against Kimi (runbook section) |

## Proposed additions to `.adlc/context/architecture.md`

- Under Key Patterns: *"A user-invoked probe reuses the call path it probes"* —
  a connection test that took a shortcut (a `/models` list, a HEAD) would
  prove reachability of an endpoint the product never POSTs to; the test is
  the real request, minimal, typed on the way back, and moves the same health
  the router reads. Corollary: outcomes are named from facts the product owns
  (status, dial host, configured model, credential reference), never from a
  third party's prose.
