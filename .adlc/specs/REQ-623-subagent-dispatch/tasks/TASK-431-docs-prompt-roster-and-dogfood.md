---
id: TASK-431
title: "Docs, the REQ-617 roster sentence, architecture-context additions, and the AC-18 dogfood runbook"
status: complete
parent: REQ-623
created: 2026-10-05
updated: 2026-10-07
dependencies: ["TASK-428"]
repo: teton-code
---

## Description

The surfaces that tell people and the model the tool exists. The REQ-617 command roster
gains the `agent` sentence (written for the product after this REQ lands, LESSON-570,
naming it as the dispatch tool the skills call "Agent"); README and CHANGELOG describe
dispatch, bounds, and `[agent]`; `.adlc/context/architecture.md` gains the three additions
the architecture doc proposes; `docs/manual-verification.md` gains the AC-18 runbook (run
`/analyze` and `/proceed` Phase 4 from a Teton session, record where `/proceed` next stalls
as evidence for the companion-files spec). REQ-587's AC-15 note is amended to point here.

## Files to Create/Modify

- `crates/tetond/src/harness/self_config.md` — the resident prompt (`SELF_CONFIG_GUIDE`): the `agent` sentence beside the `skill` one on line 4; the `/help`-era wording that the model runs nothing but skills is swept (LESSON-570)
- `crates/tetond/src/harness/docs/commands.md` — `teton_docs commands`/tools entry for `agent`: schema, bounds, statuses, `[agent]` keys
- `README.md` — "In the session": dispatching children, what they can and cannot do, the config keys
- `CHANGELOG.md` — entry
- `.adlc/context/architecture.md` — child turns, share pool, fixture rules
- `docs/manual-verification.md` — REQ-623 AC-18 runbook, marked OUTSTANDING until run
- `.adlc/specs/REQ-587-model-invoked-skills/requirement.md` — dated note on AC-15 pointing to REQ-623

## Acceptance Criteria

- [x] `SELF_CONFIG_GUIDE` names `agent` beside `skill`; the REQ-617 guard `the_resident_prompt_names_every_command_family_the_roster_carries` still passes (the sentence is not a `/command`, so it must not join the built-in commands clause)
- [x] README/CHANGELOG strings present (grep)
- [x] architecture.md sections present
- [x] AC-18 runbook present; its result recorded in REQ-623's Validation section when run (runbook written and marked OUTSTANDING — the run itself is the wrapup's to record)

## Verification

| rule | kind | artifact | benign_path |
|------|------|----------|-------------|
| BR-14 | test-case | `crates/tetond/src/harness/turn_loop.rs::tests::the_resident_prompt_names_the_agent_tool_beside_skill` | yes |
| AC-18 | structural-check | `docs/manual-verification.md`, `README.md`, `.adlc/context/architecture.md`: strings present (grep) | no |

## Technical Notes

- LESSON-570: sweep the whole prompt for any sentence that still says the model cannot dispatch work.
- AC-18 itself is a dogfood run and cannot be a `test-case`; the structural check pins the runbook's existence, the Validation section records the run.
