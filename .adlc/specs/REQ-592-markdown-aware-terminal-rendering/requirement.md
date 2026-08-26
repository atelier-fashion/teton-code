---
id: REQ-592
title: "Markdown-aware terminal rendering, and a system prompt that knows where its words land"
status: approved
deployable: true
created: 2026-08-26
updated: 2026-08-26
component: "cli"
domain: "clients"
stack: ["rust", "cli", "daemon"]
concerns: ["developer-experience", "security"]
tags: ["rendering", "markdown", "word-wrap", "terminal-width", "sgr", "surface", "system-prompt", "tables", "code-fence"]
---

## Description

A dogfood session on 2026-08-26 asked Teton to audit this repository for security defects. The
model produced a good audit. What reached the terminal was close to unreadable: markdown tables
printed as raw `|`-delimited prose, `**bold**` and backticks shown as literal punctuation, and
every long line hard-broken **mid-word** by the terminal — `defens-\ne-in-depth`, `bypac-\nk`,
`sc-\nan`. A user cannot skim that, and a security finding a user does not read is a security
finding that did not ship.

That reply's widest table is checked in at
[`fixtures/audit-2026-08-26.md`](fixtures/audit-2026-08-26.md) and is the evidence this REQ is
measured against: 7 data rows, second column 155..243 characters, widest raw row 263. AC-4 reads
it from disk; AC-13 compares against it by eye.

There are **two independent causes**. Neither is a bug in the other's layer, and fixing either
alone leaves a visibly bad screen.

### Cause 1 — the CLI renders nothing

`crates/teton/src/render.rs`'s `PlainSurface::fragment` is the whole of the assistant-text path:

```rust
fn fragment(&mut self, text: &str) {
    let shown = defused_multiline(text);
    let _ = write!(self.out, "{shown}");
    self.at_line_start = shown.ends_with('\n');
}
```

Defusing, and nothing else. No wrapping, no width awareness, no notion that the bytes are
markdown. The *terminal* does the line breaking, and a terminal breaks at the column, not at the
word — which is the entire explanation for `defens-\ne-in-depth`. A markdown table's rows are
prose to this function, so a 200-column `|`-row wraps into a ribbon whose cell boundaries land
wherever the window happens to end.

The width is not missing from the crate. `crates/teton/src/prompt.rs:511`'s `terminal_width()` is
already `pub(crate)`, already `TIOCGWINSZ`, already falls back to a conservative 80 — and is
called by the status row and by nothing on the assistant-text path.

### Cause 2 — the system prompt never says where the words land

`crates/tetond/src/harness/turn_loop.rs:2489`'s `build_system_prompt` composes: the agent opener,
REQ-583's environment block, the verification clause, REQ-563/572's web-capability clause, the
bundled `SELF_CONFIG_GUIDE`, and the tool docs. There is **no clause about output format**. The
model is told what it is, where it is, and what it can call — never that its prose is about to be
printed verbatim into a plain terminal that renders no markdown at all.

So it writes what a chat surface would want: two-column tables with 180-character cells, nested
emphasis, fenced spans. Reasonable output for a renderer that does not exist here. This is
BUG-181's shape in a new place — the model answers from what it can see, and it cannot see the
terminal. BUG-181 fixed *capability* confabulation by putting facts about Teton in the prompt;
this puts the one missing fact about Teton's **surface** there.

### Why both halves are required

The renderer is the guarantee and the prompt clause is the improvement, and neither substitutes
for the other:

- **A prompt clause alone is advisory.** Models honour formatting guidance imperfectly and
  unevenly across tiers; a local 4B model will drift back to tables. Nothing about a clause makes
  a wide table render legibly on the day the model emits one anyway.
- **A renderer alone cannot rescue every shape.** BR-3's transposition makes a 6-column table
  *readable*, not *good*. Content authored for 80–120 columns beats content rewritten into it.

## System Model

