---
id: TASK-003
title: "The self-config guide carries the command names, and the prompt margin is re-measured last"
status: draft
parent: REQ-617
created: 2026-09-04
updated: 2026-09-04
dependencies: ["TASK-001", "TASK-002"]
---

## Description

BR-1's resident half and AC-3. Per ADR-2 the guide carries the command *names*
grouped by family plus BR-1's closing sentence; the effects live in
`teton_docs commands` (TASK-002). Per REQ-583 ADR-2 the task that measures a
composed artefact runs **after** every task that writes to it, which is why this
depends on TASK-002 — the topic index is resident prompt too.

## Files to Create/Modify

- `crates/tetond/src/harness/self_config.md` — the roster line(s) and BR-1's
  sentence: *"You cannot run any of these. Name the one the user should type,
  then stop."*
- `crates/tetond/src/egress/redact.rs` — `RECORDED_PROMPT_MARGIN_BYTES` and
  `RECORDED_WEB_PROMPT_MARGIN_BYTES` re-measured, and a ledger line on
  `REDACT_BODY_OVERHEAD_BYTES` naming this REQ and what it spent.

## Acceptance Criteria

- [ ] The guide names every `SESSION_COMMANDS` name.
- [ ] The two margin constants are **re-measured from the failing assertion's
      reported figure**, never computed by hand (the doc comment's own
      instruction: re-measure, do not reason).
- [ ] The two margins stay exactly 47 bytes apart, which is the check that this
      change spent the same bytes on both prompt shapes.
- [ ] `REDACT_BODY_OVERHEAD_BYTES` is **unchanged** at 23 KiB, and therefore
      `REDACT_TOTAL_CAP_CHUNKS`, `REDACT_INPUT_MAX_BYTES`,
      `REDACT_SCANNABLE_CONTEXT_BYTES` and `REDACT_MAX_CHUNKS` are unchanged.
- [ ] The remaining margin is at or above `MIN_PROMPT_HEADROOM_BYTES` (48). If
      it is not, the roster is shortened — not the ceiling raised (ADR-2).

## Verification

| rule | kind | artifact | benign_path |
|------|------|----------|-------------|
| BR-1 | test-case | `crates/tetond/src/egress/redact.rs::tests::the_system_prompt_clears_the_outbound_body_overhead` | no |
| AC-3 | test-case | `crates/tetond/src/egress/redact.rs::tests::the_system_prompt_clears_the_outbound_body_overhead` | no |

## Technical Notes

The existing web-shape twin
(`harness::tools::web::tests::the_web_tool_docs_clear_the_outbound_body_overhead`)
measures the other prompt shape against the same constants and will fail in the
same commit. Both numbers move together or the pair is inconsistent.

29 names at their raw spelling total 360 bytes with `/` and a separator. Grouping
by family (`/model`, `/model set|list|status`) is what buys the fit; do not
invent abbreviations, because a name the model cannot type verbatim is worse
than a name it does not have.
