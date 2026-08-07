---
id: TASK-069
title: "The redact duty: model pass, output contract, and quarantined-output parsing"
status: draft
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

- [ ] `REDACT_DUTY` follows the DutyKind pattern (category + ceiling); the prompt embeds `REDACTION_OUTPUT_CONTRACT` within `DUTY_CONTRACT_PREFIX_BYTES` so the scripted engine recognizes it, and the contract wording cannot be confused with grep-result content (see the REQ-561 disambiguation trap in runtime.rs).
- [ ] Model-output parsing: a reply quoting a substring present in the payload yields a Low finding with the correct byte span and NO text field; a reply quoting a substring NOT in the payload yields no finding (hallucination-drop test); a malformed/empty reply yields `Unavailable`, never `Clean` (a parse failure is a scan that did not run — BR-3, LESSON-447).
- [ ] The model's raw reply is never logged, never embedded in any error string, and never leaves the parse function's scope — asserted where practical (error paths return static/derived strings only).
- [ ] `scan()` composes both passes: pattern hits High, model-only hits Low; a string found by both passes reports once at High (dedupe by overlapping span); over-cap input short-circuits to `Unavailable` before any model call (with the ADR-6 allowance: the pattern pass MAY run first, but the outcome for over-cap without a High hit is still Unavailable → Block, never Forward).
- [ ] Duty-seam integration: performing through `DutyRoute` respects the seam's deadline; a deadline overrun surfaces as `Unavailable` (ADR-8), with a test using a stalling scripted engine if the seam supports it — otherwise record the gap explicitly in the task completion note rather than silently skipping (Process rule 5).
- [ ] `cargo test -p tetond` green; no clippy warnings.

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
