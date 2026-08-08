---
id: REQ-562
title: "redact: a model-based secret and PII scan inside the egress choke point"
status: approved
deployable: true
created: 2026-08-07
updated: 2026-08-07
component: "daemon/egress"
domain: "privacy"
stack: ["rust", "daemon"]
concerns: ["security", "privacy", "correctness"]
tags: ["redact", "egress", "pinned-local", "fail-closed", "br1"]
---

## Description

`redact` is the eleventh routing category and the last with no call site. It is
specced alone, apart from REQ-561's four, for one reason: **it is a model call
inside the egress choke point** — the code path whose entire job is deciding what
may leave the machine. Every other category decides *where* a call goes. This one
decides *whether* content goes at all.

Today, egress enforcement is entirely **provenance-based** (`egress/inspector.rs`):
a payload is blocked when its provenance intersects a `local-only` boundary glob,
or when provenance is unknown (fail-closed, REQ-544 C-1). That catches content by
**where it came from**. It cannot catch a secret the model paraphrased into its own
prose, a key pasted into a prompt by hand, or a credential in a file nobody thought
to add a boundary for. `redact` is the content-based second pass.

**The danger is that it is a guard whose failure mode is silence.** A routing
category that fails routes nowhere and the user sees an error. A redactor that
fails *permissively* leaks, and nothing tells anyone. REQ-544 BR-1 is the product's
central promise; this REQ adds a component that can weaken it while appearing to
strengthen it.

**Its pin is already structural.** REQ-558 gave `redact` no `ConfigurableCategory`
variant, so no config, CLI, or RPC path can bind it (ADR-B), and TASK-057's fix
made the pin resolve to a provider id **only when that id is genuinely
engine-backed** — because a provider registered under the id `local` can be a
remote HTTP endpoint (BUG-156). `redact` inherits both. This REQ must not add a
runtime guard that re-implements either; it must consume them.

## System Model

### Entities

| Entity | Field | Type | Constraints |
|--------|-------|------|-------------|
| RedactionVerdict | outcome | enum(Clean, Findings, Unavailable) | required |
| RedactionVerdict | findings | Vec\<Finding\> | non-empty iff `Findings` |
| RedactionVerdict | scanned | bool | false when the scan did not run; the report must say so |
| Finding | kind | enum(secret, credential, pii, unknown) | required |
| Finding | span | byte range | required; **never the matched text itself** (see BR-6) |
| Finding | confidence | enum(high, low) | drives BR-4's action |

`redact` needs no config entity. It has no binding, no override, and no tier
inheritance — by construction (REQ-558 ADR-B).

### Events

| Event | Trigger | Payload |
|-------|---------|---------|
| privacy_block | **gains a redaction cause** | existing shape; `reason` distinguishes a provenance block from a redaction block |
| route_decided | per REQ-561 BR-2 | the duty announces itself like any other |

No new RPCs.

## Business Rules

- [ ] BR-1: **When enabled (BR-10), `redact` runs on the egress payload, after
      provenance inspection, and before a byte leaves.** It is a second pass, not a replacement: provenance
      still blocks first and blocks fail-closed on unknown. A payload that
      provenance already refuses is never sent to the redactor.
- [ ] BR-2: **`redact` is local, by construction, and this REQ adds no check to make
      it so.** It has no configurable counterpart (REQ-558 ADR-B) and the pin
      resolves to an engine-backed provider only (BUG-156 / TASK-057). Adding a
      locality guard here would be LESSON-484's error — enforcing a rule where it is
      convenient rather than where the decision is made — and LESSON-443's, since
      such a guard would be predicated on the absence of a binding that cannot exist.
- [ ] BR-3: **A redactor that cannot run FAILS CLOSED** (OQ-1, resolved). With
      `redact` enabled and no local tier able to serve it, the payload is
      **blocked** — a guard that cannot run does not become a guard that passes
      everything. `scanned: false` still rides on the report so the *reason* is
      legible: the user is told the scan could not run, not that it found
      something, and those are different problems with different fixes.

      This is affordable only because of OQ-3: nobody who has not opted in is
      affected, so the first-run and weights-still-downloading regressions REQ-547
      and BUG-152 guarded against do not apply.
- [ ] BR-4: **A high-confidence finding blocks; a low-confidence finding is the
      user's call** — and confidence is **derived, not self-reported** (OQ-2,
      resolved): a deterministic pattern hit is high-confidence by construction, a
      model-only hit is low. The alternative was trusting a 3B model's own estimate
      of its certainty, which is the least trustworthy thing in the pipeline. Blocking on every low-confidence hit makes the feature
      unusable and trains users to disable it; blocking on none makes it decorative.
      The action per confidence level is a stated rule, not an emergent one.
