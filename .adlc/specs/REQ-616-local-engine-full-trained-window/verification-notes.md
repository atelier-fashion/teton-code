# REQ-616 — Verification notes

## AC-12: the recall trial (manual dogfood, **not run**)

**Status: NOT RUN. This REQ ships without it.**

AC-12 asks for a recall trial on the shipped local model at 200,000 tokens of
repository context, with a fact planted at the 10 %, 50 % and 90 % marks,
retrieved three times out of three, at the KV type the probe chose.

It has not been performed, and nothing in the automated suite stands in for it.
Saying so plainly is the point: a reader who finds a green CI run and an
acceptance criterion about recall could otherwise reasonably conclude the
criterion was met.

### Why it could not run here

The real engine is behind the non-default `llama` feature. CI never compiles
llama.cpp (`crates/teton-inference/Cargo.toml` states this explicitly, and the
whole ADR-616-3 split exists because of it), and the trial additionally needs the
17.3 GiB of weights on disk and a machine that can hold a 262,144-token context.
None of that is reachable from a test run.

### What *is* asserted, and what it does not cover

| claim | covered by | covers AC-12? |
|---|---|---|
| the probe picks `q8_0` at 262,144 on a 48 GiB machine | `window::tests::fit_window_table_at_four_ram_figures` | no — this is the *choice*, not its effect |
| the chosen type is recorded and reported | `selection_store::window_record_tests`, `model_ui::window_clause_tests` | no |
| `q8_0` KV does not degrade recall | **nothing** | — |

The assumption under test — that `q8_0` KV has no measurable effect on recall or
tool-call parsing for this model — is stated in the REQ's Assumptions and remains
**unverified**. It is the one claim in REQ-616 that rests on judgement rather
than on arithmetic or a test.

### How to run it, when the weights are present

1. Build with the real engine: `cargo build --features tetond/llama` (needs
   cmake).
2. Confirm the load: `teton model status` should print
   `context:   262,144 tokens (KV q8_0)` on a 48 GiB machine.
3. Assemble ~200,000 tokens of this repository as a single prompt, planting a
   distinctive fact at the 10 %, 50 % and 90 % marks.
4. Ask for each fact in turn, three runs.
5. Record the outcome — all three marks, three of three — in this file, with the
   KV type `local_window_decided` reported.

A failure here is a product finding, not a bug in this REQ: it would mean the
probe should prefer a smaller `f16` window over a larger `q8_0` one, which is a
change to `fit_window`'s ladder and a decision for the product owner.

## What the automated suite does verify

Every other acceptance criterion is covered by a test that has been shown to
fail. The mutations are recorded in the tests' own doc comments; the ones worth
naming here are the three that would otherwise have shipped silently:

- **AC-4's original claim was false.** "The byte half is never the binding half
  for prose or code" cannot be true at any window: the halves meet at exactly
  3 bytes per whitespace-word, and the corpus measures prose at 5.56 and code at
  6.80. `crossover_is_three_bytes_per_word_at_every_window` now asserts what is
  true, including that the binding half does not *change* with the window —
  which is the assertion that would have caught REQ-590's regression.
- **BR-8's "same fraction" is false at 262,144.** Both digest ceilings bind
  (words 63,750 against 20,000; bytes 191,250 against 163,840), so the budget
  grew 8.2× and the digest thresholds did not move at all. Recorded as OQ-3
  rather than silently changed, because the ceilings are a deliberate
  window-independent product judgement.
- **`bound_clause` would have lied.** It quoted the compile-time constant, so a
  machine serving 262,144 tokens would have been told its window was 32,768.
  Caught by writing the test for BR-6, not by review.
