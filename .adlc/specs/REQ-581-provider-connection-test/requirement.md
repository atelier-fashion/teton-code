---
id: REQ-581
title: "A first-class provider connection test: `/provider test <id>` makes one consented call and says exactly what came back"
status: complete
deployable: true
created: 2026-08-17
updated: 2026-08-17
component: "cli"
domain: "providers"
stack: ["rust", "cli", "daemon", "json-rpc", "llm-providers"]
concerns: ["developer-experience", "routing", "security"]
tags: ["provider-test", "connection-test", "guided-enablement", "slash-command", "provider-health", "egress", "hand-off", "kimi"]
---

## Description

A user who has just registered a remote provider asks the obvious next
question — *"is it working?"* — and today the product has no answer of its
own. Observed 2026-08-17 on v0.1.19, in a session where `kimi` was correctly
registered and bound to `build`:

```
› alright, I followed you instructions. Can you test the Kimi connection?
 - shell: teton provider list [done]
I see there's an issue with reading the config file. Let me try a different approach…
 - shell: teton policy show [done]
I see there's an issue with accessing the configuration…
 - shell: ls -la ~/.teton/ …
 - shell: find ~ -name "teton" -type d … [failed]
… 2. Make sure you've set your API key properly:
export TETON_PROVIDER_KEY=your_actual_kimi_api_key_here
```

The question was classified `think`-class, so it ran on the **local** model,
which was never going to reach Kimi; the local model then improvised shell
commands (all of which succeeded and printed the right thing), misread their
output as a config problem, guessed at a config directory and a `teton status`
command that do not exist, and told the user to re-supply the key through
`TETON_PROVIDER_KEY` — a real variable, but one only `teton provider add` reads,
useless for a key already in the keychain. Meanwhile the connection was fine —
one `edit`-class turn later routed `[edit/build] → kimi kimi-k3` and answered.

REQ-579 deliberately left a connection test out of `/provider setup` (BR-13:
"verification is a consented turn" — nothing leaves the machine unless the
user asks). This REQ is that consented turn, made first-class: a command the
user runs on purpose, that makes **one** minimal call to the provider through
the exact path a real turn takes, and reports what came back in the daemon's
own vocabulary — reached / refused (401, 403) / model unknown (400) / rate
limited / unreachable — with latency and the cost it recorded. And, as with
REQ-579, the session **hands off** to it instead of improvising: "test the
Kimi connection" should leave `/provider test kimi` on screen, not four
guessed shell commands.

The user experience this is for:

```
› /provider test kimi
   provider:  kimi (openai-compatible, kimi-k3) — https://api.moonshot.ai/v1/chat/completions
   this sends one minimal request (≈ 20 tokens) to api.moonshot.ai. proceed?  [y/N] y
   kimi kimi-k3: reachable — answered in 1.4 s (2040 in / 21 out, $0.0064 recorded);
   provider health: healthy. `build` routes here (edit, shell).
```

and on a bad key:

```
   kimi kimi-k3: refused — HTTP 401 from api.moonshot.ai (the vendor did not accept the
   credential at keychain://teton/kimi). Nothing else was sent. Re-run `/provider setup kimi`
   to store a new key, or `teton provider add kimi --model kimi-k3` from a shell.
```

**Provenance.** REQ-579 (the trio, the hand-off, the surface nudge), REQ-578
(the endpoint the daemon will actually POST), REQ-544 M-5 (per-provider health
the router reads), REQ-562/BR-1 (every remote byte goes through the egress
choke point; the CLI has no network path of its own).

## System Model

### Entities