- [ ] BR-5: **`redact` never rewrites the payload in v1.** It blocks or it passes.
      Silently altering content the model then reasons about is a correctness hazard
      of a different kind, and "redaction" as *substitution* deserves its own REQ.
      The name is aspirational; the v1 behaviour is detection.
- [ ] BR-6: **A finding never quotes the matched text** — not in the event, not in
      the log, not in the turn-failure sentence. A secret detector that echoes the
      secret into a log has moved it, not caught it. Spans and kinds only. REQ-558's
      classifier established this rule for model output (`Failed` never quotes the
      model, because model output can echo the prompt); this is the same rule where
      it matters most.
- [ ] BR-7: **The scan is bounded** — input capped like every other duty (REQ-561
      BR-8), and a payload too large to scan is BR-3's "cannot run" case, not a
      silent pass.
- [ ] BR-8: **Session taint still overrides everything.** A tainted session is
      pinned local, so its payloads do not reach egress at all; `redact` does not
      change that ordering and is not a substitute for it.
- [ ] BR-9: **Latency is on the critical path of every remote turn.** Unlike
      REQ-558's classifier (freeform judgment turns only) and REQ-561's duties
      (threshold-triggered), this runs before every remote call. A stated budget and
      a measurement are acceptance criteria, not nice-to-haves.

- [ ] BR-10: **`redact` is off by default and enabled by a switch that is NOT a
      category binding** (OQ-3). The distinction is load-bearing and easy to get
      wrong: REQ-558 ADR-B removed `redact` from `ConfigurableCategory` so that
      *which provider serves it* is unconfigurable. An opt-in switch answers a
      different question — *whether it runs at all* — and the two are orthogonal.

      Putting the switch in the `[[categories]]` table would reintroduce exactly the
      surface ADR-B deleted, and would make `redact` deserializable as a
      configurable category again. It belongs in its own key (e.g. a `[privacy]`
      table), and `ConfigurableCategory` must still have no `Redact` variant
      afterwards.

      "Off" means genuinely off: no scan, no model load, no added latency, and no
      claim in the report that content was scanned.

## Acceptance Criteria

- [x] AC-1: A payload containing a planted secret **that provenance cannot catch** —
      a credential the model paraphrased into prose, with clean provenance and no
      matching boundary glob — is blocked, and `privacy_block` names redaction as
      the cause. This is the capability the REQ exists for and it fails against
      today's binary.
- [x] AC-2: A clean payload passes, and a test proves the scan actually ran
      (`scanned: true`) rather than being skipped — the non-vacuity pairing
      (LESSON-485).
- [x] AC-3: With no local tier, the BR-3 posture chosen in OQ-1 holds, asserted by
      captured bytes **and** by `scanned: false` in the report. No configuration
      makes the scan appear to have run when it did not.
- [x] AC-4: `redact` cannot be bound by any path — config file, `policy
      set-category`, `config/set` RPC, tier inheritance, or migration (BR-2).
      Inherited from REQ-558; re-asserted here because this REQ is the one that
      gives the pin consequences. A test also asserts **no locality guard was
      added** — the pin is the type and the engine-backed derivation, and a runtime
      check here would be LESSON-484's error.
- [x] AC-5: **The pin resolves to an engine-backed provider only.** With a
      remote-kind provider registered under the id `local`, `redact` does not
      dispatch over HTTP — asserted by captured bytes, not by an id comparison
      (BUG-156, LESSON-485).
- [x] AC-6: No finding, event, log line, or error message contains matched text.
      Asserted by planting a distinctive sentinel and grepping every emitted
      surface for it (BR-6).
- [x] AC-7: Latency measured on real weights and recorded against BR-9's stated
      budget. If it cannot be measured
      in CI, `docs/manual-verification.md` records the procedure and says **NOT
      RUN** — the standard REQ-557 and REQ-558 set.
- [x] AC-9: **The payload is never modified** (BR-5). A scan that finds nothing and
      a scan that finds something both leave the outbound bytes byte-for-byte
      identical to what provenance inspection passed through — the second case
      blocks, it does not send an altered payload. Asserted by capture, not by
      reading the code path. Without this, "v1 detects, it does not substitute" is
      a comment rather than a rule (LESSON-486).
