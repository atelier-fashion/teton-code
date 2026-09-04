---
id: ASSUME-044
title: "`/shell allow` does not need a slot in the resident prompt's command-family list"
status: invalidated
req: REQ-614
created: 2026-09-04
resolved: 2026-09-04
---

## Assumption

That the model does not need resident awareness of `/shell allow`, and that
carrying it in `teton_docs commands` alone is sufficient.

## Context

REQ-617 split command awareness in two: the resident prompt
(`harness/self_config.md`) names **17 families** —
`/help /cost /effort /model /clear /cd /projects /verbose /transcript
/context /permissions /web /provider /boundary /policy /doctor /quit` — and
`teton_docs commands` carries the sub-commands and their effects. The split was
forced, not chosen: ASSUME-043 records that REQ-615 and REQ-617 spent the same
prompt margin in one sprint and the roster had to be collapsed from 29 names to
those 17 families to fit at all.

REQ-614 landed `/shell allow` while blocked on a rebase, and it is the first
command in a family that is **not** on that list. Its roster row and its
`commands.md` line were added during the rebase — both required by REQ-617's
two-guard chain — so the docs page names it. The resident list does not, and
nothing checks that it should: `slash.rs` pins the roster to the CLI dispatch
table, `docs.rs` pins `commands.md` to the roster, and **no guard ties
`self_config.md`'s family list to either**. The omission is therefore silent by
construction, not caught and accepted.

The case for leaving it: the model cannot run any built-in command, and BR-7's
pin announcement names `/shell allow` to the **user** directly, in the CLI, at
the moment the pin happens — which is the path REQ-614 designed for. The case
against: REQ-617 exists precisely because a model that does not know a command
exists never offers it, and "why is my session pinned?" is a question the user
will put to the model, not to the announcement they already scrolled past.

Measured cost of resolving it the other way: `/shell` is 7 bytes against 64
usable above the floor (margin 112, floor 48, re-measured on REQ-614's rebased
tip and unchanged by it). It fits without touching
`REDACT_BODY_OVERHEAD_BYTES` — so this is a product decision, not a budget one,
which is why it is recorded here rather than settled during a rebase.

## Resolution

**Invalidated.** `/shell` is on the resident list, between `/web` and
`/provider` in `/help` order, and the missing guard is in place.

The deciding fact is what the model has to answer from *after* the pin line
has scrolled past. BR-7's announcement is the user's path and it is a good one,
but it fires once, at pin time, and it is not addressed to the model. When the
user later asks "why is my session on the local model?", the model's resident
sources are the guide and the tool descriptions — and the `shell` tool's
description (checked: REQ-615's cwd contract and REQ-614's provenance duty,
nothing about the pin or its remedy) does not name `/shell allow` either.
`teton_docs commands` does, but a model only opens that page for a command it
already suspects exists, which is the exact circularity REQ-617 was opened to
break. Seven bytes is a cheap price for closing it, and the margin can pay:
re-measured, not reasoned, both pins moved by exactly 7 — **112 → 105** and
**159 → 152**, the gap still 47, 57 bytes of usable room above the 48-byte
floor. `REDACT_BODY_OVERHEAD_BYTES` is unmoved; the ledger line is on it.

The guard is
`harness::turn_loop::tests::the_resident_prompt_names_every_command_family_the_roster_carries`.
It reads the families out of the command sentence itself — between `The
built-in commands are ` and the `;` that closes the list, because the guide
also carries `/v1/messages`-shaped endpoint paths and a whole-file slash scan is
not a parser for this clause — and holds them equal to the distinct first words
of `SESSION_COMMANDS`, in both directions and in `/help` order. A
`FAMILIES_KEPT_OUT_OF_THE_RESIDENT_PROMPT` list beside it is the one route for
a family deliberately left off the prompt; it is empty, and an entry has to
name a family that exists. Mutations run: dropping `/shell` → red naming it
under "in the roster, absent from the resident prompt"; adding `/bogus` → red
under the other heading; swapping two families → red on order.

What this does not decide: whether BR-7's line should *also* be surfaced to the
model as a turn-level fact (a pinned session's routing is state the model
cannot read from any file, like every other switch the guide describes). That
is a separate REQ's question. This one only closes the gap where a command the
session accepts was invisible to the model by construction.
