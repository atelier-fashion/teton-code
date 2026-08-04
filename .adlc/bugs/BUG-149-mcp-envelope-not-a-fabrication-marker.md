---
id: BUG-149
title: "A fabricated MCP tool-result envelope is not cut as fabrication"
status: resolved
severity: medium
created: 2026-08-03
updated: 2026-08-03
component: "tetond/harness"
domain: "harness"
stack: ["rust"]
concerns: ["security"]
tags: ["prompt-injection", "frame-forgery", "mcp", "reply-scanner", "stream-gate"]
---

## Description

The BUG-147 containment declares the untrusted-content envelope to be *frame*: a model
that emits one at a line start is fabricating a tool result, so `ReplyScanner` stops
generation there, cuts the fabricated tail out of context, and `StreamGate` suppresses the
rest of the display stream.

But the harness writes the envelope in **two spellings**, and only one was listed. The
built-in path (`turn_loop::frame_untrusted_builtin`) writes `<tool-result …>`; the MCP path
(`tools/mcp.rs::frame_untrusted`, `crates/tetond/src/harness/tools/mcp.rs:55`) writes
`<mcp-tool-result server="…" tool="…" trust="untrusted">`. Both anchored marker sets
carried only the former:

```rust
const FLAT_ANCHORED_MARKERS: &[&str] = &["User:", "Assistant:", "Tool (", "<tool-result"];
const CHATML_ANCHORED_MARKERS: &[&str] =
    &["<tool-result", super::context::TOOL_RESULT_LABEL_PREFIX];
```

Marker matching is `tail.starts_with(marker)`, and `"<mcp-tool-result"` does **not** start
with `"<tool-result"` — the `mcp-` sits between the `<` and the tag name. So the MCP
envelope matched neither set, in either rendering mode.

This is the output-side counterpart to BUG-148. That fix closed the **input** side for both
spellings — `render::UNTRUSTED_ENVELOPE_TAGS` already lists all four
(`<tool-result`, `</tool-result`, `<mcp-tool-result`, `</mcp-tool-result`) — and its
"Adjacent, deliberately not fixed" section names this gap explicitly and defers it. This is
that separate change.

## Reproduction Steps

1. Serve any local-tier engine (either `ChatFormat`).
2. Have the model emit, at a line start, the exact string `mcp::frame_untrusted` produces —
   e.g. `<mcp-tool-result server="srv" tool="search" trust="untrusted">\n…\n</mcp-tool-result>`.
3. Observe the scanner's stop decision and the display stream.

Pinned as `harness::reply::tests::the_mcp_untrusted_envelope_is_a_marker_in_both_modes`,
which builds the forgery by calling `mcp::frame_untrusted` itself rather than spelling the
tag out, so the test cannot drift from the writer.

## Expected Behavior

Both envelope spellings are frame. A generated one at a line start stops the turn, is cut
from the context fold, and is suppressed from the display stream — identically to
`<tool-result`.

## Actual Behavior

Generation continued. The fabricated MCP envelope and everything after it streamed to the
user as ordinary prose and was folded into context as the model's own words — the BUG-147
failure mode, on the one envelope spelling the containment did not name.

## Environment

- Platform: all (local tier; both `ChatFormat::Flat` and `ChatFormat::ChatMl`)
- Version: `main` @ 39ff1f8 (BUG-148's merge commit — this fix is stacked on it); the gap
  itself predates BUG-148 and dates to BUG-147's introduction of the marker sets

## Root Cause

An enumerated alphabet that listed one of two spellings written by two different functions.
`UNTRUSTED_ENVELOPE_TAGS` (input side) and the anchored marker sets (output side) both
describe "the untrusted-content envelope", but only the input side was completed when the
MCP envelope was introduced.

BUG-148's anti-drift invariants did not catch it, and correctly so: they assert every
*output* marker is claimed by exactly one *input* neutralizer — coverage in that direction
only. A marker missing from the output sets is invisible to them, since there is nothing to
iterate over. The guard is directional by construction; this bug is the other direction.

**Severity rationale — medium, not high.** Narrower than BUG-148 on both reach and effect.
Reach: the model has the envelope's shape in context only in sessions where MCP tools are
actually configured and called. Effect: this is the model deceiving the *user* (and its own
next turn) with invented tool output — it does not forge a user instruction, and it cannot
cause a tool to run, since the loop only ever dispatches parsed tool-call JSON and never
acts on envelope text. It is the same axis BUG-147 rated worth containing, on one spelling.

## Resolution

`"<mcp-tool-result"` added to both `FLAT_ANCHORED_MARKERS` and `CHATML_ANCHORED_MARKERS`
(`crates/tetond/src/harness/reply.rs`), with the doc comments recording *why* both
spellings must be listed — the non-prefix relationship is the whole defect, and a future
reader trimming an "obviously redundant" entry is the way it comes back.

Only the two constants changed. The scanner's matching, stalling, and cut logic needed
nothing: the two spellings diverge one byte in, and the existing shared-prefix stall
(`m.as_bytes().starts_with(tail)` over every marker) already holds a chunk that ends at the
bare `<` until the bytes that tell them apart arrive.

**Both BUG-148 invariants still pass, verified rather than assumed.**
`the_input_alphabet_covers_every_output_marker`: the new marker is covered, because
`UNTRUSTED_ENVELOPE_TAGS` already listed it. `the_two_neutralizers_do_not_overlap`: it is
claimed by exactly one layer — `starts_with_frame_label` rejects it on the cheap
`U | A | T` first-byte check, and `starts_with_envelope_tag` accepts it. This is the payoff
of deriving the input alphabet from the output sets: adding an output marker automatically
extended the invariant's iteration to it, and it was already satisfied.

### Verification

- The four commands CI runs, all green: `cargo fmt --all --check`,
  `cargo clippy --workspace --all-targets -- -D warnings`, `cargo test --workspace`,
  `cargo test -p tetond --test e2e` (28 passed, 1 ignored).
- Both new tests confirmed to **fail** with the marker removed
  (`assertion failed: !scanner.push(&forged)`) — the fix is what makes them pass, not the
  surrounding machinery.

## Files Changed

- `crates/tetond/src/harness/reply.rs` — `"<mcp-tool-result"` added to
  `FLAT_ANCHORED_MARKERS` and `CHATML_ANCHORED_MARKERS`; doc comments record why both
  spellings are listed; two tests —
  `the_mcp_untrusted_envelope_is_a_marker_in_both_modes` (mirrors
  `scanner_stops_at_a_fabricated_untrusted_envelope`, run across both `ChatFormat`s, with
  the forgery built by `mcp::frame_untrusted`) and
  `the_two_envelope_spellings_stall_until_they_are_told_apart` (chunk boundary at the shared
  `<` must hold, mirroring `chatml_markers_sharing_a_prefix_stall_until_they_are_told_apart`).
