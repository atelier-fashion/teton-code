# REQ-588 — Architecture: a spend cap, and an event vocabulary a future kind cannot break

Eight ADRs. Two legs that share a REQ only because REQ-586's review produced
both; they touch different files and are independent after ADR-1.

The through-line for the cap: **the ledger already prices every call, and the
choke point already sees every remote call.** The ceiling is the join of two
things that exist, not a new accounting system.

---

## ADR-1 — Per-prompt scope is a *lifetime*, not a key

`PromptSpend` is a small shared counter (`Arc<PromptSpend>` — an atomic in
micro-cents plus an unpriced flag). `run_prompt_turn` creates **one per
prompt** and threads it onto every `EgressContext` that prompt builds.

**Why a lifetime rather than a `prompt_id` key.** A keyed accumulator needs a
key that is right, a map that is pruned, and a rule for what happens when the
key is missing. All three are ways to get "per prompt" wrong. An `Arc` whose
scope *is* the prompt cannot be keyed wrongly, needs no pruning, and its
absence is a type-level fact (`Option<Arc<PromptSpend>>` = "no ceiling in
force") rather than a lookup miss to interpret.

This is also what makes OQ-1's answer structural: the accumulator is created
where the prompt is, so "per prompt" is not a policy the code has to remember.

## ADR-2 — The check is a **floor crossing**, not a prediction, and says so

Rule: **refuse the next call once this prompt's recorded spend has reached the
ceiling.**

It is not "refuse a call that would exceed", because that is not knowable — a
call's cost depends on the *output* tokens, and no one can price those before
the model writes them. Pretending otherwise would mean either a guess (wrong,
silently) or refusing on a worst-case estimate (refusing calls that would have
been fine).

**The consequence, stated rather than hidden: a prompt can overshoot its
ceiling by at most one call.** That bound is real and small — the ledger prices
each call as it completes, so the overshoot is one call's spend, not a prompt's.
It is named in the refusal, in `teton_docs`, and in the release notes (BR-5), so
a user reading "$5.00 ceiling" knows what they are actually being promised.

The alternative — a pre-flight estimate from input tokens plus `max_tokens` —
was rejected: it makes the ceiling bind *earlier* than the user's number in
every case, which is a different lie.

## ADR-3 — Currency, and **unpriced is a refusal** (OQ-2, user decision)

The ceiling is money, in micro-cents to keep the arithmetic integral. The
ledger prices every call, so the figure is available; a token ceiling would
mean different money per provider, which is not what a ceiling is for.

**When the price table cannot price the provider/model, and a ceiling is
configured, the call is refused.** This is the half the original lean did not
carry, and it is load-bearing: an unpriced call cannot be counted, so allowing
it would make the ceiling silently not-a-ceiling for exactly the provider
nobody has a price for. A missing price must not become a missing ceiling.

**What this does *not* close, and A-1 says so:** a price that is present but
*stale* still yields a wrong ceiling, and nothing here detects that. Absent is
detectable; wrong is not. The mitigation is the price-table re-verify, tracked
separately.

No ceiling configured ⇒ no check, no pricing lookup, no refusal. An
un-opted-in machine behaves exactly as it does today (ADR-6).

## ADR-4 — A typed outcome that is **not** a provider failure (BR-3)

A new `EgressError::SpendCeilingReached { spent, ceiling, bound, unpriced }`,
raised at the choke point beside `PrivacyBlocked` — the existing precedent for
"the choke point refused this, and it is not the provider's fault".

**Health is the thing to get right.** A ceiling refusal must never mark the
provider `Unavailable`: the provider did nothing wrong, and degrading it would
make a *budget* decision look like an *outage*, then reroute later turns away
from a healthy provider for the rest of the session. It rides the same arm
`PrivacyBlocked` does, which the router already excludes from health.

It is also not a `FailureAction::Fallback`: falling back to a cheaper provider
is precisely the silent downgrade OQ-4 rejected.

## ADR-5 — Refuse, naming the spend (OQ-4, user decision)

The refusal sentence carries: what was spent this prompt, what the ceiling is,
which ceiling bound it (ADR-7), and — when relevant — that one call may have
overshot (ADR-2). It is composed **once**, in `teton-core`, and rendered by both
the model-facing error and the CLI line, for LESSON-529's reason.

Offering a cheaper tier as an *accepted* recipe was considered and left out of
scope: it needs a new consent surface, and refusing plainly is the smaller
correct thing. Recorded here so the next reader knows it was weighed.

## ADR-6 — Opt-in, in `[cost]`, mirroring `[privacy] redact` (OQ-3)

`[cost] prompt_ceiling_usd` — absent means no ceiling, and "off" means the
check **does not exist** rather than runs-and-permits: with no ceiling
configured the choke point does no pricing lookup and builds no accumulator, so
an un-opted-in machine pays nothing, exactly as `[privacy] redact` is installed
only when true (REQ-563 ADR-2).

REQ-586's big-window notice gains a pointer at it — the notice that says "you
just declared a 1M window" is the right place to mention the knob that bounds
what that costs.

## ADR-7 — One home for the binding ceiling, in REQ-586's `bound` shape (BR-2)

Today there is one ceiling. There will plausibly be more (a per-session one, a
per-provider one). So the *shape* is REQ-586's: a `SpendBound` enum naming
which constraint bound this refusal, derived in one function, carried on the
error and rendered on `/verbose`.

One ceiling today means `SpendBound` has one real variant — and that is fine.
The value is that adding the second one is a variant plus a rendering, not a
retrofit of "which number did we actually use" into a sentence that never had
to say.

## ADR-8 — BR-4 copies BUG-186's pattern, and its test, verbatim

`#[serde(other)] Unknown` on `ContextPressureKind` and `BudgetBound`, both
verified still closed at validation. The four-leg skew test BUG-186 wrote is
the template: unknown kind degrades, known kind still parses (non-vacuity), the
whole frame survives, and the fail-closed sibling (`PermissionSubject`) is
asserted to stay closed.

**Scoped to enums inside a payload.** The top-level `Event` enum is a larger
hole — an unknown event *kind* drops the whole frame — and is tracked
separately, because widening it touches every match on `Event`.

---

## Task graph

```
TASK-A (serde tolerance: BR-4)          — independent, ships alone
TASK-B (config: [cost] prompt_ceiling)  ─┬─► TASK-D (the check at the choke point)
TASK-C (PromptSpend + SpendBound + the  ─┘        │
        refusal composer)                          ├─► TASK-E (surfaces: /verbose, refusal line)
                                                   └─► TASK-F (docs: teton_docs + release note)
```

Tier 1: A, B, C. Tier 2: D. Tier 3: E, F.

TASK-A is deliberately first and independent: it is the half with no product
questions in it, and it is worth landing even if the cap's design changes.
