---
id: BUG-236
title: "A simple quoted literal pins the session, and three /analyze children each wrote one"
status: open
severity: high
created: 2026-10-08
updated: 2026-10-08
component: "daemon/harness"
domain: "harness"
stack: ["rust", "daemon"]
concerns: ["privacy", "reliability", "developer-experience", "cost"]
tags: ["shell-provenance", "classifier", "quoting", "privacy-pin", "unknown-shell", "subagent", "analyze"]
introduced_by: ["REQ-614", "REQ-620"]
attribution: derived
---

## Description

The shell provenance classifier (`crates/tetond/src/harness/tools/shell_provenance.rs`)
refuses every command containing a `'` or a `"` before it reads the verb
(`UNMODELLED`, `UnmodelledSyntax::Quote`). The rule was written to keep the
grammar from becoming a shell lexer (ADR-614-1). But a *simple* quoted literal
— `find crates -name "*.toml"`, `grep -rn 'fn main' src` — is the single most
common thing a model writes into a shell command, and it is exactly the shape
whose meaning `sh` fixes completely: with no `$`, backtick, backslash or inner
quote, quote removal yields the span's bytes and nothing is expanded, split or
globbed.

So a remote-routed turn that fans out REQ-623 children pins itself within
seconds. Each child writes an ordinary read-only command with a quoted
pattern, each is `unknown_shell`, the session pins, every in-flight call is
rerouted to the local tier, and the parent — which inherits the taint through
the children's derived reports — has its typed skill refused at the BR-8 refit
because the expansion no longer fits the local window.

## Reproduction Steps

Observed 2026-10-08 in `sess-vrx4j15qr4krafczy9r5f8sszm` (teton-code checkout,
`/analyze` on kimi-k3):

1. Type `/analyze`. The preamble and the parent's own `git` calls all classify
   `rooted` (BUG-220's rewrite holds). The route classifier sends the turn to
   kimi (think tier). Two model calls run.
2. The parent dispatches four children (code-quality, security, test,
   convention) at 20:38:19.
3. Within fifteen seconds three of them run a quoted command:
   - security: `find crates -maxdepth 2 -name "*.toml" | head -20 && echo --- && ls …`
   - code-quality: `find crates -name '*.rs' -path '*/src/*' | xargs wc -l | …`
   - convention: `grep -rnE 'std::fs|std::net|…' crates/teton-core/src --include='*.rs' | …`
   Each is `unknown_shell` with reason "the command uses a quoted string this
   classifier does not model". `session_pinned` fires at 20:38:27.
4. Four `privacy_block` events (`<unknown-provenance>`, `rerouted_to_local`)
   follow: one per child and one for the parent. Each child is re-fitted into
   the 21,162-token local budget; three complete, code-quality fails.
5. The parent's refit refuses `/analyze`: 9,109 words / 66 KB against a
   63 KB budget (`WithDynamicContext` arm). The turn ends with nothing kept.

The `test` child used only `2>&1`, `;` and `| head` and stayed `rooted`.

## Expected Behavior

A command whose quoted spans are simple literals classifies exactly as the
unquoted word would: `find … -name "*.toml"` is `rooted`, `cat ".env"` is a
`boundary_touch` naming `.env`, and `echo "a && cat .env"` runs no `cat`.
Spans that could expand (`"$HOME"`, a backslash, an inner quote) stay
`unknown` on the quote, with the same sentence as before.

## Actual Behavior

Every quote character refuses the whole command. Three children pinned a
session that had read nothing protected; the parent's billed remote work was
discarded.

## Environment

- Platform: macOS, Apple M5 Max
- Version: teton v0.1.39 (be6194e1)

## Root Cause

`classify_with_budget` runs `first_unmodelled_class` over the whole command
text and refuses on `'` or `"` wherever they fall. There is no reading of what
a quote *does* — the design refused one, and that refusal was right for the
general case and wrong for the one shape that is both ubiquitous and fully
determined. REQ-620 established the pattern for widening this grammar without
a lexer: a narrow pre-pass that lifts a provably harmless form as whole words
before the scan and the split (`shell_syntax::strip_null_redirects`). Quoting
had no such pre-pass.

Secondary, not fixed here: a child's `unknown_shell` pin is session-wide and
its report carries the taint into the parent by derivation (REQ-544 C-1,
REQ-623 BR-6), so a child that improvises one unmodelled command ends a
parent's typed skill turn. That is correct given the verdict; the verdict was
wrong. Whether a remote parent should instead withhold a tainted child's report
is REQ-623's question to reopen.

## Resolution

A new pre-pass, `harness::tools::shell_quotes::lift_quoted_literals`, runs
first in `classify_with_budget` — ahead of REQ-620's redirect strip, the
unmodelled scan and the segment split. It lifts every **simple** quoted span
(`'X'` / `"X"`, `X` non-empty, not starting with `~`, free of quotes, backtick,
`$`, backslash and newline) into an indexed private-use placeholder, so none of
the three REQ-614 stages ever sees a quoted byte: a `*` inside quotes is not a
glob, a `|` is not a pipe, a `2>&1` is not a redirect. `classify_segment` then
expands each word's placeholders back to the span's bytes **before** every
check it makes — the `=` rule, the opaque table, the `find -exec` guard, the
verb tables and `resolve_token` — so the grammar reads exactly the word `sh`
hands the program. A span that is not simple keeps its opening quote in the
residue and the command is refused on the `Quote` class as before; a command
carrying the placeholder character itself is refused outright. The `shell`
tool's reach contract tells the model plain quotes are fine (414 → 419 bytes,
one under ADR-620-5's ceiling; both prompt-margin ledgers moved by −5 in the
same diff).

