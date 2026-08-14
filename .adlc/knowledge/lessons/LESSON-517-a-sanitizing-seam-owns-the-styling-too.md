---
id: LESSON-517
title: "A sanitizing seam owns the styling too — and the seam is the only ground truth for parity"
component: "cli/prompt"
domain: "clients"
stack: ["rust", "cli", "json-rpc"]
concerns: ["security", "developer-experience", "test-determinism"]
tags: ["sanitization", "defusing", "sgr", "prompt-rendering", "parity-fixtures", "e2e", "seam"]
req: REQ-573
created: 2026-08-14
updated: 2026-08-14
---

## What Happened

REQ-573's verify pass found that daemon-supplied text (the RPC-carried auth
template) reached the prompt writer without `defused()` — the one path to the
terminal the `Surface` sanitizer did not cover. The fix defused every
`Prompter` question at the seam. The confirmation pass then found what the fix
had broken: `main.rs` hand-composed the interactive entry chevron's SGR
(`\x1b[36m›\x1b[0m`) *into the question string*, so the sanitizer — doing
exactly its job — shredded the tint into literal `[36m› [0m` debris. No test
caught it; every existing question fixture was escape-free.

On the same branch, a second variant of the same shape: the CLI's hand-copied
parity fixture (`shipped_catalog()`) drifted from the daemon's catalog within
three commits (a `notes` field added on one side only). Two independently
maintained goldens both stayed green while the "byte parity" claim was false;
a human caught it.

## Lesson

**When a seam gains a sanitizer, the seam must also take over every legitimate
use of the alphabet it destroys.** Styling that used to travel through the
seam as caller-composed escapes has to move *inside* it, applied after
sanitization — the chevron's tint now lives in `FramedStdinPrompter`, and
callers hand in plain text. Grep the callers for the sanitized alphabet before
shipping the sanitizer; the collision is mechanical to find and silent to
ship.

**And for anything two sides must agree on, the seam is the only ground
truth.** Twin goldens (daemon pins its bytes, client pins its copy) each
verify their own author. The assertion that holds is the one on the bytes that
actually cross: the e2e walkthrough now pins the three rendered rows against a
real spawned daemon, which is what makes fixture drift a red test instead of a
reviewer's catch.

## Why It Matters

Both failures are green-suite failures: the debris shipped past 516 passing
tests, and the fixture drift survived byte-parity tests that kept claiming
parity. Security hygiene colliding with presentation, and duplication
masquerading as verification, both hide in exactly the places the suite looks
strongest.

## Applies When

Adding sanitization/neutralization to any shared writer (terminal, log, wire
frame) — enumerate callers that legitimately used the destroyed alphabet
first. Hand-maintaining a fixture that mirrors data owned by another crate or
process — ask what test fails when the copies diverge; if the answer is
"none", pin the seam instead (see [[LESSON-456]]: one classifier per fact,
never two).
