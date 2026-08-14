---
id: REQ-576
title: "Presence attestation for config/set (the larger daemon-wide sibling)"
status: approved
deployable: true
created: 2026-08-14
updated: 2026-08-14
component: "daemon/session"
domain: "harness"
stack: ["rust", "daemon", "cli"]
concerns: ["security", "privacy"]
tags: ["attestation", "presence", "config-set", "register-provider", "privacy-boundary", "daemon-wide", "br-10b", "egress", "bug-162"]
priority: high
follow_up_to: REQ-575
---

> **Urgency (recorded at filing).** This is a **high-priority** follow-up, not a
> someday item. `config/set` is a *larger* daemon-wide config-writer than the
> `web/setup_commit` REQ-575 just gated (it can redirect egress via
> `RegisterProvider` and rewrite the privacy boundary via `SetPrivacyBoundary`),
> and it remains layer-(a)-only until this REQ lands. The stated residual in
> REQ-575's architecture (ADR-2) and REQ-572's finding-7 disposition both point
> here; leaving it open indefinitely is exactly the drift REQ-575 BR-5's standing
> obligation exists to prevent. Should follow REQ-575 promptly.

## Description

Filed by REQ-575's architecture phase (ADR-2) as the tracked follow-up to the
config/set sibling that `/validate` surfaced against REQ-575. REQ-575 gates
`web/setup_commit`; this REQ addresses the **larger** daemon-wide config writer
it deliberately left out of scope.

`config/set` (`handle_config_set` → `DaemonRuntime::apply_config_update`,
crates/tetond/src/server.rs) is live in dispatch and gated with
`refuse_daemon_wide` (REQ-570 BR-10 **layer a**) only. Its `ConfigUpdate` enum
includes `RegisterProvider` (an arbitrary remote-provider **egress endpoint**),
`SetPrivacyBoundary` (the **privacy boundary** itself — the product's second
visible promise), `SetTierBinding`, and `SetCategoryBinding`. It durably
rewrites `config.toml` and live-swaps the daemon-wide in-memory config, and is
reachable by the same NotDescendant same-UID process REQ-572 finding 7
describes. By REQ-575's own test it is a BR-10(b) commitment at least as strong
as `web/setup_commit`.

It was left to its own REQ because gating it **reverses a documented,
deliberate decision**: the `handle_config_set` comment records that it was
knowingly kept at layer (a) after BUG-162, on the "removes immediacy, not
capability" reasoning REQ-575's spec rejects. Reversing a recorded decision
that touches `SetPrivacyBoundary` warrants its own spec, review, and validation
rather than a rider on a web-setup REQ. Until this REQ lands, that layer-(a)
gate is a **stated residual**, not a silent one (REQ-575 ADR-2 / PR body).

This REQ also folds in the low-severity **consent-path `persist_web_tier`**
(reached via `permission/respond` + the `enable_permanent` option) for a
considered disposition: it is raise-only within an already-configured `[web]`
table and cannot author an endpoint or credential, so a reflex gate would put a
presence prompt on the ordinary "enable permanently" answer (a REQ-570 AC-8
regression concern). Decide gate-vs-accept explicitly rather than by omission.

## Business Rules

- [ ] BR-1: `config/set` is classified as a REQ-570 BR-10(b) daemon-wide
  commitment: in addition to the existing `refuse_daemon_wide` (layer a) gate,
  `handle_config_set` runs the shared `refuse_unattested_commitment` — the same
  live check `model/confirm`, `model/set`, and `web/setup_commit` run — through
  the same code path, not a parallel implementation. Because it may attest, it
  moves to `handle_client`'s `blocks_on_a_human` spawn path (REQ-575 ADR-1
  precedent) if it is not already off the reader loop.
- [ ] BR-2: The no-mechanism posture is REQ-570 BR-8's asymmetry, unchanged:
  builds with no presence mechanism degrade to the layer-(a) gate with the
  stated notice — `teton provider add` and tier/category binding gain **zero**
  new prompts on shipped builds.
- [ ] BR-3: The `teton provider add` and binding CLI flows are analyzed as an
  AC-8-style regression surface: on a `--features presence` build, prompting on
  a deliberate machine-wide config act is confirmed as intended (or scoped),
  not stumbled into. The single-client first-run path raises no *new* prompt
  beyond what REQ-570 already established for `model/confirm`.
