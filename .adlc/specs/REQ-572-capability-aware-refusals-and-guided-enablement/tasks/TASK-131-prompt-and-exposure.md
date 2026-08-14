---
id: TASK-131
title: "Harness: per-state prompt clauses, bundled [web] guide, shared exposure predicate"
status: complete
parent: REQ-572
created: 2026-08-13
updated: 2026-08-13
dependencies: ["TASK-128"]
---

## Description

The refusal half (spec BR-1/BR-2/BR-3): replace the single `WEB_OPT_IN_CLAUSE`
with per-`WebCapabilityState` clauses naming `/web setup`, extend the BUG-160
bundled guide with the `[web]` enablement surface, add the once-per-
conversation dedup instruction, re-express `register_web_tool`'s predicate as
a consumer of `web_capability_state`, and emit the dead-end event at the web
tool's tier-gap refusals (architecture ADR-4).

## Files to Create/Modify

- `crates/tetond/src/harness/turn_loop.rs` — replace `WEB_OPT_IN_CLAUSE` (line ~954) with a `fn web_capability_clause(state: &WebCapabilityState) -> Option<&'static str>`: OffAvailable → clause naming that web lookup exists, is off, and is enabled with `/web setup` (or the `[web]` config table), imperative "tell the user this — do not search the repository for it" (LESSON-493 phrasing discipline, LESSON-482 every-ending-named); SearchUnavailable → clause naming fetch works and why search does not; Ready → None. Add the dedup instruction sentence ("if you already named the opt-in in this conversation, refer to it in one line"). Thread the state into `build_system_prompt` via `HarnessConfig` (additive field) — the callers in `prepare`/tests supply it from `web_capability_state`.
- `crates/tetond/src/harness/self_config.md` — new `[web]` section: `/web setup`, the `[web]` table keys, keychain-reference rule, search-needs-local-model note, and the BUG-165 `search_auth` template with the two named backend examples. Keep it terse — headroom is measured (BUG-160 sized the guide at ~1 KB; the ceiling test is the arbiter).
- `crates/tetond/src/harness/tools/web.rs` — `register_web_tool` consumes `web_capability_state` for its registration decision (BR-3 single classifier); the tier-gap refusal paths (model calls above granted/configured tier) publish the dead-end event through the existing seam's event sink.
- `crates/tetond/src/harness/turn_loop.rs` — regression tests in the BUG-160 pattern: pin the OffAvailable clause content (names `/web setup` AND the config table) and the SearchUnavailable clause on BOTH default and strong-model profiles, with the update-not-delete failure message; extend the headroom test so the enlarged guide + clauses still clear `REDACT_BODY_OVERHEAD_BYTES` with asserted margin (AC-9).

## Acceptance Criteria

- [x] With state OffAvailable, `build_system_prompt` output contains the clause naming the capability, its off state, and both enablement paths; with Ready it contains neither clause — pinned on both prompt profiles — `the_off_clause_names_the_capability_its_off_state_and_both_enablement_paths` (content predicates: "available", "switched off", `/web setup`, `[web] tier`, "repositor"), `a_ready_capability_gets_neither_clause_on_either_profile` (all three tiers), `the_search_unavailable_clause_names_the_gap_and_keeps_fetching_alive`, `every_capability_clause_carries_the_repeat_instruction_and_only_a_clause_does`, and `an_unstated_capability_falls_back_to_the_tool_registry` for the additive field's no-regression promise (all in `turn_loop.rs`)
- [x] The headroom test passes with the new text and asserts remaining margin > 0 bytes explicitly in its failure message — **two** tests, because the opted-out and opted-in shapes are measured in different modules: `the_total_cap_clears_the_harness_context_budget_with_margin` (`egress/redact.rs`) now sweeps every capability state, and `the_web_tool_docs_clear_the_outbound_body_overhead` (`tools/web.rs`) sweeps the states the registered tool can be in — the second is the real worst case (guide + `SearchUnavailable` clause + description + schema) and clears the ceiling by **87 bytes**; both messages state the overage and the margin assertion is separate from the fit assertion
- [x] `register_web_tool` registers iff `web_capability_state` is not OffAvailable — asserted by a test that feeds both paths the same `WebConfig` values — `registration_is_the_capability_classifiers_exposure_predicate`, sweeping `WebTier::ALL` × both local-model answers
- [x] A scripted-engine turn with the tool registered at fetch_user_url and a model-composed URL call emits the dead-end event alongside the existing tier refusal (existing refusal text unchanged — REQ-563 AC-4 regression intact) — `a_tier_gap_in_a_scripted_session_announces_the_capability_it_ran_out_of` (`tests/web_lookup_egress.rs`), with the served-at-`fetch_any_url` falsification leg; the loop's half is pinned separately by `a_tool_that_names_its_dead_end_gets_it_announced_to_the_session` / `an_ordinary_tool_result_announces_no_dead_end`

## Technical Notes

Coordinate with the in-flight refusal-wording session (architecture
"Interaction with in-flight work"): write clause tests against content
predicates (`contains("/web setup")`, `contains("[web] tier")`), not exact
strings, so a merge of strengthened wording composes. `build_system_prompt`'s
signature: prefer the additive `HarnessConfig` field over a new parameter —
`template_smoke.rs`, `offline_session.rs`, `remote_loop.rs` all call it and
should compile with a defaulted field.

## Implementation notes (as built)

