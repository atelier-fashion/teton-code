# REQ-586 — Architecture: a turn's context budget follows its route

_Drafted 2026-08-19 by `/architect` (under `/proceed`). Companion to
`requirement.md`; the ADRs below follow the style of
`.adlc/context/architecture.md`._

## Approach

One pure derivation, one owner, one projection. The budget a turn runs under
becomes a **property of the route attempt**: the router computes it in the
same place and the same shape it already computes the per-route effort
(`Router::effort_for` — "every Route this router builds calls this, and so
does the `teton effort` view — one function, so the event, the request and
the surface cannot disagree", router.rs:414-457). The derivation itself is a
pure function over plain data (window, cap, reservation, is-local,
redact-scan) in a new `harness/budget.rs`, table-tested without a router or a
daemon (architecture.md "Policy is pure, mechanism is gated"). The router
stamps the result into `HarnessConfig` (the pair every consumer already
reads — `CarriedTurn::begin`, the loop-top gate, the carry commit), into
`Route` (so `Route::route_decided()` projects `budget_tokens`/`bound` off the
route, never recomputed — ADR-D/ADR-G of REQ-559). `/doctor` and `/provider
list` render the **window field** the snapshot carries (AC-4); "defaulted"
and "inert cap" are rendered from that field client-side — two display
predicates over a daemon-owned fact, not a second derivation of the budget.

Three things that are today implicit become explicit seams:

1. **The context manager can be re-budgeted mid-turn.** `truncate_to_budget`
   grows a typed return (`PressureReport`) and the manager grows
   `rebudget(tokens, bytes) -> PressureReport`; the runtime's reroute arms
   (privacy block → local pin; provider failure → fallback) call it before
   the next attempt and emit `context_pressure { refit_on_reroute }`. The
   `Degrade` arm keeps the provider's budget because
   `degraded_harness_config` now derives from the **failed provider's**
   capability with only the tool-call tier overridden (the tracer's gotcha
   #1: today it builds from `CapabilityProfile::default()`).
2. **Pressure is news, not a flag.** `PressureReport` feeds a
   `SessionEvents::context_pressure(..)` emitter (the narrow typed-emitter
   pattern of `prefix_cache`/`capability_dead_end`, turn_loop.rs:358-414),
   the CLI renders one line never gated by `/verbose` (the `ContextCleared`
   precedent), and a newest-user-block elision is additionally a turn notice.
   The in-prompt elision marker stops hard-coding "local context window": the
   manager carries the route's window label and `truncate_to_budget` uses a
   marker-parameterized clamp while the six duty callers of `truncate_middle`
   keep theirs (gotcha #4).
3. **The redact chain is extended, not forked.** The scannable bound is a
   `const` expression over the same constants as `REDACT_TOTAL_CAP_CHUNKS`
   in `egress/redact.rs`, the test-only overhead assumption is promoted to a
   production constant with the same value, and the existing margin test
   keeps measuring the default (local) shape (LESSON-491: "write the chain
   down once and derive each number from its neighbor"; LESSON-456).

Everything user-visible rides additive protocol: two optional fields on the
wire `ProviderConfig` (so `/provider add`, `/provider setup`, `config/set`,
`/doctor` and `/provider list` all inherit the window through the one
`apply_update(RegisterProvider)` path — gotcha #6: merge field-wise, never
replace), two optional fields on `route_decided`, one new event, and a
`max_context` per vendor recipe. No `PROTOCOL_VERSION` move (the
REQ-573/REQ-559 rule).

## Data model / protocol changes

| Surface | Change | Additivity |
|---|---|---|
| `teton-core` `ProviderCapabilities` | `context_budget_cap: u32` (`#[serde(default)]`, 0 = none; `skip_serializing_if` zero so REQ-574 canonical renderings do not grow a line — `config_preservation.rs:831-843` lists the rendered keys) | config round-trip only |
| `teton-providers` `CapabilityProfile` | `context_budget_cap: u32`; `from_core`/`to_core` carry it (`core_roundtrip_is_lossless`) | internal |
| `teton-protocol` wire `ProviderConfig` | `max_context: Option<u32>`, `context_budget_cap: Option<u32>` (`#[serde(default, skip_serializing_if = "Option::is_none")]`). Daemon **always populates** `max_context` on the snapshot (`Some(0)` = unknown) — `None` means "older daemon" — the `RouteDecided.effort` rule | additive |
| `teton-protocol` `ProviderRecipeEntry` / daemon `ProviderRecipe` | `max_context: u32` (never 0 in the shipped catalog; contract test) | additive |
| `teton-protocol` `ProviderSetupCandidate` | `max_context: Option<u32>` (recipe default; the setup UI carries it silently) | additive |
| `teton-protocol` `RouteDecided` | `budget_tokens: Option<u64>`, `budget_bytes: Option<u64>`, `bound: Option<BudgetBound>` — daemon always populates | additive |
| `teton-protocol` new | `BudgetBound { Window, DefaultUnknown, RedactScan, UserCap, LocalEngine }` (snake_case); `ContextPressureKind { BlocksDropped, BlockElided, RefitOnReroute }`; `Event::ContextPressure(ContextPressure { kind, dropped_blocks: u64, elided_bytes: u64, newest_user_elided: bool, budget_tokens: u64, budget_bytes: u64, bound: BudgetBound })` — no `session_id` in the payload (envelope rule, events.rs:2048-2059); `Event::name()` `"context_pressure"` | additive |
| `teton-providers` `ProviderError` | `ContextLengthExceeded { provider_id }` — `failure_class() → None` (the `EffortRefused` shape, lib.rs:372-445) | internal |
| `tetond` `HarnessConfig` | unchanged field names for the pair; **new** `summarize_threshold_bytes`, `budget: RouteBudget` (bound + window label) | internal |
| `tetond` new `harness/budget.rs` | `RouteBudget { budget_tokens, budget_bytes, bound, window_label }`, `BudgetInputs { window, cap, reservation, is_local, redact_scan }`, `derive(BudgetInputs) -> RouteBudget`, the constants below | internal |

## Constants (one home each — LESSON-446/456)

| Constant | Value | Home | Pinned by |
|---|---|---|---|
| `REMOTE_TOKENS_PER_WORD` (safety ratio) | 3/2 (integer num/den) | `harness/budget.rs` | AC-3 corpus test (`max(words×3/2, bytes/2) ≥ tokens`) |
| `REMOTE_BYTES_PER_TOKEN_FLOOR` | reuse `DUTY_REQUEST_BYTES_PER_TOKEN` = 2 (harness/duty.rs:438) — not a third bytes-per-token number (gotcha #12) | `harness/duty.rs` | AC-3 corpus test |
| `LOCAL_BUDGET_TOKENS` / `LOCAL_BUDGET_BYTES` | 4,096 / 4,096 × `APPROX_BYTES_PER_TOKEN` — **the one home** of the default pair; `HarnessConfig::default()` reads them (no recursion into `derive`) | `harness/budget.rs` | existing + margin tests |
| `LOCAL_DIGEST_THRESHOLD_TOKENS` / `_BYTES` | 1,500 / 12,000 — the one home; the digest fraction is written as `LOCAL_DIGEST_THRESHOLD_* / LOCAL_BUDGET_*` (36.6%), default route byte-identical to today | `harness/budget.rs` | context.rs digest tests |
| `DIGEST_ABSOLUTE_CEILING` (OQ-7) | 20,000 words / 160 KiB — any single tool result above is digested on every route. Rationale: ≈ the largest single file a code task legitimately reads whole (a 160 KiB source file ≈ 4k lines); above it a raw fold displaces more conversation than it informs. A placeholder until TASK-183's corpus numbers say otherwise; the words ceiling binds on 200k, the bytes ceiling does not (145,734 < 163,840) | `harness/budget.rs` | context.rs test |
| `REDACT_SCANNABLE_CONTEXT_BYTES` | `(REDACT_INPUT_MAX_BYTES − REDACT_BODY_OVERHEAD_BYTES) × 10 / 11` ≈ 89,127 | `egress/redact.rs` | new assertion beside the margin test |
| `REDACT_BODY_OVERHEAD_BYTES` | 10 KiB, **promoted** from `#[cfg(test)]` to `pub(crate) const` | `egress/redact.rs` | existing margin tests unchanged |
| `COMPACT_PROMPT_BUDGET_BYTES` | the duty prompt budget for the local binding (same derivation shape as `REDACT_PROMPT_BUDGET_BYTES`: `(LOCAL_ENGINE_N_CTX − max_tokens) × DUTY_BYTES_PER_TOKEN − envelope`) | `harness/compact.rs` | compact.rs test |
| `COMPACT_PRESSURE_PERCENT` | 70 — unchanged | `harness/compact.rs` | existing |
| local default pair | (4,096 words, 32,768 B) — unchanged (OQ-3) | `HarnessConfig::default` | existing + margin tests |

## Derivation (BR-1/BR-2/BR-4/BR-5/BR-8), the one function

```
derive(inputs):
  if inputs.is_local                 → default pair, bound = LocalEngine
  elif inputs.window == 0            → default pair, bound = DefaultUnknown
  else:
    usable   = window − reservation (saturating; if 0 → default pair, DefaultUnknown)
    tokens   = usable × 2 / 3                      (REMOTE_TOKENS_PER_WORD)
    bytes    = usable × 2                           (REMOTE_BYTES_PER_TOKEN_FLOOR)
    bound    = Window
    if cap > 0 and cap < window:  window_eff = cap — the cap is a window ceiling:
                                    usable/tokens/bytes recomputed from window_eff; bound = UserCap
    if redact_scan and bytes > REDACT_SCANNABLE_CONTEXT_BYTES:
                                    bytes = REDACT_SCANNABLE_CONTEXT_BYTES; bound = RedactScan
                                    (applies LAST; words stay; the byte guard binds — BR-4)
  digest thresholds = fraction × (tokens, bytes), capped by DIGEST_ABSOLUTE_CEILING
  window_label = "the local context window" | "<id>'s context window (Nk)" | "the redact-scannable window"
```

Precedence is stated and tested pairwise: `LocalEngine` > `DefaultUnknown`
> (`RedactScan` when it bites) > `UserCap` > `Window` — i.e. the redact clamp
is applied last and names the bound when it binds, otherwise the cap, otherwise
the window. **Both currencies are surfaced**: on remote routes the byte guard
(2 B/token) is what binds for prose and code (≈3 B/word budget vs ≈5.7 B/word
prose), so `route_decided`, `context_pressure`, `/verbose` and the `context`
topic carry and print `budget_tokens` **and** `budget_bytes`; the word figure
alone would overstate what fits. `is_local` is classified by the router from
`CategoryTable::local_provider_id` (gotcha #9), never from "capabilities ==
default".

## Key decisions (ADRs)

### ADR-1: The router owns the budget, a pure module derives it, `Route` carries it

**Decision**: `harness/budget.rs` holds `derive()`; `Router::harness_config_for(id)`
(router.rs:830-836) becomes the one caller for routes, threading the result
into `HarnessConfig` (pair + `summarize_threshold_*` + `budget`); `Route`
gains `budget: RouteBudget` and `Route::route_decided()` projects
`budget_tokens`/`bound` off it. `Router` gains `with_redact_scan(bool)` (the
`with_local_available` builder shape), fed by `build_router(config)` from
`config.privacy.redact` — today the router never sees it (gotcha #2), and a
bound computed anywhere else would disagree with the gate `redaction_gate`
installs. The reservation is `HarnessConfig::default().gen_params.max_tokens`
(1,024), the same value the adapters send as `max_tokens`.
**Rationale**: `effort_for` is the precedent for "one per-route fact"; a
`RouteBudget` computed at route time is what lets `/verbose`, `/doctor`, the
event and every refusal read one value (BR-8, LESSON-456).
**Alternatives rejected**: deriving in `run_one_attempt` from `route.harness`
(a second place, and the snapshot for `/doctor` could not reach it); putting
the window on `HarnessProfile` (that struct is the *degradation* profile —
mixing a window into it would couple tier and budget).

### ADR-2: `Degrade` keeps the provider's budget; `Fallback`/`Retry` re-derive; reroute re-fits

**Decision**: `degraded_harness_config()` (router.rs:1167-1176) takes the failed
provider id and derives from `capability_of(id)` with `tool_call_tier` forced
to `Degraded` — so `max_context`/cap survive and the bound stays `Window`.
The runtime's two reroute arms (runtime.rs:2961 privacy block, 3009-3016
failure) call `conversation.ctx_mut().rebudget(route.harness.pair)` and emit
`context_pressure { refit_on_reroute }` with the report **before** `continue`.
**Rationale**: BR-1/AC-15; the tracer's gotcha #1 is exactly the regression a
naive derivation would ship.

### ADR-3: `truncate_to_budget` reports; the manager can be re-budgeted; the commit seam re-asserts

**Decision**: `truncate_to_budget() -> PressureReport { dropped_blocks,
elided_bytes, newest_user_elided }` (`#[must_use]`); `rebudget(tokens,
bytes) -> PressureReport` sets both budgets and runs the gate. The four call
sites (turn_loop.rs:595, 636, 748; carry.rs:248) each decide what to do with
the report: the loop gates emit via `SessionEvents::context_pressure`; the
carry commit returns it to the runtime, which emits it for the
between-turns drop (BR-10) — the commit itself stays event-free (LESSON-501:
the seam re-asserts the invariant; the news is published where the events
handle lives).
**Rationale**: BR-7 "never silent"; LESSON-501; a setter is the only way to
honor BR-1 without re-seeding (which would lose the turn's blocks).

### ADR-4: The elision marker is parameterized; the duty callers are untouched

**Decision**: `truncate_middle(text, room)` keeps its signature for
`classify.rs`, `triage.rs`, `title.rs`, `shell_duty.rs`, `compact.rs` and the
mechanical digest fallback; a sibling `truncate_middle_with(text, room,
marker)` is used by `truncate_to_budget` with the manager's
`window_label`; the marker lives inside the clamped block and is bounded by
`room` by construction — what a longer label touches is `truncate_middle`'s
`keep < 64` degenerate branch, which the clamp test covers. The `assemble` note
("truncated to fit the context window") is already window-neutral and stays.
**Rationale**: BR-7 / F-9; six duty callers must not change bytes.

### ADR-5: Digest thresholds scale as a fraction with an absolute ceiling; the compact prompt is bounded

**Decision**: `HarnessConfig` gains `summarize_threshold_bytes`; both
thresholds are derived in `budget.rs` as today's fraction (36.6%) of the
route pair, capped at `DIGEST_ABSOLUTE_CEILING`, and the default route stays
byte-identical (1,500 / 12,000). `summarize_if_large` takes both thresholds
(gotcha #3). `compact_prompt(blocks, prompt_budget_bytes)` offers the
**oldest** blocks up to `COMPACT_PROMPT_BUDGET_BYTES` (the duty's own prompt
must fit the local engine — the `SUMMARIZER_INPUT_MAX_BYTES` precedent,
context.rs:1489-1497); a partial offer still compacts because the answer is
block numbers; `COMPACT_OUTPUT_MAX_BYTES` keeps its pin to the default byte
budget (the summary's size is about the *local* tier's window, not the
route's).
**Rationale**: BR-6; Phase-1 F-2/F-3.

### ADR-6: Scannable bound derived in `egress/redact.rs`; overhead constant promoted

**Decision**: `pub(crate) const REDACT_SCANNABLE_CONTEXT_BYTES` next to
`REDACT_INPUT_MAX_BYTES`, written as the expression in the table above;
`REDACT_BODY_OVERHEAD_BYTES` loses `#[cfg(test)]` (value unchanged); the
arithmetic doc blocks (redact.rs:305-369) gain the bound's derivation; a
new assertion beside `the_total_cap_clears_the_harness_context_budget_with_margin`
pins `2 × (SCANNABLE + OVERHEAD) … ≤ INPUT_MAX` is **not** required (the
bound is cap-minus-overhead by decision) but `SCANNABLE + OVERHEAD + escaping
≤ INPUT_MAX` is, and that changing either constant alone fails. The margin
test keeps measuring `for_strong_model()`'s default pair (AC-13).
**Rationale**: BR-4/BR-11; LESSON-491.

### ADR-7: Additive wire fields, field-wise merge, daemon-populated `Option`s

**Decision**: as in the table. `apply_update`'s `RegisterProvider` arm
(runtime.rs:9936-9964) merges `max_context`/`context_budget_cap` field-wise:
`Some(v)` writes, `None` preserves the stored value (an older client's
re-registration cannot zero a declared window). The snapshot projection
(runtime.rs:9785-9791) always emits `Some(max_context)` (0 = unknown). The
setup commit (`derive_provider_setup`, runtime.rs:5357-5366) writes the
candidate's window (recipe default). A `context_budget_cap` above the window
is **inert, not invalid**: `derive()` takes the minimum, so it cannot bind,
and `Config::validate` stays structural-only (REQ-557 ADR-E); `/doctor` notes
an inert cap as an advisory line (the usability pass), never a startup
refusal.
**Rationale**: BR-3/BR-5; REQ-573's additive rule; gotcha #6.

### ADR-8: `ContextLengthExceeded` is a typed, class-less provider error

**Decision**: `ProviderError::ContextLengthExceeded { provider_id }` with
`failure_class() → None`; adapters recognize it in `classify_client_error`
(lib.rs:291-308) by a **narrow** body-head sniff of the exact vendor
spellings (OpenAI-compatible: `"code":"context_length_exceeded"` /
`maximum context length`; Anthropic: `prompt is too long`) — the
`body_names_the_effort_field` posture, never a general parse;
`RemoteProviderSource::produce_turn` maps it to
`HarnessError::ContextLengthExceeded { provider_id, assembled_tokens,
budget_tokens }`; `run_prompt_turn` gains an arm before `Remote(perr) if
attempts < 2` that ends the turn with a typed `RpcError` naming the window
and the assembled size, records **no** health change and runs **no**
`on_provider_failure`.
**Rationale**: BR-2; REQ-581's typed-outcome rule; `EffortRefused` precedent.

### ADR-9: CLI surfaces

**Decision**: `render_config` (main.rs:3624-3665) appends `window: 128k` /
`window: unknown — context budget defaulted (set capabilities.max_context)`
/ `window: not reported` (field `None` = older daemon); `doctor_report_on`
inherits it and adds one advisory line per unknown-window provider (the
`advise_on_base_url_endpoints` shape); `ProviderAction::Add` gains
`--max-context <tokens>` and `--context-budget-cap <tokens>`;
`provider_setup_ui` carries `entry.max_context` into the candidate silently
(OQ-1 lean); `format_route` (session_ui.rs:2264-2284) appends
`· budget N words (bound: x)` under `/verbose`; `Event::ContextPressure`
renders one line unconditionally (`context: N older blocks dropped to fit the
M-word budget (bound: …)` / `…newest message middle-elided by K bytes…`).
**Rationale**: BR-3/BR-7/BR-9; REQ-582 one-renderer rule.

### ADR-11: Budget vocabulary ships as its own `teton_docs` topic

**Decision**: a fifth bundled topic `context` (`crates/tetond/src/harness/docs/context.md`,
≤ 4 KiB) carries the budget/window/bound vocabulary, the `context_pressure`
line, `capabilities.max_context` / `context_budget_cap`, and the worst-case
per-prompt input; `TOPICS`, `TOPIC_INDEX` and `DESCRIPTION` gain the one word
(docs.rs:68-83, 127-130 — the BR-10 "a fifth topic costs one word" design).
`providers.md` is at **4,050 of its 4,096 B** ceiling, so it gains at most a
≤ 40 B pointer (`Windows: teton_docs context.`) — or nothing — and the window
sentence lives in the `context` topic. The resident
guide (`self_config.md`) is **not** touched — its headroom is BUG-181's.
**Rationale**: REQ-577 ADR-3 (depth lives in the tool, not the prompt);
`every_bundled_topic_is_under_the_ceiling`.

### ADR-10: AC-3's corpus is a committed fixture, not a runtime or dev dependency

**Decision**: `crates/tetond/tests/fixtures/token_corpus/{prose.txt, rust.rs,
minified.json, paths.txt, base64.txt}` plus `token_counts.json` generated
once by `tools/token_corpus/count.py` (tiktoken `o200k_base`, documented),
and a test that asserts `max(words×3/2, bytes/2) ≥ tokens` per sample and
that the fixture's counts still match its files' word/byte counts (so a
stale fixture is a red test).
**Rationale**: no network or heavy dev-dependency in the test graph; the
spec allows either; LESSON-460 governs fixture fidelity — the generator is
checked in and the counts are reproducible.
**Measured (TASK-183, tiktoken 0.14.0 `o200k_base`)**: prose 4.59 B/token,
Rust 4.01, minified JSON 3.58, paths 3.58, random base64 **1.45**. Decision:
the 2 B/token floor stays — base64 is the documented uncovered class
(`KNOWN_UNCOVERED_AT_PINNED_FLOOR`, asserted both ways), bounded by the
digest threshold and backstopped by the typed `context_length_exceeded`
outcome; lowering the floor to 1.45 would shrink every prose/code prompt's
remote byte budget by ≈25% to protect content the harness rarely carries raw.
The word ratio 3/2 is pinned only by "prose is covered by words alone"
(mutation 3/2 → 1/1 is caught there).

## Tracer gotchas the tasks cite

1. `degraded_harness_config()` builds from `CapabilityProfile::default()` (router.rs:1167-1176) — a naive derivation gives Degrade `DefaultUnknown`.
2. `harness_config_for` takes only a provider id; the router never sees `[privacy] redact` (read solely in `redaction_gate`, runtime.rs:3994-4010) nor `max_tokens`.
3. `summarize_if_large`'s byte twin is `threshold_tokens × APPROX_BYTES_PER_TOKEN` (context.rs:1620).
4. Three hard-coded "local context window" strings: `truncate_middle`'s marker (context.rs:1505), shared by the clamp, the mechanical digest fallback (1633) and `compact_prompt` (compact.rs:300).
5. `REDACT_BODY_OVERHEAD_BYTES` is `#[cfg(test)]` (egress/redact.rs:284); `REDACT_TOTAL_CAP_CHUNKS` is private.
6. `apply_update`'s `RegisterProvider` arm replaces the whole `ModelProvider` and preserves capabilities by lookup (runtime.rs:9946-9958) — merge field-wise.
7. `format_route` does not render `effort` (session_ui.rs:2264-2284); `render_config` (main.rs:3624) is the single row renderer for `/doctor` and `/provider list`.
8. `CompactionOutcome`/`truncate_to_budget` return no counts; `truncate_to_budget()` is called at four sites (turn_loop.rs:595, 636, 748; carry.rs:248) and the carry commit has no `SessionEvents`.
9. The local tier is absent from `Router.providers` — classify `local_engine` from `table.local_provider_id` as `effort_for` does.
10. `route.harness.web_capability`/`session_root` are stamped only on the first route (runtime.rs:2877-2883).
11. `ProviderRecipe`/`ProviderRecipeEntry` are pinned field-for-field and by a hand-written golden.
12. `APPROX_BYTES_PER_TOKEN = 8` (word bridge) vs `DUTY_REQUEST_BYTES_PER_TOKEN = 2` (BPE estimate) are two different numbers — the byte floor reuses the duty constant.

## Proposed additions to `.adlc/context/architecture.md`

- Key Pattern: **A per-route fact is derived once, where the route is
  decided, and every surface reads that value** — the budget joins effort as
  the second instance (`RouteBudget` beside `ResolvedEffort`); a fact that
  changes with a mid-turn reroute is re-derived and re-applied before the
  next call, with the change published as news.
- ADR-006 consequence note (line ~491): "budget/window currency compatibility
  is now per route: the remote pair derives from the provider window with
  pinned allowances; the local pair is unchanged."

## Task graph

```
Tier 0 (parallel, no deps)
  TASK-181 protocol+core+capability types (wire fields, event, enum, recipe entry, config cap)
  TASK-183 token corpus fixture + generator + AC-3 pin
  TASK-184 egress/redact scannable bound + promoted constant + assertions
  TASK-185 providers ContextLengthExceeded (error, sniff, class-less) + conformance
Tier 1
  TASK-182 harness/budget.rs pure derivation + constants + table tests  [181,184]
  TASK-188 recipes window + apply_update merge + setup commit + snapshot projection  [181]
Tier 2
  TASK-186 router: budget_for/harness_config_for/degraded(id)/with_redact_scan/Route/route_decided  [182]   (188 lands first — both touch runtime.rs in disjoint regions)
  TASK-187 context manager: PressureReport, rebudget, marker label, digest thresholds, compact prompt bound  [182]
  TASK-190 CLI: --max-context, window column + doctor advisory, verbose budget line, context_pressure line, setup candidate; cli_e2e  [181,188]
Tier 3
  TASK-189 runtime wiring: reroute refit + events, ContextLengthExceeded arm, SessionEvents emitter, notice, carry report; remote-loop + routing + carry unit tests  [185,186,187]
Tier 4
  TASK-193 e2e/integration fixtures: privacy_fixes refit (AC-15a), ac_matrix fallback (AC-15b), conversation_carry AC-11, redact_egress AC-6, AC-10 daemon emissions  [189]
Tier 5
  TASK-191 docs: context topic, doctor/providers pointer, README, CHANGELOG, manual-verification, architecture.md pattern, recorded headroom  [193,190]
Tier 6
  TASK-192 verification sweep: workspace --no-fail-fast, margin tests, mutation checks (a–i), one-home grep, guide headroom untouched  [191]
```