| Entity | Field | Type | Constraints |
|--------|-------|------|-------------|
| ProviderTestParams | session_id, provider_id | ids | the provider must be registered; a `kind = "local"` provider is refused (there is nothing to dial) |
| ProviderTestResult | outcome | `reached { latency_ms, input_tokens, output_tokens, usd_micros: Option<i64> }` \| `refused { status: u16, reason }` \| `unknown_model { status, reason }` \| `rate_limited { retry_after_secs: Option<u64> }` \| `server_error { status, reason }` \| `unreachable { reason }` \| `not_a_completion { reason }` \| `timed_out { after_secs: u64, reason }` | `reason` is the **daemon's** sentence built from the status, the dial host, the configured model and the credential *reference* — never the credential value, never the request body, never the vendor's response body (architecture ADR-3; *amended at architect: `server_error` added for 5xx, `usd_micros` for the ledger's own unit*; *amended at verify: `not_a_completion` (a 2xx/3xx that completed nothing) and `timed_out` (the probe's own deadline elapsed) split out of `unreachable`, which now means "nothing answered" and nothing else — three different next moves for the user, so three values rather than one carrying distinguishing prose (BR-3, LESSON-456)*) |
| ProviderTestResult | dial_host | string | the host the request went to (REQ-578's reading), for the report line |
| ProviderTestResult | health_after | Healthy \| Degraded \| Unavailable | what the router will read on the next turn |

### Events

| Event | Trigger | Payload |
|-------|---------|---------|
| `provider_tested` | a `provider/test` completed (any outcome) | `provider_id`, `outcome` (the typed value above, sans token counts), `health_after` — session-scoped, so a second client attached to the session sees routing health change |
| `cost_recorded` (existing) | a `reached` test | the ordinary ledger row: the test **is** a model call and is billed as one, tagged so `teton cost` can show it as a probe rather than a turn |

### Permissions

| Action | Roles Allowed |
|--------|---------------|
| `provider/test` | the session's own user (REQ-568 `may_drive`; a tool call naming the method and a foreign connection are refused with `NOT_ATTACHED`, as `provider/setup_commit` is) |
| the outbound request | only after the in-session confirmation (BR-2); non-TTY invocation requires `--yes`. `teton provider test <id>` opens a freeform session first — the method is session-gated and the cost row needs one (architecture ADR-5) |

## Business Rules

- [ ] BR-1: **The test is the real path, minimal.** One completion request, through the same adapter, transport, credential resolution and egress choke point a turn uses — never a side channel, never a `GET /v1/models` shortcut that would prove reachability for an endpoint a turn does not POST to. Fixed prompt, `max_tokens` at the floor the adapter allows, no tools, no conversation context. *(informed by REQ-562 BR-1, REQ-578)*
- [ ] BR-2: **Consented and previewed.** Before anything leaves the machine the surface names the provider, model and dial host and asks; a `no` sends nothing. Piped/non-TTY needs `--yes`. The command *is* the user asking, and the preview is what makes that true in front of them (REQ-579 BR-13's rule, kept). *(informed by REQ-579)*
- [ ] BR-3: **The report is the daemon's classification, typed — never the client re-reading a sentence.** Outcomes are the variants above; the CLI branches on the variant and renders `reason` verbatim. A 401/403 is `refused` and names the credential *reference* (`keychain://teton/kimi`), never its value; a 404/400 naming the model is `unknown_model`; a 429 is `rate_limited` with the vendor's `Retry-After`; DNS/TCP/TLS failures are `unreachable`. *(amended at verify: the probe's own deadline elapsing is `timed_out { after_secs }` and a 2xx/3xx that completed nothing is `not_a_completion` — both were `unreachable` with distinguishing prose, which is the thing this BR forbids)* *(informed by LESSON-456, REQ-544 M-3)*
- [ ] BR-4: **A test moves the same health the router reads.** `reached` records the provider healthy (clearing any downgrade, exactly as a served turn does); a persistent failure stamps the half-open cooldown a failed turn would. The report says what the next turn will therefore do. *(informed by REQ-544 M-5)*
- [ ] BR-5: **It is a model call and is billed as one.** The `reached` outcome writes an ordinary ledger row (priced when the model has a price), tagged as a probe so `teton cost` can say "1 probe" rather than counting it as a turn. No cost is guessed for an unpriced model. *(informed by REQ-544 BR-2)*
- [ ] BR-6: **The session hands off; the model does not improvise.** When a turn asks whether a provider works / is connected / is reachable, the session's answer names `/provider test <id>` — by the resident guide saying so first, or by the interactive surface appending one harness-voiced line when the reply recites `teton provider …` or shell probing (REQ-579 ADR-9's nudge, extended to this intent). The CLI form `teton provider test <id>` is the non-interactive answer. *(informed by REQ-579 BR-1)*
- [ ] BR-7: **Both surfaces, one implementation.** `/provider test <id>` (in-session) and `teton provider test <id>` (shell) are two call sites of one `provider/test` daemon method; the CLI has no network path of its own (BR-1 of the project). `teton doctor` stays passive and says so, unless OQ-2 decides otherwise.
- [ ] BR-8: **A local provider is refused with the reason, not tested.** `kind = "local"` has nothing to dial; the answer is the tier's state (REQ-580's classification), and the command says to read `teton doctor` for it.

## Acceptance Criteria

- [ ] AC-1: In a TTY session with `kimi` registered and a valid key, `/provider test kimi` previews provider/model/host, waits for `y`, and reports `reached` with latency, token counts and recorded cost; `teton cost` afterwards shows one probe row for `kimi`; the provider's health is `healthy`. Asserted over the socket against a mock provider (the e2e harness's `MockProvider`), and recorded once live.
- [ ] AC-2: With the mock provider answering 401, the report is `refused — HTTP 401 …`, names `keychain://teton/kimi` and never the key value (asserted on the rendered line and on every event payload), and no ledger row is written.
- [ ] AC-3: With the mock answering 404 for the model, `unknown_model`, naming the model the config declares. With 429, `rate_limited` — carrying `retry_after_secs` only when the transport surfaces the header, which v1's does not by design (architecture ADR-2: `TransportResponse` carries exactly one named header, and this REQ does not grow that); the report says "try again shortly". With 5xx, `server_error { status }`. With a closed port, `unreachable`. Each is a distinct typed outcome, not distinguishing prose. *(amended at architect — the original read "carrying 7"; deferred, not dropped: OQ-5)*
- [ ] AC-4: `n` at the preview sends nothing — the mock records zero requests — and the ledger is unchanged. Piped stdin without `--yes` sends nothing and says why.
- [ ] AC-5: A `reached` test on a provider the health map holds as `Unavailable` returns it to `healthy`, and the next turn's `route_decided` selects it (asserted through `run_prompt_turn` after the test).
- [ ] AC-6: `provider/test` from a connection not attached to the session, and from a model tool call naming the method, are refused `NOT_ATTACHED` in the response, and no request leaves the machine.
- [ ] AC-7: `/provider test onlocal` (a `kind = "local"` provider) refuses in-response with the local tier's current state sentence and makes no call. *(amended at verify: the daemon method refuses so — tested in tetond for direct callers — and the interactive surface short-circuits **before any RPC** for a snapshot that says local, printing that a connection test dials nothing and pointing at `teton doctor` for the tier's state; that closes a preview-then-call race a reviewer found, and it means the CLI's line does not itself carry the tier-state sentence.)*
- [ ] AC-8a: The resident guide names `/provider test <id>` for the "does my provider work" question, pinned by the same contract test that gates the guide against the recipe catalog (REQ-579 AC-5's shape).
- [ ] AC-8b: The surface nudge fires for a reply that recites `teton provider` (or shell-probes `teton …`) in answer to a connection question — trigger as settled by OQ-3 — printed at most once per turn and never on a non-TTY surface; unit-tested over the render seam with a scripted reply, and A/B'd live before the guarantee is claimed (LESSON-532).
- [ ] AC-9: `/help` lists `/provider test` from the same command table that dispatches it (REQ-555 BR-7).

## External Dependencies

- None new. Reuses the adapter/transport/egress path, the REQ-544 health map and ledger, the REQ-579 command table and nudge seam.

## Assumptions

- Every supported provider kind accepts a one-token completion as a legitimate request (OpenAI-compatible and Anthropic both do). If a future kind has a cheaper *documented* probe on the same endpoint family, BR-1 admits it only if a turn would take that same path.
- The failure classes the transport already produces (`FailureClass`, HTTP status) carry enough to fill the typed outcomes; if `unknown_model` cannot be told from a generic 400 for some vendor, it degrades to `refused { status: 400, reason }` and the report says the vendor's sentence.
- One consented call's cost is acceptable to the user by construction: they asked, and the preview told them what it would cost in shape (tokens) — the exact figure is only knowable after.

## Open Questions

- [x] OQ-1: **Should the preview show an estimated cost?** Resolved at verify: **no** in v1. REQ-544 M-7 has the CLI compute no spend, so an estimate before the call would need a new daemon RPC for one line; the preview names the shape of the request instead (a few tokens in, at most 8 out) and the report shows the *recorded* figure the moment it exists. Revisit with OQ-2 if a `doctor --probe` surface wants an up-front budget.
- [ ] OQ-2: **`teton doctor --probe`** as a third surface that tests every remote provider in turn? Lean: not in v1 — `doctor` is the passive, no-egress diagnostic and its line saying so is load-bearing (BR-1 of the project); a `--probe` flag that runs the same method N times with N previews is a follow-up once v1 is dogfooded.
- [ ] OQ-3: **What is the hand-off trigger for the nudge?** REQ-579's nudge keys on the reply reciting `teton provider add` / `policy set-tier`. A connection question's bad answers look like shell probing (`teton provider list`, `ls ~/.teton`) rather than a recognisable recipe. Lean: key on the *user's* turn text ("test|check|verify … (connection|provider|kimi|…)") plus the reply containing `teton provider` or a `shell:` call naming `teton` — and A/B it live before trusting it (REQ-579's lesson: 0/9 on prompt steering alone).
- [x] OQ-4: **Ledger tagging.** Resolved at architect: a nullable `probe INTEGER` column through the existing `ADDITIVE_COLUMNS` migration and a `CostRecord.probe: bool` wire field; the routing `Category` enum is left alone (a probe is addressed to a provider, not routed by category).
- [ ] OQ-5: **`Retry-After` on `rate_limited`.** Deferred from AC-3 (ADR-2). Revisit if a second consumer wants a response header — the fix is a second *named* field on `TransportResponse`, not a header bag.
- [ ] OQ-6: **A cap on the paid method.** `provider/test` has no in-flight cap or per-session rate limit — the same posture as `session/prompt`, and consented per call by the user's own client. Recorded at verify as accepted for v1; a small minimum interval per (session, provider) would be the defence-in-depth if a scripted caller ever matters.