- [x] AC-10: **Confidence drives the action** (BR-4). A table-driven test over
      (high, low) × (single finding, mixed findings) asserts which verdicts block
      and which pass, and that a low-confidence-only payload is not blocked. This
      is the rule that decides whether the feature is usable or decorative, and it
      is the one a later change is most likely to quietly retune.
- [x] AC-11: **The ordering holds** (BR-1). A payload that provenance already
      refuses is never handed to the redactor — asserted by a call count on the
      scanner, not by output text. Redaction is a second pass over content that
      provenance permitted, and a scanner that sees refused payloads is doing work
      on content that was never going anywhere.
- [x] AC-13: **Off by default, and off means off** (BR-10, OQ-3): with no
      `[privacy]` opt-in, a remote turn issues **zero** scanner calls — asserted by
      call count, not by output — and the egress report does not claim content was
      scanned. Enabling the switch and repeating the same turn produces a scan.
- [x] AC-14: **The switch is not a category binding** (BR-10): after this REQ,
      `ConfigurableCategory` still has no `Redact` variant, and a `[[categories]]`
      entry naming `redact` is still rejected at load naming the pin. A test asserts
      both, so the opt-in cannot quietly reopen the binding surface REQ-558 closed.
- [x] AC-12: **Session taint still short-circuits ahead of this** (BR-8): a
      tainted session's payloads never reach the redactor at all, asserted by a
      call count on the scanner. `redact` is a second line for content that was
      going to leave; it is not a substitute for the pin that stops content
      leaving, and a change that made it one would weaken BR-1 while appearing to
      strengthen it.
- [x] AC-8: Mutation checks — (a) making the unavailable-redactor path permissive,
      (b) removing the bound on scan input (BR-7), (c) letting a finding carry its matched
      text, and (d) restoring an id-based locality assertion each turn at least one
      test red. **A green mutation is reported, not quietly fixed** (LESSON-485).

## External Dependencies

- **REQ-558** (merged) — the category, its structural pin, and the engine-backed
  locality derivation.
- **REQ-561** — the shared `DutyRoute`/`Duty` seam and duty `route_decided`.
  `redact` should be the fifth caller of that seam, not a sixth bespoke path.
  If REQ-561 slips, this REQ inherits BR-6 of it.
- No new crates. OQ-2's recommendation (model **plus** a deterministic pattern
  pass) needs no dependency — the pattern chain already exists in the delegate
  redaction path and is a `sed`-equivalent regex sweep.
- **Not blocked on REQ-560.** OQ-4's recommendation is that v1 ships reporting and
  no override, which needs no permission model. If OQ-2 is settled model-only
  instead, revisit — false positives get likelier and the override question
  sharpens.

## Assumptions

- A 3B local model can identify obvious credentials and PII at acceptable recall.
  **This is the assumption most likely to be wrong**, and unlike REQ-558's
  classifier — where "any model beats a ten-word substring list" made accuracy a
  non-issue — here a miss is a leak. Dogfooding measures recall against planted
  sentinels before this is trusted.
- The egress payload is available as text at the choke point in a form a model can
  scan. `Egress` assembles a request body; whether that is scannable as-is or needs
  a text projection is an architecture-time question.
- Users will accept per-remote-call latency for this. If they will not, the honest
  outcome is an opt-in, not a faster and worse scan.

## Open Questions

- [x] OQ-1: **RESOLVED 2026-08-07 — FAIL CLOSED.** If the scan cannot run, the
      payload does not go. *(Original framing follows.)* **BR-3's posture — block or proceed-and-report when the redactor cannot
      run?** Fail-closed is the safer default and matches REQ-544 C-1's treatment of
      unknown provenance. Its cost was that a remote-only machine, or one whose
      weights are still downloading, could make no remote call at all — which
      REQ-547 and BUG-152 both went to some length to avoid.

      **OQ-3's resolution largely settles this.** With `redact` opt-in, a user who
      turns it on has accepted that it gates remote calls, and the first-run
      regression disappears — nobody who has not opted in is affected. The
      recommendation was therefore fail closed, and that is the decision: a
      redactor that cannot run does not become a redactor that passes everything.
      BR-3's `scanned: false` still exists so the *reason* is legible — a blocked
      payload says the scan could not run, not that it found something.
