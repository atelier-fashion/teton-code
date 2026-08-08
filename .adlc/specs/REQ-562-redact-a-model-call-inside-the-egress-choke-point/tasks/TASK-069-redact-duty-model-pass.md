---
id: TASK-069
title: "The redact duty: model pass, output contract, and quarantined-output parsing"
status: complete
parent: REQ-562
created: 2026-08-07
updated: 2026-08-07
dependencies: ["TASK-066"]
repo: teton-code
---

## Description

Create `crates/tetond/src/harness/redact.rs`: `REDACT_DUTY: DutyKind` for
`Category::Redact`, the `REDACTION_OUTPUT_CONTRACT` constant, the prompt
builder, and the parser that turns the model's reply into `Confidence::Low`
findings per ADR-5 (model quotes a suspicious substring → locate it in the
payload → keep the span, discard the text; unlocatable → drop). Combine the
TASK-066 pattern pass with the model pass into a single
`scan(text) -> RedactionVerdict` entry point the gate will call.

## Files to Create/Modify

- `crates/tetond/src/harness/redact.rs` — new module as described
- `crates/tetond/src/harness/mod.rs` — module declaration + `REDACT_DUTY` export (mirror the title/triage exports)
- `crates/tetond/src/runtime.rs` — ScriptedFileEngine: add the `instructs(prompt, REDACTION_OUTPUT_CONTRACT)` recognition arm returning a scripted redaction verdict (off-script, like the other duty arms)

## Acceptance Criteria

- [x] `REDACT_DUTY` follows the DutyKind pattern (category + ceiling); the prompt embeds `REDACTION_OUTPUT_CONTRACT` within `DUTY_CONTRACT_PREFIX_BYTES` so the scripted engine recognizes it, and the contract wording cannot be confused with grep-result content (see the REQ-561 disambiguation trap in runtime.rs).
- [x] Model-output parsing: a reply quoting a substring present in the payload yields a Low finding with the correct byte span and NO text field; a reply quoting a substring NOT in the payload yields no finding (hallucination-drop test); a malformed/empty reply yields `Unavailable`, never `Clean` (a parse failure is a scan that did not run — BR-3, LESSON-447).
- [x] The model's raw reply is never logged, never embedded in any error string, and never leaves the parse function's scope — asserted where practical (error paths return static/derived strings only).
- [x] `scan()` composes both passes: pattern hits High, model-only hits Low; a string found by both passes reports once at High (dedupe by overlapping span); over-cap input short-circuits to `Unavailable` before any model call (with the ADR-6 allowance: the pattern pass MAY run first, but the outcome for over-cap without a High hit is still Unavailable → Block, never Forward).
- [x] Duty-seam integration: performing through `DutyRoute` respects the seam's deadline; a deadline overrun surfaces as `Unavailable` (ADR-8), with a test using a stalling scripted engine if the seam supports it — otherwise record the gap explicitly in the task completion note rather than silently skipping (Process rule 5).
- [x] `cargo test -p tetond` green; no clippy warnings.

## Technical Notes

- The duty trait signature is `perform(&self, prompt: &str, provenance: &Provenance)`;
  the redactor receives the payload text as its prompt input and has no
  provenance of its own (it IS the inspection). Follow duty.rs's
  `bound_to_ceiling` for output bounding; input bounding is ADR-6's cap, which
  is stricter than truncation — never truncate-and-scan.
