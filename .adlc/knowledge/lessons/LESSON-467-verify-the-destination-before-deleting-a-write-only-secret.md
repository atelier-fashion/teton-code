---
id: LESSON-467
title: "Migrating a write-only secret: verify the destination holds it before deleting the source — green with both copies identifies neither"
component: "distribution/release"
domain: "security"
stack: ["github-actions", "ci"]
concerns: ["security", "reliability", "process"]
tags: ["secret-migration", "environment-secrets", "write-only", "tap-token", "post-deletion-verification", "github-environments"]
req: REQ-550
created: 2026-08-03
updated: 2026-08-03
---

## What Happened

REQ-550's runbook §11 retired the repository-scoped `HOMEBREW_TAP_TOKEN` in
favor of an environment-scoped copy. The user had been asked to paste the
token into the `tap-publish` environment during setup and reported it done —
but only the `release-signing` values had actually landed; the tap-publish
paste silently never happened. Two releases then ran green, resolving the
token from the repository copy — indistinguishable from environment
resolution, because GitHub prefers the environment copy *when it exists* and
no log line says which one served. The deletion step ran on the false belief
both copies existed. What caught it: an immediate post-deletion API listing
(`gh api repos/<r>/environments/tap-publish/secrets`) showed
`total_count: 0` — before any release could fail. Remediation was cheap
precisely because secrets are write-only anyway: mint a fresh fine-grained
PAT (nothing references the old *value*), paste it as the environment
secret, and prove resolution by re-running the already-green release's
`bump-formula` job with the environment as the only copy.

## Lesson

When migrating a write-only secret between scopes: (1) verify the
destination *by listing* (names and update timestamps are readable even
though values aren't) immediately before AND immediately after deleting the
source — human "done" reports and green runs are both unreliable witnesses,
since a run that can fall back to the source proves nothing about the
destination; (2) sequence the positive proof as its own step *after* the
deletion (re-running an idempotent consumer job is the cheap way); and
(3) remember the recovery property: because the value was never readable,
a lost secret costs only a re-mint and re-paste — never block on recovering
an old value.

## Why It Matters

The failure lands at the worst time: the next release, in the one job that
needs the credential, with an error that reads as infrastructure rather
than as a three-week-old missed paste. The whole cost of prevention is one
API listing. This incident validated §11's deliberate step ordering — and
showed the listing must happen even when every prior signal says the copy
exists.

## Applies When

- Retiring or moving any write-only secret (GitHub secrets, CI variables,
  cloud secret managers) between scopes or stores.
- Interpreting a green run as evidence of secret resolution when more than
  one resolvable copy exists ([[LESSON-461]]'s cousin: green about the
  wrong source).
- Designing migration runbooks: destination-listing before deletion,
  positive re-run proof after ([[LESSON-464]] — the check must be a step,
  not an assumption).