Verified: 43 classifier unit tests and 4 lift tests green, including the
2026-10-08 commands as fixtures; the two e2e tests that pin the `Quote` class
moved to a span holding an apostrophe and stay green; four mutations run and
recorded in `shell_provenance.rs`'s module docs (5 / 2 / 2 / 3 red, each
reverted). Full `tetond` lib suite: 2,348 passed.

Not changed, by design: `--include='*.rs'` is still refused, on the `=` rule
(which also guards `--file=.env`); spans holding `$`, a backtick or a
backslash; REQ-623's propagation of a child's taint into its parent.

## Files Changed

- `crates/tetond/src/harness/tools/shell_quotes.rs` — new: the lift, the
  placeholder, `expand`, the residue table and its tests
- `crates/tetond/src/harness/tools/mod.rs` — registers the module
- `crates/tetond/src/harness/tools/shell_provenance.rs` — lift as step 0,
  `Scope.literals`, expansion in `classify_segment`, `RESERVED_CHARACTER_REASON`,
  module-doc section and mutation record, five new tests, three fixtures moved
  to a non-simple quote, two toolkit rows flipped to `Rooted`
- `crates/tetond/src/harness/tools/shell.rs` — reach contract says plain quotes
  are fine; the needle table's description updated
- `crates/tetond/src/egress/redact.rs` — ledger line; `RECORDED_PROMPT_MARGIN_BYTES`
  208 → 203, `RECORDED_WEB_PROMPT_MARGIN_BYTES` 255 → 250
- `crates/tetond/src/harness/tools/web.rs` — recorded headroom at BUG-236
- `crates/tetond/src/harness/root_gate.rs` — gate-only comment no longer claims
  the classifier refuses `echo "2 > 1"`
- `crates/tetond/tests/boundary_coverage.rs` — `TOOL_SOURCES` roster gains the
  new file (15 → 16)
- `crates/tetond/tests/e2e/shell_pin_shape.rs`,
  `crates/tetond/tests/e2e/skill_provenance.rs` — quote-class fixtures use an
  apostrophe inside the span
- `CHANGELOG.md` — Unreleased entry
- `.adlc/knowledge/lessons/LESSON-668-tally-refusals-before-choosing-the-widening.md`
