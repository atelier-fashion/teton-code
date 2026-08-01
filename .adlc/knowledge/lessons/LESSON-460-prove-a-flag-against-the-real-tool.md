---
id: LESSON-460
title: "A gate wired to an unverified CLI-flag format fails on every input — and a fixture written from imagination blesses it"
component: "distribution/release"
domain: "testing"
stack: ["github-actions", "ci", "bash"]
concerns: ["security", "reliability", "testing"]
tags: ["signer-workflow", "seam-fixture", "cli-contract", "gh-attestation", "empirical-verification", "fix-pass"]
req: REQ-550
created: 2026-08-01
updated: 2026-08-01
---

## What Happened

REQ-550's verify-fix pass hardened the attestation gate with
`gh attestation verify --signer-workflow .github/workflows/release.yml`. The
flag actually requires `<owner>/<repo>/<path>`: gh compiles the value into the
SAN regex `^https://github.com/<value>`, so the bare path form could never
match a real certificate and every release would have died at a permanently
red gate. The selftest could not catch it, because its `gh` stand-in was
written from the same imagination: it accepted any `--signer-workflow` value
and emitted a fictional SAN without the `https://github.com/<owner>/<repo>/`
prefix. Two other instances in the same REQ had the same shape:
`REJECTION_PATTERN` was written from guessed gh error strings (live gh
prints `Error: no attestations found`, and a pre-first-release repo 404s),
and a comment claimed a signer-workflow filter-to-nothing prints "no matching
attestations" (it prints `Error: verifying with issuer "sigstore.dev"`).
Every one was caught only when a Step-D re-verify agent drove the *real*
tool against a *real* attested artifact and ran the repo's own gate script
against live output.

## Lesson

Before wiring a CLI flag, error string, or output format into a gate, prove
it against the real tool — one invocation against a known-good and a
known-bad input beats any amount of plausible reading. And when a gate is
tested through a stand-in seam, the fixture's output must reproduce the real
tool's observed shapes (exact error strings, full SAN/URL formats), not the
author's paraphrase: a fixture that mirrors the author's assumption
mechanically cements the assumption, and the suite then proves only that the
code agrees with itself. Record the observed evidence (tool version, exact
output) in a comment at the pattern so the next editor widens it from
observation too.

## Why It Matters

The failure mode is a gate that is either unpassable (here: 100% of releases
dead, with an UNCHECKED message actively blaming the wrong cause) or
silently weaker than claimed — and a green suite vouching for it. It
survived one full review round because reviewer and author shared the
assumption; only empirical adversarial verification (LESSON-441's re-verify
posture) broke the loop.

## Applies When

- Wiring any external CLI's flags/exit codes/output strings into automation
  (gh, gcloud, security, codesign, brew — anything whose contract you did
  not read from its parser source).
- Writing stand-in fixtures for a tool seam: copy observed output verbatim,
  note the tool version, and include one case asserting the *shape* the
  real integration depends on (e.g. the owner/repo prefix).
- Reviewing a fix pass that added a pattern match on tool output: ask "was
  this string observed or imagined?" (see [[LESSON-441]], [[LESSON-454]]).
