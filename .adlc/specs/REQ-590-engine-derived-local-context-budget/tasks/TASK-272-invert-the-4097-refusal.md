---
id: TASK-272
title: "The 4,097-word refusal becomes a 4,097-word success"
status: complete
parent: REQ-590
created: 2026-08-25
updated: 2026-08-25
dependencies: [TASK-270]
---

## Description

ADR-7. `turn_loop.rs:3365-3367` asserts that a 4,097-word local turn is **refused** against a
4,096-word budget. That is the exact field report that motivated REQ-589 and this REQ, currently
pinned as passing behaviour.

AC-12 requires that turn to serve. So this test's premise is deleted, not renumbered — and a
refusal case at the **new** boundary is added, so the refusal path keeps a witness.

Then sweep every other test pinning the old pair.

## Files to Create/Modify

- `crates/tetond/src/harness/turn_loop.rs` — invert the `4_097` case (line ~3365); update the
  `5_000 / 4_096` case (line ~2770) to the new boundary
- `crates/tetond/src/router.rs` — `:2566`, `:2575` assert `(4_096, 32_768)`; `:2678-2679` pin the
  whole `RouteBudget` Debug rendering including `budget_tokens: 4096, bound: LocalEngine`
- any further sites the sweep finds

## Acceptance Criteria

- [ ] AC-12: 4,097 words on the local tier is **not** refused and raises **no** over-budget offer.
      Asserted on both halves — a turn that serves but silently offers would pass a weaker test
- [ ] A refusal case exists at the new boundary (just over 10,240 words), so
      `ContextRefusalOrigin::LocalEngine` keeps a witness. Paired with a just-under case on the
      same fixture
- [ ] `router.rs:2678-2679`'s Debug pin updated — including `digest_threshold_*`, which move to
      3,750 / 11,250 as a consequence of the budget moving (D-3, accepted)
- [ ] AC-13: full suite green, `cargo audit` clean, no new clippy warnings, `cargo fmt --check`
      clean. **This task owns AC-13** because it is the one that lands the test sweep — the last
      point at which "green" means anything about the change as a whole
- [ ] A grep for the literals `4_096` / `4096` / `32_768` / `32768` in tests returns only sites
      that are genuinely about the **constants** (which do not move), not about the local route

## Technical Notes

**Do not mechanically renumber.** An exploration pass listed line 3367 as a `4,096 → 10,240`
substitution; that keeps the refusal and leaves AC-12 unwitnessed while the suite goes green.
Read each site and ask which fact it is pinning: the *constants* (unchanged) or the *local
route's derived pair* (changed).

`assert_eq!(LOCAL_BUDGET_BYTES, 32_768)` at `budget.rs:3202` stays true and must not be touched.

The digest thresholds moving is expected and accepted (D-3). Note the asymmetry so it is not
mistaken for a bug: words go **up** 1,500 → 3,750 while bytes go **down** 12,000 → 11,250,
because `digest_thresholds` scales each half by its own constant and the byte half of the budget
fell.

## Implementation notes

**The fixture at `turn_loop.rs:3271` could not be made to measure a budget, and AC-12's witness
moved.** TASK-270 flagged that line 3365 "did not redden and cannot"; verified. The
`ScriptedTurn::WindowRefusal` arm took `_config: &HarnessConfig` and discarded it, so `4_097 /
4_096` were invented by the fixture and asserted back by the test — LESSON-552's shape exactly.
Nothing in `run_session_turn_with_pressure_policy` refuses on a budget at all: the loop's answer
to an oversized context is *truncation*, so no turn driven through that fixture can witness "a
4,097-word local turn serves". What the arm now does instead is read the config it was handed
(`config.context_budget_tokens + 1` / `context_budget_tokens`), so the assertion says something
real — the loop passed the route's budget down and the typed refusal came back carrying it
unaltered — and the false "the pair the reported `/analyze` failure measured" doc claim is gone.

**AC-12 is witnessed in `tests/skill_over_budget_offer.rs`**, on the path the field report
actually died on: a real prompt turn, a real router derivation, Stage A measuring a real body
against the real derived pair. `the_reported_analyze_measurement_serves_and_the_byte_half_is_the_boundary_now`
holds the word count at the reported **4,097** across two legs and moves only the bytes, one byte
either side of `budget_bytes`:

| measured | outcome |
|---|---|
| 4,097 words / `budget_bytes + 1` | offered; declined → `-32023`; accepted → dispatched |
| 4,097 words / `budget_bytes`     | **served** — no offer, no `SkillOverBudget*` event |

Mutation-confirmed: changing the serving leg's target to `budget_bytes + 1` reddens it (the
offer count goes 3 → 4). Both halves of AC-12 are asserted, per the task's own warning that a
turn which serves but silently offers would pass a weaker test.

**The task's second criterion — a refusal at "just over 10,240 words" — was not written as
stated.** The word half is reachable only below 3 bytes per whitespace word (Stage A's overhead
on this fixture is 970 words / 7,754 bytes, so 10,241 measured words needs ≥26,295 measured
bytes, which fits). No realistic content is that sparse. The boundary that a body actually meets
on the local route is the **byte** half, so that is where the paired just-under / just-over case
sits — the mirror image of the report, which was one word over with bytes to spare.

**`5_000 / 4_096` at `turn_loop.rs:2750-2819` was deliberately left alone.** Those two tests pin
the *wording* of `window_refusal_sentence` and `Display`; the numbers are inputs to a formatter,
not a budget, and renumbering them would say nothing more while inviting the next reader to
mistake a rendering test for a budget one. A note now says so in place.

## Findings for the REQ

1. **REQ-590 does not make the reported `/analyze` body serve.** The report measured 4,097 words
   / ~31 KB. The word half now holds it; the **byte half fell** 32,768 → 30,720, and the 31,000 B
   figure REQ-589 reproduced is 280 B *over* it. Pinned as an assertion in the AC-12 test so it
   cannot be rediscovered by surprise.
2. **For content denser than 7.5 B per whitespace word, this REQ lowers the local budget.** Old
   capacity was `min(4,096 words, 32,768/d)`; new is `min(10,240, 30,720/d)`. They cross at
   d = 7.5. Below it the raise is real (prose at 6 B/word: 4,096 → 5,120 words, +25%). Above it
   both are byte-bound and the new pair is a flat **6.25% smaller**. Code averages 7–8 B/word, and
   the reported body's own density is only known to a range — `31 KB` rendered, so
   [30,500, 31,499] B over 4,097 words, i.e. **7.44–7.69 B/word**, which straddles the crossover.
   The word half's 2.5× raise is inert for anything real.
3. **The offer sentence quotes two identical byte figures at the boundary.** `bytes_figure`
   rounds to the nearest KB, so a measurement one byte over a 30,720 B budget renders as
   "about 4,097 words / 31 KB, and the budget is 10,240 words / 31 KB" — the user cannot see
   which currency refused them. Pinned, not fixed.
4. **Two fixtures were resized past discovery's 64 KiB `SKILL_MAX_BYTES` ceiling** on the first
   attempt, and every test using them failed with "no skill you can dispatch" rather than with
   anything about a budget. The word budget rose 2.5× while the file ceiling did not, so the room
   between them is now narrow: 10,240 words above ~5.1 B/word does not fit in 64 KiB at all. Both
   fixtures now assert the ceiling explicitly.
