---
id: LESSON-539
title: "Claim first, then re-read — session state snapshotted before the turn claim is stale by construction"
component: "daemon/session"
domain: "harness"
stack: ["rust", "daemon", "tokio"]
concerns: ["reliability", "privacy"]
tags: ["toctou", "turn-claim", "session-root", "cwd", "set_cwd", "race", "req-583"]
req: REQ-583
created: 2026-08-19
updated: 2026-08-19
---

## What Happened

REQ-583's `session/set_cwd` moves a live session's jail root under the turn
claim (`try_begin_turn`), so a running turn cannot have its root moved
underneath it. The verify pass found the hole on the other side: `session/prompt`
snapshotted `SessionSummary.cwd` in `spawn_prompt_turn`, **then** spawned the
task, and only inside the task did `run_prompt_turn` take the claim. A `/cd`
landing between snapshot and claim succeeded (no claim was held yet), moved the
root and cleared the conversation — and the turn then ran jailed to the *old*
root, with an environment block naming it, and committed its blocks into the
just-cleared conversation. Every test stayed green because every test built the
turn from the same value it asserted on.

## Lesson

Session state read before a claim is taken is a *hint*, not a fact. The order
is **spawn → claim → re-read → use**: once the claim is held, re-read the
registry's authoritative value and derive everything (jail, probed view,
prompt) from that one read; keep the pre-claim parameter only as a fallback for
a session that vanished. Then pin it with a test that stages the interleaving
(mutate between snapshot and claim) and asserts the turn saw the post-mutation
value — a test that builds the turn from the asserted value proves nothing.

## Why It Matters

Provenance identities are root-relative: a turn on the wrong root reads files
under one root and records identities judged under another — the exact hazard
the `/cd` clear exists to prevent. The window is microseconds and self-inflicted
(two of the user's own clients), but the invariant the docs stated ("the jail
cannot move under a turn") was simply false, and nothing would have said so.

## Applies When

Any async handler that (a) snapshots mutable session state, (b) spawns a task,
and (c) takes a turn/lock claim inside that task — cwd, title, permission
grants, taint, anything a second RPC can change. Also when writing the test:
build the turn from a deliberately stale snapshot and assert the fresh value.