- [ ] BR-4: The check is one line per method, mutation-deletable in isolation
  (LESSON-502/508), with its own tests at the config/set seam.
- [ ] BR-5: The BR-10(b) commitment set docs are updated to **four** methods.
- [ ] BR-6: The consent-path `persist_web_tier` receives an explicit,
  documented disposition (gate or accept-with-rationale) — never silence.
- [ ] BR-7: The `handle_config_set` BUG-162 comment is rewritten to record the
  reversal honestly: layer (a) alone was the pre-REQ-570 posture; a daemon-wide
  commitment now attests, and the "can edit config.toml directly" mitigation is
  insufficient for a commitment (the reasoning REQ-570/REQ-575 established).

## Acceptance Criteria

- [ ] AC-1: With a present-but-refusing verifier, a `config/set`
  (`RegisterProvider` and `SetPrivacyBoundary` cases) from an otherwise-allowed
  connection is refused with the attestation code; `config.toml` byte-identical
  on disk and the in-memory config not swapped (asserted by inspection).
- [ ] AC-2: With the shipped no-mechanism verifier, `config/set` lands and the
  stated degradation notice appears — `teton provider add` unaffected on
  shipped builds.
- [ ] AC-3: Mutation check — removing the attestation line from
  `handle_config_set` turns at least one test red, independently of the other
  three BR-10(b) seams.
- [ ] AC-4: The consent-path disposition (BR-6) is implemented and tested to
  the strength it is decided at.
- [ ] AC-5: The BR-10(b) docs name the four-method set; no stale three-method
  or "only those two" framing remains.
- [ ] AC-6: One recorded human pass on a `--features presence` macOS build:
  `teton provider add` (or a `config/set`) raises the OS presence prompt;
  approve lands, cancel refuses with nothing written.

## External Dependencies

- None. Reuses REQ-570's verifier + REQ-575's `blocks_on_a_human`/gating
  precedent.

## Assumptions

- **VALIDATED (2026-08-14, at /validate):** `config/set`'s current gating is
  `refuse_daemon_wide` only, it is `fn` (synchronous) in the `dispatch` match
  (not on `blocks_on_a_human`), and its `ConfigUpdate` enum carries
  `RegisterProvider`, `SetTierBinding`, `SetCategoryBinding`, `SetPrivacyBoundary`,
  `SetEffort`. Re-verify again at implementation start (id-recheck discipline).
- **VALIDATED:** REQ-575 has landed (merged as `c81e156`), so the three-method
  BR-10(b) set, the `blocks_on_a_human` precedent, and the
  `TETON_PRESENCE_ACCEPT=fail` test seam all exist to extend to four.
- **Architecture note (surfaced at /validate):** unlike `web/setup_commit`,
  `config/set` is a genuine `daemon_wide_method` (uses `refuse_daemon_wide`, the
  ancestry gate) — it is already in `daemon_wide_methods()` and `route_for_test`,
  so its BR-10(b) commitment coverage plugs directly into the existing shared
  `only_a_daemon_wide_commitment_demands_presence` harness rather than needing a
  session-scoped variant. `/architect` should leverage this.

## Open Questions

- [ ] OQ-1: Do any programmatic (non-CLI, non-user-initiated) `config/set`
  callers exist that a presence prompt would deadlock or break? Enumerate all
  `apply_config_update` call paths before gating.
- [ ] OQ-2: Should `SetPrivacyBoundary` carry a distinct, stronger prompt reason
  than `RegisterProvider`, given it mutates the privacy promise directly?

## Out of Scope

- `web/setup_commit` — closed by REQ-575.
- The REQ-569 ADR-A ancestry residual (the one-shell-word escape) — BR-10(b) is
  the compensating control, not a fix.
- Any attestation TTL / single-use / persistence change (REQ-570 BR-6 untouched).

## Retrieved Context

- Inherited from REQ-575 (same area): BUG-162 (model/confirm answerable by any
  connection), LESSON-504 (a gate's precondition is part of its claim),
  LESSON-502 (multi-seam invariant needs a test at each seam), LESSON-508 (a
  redundant guard needs its own test), LESSON-513 (a pre-authorization publish
  is attacker-paced). Full retrieval to be run when this REQ enters its own
  `/spec`/`/validate` cycle.
