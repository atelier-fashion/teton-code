---
id: REQ-562
title: "redact: a model-based secret and PII scan inside the egress choke point"
status: draft
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

- [ ] BR-1: **`redact` runs on the egress payload, after provenance inspection, and
      before a byte leaves.** It is a second pass, not a replacement: provenance
      still blocks first and blocks fail-closed on unknown. A payload that
      provenance already refuses is never sent to the redactor.
- [ ] BR-2: **`redact` is local, by construction, and this REQ adds no check to make
      it so.** It has no configurable counterpart (REQ-558 ADR-B) and the pin
      resolves to an engine-backed provider only (BUG-156 / TASK-057). Adding a
      locality guard here would be LESSON-484's error — enforcing a rule where it is
      convenient rather than where the decision is made — and LESSON-443's, since
      such a guard would be predicated on the absence of a binding that cannot exist.
- [ ] BR-3: **A redactor that cannot run FAILS CLOSED or is DECLARED OFF — never
      silently permissive.** The two acceptable postures, and the choice is OQ-1:
      (a) no local tier ⇒ the payload is blocked; (b) no local tier ⇒ redaction is
      **reported as not performed** and egress proceeds on provenance alone. What is
      forbidden is proceeding while implying the content was scanned. `scanned:
      false` exists to make that impossible to fudge.
- [ ] BR-4: **A high-confidence finding blocks; a low-confidence finding is the
      user's call.** Blocking on every low-confidence hit makes the feature
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

## Acceptance Criteria

- [ ] AC-1: A payload containing a planted secret **that provenance cannot catch** —
      a credential the model paraphrased into prose, with clean provenance and no
      matching boundary glob — is blocked, and `privacy_block` names redaction as
      the cause. This is the capability the REQ exists for and it fails against
      today's binary.
- [ ] AC-2: A clean payload passes, and a test proves the scan actually ran
      (`scanned: true`) rather than being skipped — the non-vacuity pairing
      (LESSON-485).
- [ ] AC-3: With no local tier, the BR-3 posture chosen in OQ-1 holds, asserted by
      captured bytes **and** by `scanned: false` in the report. No configuration
      makes the scan appear to have run when it did not.
- [ ] AC-4: `redact` cannot be bound by any path — config file, `policy
      set-category`, `config/set` RPC, tier inheritance, or migration (BR-2).
      Inherited from REQ-558; re-asserted here because this REQ is the one that
      gives the pin consequences. A test also asserts **no locality guard was
      added** — the pin is the type and the engine-backed derivation, and a runtime
      check here would be LESSON-484's error.
- [ ] AC-5: **The pin resolves to an engine-backed provider only.** With a
      remote-kind provider registered under the id `local`, `redact` does not
      dispatch over HTTP — asserted by captured bytes, not by an id comparison
      (BUG-156, LESSON-485).
- [ ] AC-6: No finding, event, log line, or error message contains matched text.
      Asserted by planting a distinctive sentinel and grepping every emitted
      surface for it (BR-6).
- [ ] AC-7: Latency measured on real weights and recorded against BR-9's stated
      budget. If it cannot be measured
      in CI, `docs/manual-verification.md` records the procedure and says **NOT
      RUN** — the standard REQ-557 and REQ-558 set.
- [ ] AC-9: **The payload is never modified** (BR-5). A scan that finds nothing and
      a scan that finds something both leave the outbound bytes byte-for-byte
      identical to what provenance inspection passed through — the second case
      blocks, it does not send an altered payload. Asserted by capture, not by
      reading the code path. Without this, "v1 detects, it does not substitute" is
      a comment rather than a rule (LESSON-486).
- [ ] AC-10: **Confidence drives the action** (BR-4). A table-driven test over
      (high, low) × (single finding, mixed findings) asserts which verdicts block
      and which pass, and that a low-confidence-only payload is not blocked. This
      is the rule that decides whether the feature is usable or decorative, and it
      is the one a later change is most likely to quietly retune.
- [ ] AC-11: **The ordering holds** (BR-1). A payload that provenance already
      refuses is never handed to the redactor — asserted by a call count on the
      scanner, not by output text. Redaction is a second pass over content that
      provenance permitted, and a scanner that sees refused payloads is doing work
      on content that was never going anywhere.
- [ ] AC-12: **Session taint still short-circuits ahead of this** (BR-8): a
      tainted session's payloads never reach the redactor at all, asserted by a
      call count on the scanner. `redact` is a second line for content that was
      going to leave; it is not a substitute for the pin that stops content
      leaving, and a change that made it one would weaken BR-1 while appearing to
      strengthen it.
- [ ] AC-8: Mutation checks — (a) making the unavailable-redactor path permissive,
      (b) removing the bound on scan input (BR-7), (c) letting a finding carry its matched
      text, and (d) restoring an id-based locality assertion each turn at least one
      test red. **A green mutation is reported, not quietly fixed** (LESSON-485).

## External Dependencies

- **REQ-558** (merged) — the category, its structural pin, and the engine-backed
  locality derivation.
- **REQ-561** — the shared `DutyRoute`/`Duty` seam and duty `route_decided`.
  `redact` should be the fifth caller of that seam, not a sixth bespoke path.
  If REQ-561 slips, this REQ inherits BR-6 of it.
- No new crates. Whether detection is model-only or model-plus-pattern is OQ-2.

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

- [ ] OQ-1: **BR-3's posture — block or proceed-and-report when the redactor cannot
      run?** Fail-closed is the safer default and matches REQ-544 C-1's treatment of
      unknown provenance. But it makes a remote-only machine, or one whose weights
      are still downloading, unable to make any remote call at all — which REQ-547
      and BUG-152 both went to some length to avoid. This is the central decision of
      the REQ.
- [ ] OQ-2: Model-only, or model plus a deterministic pattern pass (the
      `sk-`/`AKIA`/`ghp_` shapes already used in the delegate redaction chain)?
      Patterns are fast, precise, and catch the common case; the model catches
      paraphrase. Running both costs one model call plus a regex sweep and makes
      recall explainable.
- [ ] OQ-3: Is `redact` opt-in in v1? An always-on scan on every remote call is a
      large behaviour and latency change for a guarantee users have not asked for
      yet. Opt-in ships the capability and measures it; always-on is the honest
      reading of BR-1's promise.
- [ ] OQ-4: What does a user *do* with a block? A blocked turn with "a credential
      was detected at bytes 1400–1436" and no way to proceed is a dead end. An
      override needs a permission model — which is REQ-560's subject, so this may
      need to sequence after it.

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
