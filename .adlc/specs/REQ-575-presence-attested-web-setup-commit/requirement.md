---
id: REQ-575
title: "Presence attestation for the web setup commit"
status: complete
deployable: true
created: 2026-08-14
updated: 2026-08-14
component: "daemon/session"
domain: "harness"
stack: ["rust", "daemon", "cli"]
concerns: ["security", "privacy"]
tags: ["attestation", "presence", "consent", "web-search", "config", "setup", "opt-in", "daemon-wide", "br-10b", "egress", "web-setup-commit"]
---

## Description

REQ-572's security review (finding 7) traced a path this REQ closes: a
same-UID process breaks the ancestry chain with `setsid` — the one-shell-word
escape REQ-569's architecture records — handshakes as `NotDescendant`, mints
its own session via `session/create` (which auto-attaches, satisfying the
`may_drive` gate), and calls `web/setup_commit`. That call durably rewrites
`config.toml` and live-swaps the daemon's in-memory config for **every**
session, with no human in the loop.

REQ-570 BR-10(b) already defines the class this belongs to: *a daemon-wide
commitment additionally requires a presence attestation, because its blast
radius is the whole machine rather than one session.* `model/confirm` and
`model/set` carry that check (`refuse_unattested_commitment`); the check's own
documentation froze the set at "those two methods, and only those two."
REQ-572 then added a third daemon-wide commitment — the web setup commit —
without re-running the BR-10(b) classification. This REQ is the spec-level
decision the review deferred, and it decides **extend, not accept**:
`web/setup_commit` joins the BR-10(b) commitment set.

Why extend rather than formally accept the residual:

1. **The commit passes BR-10(b)'s own classification test.** It is a durable
   daemon-wide config write plus a machine-wide live swap. And what it commits
   is the egress boundary itself: the `[web]` table names the search endpoint
   URL, the credential header shape (BUG-165 made it configurable), the key
   reference, and which tiers are enabled — and IP-literal/localhost endpoints
   are accepted by design (REQ-563). An attacker-authored commit therefore
   redirects every session's lookups to a server the attacker chooses and can
   enable tiers the user never opted into. Privacy boundaries are the
   product's second visible promise; a model swap arguably has a *smaller*
   blast radius than this, and the model swap is gated.

2. **The mitigating argument proves too much.** The review's mitigating
   context — a same-UID process can already edit `config.toml` directly and
   restart the daemon, so the marginal capability is "only" the no-restart
   live swap — applies verbatim to `model/set`, and REQ-570 gated `model/set`
   anyway. The distinguishing fact runs in favor of gating: the direct-edit
   path needs a daemon restart, which is loud (sessions drop, multi-GiB
   weights reload), while the RPC path is silent and immediate. Attestation
   prices exactly the quiet path, which is the one an attacker wants.

