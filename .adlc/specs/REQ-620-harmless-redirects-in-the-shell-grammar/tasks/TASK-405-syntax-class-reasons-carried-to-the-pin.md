---
id: TASK-405
title: "The unmodelled scan names its syntax class, and the reason rides the provenance bit to the pin notice"
status: draft
parent: REQ-620
created: 2026-09-09
updated: 2026-09-09
dependencies: ["TASK-403"]
repo: teton-code
---

## Description

Replace the single "shell syntax this classifier does not model" sentence with one
content-free sentence per `UnmodelledSyntax` class, reported for the first offending class
in the fixed order. Carry the verdict's reason as an explicit `Option<&'static str>` beside
the unknown bit on `ToolProvenance`, through the egress `Provenance`, the tainting sink and
`TaintRegistry::mark`, onto an additive `SessionPinned.reason`, and into the CLI notice
(ADR-620-2 step 2, ADR-620-4).

## Files to Create/Modify

- `crates/tetond/src/harness/tools/shell_provenance.rs` — `UnmodelledSyntax` enum with `reason(self) -> &'static str`; the scan reports the first class in order `quote, substitution, variable, redirect, glob, brace, escape, history`
- `crates/tetond/src/harness/tools/mod.rs` — `ToolProvenance::from_bits(sources, unknown: Option<&'static str>, out_of_root)`; the unknown-reason accessor
- `crates/tetond/src/harness/tools/shell.rs` — pass `verdict.unknown_reason()` at the `from_bits` call; the stderr line unchanged
- `crates/tetond/src/harness/tools/skill.rs`, `crates/tetond/src/harness/context.rs` — the fold passes the same reason it already writes to `reach_reason`
- `crates/tetond/src/egress/provenance.rs` — `Provenance` carries the reason beside `unknown`; `with_unknown_lifted` clears both
- `crates/tetond/src/runtime/taint.rs` — `mark(session, cause, reason: Option<&'static str>)`, `session_pinned_payload(cause, budget, reason)`, the sink's `UNKNOWN_PROVENANCE_PATH` arm passes it
- `crates/teton-protocol/src/events.rs` — `SessionPinned.reason: Option<String>` with `#[serde(default, skip_serializing_if = "Option::is_none")]`; round-trip test with and without
- `crates/teton/src/session_ui.rs` — `format_session_pinned` appends ` — <reason>` after the cause when present; `session_pin_render` tests
- `crates/tetond/tests/e2e/skill_provenance.rs` — one preamble per class asserts `reach_reason`
- `crates/tetond/tests/e2e/shell_pin_shape.rs` — a pinning shell call asserts `session_pinned.reason` and that no event carries a planted marker from the command text

## Acceptance Criteria

- [ ] Eight classes, eight distinct sentences, each naming only its class; the sentence for a command with both a quote and a glob is the quote's
- [ ] `session_pinned` for an `unknown_shell` pin carries `reason`; for a `boundary_hit` pin it is absent
- [ ] `skill_invoked.outcomes[].reach_reason` for a preamble with each class carries that class's sentence
- [ ] The CLI notice reads `cause: unknown_shell — the command uses a quoted string this classifier does not model. \`/shell allow\` lifts it …`
- [ ] A marker string planted only in the command text appears in no published event and in no captured provider request (egress-capture posture, LESSON-624)
- [ ] `PROTOCOL_VERSION_MIN == MAX == 2` unchanged; the wire round-trip test covers `reason` present and absent

## Verification

| rule | kind | artifact | benign_path |
|------|------|----------|-------------|
| BR-6 | test-case | `crates/tetond/src/harness/tools/shell_provenance.rs::tests::each_unmodelled_class_names_itself_and_nothing_else` | no |
| BR-6 | test-case | `crates/tetond/tests/e2e/shell_pin_shape.rs::a_pin_carries_the_class_that_refused_and_no_command_bytes` | yes |
| BR-6 | test-case | `crates/teton/src/session_ui.rs::session_pin_render::a_reason_follows_the_cause_and_a_boundary_hit_has_none` | yes |
| AC-6 | test-case | `crates/tetond/tests/e2e/skill_provenance.rs::each_class_reaches_reach_reason_verbatim` | no |
| AC-6 | test-case | `crates/tetond/tests/e2e/shell_pin_shape.rs::a_pin_carries_the_class_that_refused_and_no_command_bytes` | yes |

## Technical Notes

- `&'static str` is the content-freeness proof; do not introduce a `String` reason
  anywhere between the classifier and the event. The protocol field is `String` only
  because the wire type cannot be static.
- LESSON-653: the reason is a field on the value both readers share. Do not re-derive the
  class at the notice from the cause string.
- LESSON-650: `with_unknown_lifted` must clear the reason too, or a lifted session's next
  block would report a stale class.
- The `boundary_hit` arm keeps `reason: None`; the file is named by `privacy_block`.
