---
id: LESSON-668
title: "Tally a fail-closed classifier's refusal reasons before choosing its next widening"
component: "daemon/harness"
domain: "harness"
stack: ["rust", "daemon"]
concerns: ["privacy", "reliability", "developer-experience"]
tags: ["shell-provenance", "classifier", "allowlist-grammar", "quoting", "transcripts", "widening", "subagent"]
req: BUG-236
created: 2026-10-08
updated: 2026-10-08
---

## What Happened

REQ-614 shipped the shell provenance classifier as a strict allowlist grammar
and left OQ-3 open: "ship the strict form and measure how often `/shell allow`
is typed before widening". REQ-620 widened it one month later for the one shape
a single dogfood session had tripped on (`2>&1`), and listed quoting among the
shapes that "stay unmodelled". Every `/analyze` session since pinned on a
quote — BUG-214's preamble, BUG-227's delegation gate, and on 2026-10-08 three
of four REQ-623 children within fifteen seconds of starting — and each time the
pin was diagnosed from scratch as a new incident. The transcripts had been
recording `session_pinned.reason` with the exact class sentence the whole time;
nobody had grouped them.

The fix (BUG-236) was the same shape as REQ-620's: a narrow pre-pass that lifts
one provable form — a quoted span with no `$`, backtick, backslash or inner
quote, whose meaning POSIX `sh` fixes completely — before the scan and the
split, with the lifted bytes restored only where the grammar reads a word. Its
soundness argument is one sentence: a residue that keeps *any* quote standing
is refused, so a partial understanding of a quote can only ever land on the
old answer.

## Lesson

When a fail-closed classifier logs *why* it refused, the next widening is a
query, not a guess: group the refusal reasons across the transcripts you have
and widen the most frequent provable shape first. A class that is refused
"by design" and also tops the tally is a defect wearing a design's clothes —
the design decided the general case, and the general case is not what the
actor writes. Record the tally in the REQ that ships the widening so the next
one starts from it.

Two corollaries from the same incident:

- **A widening's safety is in its fallback, not its parser.** Lift exactly one
  shape and make every miss — a span that could expand, an unterminated quote,
  a forged placeholder — leave a byte the old scan refuses on. Then the proof
  obligation is "the lifted shape means what we say", and nothing else.
- **A child inherits the rule only if you forward it.** The toolkit skill told
  the parent not to write quotes and said nothing to the children it fanned
  out; the children wrote the most common spelling there is. Whatever grammar
  the harness enforces, the task text handed to a subagent has to carry the
  same constraint the parent was given.

## Why It Matters

Each pin cost one `/analyze` run: billed remote calls discarded, the audit never
produced, and twenty minutes of transcript diagnosis to arrive at the same
sentence as the last time. Three incidents over five weeks against a refusal
that one `grep -c` over `~/Library/Application Support/teton/transcripts`
would have ranked first on day one.

## Applies When

- Deciding what a fail-closed grammar (`shell_provenance`, `root_gate`, the
  redaction scan, any allowlist) should learn to accept next.
- Reading a privacy pin or an `unknown` verdict in a transcript: check
  `session_pinned.reason` across *all* recent sessions before treating it as a
  one-off.
- Writing or editing a skill that dispatches subagents: the shell-grammar rule
  the parent is given must appear in the children's task text too.