- [x] OQ-2: **RESOLVED 2026-08-07 — BOTH.** Model plus a deterministic pattern
      pass. *(Original framing and rationale follow.)* Model-only, or model plus a deterministic pattern pass (the
      `sk-`/`AKIA`/`ghp_`/`Bearer`/`*_API_KEY=` shapes already used in the delegate
      redaction chain)?

      **Recommendation: both.** The two fail in opposite directions, which is the
      whole argument. Patterns have near-perfect precision and poor recall — a
      string matching `AKIA[A-Z0-9]{16}` essentially *is* an AWS key, but the pass
      is blind to anything off-shape. A model has moderate recall and *uncertain*
      precision: it catches what patterns structurally cannot (a key paraphrased
      into prose, a credential described rather than pasted, PII with no fixed
      shape) and can both miss and invent.

      **This also largely answers BR-4.** Confidence has to come from somewhere,
      and model-only means trusting a 3B model's self-reported confidence — the
      thing least worth trusting. Running both derives it structurally: a pattern
      hit is high-confidence by construction, a model-only hit is low. BR-4 stops
      being a judgment call and becomes a consequence of the design.

      Cost is roughly a wash — a regex sweep is microseconds against a model call
      of hundreds of milliseconds.

      **The argument against**, recorded because it is real: a pattern pass makes
      the feature *look* like it works, which reduces the pressure to measure the
      model's recall — and the model is the part that justifies this REQ existing.
      Mitigation: scope the dogfooding measurement to *"what did the model catch
      that patterns did not?"* That question, not raw recall, is the honest test of
      whether the model call earns its latency.
- [x] OQ-3: **RESOLVED 2026-08-07 — `redact` is opt-in in v1.** An always-on scan
      on every remote call is a large behaviour and latency change for a guarantee
      users have not asked for yet, and BR-9 puts it on the critical path of every
      remote turn. Opt-in ships the capability and lets its recall be measured
      before anything depends on it. See BR-10 for the switch, and note what the
      switch must **not** be.
- [x] OQ-4: **RESOLVED 2026-08-07 — v1 ships good reporting and no override; does
      NOT sequence behind REQ-560.** The contingency on OQ-2 is discharged (see
      final paragraph). *(Original framing follows.)* What does a user *do* with a block?

      **This is two questions, and only one of them needs REQ-560.**

      *"Can the user act?"* is answerable now. BR-6 forbids quoting the matched
      text, so a block currently offers "a credential was detected at bytes
      1400–1436" — useless, because the user cannot see those bytes. But kind +
      span + **which content block it came from** is both permitted and actionable:
      *"a credential-shaped string in the file you read at `src/config.rs`"* tells
      them what to fix without echoing the secret. That is a reporting decision,
      independent of permissions.

      *"Can the user override?"* needs REQ-560 — any "send anyway" is a permission
      prompt with a durable answer, which is that REQ's subject.

      **Recommendation: v1 ships good reporting and no override, and this REQ does
      NOT sequence behind REQ-560.** OQ-3's opt-in resolution is what makes that
      safe: a user who hits a false positive can turn `redact` off, so a blocked
      turn is never a true dead end.

      **The risk in relying on that**, recorded because it is the likely failure
      mode: the escape hatch is all-or-nothing. One false positive and the safety
      feature gets switched off permanently — the classic shape where a noisy check
      trains people to disable it. That is an argument for getting OQ-2 right
      (patterns keep precision high) rather than for building an override.

      **The contingency is discharged (2026-08-07).** OQ-2 resolved to "both", so
      the pattern pass keeps precision high, false positives stay rare, and the
      all-or-nothing hatch should be pulled rarely. The recommendation therefore
      stands and **REQ-562 does not sequence behind REQ-560**. If dogfooding shows
      false positives are common anyway, that is the signal to revisit — the
      trigger to watch is users disabling `redact` after a block, not the raw
      false-positive count.

## Out of Scope

- **Substitution.** v1 detects and blocks; replacing content with placeholders is a
  separate REQ (BR-5).
- Redacting content at rest, in the ledger, or in logs.
- Any configurable binding for `redact` — the pin is the point.
- The other ten categories (REQ-558, REQ-561).

## Retrieved Context

- REQ-544 BR-1 (charter) — the privacy guarantee this extends, and C-1's
  fail-closed treatment of unknown provenance
- REQ-558 ADR-B — `redact`'s structural pin, and why no runtime guard belongs here
- BUG-156 — a privacy pin bypassed by a recovery path; the reason AC-5 asserts
  captured bytes rather than an id
- LESSON-447 — a guard's failure fallback must preserve the guarded invariant
- LESSON-484 — enforce the rule where the decision is made
- LESSON-485 — a fixture that cannot discriminate is not a test
- LESSON-432 — privacy claims need egress-capture tests, not code inspection