No new persisted entity, no wire event, and no permission surface — the whole change is a
rendering transform plus one prompt clause. What follows is therefore not a data model but the
**recognized-construct table**: the closed set of markdown this REQ renders, and what each one is
required to look like on screen. How the renderer represents these internally is architecture's
call, not this document's.

### Recognized constructs

Anything not in this table renders as literal text — the honest fallback, since the bytes really
did contain those characters. Extending the set is a change to this table.

| Construct | Recognized as | Required rendering |
|---|---|---|
| Paragraph | any run of non-blank lines that is none of the below | wrapped at the terminal width, broken only at word boundaries (BR-3) |
| ATX heading | leading `#`..`######` plus a space | the `#` markers are not printed; the text is emphasized (BR-5) |
| Table row | a line with a leading and trailing `\|` | buffered with its consecutive neighbours and laid out as a block (BR-4) |
| Table separator | a table row whose cells are only `-`, `:` and spaces | consumed as structure; drawn as a rule, never printed literally |
| List item | leading `-`, `*`, `+`, or `<digits>.` plus a space | marker preserved; continuation rows indented under the item's text (BR-3) |
| Block quote | leading `>` | quoted text wrapped, with the quote marker preserved on each emitted row |
| Fenced code | a line that is exactly ```` ``` ```` or ```` ``` ````+language | fence markers not printed; content verbatim, never wrapped, never styled (BR-6) |
| Strong | `**text**` | terminal bold, authored by the seam (BR-5) |
| Emphasis | `*text*` outside a code span | terminal italic or dim, authored by the seam (BR-5) |
| Code span | `` `text` `` | visually distinguished; the backticks are not printed |
| Thematic break | a line of three or more `-`, `*` or `_` | a horizontal rule at the terminal width |
| Blank line | an empty line | preserved as paragraph separation, never collapsed |

The constructs deliberately **excluded** from this set are listed under Out of Scope; each one
falls through to literal text rather than to a parse error.

### Events

None. No protocol change, no new `SessionUpdatePayload` variant, no bus record. (Contrast
BUG-189, where a decision the surface had to show carried no record — here the surface already
receives every byte it needs.)

### Permissions

Not applicable — rendering crosses no gate.

## Business Rules

- [ ] **BR-1: The system prompt carries an output-format clause, and the clause is true after
      this REQ ships.** `build_system_prompt` gains a clause stating that the reply is printed
      into a **narrow terminal**, and asking for short paragraphs and bullets over tables; that
      tables, when genuinely the right shape, stay narrow (at most three short columns, never a
      sentence in a cell); and that emphasis and fenced code are used sparingly. It follows
      `effective_web_clause`'s shape — the words live in one named constant, the *decision to
      include it* lives in `build_system_prompt` (informed by BUG-181).

      *Amended during implementation (2026-08-26).* This rule originally said the clause should
      state the terminal "renders no markdown". That is true today and **false the moment BR-3..BR-6
      land in this same REQ** — the CLI will render bold, emphasis, code spans, headings and
      tables. Shipping it would put a false claim about Teton's own surface into the system prompt,
      which is exactly BUG-181's defect class and exactly what this REQ cites as its motivation.
      The operative fact is **narrowness**, not absence of rendering: a wide table is unreadable
      whether or not it is laid out. The clause must state what stays true (informed by BUG-181,
      LESSON-548 — a remedy is a claim about your own surface).

