# REQ-544 Phase 5 — Consolidated Review Findings

Six agents (reflector, correctness, quality, architecture, test-coverage,
security). Deduped, ranked. "Confirmed by N" = independent agents converging.

## CRITICAL

### C-1 — BR-1 privacy bypass via shell/grep/glob + non-path MCP results
Confirmed by reflector, security (C-1), correctness (3 independent). Egress
enforcement is provenance-based, but provenance is derived from a tool's
literal `path` argument (`path_arg` in turn_loop.rs:439), NOT the files a tool
actually touches. `shell {command:"cat secrets/prod.env"}`, `grep`, `glob`, and
local-MCP results whose boundary arg isn't named `path` fold into context with
EMPTY provenance → egress inspects nothing → boundary content reaches the
remote provider on the next turn, no `privacy_block`. Exploitable with the
SHIPPED fixture `tests/e2e/fixtures/demo_repo/secrets/prod.env`. The correct
helper (`result_context_block`/`call_provenance`) exists but is dead/test-only.
Only `read`-with-`path` and remote-MCP paths are enforced (why tests pass).
→ Files: harness/completion.rs:291-302, turn_loop.rs:439/631, tools/{shell,grep,glob}.rs, tools/mcp.rs:86

### C-2 — Documented session-taint backstop does not exist
Confirmed by reflector, security (H-2), correctness, architecture, test-audit.
egress/mod.rs:35-47 claims the residual-leak mitigation is "a session that
touched local-only stays on the local tier — router enforces above." No such
code: Router has no per-session taint state. The safety net for C-1 is absent
and the doc overclaims.
→ Files: egress/mod.rs:35-47, router.rs (missing)

## MAJOR

### M-1 — Privacy block misclassified as transient → retried 2×, false event
Confirmed by reflector, security, correctness, test-audit. PrivacyBlocked →
TransportError::Connect → FailureClass::Transport → Retry(retryable). Runtime
retries the SAME blocked provider up to 2×, emitting up to 3× `privacy_block`.
Event advertises `action: ReroutedToLocal` but daemon NEVER reroutes — ends in
generic INTERNAL_ERROR. (No cost double-count — blocked call returns before
metering.)
→ Files: runtime.rs:394-424, egress/mod.rs:114-120, providers/failure.rs:73, router.rs:344

### M-2 — Built-in tool results not framed untrusted (prompt injection)
Security (H-3). MCP results get an `<untrusted>` envelope; `read`/`grep`/`shell`
output is folded raw. Injection text in a repo file + an allowlisted `shell`
grant → model acts on injected instruction, fires shell with NO user prompt.
→ Files: turn_loop.rs:423-439 vs tools/mcp.rs:42-61, permissions.rs:217

