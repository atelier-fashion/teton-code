---
id: TASK-032
title: "Mode-aware fabrication markers for ReplyScanner and StreamGate"
status: complete
parent: REQ-554
created: 2026-08-03
updated: 2026-08-03
dependencies: ["TASK-030"]
---

## Description

Make the BUG-147 containment's fabrication-marker set follow the rendering
mode (REQ-554 BR-4, ADR-4). `ReplyScanner::for_format(ChatFormat)` and
`StreamGate::for_format(ChatFormat)` select the marker set: Flat keeps the
existing four markers; ChatMl uses `<|im_start|>`, `<|im_end|>`, and
`<tool-result`. The JSON tool-call stop logic is format-agnostic and
unchanged. All existing flat-mode tests keep passing unchanged.

## Files to Create/Modify

- `crates/tetond/src/harness/reply.rs` — replace the single `FRAME_MARKERS`
  const with per-format sets; `ReplyScanner::for_format` /
  `StreamGate::for_format` constructors (existing `new()` becomes the Flat
  alias so current call sites/tests compile unchanged); `scan_all` gains a
  format-aware variant used by TASK-033; new tests.

## Acceptance Criteria

- [x] ChatML mode: a reply emitting `<|im_start|>user` (the model fabricating
      the next turn) is cut before context and never displayed by the gate —
      the AC-4 test, covering both scanner cut and gate suppression.
- [x] ChatML mode: `<|im_end|>` at a line start ends the turn (defense in
      depth behind the engine's EOG handling).
- [x] ChatML mode: flat markers do NOT fire — a reply containing `User:` at a
      line start streams through untouched (BR-4 false-stop test).
- [x] Flat mode: behavior is byte-identical — every existing reply.rs test
      passes without modification.
- [x] `<tool-result` is a marker in BOTH modes (harness-authored envelope is
      fabricatable in any mode).
- [x] The tool-call JSON stop works identically in both modes.

## Technical Notes

- Markers are ASCII, so the existing byte-scanner boundary logic (line-start
  detection, partial-marker stall) works unchanged — only the marker slice
  changes. `<|im_start|>` shares the `<` prefix with `<tool-result`; the
  incremental partial-prefix stall already handles shared prefixes.
- Marker sets are hardcoded per family (OQ-2 resolved) — a
  `fn markers(format: ChatFormat) -> &'static [&'static str]`.
- Keep `ReplyScanner::new()` = `for_format(Flat)` so TASK-031/033 land
  independently.
