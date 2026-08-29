---
id: TASK-287
title: "Route every daemon boundary reader through the composer"
status: pending
parent: REQ-597
repo: teton-code
created: 2026-08-29
updated: 2026-08-29
dependencies: [TASK-285, TASK-286]
---

## Description

The behaviour change. Seven production sites in `runtime.rs` read `config.boundaries` for
enforcement or reporting; each becomes `config.effective_boundaries()`. The one write site
stays as it is. Covers BR-2, BR-4, BR-6 (daemon half), BR-8.

## Files to Create/Modify

- `crates/tetond/src/runtime.rs` — convert the seven read sites:
  - `5467` — `CarriedTurn::begin(...)`, the session-taint list (`carry.rs::context_is_sensitive`)
  - `6728` — MCP egress
  - `6868` — remote provider egress
  - `7342` — `Egress::new`
  - `9067` — provider connection test
  - `9599` — `Egress::new`
  - `13772` — the `config/get` snapshot's `privacy` field, which additionally maps `origin`
    through the wire enum

## Acceptance Criteria

- [ ] BR-4: builtin and user rows are indistinguishable at enforcement — the seven sites pass
      one composed list to the same `BoundaryMatcher`, the same `privacy_block`, the same
      taint. No site branches on `origin`.
- [ ] BR-6 (daemon half): `config/get`'s `privacy` carries the **effective** set with each
      row's origin, in composed order.
- [ ] `ConfigUpdate::SetPrivacyBoundary` (`14005`/`14011`) still reads and writes
      `config.boundaries` — the user's table alone. A test asserts that adding a boundary
      through `config/set` leaves the persisted table free of builtin rows.
- [ ] BR-8: no site introduces a new path spelling. The composed globs are matched against the
      same canonical provenance form the user's rows already were — verified by the fact that
      no call site changes what it passes to `match_path`, only which list it matched against.
- [ ] `cargo test -p tetond --no-fail-fast` is run and its failures triaged (see notes).

## Technical Notes

Expect existing tests to move — that is the signal, not the noise. This changes a default, and
the suite was written against the old one. Two shapes are expected:

1. Tests asserting an empty effective boundary set. `runtime.rs:21350` is the named example:
   its premise is *"no boundaries, so `context_is_sensitive` cannot be what pins"*. That premise
   is now false by default. Re-establish it by setting `disable_default_boundaries` on the
   fixture config — **never** by weakening the assertion the test exists to make.
2. Tests whose fixture paths happen to match a builtin glob (`*.key`, `*.pem`, `.env`).
   Rename the fixture path; do not narrow the builtin list to accommodate a fixture.

Record every test you move and why, in the commit body. TASK-292 reconciles that list against
the final suite.

Leave `config.rs:1701` (`validate()`'s glob compile) reading `self.boundaries`: it validates
what the *user wrote*, and a builtin glob failing to compile is this REQ's bug, not the user's
config error. TASK-285's unit tests are where builtin globs are proven to compile.
