---
id: REQ-559
title: "Global reasoning effort with per-provider clamping and thinking-token attribution"
status: approved
deployable: true
created: 2026-08-05
updated: 2026-08-11
component: "providers/openai-compat"
domain: "providers"
stack: ["rust", "daemon", "llm-providers", "cli"]
concerns: ["cost", "extensibility", "latency"]
tags: ["reasoning-effort", "thinking-tokens", "kimi", "deepseek", "anthropic", "cost-attribution"]
---

## Description

Teton sends **no reasoning controls at all**. The Anthropic adapter's request
body is `{model, max_tokens, messages, …}` (`crates/teton-providers/src/anthropic.rs:88`)
and the OpenAI-compatible adapter's is the same shape
(`crates/teton-providers/src/openai_compat.rs:94`). Neither sends `effort`,
`thinking`, nor `reasoning_effort`.

Since the charter was written, reasoning effort has become near-universal across
the providers Teton targets, and the omission has stopped being neutral:

| Provider | Field | Levels | Default when omitted |
|---|---|---|---|
| Anthropic | `output_config.effort` (+ `thinking: {type:"adaptive"}`) | low / medium / high / xhigh / max | `high` |
| Moonshot (Kimi) | top-level `reasoning_effort` | low / high / max on K3 | **`max`** |
| DeepSeek | `reasoning_effort` | low / high / xhigh / max on V4 Pro/Flash | thinking on, effort `high` |
| OpenAI-compatible generally | `reasoning_effort` | minimal / low / medium / high | varies |
| Local llama.cpp | none | — | thinking is a prompt-template property |

Two things follow, and the first is a live cost defect:

1. **Omission is not "no opinion" — it inherits the provider's default, and
   Kimi K3's default is `max`.** A user who registers Kimi as their cheap tier
   today gets every call run at the most expensive setting on that axis, silently,
   with a cost meter that reports the spend as normal. For a product whose
   headline promise is cost control (REQ-544 BR-2), shipping a request that
   declines to state its effort is worse than not supporting effort at all. This
   is LESSON-443's shape: a behavior predicated on the *absence* of a field, which
   is correct only while the field doesn't exist.

2. **Effort is a bigger cost lever than model choice on some workloads.**
   Sonnet at `low` versus Sonnet at `max` swings further than Sonnet versus a
   cheap open model. REQ-558's category→model bindings do the coarse cost work;
   effort is the orthogonal "how hard am I thinking right now" dial.

This REQ adds **one global effort setting** applied to every model call, a
canonical five-level ladder clamped per provider, and the capability flag that
keeps the clamping honest. It deliberately does **not** add per-category effort —
the category bindings already carry the per-workload distinction, and a second
table to maintain buys less than it costs.

Three provider quirks force real machinery rather than a passthrough string:

- **Level sets differ.** Five on Anthropic, three on Kimi K3, four on DeepSeek.
  A canonical ladder needs a per-provider clamp, not a rename.
- **Shapes are mutually exclusive.** Kimi K2.5/K2.6 return **HTTP 400** when both
  `thinking` and `reasoning_effort` are sent. An adapter that emits both
  unconditionally breaks those models outright.
- **The local tier has neither.** llama.cpp exposes no effort parameter; thinking
  is controlled by the chat template REQ-554 already renders through. Effort is a
  no-op there and must be *reported* as one rather than silently ignored.

Finally, the cost meter is blind to what effort buys. Both adapters read
`completion_tokens` / `output_tokens`, which **already include** reasoning
tokens — so totals are correct — but neither parses
`completion_tokens_details.reasoning_tokens`, so `teton cost` cannot say that
80% of a `design` call was thinking. Without that attribution, effort is a dial
with no gauge.

Depends on REQ-557 (a level is clamped against a declared model, not an inferred
one) and is sequenced alongside REQ-558.

## System Model

### Entities

