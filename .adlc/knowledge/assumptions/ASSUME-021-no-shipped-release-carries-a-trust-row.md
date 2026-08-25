---
id: ASSUME-021
title: "No shipped release carries a trusted_project_roots row, so the format change needs no migration"
status: validated
req: REQ-591
created: 2026-08-25
resolved: 2026-08-25
---

## Assumption

`[skills] trusted_project_roots` has never existed in a tagged release, so REQ-591 D-4's change
of the row format (home-relative → canonical absolute) and D-5's load-time validation can ship
with **no migration path** — a row in the old form simply fails validation.

## Context

This is load-bearing for two decisions taken together, and it was stated only in a commit
message.

D-4 changed what a row means. D-5 made a malformed row a **fatal** `Config::validate` failure,
and `Config::validate` gates daemon **start** — so a machine carrying an old-format row does not
get a degraded skills subsystem, it gets a daemon that refuses to boot, taking routing, providers
and MCP with it. That blast radius is only acceptable because the population is empty.

If the assumption were false, the correct design would differ: drop malformed rows with a loud
stderr line rather than failing the document, trading D-5's stated guarantee for availability.

## Resolution

**Validated.** The table was introduced by REQ-589's D-13 work, which merged 2026-08-24 (PR #212)
and 2026-08-25 (PR #213); the last tagged release before both is v0.1.24 (2026-08-23) and does
not contain it. The only machines that can hold an old-format row are ones that built the
pre-carve-out branch directly.

**This assumption expires on the next release.** Once a version ships carrying the table, any
future change to the row format inherits a real migration obligation and this reasoning must not
be reused.
