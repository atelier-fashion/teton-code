---
id: LESSON-432
title: "Provenance must derive from what a tool touches, not from an argument name"
component: "daemon/egress"
domain: "privacy"
stack: ["rust", "daemon"]
concerns: ["privacy", "security"]
tags: ["provenance", "boundary-enforcement", "fail-closed", "false-negative-tests", "br-1"]
req: REQ-544
created: 2026-07-21
updated: 2026-07-21
---

## What Happened

The BR-1 privacy guarantee ("local-only content never leaves the machine,
enforced by construction") was implemented by tagging each tool result's egress
provenance from the tool call's literal `path` argument (`path_arg`). Only
`read`/`edit` (and remote MCP) actually carry a `path` argument, so `shell`
(`{command: "cat secrets/prod.env"}`), `grep`, `glob`, and MCP tools whose
boundary argument wasn't named `path` folded boundary content into context with
EMPTY provenance. Egress inspected nothing and forwarded it to a remote provider
on the next turn — no `privacy_block`. The whole acceptance suite passed because
the only tested paths (`read`-with-`path`, remote-MCP) were the two that
happened to be enforced. Three independent review agents converged on it.

## Lesson

Derive a security tag from the actual effect of an operation, never from the
shape of its request. A tool's provenance is the set of files it *read*, which
the tool must report alongside its output — for an opaque operation (`shell`, an
untrusted MCP server) that set is unknowable, so it must be `Unknown` and
fail-closed (block remote egress when any boundary exists), exactly like a known
boundary hit. Pair per-operation tagging with a coarse session-level backstop
(a tainted session is pinned to the local tier) so a missed tag can't leak.

## Why It Matters

This is a false-negative that passing tests actively hide: the guarantee is the
product's flagship claim, the leak is silent (no event, meter still shows
"enforced"), and it's exploitable with a fixture that already shipped in the
repo. A tag keyed on request shape looks correct in the covered case and is
wrong for every uncovered tool — the coverage gap and the security gap are the
same gap.

## Applies When

Enforcing any by-construction guarantee (privacy, taint, capability) where the
enforcement point reads metadata about an operation rather than its effect;
writing tests for a "hard guarantee" (enumerate every tool/path that reaches the
choke point, not just the obvious one); adding a new tool that surfaces external
or file content into an LLM context.