| Entity | Field | Type | Constraints |
|--------|-------|------|-------------|
| EffortLevel | value | enum(low, medium, high, xhigh, max) | the canonical ladder — Anthropic's set, chosen as the superset |
| Config | **effort** | EffortLevel | **new**; persisted across sessions; default `high` |
| SessionState | effort_override | Option\<EffortLevel\> | session-scoped; set by `/effort`, persisted per BR-8 |
| ProviderCapabilities | **reasoning_shape** | enum(effort_only, thinking_flag_only, none) | **new**; declares which request field(s) the provider accepts. Sits beside the existing tool-call-reliability tier |
| ProviderCapabilities | **effort_ladder** | ordered set of EffortLevel | **new**; the levels this provider actually accepts; canonical levels clamp into it |
| TokenUsage | **reasoning_tokens** | Option\<u64\> | **new**; parsed where the provider reports it. `None` means unreported, never `0` |
| CostRecord | **reasoning_tokens** | Option\<u64\> | **new**; a subset of `output_tokens`, never added to it |

### Events

| Event | Trigger | Payload |
|-------|---------|---------|
| route_decided | unchanged | gains the **effective** effort for this call — post-clamp, per provider — so a clamped level is visible at the moment it happens (REQ-544 BR-5) |
| cost_recorded | unchanged | `CostRecord` gains `reasoning_tokens` |

No new RPCs. `config/get`/`config/set` carry the effort key; `cost/query`'s
report gains the thinking split.

### Permissions

| Action | Roles Allowed |
|--------|---------------|
| Set the persisted effort default | the user only, via `teton effort <level>` or config-file edit |
| Set the session effort | the user only, via typed `/effort <level>` — never inferable from model output or file content (REQ-544 permission posture) |
| Read the effective effort per provider | any attached client |

## Business Rules

- [ ] BR-1: An effort value is sent on **every** model call to a provider whose
      `reasoning_shape` is not `none`. Omitting the field is never a valid
      outcome of the resolution chain — a request that declines to state its
      effort inherits the provider's default, and at least one target provider
      defaults to `max`. The absence of a user setting resolves to the declared
      default (`high`), not to an absent field. (informed by LESSON-443)
- [ ] BR-2: Effort is **one global setting** applied to all model calls in a
      session. There is no per-category, per-tier, or per-provider effort
      configuration. The single exception is the BR-5 clamp, which is a
      capability constraint, not a user setting.
- [ ] BR-3: The canonical ladder is `low < medium < high < xhigh < max`, ordered
      and closed. It is the only vocabulary the router, the config, the CLI, and
      the events speak; provider-native spellings exist only inside the adapter.
- [ ] BR-4: `ProviderCapabilities.reasoning_shape` selects which field(s) the
      adapter emits, and the adapter emits **exactly one shape**: `effort_only`
      sends the effort field alone; `thinking_flag_only` sends the thinking flag
      alone; `none` sends neither. Emitting both is a 400 on Kimi K2.5/K2.6, so
      the mutual exclusion is a correctness constraint, not a style preference.
      The shape is declared per provider, never sniffed from a response.

      **An OpenAI-compatible endpoint with no declared shape defaults to
      `effort_only`** (resolves OQ-2; user decision 2026-08-11). Defaulting to
      `none` would reintroduce the Kimi-defaults-to-`max` hazard at exactly the
      BYOM endpoint REQ-544 BR-6 exists to serve — the defect this REQ was
      written to fix, reappearing at the provider Teton knows least about. The
      opposite risk is bounded and already handled: a server that rejects the
      unknown field answers 400, which BR-12 turns into a typed error and a
      fallback to the `none` shape. A stated effort some endpoints refuse is
      recoverable; an unstated effort that silently bills at `max` is not.
- [ ] BR-5: A canonical level is **clamped** into the provider's `effort_ladder`
      by nearest-supported-at-or-below, then nearest-supported-above if no lower
      level exists. Clamping is a pure function, table-driven-testable, and the
      **effective** level after clamping is what `route_decided` reports —
      reporting the requested level would make the event lie about the call.
      (informed by REQ-544 BR-5, LESSON-456)
