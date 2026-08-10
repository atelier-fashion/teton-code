---
id: ASSUME-005
title: "One prefix-cache slot suits the dominant usage pattern"
component: "inference/local"
domain: "inference"
stack: ["rust", "llama.cpp"]
req: REQ-564
status: unvalidated
created: 2026-08-10
updated: 2026-08-10
---

## The Assumption

REQ-564 ships a **single** prefix-cache slot per loaded engine (BR-3). This
assumes the dominant pattern is one interactive session at a time, so a second
concurrent session is rare enough that thrashing the slot costs little.

## Why It Might Be Wrong

The daemon is explicitly multi-client (BR-4, ADR-002): sessions outlive any
client and several clients may attach at once. Two agent sessions alternating
turns thrash the slot completely — every turn reports `session_switch` and
cold-prefills, so both sessions pay the full prefill *and* the cache pays its
bookkeeping for nothing. That is correct (proved by the interleaved-session
acceptance test) but strictly worse than no cache at all for that pattern.

REQ-565's exit-on-last-client lifetime makes the single-session case more
likely, which cuts the other way.

## How To Validate

The `prefix_cache` event carries every miss reason. Once dogfooding produces
real event volume, count `session_switch` misses as a fraction of all local
turns. A non-trivial fraction invalidates this and promotes OQ-2 (make the slot
count a config value, default 1) from deferred to scheduled.

## What Changes If It's Wrong

Multi-slot LRU, which the architecture deliberately left out of scope. The
`PrefixCacheState` type is per-slot already, so the change is a map of them
keyed by session plus an eviction policy — the plumbing does not have to move.
