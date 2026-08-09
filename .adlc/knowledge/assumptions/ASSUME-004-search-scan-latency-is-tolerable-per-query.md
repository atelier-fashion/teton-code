---
id: ASSUME-004
title: "A local-model redaction scan on every search query is fast enough that users keep the search tier"
status: unresolved
req: REQ-563
created: 2026-08-09
resolved:
component: "daemon/egress"
domain: "privacy"
---

## Assumption

REQ-563 BR-14 hard-couples the search tier to the redaction scan: enabling
search enables scanning, with no configuration that yields one without the
other. The spec assumed the cost is acceptable because "queries are short" —
a search query is tens of bytes where a provider payload is kilobytes.

## Context

The coupling is deliberate and the strongest privacy property the search tier
has: a free-text query composed by the model, sent to a third party that logs
it, is the highest-leak-risk egress in the product, and the scan is the only
guard that catches secrets living *outside* declared privacy boundaries (a
pasted key, a stray `.env`). The user chose the hard couple over a soft one
during verify (decision 2/1b), explicitly rejecting a configuration that could
turn scanning off while leaving search on.

What rides on the latency assumption is therefore not correctness but
**adoption**: the scan is a local-model inference on the critical path of
every search, and the composite is inherited from REQ-562 (deterministic
pattern pass plus a model pass — see [[ASSUME-003]], itself unresolved on
whether the model pass earns its latency at all). If a search feels slow
enough to annoy, the user's only remedy is to drop the tier entirely, because
the coupling gives them nothing smaller to turn off. The privacy property
would then be preserved in the code and abandoned in practice — the failure
mode that matters, and the one a green test suite cannot see.

Unmeasured today: no benchmark of scan latency on query-sized input exists,
and REQ-563 shipped without one. The cap machinery is correct (LESSON-491:
measured on the rendered scan prompt, not the raw query), so the input is
small by construction — but "small input" is not the same claim as "fast
enough that nobody turns it off."

## Resolution

Not yet resolved. What would resolve it: dogfooding the search tier with a
real backend and recording per-query wall time from tool call to result,
separated into scan time and network time. The number that matters is scan
time as a fraction of total — if the third-party round trip dominates, the
scan is free in practice and this closes as validated. The trigger to revisit
is a user disabling `[web] tier = "search"` after enabling it, or scan time
exceeding roughly a quarter of a typical lookup's total latency.

Related: [[ASSUME-003]] asks whether the model half of the same composite
catches anything patterns do not. If ASSUME-003 resolves to "no," the honest
response is to drop the model pass — which would also resolve most of this
assumption's cost as a side effect.
