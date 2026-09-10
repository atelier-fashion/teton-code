---
id: LESSON-659
title: "Measure a terminal row after its last transform, as a string — and a submitted line is a cursor move the client cannot see"
component: "cli"
domain: "harness"
stack: ["rust", "cli"]
concerns: ["developer-experience", "reliability", "security"]
tags: ["activity-row", "unicode-width", "defuse", "emoji-presentation", "canonical-mode", "pty-testing", "mutation-testing", "verify-panel"]
req: REQ-621
created: 2026-09-10
updated: 2026-09-10
---

## What Happened

REQ-621's live activity row landed with a full mutation record: the frozen spinner, the
skipped withdraw, the suppressed tick arm each reddened their test. The Phase 5 verify
panel still found three defects none of those mutations could reach, and Step D's
re-verify found a fourth inside the fix for the first.

1. `activity.rs::fit` truncated the row **before** `render::defused` replaced control
   bytes with spaces. The width function charged a control byte zero columns; the
   terminal drew one. A security reviewer measured 200 × `\x01` fitted at width 80 as
   232 columns — a hard wrap, so `withdraw_row_above(1)` cleared one of three rows
   and left model-chosen bytes in scrollback (BR-5). The tool title is model-proposed,
   so this was attacker-reachable, not cosmetic.
2. The fix fitted the defused string, but its loop still **summed per-char widths**.
   `unicode-width` measures an emoji presentation sequence (base + U+FE0F) as two
   columns at the string level and one per `char`, so ~100 hearts still wrapped the
   row. Fixed in `84d93f3` by re-measuring the growing prefix as a string after every
   push.
3. The pty legs asserted that bytes typed during the animation were delivered. None
   **submitted** a line. The terminal stays in canonical mode during a turn, the kernel
   echoes the newline, the cursor drops a row, and the client cannot observe it — so
   the next `repaint_row_above(1)` overwrote the user's echoed line. Mitigated with
   `RowState::abandon` on `poll(stdin, 0)`; the residual frame is BUG-225.
4. The mutation record for the frame ignoring its tick honestly reported 0 of 28 pty
   legs reddening: the legs checked glyph membership, which a frozen spinner satisfies.
   A mutation table that names what did *not* redden is worth more than one that does not.

## Lesson

- Enforce a width budget on the exact bytes the terminal will receive: apply the final
  transform (`defused`) first, then measure, then reserve the last column.
- Never truncate with a per-char width accumulator when the string measure differs;
  grow the prefix and re-measure it as a string (grapheme clusters, variation selectors).
- A terminal row owned by a canonical-mode client has geometry that a submitted line
  silently destroys. Either detect the submission (`poll` on stdin with a zero timeout)
  and stop painting, or take raw mode. PTY legs that only *type* prove nothing about it.
- Run the verify panel even when every implementer mutation was red; the panel's value
  is exactly the classes the implementer did not think to mutate (LESSON-441, LESSON-569).
- A multi-finding fix pass is safe to parallelise when the two agents own **disjoint
  files** (source + docs vs tests + fixtures) and each stages by explicit path.

## Why It Matters

The row interpolates daemon-composed strings that originate in model-proposed tool
arguments. A width defect there is a scrollback-residue and repaint-over-user-input
defect a prompt-injected model can trigger on purpose. The defuser held on every
byte; the width accounting was the gap, and it was invisible to every test that used
ASCII titles at width 120.

## Applies When

- Any in-place terminal row, status line, or progress indicator that fits text to a
  width — especially one that renders daemon- or model-supplied strings.
- Any client that repaints above the cursor while the terminal is in canonical mode
  with echo on.
- Any mutation record: name what did not redden and why, so the next reader knows
  which claims the suite cannot make.
