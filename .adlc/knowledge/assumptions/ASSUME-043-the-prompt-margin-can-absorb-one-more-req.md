---
id: ASSUME-043
title: "The 23 KiB body overhead has room for the next REQ's prompt sentence, so shortening is always an available answer"
status: invalidated
req: REQ-617
created: 2026-09-04
resolved: 2026-09-04
---

## Assumption

That `REDACT_BODY_OVERHEAD_BYTES` at 23 KiB leaves enough margin that a REQ
needing a sentence in the resident system prompt can always pay for it by
shortening something — its own wording, or a clause it supersedes — rather than
by raising the ceiling and re-deriving `REDACT_TOTAL_CAP_CHUNKS`,
`REDACT_INPUT_MAX_BYTES`, `REDACT_SCANNABLE_CONTEXT_BYTES` and
`REDACT_MAX_CHUNKS`.

## Context

REQ-612 raised the overhead to 23 KiB and left 733 bytes of margin against a
48-byte floor — 685 usable, the loosest the pin has recorded. The ledger's own
advice to the next REQ has been "shorten a clause; a raise is a whole-KiB move
and belongs to a REQ that means to make it." REQ-615 followed it, spending 278
and leaving 455. REQ-617 planned on the same, and the *architecture* budgeted
against 733 because REQ-615 had not merged yet.

## Resolution

**Invalidated, at the second concurrent claimant.** REQ-615 and REQ-617 spent
from the same margin in the same sprint. Written against 733, REQ-617's roster
spent 540 and left 193; REQ-615 then merged with its 278, and the two together
were 85 bytes **over** the ceiling — the rebase failed the
`spent < REDACT_BODY_OVERHEAD_BYTES` assertion before the floor was ever
consulted.

The product decision was to shorten again rather than raise: REQ-617's roster
lost a further 197 bytes (29 command names to 17 families, with `teton_docs
commands` carrying the sub-commands, and BR-3's ending stripped of a
parenthetical that re-listed five switches the roster already named). Both
recorded pins were re-measured, not reasoned: **455 → 112**, and 502 → 159 on
the web shape, the gap still 47.

112 leaves **64 bytes of usable room** above the floor — less than the 81 the
pin was introduced at, and the tightest it has ever recorded. The assumption
does not survive that: the next REQ that needs a sentence here cannot shorten
its way in, because there is no longer a sentence's worth of room to reclaim.
It should expect to raise the ceiling and re-derive, which the ledger has
always said is a decision a REQ makes deliberately. What changed is that it is
now the *only* option, not the expensive one.

Two further notes for whoever makes that raise. The margin is a shared resource
with no reservation mechanism — two REQs in one sprint each measured against
733 and neither was wrong at the time it measured, which is the actual failure
mode and is not fixed by having more room. And `RECORDED_MARGIN_GAP_BYTES`
(REQ-617) now refuses any change that moves one prompt shape's margin and not
the other's, so a raise must re-measure both sweeps.
