---
id: ASSUME-032
title: "AC-3's body-length budget survives the collapse — twelve lines of headroom is enough"
status: validated
req: REQ-606
created: 2026-09-01
resolved: 2026-09-01
---

## Assumption

Added to REQ-606's spec during Phase 1 validation, because the spec's original
Assumptions section named the argument-limit failure mode (ASSUME-031) but not
this one:

> The same applies to AC-3's body-length budget, which is tighter than it looks.
> `run_prompt_turn` is at **188** lines against AC-3's 200 — twelve lines of
> headroom, re-derived at this REQ's base rather than taken from REQ-600's
> record. Collapsing an *input* bundle moves its fields back to the call site,
> and for the input bundles that call site is `run_prompt_turn`'s body. If a
> collapse that is right on the classification cannot be had without pushing the
> body over 200, that is the same kind of finding as the argument limit: record
> it, and keep the bundle.

The concern was real and directional: the whole REQ moves values *out* of
bundles and *into* call sites, and `run_prompt_turn` is the call site for most of
them. The margin was 6%.

## Disposition: **validated**

The body went **188 → 185 lines** against a limit of 200. It moved in the
correct direction, and the headroom grew rather than shrank.

Measured under REQ-600 AC-1's stated rule — body span, the `fn` signature line
through its closing brace, with braces inside string, char and comment tokens
excluded — by one instrument applied to both trees, so the delta is sound
independently of the absolute figure. Re-derived at every rebase and unchanged
at each: 185 after REQ-603, 185 after REQ-604, 185 at the merged commit.

### Why the feared direction did not materialise

Because the collapses that survived ASSUME-031's arithmetic were **not** the
input-bundle collapses the assumption was written about:

- `PreparedAttempts` is a **return-position** bundle. Collapsing it *shortened*
  the call site — a five-line destructuring pattern became
  `let (mut st, typed_refit) = …`, worth four lines.
- `ToolCallSite` lives in `turn_loop.rs` and does not touch `run_prompt_turn` at
  all.

Every bundle whose collapse would have grown `run_prompt_turn`'s body was kept
by ASSUME-031's gate before this budget was ever tested. The two constraints did
not trade against each other; the first one filtered out everything that would
have stressed the second.

### One line was spent and then reclaimed

The first implementation *did* push the body to **189** — over the baseline,
though still under the limit — by adding a five-line explanatory comment at the
`refit_system: &system` call site. The explanation already existed in
`prepare_the_attempts`'s doc, so the duplicate was cut to one line and the body
landed at 185.

Worth recording because the mechanism was not the one the assumption predicted:
the risk to a body-length budget in a heavily-documented codebase is **comment
prose**, not moved parameters. Doc comments inside the body count under AC-1's
rule.

## Residual

The budget is now 185/200 — 15 lines of headroom, better than the 12 it started
with, but still the tightest constraint on this function. Any future REQ adding
a stage call, or a rationale comment, to `run_prompt_turn` should re-derive the
span rather than assume the margin. Nothing enforces AC-3 mechanically: there is
no test asserting the 200-line bound, so it is checked only when a REQ chooses
to check it.
