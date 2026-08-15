---
id: TASK-151
title: "Doctor advisory, e2e acceptance, changelog, gates"
status: complete
parent: REQ-578
created: 2026-08-15
updated: 2026-08-15
dependencies: ["TASK-149", "TASK-150"]
repo: teton-code
---

## Description

Close the REQ: the doctor advisory (ADR-4), the AC-1..AC-5 e2e suite through
a real daemon, the changelog entry, and the full workspace gates.

## Files to Create/Modify

- `crates/teton/src/main.rs` — doctor pass over remote providers applying
  the classifier's would-compose predicate; one `LineKind::Notice` per
  class-(b) endpoint naming the exact full form; unit test for the advisory
  rendering (flagged vs custom-path-silent).
- `crates/teton/tests/cli_e2e.rs` — new e2e tests per ADR-5: AC-1
  (base-URL composes through real config/set and persists the full URL),
  AC-2 (idempotence, no echo), AC-3 (Anthropic default + ordering), AC-4
  (custom path verbatim), AC-5 (doctor advisory, exit status unchanged —
  extend the existing doctor e2e).
- `CHANGELOG.md` — `[Unreleased]` entry: base URLs now compose at
  registration, Anthropic endpoint defaults, the echo line, the doctor
  advisory; note hand-edited configs are untouched.

## Acceptance Criteria

- [ ] All five e2e ACs green against a spawned daemon (TestDaemon fixture);
  AC-5's advisory does not change doctor's exit status.
- [ ] Full gates: `cargo build --workspace` then `cargo test --workspace
  --no-fail-fast` (counts reported honestly), `cargo clippy --workspace
  --all-targets` clean, `cargo fmt --all -- --check` clean,
  `tools/release/changelog-section.sh` exit 0, and the LESSON-515 gated
  sweep (`cargo check -p tetond -p teton-inference --features
  tetond/llama,teton-inference/llama --tests`).
- [ ] AC-6 audit repeated at REQ level: zero diff on the three protected
  files across the whole branch.

## Technical Notes

- The doctor e2e at cli_e2e.rs:393
  (`teton_doctor_and_cost_report_against_a_live_daemon`) is the fixture to
  extend — inject a bare-`/v1` provider via hand-written config (the
  hand-edit path is exactly what the advisory exists for).
- REQ-576 presence attestation degrades to allow on no-presence builds, so
  the e2e needs no seams (integration explorer confirmed).

## Implementation Note (2026-08-15) — the AC-1..AC-4 e2e stops before the keychain

ADR-5 put "AC-1 base-URL composition through real `config/set`" in
`cli_e2e.rs`. It cannot go there whole, and the reason is a rule this
repository already states twice: the shipped CLI writes credentials to the
**real OS keychain** (`keychain::default_keychain`) with no test seam in front
of it, so "no test may do that" (cli_e2e.rs's `/web setup` section header,
echoed in pty_e2e.rs). A completed remote `provider add` would create — and on
a rejected registration delete — an entry in whoever's login keychain ran the
suite, and would additionally fail outright on the Linux CI leg, where
`UnsupportedKeychain` refuses every store.

So the flow is proven in two halves that meet at one literal value:

1. **`crates/teton/tests/cli_e2e.rs` (real binary, real daemon, real argv)** —
   AC-1/AC-2/AC-3/AC-4 on what the registration flow *decides and says*: the
   composed URL echoed in full, silence for a verbatim store, the Anthropic
   default, the custom path untouched, and the BR-5 ordering (the echo's offset
   in stdout precedes the credential prompt's). Each run then ends at the
   credential step on a closed stdin — which is why no keychain is reached, and
   is itself evidence for BR-5.
2. **`crates/tetond/tests/composed_endpoint_registration.rs` (new)** — the
   composed URL and `ANTHROPIC_DEFAULT_ENDPOINT` driven through the real
   `config/set` over the socket, asserted on the persisted document *and* the
   live `config/get` snapshot. No credential anywhere: the payload's `auth_ref`
   is an `env:` reference to a variable that is never set. This is BR-8's
   "at least one end-to-end registration that executes the composed result
   through the real `config/set` validation path".

AC-5 is unaffected and lands whole in `cli_e2e.rs` — a hand-written config, a
real `teton doctor`, the advisory's full form asserted per line, the custom-path
provider asserted *not* flagged, and the exit status asserted unchanged.
