---
id: TASK-393
title: "Derive the redact scan and compact prompt caps from the window"
status: complete
parent: REQ-616
created: 2026-09-04
updated: 2026-09-04
dependencies: [TASK-392]
---

## Description

`REDACT_SCANNABLE_CONTEXT_BYTES`, `REDACT_CHUNK_MAX_BYTES` and
`COMPACT_PROMPT_BUDGET_*` are `const`s computed from the window. Turn them into
`const fn`s of it, keeping today's constants as the function's value at the
default window (ADR-616-2), so REQ-562's "one number, one place" property holds
at any window and the tests that pin 184,265 keep a name to pin.

## Files to Create/Modify

- `crates/tetond/src/egress/redact.rs` — `redact_scannable_context_bytes(n_ctx)`,
  `redact_chunk_max_bytes(n_ctx)`; existing constants defined at the default
- `crates/tetond/src/harness/compact.rs` — `compact_prompt_budget_bytes(n_ctx)`
  and its token twin; existing constants defined at the default
- `crates/tetond/src/router.rs` — pass the live window where the scan bound is
  applied

## Acceptance Criteria

- [ ] `redact_scannable_context_bytes(LOCAL_ENGINE_N_CTX_DEFAULT)` equals today's
      `REDACT_SCANNABLE_CONTEXT_BYTES` exactly — the 184,265 assertion in
      `redact_egress.rs` passes untouched
- [ ] At a 262,144 window the scan bound scales with it, and a route whose byte
      budget exceeds the scan still reports `bound = redact_scan` with the exact
      figure (BR-7, AC-10)
- [ ] The `redact` egress suite passes at both windows
- [ ] The digest threshold test passes at 32K and 262K with the same fraction,
      and a test pins the fraction itself (BR-8, AC-8)
- [ ] `COMPACT_PROMPT_BUDGET_*` scales with the window

## Verification

| rule | kind | artifact | benign_path |
|------|------|----------|-------------|
| BR-7 | test-case | `crates/tetond/tests/redact_egress.rs::scan_bound_follows_the_window` | yes |
| BR-8 | test-case | `crates/tetond/src/harness/budget.rs::digest_fraction_is_pinned_and_scale_free` | no |
| AC-8 | test-case | `crates/tetond/src/harness/budget.rs::digest_thresholds_same_fraction_at_32k_and_262k` | no |
| AC-10 | test-case | `crates/tetond/tests/redact_egress.rs::route_over_scan_reports_redact_scan_bound` | yes |

## Technical Notes

- These must stay `const fn` so the default-window constants remain `const`
  contexts. If a computation cannot be `const`, keep the constant literal and
  add a test asserting the function agrees with it — do not silently downgrade
  the constant to a `static`.
- The redact duty runs on the local engine, which is why its bound follows the
  local window and not the route's provider window (BR-7).