- Emission of `route_decided` on perform comes free from the seam (REQ-561
  ADR-8 / BR-2 of the spec's Events table).
- Confidence dedupe rule: High wins where spans overlap; do not double-report.
- LESSON-487 applies while implementing: if a seam constraint here contradicts
  a test, leave it red and report — do not edit the test to fit.

## Completion Notes

### The scan API TASK-070 calls

```rust
// crates/tetond/src/harness/redact.rs
pub const REDACT_DUTY: DutyKind;                    // Category::Redact, 2 KiB ceiling
pub const REDACT_OUTPUT_MAX_BYTES: usize = 2_048;   // 16 findings x 128 bytes
pub const REDACTION_OUTPUT_CONTRACT: &str;          // also the ScriptedFileEngine's recognizer
pub fn redact_prompt(payload: &str) -> String;      // exposed for fixtures; `scan` builds its own

pub async fn scan(route: &DutyRoute, payload: &str) -> RedactionVerdict;
```

`scan` is the whole surface the gate needs: the outbound text, and a
`DutyRoute` resolved for `REDACT_DUTY`. It is **total** — every failure is
`RedactionVerdict::unavailable()`, so there is no error arm for the caller to
map — and it never panics, never logs, and never returns anything derived from
the model's reply.

`RedactionGateImpl` (TASK-070) therefore looks like: resolve the route per
scan, `announcing(...)` it as the five siblings do so `route_decided` fires on
performance, then `harness::redact::scan(&route, text).await` and hand the
verdict to `egress::redact::decide`. Nothing else in this module needs calling.

**What collapses to `Unavailable`** (all four → `decide` → `Block`, ADR-6):

| cause | where it is decided |
|---|---|
| payload > `REDACT_INPUT_MAX_BYTES` | before the prompt is built — **zero** model calls (asserted by call count) |
| route unresolved | before the prompt is built |
| engine/provider error | `DutyRoute::perform`'s `Err` |
| deadline overrun (ADR-8) | the seam's own `DUTY_DEADLINE`, surfaced as `Err` |
| model reply unreadable | `read_findings` `Err` |

The verdict carries no sub-reason and deliberately so: the reported granularity
is TASK-068's `BlockCause::ScanUnavailable`, which is the distinction BR-3 asks
for ("could not run" vs "found something"). If TASK-070 wants a finer sentence,
add it at the gate — do not thread the model's failure text out of this module.

**A completed scan means both passes completed.** `scan` returns `scanned: true`
only when the pattern pass and the model pass both ran. An engine error on a
payload the pattern pass already flagged is still `Unavailable`, not `Findings`:
both block, but `Findings` rides with `scanned: true` and would claim a
completed scan of a payload the model never saw.

**Merge rule.** Pattern findings (High) seed the result; a model finding whose
span overlaps any kept finding is dropped, so High wins on overlap and one
secret is one finding. Output is sorted by span start.

### Deadline test — the gap the AC allowed for is NOT present

The AC allowed recording a gap if the seam could not express a stalling engine.
It can: `harness::redact::tests::a_scan_that_overruns_the_deadline_is_unavailable`
builds a `DutyRoute::Serves` over a `Duty` that records the call and then
`pending().await`s, runs on a paused clock, and asserts `Unavailable` + `Block` +
one recorded call (non-vacuity). No gap to record.

### Deviation: one existing test's census was updated

`harness::duty::tests` keeps `DUTY_MODULES`, a list of the modules where ADR-3
allows per-category source to live, and asserts `count("DutyKind::new(") ==
DUTY_MODULES.len()`. Creating `REDACT_DUTY` makes that 6 vs 5 and turns the test
red. Per LESSON-487 an existing test is not edited to fit a constraint — but this
is a **census, not an invariant**: the rule it enforces ("one `DutyKind` per duty
module and no others"; "no duty module carries the seam's concerns") is unchanged,
and REQ-562's architecture names `harness/redact.rs` as a new duty module. The
same species of census in `call_sites.rs` is documented as intended to go red and
be updated by the REQ that adds a call site.

So `harness/redact.rs` was **added** to `DUTY_MODULES` (and `Redact` to
`names_a_duty_category`'s `DUTIES`), which *strengthens* the scans: the new
module is now subject to the seam-plumbing check and to AC-10's
"no category is produced from text". No assertion was weakened or removed.
Reversible in one line if a reviewer disagrees.

### Mutations applied and observed (LESSON-441)

| Mutation | Turns red |
|---|---|
| the `redact` recognition arm moves after the other five | `a_scan_of_another_dutys_prompt_is_answered_as_a_redaction` |
| `read_findings` returns `Ok(vec![])` instead of `Err` on an unreadable reply (AC-8a at this layer) | `an_unreadable_answer_blocks_rather_than_passing_as_clean`, `an_unreadable_answer_is_an_error_never_an_empty_finding_list`, +2 |
| the input-cap short-circuit is removed (AC-8b) | `an_over_cap_payload_is_unavailable_before_any_model_call`, `an_over_cap_payload_carrying_a_credential_still_says_the_scan_could_not_run` |
| the overlap check in `merge` is removed | `a_string_both_passes_find_is_reported_once_at_high` |
| an unlocatable quoted string is reported at span `0..0` instead of dropped | `a_quoted_string_absent_from_the_payload_is_dropped_not_reported` |

### For TASK-070 / TASK-071

- The `redact` arm must stay **first** in `ScriptedFileEngine::complete`. A
  redact prompt's material is an outbound body, which for a `RemoteDuty` send is
  another duty's prompt verbatim — inside `DUTY_CONTRACT_PREFIX_BYTES`.
- `SCRIPTED_REDACTION` is `"NONE"`: the stand-in cannot judge sensitivity, so it
  answers "found nothing", which parses. Scripted fixtures therefore still block
  planted `sk-…`/`AKIA…` credentials **via the pattern pass**; there is no way to
  make the stand-in produce a `Low` finding, so a TASK-071 fixture that needs one
  must use a `MockEngine` with a canned contract-shaped reply.
- Pre-existing and untouched: `cargo fmt --all --check` fails on
  `crates/teton-core/src/config.rs:2229` (landed with TASK-067). Verified present
  with this task's changes stashed. Out of scope here; someone should fix it
  before the PR.
