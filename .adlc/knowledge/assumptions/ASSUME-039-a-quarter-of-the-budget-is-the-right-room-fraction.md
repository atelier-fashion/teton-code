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

**Both figures in that sentence are wrong about this repository today**, and the
implementation measured them rather than inheriting them:

| the spec says | measured |
|---|---|
| largest shipped body 25 KB | `/proceed` is **51,037 bytes** |
| routes at or above 262,144 tokens | the local window is **32,768** until REQ-616 lands |

At 32,768 tokens the byte half is 63,488 and the quarter is 15,872 — so on the
local tier today, `/proceed` and every skill of comparable size is **offered**
rather than expanded. The user answers REQ-589's question once per invocation and
it proceeds; nothing is lost, but nothing is silent either.

## Context

Three shipped behaviours move at 25%. One is intended; two are collateral the
spec did not name.

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
   | local (32,768) | 63,488 | 23,250 | 15,872 |
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

**REQ-616 landing** removes the practical bite: at a 262,144-token window the
byte half is 522,240 and the quarter is 130,560, which no body in this corpus
approaches. If REQ-616 ships before or with REQ-618, item (1) above is the only
behaviour that changes for a user, and it is the change the REQ asked for.

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

To invalidate: run a real session on the local tier with a large skill and count
how many times the offer is answered before the user finds it tiresome. One
answer per invocation on `/proceed` is the observation to watch for.
