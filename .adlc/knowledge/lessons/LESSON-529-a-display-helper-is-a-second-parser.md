---
id: LESSON-529
title: "A display helper is a second parser — render the host the request will reach"
component: "cli"
domain: "providers"
stack: ["rust", "cli"]
concerns: ["security", "developer-experience"]
tags: ["display-vs-dial", "url-parsing", "redaction", "confirmation-loop", "userinfo", "req-578"]
req: REQ-578
created: 2026-08-15
updated: 2026-08-15
---

## What Happened

REQ-578's first verify fix batch added `displayed_endpoint`, a small helper
masking URL userinfo in the credential-deciding echo line ("endpoint stored
as X — that exact URL is what Teton will POST"). The Step-D confirmation
re-review found the helper had *introduced* the very defect class the batch
was closing: it ended the URL authority at `/?#` while WHATWG (and the url
crate at dial time) also ends it at `\`. For
`https://evil.example\@127.0.0.1/v1`, the mask read everything before the
last `@` as userinfo and rendered `https://***@127.0.0.1/…` — the one line a
user consults before typing a key showed **loopback** while the key went to
`evil.example`. The pre-fix code, printing the string verbatim, had been
safer: the attacker host was at least visible. A Unicode sweep found U+005C
was the single divergent code point — one character between a redaction
feature and a spoofing primitive.

## Lesson

Anything that extracts structure from a string in order to display it — a
masker, a highlighter, a truncator — is a parser, and if the string is later
consumed by a different parser, every divergence between the two is a lie on
screen. When the displayed claim is load-bearing ("this is what will be
dialed"), the display parse must provably agree with the dialing parse: reuse
the same authority-splitting rule (REQ-578 gave the helper the sibling's
`\`-terminator), refuse inputs where the two available parses name different
hosts (the shape gate), and pin the property itself — "rendered host equals
dialed host" — not just the masking behavior. Corollary from the same review
round: a test that *re-enacts* a flow in its own body (build the value, call
the assembler, assert) pins nothing about the production wiring — the
mutation "echo the settled value, register the raw one" survived the whole
suite until the post-settle sequence was extracted into a function the test
and the flow both call ([[LESSON-519]]'s seam rule, output-side).

## Why It Matters

The echo-before-credential pattern is only as strong as the honesty of the
rendered string; a divergent display parse converts the mitigation into an
attack amplifier, with the user's trust pointed at the wrong host. And the
defect arrived *in a security fix batch* — which is precisely what
confirmation re-reviews exist for: the round that found this also found the
mirror bypass (LESSON-528), both in code written to close findings from the
round before.

## Applies When

Writing any redaction/formatting helper for values another component parses
(URLs, paths, addresses); asserting "what you see is what happens" anywhere —
name the both-parses-agree property and pin it; reviewing a fix batch — the
new helpers it adds are new attack surface, review them at the same depth as
the code they fix. See [[LESSON-524]] (the sibling exposure/callability
split) and [[LESSON-475]] (derive behavior from the emitter, not the spec).