## Verify record (Phase 5)

Six reviewers, 15 advisory candidates (9 refuted). Fixed in one pass: a
**Critical** (the consent preview echoed a stored endpoint's userinfo — every
other CLI line masks it; now it does too, and the "never the key" fixture
plants the key in the endpoint so it can fail); six **Major** (a redirect /
non-SSE 2xx read as `reached` — now `unreachable`, health untouched; an
in-flight probe was `abort()`ed at teardown like an attach rather than
drained like a turn — now drained, so a billed request keeps its row; no
deadline — now `PROBE_DEADLINE`, so `Timeout → unreachable` is live; the
local-kind branch reached the RPC without a confirm — now RPC-free; the
calling client rendered the notice *and* the report — the notice is for other
clients only; the shell entry point had no test — a `cli_e2e` leg); and the
minors listed in the PR. Both of those two then earned their **own** outcome
rather than a sentence inside `unreachable` (BR-3's own rule, applied to the
fix): a 2xx/3xx that completed nothing is `not_a_completion` and a deadline
elapse is `timed_out { after_secs }`, so `unreachable` means "nothing answered"
and nothing else. Deferred: OQ-5, OQ-6, an Anthropic-shaped fixture
pinning the reported `usd_micros` against the ledger row (the OpenAI shape is
pinned), and a "health unchanged" phrasing for transient failures.

## Out of Scope

- Testing the *local* tier (that is REQ-580's classification and `teton doctor`).
- Running a test automatically at `/provider setup` commit time (BR-13 stands; the setup flow may *offer* `/provider test <id>` in its completion line, which is one sentence and no egress).
- Any change to what a failed *turn* reports; this REQ is the user asking on purpose.
- A background health poll. Health moves only on turns and on tests the user ran.

## Retrieved Context

Filed 2026-08-17 from a dogfood session (v0.1.19), beside BUG-177 (attach
replay noise seen in the same screenshot). Related: REQ-579 (setup trio,
hand-off, nudge — the shape to copy), REQ-578 (dial host), REQ-544 M-5 (health)
and BR-2 (ledger), REQ-562 BR-1 (egress choke point), REQ-580 (local-tier
state for BR-8), LESSON-456 (typed outcomes, not re-read prose), LESSON-532
(presence in context is not instruction-following — why BR-6 has a surface
guarantee and not only a guide sentence).
