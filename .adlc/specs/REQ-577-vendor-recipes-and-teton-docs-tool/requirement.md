---
id: REQ-577
title: "Vendor provider recipes and a bundled teton_docs knowledge tool"
status: approved
deployable: true
created: 2026-08-14
updated: 2026-08-14
component: "tetond/harness"
domain: "harness"
stack: ["rust", "prompt", "daemon"]
concerns: ["developer-experience", "cost"]
tags: ["self-configuration", "bundled-docs", "system-prompt", "provider-recipes", "teton-docs", "onboarding", "tool-registry"]
---

## Description

Close the remaining gap in Teton's self-knowledge, in two coupled parts.

**Part A — vendor recipes.** The bundled self-configuration guide (BUG-160)
teaches the *generic* provider surface (`teton provider add … --kind
openai-compatible --endpoint <url> --model <model>`), but a user who says
"hook up Kimi for deep reasoning" needs facts the local model's weights do not
reliably hold: the vendor's real endpoint URL, which provider kind it speaks,
and a plausible example model name. Without them, the best possible answer is
a command template with a hole in it — and live behavior shows the model
falls back to hunting the user's repository instead (the BUG-154/BUG-160/
BUG-168 failure family). Ship a small canonical set of vendor recipes so the
agent can hand the user exact, runnable commands for the providers users
actually name.

**Part B — a `teton_docs` retrieval tool.** The inline guide is the right
vehicle for the always-needed surface, but it lives under a hard byte ceiling
with roughly 1.4 KB of headroom left (BUG-160's sizing; BUG-168 already had
to shorten guide phrases to pay for one longer clause). Product knowledge
beyond that — fuller provider recipes and troubleshooting, policy/tier
semantics, web-lookup setup depth — needs a growth path that does not grow
the resident prompt. Add a read-only `teton_docs` tool that returns bundled,
compiled-in topic documents on demand: versioned with the binary (never
stale, no egress, works offline), spending context only when a turn actually
needs the knowledge.

Also folded in: an explicit **referral posture** sentence. In the motivating
transcript the agent tried to *perform* the setup itself before flailing.
Provider registration is deliberately human-gated (echo-off key entry into
the keychain; presence-attested config mutation per REQ-575/576), so the
prompt must state outright that the agent's job is to hand the user the exact
commands, not to run them.

Why now: this is the product's front door. First-run users asking "how do I
connect Claude / Kimi / DeepSeek?" is the top of the funnel for the entire
cost-control promise, and every unanswerable self-question burns local-tier
turns and context on a hunt that cannot succeed.

## System Model

### Entities

| Entity | Field | Type | Constraints |
|--------|-------|------|-------------|
| ProviderRecipe | vendor | string | required, unique within the recipe set (e.g., "Moonshot (Kimi)") |
| ProviderRecipe | kind | enum: `anthropic` \| `openai-compatible` | required; must be a kind `teton provider add` accepts |
| ProviderRecipe | endpoint | string (URL) | required for `openai-compatible`; absent for `anthropic` (built-in endpoint) |
| ProviderRecipe | example_model | string | required; labeled as an example — users may substitute any model the vendor serves |
| ProviderRecipe | notes | string | optional; bounded (e.g., "local, keyless" for Ollama) |
| DocTopic | name | string | required, unique, lowercase single token (the `teton_docs` argument) |
| DocTopic | body | markdown text | required; compiled into the binary; size ≤ the pinned per-topic ceiling (BR-9) |

### Events

| Event | Trigger | Payload |
|-------|---------|---------|
| tool_call | model invokes `teton_docs` | existing tool-call event shape; title names the tool and the requested topic |

No new event types. `teton_docs` calls never produce egress events (BR-6).

### Permissions

| Action | Roles Allowed |
|--------|---------------|
| call `teton_docs` | the model, in any session, without a permission prompt (read-only, no egress, touches no user data) |
| execute `teton provider add` / `teton policy set-tier` | the user only — the agent refers, never executes (BR-5) |

## Business Rules

- [ ] BR-1: The bundled self-config guide carries a recipe line for every
  vendor in the canonical recipe set, each yielding an exact, runnable
  `teton provider add` command (kind, endpoint where applicable, example
  model) plus the `teton policy set-tier` routing step — so a request naming
  any recipe vendor is answerable with zero repository searching. (informed by
  BUG-160, LESSON-493)
- [ ] BR-2: Recipe facts have exactly one typed, daemon-owned source; every
  prose copy of them — the inline guide, the `teton_docs` providers topic, any
  README rows that name them — is CI-gated against that source
  bidirectionally, per the established suggestion-catalog pattern (REQ-573).
  Drift in either direction is a test failure, not a doc bug.
- [ ] BR-3: Every named vendor recipe is verified against the vendor's real,
  current contract (endpoint URL, provider kind, auth expectations) at
  implementation time — named examples are the requirement's test vectors,
  chosen because they are what users will actually configure. (informed by
  LESSON-512, BUG-165)
