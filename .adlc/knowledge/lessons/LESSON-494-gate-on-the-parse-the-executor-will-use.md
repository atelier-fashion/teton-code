---
id: LESSON-494
title: "A security gate and the client that executes the request must share one parser"
component: "daemon/egress"
domain: "privacy"
stack: ["rust", "daemon"]
concerns: ["security", "privacy"]
tags: ["url-parsing", "allowlist", "consent", "parser-differential", "whatwg", "ssrf"]
req: REQ-563
created: 2026-08-09
updated: 2026-08-09
---

## What Happened

REQ-563's web tool decided *where a request was going* twice, with two
different parsers. The allowlist check and the consent prompt used a
hand-rolled authority splitter (`web::host_of`: take everything before the
first `/` or `?`, then the last `@`). The socket used `reqwest`, i.e. the
WHATWG URL standard, which treats `\` as an authority terminator for special
schemes.

For `https://evil.example\@allowed.example/x` the two disagree completely:

| | host |
|---|---|
| `web::host_of` (gate, prompt) | `allowed.example` |
| `url` 2.5.8 (the wire) | `evil.example` |

With `allowed_domains = ["allowed.example"]`, a model-composed URL of that
shape passed the allowlist, rendered `(host allowed.example)` on the Ask
prompt the user approved, and fetched `evil.example` — with the query string
free to carry whatever the model had in context. One backslash defeated both
BR-11 (the allowlist) and BR-4 (consent names the real destination).

## Lesson

**A gate that decides about a request must operate on the same parse the
executor will use — and the string it approved must be the string that is
sent.** "Two parsers that agree on ordinary input" is not a property; every
URL parser agrees on ordinary input, and the ones that matter are the
adversarial spellings where they diverge. A gate on a *different* parse is
not a weaker gate, it is a **bypassable** one: an attacker only has to find
one string the two read differently, and URL standards are a rich source of
those (backslashes, userinfo, tab/newline stripping, IDN, percent-encoded
hosts, trailing dots).

The fix is structural, not a patch: **parse once, at the entry point, with the
executor's parser; derive every downstream decision from that single parsed
value; and hand the executor the re-serialization, not the original bytes.**
Then "the string that was checked" and "the string that is sent" are the same
object, and no divergence can exist to exploit. REQ-563 made the old parser
`#[cfg(test)]`-only so the wrong function does not exist in a production
build — a fence the compiler enforces, not a comment asking politely.

This is the same shape as LESSON-490 (a guard that runs on an encoded form
must be tested against the encoder's output) and LESSON-432 (provenance must
derive from what a tool touched, not from an argument's name): in all three,
the defect is a guard reasoning about a *representation* other than the one
that carries the consequence.

## How to Apply

- When adding any allowlist, denylist, consent prompt, or audit record about
  a URL, path, or address: identify the component that will actually act on
  it and use **its** parser. If that parser is in a dependency, call the
  dependency.
- Pass the canonical re-serialization downstream, never the raw input.
- Test with a differential table, not examples: for each hostile spelling,
  assert what each parser returns and that the gate and the wire agree.
  REQ-563's suite sweeps ~25 spellings and asserts re-serialization is
  idempotent, so a future parser bump that changes normalization fails a test
  instead of quietly reopening the hole.
- If a second parser must survive for a narrower job (here: matching URLs the
  user pasted), fence it so it cannot be reached from a security decision —
  `#[cfg(test)]`, a private module, or a distinct type.
