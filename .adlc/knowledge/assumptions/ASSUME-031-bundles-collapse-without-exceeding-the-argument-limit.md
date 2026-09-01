---
id: ASSUME-031
title: "The bundles can be collapsed without pushing any signature back over the argument limit"
status: invalidated
req: REQ-606
created: 2026-09-01
resolved: 2026-09-01
---

## Assumption

REQ-606's spec carried one assumption, and it was the one the whole REQ rested
on:

> The bundles can be collapsed without pushing any signature back over the
> argument limit. If one cannot, that is a finding to record — it would mean the
> cluster is real and the bundle earns its name after all.

It was written with its failure branch explicit, which is the only reason the
outcome below is a result rather than a disappointment. The REQ was filed on
review's judgement that "roughly five earn a name and the rest are transport" —
so the assumption was that most of the fourteen would collapse.

## Disposition: **invalidated**

**Twelve of the fourteen cannot be collapsed.** The failure branch is the one
that was taken, and it is the REQ's headline result.

### The mechanism, not just the verdict

Clippy's `too_many_arguments` threshold is **7** — it fires at 8 — and REQ-606
AC-2 forbids the `#[allow(clippy::too_many_arguments)]` that would silence it
(`suppression_ratchet.rs` refuses it by design: "a new suppression is a new
unnamed parameter cluster; name it instead"). So for an *input* bundle,
"collapse it" is not a free readability choice. It is a signature-width change
with a hard gate on the other side.

That makes the classification arithmetic. For each input bundle the consumer's
post-collapse argument count is `current_args − 1 + fields`:

| bundle | fields | consumer | args now | if collapsed |
|---|---:|---|---:|---:|
| `AttemptInputs` | 8 | `run_attempts` | 4 | **11** |
| `SessionFacts` | 6 | `resolve_the_route` | 4 | **9** |
| `PromptRequest` | 6 | `resolve_the_route` | 4 | **9** |
| `ExpansionInputs` | 5 | `settle_expansion` | 4 | **8** |
| `TurnProducts` | 4 | `prepare_the_attempts` | 6 | **9** |
| `LoopContext` | 6 | `serve_tool_call` | 5 | **10** |
| `ModelReply` | 7 | `serve_tool_call` | 5 | **11** |

Every row exceeds 7. `SessionFacts` and `PromptRequest` collapse into the *same*
signature — fourteen arguments — which is why REQ-600 split them into two rather
than widening one.

### The threshold was verified empirically, which is what makes this checkable

The entire table depends on that 7. It was measured rather than recalled.
`ExpansionInputs` — **the narrowest margin in the set, and therefore the one row
that would expose an off-by-one** — was actually collapsed, its call site
updated, and clippy run:

```
error: this function has too many arguments (8/7)
    = help: to override `-D clippy::all` add `#[allow(clippy::too_many_arguments)]`
```

Clippy names the forbidden suppression as the only escape, which is the whole
argument in one message. The probe was then reverted.

**Had the threshold been 8, five of the seven rows above would have flipped**
to collapsible. That is the difference between a result a reader can check and
one they must take on trust.

## What Invalidation Cost, And What It Bought

Nothing was wasted. The assumption's failure branch *was* the deliverable: AC-1
asked for a classification, and "this cluster is real" is a classification.
`TurnProducts` is the sharpest case — the spec called it transport in as many
words ("built from four loose locals at the call site and destructured on the
callee's first line"), both halves of that sentence are true, and the conclusion
still does not follow, because collapsing it reaches 9 arguments and the best
available reduction still reaches 8.

The two that *did* collapse were found by a different rule than the one the spec
proposed — not "is this transport?" but "is this field already reachable through
another field of the same bundle?":

- `PreparedAttempts.refit_system` was `system.to_owned()` returned to a caller
  still holding `system` unmutated. Deleting it left two fields, where a tuple
  is idiomatic, and removed a per-turn `String` allocation.
- `ToolCallSite` carried `name` and `arguments` beside `call`, whose `ToolCall`
  was built from clones of exactly those two, unrebound.

## Residual

**A future REQ proposing to collapse a parameter bundle in this codebase must do
this arithmetic before committing to the work.** The result is not "the turn path
has too many bundles"; it is "a bundle guarding a signature at 8+ arguments is
load-bearing while AC-2's rule holds". The generalisation is recurring, unlike
ASSUME-030: any refactor that trades a struct for loose parameters meets the
same gate.

The residual risk is the inverse move — a future change *adding* a parameter to
one of these stages will push its bundle wider rather than its signature, which
is the correct direction but silently makes the bundle harder to justify on
cohesion grounds. Nothing currently checks that.
