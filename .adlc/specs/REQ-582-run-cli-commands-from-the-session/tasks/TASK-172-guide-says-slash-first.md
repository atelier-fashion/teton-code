---
id: TASK-172
title: "The guide says the session spelling first, under its 2-byte budget; guide/table cross-check test"
status: complete
parent: REQ-582
created: 2026-08-18
updated: 2026-08-18
dependencies: [TASK-169]
repo: teton-code
---

## Description

ADR-7 / BR-9 / AC-10. Rewrite `crates/tetond/src/harness/self_config.md` so
every mirrored command it names appears in its `/` spelling (`/policy
set-tier`, `/policy set-category`, `/policy show`, `/provider list`,
`/doctor`), with one short sentence teaching shell users the mapping, while:
(a) `the_total_cap_clears_the_harness_context_budget_with_margin`
(`crates/tetond/src/egress/redact.rs`) stays green — today's margin is **50
bytes against a floor of 48**, so the guide may grow by ≤2 bytes: pay for
new text by shortening; (b) REQ-579's step-1 test (`/provider setup` before
`teton provider add`, "shell only" present) and REQ-581's step-3 test
(`` `/provider test <id>` `` in step 3) stay green; (c) the prohibition line is
byte-identical and remains the only line containing "ask"; (d) "You cannot
run these commands yourself; hand them to the user." stays. Add the AC-10
cross-check test in `crates/teton` reading the guide via
`include_str!("../../tetond/src/harness/self_config.md")`: every `teton
<sub>` named in the guide whose `<sub>` is a mirrored row must also appear
as `/<sub>` — with the explicit equivalence `provider add → /provider setup`.

## Files to Create/Modify

- `crates/tetond/src/harness/self_config.md` — the rewrite.
- `crates/tetond/src/harness/turn_loop.rs` — only if a pinned sentence legitimately changed: update the expectation deliberately (the tests say so); do not weaken.
- `crates/teton/src/cli_rows.rs` (tests) — `the_guide_names_every_mirrored_command_in_its_session_spelling` (AC-10) with the equivalence map.
- `crates/tetond/src/provider_recipes.rs` — unchanged expected (step-3 test); verify.

## Acceptance Criteria

- [x] `cargo test -p tetond --lib the_total_cap_clears_the_harness_context_budget_with_margin` green; report the new margin in the task's completion note. — green. **Margin before 50 bytes, after 51** (floor 48), i.e. the guide got *one byte shorter* (2412 → 2411) rather than spending the 2-byte allowance. `the_web_tool_docs_clear_the_outbound_body_overhead` green too.
- [x] REQ-579/REQ-581 guide tests green unchanged. — no pinned expectation moved: `the_system_prompt_forbids_asking_for_a_credential_in_the_conversation` (whole-line prohibition, step-1 order, "shell only", "ask" uniqueness) and `the_bundled_guide_names_the_connection_test_command` (`` `/provider test <id>` `` in step 3) pass against the rewritten guide, as do the 16 `web_setup_contracts.rs` gates (recipe pairing, `;` segmentation, auth templates, keyless SearxNG, note echoes).
- [x] AC-10 test green; a mutation (re-spell `/policy show` as `teton policy show` only) makes it fail. — `the_guide_names_every_mirrored_command_in_its_session_spelling` in `crates/teton/src/cli_rows.rs` (`guide_tests`). Mutation applied locally and reverted: it fails twice over, on BR-9's unconditional `/policy show` and again on the conditional sweep (`the bundled guide names `teton policy show` and never names `/policy show``).
- [x] The guide still says the model cannot run these commands and hands them to the user — now naming `/` spellings the user can type in the session. — line 3 ("You cannot run these commands yourself; hand them to the user.") is byte-identical; steps 2 and 3 now name `/policy set-tier`, `/policy set-category`, `/policy show`, `/provider list`, `/doctor`.

## Technical Notes

- Byte math: `teton ` → `/` saves 5 bytes per mention; the mapping sentence costs ~50; trim the config-location clause or the recipe list punctuation to net ≤ +2 bytes. Measure with the margin test, not by hand.
- The guide is `include_str!`ed by tetond at build; no runtime change.

## Completion Note

**What the guide says now** (lines 6–7; 1–5 and 8 are unchanged except one trim on 5):

```
2. `/policy set-tier <tier> <provider-id>` routes a tier: `reflex` always-on duties, `scan` bulk reads, `build` edits, `think` deep reasoning. Deep reasoning means `think`. `--fallback <id>` names a backup; `/policy set-category <category> <provider-id>` overrides one category.
3. Inspect: `/policy show`, `/provider list`, `/doctor` (shell: `teton policy show`, etc.); only `/provider test <id>` dials. Config: config.toml in Teton's state dir (or $TETON_CONFIG); keys never go in it.
```

**How the bytes were paid.** Five `teton ` → `/` respellings save 25; the shell
mapping `(shell: \`teton policy show\`, etc.)` costs 35; step 1's
"Recipes, each `--model` an example the vendor still serves:" became
"Recipes, each `--model` an example still served:" (−11). Net −1.

**Two deliberate wording choices, both about test strength.**
The mapping is a *parenthetical scoped to the inspect list*, not a general
"drop the `/`" rule: `/provider setup` has no `teton provider setup` twin
(step 1's shell path is `teton provider add`), so a general rule would teach a
command that does not exist. And it names `teton policy show` rather than
`/policy show` — an example spelled with the `/` form would sit in the guide
permanently and mask exactly the mutation AC-10 exists to catch, while the
shell form makes the conditional sweep non-vacuous for a second row.

**Measurement method.** The margin test only prints its number on failure, so
it was read by padding *the guide* (20 bytes) rather than by editing
`redact.rs`: 30 printed before the rewrite → 50, 31 after → 51. No file
outside this task's scope was touched at any point.
