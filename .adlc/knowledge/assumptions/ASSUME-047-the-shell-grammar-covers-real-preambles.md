---
id: ASSUME-047
title: "REQ-614's verdict grammar is sufficient for the preambles skill authors actually write"
status: partially-validated
req: REQ-619
created: 2026-09-05
resolved: 2026-09-05
---

## Assumption

That `cat`, `ls`, `git status` and the other spellings real `SKILL.md`
preambles use are `rooted` under the REQ-614 grammar, so classification
removes the pin for the common case and `unknown` is left for genuinely
opaque commands.

## Context

REQ-619 replaced "any spawned preamble pins" with a per-command verdict. If
the grammar left most real preambles `unknown`, the REQ would have shipped a
classifier and changed nothing a user could feel.

## Resolution

Validated for the shapes the acceptance suite exercises — `cat <in-root>`,
`ls -la`, `git status` are `rooted` end to end (AC-3, AC-11, AC-13). **Not**
validated for the toolkit's own skills: every `/analyze`-style skill opens
with `sh .adlc/partials/ethos-include.sh …`, and `sh` is an opaque verb, so
those still pin — liftably, and announced, but pinned. Two further limits
found at verify and written into BR-2: a whole-command grammar short-circuit
(`>`, `$`, quotes) is `unknown` before any path is examined, and a
path-qualified verb (`bin/ls`) is deliberately `unknown` (verify H2). The
rewrite of the toolkit partials to `cat` is the toolkit's follow-up.
