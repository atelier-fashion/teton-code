# REQ-576 — Architecture: presence attestation for config/set

## Approach

`config/set` becomes the **fourth** REQ-570 BR-10(b) daemon-wide commitment,
joining `model/confirm`, `model/set`, and (REQ-575) `web/setup_commit`. It runs
the *same* `refuse_unattested_commitment` check. Because `config/set` is a
**genuine `daemon_wide_method`** — its layer (a) gate is `refuse_daemon_wide`
(the ancestry gate), not `web/setup_commit`'s session-scoped `may_drive` — it
plugs directly into the existing shared commitment-test harness, so most of its
coverage comes from *adding it to one list* rather than new fixtures. This is the
cleaner half of the split REQ-575's OQ-3 anticipated.

The mechanism is entirely reused (REQ-570 verifier, REQ-575's `blocks_on_a_human`
move and `TETON_PRESENCE_ACCEPT=fail` seam). What is genuinely new here is only:
(1) reversing config/set's documented BUG-162 layer-(a)-only posture, and (2) a
considered disposition for the consent-path `persist_web_tier`.

## Key decisions

### ADR-1: config/set is a BR-10(b) commitment, gated through the shared seam and shared test harness

**Decision**: add `refuse_unattested_commitment(daemon, conn, &id).await` to
`handle_config_set`, **after** `refuse_daemon_wide` and **before**
`apply_config_update`. Make `handle_config_set` `async`. **Remove** its arm from
the `dispatch` match and add `ConfigSetParams::METHOD` to `handle_client`'s
`blocks_on_a_human` set — the `matches!` guard, the if/else-if spawn chain (a new
explicit branch ahead of REQ-575's `unreachable!()` else), and the test router
`route_for_test`. **Add `ConfigSetParams::METHOD` to the `commitments` list** in
`only_a_daemon_wide_commitment_demands_presence` (server.rs ~4287).

**Rationale**: this is BR-1. Ordering after `refuse_daemon_wide` mirrors
`model/confirm` (a caller that fails the ancestry gate is refused with no prompt,
BR-2). Reusing `refuse_unattested_commitment` is BR-1/LESSON-499. The
`commitments`-list addition is the payoff of config/set being a real
`daemon_wide_method`: the shared harness already iterates `daemon_wide_methods()`
(which includes config/set) through `route_for_test`, so once config/set is in
`commitments`, `only_a_daemon_wide_commitment_demands_presence` asserts it
**refuses** under `AlwaysFailsVerifier`, `a_commitment_degrades_to_layer_a_where_no_mechanism_exists`
asserts it **degrades** (BR-2), and `layer_a_refuses_independently_of_any_attestation_mechanism`
asserts the ancestry gate still answers first — three ACs covered by one list edit.

**Degrade, don't refuse (BR-2)**: inherited verbatim from
`refuse_unattested_commitment`'s `Unavailable` arm. Shipped builds gain **zero**
new prompts — `teton provider add`, tier/category binding, and the in-session
config slash commands all keep working. Only a `--features presence` build
prompts. (Parity with the shipped-build honesty REQ-575 recorded: this control is
also inert on the release artifact until `presence` ships.)

**Integration-ordering risk to verify (TASK-140)**: `config/set` is exercised
over the real socket by `event_response_ordering.rs`, `multi_client.rs`, and
several `e2e/*` suites. Moving it onto the `blocks_on_a_human` spawn path must
preserve event→response ordering — the spawn path already applies the same
`fence.sync().await` before sending the response that `dispatch` does, so ordering
is expected to hold, but this must be confirmed green, not assumed.

### ADR-2: OQ-1/OQ-2 resolved — safe to gate; generic prompt for v1

**OQ-1 (resolved, safe)**: `apply_config_update` has exactly **one** production
caller — `handle_config_set` (server.rs ~2736). No first-run, migration, or
programmatic path touches it (first-run model selection uses the already-gated
`model/confirm`/`model/set`). Every CLI caller (`teton provider add` →
`RegisterProvider`; the privacy-boundary set → `SetPrivacyBoundary`; tier/category
binding; the in-session config slash command) is **user-initiated**, so a presence
prompt there is the deliberate machine-wide act BR-3 anticipates, never an
automatic flow a prompt would deadlock. Re-verify the single-caller fact at
implementation start (id-recheck discipline).

**OQ-2 (resolved)**: `SetPrivacyBoundary` uses the **same generic prompt** as
every other commitment for v1. `refuse_unattested_commitment` binds a synthetic
per-connection id and the mechanism layer (`LocalAuthentication`) is not currently
parameterized with a per-method reason string; adding per-variant reasons is a
mechanism-layer enhancement, out of scope here. Recorded as a future nicety, not a
v1 requirement.

### ADR-3: The consent-path `persist_web_tier` is accepted, not gated (BR-6)

**Decision**: `persist_web_tier` (reached via `permission/respond` +
`enable_permanent`) is **not** brought under BR-10(b). It is documented as an
accepted low-severity residual with a code comment stating why, and a test
asserts the ordinary `enable_permanent` answer still lands **without** a presence
prompt (no regression).

**Rationale**: it is **raise-only within an already-configured `[web]` table** and
cannot author an endpoint, a credential, or a new capability — the dangerous
powers unique to `config/set` and `web/setup_commit`. Its marginal capability to
the finding-7 attacker is raising the web tier within config the user already
set up, and only after the attacker drives its own session to raise a genuine
web-consent prompt. Gating it would instead put a Touch ID prompt on the ordinary
"enable permanently" answer a real user gives — a direct intrusion on the common
consent flow and a REQ-570 AC-8 regression. The security value is marginal; the
UX cost is not. It is also structurally a `permission/respond` answer, not a
daemon-wide commitment method — folding it into the commitment gate would be a
category error. AC-4 is therefore satisfied by the documented acceptance plus the
no-regression assertion, at the strength the decision was made.

### ADR-4: Test strategy — one list edit + a dedicated disk-inspection e2e + the reversal comment

- **Shared harness (AC-3 mutation, BR-2 degradation, layer-a)**: adding
  `config/set` to `commitments` makes `only_a_daemon_wide_commitment_demands_presence`
  the per-seam mutation test — deleting the `refuse_unattested_commitment` line
  from `handle_config_set` flips config/set from `ATTESTATION_FAILED` to a served
  outcome and turns that test red, independently of the other three seams
  (LESSON-502/508).
- **Dedicated e2e (AC-1 inspect-not-infer)**: reuse REQ-575's
  `TETON_PRESENCE_ACCEPT=fail` seam. A spawned daemon with a **real config file**
  is asked to `config/set` a `RegisterProvider` and a `SetPrivacyBoundary` from an
  ancestry-passing connection; the commit is refused with `ATTESTATION_FAILED`,
  and the test reads the config bytes (before == after) and the live config
  snapshot (`config/get`) back — not inferred from the error (LESSON-519).
- **Reader-loop liveness**: inherited — config/set uses the identical
  `blocks_on_a_human` machinery REQ-575's `a_parked_web_setup_commit_does_not_stall_the_connection`
  already pins on a multi-thread runtime (the `block_in_place` branch). A routing
  assertion that config/set left `dispatch` is the config/set-specific pin.
- **BR-7 comment**: rewrite the `handle_config_set` doc comment so it no longer
  claims layer (a) suffices; it records that a daemon-wide commitment now attests
  and the "can edit config.toml directly" mitigation is insufficient for a
  commitment (REQ-570/REQ-575 reasoning).
- **AC-6** is a recorded human pass on a `--features presence` build
  (`teton provider add` raises the prompt), written to `docs/manual-verification.md`
  at the strength verified.

## Data model changes

None. No new protocol types, config schema, or events. `config/set`'s
`ConfigSetParams`/`ConfigUpdate` shapes are unchanged.

## Interaction with in-flight work

- Builds directly on REQ-575 (merged `c81e156`): the `blocks_on_a_human` set, the
  `unreachable!()` else-arm hardening, `route_for_test`'s web/setup_commit branch,
  and the `TETON_PRESENCE_ACCEPT=fail` seam all exist. This REQ adds one member to
  each and one entry to the `commitments` list.
- After this REQ the BR-10(b) commitment set is **four** methods; REQ-575's BR-5
  standing-obligation doc (in `refuse_unattested_commitment`) is updated from three
  to four, and its "config/set is the known next candidate … tracked in REQ-576"
  paragraph is updated to "now gated (REQ-576)".
- No conflict with REQ-573 (daemon-owned web-setup catalog) — different surface.

## Spec-mapping table

| Spec item | As designed | Why the intent holds |
|---|---|---|
| BR-1 (gated via shared seam) | `refuse_unattested_commitment` in `handle_config_set`; async; moved to `blocks_on_a_human`; added to `commitments` | same function + same harness as the model methods |
| BR-2 (degrade) | inherited `Unavailable` arm; asserted by the shared degradation test | REQ-570 BR-8 asymmetry |
| BR-3 (provider-add regression) | OQ-1 analysis: all callers user-initiated; shipped builds degrade | no automatic caller; prompt only on presence builds |
| BR-4 (one line, per method, own test) | single call line; the `commitments` addition is its per-seam mutation test | LESSON-502/508 |
| BR-5 (four-method docs) | update `refuse_unattested_commitment` doc + REQ-575 references | additive |
| BR-6 (consent-path disposition) | ADR-3: accept, documented + no-regression test | raise-only, AC-8 UX cost > marginal security |
| BR-7 (BUG-162 comment reversal) | rewrite `handle_config_set` doc comment | honest reversal record |
