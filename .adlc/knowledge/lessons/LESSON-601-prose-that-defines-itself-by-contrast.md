---
id: LESSON-601
title: "Prose that defines itself by contrast breaks when the thing it contrasts with changes"
component: "ci"
domain: "maintainability"
stack: ["github-actions", "yaml"]
concerns: ["maintainability", "developer-experience"]
tags: ["comments", "documentation-drift", "blast-radius", "cross-reference", "grep-for-mentions"]
req: REQ-605
created: 2026-08-31
updated: 2026-08-31
---

## What Happened

REQ-605 changed one line in `ci.yml` so a newer push no longer cancels an
in-flight run. The behavioural blast radius was exactly that line.

The *prose* blast radius was not. Two sibling workflows explained their own
settings by pointing at `ci.yml`:

- `release.yml:22` — "**Unlike ci.yml**, an in-flight run is NOT cancelled by a
  newer one."
- `deploy-site.yml:60` — "**Unlike ci.yml**, an in-flight run is NOT cancelled by
  a newer one: …"

Both sentences became false on the commit that landed the change, in files the
change never touched. Nothing would have caught it: `actionlint` and `shellcheck`
both pass, no test reads a comment, and the diff for those files was empty. The
files were found only by running `grep -rn "ci\.yml" .github/ docs/ tools/`
before implementing — looking for *mentions*, not callers.

The distinction was still real, it had moved: `release.yml` and `deploy-site.yml`
**queue** same-group runs; `ci.yml` now runs distinct commits **concurrently** and
cancels only same-commit duplicates. Both comments were rewritten to say that,
each keeping its own reason for `cancel-in-progress: false`.

The same trap then caught the REQ's own artifacts: its Description cited
`ci.yml:10-12`, and the change moved the block to line 31, so the citation
pointed at comment text by the time it merged.

## Lesson

**A cross-reference is a dependency the compiler cannot see, and "Unlike X" is
the most fragile form of it** — it encodes a claim about *another file's current
behaviour*, so it rots when that file changes rather than when this one does. The
file that breaks is not the file you edited, and it shows up in no diff.

Before changing a documented behaviour, grep for **mentions** of the thing being
changed, not just its callers: the filename, the setting name, the symbol. Check
line-number citations too — they drift when a comment above them grows.

Where possible, prefer prose that states its own reason over prose that borrows
one by contrast. "Runs queue here because a half-published release is worse than
waiting" survives any change to `ci.yml`; "unlike ci.yml" does not.

## Why It Matters

The failure is silent and durable. A stale contrast does not merely fail to help
— it actively misinforms, and it is *more* trusted than ordinary comments because
it reads as a deliberate cross-file design note. The next person reasoning about
release-pipeline concurrency would have been told, confidently and in-repo, the
opposite of what CI now does.

This is [[LESSON-599]]'s hazard arriving from the other direction. There, a
mechanical rename over-reached into strings and comments. Here, a behaviour change
under-reached and left comments behind. Both are the compiler-invisible half of a
change; both need the prose diffed deliberately.

## Applies When

Changing any documented default, setting, or behaviour that other files, runbooks
or READMEs describe — especially one whose name is greppable (`ci.yml`,
`cancel-in-progress`, a flag, an env var). Reviewing a diff that changes behaviour
in one file and touches no docs. Writing a comment that begins "Unlike…",
"Whereas…", or "In contrast to…" — that phrasing is the smell. And citing a line
number in a spec for a file the same change is about to edit.
