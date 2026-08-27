---
id: ASSUME-023
title: "unicode-width measures correctly for the content Teton actually renders"
status: validated
req: REQ-592
created: 2026-08-26
resolved: 2026-08-26
---

## Assumption

That `unicode-width` 0.2's per-character display width is correct for the text a Teton reply
actually contains — English technical prose, code identifiers, CJK, punctuation and the occasional
emoji — and therefore that a row measured as fitting the terminal will fit it.

ADR-5 rests on this. It took the crate over the two in-repo precedents that declined a
Unicode-tables crate (`render.rs`'s `is_display_steering`, `session_root.rs`'s bounded fields), on
the argument that those declined a *format-character category* table — a cosmetic gap — whereas a
wrong display width makes a wrapped row exceed the terminal, the terminal hard-wraps it mid-word,
and REQ-592's entire defect returns for that content.

## Context

The whole REQ depends on it. If measurement is wrong, wrapping is wrong, table columns misalign,
and BR-4's "no emitted row exceeds the width" is unenforceable. It also justified the CLI's first
dependency beyond `clap`/`serde_json`/`anyhow`/`libc`, in a crate whose manifest treats its thin
dependency set as a property.

## Resolution

**Validated for the content in scope, with two exclusions now recorded rather than assumed.**

Evidence gathered during implementation and verify:

- CJK measures correctly — the aligned-table path lines up against real wide characters, pinned by
  a test that a `chars().count()` implementation fails.
- A 400,000-input fuzz (security re-verify) and a 200,000-input fuzz (correctness re-verify) over
  an alphabet of CJK, kinsoku marks, fullwidth forms, combining marks, emoji and escape remains:
  no panics, every range on a char boundary, no character lost or duplicated.
- A differential run of the pre-CJK greedy wrap against the new one over an ASCII/accent/emoji
  corpus × widths 0..40 produced **0 diffs** — Latin behaviour is unchanged by construction.

**Two exclusions, both real, both recorded in the CHANGELOG's `Known` section:**

1. **Grapheme clusters measure as the sum of their parts.** A ZWJ emoji sequence counts as its
   components and can push a row past the edge. Fixing it needs `unicode-segmentation` as well, and
   was left out of scope deliberately. `unicode-width` is not wrong here — it answers a
   per-character question correctly, and the cluster question is a different one.
2. **`display_width` is not monotonic along a prefix.** `display_width("⌚")` is 2 but
   `display_width("⌚\u{FE0E}")` is 1 — a text-presentation selector *shrinks* the measured string.
   Found by the correctness re-verify, because two comments had been justified on the assumption
   that widths only grow. Harmless in effect (an early exit can only under-fill a row), but the
   reasoning had to be restated on what actually holds: the greedy extension only ever crosses
   whitespace, and whitespace joins nothing.

The second one is the reason this assumption is worth a file. "The measurement is correct" was
true; **"and therefore widths grow monotonically" was an unstated inference from it**, and it was
false, and it was load-bearing in two places before anyone checked.
