---
id: TASK-254
title: "Verify the remedy write on disk, and the attestation posture"
status: complete
parent: REQ-589
created: 2026-08-24
updated: 2026-08-24
dependencies: [TASK-250]
---

## Description

AC-13 + AC-20 / LESSON-519 + LESSON-520. Inspect the artifact, do not infer from a return code — and pair every refusal with an accepted counterpart on the same fixture or the refusal test is vacuous.

## Files to Create/Modify

- `crates/tetond/tests/config_set_attestation.rs` — AC-20 on both presence configurations
- `crates/tetond/tests/config_preservation.rs` — the on-disk double check

## Acceptance Criteria

- [x] The applied remedy is verified by reading the config FILE and re-parsing it — both, per `a_field_less_registration_preserves_the_stored_window_and_a_declared_one_writes_it` (config_preservation.rs:885)
- [x] The refusal leg asserts the config is byte-identical before and after, on the same fixture (LESSON-520)
- [x] AC-20 runs on a build with and without `presence`, using `TETON_PRESENCE_ACCEPT=1` and `=fail`
- [x] The ordering invariant is tested by failing the SECOND write and asserting the config never reaches the forbidden state (ADR-5)
- [x] `verified_on` is recorded alongside the written window (ADR-7)

## Technical Notes

`config_set_attestation.rs:37` is the narrowest existing fixture for the presence seam and the most directly reusable.

## What shipped, and two things that had to change

**`config_preservation.rs` — four tests, one fixture set.**

* `the_ordered_rebind_declares_the_window_then_binds_the_tier_and_both_reach_disk`
  — BR-9's pair applied in ADR-5's order, then checked **twice**: the file's own
  bytes through `assert_only_these_lines_changed` (removals `[]`, so nothing was
  rewritten on the way) and the re-parse through the production loader. This is
  also the accepted counterpart that makes both refusal legs non-vacuous.
* `a_refused_second_write_leaves_a_declared_window_on_an_unbound_tier_never_the_circle`
  — the ordering invariant's on-disk half, plus the block that makes it
  discriminating: on the same fixture, the **reverse** order's first write
  applied alone really does leave `build` bound to a provider with
  `max_context = 0`, and `derive` over that state is `DefaultUnknown` with a pair
  the same measurement overflows again. The circle, on disk.
* `a_refused_remedy_write_leaves_the_document_byte_identical` — LESSON-520's leg.
* `the_window_written_to_disk_is_the_one_the_offer_named_with_its_date` — ADR-7.

**`config_set_attestation.rs` — four tests.** A refuse/accept pair for the
remedy's own two payload shapes (`SetTierBinding` had no pair in this file at
all), the AC-20 wording guard run under both `TETON_PRESENCE_ACCEPT` postures,
and the ADR-18 item 3 posture test.

### Deviation 1 — ADR-18 item 3, tested as it is (not as ADR-4 says)

`the_remedys_durable_write_does_not_pass_the_daemon_wide_commitment_gates` pins
the gap rather than asserting the gates run, because they do not:
`RemedyWrites::apply` calls `DaemonRuntime::apply_config_update`, while
`refuse_daemon_wide` and `refuse_unattested_commitment` wrap that body in
`server.rs::handle_config_set`. The test shows the contrast directly — the same
payload the wire refuses under `=fail` is applied by the seam the remedy uses.
AC-20 is satisfied by construction (nothing in the offer's wording claims an
attestation), and the test goes red the day the gates move, which is what stops
the deviation being silent.

### Deviation 2 — `verified_on` could not be tested on the rebind arm

The AC's `verified_on` bullet was written expecting the rebind fixture to carry
it. It cannot: `Remedy::BindTierRemote`'s clause says "declare that provider's
`capabilities.max_context`" and names neither the figure nor its date — the same
gap ADR-18 item 2 records for the provider's name, and TASK-260's scope. The tie
is therefore pinned on `DefaultUnknown`/`DeclareWindow`, whose label does carry
both, asserting that the figure in the label and the figure the loader reads back
off disk are the same `proposed_window` value and that its `verified_on` and
vendor ride beside it. No literal window or date is pinned in either test — a
second copy of the figure would be the drift LESSON-546's one-home rule exists to
prevent.

### Mutation checks run (LESSON-520)

* refusal leg under an accepting verifier (`=fail` → `=1`) → red, at the
  byte-identical assertion.
* refusal legs with the induced refusal removed → red, and the failure output
  shows the document genuinely gaining `[[tiers]]`, so the inspection — not the
  return code — is what fires.
* an offer word added to the AC-20 forbidden list → red, proving the substring
  scan reaches real composed text rather than empty strings.
