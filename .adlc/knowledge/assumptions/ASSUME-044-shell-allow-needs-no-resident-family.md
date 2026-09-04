---
id: ASSUME-044
title: "`/shell allow` does not need a slot in the resident prompt's command-family list"
status: unresolved
req: REQ-614
created: 2026-09-04
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

Unresolved. Deliberately not decided inside REQ-614's Phase-7 rebase: the
merge was scoped to composing four REQs, and adding a family to the resident
prompt is a new behavioral choice rather than a composition of existing ones.
Resolve by either adding `/shell` to `self_config.md` (7 bytes, fits) or
recording that the announcement path is sufficient — and in **either** case
adding the missing guard, so the next command in a new family is a decision
somebody makes rather than one nobody sees.
