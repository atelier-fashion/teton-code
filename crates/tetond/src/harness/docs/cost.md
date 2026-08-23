# What a turn costs, and capping it

Every remote call is priced and recorded as it streams, so `/cost` reports
what was actually billed rather than an estimate. Local-tier calls cost
nothing and are not priced.

You cannot run these commands. Print them; the user runs them.

## There is no spend ceiling until one is configured

A fresh install has **no cap**. Nothing limits what a prompt can spend until
the user sets one, and no ceiling appears on its own — a limit that arrived
silently would be a limit nobody could plan around.

To set one, in `config.toml`:

```toml
[cost]
prompt_ceiling_usd = 5.00
```

Omit the key, or the whole `[cost]` table, and no ceiling is in force. That is
the default and it is not a mistake to be corrected.

## What the ceiling actually promises

The scope is **one prompt**, not a session, a day or a month. Each new prompt
starts from zero; the total is everything that prompt causes, across its
retries, its fallbacks to another provider, and the duties it runs.

The check happens **between calls**, and this is the part worth reading
carefully. What a call will cost cannot be known before the model has written
its reply, so Teton cannot decline a call for being too expensive. It can only
refuse to start the *next* one once the ceiling has been reached. A prompt can
therefore finish slightly over the number set — by at most the cost of
one call: the one that was already in flight. A ceiling of `$5.00` is a promise to
stop at $5.00, not a promise never to exceed it.

Two things the ceiling deliberately does not do:

- It does not fall back to a cheaper provider. Rerouting because the budget ran
  out would spend more money, not less, and would do it without saying so.
- It does not mark the provider unhealthy. The provider is working; the budget
  ran out. Treating that as an outage would route later turns away from a
  provider with nothing wrong with it.

## An unpriced model is refused, not waved through

With a ceiling in force, a call Teton cannot price is refused rather than sent
uncounted — a missing price must not quietly become a missing ceiling. The
refusal names the provider and the model. The fix is to add a price for that
model, or to remove the ceiling and send it unmetered.

With no ceiling configured, pricing is never consulted at all, so an unpriced
model is not an obstacle.

## Reading it back

The `/verbose` route line names the ceiling in force, if any. The refusal, when
it comes, names what the prompt spent and which ceiling it reached — the same
words, from the same place, so the two cannot describe different limits.
