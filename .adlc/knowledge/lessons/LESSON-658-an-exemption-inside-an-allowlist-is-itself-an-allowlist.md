---
id: LESSON-658
title: "An exemption inside an allowlist grammar is itself an allowlist — a denylist there inverts the fail-closed polarity"
component: "daemon/egress"
domain: "privacy"
stack: ["rust"]
concerns: ["privacy", "security", "reliability"]
tags: ["shell-provenance", "classifier", "allowlist", "denylist", "fallthrough", "exemption", "multi-agent-review", "req-620", "req-614"]
req: REQ-620
created: 2026-09-09
updated: 2026-09-09
---

## What Happened

REQ-620 widened REQ-614's shell provenance grammar — an allowlist whose
fallthrough is `Unknown` — with three exemptions: lift `2>/dev/null`-style
redirects, let a piped reader take its stdin instead of walking the root, and
treat `&>/dev/null` as harmless. Each exemption was written as a **denylist**
inside the allowlist: a table of recursive-`grep` flags to keep walking, a
list of filter verbs assumed to read only stdin, and a lifted `&>` form. Each
leaked. `grep --directories recurse` and `-nd recurse` were not in the flag
table; `wc --files0-from -` was a listed verb with an unlisted flag; `&>` is a
bash extension that dash splits on `&`, hiding a second command from the verb
check; and the write gate's verb trigger still read the raw command, so a
leading `2>/dev/null` shadowed `rm`. The six-agent verify round and its
four-agent confirmation loop found all four before merge; the implementer's
own mutation records had not, because a denylist's tests enumerate what it
knows.

## Lesson

Inside a fail-closed grammar, an exemption's fallthrough must be the grammar's
fallthrough. Write the exemption as a closed list of what is proved harmless —
exact verbs, exact flag shapes, exact residue forms — and let everything else
land on the old answer. A doc comment claiming exhaustiveness over another
program's option table ("the forms GNU and BSD grep accept") is the tell: it
is a claim no test holds. And when two gates read one syntax, they must share
the *wrapper* as well as the recogniser, or they disagree on the commonest
spelling (LESSON-494).

## Why It Matters

The classifier is the privacy charter's control: a false `rooted` sends
protected bytes to a remote provider under a clean provenance, with no pin and
no `privacy_block`. Every one of the four leaks was a regression against main,
introduced by a change whose purpose was to *reduce* false pins. The cost of
the allowlist form is a handful of extra root walks on unusual flags; the cost
of the denylist form is unbounded.

## Applies When

Adding any exemption, fast path, or "obviously harmless" case to a fail-closed
classifier, gate, or sanitizer; reviewing a diff that turns `Unknown` into
`Rooted` for a family of inputs; reading a comment that asserts a flag table
or spelling list is complete; unifying two gates over one parser.
