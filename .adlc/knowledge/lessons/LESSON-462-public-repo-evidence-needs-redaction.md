<!--
Filename MUST be `LESSON-xxx-slug.md` (e.g., `LESSON-041-signed-url-ttl-mismatch.md`).
-->
---
id: LESSON-462
title: "Public-repo spec evidence must reference secrets, not restate them — GitHub pushes are effectively permanent"
component: "distribution/release"
domain: "adlc"
stack: ["github-actions", "gcp", "static-site"]
concerns: ["security", "documentation"]
tags: ["redaction", "public-repo", "force-push", "orphaned-commit", "oq-closeout", "adr-548-3"]
req: REQ-548
created: 2026-08-03
updated: 2026-08-03
---

## What Happened

Closing REQ-548's OQ-5 "against evidence" (as `docs/site-deploy-runbook.md` §1
instructed) meant writing the landed GCP coordinates into the requirement file.
The first pass recorded them literally — project id, project number, global IP,
and service-account email — in a public repository, directly contradicting
ADR-548-3, which deliberately keeps the project id in a repo *secret* so it
never appears in public logs. The runbook's own intake instruction steered the
work into the leak: "record the answers in REQ-548's requirement file" said
nothing about redaction.

Recovery was three separate operations, none complete on its own: a
force-push (the old commit stays fetchable by SHA, and the PR's force-push
timeline event advertises that SHA publicly), deleting the PR body's edit
revision (UI-only), and a GitHub Support ticket to garbage-collect the orphaned
commit — which Support may decline because its documented purge process covers
*sensitive data*, and infrastructure identifiers may not qualify.

## Lesson

In a public repo, spec evidence records **where the authoritative value
lives**, not the value: "project id is in the repo secret `GCP_PROJECT`" closes
an open question just as well as the id itself. Values already public on `main`
(the runbook's canonical resource names) or trivially discoverable (`dig NS`)
need no redaction — check what's already exposed before deciding what to
scrub. And treat every push to GitHub as permanent at the moment of push:
orphaned commits, PR timelines, and body-edit histories all outlive a
force-push, so the redaction pass happens *before* commit, not after.

## Why It Matters

A leak that takes one `git push` to create takes a force-push, a UI cleanup, a
support ticket, and an uncertain GC to mostly-undo — and the residual (targeted
phishing against named infrastructure) never fully clears. The repo's own docs
can be the injection vector: an instruction written when the repo's audience
was assumed private ("record the answers here") silently becomes a leak
procedure once the repo is public.

## Applies When

Recording infrastructure evidence, OQ closeouts, or deploy runbook intake in
any public (or later-to-be-public) repository; writing runbook instructions
that tell a future operator to "record" operational values; assessing whether
force-push actually removed something from GitHub.
