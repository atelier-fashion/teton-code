---
id: LESSON-459
title: "A gate proves only what it exercises — the platform CI installs is the only platform verified"
component: "distribution/release"
domain: "distribution"
stack: ["ci", "homebrew", "bash"]
concerns: ["reliability", "security", "testing"]
tags: ["verification-asymmetry", "head-vs-hash", "checksum-pairing", "retry-topology", "idempotency", "cross-platform"]
req: REQ-548
created: 2026-08-01
updated: 2026-08-01
---

## What Happened

Auditing the release pipeline after v0.1.1 turned up two gates that looked
thorough and proved less than they appeared to.

**The checksum pairing.** The formula ships three `url` + `sha256` pairs, one
per platform. `render-formula.sh` builds the URLs and the digests in two
independent passes, tied together only by matching variable names —
`url_x64_linux` beside `sha_x64_linux` — so a mispairing is one edit away. The
gate that was supposed to catch it ran `curl -fsSIL`: a HEAD request. HEAD
proves a URL serves *something*; it cannot prove the digest written beside it
describes that something. The only pair ever actually proven was arm64
macOS — because arm64 is the one platform the pipeline runs `brew install` on.
A mispaired Linux digest would have passed every gate green and failed on a
user's machine at install time.

**The retry topology.** `gh release create` errors on an existing release. The
publish is the *cheap, early* half of a release; the tap push, the audit, the
install and the service checks are the *expensive, fallible* half that runs
after. So the one thing guaranteed to be already done when a release fails was
the one thing that made re-running impossible — v0.1.1 had to delete a
published release to retry.

## Lesson

**Ask of every gate: what does it actually exercise, and what does it merely
observe?** Reachability is not correctness; presence is not pairing; a
`--version` that prints is not an artifact that installs. Where a cheap probe
stands in for the real operation, name the gap or close it — here, fetching
and hashing each artifact costs seconds and converts an observation into a
proof.

**Then ask which platforms the answer covers.** A verification asymmetry —
one platform installed in CI, three shipped — silently downgrades every other
platform's guarantee to "we built it." That asymmetry is invisible in a green
run, because the green comes from the platform that *is* covered.

**And check the retry topology of any multi-stage publish**: if an early stage
has an irreversible side effect and a later stage can fail, the pipeline is
un-retryable by construction. Make the early stage idempotent (re-upload,
clobber, upsert) — and distinguish a *retry* from a genuine conflict, so
re-running is safe but re-pointing published bytes at different content is not.

## Why It Matters

Both failures are invisible while the pipeline is green, and both surface at
the worst possible moment: on a user's machine at install time, or during an
incident when the fix is blocked on deleting something already published.
A gate that cannot fail is indistinguishable from a gate that passes.

## Applies When

- Reviewing any release/publish pipeline: list the gates, and for each write
  down what it *proves* versus what it *observes* (see also [[LESSON-454]] —
  a gate whose harness supplies the pass signal).
- Shipping N platforms while CI exercises fewer than N — say which are proven
  and which are merely built ([[LESSON-433]] is the same rule for test runs).
- Any multi-stage pipeline where an early stage publishes, uploads, tags, or
  otherwise leaves a durable artifact before later stages can fail.
- Choosing between `HEAD`/`--dry-run`/`--check` and doing the real thing: the
  cheap probe is right only when you can name what it does not cover.