- [ ] **BR-2: BR-1's clause fits the resident-prompt budget, or moves it deliberately.**

      *Amended after architecture (ADR-6): there are **two** independent ceilings, not one. This
      rule originally named only the first. Both must be re-measured, and a larger clause could
      flip which one binds.*

      **(a)** `egress::redact::REDACT_BODY_OVERHEAD_BYTES` (11 KiB) is measured against the worst-case
      system prompt by `the_total_cap_clears_the_harness_context_budget_with_margin`, with a
      `MIN_PROMPT_HEADROOM_BYTES` floor of 48.

      *Corrected during implementation (2026-08-26).* This rule originally quoted the boundary
      recorded in `REDACT_BODY_OVERHEAD_BYTES`'s doc ledger — "710 bytes of filler passes, 711
      fails". **That ledger is stale and was stale before this REQ began.** It dates from REQ-587;
      the worst-case prompt has grown ~234 bytes since across REQ-583/585/587/589/590/591 without
      the ledger being restated. The **measured** pre-edit margin is **476** bytes against the
      48-byte floor. This is why the rule says *measure, do not trust the figure* — the figure it
      originally quoted is the one that was wrong. Restating the stale ledger is a pre-existing
      defect, filed separately rather than absorbed here. If the clause does not fit, the constant moves the
      way REQ-577, BUG-181 and REQ-587 moved it — and, since REQ-586 gave it a production reader,
      the raise also narrows every `[privacy] redact = true` route's scannable budget and must
      re-state `the_overhead_raise_restates_the_chunk_count_and_the_scannable_bound`. The floor is
      never traded away (REQ-577 BR-4).

      **(b)** `harness::budget::MIN_BUDGET_BYTES` (16,384) is asserted at `>= 2 ×` the
      **default-config** prompt by `min_budget_bytes_holds_the_harnesss_own_system_prompt`
      (budget.rs:4016) — a ceiling of 8,192 bytes on the prompt itself. Measured on this branch
      before any edit: **6,411 bytes, 1,781 of slack.** Independent of (a), which measures a
      different shape (worst case, with the skill roster at its cap) against a different constant.

      **(c)** A constraint on the *wording* rather than the size:
      `a_harness_authored_system_prompt_is_byte_identical` (harness/render.rs:825) requires
      `neutralize_frame_labels` to be a no-op on the prompt, so the clause must carry **no
      flush-left `User:` or `Assistant:` label**.

      Measured slack: (a) 710 bytes, (b) 1,781 bytes — **(a) binds first.** Candidate wordings
      measure 184–322 bytes, so neither constant is expected to move.

- [ ] **BR-3: Assistant prose is word-wrapped at the terminal's width.** The surface holds a
      partial line across `fragment()` calls — assistant text arrives token-by-token, so no single
      call is a line — and on each completed line emits it broken at word boundaries at
      `terminal_width()`, never mid-word. A single word longer than the width is emitted on its
      own row rather than silently truncated. Wrapped continuation rows of a list item or block
      quote are indented to the item's hanging indent.

- [ ] **BR-4: A run of table rows is laid out as a block, not as prose, and no emitted row
      exceeds the terminal width.** Consecutive table rows are buffered until the run ends, then
      measured and emitted one of two ways. **When the columns fit**: cells are padded so that
      each column's values line up vertically, and the separator row is drawn as a rule rather
      than printed. **When they do not**: each data row becomes its own labelled block — one line
      per column, carrying the column's header and that row's value, with the value wrapped under
      BR-3 and the blocks separated by a blank line. Buffering costs streaming *within* a table;
      that is the accepted trade.

- [ ] **BR-5: Inline styling is authored by the seam, after defusing, from a fixed table.** The
      renderer parses `**strong**`, `*emphasis*`, `` `code` `` and `#`-headings out of the
      **already-defused** text and emits the corresponding SGR itself, from a fixed table in the
      same shape as `LineKind::sgr()`. Model-supplied escape bytes are never passed through: they
      have already been replaced with spaces by `defused_multiline`, and that stays true. This is
      LESSON-517's rule applied forward — when a seam gains a sanitizer it must own every
      legitimate use of the alphabet it destroys — and its inverse is the hole REQ-563/573 closed:
      a renderer that let markdown style itself by admitting escapes would hand a fetched page the
      cursor back (informed by LESSON-517, REQ-573, REQ-563).

      **Recorded limitation (2026-08-26): inline styling does not apply inside table cells.** BR-4
      requires column measurement to ignore inline markers (a `**bold**` cell measures 4 columns,
      not 8), so the table layout returns final display text with markers already removed. Styling
      it would need either a second `parse_inline` pass — which strips a second time and shifts
      every cell 4 columns left per marker pair, un-aligning the table — or markers left in the
      output, which mis-measures (`| **a | b** |` is two literal cells to the table splitter but
      one strong run once joined). A bold cell therefore renders **unstyled at the right column
      rather than bold at the wrong one**. Alignment is the whole point of BR-4, so that is the
      correct trade; lifting it needs a richer return type, not a second parse, and is a follow-up.