- [ ] BR-6: A provider whose `reasoning_shape` is `none` — the local tier — makes
      effort a **declared no-op**: no field is sent, and the surface says so
      rather than displaying a level the model is ignoring. A silently ignored
      setting is the misattribution family of BUG-146 and BUG-153: the user set
      something and something else happened. (informed by LESSON-456, BUG-153)
- [ ] BR-7: Categories pinned to the local tier by REQ-558 (`route`, `redact`,
      and any category whose binding resolves local) are capped at the local
      provider's ladder — which is empty — so a global bump to `max` for a hard
      design question cannot inflate the reflex tier. The cap lives in the clamp
      table, not in per-category configuration (which BR-2 forbids).
- [ ] BR-8: The effort setting **persists across sessions**; `/effort <level>`
      writes it and the next session starts there. This is the deliberate
      asymmetry with REQ-560's permission level, which is session-scoped and
      resets — an effort level that survives a restart costs money predictably,
      a permission level that survives one removes a guardrail invisibly.
- [ ] BR-9: `teton effort` and `/effort` with no argument print the current
      level and, for each registered provider, the level it clamps to. Both
      render through **one** resolution function shared with the router — two
      surfaces describing one setting must not drift. (informed by LESSON-456,
      REQ-555 BR-4)

      **This REQ owns the `/effort` row** in the `COMMANDS` table, including its
      bare-argument read path and its appearance in `/help` (REQ-555 BR-7).
      REQ-560 renders the effort *value* in the status line and adds the
      `/permissions` row; it does not add, alias, or duplicate `/effort`. Stated
      because both specs previously claimed the command.
- [ ] BR-10: `reasoning_tokens` is parsed where the provider reports it
      (`completion_tokens_details.reasoning_tokens` on the OpenAI-compatible
      path) and recorded on the `CostRecord`. It is a **subset** of
      `output_tokens`, never added to it — today's totals are already correct and
      must stay byte-identical for a workload whose reasoning tokens are
      unreported. An unreported count is `None`, never `0`. (informed by
      REQ-544 BR-2)
- [ ] BR-11: `teton cost` reports the thinking split where it is known and says
      "unreported" where it is not. REQ-544 BR-2 forbids displaying estimated
      spend as actual; a `0` standing in for "the provider didn't tell us" is
      exactly that.
- [ ] BR-12: An adapter that cannot honor the resolved effort (unknown level for
      a provider whose ladder is stale, a 400 naming the effort field) reports a
      typed error that names the provider, the requested level, and the clamped
      level, and falls back to the provider's `none` shape for that call — it
      never retries silently and never sends both shapes to "see which works".
      (informed by LESSON-447, REQ-554 BR-6)
- [ ] BR-13: Effort changes nothing about egress. Boundary enforcement, session
      taint pinning, and CostRecord emission are unchanged; a higher effort level
      is not a reason for a call to take a different path. (informed by
      LESSON-432)

## Acceptance Criteria

- [ ] AC-1: Every outbound request to a provider with `reasoning_shape ==
      effort_only` carries an effort field. A mock-transport test asserts the
      field's presence across all four tiers and both adapters, and fails if any
      call path omits it. This is the direct regression for the Kimi-defaults-to-
      `max` defect. (BR-1)
- [ ] AC-2: A provider declared `thinking_flag_only` receives the thinking flag
      and **no** effort field; a provider declared `effort_only` receives the
      effort field and **no** thinking flag. A test asserts no request ever
      carries both. (BR-4)
- [ ] AC-2b: A registered OpenAI-compatible provider with **no declared
      `reasoning_shape`** sends the effort field on its first call — the
      `effort_only` default. Against a mock endpoint that answers 400 on that
      field, the call produces BR-12's typed error and falls back to the `none`
      shape without a silent retry, and the capture contains no request carrying
      both shapes. This is the BYOM leg of AC-1's regression. (BR-4, BR-12)
- [ ] AC-3: Clamp table: canonical `xhigh` against a three-level ladder
      (`low/high/max`) resolves to `high`; canonical `medium` against the same
      resolves to `low`; canonical `low` against a ladder whose floor is `high`
      resolves to `high` (nearest-above when nothing lower exists). Table-driven
      across all five canonical levels × at least three ladders. (BR-5)
