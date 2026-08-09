---
id: LESSON-497
title: "A test fixture that looks like a real credential blocks the push that ships it"
component: "daemon/egress"
domain: "privacy"
stack: ["rust", "ci", "github-actions"]
concerns: ["security", "developer-experience"]
tags: ["test-fixtures", "secret-scanning", "push-protection", "sentinels", "history-rewrite"]
req: REQ-563
created: 2026-08-09
updated: 2026-08-09
---

## What Happened

REQ-563's acceptance suite plants a credential in a search query to prove the
redaction scan blocks it before the wire. The fixture was spelled like a live
Stripe key: `STRIPE_API_KEY=sk_live_51H8x…`. Entirely synthetic — but shaped
closely enough that GitHub's push protection recognized it.

The block landed at the **worst possible moment**: after the feature was
implemented, reviewed by six agents, fixed across five waves, and had a green
local suite. `git push` was rejected, and because the literal was baked into
seven commits, fixing the working tree was not enough — the branch history had
to be rewritten (`filter-branch` over the unpushed range, full suite re-run,
force-push with lease) before a single byte could reach the remote.

The repository already had the answer. Its own house convention is obviously
synthetic sentinels — `AKIASENTINEL562ABCDE`, `sk-ZZQUUXSENTINELCREDENTIAL0123`,
`AKIAMCPWIRESENTINEL0` — visible in the very test files this REQ sat beside.
The new fixture simply didn't follow it.

## Lesson

**Plant sentinels, not lookalikes.** A credential fixture has exactly one job:
match the *shape* your scanner keys on. It does not need to be plausible as a
real key, and plausibility costs real money — forge-side secret scanning is
shape-based too, so a convincing fixture is indistinguishable from a leak to
every automated reader between you and `main`.

Note what the test was actually about: the production pattern pass catches the
`KEY=value` **assignment layout** (the shape LESSON-490 found it blind to).
The value's Stripe-ness contributed nothing to the assertion — it was pure
decoration that happened to be expensive. When a fixture's realism is not
load-bearing for the assertion, realism is a liability.

The cost profile is what makes this worth writing down. The defect is trivial
(one string), the fix is trivial (one string), and the *remediation* is not:
history rewriting, a full re-verification run, and a force-push, executed
under time pressure at the end of a pipeline, on a branch someone might
already have fetched.

## How to Apply

- Spell every planted credential so a human reads "sentinel" at a glance:
  embed the word, keep it obviously non-random, and keep it just long enough
  to match the pattern under test.
- Grep the neighbouring test files for the project's existing sentinels before
  inventing one. There is almost always a convention already.
- Route the fixture through **one** const and have every assertion reference
  it, so the value and its assertions cannot drift and a future change is one
  edit.
- Catch it early: a pre-commit or CI grep for real-provider key shapes in test
  files costs nothing and moves this from "blocked at merge, rewrite history"
  to "blocked at commit, change a string."