- [ ] **BR-6: Fenced code is passed through, never reflowed and never styled.** Inside a ```` ``` ````
      fence, BR-3's wrapping and BR-5's inline parsing are both off: a wrapped line of code is a
      wrong line of code, and `*` inside a shell glob is not emphasis. Fence content is emitted
      verbatim (still defused) and is allowed to exceed the terminal width. The fence markers
      themselves are not printed.

- [ ] **BR-7: Rendering is gated on a terminal, and the renderer is inert off one.** Wrapping and
      layout apply when stdout is a terminal (`main.rs:1042`'s `interactive`); SGR additionally
      requires `color` (`main.rs:1045`, `interactive && banner::color_enabled()`, which is false
      under `NO_COLOR` or `TERM=dumb`). Off a terminal — `cli_e2e`, shell composition, a user's
      `> audit.md` — the surface emits the model's bytes unchanged, markdown intact.

      *The claim is about the renderer, not about end-to-end output.* BR-1 changes what the model
      writes for piped sessions too, so a real piped session's bytes will differ from today's.
      What must not differ is the transform applied to them: none. AC-7 asserts exactly that, by
      comparing a scripted turn's piped output against the chunks the daemon sent.

- [ ] **BR-8: The pending buffer is flushed before anything else claims a row.** `line()` and
      `repaint_row_above()` each own a row and already close an open streamed line; with a
      buffer in play they must first *emit* it. The buffer is also flushed at turn end, before
      `hand_off_after_turn` can print. No path may leave a partial line unwritten when the session
      returns to the prompt.

- [ ] **BR-9: The turn accumulator keeps receiving raw model text.** `session_ui.rs:2425` pushes
      each chunk into `state.turn_reply` and *then* hands it to the surface. Rendering happens
      inside the `Surface`, downstream of that push, so the REQ-579/581/582 hand-off predicates
      keep matching on the model's own words — including the backticks `without_backticks` strips.
      A design that rendered upstream of the accumulator would silently break three shipped
      features (informed by LESSON-529: a display helper that disagrees with the consuming parser
      is a lie on screen).

- [ ] **BR-10: Every layout decision is reachable without a terminal.** What the renderer
      *decides* — where a line breaks, whether a table fits, which spans are styled — must be
      determined by pure logic that takes the width as input rather than reading it, so the
      default `cargo test` suite can drive it with no TTY, no pty and no clock. Only the byte
      emission stays under BR-7's gate. A feature gated on an environment the suite does not
      provide is a feature the suite cannot see, and that is a finding about the design rather
      than a detail to sort out at implementation time (informed by LESSON-481).

## Acceptance Criteria

_Ordered by the business rule each covers. Every BR-1..BR-10 is named by at least one AC below;
an AC that covers no rule, or a rule no AC names, is a finding against this list._

- [ ] **AC-1** *(BR-1)*: `build_system_prompt` output contains the output-format clause, and
      contains it **exactly once** — asserted in the shape
      `turn_loop::tests::the_system_prompt_states_what_the_session_can_run_and_from_where` already
      uses for BUG-181's capability sentence (filter the guide's lines for the clause's anchor
      phrase, assert the count is 1, with a message saying that a second sentence about output
      format is a decision rather than an accident). Mutation-checked: remove the `push_str` and
      the test fails. A budget test alone would not catch a deleted clause, which is why this one
      exists separately from AC-2.
- [ ] **AC-2** *(BR-2)*: `the_total_cap_clears_the_harness_context_budget_with_margin` is green
      with BR-1's clause in place. If `REDACT_BODY_OVERHEAD_BYTES` moved, the PR shows the
      re-stated chunk count and scannable bound
      (`the_overhead_raise_restates_the_chunk_count_and_the_scannable_bound`), and the new margin
      figure is recorded in the constant's doc comment alongside the REQ-577/BUG-181/REQ-587
      entries.
- [ ] **AC-3** *(BR-3, BR-10)*: Unit tests pin the wrap function: a paragraph wider than the width
      breaks at spaces and never mid-word; a single token longer than the width occupies its own
      row intact; a hanging-indent list item's continuation rows align under its text; width `80`
      is used when `terminal_width()` reports no terminal. These run in the default `cargo test`
      suite with no pty and no TTY — which is BR-10's claim, asserted by the fact that they run at
      all (informed by LESSON-481).
- [ ] **AC-4** *(BR-4)*: Unit tests pin both table modes against a fixed width. A narrow table
      renders with its columns lined up vertically and its separator row drawn as a rule rather
      than printed. The table recorded in
      [`fixtures/audit-2026-08-26.md`](fixtures/audit-2026-08-26.md) — the real reply that
      motivated this REQ, 7 data rows whose second column measures 155..243 chars against a widest
      raw row of 263 — renders as one labelled block per data row at both 100 and 200
      columns, with **no emitted row exceeding the width**. The fixture is
      read from disk by the test, not transcribed into it: a table authored while knowing the
      layout algorithm tests the author's assumptions rather than the algorithm (LESSON-529's
      re-enactment corollary).

      *Corrected during implementation (2026-08-26).* This criterion originally also demanded
      "every value wrapped" at both widths. **That is false at 200 columns and asserting it would
      have pinned a bug**: the label prefix leaves 191 usable columns, and four of the fixture's
      seven values (153, 161, 168, 170) fit on one row — a break inserted there would be wrong.
      The implemented assertion is the biconditional **wraps ⟺ value wider than the available
      width**, which is strictly stronger than the original at both widths, plus an unconditional
      "all seven wrap" at 100 columns where it does hold, plus a word-for-word round-trip proving
      no value is clipped.
- [ ] **AC-5** *(BR-5)*: A chunk containing `\x1b[2K\x1b[1A` renders as visible spaces, not as
      cursor motion, **with the markdown renderer in the path** — and the assertion is
      mutation-checked by removing the defuse call and watching it fail. `**bold**` in the same
      chunk still emits SGR, proving the styling comes from the fixed table and not from the input.
- [ ] **AC-6** *(BR-6)*: A turn whose reply is a fenced diff or shell block emits the fence content
      with original line breaks and no SGR, including lines longer than the terminal width.
- [ ] **AC-7** *(BR-7, pipe leg)*: `cli_e2e` (piped stdout) output for a scripted turn carrying a
      table, bold text, and a code fence equals the concatenated raw chunks the daemon sent — the
      renderer is inert off a terminal, asserted rather than assumed.
- [ ] **AC-8** *(BR-7, colour leg)*: The terminal-but-no-colour path is covered on both halves.
      **Unit**: a surface constructed with `color = false` at a fixed width wraps its input and
      emits **zero** `\x1b` bytes, while the same input at `color = true` emits the SGR AC-5 pins.
      **pty**: a session launched with `NO_COLOR=1` in the child environment produces wrapped rows
      and no escape sequences. Both legs are required — the unit leg pins the surface's own branch,
      the pty leg pins that `banner::color_enabled()`'s reading of the environment actually reaches
      it. A reachable shipped path (`NO_COLOR`, `TERM=dumb`) that no test drives is LESSON-481's
      blindfold with the gate on the other side.
- [ ] **AC-9** *(BR-8, interleave)*: A `Notice`/`Tool` line arriving mid-stream emits after the
      pending buffer, not through it: the streamed sentence is complete on its own rows and the
      notice starts clean. Asserted on a `RecordingSurface` as an ordered `(kind, text)` sequence
      — **and, because a recorder has no buffer and so preserves call order trivially, that leg is
      near-vacuous. The substantive assertion is the byte-level one on `PlainSurface`,** which is
      where the pending buffer actually exists. Noted 2026-08-26 rather than left to look like
      coverage it is not.
- [ ] **AC-10** *(BR-8, tail flush)*: **No line may fall off the end of a turn.** A turn whose
      final chunk carries no trailing newline still has its last line on screen before the session
      returns to the entry prompt. Asserted on both legs, because neither alone is sufficient:
      on a `RecordingSurface`, the tail is *emitted* rather than held; at the **pty**, the row is
      actually visible above the entry frame, and it appears **before** any
      `hand_off_after_turn` line. Mutation-checked by removing the end-of-turn flush and watching
      the final line disappear — a buffer that silently eats the last sentence of every reply
      whose model happened not to send a trailing `\n` is the worst failure this REQ can ship, and
      the pipe path (AC-7) is structurally blind to it.
- [ ] **AC-11** *(BR-9)*: The REQ-579 setup hand-off, the REQ-581 connection hand-off, and the
      REQ-582 command hand-off each still fire on a turn whose reply contains the trigger text
      wrapped and styled — proving the accumulator saw the raw text and BR-9's ordering held.
- [ ] **AC-12** *(BR-3, BR-4, BR-5 at the real seam)*: A `pty_e2e` leg asserts the *rendered bytes*
      of a turn at a fixed pty width: wrapped rows, a transposed table, and one SGR-styled span.
      This leg is written in this REQ, not claimed as covered by an existing test — BUG-191 is what
      an unwritten pty claim looks like in a verify pass (informed by BUG-191, LESSON-481).
- [ ] **AC-13** *(whole-feature, manual)*: `docs/manual-verification.md` gains a runbook entry: ask a
      real session for a security audit of this repository at 100 and at 200 columns, and record
      that the tables and prose are legible.
      [`fixtures/audit-2026-08-26.md`](fixtures/audit-2026-08-26.md) is the **before** — the
      fixture holds the reply, not the prompt, so the runbook carries the prompt itself. The
      comparison is a judgement the runbook records in prose; AC-4 is what makes the layout claim
      mechanical.
- [ ] **AC-14** *(recognized-construct table's fallthrough rule)*: Each construct named under Out
      of Scope — a nested list, a setext heading, an indented code block, nested emphasis, and a
      `|` inside a code span inside a table cell — renders as **literal text**, wrapped under BR-3,
      with no panic, no dropped characters, and no partial styling. This is the mitigation OQ-2's
      hand-rolled decision rests on, so it is asserted rather than assumed: an unrecognized
      construct that mangles or swallows content would make the decision wrong.

      **"No dropped characters" means no lost content, not preserved markers** (clarified
      2026-08-26). A `|` inside a code span inside a table cell falls through to prose as promised
      — and as prose, its code span is an ordinary code span, so the backticks are consumed exactly
      as `**bold**`'s asterisks are. Reading the phrase literally would forbid inline styling
      everywhere. The implemented assertions say what actually matters for that construct: every
      pipe survives, no rule is drawn, no column padding is applied, and the span is styled whole
      rather than split at the pipe.

      **One carve-out, recorded during implementation (2026-08-26): the `-----` setext underline.**
      A line of three or more dashes is *already* a thematic break in the recognized-construct
      table, and it is one in CommonMark too, independent of any text above it. A line-oriented
      streaming classifier cannot tell the two readings apart without lookahead it does not have
      (BR-3 and BR-8 require emitting as text arrives, not at end of turn). It therefore draws a
      rule. The heading *text* on the line above is unaffected and still renders as literal prose,
      so nothing is mangled or swallowed — the screen shows text followed by a full-width rule,
      which reads as an underlined heading. The `=====` form has no such ambiguity and is fully
      literal. Both behaviours are asserted explicitly in `a_setext_heading_is_literal_text`
      rather than left to be discovered.

## External Dependencies

- None **required**. The wrap, layout and inline logic is implementable against `std` alone.
- `pulldown-cmark` is **rejected** (OQ-2, decided). Nothing in this REQ takes a markdown-parsing
  dependency.
- `unicode-width` remains the one open candidate (OQ-1): correct display width for CJK and emoji,
  which column alignment and break placement both need. It would be the first non-trivial
  dependency the CLI has taken beyond `clap`/`serde_json`/`anyhow`/`libc`, and the crate's
  manifest documents that thinness as a property — so it is an architecture decision, not an
  implementation detail. If it is refused, the fallback is `chars().count()` and the consequence
  (visible mis-alignment on CJK and emoji tables) must be recorded, not absorbed silently.

## Assumptions

- The `teton` CLI is the only `Surface` consumer that ships today (the VS Code extension is
  phase 2), so BR-1's "your output is printed into a plain terminal" is true for every client that
  currently exists. It stops being unconditionally true the day a second client renders markdown
  — see OQ-3.
- Models honour formatting guidance imperfectly and unevenly by tier. BR-1 improves the common
  case; BR-4's transposition is what makes the uncommon case survivable. The spec does not depend
  on any model complying.
- `terminal_width()`'s 80-column fallback is the right non-terminal default and is not re-litigated
  here; BR-7 means it is only ever consulted on the terminal path anyway.
- REQ number allocated with the remote counter reachable (`ADLC_ALLOC_DEGRADED=0`), so no
  post-hoc id verification is owed.
- The checked-in fixture is **one section of one reply**, transcribed from the session rather than
  captured from stdout — the "Areas Verified as Strong" table, without the audit's second table or
  its surrounding prose. It is sufficient for AC-4 (which needs one real wide table) and honest as
  AC-13's before, and it is not a complete transcript. Nothing downstream should describe it as
  one, and a wider corpus of real replies would strengthen AC-4 if one is ever captured.

## Open Questions

- [x] **OQ-1** *(decided in architecture — ADR-5: **take `unicode-width`**. The two prior in-repo
      declines were of a *format-character category* table, a cosmetic gap; display width breaks the
      feature when wrong — a CJK char measured 1 but displayed 2 makes a wrapped row exceed the
      terminal and hard-wrap mid-word, which is the defect this REQ fixes. Zero transitive deps.
      Grapheme clusters/ZWJ emoji stay wrong under either choice — recorded, out of scope.)*:
      **`unicode-width`, or `chars().count()`?** BR-4's column alignment and BR-3's break
      placement both need a *display* width. `chars().count()` misjudges CJK
      (double-width) and emoji, so a table containing either would visibly mis-align. Note that
      `neutralized` already removes the zero-width and joiner set, which is the other half of the
      problem — so the remaining gap is specifically wide characters. Recommend taking the
      dependency; record the decision either way.
- [x] **OQ-2** *(decided at spec time, 2026-08-26 — **hand-rolled**, scoped to the
      recognized-construct table above)*: **hand-rolled line-oriented parser, or `pulldown-cmark`?**
      The constructs this REQ renders are a small, line-oriented subset. `pulldown-cmark` is
      correct CommonMark but is event-based over a *complete document*, which fights the
      incremental line buffer BR-3 and BR-8 require — assistant text arrives token-by-token and
      must render as it arrives, not at end of turn. A hand-rolled scanner over the closed
      construct set streams naturally and takes no dependency. **The cost, recorded rather than
      discovered later:** the parser will be wrong on CommonMark edge cases the table does not
      name (nested emphasis, `|` inside a code span inside a table cell, indented code blocks).
      Those fall through to literal text by design, which is legible-but-unstyled rather than
      mangled. If that fallback proves wrong in practice, revisiting this is a new REQ, not a
      patch — swapping the parser changes the streaming model.
- [x] **OQ-3** *(decided in architecture — ADR-9: **unconditional**. A protocol hint buys back
      ~250 bytes of a budget with 710 spare, and the CLI is the only client that ships. Revisit when
      a second client exists.)*: **should BR-1's clause be conditional on the client being a terminal?** The daemon does
      not know — nothing on `session/create` carries a client-surface hint. Making it conditional
      is a protocol addition that buys back BR-2's bytes for piped sessions and keeps the clause
      honest for a future markdown-rendering client; making it unconditional costs those bytes
      always and is one line. Recommend unconditional for this REQ and record the protocol
      addition as a follow-up if a second client lands.
- [x] **OQ-4** *(decided in architecture — ADR-9: **no handler**. Width is read per flushed block,
      so a resize takes effect on the next block. The CLI has no SIGINT handler either; signal
      handling for cosmetics is not proportionate.)*: **does a `SIGWINCH` mid-turn re-wrap?** Simplest behaviour is to read
      `terminal_width()` per flushed line, so a resize takes effect on the next line and already
      printed rows keep their old breaks. Confirm that is acceptable rather than installing a
      handler.
- [x] **OQ-5** *(decided in architecture — ADR-9: **unwrapped**, as the spec leaned. Wrapping them
      moves bytes several `cli_e2e`/`pty_e2e` fixtures pin, for text that is short by construction.
      Filed as a follow-up if it still reads badly after this ships.)*: **do `line()` kinds wrap too?** A long `Notice` or `Tool` line hard-wraps today for
      the same reason assistant prose does. Wrapping them is a small extension of BR-3 but changes
      bytes that several e2e fixtures pin. Recommend leaving `line()` unwrapped in this REQ and
      filing it separately if it still reads badly afterwards.

## Out of Scope

- Full CommonMark. Concretely, and as the constructs the recognized-construct table's fallthrough
  rule covers: nested lists beyond one level, reference and autolinks, setext headings, indented
  (non-fenced) code blocks, inline HTML, footnotes, task lists, nested emphasis, and `|` appearing
  inside a code span inside a table cell. Each renders as literal text — legible but unstyled —
  rather than raising a parse error. This is the accepted cost of OQ-2's hand-rolled decision.
- Syntax highlighting inside code fences.
- OSC 8 hyperlinks, or any escape family beyond the SGR set BR-5's fixed table names.
- A ratatui / alternate-screen TUI. The `Surface` seam exists so that is a separate REQ; this one
  stays inside `PlainSurface`'s successor.
- The `LineKind::Diff` path — proposed diffs have their own renderer and their own reasons.
- Making the model *able* to render markdown differently per client (a capability negotiation);
  BR-1 states one fact about one surface.
- Re-tuning any existing `SELF_CONFIG_GUIDE` sentence to pay for BR-1's bytes. Those lines are
  pinned by live A/B (REQ-579); BR-2's answer to a budget overrun is to move the constant, not to
  re-open a sentence that works.

## Retrieved Context

- LESSON-537 (lesson, score 11): A second surface inherits every grammar and gate it touches
- LESSON-548 (lesson, score 9): A refusal's remedy is a claim about the product's own surface
- LESSON-529 (lesson, score 9): A display helper is a second parser
- LESSON-481 (lesson, score 9): A gate that hides a feature also hides its tests
- BUG-189 (bug, score 8): Two refusal reasons publish no record, so the session surface never says
- LESSON-517 (lesson, score 8): A sanitizing seam owns the styling too
- BUG-164 (bug, score 7): A targeted e2e run can pass against a stale daemon binary
- BUG-191 (bug, score 7): No pty leg for the acknowledgment prompt bytes
- LESSON-535 (lesson, score 7): A probe is a billed call and a preview is a surface
- BUG-173 (bug, score 7): The pty suite's entry-prompt wait absorbs daemon startup
- LESSON-510 (lesson, score 7): Existence is not freshness
- LESSON-495 (lesson, score 7): A grant is only as narrow as its key
- BUG-181 (bug, score 6): The model affirms capabilities Teton does not have
- LESSON-524 (lesson, score 6): Exposure is not callability
- LESSON-518 (lesson, score 6): A blocking gate's reader-loop freedom needs a parked verifier
