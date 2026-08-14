---
id: TASK-144
title: "Guide + README recipes, referral clause, and prose gates"
status: complete
parent: REQ-577
created: 2026-08-14
updated: 2026-08-14
dependencies: ["TASK-143"]
repo: teton-code
---

## Description

Add compact vendor recipe lines and the referral-posture sentence to the
bundled self-config guide, pin the README's existing (currently unpinned)
provider examples, and add the bidirectional prose↔catalog CI gates
(spec BR-1, BR-2, BR-4, BR-5; ADR-2, ADR-4).

## Files to Create/Modify

- `crates/tetond/src/harness/self_config.md` — add one compact endpoint line
  covering all six vendors inside recipe step 1 (target ≤ 450 added bytes),
  plus the dictated referral sentence beside the existing never-ask-for-a-key
  rule: the agent cannot run these commands itself and must give the user the
  exact commands to run (BUG-168 wording rules: imperative, no em-dash aside).
- `README.md` — "Hooking up an external model" section: keep the bash block
  but make its vendor facts (currently `kimi` + `api.moonshot.ai/v1`,
  unpinned) agree with the catalog; add Grok/DeepSeek/Ollama one-liners only
  if they fit the section's voice — every fact shown must be catalog-backed.
- `crates/tetond/tests/web_setup_contracts.rs` — new
  `the_bundled_guide_and_the_recipe_catalog_agree` and
  `the_readme_recipes_and_the_catalog_agree`: every catalog endpoint appears
  in the prose copy AND every endpoint the prose names is in the catalog
  (both directions; anchored parsing, fail closed on missing markers).
- `crates/tetond/src/harness/turn_loop.rs` — extend the BUG-160-lineage
  guide-content pin tests (both harness profiles) to pin the referral clause,
  with the update-don't-delete failure message.

## Acceptance Criteria

- [x] Both margin tests stay green with recorded headroom updated in their
  comments: `the_total_cap_clears_the_harness_context_budget_with_margin`
  (egress/redact.rs:1938) and
  `the_web_tool_docs_clear_the_outbound_body_overhead` (tools/web.rs:2227)
  — the 48-byte floor is never traded away (BR-4). If the floor breaks,
  apply the ADR-2 fallback (recipes move to the providers topic; gates
  retarget) instead of shrinking the floor.
- [x] Mutating a catalog endpoint without the guide fails the new gate, and
  editing a guide/README endpoint without the catalog fails it too —
  demonstrate both directions once locally and note it in the commit message
  (AC-7).
- [x] Referral clause pinned on both profiles; existing key-rule sentence
  preserved verbatim (BR-5).
- [x] `cargo test -p tetond` green; clippy + fmt clean.

## Technical Notes

- Guide is 1,645 bytes today against an 8,192-byte total-prompt ceiling that
  also carries clauses + tool docs; BUG-168 had to shorten phrases to afford
  one clause — write the recipe line tight and measure early.
- Parse prose the way `the_bundled_guide_and_the_catalog_agree`
  (web_setup_contracts.rs:394) does: anchored extraction, never regex-loose.
- Do not reword existing pinned guide sentences — the pins tell you which.
