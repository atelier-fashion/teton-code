---
id: LESSON-490
title: "A guard that runs on an encoded form is tested against the encoder's output"
component: "daemon/egress"
domain: "privacy"
stack: ["rust", "daemon"]
concerns: ["security", "correctness", "verification"]
tags: ["json-escaping", "word-boundary", "pattern-matching", "confirmation-loop", "encoding"]
req: REQ-562
created: 2026-08-08
updated: 2026-08-08
---

## What Happened

REQ-562's pattern pass scans the outbound request body — the JSON-serialized
wire bytes, by design (ADR-1: scan the thing that is sent). A review found the
credential shapes matched mid-word (`disk-encryption-configuration` tripped the
`sk-` shape, a High finding, a blocked turn). The fix required a left word
boundary: a match counts only when the preceding byte is outside the shape's
alphabet.

The fix was tested against raw text and shipped green. The Step-D confirmation
pass then reproduced, with executable evidence, that it had **silently disabled
three of the four credential shapes for the most common credential layout**: in
a JSON body, a content newline is the two bytes `\` `n` — and the letter `n` is
a word byte. Every credential at the start of a content line (`cat .env`
output, one key per line) was now preceded by a "word character" and skipped.
Only `AKIA` survived, because its alphabet excludes lowercase.

The final predicate treats the last byte of a JSON string escape (`\n`, `\t`,
`\r`, `\b`, `\f`, and `\uXXXX` decoding to a non-word character, behind an
odd-length backslash run) as a boundary — and its fixture serializes a
multi-line body with `serde_json` and scans those bytes, not hand-written text.

## Lesson

A rule about *content* that executes against an *encoding* of that content must
be specified and tested against the encoder's output. "Preceded by a
non-word character" is a claim about raw text; the scanner never sees raw text.
Every fixture written by hand in raw form tested a representation the
production path never receives.

The confirmation loop, not the original review, caught it — the regression was
*introduced by a reviewed fix*, which is LESSON-488's shape one layer up:
recognising a hazard (mid-word matches) does not inoculate the fix against
creating the inverse hazard (escape letters read as words).

## Why It Matters

The failure direction was maximally quiet: fail-open, on the feature's headline
case, in the only pass with blocking power, while the suite stayed green —
every existing fixture placed credentials mid-sentence after a space. A privacy
feature that users opt into and pay latency for was structurally blind to
`KEY=...` lines, and nothing could tell anyone.

## Applies When

- Any validator, sanitizer, or matcher that runs on serialized/escaped/encoded
  bytes (JSON, URL-encoding, base64 headers, shell quoting) while its rule is
  stated in terms of the decoded content. Build at least one fixture through
  the real encoder.
- Reviewing a precision fix to a detector: ask what the recall cost is in the
  representation production actually scans, not the one in the test file.
- Confirmation passes after review fixes (LESSON-488): re-review the fixes with
  the same adversarial budget as the original diff.

Related: [[LESSON-488]], [[LESSON-485]], [[LESSON-432]].
