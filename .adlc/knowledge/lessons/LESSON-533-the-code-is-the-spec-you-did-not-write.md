---
id: LESSON-533
title: "The code is the part of the spec you did not write — read it before the task file, and again at review"
component: "adlc/pipeline"
domain: "devtools"
stack: ["rust", "json-rpc", "cargo"]
concerns: ["security", "developer-experience"]
tags: ["parallel-agents", "fail-fast", "architecture-assumptions", "review-pass", "worktree", "fmt"]
req: REQ-579
created: 2026-08-16
updated: 2026-08-16
---

## What Happened

Four things in one pipeline run:

1. **Three architecture assumptions the code contradicted.** The architecture
   named a wire code `SETUP_REJECTED_NONUSER` (does not exist — the precedent
   answers `NOT_ATTACHED` and emits an event), and the spec's mock spelled the
   config as `[policy.tiers.think]` / `api_key` (the schema has `[[tiers]]` /
   `auth_ref`, no `[policy]` table). All three were caught by task agents who
   read the precedent code *before* implementing, and were corrected in the
   architecture and spec mid-Phase-4 rather than shipped.
2. **The review pass found a real High in a "narrow" seam.** The spec said
   `key_ref` is keychain-only; the daemon gated it on
   `teton_core::is_recognized_auth_ref`, which admits `env:`/`op://`/any
   keychain service because hand-written configs legitimately use them. One
   commit could compose attacker endpoint + `env:<daemon secret>` + a `think`
   binding. Fixed to exact `keychain://teton/<id>` on this seam only.
3. **Cargo's default fail-fast hid two red gates for two rounds** — the
   workspace "passed" while a later target had never run
   (`cargo-test-fail-fast-hides-targets` was already a project memory).
4. **Two agents editing one worktree** — `cargo fmt --all` from one reformatted
   the other's in-progress file; per-crate `fmt -p` is the rule while tasks
   run in parallel.

## Lesson

A spec or architecture sketch is a hypothesis about the code; the code is
ground truth. Two habits kept this run honest: task agents *start* by reading
the precedent they are mirroring and report contradictions instead of working
around them; and the review pass audits a security boundary against the
predicate the code actually calls, not the one the spec named. Operationally:
`cargo test --workspace --no-fail-fast` always (and grep `FAILED`, do not sum
counts — `; ` splits into an empty field and under-counts), and per-crate fmt
while more than one agent has the tree.

## Why It Matters

Each of these fails quietly. A wrong wire code ships a mismatch that no
same-build CI can see; a wider predicate ships a credential path the spec
promised was closed; a hidden red target ships as "green"; a stray fmt
rewrites a colleague's file under them. The cost of catching them was one
line in each agent's brief.

## Applies When

- Writing task briefs for parallel implementers: say "read the precedent
  first; report contradictions, do not work around them".
- Reviewing any seam the spec calls "narrow": find the predicate and read it.
- Any `cargo test` invocation in this workspace, and any summed test count.
- More than one agent editing the same worktree.
