---
id: ASSUME-022
title: "The 2 B/token bridge is treated as a worst case; it is not one"
status: invalidated
req: REQ-590
created: 2026-08-26
resolved: 2026-08-26
---

## Assumption

`DUTY_REQUEST_BYTES_PER_TOKEN = 2` is used wherever a byte budget must be converted to provider
tokens safely — it is described as "the 2 B/token BPE floor", i.e. a worst case no real content
falls below.

## Context

The assumption became load-bearing in REQ-590. The local budget's crossover density moved from
8 B/word to 3.2, so the **byte** half is now the binding guard for essentially all real content,
and the only thing standing between that guard and the engine's hard window is this ratio.

## Resolution

**Invalidated, by measurement in this REQ.** Two of six corpus samples fall below it:

- `base64.txt` — 1.447 B/token
- `numeric_grid.txt` — **1.00 B/token** (added by this REQ; `o200k_base` gives every digit *and*
  every separating space its own token, so the density is a property of the format, not of the
  sample — random, sparse and run-heavy grids of the same shape all measure exactly 2.000
  tokens/word)

At full local budget the grid costs 20,480 real tokens against 15,360 usable — **1.33× over**,
with **both** harness guards admitting it. `KNOWN_UNCOVERED_AT_PINNED_FLOOR` now holds two entries.

It is a heuristic wearing a floor's name. No byte value catches these classes: the only backstop
is the engine's typed `context_length_exceeded`. REQ-589's over-budget offer does **not** cover
them — it fires only when a *skill expansion* exceeds the budget, and here the harness believes
the turn fits.

**What this means for the next change:** any argument of the form "the byte half is safe because
2 B/token is conservative" is unsound. The real bound requires a tokenizer, which is the shape a
future REQ should take if the local tier needs a genuine guarantee rather than a heuristic.
