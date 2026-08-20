---
id: TASK-192
title: "Verification sweep: workspace --no-fail-fast, margin tests, mutation checks on the bound/refit/report, constants one-home grep, guide headroom untouched"
status: complete
parent: REQ-586
created: 2026-08-19
updated: 2026-08-19
dependencies: ["TASK-191"]
repo: teton-code
---

## Description

The AC-13 gate and the adversarial pass before `/proceed` Phase 5: run the
whole workspace, prove the pins bite, and prove no number grew a second home.

## Files to Create/Modify

- (no new source) `crates/tetond/src/harness/budget.rs`, `router.rs`, `context.rs`, `egress/redact.rs` — only if a mutation below survives: add the missing assertion.
- This task file — a "Verification log" section with the failing test per mutation.

## Acceptance Criteria

- [x] `cargo fmt --all --check`, `cargo clippy --workspace --all-targets -- -D warnings`, `cargo test --workspace --no-fail-fast` green (grep the log for `FAILED` — LESSON-533).
- [x] Mutations each make ≥ 1 test fail (apply, run, revert; record the failing test): (a) `derive()` returns `Window` for `is_local`; (b) Degrade arm builds from `CapabilityProfile::default()` again; (c) reroute arm skips `rebudget`; (d) `truncate_to_budget` returns a zeroed report; (e) `REMOTE_TOKENS_PER_WORD` 3/2 → 1/1; (f) `RegisterProvider` replaces instead of merges; (g) `ContextLengthExceeded` given a `FailureClass`; (h) `REDACT_SCANNABLE_CONTEXT_BYTES` replaced by the literal `89_127` — the assertion passes (expected) but the one-home grep below catches it; (i) `with_redact_scan` ignored by `budget_for` → TASK-193's redact_egress AC-6 test fails.
- [x] `grep -rn "89_127\|89127" crates/ --include=*.rs | grep -v "//"` is empty; `4_096`/`1_500` each have exactly one non-test home (`budget.rs` `LOCAL_*`), and `32_768`/`12_000` appear in no non-test source (derived).
- [x] `git diff main -- crates/tetond/src/harness/self_config.md` is empty; both prompt-margin tests green with the ceiling unchanged.
- [x] Every task file in this REQ has status `complete`; the spec's automated AC checkboxes are ticked with the test name.

## Technical Notes

- **From TASK-187**: `context.rs`'s `DEFAULT_WINDOW_LABEL` and `budget.rs`'s private `LOCAL_WINDOW_LABEL` are two homes for the same string (a test asserts they agree, so drift is red). Collapse them in the one-home pass: make `budget.rs`'s label `pub(crate)` and have `context.rs` read it, then delete the equality test's reason for existing (keep the test if it still means something).

- LESSON-441 (a fix pass is new code — re-verify adversarially).
- Commit as `chore(REQ-586): verification sweep — mutations, one-home grep, headroom [TASK-192]`.

## Verification log

Run on 2026-08-19 against `feat/REQ-586-route-aware-context-budget`, in the
worktree, after TASK-181…TASK-191 and TASK-193 had landed.

### The gate (workspace-wide, as CI runs it)

| command | result |
|---|---|
| `cargo fmt --all --check` | clean (exit 0) |
| `cargo clippy --workspace --all-targets -- -D warnings` | clean (exit 0), no warnings emitted |
| `cargo test --workspace --no-fail-fast` | exit 0 — **3,159 passed / 0 failed / 1 ignored** over **59 targets**; `grep -n "FAILED"` on the log finds nothing (LESSON-533: the total is summed from all 59 `test result:` lines of a run that did not stop early, not from an interrupted one) |

Re-run in full *after* every fix below; the numbers above are the final run.

### Mutations