- [ ] AC-4: `route_decided` reports the **clamped** level, not the requested one.
      With the session at `xhigh` and a three-level provider, the event says
      `high`. (BR-5)
- [ ] AC-5: A call routed to the local tier sends no effort field and no thinking
      flag; `teton effort` shows the local provider as "not applicable" rather
      than showing a level. (BR-6)
- [ ] AC-6: With the session at `max`, a `route`-category call still resolves to
      the local tier with no effort field — a global bump cannot inflate a
      local-pinned category. (BR-7)
- [ ] AC-7: `/effort low` in a session, then a full daemon restart and a fresh
      session, shows `low` — the setting persisted. Contrast test with REQ-560's
      permission level, which resets. (BR-8)
- [ ] AC-8: `teton effort` with no argument prints the current level and each
      provider's clamped level, rendered through the same function the router
      calls — asserted by a shared-resolver test, not by string coincidence.
      (BR-9)
- [ ] AC-9: A response carrying `completion_tokens_details.reasoning_tokens`
      produces a `CostRecord` whose `reasoning_tokens` is that value and whose
      `output_tokens` is unchanged from today's parse — proving the subset
      relationship. A response without the field produces `None`, and
      `teton cost` renders "unreported" rather than `0`. (BR-10, BR-11)
- [ ] AC-10: A provider returning 400 on the effort field produces a typed error
      naming the provider, requested level, and clamped level; the session
      continues via the existing degradation path; no request in the capture
      carries both shapes. (BR-12)
- [ ] AC-11: Egress-capture: raising effort to `max` on a session with a
      `local-only` boundary produces zero remote calls containing boundary
      content. (BR-13, REQ-544 AC-5 posture)
- [ ] AC-12: Mutation check — removing the always-send rule (BR-1), or making the
      clamp an identity function, each makes at least one test red. (informed by
      LESSON-441)

## External Dependencies

- **REQ-557 must land first** — an effort level is clamped against a declared
  model, not one inferred from a price table.
- Provider APIs: Anthropic `output_config.effort` + `thinking`, Moonshot/Kimi
  top-level `reasoning_effort`, DeepSeek `reasoning_effort`. All are existing
  fields on endpoints Teton already calls; no new SDK, no new crate.
- No new crates.

## Assumptions

- The published ladders are current as of 2026-08-05: Anthropic five levels
  (default `high`), Kimi K3 `low`/`high`/`max` (default `max`), DeepSeek V4
  Pro/Flash `low`/`high`/`xhigh`/`max`. **These are provider-published values
  that change without notice** — which is exactly why BR-4/BR-5 put them in a
  per-provider declaration rather than hardcoding a switch on provider kind.
  A stale ladder degrades through BR-12, not through a wrong request.
- Reasoning tokens are already inside `completion_tokens` / `output_tokens` on
  both adapters, so BR-10 is an attribution change and not a totals change.
  **Verified against both parsers**, which read only the aggregate fields today.
- The local tier's thinking behavior is a property of the chat template REQ-554
  renders through, so effort has no local expression to map onto. If a future
  local model exposes a runtime thinking toggle, BR-6's `none` shape gains a
  fourth variant rather than being reinterpreted.
- One global effort is the right granularity — user decision, 2026-08-05,
  overriding an earlier per-category proposal. The category bindings (REQ-558)
  carry the per-workload cost distinction; effort is a session-level dial.
- id allocated with remote verification (no degradation warning from the
  allocator).

## Open Questions

- [ ] OQ-1: How does a provider's `effort_ladder` get populated — hardcoded per
      `kind` in the adapter, declared in the provider config by the user, or
      probed once at registration? Hardcoding is wrong for arbitrary
      OpenAI-compatible endpoints (REQ-544 BR-6 promises any endpoint with no
      code change); user-declared is honest but is a knob nobody wants to fill in.
      Leaning: a per-kind default table the user may override.
- [x] OQ-2 — **closed 2026-08-11: `effort_only`.** An unknown
      OpenAI-compatible endpoint states its effort. Recorded in BR-4 with the
      reasoning and pinned by AC-2b. The 400 risk is real but bounded and
      already has a handler (BR-12); the `none` alternative silently
      reintroduces this REQ's originating defect at the least-known provider,
      which does not.
