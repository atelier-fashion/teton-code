---
id: REQ-558
title: "Purpose-oriented routing categories as the runtime dispatch key"
status: complete
deployable: true
created: 2026-08-05
updated: 2026-08-12
component: "daemon/router"
domain: "routing"
stack: ["rust", "daemon", "cli"]
concerns: ["routing", "cost", "developer-experience"]
tags: ["routing-categories", "tiers", "freeform-heuristics", "phase-migration", "intent-classification"]
---

## Description

**`teton policy set` has no effect on a normal session.**

The configurable routing axis is `Phase` (`crates/teton-core/src/phase.rs`):
`Spec`, `Architect`, `Implement`, `Review`, `Io`, `Freeform`. The user sets a
phase→provider table with `teton policy set`, and `Router::resolve_structured`
reads it. But `resolve_structured` is only reached in `SessionMode::Structured`.
REQ-544 BR-3 makes **freeform the default experience**, and a freeform turn calls
`Router::resolve_freeform`, which ignores the policy table entirely and calls
`route_freeform` (`crates/tetond/src/heuristics.rs:128`).

What `route_freeform` does is pick between exactly two providers using a
hardcoded ten-word substring list:

```rust
const AUXILIARY_SIGNALS: &[&str] = &[
    "summarize", "summary", "classify", "classification", "triage",
    "commit message", "what does", "explain", "describe", "grep",
];
```

A prompt containing any of those words goes local; everything else goes to the
default provider. So *"explain the tradeoffs between these two architectures"*
routes to the 3B local model because it contains `explain`, and the user's
carefully configured policy table is not consulted. This is the failure mode
REQ-544's charter explicitly set out to avoid — *"rather than guessing task
difficulty from prompt text (the failure mode of generic routers)"* — reintroduced
in the mode that ships by default. It is also LESSON-482's family: the harness's
framing decides what the user meant, and the user cannot see or change the
enumeration doing the deciding.

This REQ replaces `Phase` as the **runtime dispatch key** with a purpose-oriented
category, dispatched identically in both modes, and makes the configurable table
the thing the runtime actually reads.

**Four tiers** — the primary configuration surface, what most users set:

| Tier | Shape of work | What dominates the choice |
|---|---|---|
| `reflex` | Sub-second, every turn, never leaves the machine | latency + privacy |
| `scan` | Read a lot, emit a little | context window + $/input-token |
| `build` | The agentic loop: read → edit → run → verify | tool-call fidelity |
| `think` | Design, debug, critique | reasoning depth |

**Eleven categories**, each inheriting its tier and individually overridable:

| Category | Tier | Why it is its own knob |
|---|---|---|
| `route` | reflex | Classifies the judgment categories. Must be local — a router that calls a remote model to decide has spent what it was saving |
| `redact` | reflex | Secret/PII scan before egress. **Pinned local, not configurable** |
| `title` | reflex | Session and branch naming |
| `digest` | scan | File and diff summarization into context |
| `compact` | scan | Conversation compaction. Split from `digest`: it decides what to *forget*, and a bad compaction silently corrupts every later turn |
| `triage` | scan | Ranking grep/glob hits |
| `edit` | build | Write code from a task artifact |
| `shell` | build | Command construction and output interpretation. Split from `edit`: "good at diffs" and "safe at `rm`" are different competencies |
| `design` | think | Architecture, decomposition, spec authoring |
| `debug` | think | Root-cause on a failure. Split from `design`: needs depth *and* long context; usually the most expensive single call in a session |
| `review` | think | Adversarial critique. Some users will deliberately pick a different vendor here than the one that wrote the code |

The load-bearing implementation split: **seven of the eleven are known at the
call site**. The harness does not guess that it is compacting — it *is*
compacting. `route`, `redact`, `title`, `digest`, `compact`, `triage`, and the
output-interpretation half of `shell` are tagged where they are invoked and must
never reach a keyword matcher. Only `edit`, `design`, `debug`, and `review`
require reading user intent, and only those consult the `route` category. The
current design guesses all of them from prompt text, which is why `"explain
this"` goes local even when the user meant a hard architectural question.

`Freeform` stops being a phase value and becomes what it always was — a session
*mode*. A freeform session classifies per call into the same eleven categories a
structured session uses; the difference between modes is where the four judgment
categories get their signal (ADLC artifacts vs. the `route` classifier), not
whether the policy table is consulted.

Depends on REQ-557: a category binds to a provider whose model is declared.

## System Model

### Entities

