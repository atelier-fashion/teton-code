---
id: REQ-590
title: "Derive the local tier's context budget from the engine's real window"
status: draft
deployable: true
created: 2026-08-24
updated: 2026-08-24
component: "daemon/router"
domain: "routing"
stack: ["rust", "daemon", "llama.cpp"]
concerns: ["routing", "latency", "cost", "reliability"]
tags: ["context-budget", "local-engine", "n_ctx", "oq-3", "generation-reservation"]
---

## Description

**This REQ discharges REQ-586's recorded OQ-3.** It is filed, not yet specified: the
open question explicitly calls for a *measured* decision, and the measurement has not
been taken. What follows is the problem statement and the facts a specifier will need,
so they are not rediscovered a third time.

REQ-586 made every route's context budget derive from that route's window facts —
except the local tier, which short-circuits in `derive` to a fixed pair
(`LOCAL_BUDGET_TOKENS` 4,096 words, `LOCAL_BUDGET_BYTES` 32,768 B) regardless of what
engine is loaded or how large its window is. OQ-3 asked whether that pair should come
from the engine's real `n_ctx` instead, and deferred on the grounds that REQ-564's
prefix-cache work and local prompt-processing cost make it a measured call.

REQ-589 hit the consequence in the field: a user's `/analyze` was refused at 4,097
words against the fixed 4,096-word budget on a machine whose engine had a
16,384-token window loaded. REQ-589 gives that user an exit (offer to proceed); it
deliberately does **not** touch the budget itself, which is this REQ.

## Facts for whoever specifies this

Three findings, each verified against the code, that shape the work:

1. **The router cannot currently read the engine's window at all.** `EngineLoadReport`
   (`crates/tetond/src/model_consent.rs:665`) carries only `benchmark` and `duty`. The
   16,384 figure is visible in llama.cpp's own stdout, not in any Rust value the router
   can reach. Any engine-derived budget needs the window plumbed from
   `LocalEngineLoader` up through the engine slot to `Router::budget_for` — this is real
   plumbing, not a constant swap.

2. **The two halves are not symmetric, and the byte half is the dangerous one.** This is
   OQ-3's own note: `LOCAL_BUDGET_BYTES` (32,768) is exactly `LOCAL_ENGINE_N_CTX`
   (16,384) × 2 B/token — *the whole engine window, with no room left for the reply* —
   whereas every remote pair subtracts the generation reservation from the window
   first (REQ-586 BR-2). The word half (4,096 ≈ 6,144 provider tokens at the 3/2 safety
   ratio) is well under the window. So "raise the local budget" is not one decision: the
   word half has headroom, and the byte half is already past where a remote route would
   have stopped. A naive `n_ctx`-derived pair that applied the remote formula would
   *lower* the byte half — which may well be correct, and would be a behavior change.

3. **The local pair is load-bearing beyond the local tier.** `derive` returns it for the
   unresolvable route too, and `HarnessConfig::default()` reads the same constants
   (REQ-586 AC-1 pins `max_context = 0` yielding today's `(4096, 32768)`). Changing the
   constants moves those callers; changing only the local *tier's* derivation does not.
   A specifier must decide which is intended.

## Open Questions

- [ ] OQ-1: Derive from `n_ctx` with the remote formula (window − reservation, then the
  3/2 and 2 B/token rules), or keep a local-specific rule? The remote formula lowers
  today's byte half.
- [ ] OQ-2: What does the prefix-cache (REQ-564) cost model say about a larger local
  budget — does a bigger context defeat the KV reuse that made local turns cheap?
- [ ] OQ-3: Does `HarnessConfig::default()` / the unresolvable route follow the local
  tier, or keep today's constants?
- [ ] OQ-4: What measurement settles it? REQ-589 AC-15's dogfood runbook produces the
  first real data point (an accepted over-budget local turn, and whether it serves).

## Out of Scope

- Anything REQ-589 covers — the over-budget offer, the consent flow, and the durable
  remedy for remote routes.

## Retrieved Context

Filed as a deferred follow-up rather than authored from retrieval — the retrieval that
produced it is recorded in REQ-589. Primary sources: REQ-586 OQ-3
(`.adlc/specs/REQ-586-route-aware-context-budget/requirement.md:610`) and the REQ-589
field report.