- [ ] BR-4: With recipes and the new tool's docs included, the assembled
  system prompt still clears the existing pinned size margins
  (`REDACT_BODY_OVERHEAD_BYTES` ceiling and `MIN_PROMPT_HEADROOM_BYTES`) on
  every harness profile; the margin tests stay green and their headroom is
  re-recorded. (informed by BUG-160, BUG-168)
- [ ] BR-5: Referral posture: the prompt states imperatively that the agent
  cannot run Teton's own setup commands and must give the user the exact
  commands to run. The sentence follows the BUG-168 wording rules — stated
  outright, never in an em-dash aside behind a meta-instruction — and is
  pinned by a regression test whose failure message tells a rewording
  maintainer to update rather than delete. The existing "never ask for a key
  in-conversation" rule is preserved verbatim. (informed by BUG-168,
  LESSON-482)
- [ ] BR-6: `teton_docs` is read-only and self-contained: it takes a topic
  name, returns the bundled topic body from process memory, and touches no
  filesystem path, no network, and no user data at call time. A session using
  it emits zero egress events. (informed by BUG-160 — the "no docs tool" root
  cause)
- [ ] BR-7: `teton_docs` is exposed in every session profile — default,
  strong-model, degraded, and offline — and its exposure survives the
  `max_tools` cap via cap exemption (per OQ-3's resolution), pinned by a
  test, never a registration-order accident. The one unacceptable outcome is
  silently absent. (informed by LESSON-496)
- [ ] BR-8: The tool's docstring (visible in the system prompt) enumerates
  the topic index; calling with an unknown topic returns a didactic error
  naming the valid topics — never a crash, never an empty result.
- [ ] BR-9: Every bundled topic body sits under a pinned per-topic byte
  ceiling sized against the default profile's context budget, enforced by a
  test, so a single docs read can never evict the conversation it is serving.
  (informed by LESSON-482 — the 4,096-token window is a real constraint)
- [ ] BR-10: The growth path for future product knowledge is the tool, not
  the prompt: adding a topic must not grow the resident system prompt beyond
  the tool docstring's topic-index line, and the inline guide's byte size
  stays pinned. Depth lives in topics; the guide keeps only the
  always-needed surface.

## Acceptance Criteria

- [ ] AC-1: Live A/B against an isolated daemon (release build with
  `tetond/llama`, the LESSON-482 isolation method): "I want to hook up Kimi
  for deep reasoning" yields the exact two commands — `teton provider add`
  with Moonshot's real endpoint and kind, then `teton policy set-tier think
  <id>` — with zero repository-search tool calls (at most one `teton_docs`
  call). Baseline pre-fix run recorded for comparison.
- [ ] AC-2: Same method: "How do I connect Claude?" answers with the
  `--kind anthropic` recipe (no endpoint flag) and the routing step; the
  control question ("What version is this crate? Check Cargo.toml.") still
  calls `read` — the tool path is unchanged.
- [ ] AC-3: `teton_docs` returns the providers topic containing every
  canonical recipe; an unknown topic returns the didactic error naming all
  valid topics (unit + e2e).
- [ ] AC-4: The prompt-margin tests pass on all profiles with recipes and
  tool docs present, and the recorded headroom is updated in the test's
  comments.
- [ ] AC-5: On the degraded profile (weak tool-calling cap) and in an
  offline session, `teton_docs` is present in the exposed tool list; the
  cap-posture assertion of BR-7 is pinned by a test that fails if the
  mechanism regresses to ordering luck.
- [ ] AC-6: Egress-capture suites show zero egress events for a session that
  calls `teton_docs`; existing redaction and provenance suites are
  unaffected.
- [ ] AC-7: Bidirectional drift gate: mutating a recipe in the typed source
  without updating each prose copy fails CI, and editing a prose copy's
  recipe facts without the typed source fails CI (both directions
  demonstrated once in review).
- [ ] AC-8: Per-topic ceiling test covers every bundled topic; its failure
  message instructs splitting or trimming the topic, not deleting the
  assertion (the BUG-160 regression-test posture).

## External Dependencies

- Vendor API documentation consulted at implementation time to verify recipe
  facts (Anthropic, OpenAI, Moonshot/Kimi, DeepSeek, Ollama, Grok/xAI). No
  runtime dependency — recipes ship as compiled-in data; nothing is fetched.

## Assumptions

- Vendor endpoint URLs are stable at release cadence; example model names may
  drift faster. Recipes label models as examples and `--model` is always
  user-suppliable, so staleness degrades to a slightly old example, not a
  broken command. Binary-versioned knowledge is acceptable freshness for this
  surface.
- The remaining inline-guide headroom (~1.4 KB after BUG-168) is enough for
  one-line recipes plus the referral sentence. If implementation finds it is
  not, the fallback posture is: recipes live only in the `teton_docs`
  providers topic, the guide keeps the generic shape, and AC-1 is satisfied
  via the one-docs-call path — BR-4's margins are never traded away.
- No new frame delimiters or envelopes are needed; topic bodies are plain
  markdown rendered through the existing untrusted-content framing. If
  architecture does introduce one, ADR-009's two-sided marker/neutralizer
  rule applies.
- The local model will call `teton_docs` when the topic index names the
  subject. BUG-168 showed prompt-adjacent behavior is chaotic under byte-level
  changes, so this is treated as unverified until AC-1/AC-2's live A/B —
  wording of the docstring may need the same dictation-style treatment as the
  web-off clause.
- The recipe set documents vendors; it blesses none. Recipes are prose an
  agent reads, never defaults the daemon applies (the REQ-563 BR-8 "no
  blessed backend" spirit).

## Open Questions

- [x] OQ-1: Exact canonical vendor roster. **Resolved (Brett, 2026-08-14):**
  Anthropic, OpenAI, Moonshot (Kimi), DeepSeek, Ollama, Grok (xAI) — the
  adapters the architecture diagram names, plus Grok.
- [x] OQ-2: Topic roster for MVP. **Resolved (Brett, 2026-08-14):**
  `providers`, `policy`, `web`, and `doctor` — the troubleshooting topic is
  in. Users reach for the agent precisely when something is broken, and the
  two failure modes with field evidence — a wrong auth shape presenting as a
  bad key (BUG-165) and CLI/daemon version skew — are exactly what a bounded
  troubleshooting topic answers. Marginal cost is one more BR-9-bounded topic.
- [x] OQ-3: BR-7's mechanism. **Resolved (Brett, 2026-08-14):** cap
  exemption. The cap exists because weak tool-callers drown in tool schemas;
  `teton_docs` is one string argument and a short docstring, so it barely
  adds to that load, and it is most needed on exactly the degraded profiles a
  cap-count approach would cut it from. A raised cap re-couples availability
  to an arithmetic coincidence — the trap LESSON-496 documents — while an
  enumerated exempt set is a rule a reader can check. The exemption rationale
  is recorded as distinct from web's (self-serving product knowledge, not
  user opt-in) so the exempt set does not quietly become a dumping ground.
- [x] OQ-4: Profile coverage for recipes. **Resolved (Brett, 2026-08-14):**
  uniform — recipes inline on every profile, including strong-model. A
  frontier model that half-knows a vendor can hallucinate a plausible
  endpoint into a runnable command, which fails later as a misleading
  connection/401 error (the BUG-165 failure texture); grounding it costs
  bytes already paid under BR-4. Forking the prompt per profile would double
  the regression-pin surface and the live A/B matrix that BUG-168 showed is
  expensive to verify. If the headroom fallback in Assumptions triggers, it
  applies uniformly too.

## Out of Scope

- External retrieval of any kind — RAG over the website, fetching vendor
  docs at runtime, auto-updating knowledge independent of releases.
- Fine-tuning local weights on Teton documentation.
- A human-facing `teton docs <topic>` CLI command (the same bundled data
  could back one later; not this REQ).
- Exposing `teton_docs` over MCP or to external clients.
- Executing provider setup on the user's behalf, auto-filling config from
  recipes, or any default/blessed provider — registration stays human-gated
  (REQ-575/576 territory).
- Restructuring the existing self-config guide beyond adding recipes and the
  referral sentence.

## Retrieved Context

- LESSON-493 (lesson, score 15): A prompt ending is only reachable if its knowledge source exists — bundle what only the product knows
- BUG-160 (bug, score 14): Asked how to hook up external models, the agent searches the user's repo — Teton's own setup instructions are not bundled
- BUG-168 (bug, score 12): The web-off clause loses both its duties on the local tier — the opt-in is never named, and the hunt it forbids is the hunt it causes
- LESSON-482 (lesson, score 12): A prompt that enumerates a turn's legal endings must name every one — the model can only stop in a way it was told about
- BUG-154 (bug, score 12): The system prompt describes no ending for a question that needs no files, so the model searches the repo instead of answering
- LESSON-475 (lesson, score 8): A marker must be anchored the way the renderer actually writes it — and scoped to what is never legitimate output
- LESSON-496 (lesson, score 7): "Cut first under pressure" means "never available" when the limit equals the count
- LESSON-515 (lesson, score 6): A feature-gated target is invisible to every refactor
- LESSON-518 (lesson, score 6): A blocking gate's reader-loop freedom is not inherited from the await-based reader-loop tests
- LESSON-519 (lesson, score 6): An 'assert by inspection, not from the error' AC needs the real artifact — add a refusing test seam to reach it
- LESSON-520 (lesson, score 6): A gate that fires before deserialization makes an invalid-payload test vacuous — use a persistable payload + a refuse/accept pair
- BUG-167 (bug, score 6): The llama-gated template smoke no longer compiles
- LESSON-512 (lesson, score 6): A spec's named example is a test case, not decoration
- BUG-165 (bug, score 6): The search credential only speaks Bearer, and the spec's own example backends do not
- LESSON-495 (lesson, score 6): A remembered grant answers every question its key matches — so the key must encode the whole question