| Entity | Field | Type | Constraints |
|--------|-------|------|-------------|
| Tier | name | enum(reflex, scan, build, think) | required, closed set |
| Tier | provider_id | string | required, FK → ModelProvider (REQ-557) |
| Tier | fallback_id | Option\<string\> | optional; FK → ModelProvider |
| Category | name | enum — the eleven above | required, closed set |
| Category | tier | enum(reflex, scan, build, think) | required; the compile-time default binding |
| Category | provider_id | Option\<string\> | when `Some`, overrides the tier binding for this category |
| Category | fallback_id | Option\<string\> | optional per-category override |
| Category | **origin** | enum(harness_known, intent_classified) | **compile-time property, not configuration.** `redact` additionally carries `pinned_local` |
| Session | mode | enum(freeform, structured) | unchanged (REQ-544); no longer duplicated as a phase value |
| Session | phase | Option\<Phase\> | retained for ADLC artifact tracking and cost attribution; **no longer a routing input** |

`Phase` is not deleted — REQ-544 BR-2 attributes CostRecords by phase and
structured mode still gates on it. It stops being the router's dispatch key.

### Events

| Event | Trigger | Payload |
|-------|---------|---------|
| route_decided | unchanged trigger | gains `category` and `tier`; `phase` becomes optional. `reason` still names the signal that fired (REQ-544 BR-5) |
| cost_recorded | unchanged trigger | `CostRecord` gains `category`; `phase` retained and unchanged |

No new RPCs. `policy/set` and its `teton policy set` surface are replaced by a
category/tier equivalent (see OQ-2 for the command shape).

### Permissions

| Action | Roles Allowed |
|--------|---------------|
| Set a tier or category binding | the user only, via CLI or config-file edit — never inferable from model output or file content (REQ-544 permission posture) |
| Override the `redact` category | **nobody** — pinned local by construction, with no configuration path (BR-4) |
| Read the effective category→provider mapping | any attached client |

## Business Rules

- [ ] BR-1: The category is the runtime dispatch key in **both** session modes.
      Every model call resolves through the category→(override or tier)→provider
      chain, and the configured table is read on every turn — freeform included.
      A configuration surface the default experience does not consult is the
      defect this REQ exists to close.
- [ ] BR-2: A category whose `origin` is `harness_known` is tagged at its call
      site and **MUST NOT** be derived from prompt text. No keyword list, no
      substring match, no heuristic may assign `route`, `redact`, `title`,
      `digest`, `compact`, or `triage`. `AUXILIARY_SIGNALS` and the two-way
      `route_freeform` split are deleted, not relocated. (informed by LESSON-482,
      BUG-153)
- [ ] BR-3: Only `edit`, `design`, `debug`, and `review` consult the `route`
      category's classifier. Its output is a category, and every classification
      emits `route_decided` naming the category, the tier, the provider, and the
      signal that fired — REQ-544 BR-5's legibility promise applies to
      classification as it does to policy. A classifier failure falls back to a
      declared default category (BR-9), never to silence.
- [ ] BR-4: `redact` is **pinned to the local tier by construction** and has no
      configuration path — no tier inheritance, no per-category override, no
      config key that names it. Its purpose is to inspect content *before* it is
      allowed to leave the machine; a `redact` call that could be bound to a
      remote provider would send the content it exists to guard. The pin is
      expressed as an unconditional property of the category, not as a guard
      predicated on the absence of a binding. (informed by LESSON-432,
      LESSON-443)

      **Two mechanisms, deliberately distinct** (validation W1): `redact` is
      absent from the configurable category enum, so *resolution* has no branch
      for it and cannot be made to have one — that is the unconditional
      property. Separately, config *validation* rejects a `categories.redact`
      key at load and names the pin, so a user who tries is told why rather
      than having the key silently ignored. The second is a config-validation
      error, not the runtime guard this rule forbids. Note this is new
      machinery: `Config` has no `deny_unknown_fields`, so an unrecognized key
      is dropped silently today.
- [ ] BR-5: `route` MUST resolve to the local tier when the local tier is
      available. When it is not (below the hardware floor, benchmark-disabled,
      shed under memory pressure), classification is **bypassed** rather than
      routed remotely: the four judgment categories fall back to their declared
      default (BR-9) and `route_decided` says so. REQ-544 BR-8's rule holds —
      local-tier value is latency, and a remote classifier costs more than the
      routing decision saves.
