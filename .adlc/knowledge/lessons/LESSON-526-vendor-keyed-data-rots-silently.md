---
id: LESSON-526
title: "Vendor-id-keyed data rots silently — gate it like prose"
component: "cost/price-table"
domain: "cost"
stack: ["rust", "toml"]
concerns: ["data-integrity", "drift", "silent-failure"]
tags: ["price-table", "vendor-model-ids", "ci-gate", "row-order-coupling", "REQ-557-migration"]
req: REQ-577
created: 2026-08-14
updated: 2026-08-14
---

## What Happened

A sweep of `crates/tetond/data/prices.toml` — deliberately left out of REQ-577
TASK-144's scope — found that **every priced row** was keyed on a vendor model
id the vendor had since retired (`claude-opus-4`/`-sonnet-4`/`-3-5-haiku`,
`deepseek-chat`/`-reasoner`, `kimi-k2`; retirements 2026-02 through 2026-07),
including the savings baseline itself. REQ-577 had already caught `kimi-k2` in
*prose* via its catalog gates; the *data* surface carrying the same fact had no
gate and rotted to 100% dead within months.

Two silent-failure mechanics made this invisible. First, price lookup keys on
the declared model string (REQ-557 ADR-A), so a dead row never errors — its
calls degrade to *unpriced*. They are still named in `teton cost`'s unpriced
bucket, but spend totals and the savings estimate simply omit them, and nothing
distinguishes "vendor retired this id" from "user typed a model we never knew".
Second, the REQ-557 legacy migration turned out to resolve a pre-REQ provider's
model from the **first table row per vendor label** — TOML row order was a
hidden API, silently steering upgraded configs toward whatever model happened
to sit first.

## Lesson

Data files carrying third-party facts (model ids, endpoints, rates) need the
same drift gates as prose carrying those facts. The fix here: a one-directional
contract test (`the_price_table_and_the_recipe_catalogs_example_models_agree`)
requiring a price row for every remote recipe example model, with the local
recipe's model required *absent* (BUG-155). One-directional because the table
legitimately prices models the catalog doesn't exemplify — a bidirectional
sweep copied blindly from the prose gates would forbid valid rows. And when
consuming logic depends on "first row wins", either document that the file's
order is load-bearing or eliminate the coupling; a sweep that reorders rows is
also a behavior change to every consumer of that order.

## Why It Matters

An unpriced call under-reports spend and inflates nothing — no error, no test
failure, no user-visible breakage. The meter looks healthier the staler the
table gets. Without a gate, the next vendor retirement cycle reproduces this
in full.

## Applies When

Editing or adding any versioned data file keyed on identifiers a third party
controls; adding "first match wins" lookups over ordered data; deciding whether
a new prose gate's posture (bidirectional vs one-directional) transfers to a
data surface.
