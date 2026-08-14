---
id: TASK-131
title: "Harness: per-state prompt clauses, bundled [web] guide, shared exposure predicate"
status: draft
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

- [ ] With state OffAvailable, `build_system_prompt` output contains the clause naming the capability, its off state, and both enablement paths; with Ready it contains neither clause — pinned on both prompt profiles
- [ ] The headroom test passes with the new text and asserts remaining margin > 0 bytes explicitly in its failure message
- [ ] `register_web_tool` registers iff `web_capability_state` is not OffAvailable — asserted by a test that feeds both paths the same `WebConfig` values
- [ ] A scripted-engine turn with the tool registered at fetch_user_url and a model-composed URL call emits the dead-end event alongside the existing tier refusal (existing refusal text unchanged — REQ-563 AC-4 regression intact)

## Technical Notes

Coordinate with the in-flight refusal-wording session (architecture
"Interaction with in-flight work"): write clause tests against content
predicates (`contains("/web setup")`, `contains("[web] tier")`), not exact
strings, so a merge of strengthened wording composes. `build_system_prompt`'s
signature: prefer the additive `HarnessConfig` field over a new parameter —
`template_smoke.rs`, `offline_session.rs`, `remote_loop.rs` all call it and
should compile with a defaulted field.