- [ ] BR-6: Category resolution is one classifier with one precedence, reused by
      every surface that describes it. `route_decided`, `teton policy show` (or
      its successor), and any turn-failure sentence derive from the same
      function — two surfaces describing one routing state must not be able to
      drift apart. (informed by LESSON-456, BUG-152)
- [ ] BR-7: **Session taint overrides every category binding.** A session pinned
      to the local tier by boundary exposure or unknown-provenance results
      (REQ-544 BR-1 backstop, `runtime.rs:1167`) stays pinned regardless of what
      any category resolves to. Category routing is a cost decision; the boundary
      is a privacy guarantee, and this REQ must not weaken it. Verified by
      egress-capture, not by inspection (REQ-544 AC-5 posture). (informed by
      LESSON-432)
- [ ] BR-8: Tier inheritance is total and explicit: every category resolves to
      exactly one provider through override → tier → error. There is no implicit
      "first remote provider" and no synthesized provider id at any step; an
      unresolvable category names itself and its unset binding. (informed by
      BUG-146, LESSON-456, REQ-557 BR-4)
- [ ] BR-9: Each of the four intent-classified categories has a **declared
      default** used when classification is bypassed (BR-5) or fails. The default
      is `edit` — the coding-turn category — matching today's behavior that a
      non-auxiliary freeform prompt is a coding turn. The default is
      configuration-visible, not a hidden constant.
- [ ] BR-10: **Migration from `Phase` is mechanical and recorded.** An existing
      phase→provider table maps: `spec` + `architect` → `design`,
      `implement` → `edit` **and** `shell`, `review` → `review`, `io` →
      `digest` + `triage` + `title` + `compact`, and `freeform`'s entry is
      **dropped** (it was never read by the freeform path it named). Each
      one-to-many expansion is reported at migration time so a user who wanted
      `implement` and `shell` to differ knows to split them. Migration runs once.
- [ ] BR-11: `Phase` survives as a cost-attribution and ADLC-gating value and is
      removed from every routing signature. `Phase::Freeform` is retired — the
      freeform/structured distinction lives in `Session.mode`, which already
      carries it, and a value that exists in two places is a value that can
      disagree with itself.

      **Historical ledger rows must be handled explicitly** (validation W2). The
      cost ledger persists `phase` as SQLite `TEXT` and `phase_from_wire` maps
      an unrecognized string to `None`, so retiring the variant does not crash
      on an existing `cost.db` — but every historical `"freeform"` row silently
      moves from the `freeform` bucket to `none` in the per-phase rollup. This
      REQ keeps `Phase` *specifically* for cost attribution, so quietly
      rewriting that history is not acceptable as a side effect. The
      architecture must choose and record one of: retain a read-only
      `Phase::Freeform` for deserialization of historical rows; migrate the
      stored rows; or accept the reattribution and state it in the release
      note. Silence is the one option ruled out.
- [ ] BR-12: The category→provider resolution is a **pure function** in
      `teton-core` with no I/O and no clock, table-driven-testable for all
      eleven categories × (bound, inherited, unresolvable) — conventions.md
      already requires this of router policy. This is also what keeps the
      seven harness-known categories verifiable without a live model.
      (informed by LESSON-481)

## Acceptance Criteria

- [x] AC-1: In a **freeform** session with `think` bound to a frontier provider,
      the prompt *"explain the tradeoffs between these two architectures"*
      routes to the `design`/`think` binding — not to the local tier. This is the
      direct regression for the `AUXILIARY_SIGNALS` defect and fails against
      today's binary.
- [x] AC-2: A table-driven test iterates all eleven categories and asserts each
      resolves through override → tier → declared error, with no path producing a
      synthesized provider id. Removing a tier binding makes the corresponding
      category name itself in the failure. (BR-8)
- [x] AC-3: No `harness_known` category is reachable from any prompt-text path,
      enforced **by the type system**: the classifier's return type admits only
      the four judgment categories, so assigning `digest` from prompt text does
      not compile. A type-level guarantee subsumes the grep-style assertion this
      AC originally also asked for (validation I1) — if the type holds, the grep
      is redundant; if the grep is needed, the type is not doing its job. Build
      the type. (BR-2)
- [x] AC-4: `redact` has no configuration path — a config file setting
      `categories.redact` is rejected at load naming the pin, and a test asserts
      the resolution function returns the local provider for `redact` even when
      every tier is bound to a remote provider. (BR-4)
- [x] AC-5: With the local tier unavailable, a freeform coding prompt resolves
      through the BR-9 default with `route_decided` naming the bypass, and **no
      remote classification call is issued** — asserted by call count, not by
      output text. (BR-5)
