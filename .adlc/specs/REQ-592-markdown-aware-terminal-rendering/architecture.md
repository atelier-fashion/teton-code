# REQ-592 — Architecture

## Approach

Two halves in two crates, joined by nothing in source: the CLI learns to render, the daemon
learns to say where its words land. The crate boundary keeps them independent, so they are
built in parallel (TASK-282 has no dependencies at all).

The CLI half is deliberately **not** a new front-end. `render.rs`'s module docs invite one — "a
future ratatui front-end is a new `Surface` impl and nothing else changes" — but markdown
rendering is the *same* plain-text front-end doing more, and the seam it must live behind is the
sanitizer, not the trait. So the transform goes **inside `PlainSurface`**, opt-in at
construction, with all layout logic in a new pure module. That single choice is what makes
BR-7 structural, keeps every existing test untouched, and satisfies LESSON-517.

## Key decisions

### ADR-1 — The transform lives inside `PlainSurface`, opt-in at construction

`PlainSurface` gains a `markdown: Option<MarkdownState>` field and a third constructor,
`with_markdown(out, color, width)`. `new` and `with_color` are unchanged and leave it `None`,
which means byte-for-byte today's behaviour.

**Why inside rather than wrapped.** A decorator (`MarkdownSurface<S: Surface>`) is the obvious
shape and it does not work: the decorator would author SGR, then hand the styled text to the
inner `PlainSurface::fragment`, whose `defused_multiline` replaces every `\x1b` with a space —
the styling arrives as visible `[1m` debris. That is not a bug to work around; it is
[[LESSON-517]] exactly ("when a seam gains a sanitizer it must also take over every legitimate
use of the alphabet it destroys"), and REQ-573 already shipped that debris once with the entry
chevron. Styling must be authored **after** defusing, by the sanitizer itself, from a fixed
table — which is what `LineKind::sgr()` already does for line classes.

**Why opt-in at construction, rather than an `if interactive` inside `fragment`.** Three
consequences fall out for free:

| | Consequence |
|---|---|
| BR-7 | The piped path constructs a surface with no renderer, so "inert off a terminal" is true **by construction** rather than by a conditional a later edit could invert. |
| Existing tests | Every byte test in `render.rs` builds via `PlainSurface::new(&mut buf)` / `with_color(&mut buf, color)` (lines 427–771). None opts in, so all of them stay green **untouched** — including the four that pin exact fragment bytes. |
| `RecordingSurface` | A separate impl, so `session_ui`'s semantic tests (`fragments() == "Hello, world"`, line 4222) are unaffected, and BR-9 holds without anyone defending it. |

**Rejected:** the decorator (above); and transforming in `session_ui` before `fragment()` — that
one breaks BR-9's accumulator ordering *and* every `RecordingSurface` assertion at once.

### ADR-2 — Layout is a new pure module; the surface owns bytes and state only

`crates/teton/src/markdown.rs`: `(text, width) → Vec<String>` wrapping, table measurement and
layout, inline-span parsing. The width is a **parameter**, never read inside.

This copies `status.rs` deliberately, including its enforcement: `status_line(level, effort,
width) -> Option<String>` is pure, and structural sweep tests (status.rs:445, 479) assert the
content module never names `print!`/`stdout`. **The same sweep is added for `markdown.rs`** —
BR-10 is otherwise a claim nobody checks.

`status.rs` also supplies the failure posture: **degrade, don't truncate** — too narrow yields
`None`, never a clipped row. The analogue here: at a width too small to lay a table out even
transposed, emit the raw source rows rather than clipping cells. Unreadable-but-complete beats
tidy-and-lossy.

### ADR-3 — The flush verb is defaulted, and `client.rs`'s pump is its only caller

The verb: `Surface::end_block(&mut self)`, **defaulted to a no-op** like `repaint_row_above` and
`flush`. This is the single biggest blast-radius lever in the REQ: a *required* method would
change `RecordingSurface`, the `Bare` test impl (render.rs:597), `provider_test_ui`'s harness,
and ripple through ~15 files that consume `&mut dyn Surface`. Defaulted, none of them change.

**Who calls it is the load-bearing half**, and the obvious answer is wrong. The natural-looking
site is `hand_off_after_turn`, and the real control flow at main.rs:1356 rules it out:

```rust
match conn.call(params, &mut ctx)? {        // `?` — transport failure escapes here
    Ok(res)  => { session_ui::hand_off_after_turn(...); /* closing line */ }
    Err(err) if err.code == METHOD_NOT_FOUND => { /* notice */ break; }
    Err(err) => { render_turn_failure(&err, ctx.surface); }   // ← no hand-off
}
```

Only the `Ok` arm reaches `hand_off_after_turn`. A flush hung there drops buffered text on
**every failed turn**, and misses the transport `?` entirely. Fragments also render with no turn
in flight at all, via `drain_events` (main.rs:787) when a second client drives the same session.

**Decision: every `end_block()` call site lives in `client.rs`'s event pump** — the one place
that owns both the surface and every path an event takes:

1. at the end of `Connection::call`, on every return including the error paths;
2. at the end of `Connection::drain_events` (the idle path);
~~3. immediately before `resolve_permission` hands control to the `Prompter` (see ADR-4).~~
   **Withdrawn during implementation (2026-08-26) — there are two call sites, not three.** See the
   amendment in ADR-4. A reader who re-adds a third site from this ADR is re-adding a known bug;
   the ownership sweep in `client.rs` fails with the reasoning attached.

**Ownership is a pointer, not a description** ([[LESSON-547]]): **TASK-280 owns every
`end_block()` call site.** `main.rs` must not add one, `hand_off_after_turn` must not call it,
and `render.rs` must not self-flush on any timer or heuristic. A task file that says "the
turn loop flushes" and a module that says "the surface flushes" are indistinguishable at review
time from two documents that agree.

### ADR-4 — The `Prompter` bypasses the `Surface`, and buffered text must not be overtaken

`prompt.rs` writes questions and the entry frame straight to stdout, calling `render::defused`
itself — that is why `defused` is `pub(crate)` (REQ-573: one sanitizer, two writers). It never
goes through a `Surface`, so it cannot know a buffer is pending.

A permission question raised mid-turn from the pump (client.rs:425–500) would therefore paint
**above** assistant text that is still buffered — the screen reordered, with nothing failing on a
pipe because `cli_e2e` never sees a prompt. This hazard is new with this REQ and is not in the
spec's BR-8, which names only `line()` and `repaint_row_above()`.

**Amended during implementation (2026-08-26). The prescribed fix was wrong and is withdrawn.**

This ADR originally said: "the pump calls `end_block()` before `resolve_permission`". Implementing
it surfaced two facts that between them kill the idea:

**It buys nothing.** `resolve_permission` calls `surface.line(...)` before it ever reaches
`prompter.ask`, on every path including both auto-decision paths and the over-budget offer — and
`line()` already emits the pending buffer. The ordering property this ADR is about was *already*
guaranteed. Removing the added call changes no bytes on any reachable path.

**And it costs something real.** `end_block()` clears the fence bit, because its whole meaning is
"a block ended". Calling it at a mid-turn *pause* means a model that opens a ```` ``` ```` fence,
hits a tool call, and resumes inside that fence has its remaining lines reclassified as markdown.
The damage is **word-wrapping** — a long code line re-flowed across rows mid-token. (An earlier
draft of this amendment said the damage was inline emphasis being applied to `*` in code. That was
wrong: unpaired asterisks survive classification. The mechanism is re-flow, and it was found by a
test that passed under mutation until its fixture was rebuilt around a line three times the width.)

So the hazard is real, and the mechanism that already handles it is `resolve_permission`'s own
`line()`. What guards it is a **property test** — a shared byte sink where the `Prompter` and the
`Surface` both write, asserting screen order — which is deliberately independent of *which* call
provides the flush. That is what makes it survive this correction: a future refactor that drops
the `line()`, or a new prompt path that asks before rendering, fails there rather than silently
reordering the screen.

`end_block()` is a turn boundary and only a turn boundary. Mid-turn writers get what they need
from `line()` and `repaint_row_above()`, which emit the buffer before claiming their row.

### ADR-5 — `unicode-width` is taken; this reverses my own earlier lean, and here is why

**Decides OQ-1.** The CLI's manifest treats its thin dependency set as a property, and the
codebase has twice declined a Unicode-tables crate — `render.rs:181` ("pulling a Unicode-tables
crate into the CLI … would cost more than the gap is worth") and `session_root.rs:84`. Reading
those, the exploration pass and my own first lean both landed on hand-rolling a wide-range table
in the same style. That is the wrong read of the precedent.

**What those two declined was a *category* table for format characters** — a cosmetic gap, in
`is_display_steering`'s own words "none of which reorder or break a row". Display width is a
different property with a different failure mode: a CJK character measured as 1 column but
displayed as 2 makes a wrapped row **exceed** the terminal width, the terminal hard-wraps it
mid-word, and the user is back to the exact defect this REQ exists to fix. A wrong width does not
blemish the feature; it disables it for non-Latin content.

For a property that breaks the feature when wrong, the maintained table wins over a hand-rolled
one. `unicode-width` has no transitive dependencies. `chars().count()` is rejected for the reason
above.

**Recorded honestly:** neither choice handles grapheme clusters, so ZWJ emoji sequences will
still measure wrong; correcting that needs `unicode-segmentation` as well and is out of scope.
See Assumptions.

### ADR-6 — BR-1's clause is measured against **two** budgets, not one

The spec's BR-2 names `REDACT_BODY_OVERHEAD_BYTES`. Exploration found a second, independent
ceiling the spec missed: `budget.rs:4016`'s `min_budget_bytes_holds_the_harnesss_own_system_prompt`
asserts `MIN_BUDGET_BYTES >= system.len() * 2`, i.e. the **default-config** prompt must stay
within 8,192 bytes. Measured on this branch:

| Budget | Shape measured | Ceiling | Current | Slack |
|---|---|---|---|---|
| `MIN_BUDGET_BYTES` (budget.rs:4016) | default config, builtin registry | 8,192 | **6,411** | **1,781** |
| `REDACT_BODY_OVERHEAD_BYTES` (redact.rs:2276) | worst case + skill roster at `ROSTER_MAX_BYTES` | 11 KiB | — | **710** |

The redact budget binds first, so BR-2 named the right constant — but only one of two, and a
larger clause would flip which binds. Candidate clause wordings measure 184–322 bytes, so
**neither constant is expected to move**; BR-2's "if the constant moved" branch is likely
unexercised, which is worth saying out loud rather than discovering in review.

A third constraint on the *wording*, not the size: `harness/render.rs:825`
(`a_harness_authored_system_prompt_is_byte_identical`) requires `neutralize_frame_labels` to be a
no-op on the prompt, so the clause must contain **no flush-left `User:` or `Assistant:` label**.

### ADR-7 — The clause is a const beside the web clauses, never a line in `self_config.md`

BR-1's words live in a named `const` with a doc comment recording why each sentence exists, and
`build_system_prompt` decides only its *place* — the discipline `WEB_OFF_AVAILABLE_CLAUSE` /
`effective_web_clause` already follow. It does **not** go into `SELF_CONFIG_GUIDE`: those lines
are pinned by whole-line and per-segment assertions tuned by REQ-579's live A/B, and the spec's
Out of Scope forbids re-opening them.

AC-1's test follows `the_system_prompt_states_what_the_session_can_run_and_from_where`
(turn_loop.rs:5015): filter for the clause's anchor phrase, assert the count is exactly **1**,
with a message saying a second sentence about output format is a decision rather than an
accident.

### ADR-8 — Pty assertions on assistant text will move, and that is a real cost

A terminal's hard wrap is a *display* artifact — the pty master receives whatever bytes the CLI
wrote, so today a long assistant line reaches the transcript contiguous. BR-3 inserts **real
`\n` bytes** into that stream. Any pty assertion matching a contiguous assistant-text substring
longer than the pty width begins failing.

Bounded, not open-ended: `cli_e2e` is piped and inert by ADR-1; existing pty tests that assert on
`line()`-kind output are untouched by OQ-5's "`line()` stays unwrapped". The exposure is pty tests
whose *assistant* text exceeds their `cols`. TASK-283 owns re-verifying each and widening `cols`
or splitting the marker — the same remedy pty_e2e.rs:894 already uses, with its comment updated to
say the wrap is now the CLI's and not the terminal's.

### ADR-9 — Remaining open questions, decided

- **OQ-3 (conditional clause):** unconditional. A protocol hint to tell the daemon its client is a
  terminal buys back ~250 bytes of a budget with 710 spare, and the CLI is the only client that
  ships. Revisit when a second client exists.
- **OQ-4 (`SIGWINCH`):** no handler. `terminal_width()` is read per flushed block, so a resize
  takes effect on the next block and already-printed rows keep their breaks. There is no SIGINT
  handler in the CLI today either; adding signal handling for cosmetics is not proportionate.
- **OQ-5 (`line()` kinds):** unwrapped, as the spec leaned. Wrapping them would move bytes several
  `cli_e2e` and `pty_e2e` fixtures pin, for a class of text that is short by construction. Filed as
  a follow-up if it still reads badly after this ships.

## What this REQ must preserve

- **BR-9's ordering.** `session_ui.rs:2425` pushes the raw chunk to `state.turn_reply`, then 2427
  hands it to the surface. ADR-1 keeps the transform downstream of that push, so the REQ-579/581/582
  hand-off predicates keep matching the model's own words. Nothing may render before the accumulator.
- **`at_line_start` bookkeeping** (render.rs:113–118, 287–296, 316–320) and the rule that a `line()`
  mid-stream closes an open fragment. Tool and diff lines interleave with fragments mid-turn.
- **Piped bytes.** `cli_e2e.rs:5512` asserts an exact occurrence count; ADR-1 makes this structural.
- **Loading-indicator geometry.** `STATUS_ROWS_ABOVE_CURSOR = 2` (main.rs:746) is a fixed offset from
  the entry frame, *not* a count of emitted output rows — verified, so buffering does not disturb it.
  Recorded because it looks like a hazard and is not; do not re-raise it.
- **The staleness guard.** `tests/common/mod.rs:46` refuses to run when `teton-code` is older than any
  daemon source. TASK-282 edits `tetond`, so every task after it must `cargo build --workspace` before
  `cargo test -p teton` (BUG-164, [[LESSON-510]]).

## Proposed addition to `.adlc/context/architecture.md`

One paragraph under the `Surface`/`Prompter` seam entry (line ~643): the surface renders markdown
for terminals only, opt-in at construction; layout is pure and width-parameterised in
`markdown.rs`; styling is authored inside the sanitizer from a fixed table, never by callers; and
`client.rs`'s pump is the sole owner of the block-flush verb.
