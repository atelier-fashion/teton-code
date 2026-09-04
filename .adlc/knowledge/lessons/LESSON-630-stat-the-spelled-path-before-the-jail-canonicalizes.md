---
id: LESSON-630
title: "Stat the spelled path before the jail canonicalizes it"
component: "tetond/repo_context"
domain: "security"
stack: ["rust"]
concerns: ["security", "privacy"]
tags: ["symlink", "lstat", "canonicalize", "toctou", "evidence"]
req: REQ-613
created: 2026-09-04
updated: 2026-09-04
---

## What Happened

The drafting evidence gatherer asked the tool jail to resolve each table member (`README.md`, `Cargo.toml`, …) and then `stat`ed the resolved path to refuse symlinks. `ToolContext::resolve` canonicalizes, so the `stat` was an `lstat` of the link's target: a repository containing `README.md -> .env` answered "regular file, not a symlink" and shipped `.env`'s bytes to the model under the heading `### README.md`. The guard was dead code, and its test had never planted an in-repo symlink so it never noticed. REQ-612's loader had the right order one module over.

## Lesson

An entry rule (symlink, FIFO, regular-file, hardlink) must run on the path **as spelled**, before any canonicalizing resolve, and the identity that `lstat` minted must travel into the read so a link planted between the two fails the dev/ino check. Canonicalize afterwards, for containment and provenance. And a symlink guard's test must plant a symlink that stays inside the root, because the outside-the-root case is caught by containment and proves nothing about the guard.

## Why It Matters

A seam that reads repository content into a model prompt is a privacy boundary. A dead guard there is silent: nothing fails, the bytes just leave.

## Applies When

Any code that opens a repository path by name after resolving it through a jail, a canonicalizer, or a path-normalizing helper.