- [x] AC-6: Egress-capture test: a session tainted by boundary content stays on
      the local tier for every subsequent turn with `think` bound to a remote
      provider and a `design`-classified prompt. Zero remote calls contain
      boundary content. (BR-7, REQ-544 AC-5 posture)
- [x] AC-7: Migration: a pre-REQ config with all six phase entries produces the
      documented category table, reports the `implement` → {`edit`,`shell`} and
      `io` → {`digest`,`triage`,`title`,`compact`} expansions by name, drops the
      `freeform` entry with a note, and does not re-run on second start.
- [ ] AC-8 **[MANUAL GATE — not CI-enforceable]**: `route_decided` for every turn carries a category, a tier, a
      provider, and a non-empty reason — asserted across a scripted session
      covering at least one harness-known and one intent-classified category.
      (REQ-544 BR-5)

      **Unticked deliberately, not overlooked.** carries a **recorded exception** in `docs/manual-verification.md`: a taint-pinned `route_decided` legitimately carries no category, so the 'every turn' wording is true only outside that case. Ticking it would overstate the claim. Do not tick without a
      recorded sign-off (REQ-547 AC-13 precedent).
- [x] AC-9: `Phase` appears in no routing signature (`resolve_*`, the policy
      table, `route_decided`'s dispatch input) while still appearing in
      `CostRecord`; `Phase::Freeform` no longer exists. Compile-level assertion
      plus a cost-attribution test.
- [x] AC-10: Mutation check — reintroducing a keyword match for any
      harness-known category, or removing the taint override in BR-7, makes at
      least one test red. (informed by LESSON-441, LESSON-479)
- [x] AC-11: **One resolver, asserted by construction** (BR-6). Every surface
      that describes a routing state — `route_decided`'s payload,
      `teton policy show` (or its successor), and the turn-failure sentence for
      an unresolvable category — is built from the return value of the single
      resolution function, and a test asserts they agree for the same input:
      resolve one category with a deliberately unset binding and assert the
      provider, category, tier, and reason are byte-identical across all three
      surfaces. A second call site computing its own answer must make this test
      red.

      This AC exists because BUG-155 shipped four instances of exactly this
      defect in this subsystem one REQ earlier — a rule enforced where it was
      convenient rather than where the decision is made (LESSON-484). BR-6 was
      the only business rule in this spec with no acceptance criterion.
- [x] AC-12: The BR-9 declared default is **configuration-visible**: it appears
      in the effective-configuration projection any attached client can read,
      and a test asserts it is reported rather than compiled in silently.
      Changing it in config changes the category a bypassed classification
      resolves to.

## External Dependencies

- **REQ-557 must land first.** A category binds to a provider whose `model` is
  declared and whose default is explicit; without it a category can only name a
  vendor, which is the limitation this REQ exists to remove.
- No new crates. `teton-core`'s policy module, the router, and the
  `route_decided` event all exist.

## Assumptions

- **OVERRIDDEN 2026-08-06 (product decision).** The assumption below — that a
  category with no call site ships "declared but unreached" — is no longer
  accepted. All eleven settings must do something. This does **not** change
  REQ-558's scope: the routing axis, the resolver, the schema and the six reached
  categories (`route`, `digest`, `edit`, `design`, `debug`, `review`) ship here.
  The five remaining call sites are scheduled as follow-up work, which ADR-A made
  cheap on purpose — no config migration, no schema change, no protocol change,
  each call site is a leaf:
    - **REQ-561** — `triage`, `shell`, `title`, `compact`.
    - **REQ-562** — `redact`, specced separately: it is a model call *inside* the
      egress choke point, BR-4 pins it local for a reason, and it needs its own
      answers to "what on a hit", "what on failure", and "how does it compose with
      the existing provenance checks". Wiring a privacy control as a task inside a
      routing REQ would give it the least scrutiny of anything in the change.

- The seven harness-known categories have identifiable single call sites today.
  `compact`, `digest`, and `triage` map onto the existing summarizer paths
  (`summarize_if_large`, the context assembler); `title`, `route`, and `redact`
  may not all exist yet as distinct calls — a category with no call site is
  **declared but unreached** in v1, which is honest, rather than being
  keyword-guessed to give it traffic. To be confirmed per category at
  architecture time.
- The four judgment categories can be classified acceptably by the local tier at
  its BR-8 latency duty. Today's classifier is a substring match, so any model
  is an improvement; the risk is latency, not accuracy. Dogfooding measures it.
- Eleven categories is not too many to configure because tier inheritance means
  the common path is four settings. If dogfooding shows users never override a
  category, collapsing to tiers alone is a future simplification, not a
  correction.
- `Session.phase` remains meaningful for structured mode's ADLC gates; this REQ
  narrows its role rather than deprecating it.
- id allocated with remote verification (no degradation warning from the
  allocator).

## Open Questions

**OQ-1, OQ-2, and OQ-5 must be closed as ADRs during `/architect`, not carried
into implementation** (validation W3/W4). OQ-1 and OQ-5 question the size of a
set the System Model declares *closed* and AC-2 tests exhaustively ("all eleven
categories") — a spec cannot both fix a closed enum and ask how many members it
has. OQ-2 determines a user-facing CLI contract the spec itself flags as a
breaking change. The remaining OQs (3, 4) are tuning questions that can ship
recorded.

- [ ] OQ-1: Does `shell` split further? Command *construction* is a `build`
      shape; interpreting a 5,000-line test log is a `scan` shape. One category
      with a `build` binding is the simpler answer and the current proposal, but
      it means log interpretation runs on the tool-fidelity model rather than the
      cheap-context one.
- [ ] OQ-2: What is the CLI surface — `teton policy set <category> <provider>`
      extended with tier support, or a new `teton route` noun? The existing
      command name says "policy" and the table is no longer phase-shaped; a
      rename is a breaking CLI change for a pre-alpha product.
- [ ] OQ-3: Should `route`'s classifier see the prompt only, or also the session
      history and the tool-call context? More context classifies better and costs
      more of the local tier's latency budget (REQ-544 BR-8).
- [ ] OQ-4: How does a category with no call site in v1 (BR-9/Assumptions) surface
      to the user — hidden from `policy show`, shown as "declared, unused", or
      omitted from the enum until it has traffic? Showing an unreached knob
      invites a user to tune something that does nothing.
- [ ] OQ-5: Does `debug` earn its split from `design` in v1, or is it a
      post-dogfooding addition? Both bind to `think` by default, so the split
      only pays off for a user who wants a longer-context model on `debug`.

## Out of Scope

- Provider `model` field and `default_provider` (REQ-557 — prerequisite).
- Reasoning effort as a per-category setting. Effort is one global session
  setting (REQ-559); this REQ deliberately does not add a second per-category
  dimension.
- Permission levels and the status line (REQ-560).
- ML-based or difficulty-scored routing. REQ-544's Out of Scope excludes an ML
  router in MVP; the `route` classifier is a category assignment, not a
  difficulty model.
- Removing `Phase` entirely — it stays for cost attribution and ADLC gating
  (BR-11).
- Per-category effort, per-category `max_tokens`, or any other second axis.
- Automatic category tuning from observed cost.

## Retrieved Context

- LESSON-456 (lesson, score 7): A `_`-discarded error is a silent downgrade — the daemon knew exactly why, and told the user something else
- BUG-146 (bug, score 7): First prompt after install fails with a message blaming the local engine for a config/timing problem
- REQ-544 (spec, score 6): Teton Code — hybrid local/remote AI coding agent with workflow-aware model routing
- REQ-555 (spec, score 6): In-session slash commands for the teton interactive CLI
- LESSON-482 (lesson, score 5): A prompt that enumerates a turn's legal endings must name every one
- BUG-152 (bug, score 5): A prompt typed while the local tier is still loading is reported as an error, not as a wait
- REQ-547 (spec, score 5): First-run local model consent
- REQ-556 (spec, score 4): Live model-loading progress in the interactive session
- BUG-153 (bug, score 4): /exit is not a command
- LESSON-481 (lesson, score 4): A gate that hides a feature from users also hides it from the test suite
- REQ-554 (spec, score 3): Local tier renders prompts through the model's native chat template
- REQ-549 (spec, score 3): Daemon process identity and interactive startup UX
- LESSON-475 (lesson, score 3): A marker must be anchored the way the renderer actually writes it
- LESSON-441 (lesson, score 3): A fix pass is new code — re-verify it adversarially, not by test count
- LESSON-432 (lesson, score 2): Provenance must derive from what a tool touches, not from an argument name

Note: `complete` treated as the local spelling of `deployed` for the spec-status
filter (precedent: REQ-555, REQ-556). The Step-1.6 delegated body-read timed out
(SIGTERM at 120s); the documented fallback path ran and the top-15 bodies were
read directly.
