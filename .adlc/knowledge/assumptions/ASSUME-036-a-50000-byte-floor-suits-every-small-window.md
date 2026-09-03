---
id: ASSUME-036
title: "A 50,000-byte floored budget is tolerable on every provider whose window derives below it"
status: unresolved
req: REQ-612
created: 2026-09-03
resolved:
---

## Assumption

`MIN_BUDGET_BYTES` rose from 16,384 to a pinned 50,000 (REQ-612, product decision
2026-09-03) so a floored route holds the system prompt with the full 8 KiB
repository-notes block under the existing "prompt ≤ half the window" invariant
(measured: 15,370 bytes with the worst-case block, ×2 = 30,740 < 50,000). The
assumption underneath it is that every provider whose derived pair falls below
the new floor can actually accept a 50,000-byte request. Before the raise a route
was floored only when its declared window was under roughly 9,200 tokens; now
any window under roughly 26,000 tokens is floored, and a floored route by design
sends more than its window derives (`teton_docs context`: "a floored route sends
more than its window declares, and on an unpinned provider nothing reports the
overflow").

## Context

The floor exists so the system prompt always fits (REQ-586 BR-2). It never
claimed to fit the *window* — the typed "context length exceeded" outcome is the
backstop for the pinned vendors, and Ollama truncates silently. The raise widens
the set of routes that rely on that backstop. The `floored` mark on the route
line, `/doctor`'s advisory, and REQ-586's docs are the surfaces that say so.

What depends on it: any user who binds a tier to a small-window model (an 8k or
16k local-server model, an older small remote model) and expects the harness to
size requests to it.

## Resolution

Unresolved. Validate by binding a tier to a provider declaring `max_context =
16000` on a pinned vendor and confirming a long turn ends in the typed
`ContextLengthExceeded` outcome with the floored mark on the route line, not in
a silent truncation. If small-window providers turn out to be common, the fix is
a smaller notes cap on floored routes (the quarter rule still exists as a latent
path — `repo_context_cap`), not a lower floor.
