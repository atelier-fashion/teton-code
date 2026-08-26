---
id: TASK-273
title: "The word half's zero slack is asserted, not accidental"
status: complete
parent: REQ-590
created: 2026-08-25
updated: 2026-08-25
dependencies: [TASK-270]
---

## Description

ADR-6, BR-10. The derivation sets `words × 3/2 = usable` **exactly**: 10,240 × 3/2 = 15,360 =
16,384 − 1,024. Today the local word half carries 2.5× headroom; after TASK-270 it carries none.

LESSON-496 is about this shape: *a policy expressed as an ordering against a cap silently becomes
"never" the moment `limit == count`, and nothing in the code says so — the two numbers live in
different places, were chosen for different reasons, and their coincidence is invisible at both
definition sites.* Its habit is to assert the headroom.

D-3 accepts the zero gap, so the assertion is **inverted rather than dropped**: pin that the gap
is zero, deliberately, with the reasoning at the assertion.

Then measure the quadrant the byte guard does not cover.

## Files to Create/Modify

- `crates/tetond/src/harness/budget.rs` — the zero-gap assertion and its rationale
- `crates/tetond/tests/token_corpus.rs` — a token-dense, byte-light sample
- `crates/tetond/tests/fixtures/token_corpus/` — the new sample and its pre-measured count

## Acceptance Criteria

- [x] A test asserts `local_words × REMOTE_TOKENS_PER_WORD_NUM / REMOTE_TOKENS_PER_WORD_DEN`
      equals `LOCAL_ENGINE_N_CTX − LOCAL_GENERATION_RESERVATION` exactly, with a comment saying
      the zero gap is D-3's accepted consequence and what would make it non-zero
- [x] AC-9: a corpus sample that is **token-dense and byte-light** — whitespace-separated
      single-character tokens or a numeric column, ~2 B/word — at full word budget. Either it
      fits, or the test records that it does not and names `context_length_exceeded` as the
      intended outcome
- [x] The sample's token count comes from the real tokenizer (`o200k_base`), pre-measured into
      the corpus like its siblings — **not** from `approx_tokens`. An assertion in whitespace
      words passes identically at 1.2 or 2.0 tokens/word and would prove nothing
- [x] The existing corpus samples still pass

## Technical Notes

`budget.rs:205-212` measures prose at 1.21 tokens/word and **Rust at 1.69** against a 1.5 ratio.
So the ratio is already exceeded by ordinary Rust; what saves those turns is the byte guard.
The uncovered quadrant is content that is dense in *tokens* but light in *bytes*, which slips
under the byte guard and overruns the word one.

The existing corpus (`tests/fixtures/token_corpus/`) has prose, Rust, minified JSON, paths and
base64. None is clearly token-dense-and-byte-light — minified JSON and base64 are dense in both.
Expect to author a new sample rather than reuse one.

State the outcome honestly. If the sample overruns, that is not a task failure — it is AC-9's
finding, and it belongs in the architecture doc as a measured number.
