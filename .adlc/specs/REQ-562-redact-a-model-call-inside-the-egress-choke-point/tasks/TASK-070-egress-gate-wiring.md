---
id: TASK-070
title: "Wire the RedactionGate into Egress::send and runtime; flip the call-site marker"
status: complete
parent: REQ-562
created: 2026-08-07
updated: 2026-08-07
dependencies: ["TASK-067", "TASK-068", "TASK-069"]
repo: teton-code
---

## Description

The integration task: `RedactionGate` trait + `Egress::with_redaction_gate`
builder; the gate hook in `Egress::send()` after provenance inspection and
before `inner.execute()` (ADR-1); `redact_route()` in runtime.rs naming
`Category::Redact` literally (ADR-3 — deliberately NO taint arm, with the ADR
cited at the resolver); gate construction iff `config.privacy.redact` (ADR-2);
block emission with the TASK-068 causes; `call_sites.rs` flipped to
`Redact => true` with the unreached list now empty.

## Files to Create/Modify

- `crates/tetond/src/egress/mod.rs` — `with_redaction_gate`, the hook in `send()`, `EgressError` variant(s) for redaction blocks
- `crates/tetond/src/egress/redact.rs` — `RedactionGate` trait definition (async scan entry point)
- `crates/tetond/src/runtime.rs` — `redact_route()` beside the five resolvers; `RedactionGateImpl` resolving the route per scan and driving TASK-069's `scan()`; gate installation at every `Egress::new` site behind the config switch
- `crates/tetond/src/call_sites.rs` — `Category::Redact => true`; unreached-list assertion updated
- `crates/tetond/src/harness/duty.rs` — mutation-check table gains the redact rows (documentation of AC-8's mutations and their catching tests)

## Acceptance Criteria

- [x] Ordering (AC-11): a payload blocked by provenance produces zero scanner calls — asserted by call count on a counting mock gate.
- [x] Off means off (AC-13): with no `[privacy]` table, a remote turn produces zero scanner calls and no event or report claims a scan ran; enabling the switch and repeating the same turn produces exactly one scan. Both legs by call count.
- [x] Fail closed (AC-3 path at this layer): gate returning `Unavailable` blocks the send — captured transport records zero requests — and emits `privacy_block` with `ScanUnavailable`.
- [x] Block on High (AC-1 path at this layer): a High-finding verdict blocks with `Redaction { kind, span }` cause and a non-secret locus `path`; Low-only and Clean verdicts forward, and the forwarded bytes are byte-identical to the input request (AC-9, asserted by capture).
- [x] `call_sites.rs` passes: the scanner finds `Category::Redact` at `redact_route()`, the marker says reached, the unreached list is empty.
- [x] Every remote path crosses the gate: a `RemoteDuty` send with the gate installed is scanned too (one test proves a duty-egress payload is subject to the gate).
- [x] `cargo build --workspace && cargo test --workspace` green (workspace build first — LESSON-489's sibling trap).

## Technical Notes

- The gate hook must not reorder cost metering: metering wraps the response of
  allowed forwards only; blocked sends bill nothing (they never execute), which
  is today's behaviour for boundary blocks — keep it.
- `EgressError`: extend `PrivacyBlocked` or add a sibling variant so the turn
  loop's existing failure sentence path renders the cause; the sentence for
  ScanUnavailable must say "could not run", not "found something" (BR-3).
- The gate's scan call is a local engine call under the duty seam — it must not
  hold locks across `.await` in `send()` beyond what the seam already does, and
  it rides the blocking pool via the seam (ADR-006's E-3 rule comes free if the
  scan goes through `DutyRoute::perform`).
- Session taint: a redaction block flows through the same `PrivacyEventSink`,
  so `TaintingPrivacySink` taints the session — subsequent turns pin local
  (BR-8 stays intact; the sink is cause-agnostic per TASK-068).
- ADR-3's asymmetry (no taint arm in `redact_route`) gets a comment citing the
  ADR so a uniformity-minded reviewer doesn't "fix" it (LESSON-484 corollary).

## Completion Notes

### The shape that shipped

```rust
// crates/tetond/src/egress/redact.rs
#[async_trait]
pub trait RedactionGate: Send + Sync {
    async fn scan(&self, payload: &str) -> RedactionVerdict;   // total, no Result
}

// crates/tetond/src/egress/mod.rs
pub fn with_redaction_gate(self, gate: Arc<dyn RedactionGate>) -> Self;   // mirrors with_cost_meter
```

`Egress::send` now runs **two** inspections in this order: provenance (cause
`Boundary`), then — if a gate is installed — the scan over
`String::from_utf8_lossy(&request.body)`, with `redact::decide` mapping the
verdict to forward/block. A blocking verdict emits `privacy_block` and returns
`EgressError::PrivacyBlocked`; a forwarding one hands `inner.execute` the
*same* `TransportRequest`, untouched.

`EgressError::PrivacyBlocked` gained a `cause: BlockCause` field and three
Display sentences (`privacy_blocked_sentence`). The `Boundary` sentence is
byte-identical to the pre-REQ-562 one on purpose — it is what is in every
existing log, and it is what a `cause`-less frame reads as.

### The locus string — a deviation from the task brief, with the reason

The brief suggested a **self-describing** `path`, e.g.
`"outbound payload (redaction: credential at bytes 1400-1436)"`. What shipped is
the plain noun phrase **`"the outbound payload"`** for both redaction causes.

Why: TASK-068's CLI renderer interpolates `path` *inside* the cause-aware
sentence — `"…detected a credential at bytes 1400–1436 of {path}, bound for
{provider}"` — so a self-describing path prints the kind and span twice in the
one sentence this REQ actually promises. TASK-068's own CLI fixture
(`session_ui::tests::the_three_block_causes_render_as_three_distinguishable_lines`)
already uses `"the outbound payload"`, so the shipped renderer was written
against this string.

The skew reading is still correct: a client predating `cause` defaults to
`Boundary` and prints *"the outbound payload would have reached anthropic — call
re-routed to the local tier"* — true, non-misleading, and content-free. What it
does not do is name redaction as the reason; that is the cost, and it is bounded
by the fact that the v2 CLI is the surface REQ-562 promises. `redaction_locus()`
takes the cause as an argument so a future locus that *does* vary by cause has
one place to vary in.

### Recursion: checked, and impossible by construction (no contradiction found)

The brief asked for any path where the scan could re-enter `Egress::send` to be
reported. There is none, and the reason is structural rather than guarded:

1. `Category::Redact` has no `ConfigurableCategory` counterpart (REQ-558 ADR-B),
   so `teton_core::category::resolve` reaches it through `resolve_pinned_local`,
   which consults **no binding**.
2. `resolve_pinned_local` can only name `local_tier_id(config)`, which yields
   `None` when a non-local provider has taken the canonical id
   (BUG-156/TASK-057) — so the pin can never name a remote provider.
3. `RedactionGateImpl::redact_route` constructs **only** `DutyRoute::local`.
   It holds no transport, no provider, no `SecretResolver` and no `CostLedger`;
   there is no field in it a network call could be made through.

Point 3 is the load-bearing one and it is why the gate does **not** go through
`DaemonRuntime::build_duty_route` like the five siblings: that function *can*
build a `RemoteDuty`, and since TASK-070 installs the gate on the duty path's
own `Egress`, routing `redact` through it would put the recursive shape in the
code even though nothing could reach it. `a_squatted_local_tier_id_leaves_the_scan_unavailable_never_remote`
pins point 2 behaviourally.

Consequence for reviewers: `redact_route` lives on the gate, not on
`DaemonRuntime`, so it is *below* the five siblings in `runtime.rs` rather than
inline with them. It still names `Category::Redact` literally at a
`router.resolve` call, which is what `call_sites.rs`'s scanner reads.

### The turn-failure sentence names its cause (BR-3 on the primary surface)

This shipped as a **follow-up commit** to TASK-070; the first pass reported it
as a gap, which was the wrong call — BR-3's promise ("the user is told the scan
could not run, not that it found something") is made on the surface a person
actually reads, so a cause-blind sentence there is a violation, not a debt.

**The problem.** `run_prompt_turn`'s privacy arm said *"this turn's content is
under a local-only privacy boundary…"* for every cause, because the cause was
erased at `EgressError::into_transport_error()` → `TransportError::PrivacyBlocked`,
a unit variant in `teton-providers` — a crate that deliberately declares no
`teton-protocol` dependency, which is what keeps the single choke point
enforceable. The configuration where this bit is not exotic: `[privacy] redact =
true` with **no local tier** is remote-only operation with the switch on, and in
it every remote turn fails closed with `ScanUnavailable` and was told to go look
for a `local-only` glob that does not exist.

**What shipped.** A providers-local `BlockDetail { Boundary, Redaction,
ScanUnavailable }` in `teton_providers::transport`, carried by
`TransportError::PrivacyBlocked(BlockDetail)` and
`ProviderError::PrivacyBlocked(BlockDetail)`. The crate boundary holds: no
protocol type crosses it, and the projection is deliberately **lossy** — the
finding's kind and byte span stay behind at the `privacy_block` event, which is
where a locatable finding belongs. `BlockDetail` derives no `Default`, so the
one construction site (`into_transport_error`) must name a real cause.

The chain is: `BlockCause` → `BlockDetail` → `ProviderError::privacy_block_detail()`
→ `HarnessError::privacy_block_detail()` → the daemon's sentence.
`is_privacy_blocked()` is now *defined in terms of* the detail accessor on both
error types, so "is it a block" and "which block is it" cannot come to disagree.

**Sentence ownership survived.** The value travels; the wording does not. Three
surfaces word it independently, as they already did: `BlockDetail`'s `Display`
is the transport seam's log clause, `egress::privacy_blocked_sentence` is
`EgressError`'s, `session_ui::format_privacy` is the CLI's, and
`runtime::refusal_clause` is the daemon's. The daemon composes one clause into
three situations — no local tier, reroute also failed, and the `route_decided`
reason on a successful reroute (that third one was cause-blind too, and is the
only account a user gets on a turn that recovers).

**Tests, each with its mutation applied and observed (LESSON-441):**

| Mutation | Turns red |
|---|---|
| the pre-fix hardcoded boundary sentence is restored at the no-local-tier arm | `runtime::tests::dispatch::redact::a_scan_that_could_not_run_fails_the_turn_saying_so_not_blaming_a_boundary` |
| `into_transport_error` collapses `ScanUnavailable` onto `BlockDetail::Boundary` | `egress::tests::every_block_cause_projects_onto_its_own_transport_detail`, and the turn test above |
| the transport→provider hop drops the detail and substitutes `Boundary` | `teton_providers::tests::every_block_detail_survives_the_hop_and_stays_non_retryable` |
| the scan-unavailable clause starts claiming a finding | `runtime::tests::dispatch::redact::the_three_block_causes_produce_three_distinct_turn_failure_sentences`, and the turn test above |

The turn test drives **`run_prompt_turn` itself**, not the sentence helper, so
what it pins is that the cause survives the whole journey. Its discriminating
assertion is the exact one the AC asks for: the message must contain "the
redaction scan could not run" and must **not** contain "local-only privacy
boundary" — the sentence the old code emitted there. It also asserts no payload
content reaches the message (a planted `sk-…` sentinel is absent). Its
non-vacuity twin, `the_same_turn_with_the_switch_off_is_not_blocked_at_all`,
runs the identical turn with the switch off and confirms it is not privacy
blocked at all — so "blocked with this sentence" is a fact about the gate rather
than about a runtime that refuses everything.

Both turn tests point their fixture endpoints at a closed loopback port: the
first must reach no network *at all* (the gate refuses before `inner.execute`),
and a fixture reaching the public internet would put a DNS lookup in a unit test.

**Not reachable end to end, and why that is fine.** Two of the nine
(cause × situation) rows cannot be produced through `run_prompt_turn`: a
`Redaction` block needs a local engine for the scan to run, and the no-local-tier
arm needs the opposite; and the reroute-also-failed arm is a belt rather than a
path (the local tier has no egress, so it cannot privacy-block twice). Those rows
are covered exhaustively at the composition layer, which is where they exist.

### Two census tests were updated (not weakened)

Both are the "the current unreached set is X" shape that TASK-069's notes
describe as *intended* to go red when a call site is wired, and both had
comments saying so:

- `runtime::tests::the_snapshot_marks_the_unreached_categories_and_the_judgment_default`
  — `vec!["redact"]` → empty. The per-row `has_call_site` agreement loop beside
  it (the actual invariant) is untouched.
- `teton::cli_e2e::policy_show_renders_the_daemons_resolved_table` — the marked
  row is gone, so the assertion became "**no** row carries the marker", plus
  `redact`'s row now reading `— sends outbound payloads` in the present tense.

That e2e half deliberately carried a second purpose: proving the renderer had
not forgotten *how* to print the marker. With the derived set empty it can no
longer serve it, so the purpose is **re-homed, not dropped** —
`teton::main::tests::policy_show_marks_the_unreached_categories_and_the_judgment_default`
and `…renders_the_content_class_beside_the_call_site_marker` render a synthetic
snapshot with `reached: false` and assert the marker. Their doc comments were
rewritten to say the row is now a fixture property rather than a daemon fact
(LESSON-486 tense honesty).

`tests/egress_capture.rs`'s exhaustive `PrivacyBlocked { .. }` pattern needed the
new field; it now asserts `cause == BlockCause::Boundary`, which strengthens it.

### Signature changes rippled

`router: &Router` is threaded into `resolve_duty`, `build_duty_route` and
`build_tools` so the gate has exactly **one** construction site per `Egress`
(`DaemonRuntime::redaction_gate`) rather than one per resolver. Both duty
functions gained `#[allow(clippy::too_many_arguments)]`. `build_tools` now
clones the whole `Config` instead of just `boundaries` (it needs
`config.privacy`).

The gate is built **only** on the remote branches, so a local turn, a local
duty, or a machine with the switch off allocates nothing.

### For TASK-071

- **Fixture reality check.** `SCRIPTED_REDACTION` is `"NONE"`, so a scripted
  engine can only ever produce a *clean* model pass. Every block a scripted e2e
  observes comes from the **pattern** pass. A `Low`-only verdict — the AC-9
  forward-with-findings row — is not reachable through `ScriptedFileEngine` and
  needs a `MockEngine` with a contract-shaped reply, or the unit-level
  `CountingGate` in `egress::tests`.
- **The gate is on three choke points**, not one: the turn path
  (`run_one_attempt`), the duty path (`build_duty_route`, remote branch only)
  and the MCP path (`build_tools`). An e2e that only exercises the turn path
  leaves two thirds of ADR-1's "every remote path" unasserted.
- **Sentinel sweep (AC-6)** has three new surfaces to grep: `PrivacyBlock.path`,
  `EgressError::PrivacyBlocked`'s Display, and the CLI's rendered line. All
  three are content-free by construction today (`Finding` has no text field), so
  the sweep should come back clean — a hit means someone added a field.
- **Latency (ADR-8/BR-9)**: the scan is on the *synchronous* send path, so every
  outbound call now waits for a local inference when the switch is on. The
  `docs/manual-verification.md` procedure is TASK-071's and is the only place
  this gets measured.
- The mutation-check table in `harness/duty.rs` gained a REQ-562 section naming
  ten mutations and the test each turns red. Those rows are **documentation
  written from the wiring**, not observed runs — applying them is TASK-071's
  AC-8 work, and that table is where the observations should be recorded.
