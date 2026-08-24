---
id: TASK-258
title: "The resident prompt fact: approvals are not remembered, observations are"
status: complete
parent: REQ-589
created: 2026-08-24
updated: 2026-08-24
dependencies: [TASK-246]
---

## Description

**Created mid-Phase-4.** TASK-246 discovered that BR-14.2's resident-prompt requirement
(LESSON-543) had **no owning task** — it needs `harness/self_config.md`, which no REQ-589
task claimed. Unassigned, the model would be free to tell a user it "remembers" an approval
it does not have, which is precisely the false self-account LESSON-543 exists to prevent.

## Files to Create/Modify

- `crates/tetond/src/harness/self_config.md` — the resident-facts block
- `crates/tetond/src/harness/turn_loop.rs` — the pin assertions (the line numbers this task
  file quoted were TASK-246's; the guide's pinning tests had moved to ~4436, ~4568, ~4742
  by the time this ran)

## Acceptance Criteria

- [x] The resident facts state BOTH halves — approvals are never remembered, observations
      are — in substance matching the wording TASK-246 recorded:
      > Teton never remembers your answer to a permission question — an approval is for that
      > one turn only, and the next one asks again. It does remember what it *observed*: a
      > skill a provider already refused as too large is named as such when the same skill is
      > offered again on the same route. Never say you remember an approval.
- [x] Pinned **in parts**, so a later REQ re-wording the block fails loudly rather than
      silently deleting the fact (LESSON-543)
- [x] **Headroom is measured BEFORE the sentence is added.** The resident prompt is
      floor-guarded by `MIN_PROMPT_HEADROOM_BYTES`; LESSON-543 records a case where the block
      had one byte spare and a new sentence silently truncated existing facts. Pay for this
      one by shortening another, and state in the commit what was shortened
- [x] `crates/teton/src/cli_rows.rs`'s `guide_tests` cross-read still passes
- [x] Discharges TASK-246's AC-5, which is currently unmet and unowned

## Technical Notes

The negative half ("never say you remember an approval") is as load-bearing as the positive
half — LESSON-543's rule is to name the negative space, not only the roster. A model with no
resident fact answers capability questions from whatever it can see in the user's files.

## Implementation notes

### Headroom, measured before the sentence was written

The pad method `docs/manual-verification.md` records, run against **both** prompt shapes
(1,000 bytes of filler appended to the guide, the reported figures un-padded):

| shape | prompt | escaping | vs. `REDACT_BODY_OVERHEAD_BYTES` (11,264) | margin | above the 48-byte floor |
|---|---|---|---|---|---|
| opted-out (`egress::redact`) | 7,242 | 3,276 | 10,518 | **746** | **698** |
| web tool (`harness::tools::web`) | 7,195 | 3,276 | 10,471 | 793 | 745 |

The **opted-out** shape is the binding one — it is now the larger of the two, which the
web sweep's own doc comment (written at REQ-587, when it was the larger) does not yet say.
746 bytes was room for the sentence without moving `REDACT_BODY_OVERHEAD_BYTES`, so no
assumption was raised and the floor was not touched. See the report for the residual note.

### What was shortened to pay for it

The fact costs **+298** bytes. Two unpinned lines paid **28** of it back:

- line 1, `-17`: `…you work in, so do not search the project files for it; answer setup
  questions from here.` → `…you work in; answer setup questions from here, never from
  project files.` The BUG-160 negative survives as `never from project files`, and the only
  assertion on this line — `system.contains("never inside the repository")` — is untouched.
- the step lead-in, `-11`: `…route work to external providers (Anthropic or
  OpenAI-compatible):` → `…route work to Anthropic or OpenAI-compatible providers:`. No test
  reads this line.

Net **+270**: margin 746 → **476**, i.e. **428** bytes still above the floor. Nothing
A/B-tuned was touched — the credential prohibition, the referral line, the capability
sentence, `Deep reasoning means \`think\``, the recipe table and the `[web]` line are all
byte-identical.

### How the pin resists re-wording

`the_system_prompt_states_that_an_approval_is_never_remembered` asserts **four short
semantic needles** (`never remembers`, `one turn only`, `observed`, `same route`) each with
its own failure message naming the claim it carries, plus the negative half
(`Never say you remember an approval`) asserted separately — the posture BUG-181's
`loads nothing from` established and REQ-585 amended rather than deleted. It also pins
**order** (`never remembers` before `observed`, so the rule frames the memory rather than
trailing it), position **before step 1**, residency in `build_system_prompt` for **both**
harness profiles, and uniqueness: exactly one guide line may contain `remember`, so a
second, contradicting sentence cannot arrive unnoticed — the hole the credential
prohibition's own test documents.

### The word `ask` is spoken for

The guide is allowed exactly one line containing `ask` (the credential prohibition). The
fact therefore says `the next turn prompts again` rather than `asks again`; the substance
TASK-246 recorded is unchanged, and no guard was widened to make room. A note saying so was
added to that test's failure message, so the next author who reaches for the natural word
gets an explanation instead of a puzzling credential error.
