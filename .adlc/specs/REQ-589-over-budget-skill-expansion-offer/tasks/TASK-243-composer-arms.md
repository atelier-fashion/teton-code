---
id: TASK-243
title: "Offer, decline, and accepted sentences — arms on the one composer"
status: complete
parent: REQ-589
created: 2026-08-24
updated: 2026-08-24
dependencies: [TASK-242]
---

## Description

BR-5. `skill_refusal` (971) is the single composer and stays that way. Add arms; do not fork. The accepted path must NOT emit the refusal's "no provider saw this turn" clause, which becomes false the moment a user proceeds.

## Files to Create/Modify

- `crates/tetond/src/harness/budget.rs` — new arms in the `skill_refusal` module; remedy-sentence renderer beside `bound_clause` (1024)

## Acceptance Criteria

- [x] The offer, the decline refusal, and the accepted record are distinct sentences from one module (BR-5)
- [x] The accepted path never emits "no provider saw this turn" (AC-11), asserted negatively
- [x] On `Window`/`UserCap` + `FitsWindow` the sentence says the prompt fits the declared
  window **but may leave little room for the reply** — never an unqualified "expected to
  serve" (ADR-15: that band IS the generation reservation). `LocalEngine`/`DefaultUnknown`
  are unaffected
- [x] `ExceedsWindow` states the window will be blown and that proceeding will very likely be rejected; `WindowUnknown` states the daemon cannot promise; `FitsWindow` claims neither (AC-6) — each arm pins its own wording
- [x] A `RaiseWindow` offer cannot render without BR-7a's risk sentence (AC-7a)
- [x] A `LocalEngine` offer names both halves and the cost consequence, and never offers a max_context write for the local tier (AC-8)
- [x] Option labels name the concrete write (`capabilities.max_context = 1000000` for `kimi`), never "raise the limit" (ADR-1)
- [x] No provider response body can reach any sentence — extend the `a_skill_refusal_carries_no_provider_response_body` pattern to the new arms

## Technical Notes

`the_refusal_names_the_skill_its_size_the_budget_and_the_bound` (budget.rs:2110) is the assertion shape: compute the expected size independently via `approx_tokens` + `SEED_OVERHEAD_BYTES` rather than re-calling the estimator.

## Implementation notes

`skill_refusal` stayed the one composer and grew a seventh parameter, a private
`SkillSentence<'_>` with three arms. Everything ahead of the tail — subject,
stage clause, both figure pairs, the spoken bound — is composed once for all
three, so an offer and its own decline cannot quote different numbers for one
measurement; `the_three_sentences_share_one_head_and_differ_only_in_the_tail`
asserts the two new sentences literally `starts_with` the refusal's head.

`OverBudgetOffer` gained four methods and no fields: `question(source, prior)`,
`decline_refusal()`, `accepted_record(source)` and `option_labels()`. All four
route through one private `compose`, whose arguments are the offer's own fields
— so no sentence can re-measure or re-derive.

**BR-7a made structural (AC-7a).** `RemedyClause { write, consequence }` has
private fields and no accessor; `RemedyClause::render` is the only way out and
concatenates the two halves unconditionally. `consequence` is a
`RemedyConsequence` — a closed four-variant enum with **no variant meaning
"nothing"** — so a remedy clause cannot be constructed without one. Both option
labels and the offer sentence go through `render`, so a risk stated in one and
missing from the other is not a state this module can represent. What remains
testable rather than structural is the *pointing*: that the `RaiseWindow` arm
names `RaisingADeclaredWindow`, which the test pins directly.

**ADR-15.** The `FitsWindow` clause splits on the bound, not only the verdict:
`Window`/`UserCap` get the reservation sentence ("the room held back for the
reply"), and `RedactScan` — the third reachable `FitsWindow` cell — gets a
plainer one, because there the band is a byte clamp and the reservation claim
would be false. "Expected to serve" appears nowhere and is asserted absent.

**Two `SkillSource` decisions.** The project marker is the daemon's own prose
and stops at the daemon: `PermissionSubject::SkillOverBudget` still carries
`source` as a *structure* for the client to render (LESSON-529). And the
`Refused` arm has no source by construction, which is what keeps a declined
offer byte-identical to today's refusal (AC-3, asserted as string equality
against `skill_fit`'s own message).

`PriorWindowRejection` was added here rather than left to the wiring task so
BR-14.2's lead comes out of the one composer. Stripping the lead leaves the
blind question byte for byte, which is AC-23's "must not pre-answer it" in its
strongest form.

**Contradiction found, not fixed (outside ownership).** `WindowVerdict::FitsWindow`'s
doc in `crates/teton-protocol/src/events.rs` still says "the send is expected to
serve, and the offer says so", and `claimed_provider_tokens` in this file repeats
it. ADR-15 supersedes both. The code is correct; the two doc comments are stale.
