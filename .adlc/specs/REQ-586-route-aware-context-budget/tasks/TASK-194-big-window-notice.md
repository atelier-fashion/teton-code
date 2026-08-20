---
id: TASK-194
title: "Say the true thing at the surface: a notice when a big window is recorded, a kind for a context that would not fit, and an honest bound when a cap cannot be honored"
status: draft
parent: REQ-586
created: 2026-08-19
updated: 2026-08-19
dependencies: ["TASK-188", "TASK-190"]
repo: teton-code
---

## Description

**Product decision, 2026-08-19 (OQ-6 amended).** The shipped recipes declare
1M-token windows for four of six vendors, so a fresh `/provider setup` records
a window that derives to ≈666k words / 2 MB **per call**, and a `Native` route
runs up to 25 iterations — ≈25M input tokens for one prompt, with only an
unset optional `context_budget_cap` in between. OQ-6 resolved "the window the
user declared is the consent" when the working figure was 128k; the recipe
correction moved it 8×, and `/provider setup` records it **silently**.

The owner's answer: keep OQ-6's posture — the declaration is still the consent,
no default cap, no behaviour change — but **say it once, where the window is
recorded**. A user who accepts a 1M window should learn the size of the cheque
at the moment they sign it, not from `/verbose` on a later turn.

This is the *only* place this REQ raises its voice: the per-turn surface stays
`/verbose`-only (BR-9/OQ-5 unchanged), and no status line changes.

## Part 2 — two surfaces that report something untrue (from the Phase-5 fix pass)

Both were found by the fix agents and left as protocol/CLI work. They are the
same defect shape as Part 1 — a fact the code knows and the surface does not
say — so they ship together.

**2a. There is no wire kind for "the gate could not fit."** `PressureReport`
now carries `over_budget` (a context the gate could not fit into either
currency — the byte-floor arm, or the word arm where the drop loop stops at
one block). It is *counted* as non-quiet news, so it is announced — but it
rides as `ContextPressureKind::BlockElided` with `elided_bytes: 0`, and the
tell is a doc comment on `pressure_kind`. BR-7 says nothing is clamped in
silence; this is worse than silence, it is announced under the wrong name.
**Fix**: add `ContextPressureKind::DidNotFit` in `teton-protocol` (additive —
older clients already ignore unknown enum payloads through the classify path),
route `over_budget` to it in `pressure_kind`, and give it its own CLI line
(the honest wording is close to "context: could not be fitted to the N-word
budget (bound: …) — the turn was sent over budget").

**2b. A cap that cannot be honored is reported as though it were.** `derive`
floors both currencies at `MIN_BUDGET_TOKENS`/`MIN_BUDGET_BYTES`, so a
sub-floor `context_budget_cap` (say 500 on a 200k provider) yields the floor
pair — more than the user asked for — while `/verbose` and `/doctor` report
`bound: user cap` with no hint that the cap could not be met. The floor's cost
is documented at the constant; it is not *surfaced*.
**Fix**: carry the fact on the wire (either a `bound: UserCapFloored` variant
or a boolean beside the bound), render it in the `/verbose` clause and in
`/doctor`'s existing inert-cap advisory family (`cap below the smallest
budget that holds the system prompt — using N words / M KB instead`), and
say it in the `context` docs topic.

## Files to Create/Modify

- `crates/tetond/src/runtime.rs` — `derive_provider_setup`: when the candidate's
  recorded `max_context` exceeds `BIG_WINDOW_NOTICE_TOKENS`, add one line to the
  preview's existing warning list (the same list the flow already renders before
  the digest-bound commit). It names the derived per-call budget in both
  currencies, the worst case across the loop ceiling, and
  `context_budget_cap`. Compose it from `harness::budget::derive` — never a
  second arithmetic (LESSON-456) — and from `NATIVE_MAX_ITERATIONS`.
- `crates/tetond/src/harness/budget.rs` — `pub const BIG_WINDOW_NOTICE_TOKENS: u32 = 256_000;`
  with the rationale in its doc: above this a single prompt's worst case passes
  ~4M input tokens, which is the point at which a user who did not choose the
  number should be told it. One home; both surfaces read it.
- `crates/teton/src/main.rs` — `provider add --max-context <n>` above the
  threshold prints the same sentence once, through the `Surface` seam, before
  the registration is sent. Build it from the shared composer, not a second
  string.
- A shared composer for the sentence (daemon-side, e.g. beside `budget::derive`
  or in `provider_recipes.rs`) so the CLI and the setup preview cannot drift —
  the REQ-582 one-renderer rule, and the reason `bound_words` moved into
  `teton-protocol` in the verify pass.
- `crates/tetond/src/harness/docs/context.md` — one clause noting the notice
  exists and what it names (the topic already carries the worst-case figure).
- Tests: `crates/tetond/tests/provider_setup_flow.rs` (a 1M recipe setup shows
  the line; a 128k one does not); `crates/teton/tests/cli_e2e.rs` (`provider add
  --max-context 1000000` prints it once, `--max-context 128000` prints nothing);
  a unit test that both surfaces render the **same** sentence for the same
  window.

## Acceptance Criteria

- [ ] A `/provider setup` that records a window > `BIG_WINDOW_NOTICE_TOKENS`
      shows exactly one notice in the preview, naming: the derived budget in
      words **and** bytes, the worst case (budget × the route's iteration
      ceiling) for one prompt, and `capabilities.context_budget_cap` as the
      knob. A window at or below the threshold shows nothing.
- [ ] `teton provider add --max-context <n>` above the threshold prints the same
      sentence, byte-identical to the setup preview's, through the `Surface`
      seam; below it, output is unchanged from today.
- [ ] The sentence is composed once and read by both surfaces; a test asserts
      byte-equality between them, and mutating the composer changes both.
- [ ] The figures come from `budget::derive` and the profile's iteration
      ceiling — no second arithmetic anywhere; `grep` finds one composer.
- [ ] No default behaviour change: no cap is written, no route is bounded, and
      a user who accepts the window gets exactly what they declared.
- [ ] **2a**: a context the gate could not fit is announced as its own kind, not as a zero-byte elision; the CLI line says it plainly; an older client still renders something sane.
- [ ] **2b**: a sub-floor cap is visibly floored on `/verbose` and in `/doctor`'s advisory, and the `context` topic says the floor exists and why; a cap at or above the floor renders exactly as today.
- [ ] `cargo test --workspace --no-fail-fast` green; fmt and clippy clean.

## Technical Notes

- The threshold is a notice threshold, not a policy threshold — nothing about
  the budget derivation reads it. Say so in its doc so a later reader does not
  mistake it for a cap.
- Keep the sentence short enough to sit in a preview warning list; the docs
  topic carries the long form.
- Commit as `feat(daemon,cli): name the cost when a big window is recorded [TASK-194]`.