**Surfaces added.** `turn_loop.rs`: `HarnessConfig::web_capability`
(`Option<WebCapabilityState>`, `None` = unstated), the private
`web_capability_clause` / `effective_web_clause`, the three clause constants
(`WEB_OFF_AVAILABLE_CLAUSE`, `WEB_SEARCH_UNAVAILABLE_CLAUSE`,
`CAPABILITY_REPEAT_CLAUSE`), and `SessionEvents::capability_dead_end`.
`harness/tools/mod.rs`: `ToolOutcome::dead_end` + the `dead_ending` builder.
`self_config.md`: the one-line `[web]` section.

Deviations from the letter of this file, each with its reason:

1. **The dead end travels on the outcome, not through the seam.** The task
   sketched publishing it "through the existing seam's event sink". A
   `WebLookupSeam` method could only have shipped with a **defaulted no-op**,
   because the one implementation with an event bus is `RuntimeLookupSeam` in
   `runtime.rs` — a file this task was explicitly forbidden to touch — so the
   daemon would have announced nothing until someone remembered the override:
   a mitigation wired to nothing (LESSON-492's shape). Instead the tool marks
   its outcome (`ToolOutcome::dead_end`, a catalog id) and `run_session_turn`
   publishes it from the `SessionEvents` it already holds. This works in the
   real daemon **today**, needs no `runtime.rs` change, keeps text out of the
   decision (nothing reads `content` to classify a refusal — ADR-4's rule), and
   generalizes: the next tool with a capability gap declares it the same way.
   Cost: one extra file, `harness/tools/mod.rs`, for a field and a builder.
2. **`web_capability_clause` returns `Option<String>`, not
   `Option<&'static str>`.** The `SearchUnavailable` clause renders
   `SearchGap::as_str()` into a `{reason}` slot rather than re-phrasing the gap,
   and a rendered clause is not a `'static` literal. The repeat instruction is
   appended by this one function so the two states cannot word it differently.
3. **`None` means "unstated", and the registry is the fallback.** With the field
   defaulted, `effective_web_clause` keys on `tools.get(WEB_TOOL_NAME)` exactly
   as the prompt did before this REQ (tool absent → the off clause; present →
   none), so no existing caller regresses. What the registry cannot say is which
   tier is granted or whether search can serve — so the `SearchUnavailable`
   clause reaches a real session only once the daemon supplies the state
   (see the hookup below).
4. **`register_web_tool` asks the classifier under a named constant.**
   `web_capability_state` needs `local_model_present`, which this call site does
   not have and does not need: exposure is the one question the classifier
   answers identically for both answers. The call passes
   `EXPOSURE_IGNORES_THE_LOCAL_MODEL = false` and the independence is asserted
   in every cell (both here and in `teton-core`), rather than assumed in a
   comment.
5. **Files touched beyond the four listed.** `harness/tools/mod.rs` (deviation
   1); `egress/redact.rs` (the headroom test this task extends lives there);
   `tests/web_lookup_egress.rs` (its two prompt pins named the replaced clause
   verbatim, and it is where a scripted session with the real tool, gate, loop
   and choke point already exists — AC-4's natural home). `runtime.rs` was not
   touched.
6. **Prompt headroom is now the binding constraint.** The bundled guide's
   `[web]` section had to be compressed to a single line, and both clauses
   tightened twice, for the worst-case prompt (guide + `SearchUnavailable`
   clause + web description + schema) to clear `REDACT_BODY_OVERHEAD_BYTES` at
   all: it clears by **87 bytes**. The next sentence added to `self_config.md`
   will turn `the_web_tool_docs_clear_the_outbound_body_overhead` red, which is
   the intended arbitration — but a follow-up should decide deliberately whether
   to re-derive that constant (it is test-only: nothing in production reads it)
   or to compress the BUG-160 provider section the same way.
7. **Catalog ids come from `permission_key_for`.** A tier gap names the
   capability the *call needed* — `web_fetch_any_url` for a model-composed URL
   above a `fetch_user_url` ceiling, `web_search` for a search above a fetch
   ceiling — which is the requirement's four-id catalog exactly. TASK-129's
   `CapabilityDeadEnd::WEB_SEARCH` names only the search one; the two spellings
   are pinned equal by an assertion rather than left to agree by luck. The
   protocol has no constant for the two fetch ids yet — worth adding beside
   `WEB_SEARCH` in TASK-133/134 if the client ever branches on them.

**Left for the orchestrator (runtime.rs, out of scope here).** The
`SearchUnavailable` clause cannot reach a real session until the daemon states
the capability. One place, `runtime.rs` at the `build_system_prompt` call
(~line 2356), where `route` is already `mut` and both inputs already exist
(TASK-129 added `local_model_present` and imports `web_capability_state`):

```rust
// REQ-572 BR-1: the prompt's capability clause needs the state, and this is
// the layer that can read both of its inputs.
route.harness.web_capability =
    Some(web_capability_state(&config.web, self.local_model_present()));
let system = build_system_prompt(&tools, &route.harness);
```

Without it the daemon keeps today's behaviour plus the improved off-clause
(the registry fallback covers `OffAvailable` ⟺ tool absent); with it, a
search-configured machine with no local model stops being told nothing.

**Suite state at commit.** `cargo test -p tetond --no-fail-fast`: 1,503 passed,
0 failed. `cargo check --workspace --all-targets`, `cargo clippy -p tetond
--all-targets`: clean.
