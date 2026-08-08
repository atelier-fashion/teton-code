---
id: TASK-071
title: "E2E acceptance matrix, sentinel grep, mutation checks, and the latency procedure"
status: complete
parent: REQ-562
created: 2026-08-07
updated: 2026-08-08
dependencies: ["TASK-070"]
repo: teton-code
---

## Description

The verification task closing every AC end-to-end through the real daemon
surfaces: the egress-capture acceptance matrix, the AC-6 sentinel grep across
all emitted surfaces, the AC-12 taint short-circuit, the AC-5 engine-backed
capture test, the AC-8 mutation checks with their catching tests, and the AC-7
latency procedure in docs/manual-verification.md.

## Files to Create/Modify

- `crates/tetond/tests/redact_egress.rs` — new integration suite (CaptureTransport + CapturingSink + scripted engine): the acceptance matrix
- `crates/tetond/tests/egress_capture.rs` — extend only if shared fixtures live here; prefer the new file
- `docs/manual-verification.md` — AC-7 latency procedure (budget from ADR-8), recorded NOT RUN
- `.adlc/specs/REQ-562-redact-a-model-call-inside-the-egress-choke-point/requirement.md` — tick ACs as they are proven

## Acceptance Criteria

- [x] AC-1: planted paraphrased credential, clean provenance, no matching boundary — blocked; `privacy_block` names the redaction cause; captured transport shows zero outbound requests. (Scripted model verdict provides the Low/High hit; use a pattern-shaped sentinel for the High leg and a scripted model catch for the paraphrase leg.)
- [x] AC-2: clean payload forwards AND the test proves the scan ran (scanner call count == 1), not skipped (non-vacuity — LESSON-485).
- [x] AC-3: `[privacy] redact = true` with no local tier (llama feature absent / engine unavailable): blocked, captured bytes show nothing sent, cause is ScanUnavailable, and no surface claims `scanned: true`.
- [x] AC-5: a remote-kind provider registered under the id `local` with redact enabled — the scan never dispatches over HTTP: captured transport records zero scan-originated requests (asserted by capture, not id comparison).
- [x] AC-12: a tainted session's turn produces zero scanner calls (call count), because the turn never reaches remote egress.
- [x] AC-4's "no locality guard was added" leg, asserted behaviorally (NOT by a src-text grep — LESSON-489): with a genuinely engine-backed local provider registered under a NON-`local` id, the redact scan still runs and serves. An id-comparison guard would wrongly refuse this fixture; its success is the discriminating evidence the guard does not exist (LESSON-485).
- [x] AC-6: a distinctive sentinel (e.g. `AKIA_SENTINEL_562_…` shaped to trip the pattern pass) planted; serialize every captured event, daemon log output, and returned error from the run; grep finds zero occurrences of the sentinel outside the payload itself.
- [x] AC-9: for both Clean-forward and Low-only-forward, captured outbound bytes are byte-for-byte identical to the assembled request.
- [x] AC-8 mutations, each run with the workspace freshly built (LESSON-489): (a) `decide`'s Unavailable arm → Forward; (b) input cap removed; (c) a text field added to `Finding` and threaded to the event; (d) an id-based locality assertion replacing the capture assertion in AC-5's test. Each names the test that goes red in the duty.rs mutation table; a green mutation is REPORTED in the task completion note, not quietly fixed.
- [x] AC-7: manual-verification.md gains the latency procedure (real weights, payload at cap, p50/p95 against ADR-8's 2s/5s budget), marked NOT RUN, following the REQ-557/558 entry format.
- [x] AC-13/AC-14/AC-10/AC-11 already pinned at their layers (TASK-066/067/070) — this task re-runs the full workspace suite and confirms no gap in the AC checklist, ticking requirement.md.
- [x] `cargo build --workspace && cargo test --workspace` green.

## Technical Notes

- Script format: duty answers are consumed off-script via contract recognition;
  keep the redaction scripted verdicts unambiguous against grep-content
  collisions (runtime.rs DUTY_CONTRACT_PREFIX_BYTES trap).
- The AC-5 fixture is the BUG-156 shape: register a remote provider under the id
  `local`; the discriminating state is that an id-comparison passes while the
  capture assertion fails if the pin ever dispatches remotely (LESSON-485 #5).
- Do not let any test read `src/` as text unless it tolerates concurrent
  modification (LESSON-489 / BUG-159); the call_sites scanner already exists —
  do not add another.
- Mutation (c) intentionally violates the type-level no-text property; it is a
  compile-plus-thread mutation — record the exact diff applied in the mutation
  table so it is reproducible.

## Completion Notes

### What shipped

`crates/tetond/tests/redact_egress.rs` — nine tests, all green, driving the
**real** choke point (`Egress::send`), the **real** scan (`harness::redact::scan`
— pattern pass *and* model pass), the **real** router (`Router::resolve` through
the pinned-local branch), and a **real** `Engine`, over a `CaptureTransport` and
a `CapturingSink`. Plus the mutation observations in `harness/duty.rs`, the AC-7
latency procedure in `docs/manual-verification.md` (NOT RUN), and the
requirement.md ticks.

### AC → test

| AC | Test |
|---|---|
| AC-1 (blocking leg) + AC-11 | `a_planted_credential_with_clean_provenance_is_blocked_and_the_event_names_redaction` |
| AC-1 (model-catch leg) + AC-9 (Low-only forward) | `a_credential_the_model_paraphrased_is_located_and_reported_without_blocking` |
| AC-2 + AC-9 (clean forward) | `a_clean_payload_forwards_and_the_scan_provably_ran` |
| AC-3 (no local tier) | `with_no_local_tier_the_payload_is_blocked_unscanned_and_nothing_claims_otherwise` |
| AC-3 (BR-7's cap) | `a_payload_past_the_input_cap_blocks_unscanned_and_costs_no_model_call` |
| AC-5 | `a_remote_provider_squatting_the_local_id_never_receives_the_scan` |
| AC-4 (no-guard leg) | `an_engine_backed_local_tier_under_another_id_still_serves_the_scan`, paired with the same-named fixture in `runtime::tests::dispatch::redact` for the daemon's own resolver |
| AC-12 | `a_turn_pinned_to_the_local_tier_costs_zero_scanner_calls` |
| AC-6 | `no_emitted_surface_carries_the_sentinel` |

### The one copy, and what it leaves outside

`RedactionGateImpl` is private to `tetond`, so the file's `TestGate` restates its
two-line body (resolve `Category::Redact`; engine slot → `DutyRoute::local`, else
unresolved). Everything downstream of `local_provider_id` is real. What sits
*outside* the file is one step — `runtime::local_tier_id`'s derivation of that
value from a config — which is pinned by
`runtime::tests::the_local_tier_id_is_never_a_registered_remote_providers_id` and
`…dispatch::redact::a_squatted_local_tier_id_leaves_the_scan_unavailable_never_remote`.
That boundary is what made the green mutation below possible, and it is stated in
the file's module docs rather than left for a reader to discover. The daemon's
own resolver now has its own fixture in `runtime::tests::dispatch::redact`, so
the two layers are covered by two tests rather than by one test and a copy.

### AC-8 — the four mutations, applied and observed

Recorded in full, with the exact diffs, in `harness/duty.rs`'s REQ-562 section
("REQ-562 AC-8 — the four mutations, applied and observed"). Each ran against a
freshly built workspace (LESSON-489).

| # | Mutation | Result |
|---|---|---|
| (a) | `decide`: `Unavailable => Block` → `=> Forward` | **RED** — 12 lib tests + 4 in `redact_egress.rs` |
| (b) | `pattern_verdict`: the `REDACT_INPUT_MAX_BYTES` guard deleted | **RED** — 4 lib tests + 1 in `redact_egress.rs` |
| (c) | `Finding` gains `text: String`, populated by both passes and threaded onto the emitted `PrivacyBlock.path` | **RED** — `egress::redact::tests::a_finding_never_carries_the_matched_text`, `egress::tests::a_high_confidence_finding_blocks_with_its_kind_span_and_locus`, and 2 in `redact_egress.rs` including the sentinel sweep |
| (d) | an id-based locality assertion, restored | **RED at both layers** — green at the runtime layer on the first run, then closed by a fixture; see below |

**Mutation (c)'s coverage boundary, stated because it matters.** The TASK-068
wire-key-set test (`teton_protocol::events::tests::a_redaction_cause_carries_only_a_kind_and_a_span`)
stayed **green**, correctly: it guards `BlockCause::Redaction`'s key set, and
this variant of the mutation rides the matched text on `PrivacyBlock.path`, which
is a `String` either way. The wire-key-set test catches the *other* spelling of
(c) (a new field on the cause); the AC-6 sentinel sweep catches this one. Neither
subsumes the other.

### A GREEN MUTATION, reported first and then closed by a fixture

**Status: closed.** Reported at the end of the first pass, then closed in a
follow-up commit by the fixture it identified — in that order, and the green
observation is kept here and in `harness/duty.rs` rather than deleted, because a
mutation table that only records reds cannot show which of its rows were ever
load-bearing.

**What was observed (first pass).** (d), placed in
`runtime::RedactionGateImpl::redact_route`, turned **nothing** red.

```rust
// between the resolve and the engine-slot read:
if provider_id != LOCAL_PROVIDER_ID {
    return DutyRoute::unresolved(format!(
        "The 'redact' category resolved to '{provider_id}', which is not the local tier."
    ));
}
```

`cargo test -p tetond --lib` → 684 passed, 0 failed. `--test redact_egress` → 9
passed, 0 failed.

**Why it survives.** `local_tier_id` returns the id of any provider declaring
`kind = "local"`, which is very often *not* the string `local` — a user who
writes `id = "on-device"` has a genuinely engine-backed tier under another name.
The guard above would fail that machine's scan closed and, since the gate is on
the synchronous send path, every one of its remote turns with it. Every
`runtime::tests::dispatch::redact` fixture builds its router from a config whose
local tier *is* the canonical id, so the guard can never fire in a test.

**What does cover the same property, one layer down.** The same mutation placed
in `harness::redact::scan` (`if route.provider() != Some("local") { … }`) is
**RED**: `redact_egress.rs::an_engine_backed_local_tier_under_another_id_still_serves_the_scan`
plus `harness::redact::tests::a_scan_that_overruns_the_deadline_is_unavailable`.
So AC-4's no-guard property *is* asserted behaviourally — through a real `Router`
whose `local_provider_id` is `on-device` and the real `scan` — but not against
the daemon's own private copy of the resolver, which an integration test cannot
reach.

**How it was closed.**
`runtime::tests::dispatch::redact::an_engine_backed_local_tier_under_another_id_still_serves_the_scan`
— one config (`[[providers]] id = "on-device", kind = "local"`, pushed onto the
opted-in fixture config) and the assertions that the pin resolves to that id,
that the scan **serves** (`Outcome::Clean`, `scanned: true`, `decide → Forward`),
that it ran on this machine's own engine exactly once, and that its
`route_decided` names `on-device`. It sits beside
`a_squatted_local_tier_id_leaves_the_scan_unavailable_never_remote`, which is the
same coin's other face: a remote provider under the canonical id resolves to
nothing; a genuine local tier under any id resolves and serves.

**Re-run after the fixture landed** (freshly built workspace, identical diff):
**RED** — `left: Unavailable, right: Clean` at the outcome assertion,
`crates/tetond/src/runtime.rs:10292`. 684 passed / 1 failed, and that one is the
new fixture. Reverted; the suite is green again.

**AC-4 is therefore ticked.** Both placements of (d) are now covered: the
integration suite for the `harness::redact::scan` layer, this fixture for the
daemon's own resolver.

### AC-1's prose contradicts AC-10, and the architecture already adjudicated it

Requirement AC-1 says a credential *"the model paraphrased into prose"* is
**blocked**. For that exact input, BR-4 + ADR-4 + AC-10 say the opposite:
confidence is derived — a pattern hit is `High` and blocks, a model-only hit is
`Low` — and AC-10 requires in as many words that "a low-confidence-only payload
is not blocked". This task's own AC-9 asks for byte-identity on a
**Low-only-forward** leg, which exists only if a low-only verdict forwards.

The contradiction is between two paragraphs of the *requirement*, not between the
requirement and the code: AC-1 predates OQ-2's resolution to "both passes" and
BR-4's derived-confidence rule. No existing test was edited to fit, and no code
was changed (LESSON-487). AC-1 is asserted as the two legs the shipped design
has — the pattern-shaped sentinel blocks, the paraphrase is *located and
reported* — and the divergence is written into the test file's module docs so the
next reader meets it where the assertion is.

### What this file deliberately does not re-assert

- The unit-level decision arms and the `CountingGate` call counts (TASK-070's
  `egress::tests`), and the resolver-level pin (TASK-070's
  `runtime::tests::dispatch::redact`). AC-11 and AC-12 appear here only in the
  end-to-end shape those tests cannot reach.
- AC-13/AC-14/AC-10 are untouched: they are pinned at their own layers
  (TASK-066/067/070) and were re-run green as part of the workspace suite.

### Verification

`cargo build --workspace && cargo test --workspace --no-fail-fast` — green, no
failures across every target. `cargo fmt --all --check` clean;
`cargo clippy --workspace --all-targets` clean.

No test in this task reads `src/` as text (LESSON-489/BUG-159); the AC-4 leg is
behavioural, and the AC-6 sweep reads emitted values rather than source.
