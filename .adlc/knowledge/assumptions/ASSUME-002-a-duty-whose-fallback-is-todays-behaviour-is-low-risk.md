---
id: ASSUME-002
title: "A duty whose fallback is today's behaviour is low-risk"
status: invalidated
req: REQ-561
created: 2026-08-07
updated: 2026-08-07
component: "daemon/harness"
domain: "routing"
---

## The assumption, as written

REQ-561 stated it plainly:

> `triage` and `shell` are lower-risk than `compact` because their fallbacks are
> the current behaviour verbatim; `compact` replaces a deterministic algorithm
> with a model call and is the one that warrants the most adversarial review.

The review effort was allocated accordingly.

## Why it was reasonable

For a duty that degrades to exactly what the code did before, a *total failure*
is indistinguishable from not having shipped the duty at all. That is a real and
valuable property, and it did hold: every failure path in `triage` and `shell`
returns the prior result verbatim, and the AC-3 matrix proves it across all four
failure conditions.

## Why it was wrong

**It reasons only about the failure path.** The risk it misses is a duty that
*succeeds* and makes things worse.

`triage` ranks grep matches by answering with match *numbers*, which
successfully prevents a model from fabricating file paths or line numbers. But
nothing bounded **omission**: a ranking of `"1"` against 200 matches silently
dropped 199 — and the ranked content is repo text an attacker may influence, so a
grep for a backdoor pattern could be steered to return the one benign hit. Three
reviewers flagged it independently. A successful `triage` was strictly worse than
no `triage`.

`compact` — the duty the assumption pointed the review at — did *not* have an
equivalent, because its risky direction was the one already anticipated and
guarded structurally (ADR-4's unconditional budget gate).

So the assumption inverted the attention: it aimed adversarial review at the duty
whose danger was already designed for, and away from the one whose danger had not
been articulated.

## What replaces it

Rank duties by **what a successful, plausible-looking answer can do**, not by
what its fallback does. Two questions per duty:

1. If this duty fails, what happens? (the fallback question — cheap, usually fine)
2. If this duty *succeeds* with an answer that looks reasonable, what is the worst
   thing that answer can cause? (the question REQ-561 did not ask of `triage`)

A duty that filters, ranks, selects, or summarises can suppress. A duty whose
output merely *annotates* cannot. That distinction predicts risk better than the
shape of the fallback.

## Resolution in REQ-561

Fixed by appending the unranked remainder below the ranked head, making
`render_ranked`'s output a **permutation** of its input rather than a subset —
asserted as set equality in both directions. Chosen over a minimum-coverage
fraction because a floor bounds suppression without removing it: at any honest
fraction, 150 of 200 hits can still vanish and the planted one can be among them.

REQ-561's fourth assumption — that ranking is useful at the 200-match scale at
all — remains **unresolved** and now carries this alongside it.
