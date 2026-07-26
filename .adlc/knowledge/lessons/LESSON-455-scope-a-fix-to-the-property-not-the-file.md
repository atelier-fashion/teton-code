---
id: LESSON-455
title: "Scope a fix to the property, not to the file the finding cited"
component: "adlc/review"
domain: "process"
stack: ["adlc", "ci"]
concerns: ["security", "process", "reliability"]
tags: ["fix-scoping", "parallel-agents", "file-ownership", "supply-chain", "partial-fix", "re-verify"]
req: REQ-548
created: 2026-07-26
updated: 2026-07-26
---

## What Happened

A security audit raised one Critical: third-party GitHub Actions pinned to
mutable `@v2` tags in the job that builds shipped binaries. The fix pass was
split across three agents by **file ownership** to avoid write conflicts, and
the pinning instruction went to the agent that owned `release.yml` — the file
the finding cited.

That agent did its job perfectly. The Critical stayed open anyway: the same
mutable-tag pattern lived in `deploy-site.yml` (including the action that
handles the GCP credential — the highest-value one in the repo to pin) and
three more times in `ci.yml`. Both files were being edited in the same fix
pass, by the other two agents, neither of whom was told to pin anything.

The re-verify pass caught it. Its verdict — "original Critical, partially open"
— was only possible because it was scoped to the *property* ("is every
third-party action pinned?") rather than to the diff of the file the finding
named. The same pass also caught a commit message claiming a change whose
string-replace had silently matched nothing.

## Lesson

A finding names a location; it rarely names the whole property. Before
dispatching a fix, restate it as an invariant over the repository — "every
third-party action is SHA-pinned", "every credential-bearing step is
environment-gated" — then grep for the property and fix every instance,
regardless of which file or which agent owns it. File-ownership partitioning is
the right way to avoid write conflicts between parallel agents, but it is the
wrong axis to partition *correctness* along: an invariant that spans files gets
split into per-file half-fixes, each locally complete and collectively useless.

Verify the same way. Re-verification that re-reads the cited diff confirms the
patch; re-verification that re-checks the property finds the instances the
patch missed.

## Why It Matters

A partially-closed Critical is worse than an open one: the finding is marked
resolved, the reviewer's attention moves on, and the remaining instances now
carry an implicit "someone looked at this". Here it would have shipped a
release pipeline whose binary-producing job was pinned while the job holding
the cloud credential was not — the reverse of the priority order anyone would
have chosen deliberately.

## Applies When

- Dispatching parallel fix agents partitioned by file (always ask: does any
  finding's property cross the partition?).
- Closing any security finding of the form "X is unpinned / unvalidated /
  ungated **here**" — the word "here" is the trap.
- Writing a re-verify prompt: scope it to the invariant, and make "which of the
  original findings are now closed vs still open" an explicit deliverable (see
  [[LESSON-441]] — a fix pass is new code).
- Claiming a change in a commit message: confirm it appears in the diff. A
  silent no-op edit plus a confident message is an audit trail that lies.
