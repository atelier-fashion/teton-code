---
id: ASSUME-029
title: "The session-lifecycle slice is still coherent as a unit"
status: validated
req: REQ-603
created: 2026-09-01
resolved: 2026-09-01
---

## Assumption

REQ-599's ADR-4 planned a session-lifecycle module and asserted the slice was
coherent as a unit. REQ-603's spec carried the assumption forward with an
explicit instruction not to inherit it:

> The slice is still coherent as a unit. REQ-599 asserted this from the plan
> side and never tested it against the code; this REQ must confirm it before
> committing to a module, and say so if it turns out the lifecycle code is
> genuinely entangled rather than merely large.

## Context

REQ-599 took its seven extraction steps cheapest-seam-first from the impl
structure and ran out of steps before it ran out of seams. Session lifecycle was
described as the most entangled of the remaining candidates — so "merely large"
versus "genuinely entangled" was an open question that decided whether a module
should exist at all. A forced module would have been worse than none.

## Resolution

**Validated for the production surface; the test surface is genuinely entangled,
and that is recorded rather than papered over.**

Method (AC-1, deliberately structural rather than id-based — REQ-599 ADR-1,
LESSON-593): cut `mod.rs` at the first column-0 `#[cfg(test)]`; enumerate every
method of the `impl DaemonRuntime` block with its span; map each to the struct
fields it touches; cluster on what each *serves*; check the clustering against
adjacency.

**Production — coherent, and the evidence is strong:**

- The session-lifecycle methods were **already one contiguous run** in `mod.rs`,
  unbroken between `record_health` and `mcp_egress`. The clustering and the file
  layout agreed without being made to.
- The run's only dependency on a private `mod.rs` item is `refused_claim_error`,
  which `turn.rs` already reaches the same way — so no visibility change was
  needed to extract it.
- Nothing required widening, confirmed by demoting and building rather than by
  grepping (LESSON-596).

**Tests — entangled, for a structural reason.** Nine of the ten
session-lifecycle tests drive a real prompt turn through `conversation_carry`'s
recording-engine fixture before clearing a conversation or moving a root,
because a cleared conversation and a moved root only mean anything once a turn
has built one. Moving them requires lifting ~230 lines of turn-path fixture into
`testsupport.rs` to serve two homes. They stayed, under AC-5's second clause,
with the reason in the module header. The tenth — which never calls
`run_prompt_turn` — moved, and the compiler forced it, since it reads
`jail_root`.

**One item identified as part of the slice did not move**, and not because of
entanglement: `store_session_skills` is blocked by a doc-attachment defect frozen
into `traceability_sweep.rs`'s baseline (LESSON-607). The slice shipped as five
items, not six, and the sixth is filed as a follow-up.

**Also re-measured**: ADR-4 estimated the slice at ~900 lines. The production
surface is 335 non-blank lines. The estimate was never re-derived at the time,
which is why the requirement asked for a measurement rather than trust.
