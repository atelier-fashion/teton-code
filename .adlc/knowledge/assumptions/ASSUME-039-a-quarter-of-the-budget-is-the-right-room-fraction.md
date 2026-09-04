---
id: ASSUME-039
title: "A quarter of the byte budget is the right ceiling for one skill body"
status: unresolved
req: REQ-618
created: 2026-09-04
resolved:
---

## Assumption

`ROOM_FRACTION_PERCENT = 25` — REQ-618 BR-4's rule that a skill body taking more
than a quarter of the route's byte budget is refused with the arithmetic, even
though it fits. The spec proposes `room_fraction = 0.25` and calls it "a starting
value", and states that "the shipped ADLC bodies (largest 25 KB) fit any route at
or above REQ-616's 262,144-token local window, and the fraction only bites on
routes below it".

**One figure in that sentence is wrong and the other has since come true**, and
the implementation measured both rather than inheriting them:

| the spec says | measured |
|---|---|
| largest shipped body 25 KB | `/proceed` is **51,037 bytes** — twice the stated size |
| routes at or above 262,144 tokens | **REQ-616 landed mid-flight**; the engine now serves 262,144 |

The measurement after the rebase onto REQ-616:

| route | byte half | room ceiling | `/proceed` at 51,037 B |
|---|---|---|---|
| local, engine at 262,144 | 522,240 | 130,560 | **9.8 % — expands** |
| local, no engine (`BudgetInputs::local`) | 63,488 | 15,872 | 80 % — offered |

So on a machine with the local engine loaded — every machine with a local tier —
the fraction does not bite, and the shipped skills expand as they always did. The
second row is the no-engine fallback, and a machine in that state has no local
tier to route to.

This is the outcome the spec predicted, reached by a different route than it
assumed: not because the bodies are 25 KB (they are not) but because REQ-616
raised the window by eight times, which is more than enough to absorb a body
twice the assumed size.

## Context

Three shipped behaviours move at 25%. One is intended; two are collateral the
spec did not name. **None of them is undone by REQ-616** — they are consequences
of the fraction's relationship to other fractions of the same budget, not of the
budget's size.

1. **REQ-590 AC-12 is superseded, deliberately.** That AC says the reported
   `/analyze` turn — 4,097 words at a byte count the field report admits — must
   serve on the local tier silently. REQ-618's Description reads the same session
   and concludes the opposite: the body was 38 % of the budget "before a single
   tool result", the turn served, and serving is what went wrong. So the outcome
   for that exact body is now the question rather than the send. Recorded at the
   test (`skill_over_budget_offer.rs`, leg 4).

2. **REQ-587's dynamic-output Stage-B refusal is unreachable.** That path needs
   `body + command output > budget` with the body admitted at Stage A. The
   output is capped at `MAX_OUTPUT_CHARS` (8,000) and the budget floors at
   50,000, so a body inside a 25 % ceiling plus 8,000 bytes cannot exceed the
   budget on any route. Stage B is still reachable through the *request block*
   (`skill_append_fit` charges it), which is what the surviving tests exercise.

3. **REQ-587 BR-7's digest bypass is pre-empted below roughly a 350k-token
   window.** `digest_threshold_bytes` and the room ceiling are both fractions of
   the same byte budget, and the threshold is the larger one until it hits its
   own cap:

   | route | byte half | digest threshold | room ceiling |
   |---|---|---|---|
   | local, no engine | 63,488 | 23,250 | 15,872 |
   | local at 262,144 (REQ-616) | 522,240 | 163,840 (capped) | 130,560 |
   | `max_context = 128000` | 253,952 | 93,000 | 63,488 |
   | `max_context = 1000000` | 2,000,000 (approx) | 163,840 (capped) | 499,488 |

   So on every ordinary route, a body large enough to be *digested* is already
   large enough to leave the turn no room, and BR-4 refuses it before the bypass
   can be observed. The bypass code still runs; what changed is that the turn
   carrying it is refused. Pinned by
   `harness::budget::tests::the_room_ceiling_and_the_digest_threshold_are_pinned_against_each_other`,
   which names which of the two is higher per route so a change to either
   constant is loud.

What depends on this: every skill invocation on a route below ~130 KB of byte
budget, which today is every local session.

## Resolution

Two things would settle it, and they are independent.

**REQ-616 landed while this REQ was in flight**, and the rebase measurement above
is what closes the practical half: at a 262,144-token window the quarter is
130,560 bytes and no body in this corpus approaches it. Item (1) is therefore the
only behaviour a user sees change, and it is the change the REQ asked for. This
half of the assumption can be considered **validated by measurement**; what stays
unresolved is the half below.

**A decision about the fraction itself** settles items (2) and (3), and it is a
product call rather than a derivation. The candidate values and what each buys:

- **25 % (as shipped)** — catches the REQ's own 38 % case with margin; costs
  items (2) and (3).
- **Above `digest_threshold_bytes / budget_bytes`** (≈ 37 % on a 128k route)
  — stops pre-empting the digest rule, and still catches 38 % by a hair. Too
  close to the motivating case to be comfortable.
- **A fraction that is not a fraction** — a floor in bytes, or a rule keyed on
  "what the turn has left after the body" rather than on the body's share. This
  is the shape that would make items (2) and (3) go away properly, and it is a
  redesign rather than a constant change.

To invalidate: a machine whose engine loads *below* 262,144 — the memory-fitted
step-down REQ-616 BR-3 describes — puts the room ceiling back under the shipped
bodies. `local_window_decided { reason: memory_fit }` in a transcript beside a
`skill_refused_no_room` for a shipped skill is the pair to watch for, and it is
the case this REQ was written to govern in the first place.