3. **The cost is one reused mechanism, not a new one.** The guard, its
   fail-closed/degrade split, its fixtures (`AcceptingVerifier`,
   `AlwaysFailsVerifier`, `TETON_PRESENCE_ACCEPT` behind `TETON_TEST_SEAMS`),
   and its platform posture all exist. On builds with no presence mechanism
   (every default/CI build) the commitment check deliberately **degrades to
   the existing gate with a stated notice rather than refusing** (REQ-570
   BR-8's asymmetry), so `/web setup` keeps working everywhere it works today
   with zero new prompts. Only a macOS `--features presence` build gains a
   prompt — one OS presence check at commit time.

Recorded honestly, at the strength it has: this brings `web/setup_commit` to
**parity with `model/set`** — protection is real where a presence mechanism
exists and stated-but-degraded where none does. **The shipped release build is
one of the degraded ones**: it is built without the `presence` feature (because
REQ-570's mechanism is not yet release-verified — its AC-3b manual pass is
outstanding), so on the artifact users install this control degrades to allow
and finding 7's exposure persists there, exactly as `model/set`'s does. Enabling
`presence` on release builds is a REQ-570-scope decision, not this REQ's. It does
not close the REQ-569 ADR-A ancestry escape, and it does not claim to.

## System Model

### Entities

No new entities. The check reuses REQ-570's `PresenceAttestation`,
`MechanismAvailability`, and refusal taxonomy unchanged; per REQ-570's design
the commitment-path check is live and never recorded into the attestation
registry (there is no consent `request_id` to bind to), and that property
carries over here by construction.

### Events

No new events. The existing degradation notice (stderr, "daemon-wide
commitment allowed on connection standing alone") now also fires for a
degraded web setup commit; the REQ-572 BR-4 rejection event and its
per-connection budget are untouched.

### Permissions

| Action | Roles Allowed |
|--------|---------------|
| `web/setup_commit` | a connection attached to the named session (REQ-572 BR-4, unchanged) **and** passing the BR-10(b) presence check where a mechanism exists |
| `web/setup_plan`, `web/setup_preview` | unchanged (session gate only — reads stay layer (a)) |

## Business Rules

- [ ] BR-1: `web/setup_commit` is classified as a daemon-wide commitment under
  REQ-570 BR-10(b): in addition to the REQ-572 BR-4 session gate, the handler
  runs the **same** live presence check `model/confirm` and `model/set` run,
  through the same code path — not a parallel implementation of it (informed
  by REQ-570, BUG-162).
- [ ] BR-2: The presence check runs only after the existing refusals
  (unmintable session id, session access). A caller that may not act here at
  all is refused without a prompt appearing on anyone's screen — the
  `model/confirm` ordering rationale, applied unchanged (informed by REQ-570).
- [ ] BR-3: The no-mechanism posture is REQ-570 BR-8's asymmetry, unchanged: a
  build with no presence mechanism **degrades to the session gate with the
  stated notice, never refuses**. `/web setup` keeps working on default builds
  with zero new prompts or steps (informed by REQ-570 BR-8, LESSON-443).
- [ ] BR-4: The check is one line at the top of the commit handler, per
  method, so a mutation check can delete exactly it and turn a test red
  independent of the sibling checks (informed by LESSON-502, LESSON-508).
- [ ] BR-5: Every doc that scopes BR-10(b) to "those two methods, and only
  those two" is updated to the three-method set, and the classification rule
  is restated as a standing obligation: **any future method that durably
  rewrites `config.toml` or live-swaps daemon-wide in-memory state must be
  classified against BR-10(b) in its own architecture phase.** This REQ exists
  because REQ-572 added such a method without running that classification
  (informed by REQ-572, LESSON-504).
- [ ] BR-6: The strength is recorded honestly everywhere it is described:
  parity with `model/set`, real on presence builds, stated-but-degraded
  elsewhere, and no claim of closing the REQ-569 ancestry escape (informed by
  REQ-569, REQ-570).

## Acceptance Criteria

- [ ] AC-1: With a present-but-refusing verifier (`AlwaysFailsVerifier`), a
  commit from a properly attached connection is refused with the existing
  attestation error code, `config.toml` is byte-identical on disk, and the
  in-memory config is not swapped — asserted by inspecting both, not inferred
  from the error (informed by REQ-570).
- [ ] AC-2: With an accepting verifier, the full REQ-572 flow — plan →
  preview → commit → live pickup without restart — passes unchanged.
- [ ] AC-3: With the shipped no-mechanism verifier (the default build's), the
  commit lands, the stated degradation notice appears, and `/web setup` gains
  zero new prompts or steps — REQ-570 AC-8's regression bar applied to this
  flow (informed by REQ-570).
- [ ] AC-4: An unattached caller, and a caller with an unmintable session id,
  are each refused **before the verifier is consulted** — asserted with a
  verifier double that fails the test if touched (BR-2).
- [ ] AC-5: Mutation check — removing the attestation line from the commit
  handler makes at least one test red, independently of the `model/confirm`
  and `model/set` seams (informed by LESSON-441, LESSON-502, LESSON-508).
- [ ] AC-6: The spawned-binary e2e path drives an attested `/web setup` commit
  through the same seams the REQ-570 acceptance suite uses
  (`TETON_TEST_SEAMS` + `TETON_PRESENCE_ACCEPT`), and the release-build
  refusal of those seams is untouched.
- [ ] AC-7: No stale "only those two methods" scoping claim survives in the
  BR-10(b) documentation; the three-method set is named where the split is
  explained.
- [ ] AC-8: One recorded human pass on a macOS `--features presence` build:
  the commit raises the OS presence prompt; approval lands the commit; cancel
  refuses it with nothing written. Recorded in `docs/manual-verification.md`
  at the strength actually verified — not satisfied by reasoning, a test, or
  the seam (informed by REQ-556, REQ-570).
- [ ] AC-9: REQ-572's architecture record for security-review finding 7 gains
  the disposition: residual closed by this REQ (the BUG-162 Resolution
  precedent — the record points at the closure rather than restating it).

## External Dependencies

- None. Reuses REQ-570's verifier trait, mechanism, fixtures, and error
  taxonomy.

## Assumptions

- The commit handler can adopt the same async shape as the two existing
  commitment handlers without protocol or client change; the dispatch already
  serves async handlers.
- The existing synthetic connection-scoped binding id the commitment check
  uses is sufficient for a third method; nothing here needs the attestation
  registry (REQ-570's live-check design carries over).
- The CLI needs no changes for the attested path: the presence prompt is
  raised daemon-side during the RPC, exactly as `model/set` clients experience
  it today.

## Open Questions

- [ ] OQ-1: Should the presence prompt carry a commitment-specific reason
  ("enable web lookup for this machine") where the mechanism supports one, so
  the human is told *what* they are approving rather than a generic daemon
  sentence? Architecture decides; the generic sentence is acceptable for v1.
- [ ] OQ-2: Should BR-5's standing classification obligation also land as a
  checklist line in the architecture template, so the next daemon-wide
  commitment method cannot skip it silently? (Template changes ride
  `/template-drift` conventions; deciding this here would be scope creep.)
- [x] OQ-3 (raised at `/validate`, deferred to this phase by product owner) —
  **RESOLVED in architecture (ADR-2); follow-up REQ-576 has since LANDED.**
  `config/set` (the larger sibling) is gated in its own tracked follow-up,
  **REQ-576** (now implemented/merged), not folded here: gating it
  reverses a documented BUG-162 decision and touches `SetPrivacyBoundary`, so
  it warrants its own spec/review rather than a rider on a web-setup REQ. The
  consent-path `persist_web_tier` is scoped out as a documented low-severity
  residual (raise-only, cannot author the endpoint), with its disposition
  folded into REQ-576. REQ-575 closes finding 7 as filed (`web/setup_commit`)
  and states the `config/set` residual plainly; it does **not** claim a
  complete BR-10(b) set — after REQ-575 the set is three methods, and REQ-576
  makes it four. Original finding preserved below.
    - **`config/set`** (`handle_config_set`, server.rs) — live in dispatch,
      gated with **`refuse_daemon_wide` (layer a) only**. Through
      `apply_config_update` it durably rewrites `config.toml` and swaps the
      daemon config mutex, and its `ConfigUpdate` enum carries
      `RegisterProvider` (an arbitrary remote-provider **egress endpoint**),
      `SetTierBinding`, and `SetPrivacyBoundary` (the **privacy boundary**
      itself). Its own comment carries the "removes immediacy, not capability /
      same-UID can edit `config.toml` directly" rationale — verbatim the
      argument this REQ's Description calls proves-too-much. By this REQ's own
      test it is a BR-10(b) candidate at least as strong as `web/setup_commit`.
      The counter-weight `/architect` must price: gating it raises a presence
      prompt on `teton provider add`, an AC-8-style regression surface.
    - **Consent-path `persist_web_tier`** (reached via `permission/respond` +
      the `enable_permanent` option, `may_drive`-gated) — durably writes
      `config.toml` and swaps in-memory config. Lower severity: **raise-only
      within an already-configured `[web]` table**, cannot set the endpoint or
      credential. Still meets BR-5's literal trigger; classify or scope out
      with rationale.

## Out of Scope

- `web/setup_plan` and `web/setup_preview` — reads; REQ-572's gates unchanged.
- `web/override` and `session/permissions` — session-scoped, not daemon-wide;
  layer (a) reasoning applies as before.
- `config/get`, `cost/query`, `web/refresh` — BR-10(b)'s split deliberately
  keeps them at layer (a); prompting for a cache eviction trains users to
  click through the prompt that matters.
- The REQ-569 ADR-A ancestry residual itself (the one-shell-word escape) — out
  of reach of this layer by construction; BR-10(b) is the compensating
  control, not a fix.
- Linux polkit enablement or any BR-11 posture change.
- Attestation persistence, TTL, or single-use semantics (REQ-570 BR-6
  untouched).

## Retrieved Context

- LESSON-501 (lesson, score 11): Carried state sheds its invariants silently
- BUG-162 (bug, score 10): model/confirm answerable by any connection
- BUG-161 (bug, score 9): Permission request ids collide across concurrent sessions
- LESSON-512 (lesson, score 8): A named example is a test case
- BUG-165 (bug, score 8): The search credential only speaks Bearer
- LESSON-504 (lesson, score 8): A gate's precondition is part of its claim
- LESSON-495 (lesson, score 8): A grant is only as narrow as its key
- LESSON-513 (lesson, score 7): A pre-authorization publish is attacker-paced
- LESSON-502 (lesson, score 7): A multi-seam invariant needs a test at each seam
- LESSON-503 (lesson, score 7): Mint ids at the scope that resolves them
- LESSON-505 (lesson, score 7): Audit controls are judged in the adversarial case
- LESSON-494 (lesson, score 7): Gate on the parse the executor will use
- LESSON-508 (lesson, score 6): A redundant guard needs its own test
- LESSON-432 (lesson, score 6): Provenance from files, not arg name
- BUG-156 (bug, score 5): A tainted session can fail over to a remote provider
