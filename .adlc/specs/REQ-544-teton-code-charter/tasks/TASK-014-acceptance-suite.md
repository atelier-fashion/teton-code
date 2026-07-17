---
id: TASK-014
title: "End-to-end acceptance suite: AC-1..AC-9 scripted verification"
status: draft
parent: REQ-544
created: 2026-07-17
updated: 2026-07-17
dependencies: [TASK-011, TASK-012, TASK-013]
---

## Description

Scripted end-to-end verification of every REQ-544 acceptance criterion against
real binaries (mock transports/engines where hardware or live APIs are
required), runnable locally and in CI. This is the "Verify, Don't Trust" gate
for the whole MVP.

## Files to Create/Modify

- `tests/e2e/harness.rs` — spawn real `tetond` + drive `teton` (or protocol client) against temp HOME/repo fixtures; egress-capture proxy; simulated hardware profiles via env override
- `tests/e2e/ac_matrix.rs` — one test per AC: AC-1 first-run offline path, AC-2 two providers, AC-3 phase routing, AC-4 cost meter, AC-5 privacy egress, AC-6 multi-client, AC-7 degradation, AC-8 probe/step-down, AC-9 MCP
- `tests/e2e/fixtures/` — demo repo, mock provider server (OpenAI-compat shape), mock MCP server, canned model catalog
- `.github/workflows/ci.yml` — add e2e job (modify)
- `.adlc/specs/REQ-544-teton-code-charter/requirement.md` — check off verified ACs (modify, at completion)

## Acceptance Criteria

- [ ] Every AC-1..AC-9 has a distinct automated test that fails when its feature is broken (spot-verified by mutation: break one thing, watch its test fail)
- [ ] Egress capture asserts BR-1 across the FULL suite run, not only AC-5's test (any boundary byte in any captured payload fails the run)
- [ ] Suite runs in CI without model weights or live API keys (mocks); a local `--features live` mode exists for real-model smoke runs
- [ ] Suite completes < 10 min in CI (parallelized; it gates every future PR)

## Technical Notes

The mutation spot-check in AC list is the skeptical-by-default guard against
vacuous tests — do it for at least AC-5 and AC-9 (the security-relevant ones).
Keep fixtures tiny; the demo repo needs ~5 files. This task also produces the
first honest first-pass-success + cost data for the structured-mode pitch
(record observations in `.adlc/knowledge/` at wrapup).
