---
id: LESSON-651
title: "A test harness's isolation choice encodes an assumption about what is durable — and hides the store that is not"
component: "daemon/lifecycle"
domain: "testing"
stack: ["rust", "daemon", "linux"]
concerns: ["reliability", "test-coverage"]
tags: ["xdg", "state-directory", "e2e-harness", "isolation", "restart", "bug-211", "req-611"]
req: REQ-611
created: 2026-09-05
updated: 2026-09-05
---

## What Happened

REQ-611 gave the e2e harness a **fresh** `XDG_DATA_HOME` per daemon spawn so
two daemons in one test could never prune each other's transcripts. That was
fine only because nothing else lived there: `cost.db`, `config.toml`, the
model decision and the weights all sat beside the socket under
`XDG_RUNTIME_DIR`, which the harness kept stable per workspace. Every restart
test in the consent suite passed *because* the durable stores were in the
wrong directory. When BUG-211 moved them to the data directory, the per-spawn
isolation would have made a restarted daemon forget its weights and its
recorded decision — the harness had to change to a stable data directory per
workspace before the fix could land, and the "two daemons prune each other"
worry turned out to be theoretical: the prune only removes files past
`retain_days`, and no e2e test asserts on transcript counts.

The same shape on the CLI side: `teton uninstall`, `teton model status` and
the service-decline marker all derived "the state directory" from the socket's
parent, so the docs' answer to "where is my config?" was a directory Linux
deletes at logout, and it was accurate.

## Lesson

When a harness isolates a directory per spawn, it is asserting that nothing a
restart depends on lives there. Write that assertion down where the isolation
is chosen, and check it against the daemon's actual store list, not against
the one store the REQ is adding. Conversely, when moving a store, expect the
harness to be the first thing that breaks — and if it does not, ask whether
the restart tests are exercising the new location at all.

Keep runtime and durable directories as two named things from the start
(`DaemonPaths::data` beside `socket`), and derive both in one place the CLI
and the daemon share. A single `base_dir` argument is how five stores
inherited a tmpfs.

## Why It Matters

On Linux a logout silently took the cost history, the config the user edited
and a multi-gigabyte download they consented to. The bug was filed by the REQ
that noticed it and sat for two days as "a follow-up", with a green suite the
whole time.

## Applies When

- Adding a per-test or per-spawn isolation directory to a harness.
- Adding a new store to a daemon: name which of the two directories it
  belongs in, and add it to `state_dir::DURABLE_ENTRIES` if it must survive
  a logout (the migration list is the durable-store list).
- Any REQ whose ADR files a "silent migration" as a follow-up: the follow-up
  needs a migration function, a keep-both rule and a cross-device copy arm,
  not a resolver swap.
