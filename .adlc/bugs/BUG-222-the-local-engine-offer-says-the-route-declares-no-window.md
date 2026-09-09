---
id: BUG-222
title: "The over-budget offer on the local tier says the route declares no context window while quoting the engine's window"
status: resolved
severity: medium
created: 2026-09-09
updated: 2026-09-09
resolved: 2026-09-09
component: "tetond/harness/budget"
domain: "local-tier"
stack: ["rust"]
concerns: ["developer-experience", "reliability"]
tags: ["over-budget-offer", "window-verdict", "local-engine", "req-589", "req-590", "req-616"]
introduced_by: ["REQ-590"]
attribution: manual
---

## Description

A skill turn on the local tier that exceeds the route's pair is offered with a
sentence that says "bound: local engine — both halves come from the engine's
32,768-token window, less the 1,024 reserved for the reply" and, two sentences
later, "This route declares no context window, so this daemon cannot promise the
send will fit". Both cannot be true. Seen 2026-09-09 with `/analyze` pinned to
the 7B.

## Root Cause

REQ-589 wrote its reachability table when the local budget was a constant pair:
`LocalEngine` reached `WindowUnknown` only, and `window_verdict` hard-wired it.
REQ-590 derived the pair from the engine's window and REQ-616 made that window
the engine's real allocation, stamping it on the budget — the clause above reads
it from there — but the verdict arm and the table were never revisited. The
offer's caller also passed the route's *declared* window (0 for local), so even
a corrected arm would have compared against nothing.

## Resolution

`window_verdict` treats `LocalEngine` as a window-bearing bound; `verdict_window`
is the one resolver for which window the comparison uses (the engine's on that
bound, the declaration otherwise — never the cap, per ADR-15), read by the offer
and by the tests that check the offer against the classifier. Two sentences are
added for the local arms, worded "the context window the engine allocated"
because the route declares nothing; the exceeds sentence names the typed
context-length outcome ADR-3 built. REQ-589's requirement and architecture tables
are amended in place. Tests: the unit rows for both local verdicts, the runtime
offer tests (the `heavy` fixture is past the window), and the integration cells
(`over_the_local_pair` is past it; the one-byte-over leg is inside it).

## Deployment

- Pending merge.

## Files Changed

- `crates/tetond/src/harness/budget.rs`, `crates/tetond/src/runtime/mod.rs`, `crates/tetond/tests/skill_over_budget_offer.rs`
- `.adlc/specs/REQ-589-over-budget-skill-expansion-offer/{requirement,architecture}.md`