### M-3 — auth_ref→keychain→header injection UNWIRED (AC-2 mock-only)
Confirmed by reflector, security (M-3), test-audit (HIGH#1), architecture.
Daemon never reads the keychain; every live HttpTransport uses empty headers.
Real Anthropic/DeepSeek call would 401. AC-2 "passes" only via mocks. Forward
risk: when wired, headers append to EVERY endpoint incl. MCP → cross-provider
credential leak (BR-7) unless endpoint-bound.
→ Files: runtime.rs:452/532, egress/mod.rs:365, keychain only in teton CLI

### M-4 — Dead teton-core entity types + f64 rounding bug
Quality. `teton-core::entities::{Session,CostRecord,TaskArtifact}` exported but
never used in production (daemon built LedgerRow + its own TaskArtifact). Dead
CostRecord uses `usd: f64` — the exact rounding bug the wire `usd_micros: i64`
was designed to avoid. Contrast `Phase`, cleanly bridged via to_protocol_phase.
→ Fix: delete dead types (or wire + fix to micros).

### M-5 — Provider health dead in production
Architecture. `set_health` never called; `build_router` reseeds Healthy every
turn. Policy layer's cross-turn health fallback is dead code; only in-turn
fallback works.
→ Files: runtime.rs:338/795-818, router.rs:221

### M-6 — Freeform routing policy silently inert
Architecture. `resolve_freeform` consults heuristics only, never the policy
table; a `phase="freeform"` policy is accepted at validation but never fires.
→ Fix: reject at config validation (cheap) OR thread through policy::evaluate.

### M-7 — CLI never calls cost/query RPC + duplicated baseline price
Architecture. TASK-014 added cost/query (daemon-authoritative) but CLI still
drains live events with a now-false comment AND reimplements savings with a
HARDCODED baseline price ($15/$75) hand-synced to prices.toml (2nd source of
truth). e2e never spawns the real teton CLI, so invisible to CI.
→ Files: teton/main.rs:387-415, teton/cost_ui.rs

### M-8 — Remote prompt shaping collapses roles into one user message
Reflector. RemoteProviderSource sets system:None, concatenates system+history+
tool-results into one Role::User blob. Degrades tool-calling on the exact
providers AC-2/3 route to, defeats prompt caching.
→ Files: harness/completion.rs:218-229

## MEDIUM

- MED-1 shell env scrub only catches *_KEY/*_TOKEN; misses *_SECRET, *PASSWORD*, DATABASE_URL creds. security M-1. shell.rs:165
- MED-2 MCP stdio subprocess inherits full daemon env (3rd-party npx sees all keys). security M-2. mcp/client.rs:602
- MED-3 looks_like_raw_key bypassable: secrets <40 chars, or containing `:`/`/` skip the check → raw key persisted to plaintext TOML. security M-4. config.rs:191. Fix: positive scheme allowlist (keychain://, env:, op://).
- MED-4 BR-6 verification gate hollow: `verified=true` for any verify-tool call even when it FAILED. Weak model edits, runs failing test, EndTurn verified. correctness. turn_loop.rs:415

## MINOR / LOW

- socket_path.rs byte-identical in teton + tetond → extract to teton-protocol. quality.
- retry re-bills partial progress (no ctx snapshot). correctness. runtime.rs:367
- shell timeout kills only direct child not process group. security L-2. shell.rs:131
- socket chmod-after-bind window (bounded by peer-cred). security L-1. server.rs:99. Fix: 0700 parent dir.
- prompt_tasks JoinHandle accumulation; extract_id→Id(0) collision; lagged-notice drop under backpressure. correctness. server.rs
- ProviderKind casing drift (OpenAiCompatible vs OpenaiCompatible). quality.
- no tracing in daemon (bare eprintln). quality.
- HttpTransport/Egress rebuilt per turn (BR-8 latency). architecture. runtime.rs:452/532
- deny_http_client is a manifest text-parser not real `cargo tree` — docs overclaim; misses transitive. architecture. Fix: cargo-deny banned-crates in CI.
- test gaps: AC-9 proves offered+gated not EXECUTED; AC-4 direction-only (>0) not exact; parallel-tool-drop untested; --version untested. test-audit.
- unpriced wire projection to 0 — ACTUALLY TESTED (test-audit confirms), keep as-is or add flag. minor.

## VERIFIED SOLID (multiple agents)
D-2 egress single-owner holds (compile + CI). BR-1 fail-closed for read/MCP-HTTP
paths. Peer-cred auth sound (no TOCTOU). Content-free error taxonomy. AC-2/3/7
e2e genuinely non-vacuous. thiserror/anyhow convention honored. No stray
unwrap/TODO. Probe/benchmark termination bounded. No server deadlock. SSE
framing correct.

## SCOPE DECISIONS — RESOLVED BY USER 2026-07-21 (reflector halt #2)
1. C-1/C-2 privacy bypass: **FIX NOW** (fail-closed session taint → pin to local tier)
2. M-3 credential injection: **WIRE NOW** (daemon keychain read, endpoint-bound headers)
3. M-1 privacy-block: **REROUTE TO LOCAL** (distinct non-retryable signal, one event)
4. MCP config surface: **FOLD INTO MAIN TOML** ([[mcp_server]] table)

## FIX PASS PLAN (sequential — shared tetond files)
- Group A (privacy core): C-1, C-2, M-1, M-2 → completion.rs, turn_loop.rs, tools/*, egress, router, failure.rs
- Group B (credentials/hardening): M-3, MED-1, MED-2, MED-3, L-1, L-2 → egress, shell.rs, mcp/client.rs, config.rs, daemon keychain
- Group C (cleanups): M-4, M-5, M-6, M-7, M-8, MED-4 + minors → entities.rs(delete), runtime.rs, config.rs, teton CLI, completion.rs, turn_loop.rs, socket_path extraction
- Group D (MCP TOML + test gaps): MCP→TOML, AC-9 exec assert, AC-4 exact, parallel-drop test, real teton CLI e2e, deny-check

## PHASE 5 RESOLUTION (2026-07-21)
All Critical + Major findings FIXED across 5 sequential fix groups (A privacy,
B credentials, C arch cleanups, D config+tests, E regression+hardening).
Re-verify pass (security/correctness/architecture) confirmed fixes sound and
caught 2 Major REGRESSIONS the fix pass introduced (M-8 leading-role → Anthropic
400; M-5 health permanent-stranding) — both fixed in Group E and confirmed by a
focused re-check. Final: 491 tests pass, clippy+fmt clean. Deferred (documented,
non-blocking): runtime.rs God-object split; retry re-bill (continue-vs-restart
product call); env-scrub denylist residual (backstopped by Unknown-provenance);
deny_http_client not transitive (docs corrected). FLAG FOR USER at wrapup: MCP
`trusted` defaults FALSE (untrusted stdio MCP taints session to local) — safe
default, may want to flip.