- [x] OQ-3 — **closed 2026-08-11: down-then-up, as already specified.** This
      question contradicted its own spec: BR-5 states nearest-supported-at-or-
      below then nearest-above, and AC-3 pins `xhigh` → `high` against a
      `low/high/max` ladder in a table-driven test. A spec cannot hold open a
      behavior it asserts two sections earlier. The cost-conservative reading
      stands — a user who wants the higher rung names it, rather than having a
      clamp round up on their behalf and bill for it.
- [ ] OQ-6: Does a BR-12 fallback **persist for the session**? BR-12 specifies
      per-call fallback and forbids silent retries, so an endpoint that 400s on
      the effort field 400s once per call for the life of the session — correct
      but wasteful. Options: remember the refusal per provider for the session,
      or downgrade that provider's declared shape to `none` and say so on the
      surface. Raised by OQ-2's resolution; not created by it.
- [ ] OQ-4: Does `teton cost` break out reasoning tokens per category (REQ-558)
      as well as in total? Per-category is the view that makes the dial tunable;
      it also widens this REQ's coupling to REQ-558's landing.
- [ ] OQ-5: Should raising effort mid-session apply to the current in-flight turn
      or only the next one? Applying mid-turn splits one turn across two effort
      levels, which makes the CostRecord ambiguous.

## Out of Scope

- Per-category, per-tier, or per-provider effort configuration (BR-2 — explicit
  user decision, not an oversight).
- Provider-native thinking-budget parameters (`budget_tokens`, `thinking_budget`)
  — deprecated or removed on the target models; the canonical ladder replaces
  them.
- Task budgets / cumulative-spend ceilings across an agentic loop. A real
  feature, a different axis, a separate REQ.
- Surfacing thinking *content* to the user. This REQ carries token counts, not
  reasoning text.
- The status line that displays the effort level — REQ-560 owns the rendering;
  this REQ owns the setting, its persistence, and the `/effort` command itself
  (BR-9), including its `COMMANDS` row and `/help` entry.
- Permission levels (REQ-560).
- Automatic effort tuning from observed cost or task difficulty.

## Retrieved Context

- REQ-544 (spec, score 7): Teton Code — hybrid local/remote AI coding agent with workflow-aware model routing
- REQ-555 (spec, score 4): In-session slash commands for the teton interactive CLI
- REQ-554 (spec, score 3): Local tier renders prompts through the model's native chat template
- LESSON-482 (lesson, score 3): A prompt that enumerates a turn's legal endings must name every one
- REQ-547 (spec, score 3): First-run local model consent
- BUG-152 (bug, score 3): A prompt typed while the local tier is still loading is reported as an error, not as a wait
- LESSON-456 (lesson, score 2): A `_`-discarded error is a silent downgrade
- BUG-146 (bug, score 2): First prompt after install fails with a message blaming the local engine
- REQ-556 (spec, score 2): Live model-loading progress in the interactive session
- LESSON-481 (lesson, score 2): A gate that hides a feature from users also hides it from the test suite
- BUG-153 (bug, score 2): /exit is not a command
- LESSON-443 (lesson, score 1): A guard keyed on a feature's absence disables itself when the feature lands
- LESSON-447 (lesson, score 1): A best-effort fallback must preserve the invariant it backs up — and fail loudly
- LESSON-432 (lesson, score 1): Provenance must derive from what a tool touches, not from an argument name
- LESSON-441 (lesson, score 1): A fix pass is new code — re-verify it adversarially, not by test count

Note: LESSON-443 and LESSON-447 scored below the cut on this query's tags but are
directly load-bearing for BR-1 and BR-12 respectively, and were read and cited on
that basis. `complete` treated as the local spelling of `deployed` for the
spec-status filter (precedent: REQ-555, REQ-556). The Step-1.6 delegated body-read
timed out (SIGTERM at 120s); the documented fallback path ran and the top-15
bodies were read directly.
