---
id: LESSON-473
title: "A daemon's cwd is never the client's: workspace paths must ride the session protocol"
component: "tetond/server"
domain: "agent-loop"
stack: ["rust", "launchd", "jsonrpc"]
concerns: ["correctness", "performance"]
tags: ["cwd", "launchd", "tool-jail", "session-protocol", "repo-root"]
req: BUG-147
created: 2026-08-03
updated: 2026-08-03
---

## What Happened

The daemon resolved its tool jail from `TETON_REPO_ROOT` falling back to *its
own* working directory. Under launchd (brew services) that is `/`, so every
session from every terminal was jailed to the filesystem root: `read
README.md` failed (`/README.md`), `pwd` printed `/`, and `find . -name
"*.rs"` crawled the entire disk until the shell tool's 30-second timeout
killed it. The session protocol (`session/create`) had no field for the
client's directory, so the right value never reached the daemon (BUG-147).

## Lesson

In any client/daemon split, per-workspace paths are **session state supplied
by the client**, never daemon environment. The client must send its cwd at
session creation; the daemon must validate it (absolute, exists) and scope all
file operations to it. A daemon-side default is only a fallback for legacy
clients — and should be conspicuous in listings/logs so a misconfigured
session is diagnosable at a glance.

## Why It Matters

The failure is silent and misleading: tools "work" (they run, return errors or
slow scans) so the symptom surfaces as model confusion or hangs rather than a
configuration error. One missing protocol field made every tool call in every
session wrong — and in this codebase it also fed the hallucination loop,
because the model invented results for the files it could not read.

## Applies When

Designing or reviewing any daemon/service that executes file operations on
behalf of interactive clients (session protocols, IDE backends, LSP-style
servers); debugging "file not found" or pathologically slow file scans in a
service that works fine when run manually from a repo checkout.
