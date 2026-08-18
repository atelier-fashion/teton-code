---
id: TASK-172
title: "The guide says the session spelling first, under its 2-byte budget; guide/table cross-check test"
status: draft
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

- [ ] `cargo test -p tetond --lib the_total_cap_clears_the_harness_context_budget_with_margin` green; report the new margin in the task's completion note.
- [ ] REQ-579/REQ-581 guide tests green unchanged.
- [ ] AC-10 test green; a mutation (re-spell `/policy show` as `teton policy show` only) makes it fail.
- [ ] The guide still says the model cannot run these commands and hands them to the user — now naming `/` spellings the user can type in the session.

## Technical Notes

- Byte math: `teton ` → `/` saves 5 bytes per mention; the mapping sentence costs ~50; trim the config-location clause or the recipe list punctuation to net ≤ +2 bytes. Measure with the margin test, not by hand.
- The guide is `include_str!`ed by tetond at build; no runtime change.