Each was applied to the working tree, the named test run, and the file
restored from a byte copy taken immediately before (never `git checkout`, which
would have taken this task's own edits with it).

| # | mutation | killed by |
|---|---|---|
| (a) | `derive()` returns `Window` for `is_local` | `harness::budget::tests::derivation_table`, `harness::budget::tests::precedence_is_pinned_pairwise` |
| (b) | Degrade arm builds from `CapabilityProfile::default()` again | **survived — equivalent mutant, see below** |
| (b′) | the Degrade arm stops stamping `budget_for(failed)` (the behaviour (b) was written to catch) | `router::tests::a_degrade_keeps_the_failed_providers_budget` |
| (c1) | the privacy-block reroute arm skips `refit_for_reroute` | `e2e::privacy_fixes::a_128k_turn_blocked_by_privacy_is_refitted_before_the_local_pin_serves_it` |
| (c2) | the failover reroute arm skips `refit_for_reroute` | `e2e::ac_matrix::ac7_degraded_provider_falls_back_and_completes` ("no refit_on_reroute before the fallback attempt") |
| (d) | `truncate_to_budget` returns a zeroed report | `harness::context::tests::a_gate_that_drops_three_blocks_reports_three_blocks` + 5 more (`an_oversized_newest_user_block_reports_the_bytes_it_lost`, `truncation_drops_oldest_and_marks_it`, `an_oversized_tool_result_is_elided_without_claiming_the_user_was`, `the_elision_marker_names_the_routes_own_window`, `rebudget_from_a_remote_pair_to_the_local_one_drops_and_reports`) |
| (e) | `REMOTE_TOKENS_PER_WORD` 3/2 → 1/1 | `token_corpus::words_guard_alone_covers_prose_but_not_dense_content`; also `harness::budget::tests::derivation_table`, `digest_thresholds_scale_with_the_pair_under_the_ceiling` |
| (e2) | the byte floor `DUTY_REQUEST_BYTES_PER_TOKEN` 2 → 4 (TASK-183's other half) | `token_corpus::combined_estimate_covers_every_sample_outside_the_documented_gap`, message naming `minified.json` |
| (f) | `RegisterProvider` replaces the window fields instead of merging them | `config_preservation::a_field_less_registration_preserves_the_stored_window_and_a_declared_one_writes_it` |
| (g) | `ContextLengthExceeded` given a `FailureClass` | `teton_providers::tests::provider_error_maps_to_failure_class`, `teton_providers::tests::a_400_with_a_vendor_spelling_is_the_typed_context_length_refusal` |
| (h) | `REDACT_SCANNABLE_CONTEXT_BYTES` written as the literal `89_127` | **passes, as predicted** — every budget and redact assertion is green (which also confirms the constant's value is 89,127); the one-home grep is the only thing that sees it, and it reports both lines |
| (i) | `budget_for` ignores `with_redact_scan` | `router::tests::the_route_budget_is_derived_from_the_routes_own_window` — but **not** the AC-6 test the task expected; assertion added, see below |

**(b) is an equivalent mutant, and that is ADR-2 working.**
`CapabilityProfile::harness_profile()`'s `Degraded` arm is a constant that reads
no field but the tier, so a profile built from `capability_of(failed)` and one
built from `CapabilityProfile::default()` produce the identical
`HarnessProfile`; no test can tell them apart. The budget the mutation was
written to protect no longer travels through that profile at all — it is
stamped from `budget_for(failed)`, which (b′) proves is pinned. The doc comment
beside the expression claimed the *old* hazard ("a derivation from the default
profile would silently re-budget a 128k route"), which is no longer true of this
code; it now says what actually holds the window and records the equivalence.

**Assertions added because a mutation was not caught where it should have been:**

- **(i)** `tests/redact_egress.rs::a_redact_scanned_128k_route_assembles_a_body_the_scan_reads_whole_and_forwards`
  advertised itself as this mutation's target but called `budget::derive`
  directly on both legs, so the router's `with_redact_scan` wiring was outside
  it. It now also demands the same two pairs of `Router::budget_for` — red under
  (i), green after the revert.
- **(b)** `router::tests::a_degrade_keeps_the_failed_providers_budget` gained a
  capped-provider leg: a degrade on a provider with `context_budget_cap` keeps
  `bound: user_cap` and the capped pair. Proven non-vacuous by a sanity mutation
  (`budget_for` passing `cap: 0`), which turns it red. The pair alone would not
  have caught a wrong budget source that happened to coincide.

### One-home greps

- `grep -rn "89_127\|89127" crates/ --include=*.rs | grep -v "//"` — **empty.**
  It was not: `crates/teton/src/session_ui.rs` had copied the value into a
  route-line render assertion, in a crate that cannot even see the constant (the
  CLI depends on `teton-protocol` and `teton-core` only). The sample is now a
  round 89,000 with a comment saying the bound's one home is the daemon's.
- `1_500` — exactly one non-test home, `budget.rs::LOCAL_DIGEST_THRESHOLD_TOKENS`.
- `12_000` — no non-test home (derived from `LOCAL_DIGEST_THRESHOLD_TOKENS`).
- `32_768` — no non-test home **after a fix**: `compact.rs::COMPACT_OUTPUT_MAX_BYTES`
  restated it while its own doc said it *is* the default context byte budget. It
  now reads `budget::LOCAL_BUDGET_BYTES`.
- `4_096` — one non-test home *of this fact*,
  `budget.rs::LOCAL_BUDGET_TOKENS`. Nine other non-test lines hold the same
  round number for unrelated facts (`EFFORT_REFUSAL_SNIFF_BYTES`,
  `GIT_FILE_MAX_BYTES`, `REDUCTION_ENVELOPE_RESERVE_BYTES`,
  `MAX_MATCH_LINE_BYTES`, `MAX_TOPIC_BYTES`, `DUTY_MAX_TOKENS_REQUEST`, an MCP
  id length check, a download progress step, and Ollama's *served* 4k window in
  `provider_recipes.rs`). Aliasing those to the budget constant would invent a
  coupling that does not exist, so they are left alone and named here instead —
  the grep is not weakened, its answer is classified.

Classification was done by script (a hit at a line after the file's first
`#[cfg(test)]`, or under `tests/`, counts as test code).

### Headroom, and the guide

- `git diff main -- crates/tetond/src/harness/self_config.md` — **empty (0 bytes).**
- `REDACT_BODY_OVERHEAD_BYTES` is `10 * 1024` on this branch and on `main` —
  byte-identical (BUG-181's value, unmoved).
- Both prompt-margin tests green with it unmoved:
  `egress::redact::tests::the_total_cap_clears_the_harness_context_budget_with_margin`
  and `harness::tools::web::tests::the_web_tool_docs_clear_the_outbound_body_overhead`.

### Other work this sweep folded in

- **TASK-187's handover.** `budget.rs::LOCAL_WINDOW_LABEL` is now `pub(crate)`
  and `context.rs::DEFAULT_WINDOW_LABEL` reads it, so the sentence has one
  literal. The equality test is **kept**: after the alias it no longer guards two
  literals, but it still pins `derive`'s *local arm* to that label, which the
  alias does not — `derive` returning any other constant for `is_local` is still
  red. Its doc says so.
- **TASK-183's deferred swap.** `tests/token_corpus.rs` reads
  `REMOTE_TOKENS_PER_WORD_NUM`/`_DEN` from `harness::budget` and the byte floor
  from `harness::duty::DUTY_REQUEST_BYTES_PER_TOKEN` (promoted `pub(crate)` →
  `pub` for it, with the reason in its doc) instead of restating `3`, `2` and
  `2`. Mutations (e) and (e2) show the suite stayed an empirical claim about the
  corpus rather than becoming a restatement of the constants. The base64
  `KNOWN_UNCOVERED` posture is untouched, and
  `python3 tools/token_corpus/count.py --check` (tiktoken 0.14.0, the version
  the fixture records) reports the committed `token_counts.json` current.
