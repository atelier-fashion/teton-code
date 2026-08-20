# The context budget

Every turn is assembled under a budget belonging to the **route it took**, in
two currencies: whitespace words and bytes. A block enters the prompt only
while both fit; past either, the oldest blocks are dropped and the newest is
middle-elided in place.

A remote route derives both from the declared window
(`capabilities.max_context`) less the 1,024 tokens reserved for the reply:
words are `usable × 2/3`, bytes `usable × 2`. That is ≈3 bytes per word, and
real text is denser (`o200k_base`: prose 4.6 B/token, Rust 4.0, minified JSON
and path-heavy shell output 3.6), so **on a remote route the byte guard
binds** for prose and code. Random base64 (1.45 B/token) is the one class
neither covers; the provider's own "context length exceeded" is the backstop —
a typed error ending the turn rather than retrying or faulting its health.

The local tier, and any route whose provider declares no window, run under a
fixed pair: 4,096 words / 32,768 bytes.

## The bound

Which constraint bound it is computed once, where the route is decided.
`/verbose` prints it on the route line — `· budget 665,984 words / 2 MB
(bound: window)` — as one of five:

- `window` — the declared window.
- `unknown window` — none declared, so the default pair. Doctor says so.
- `user cap` — `context_budget_cap` set below the window.
- `redact scan` — bytes held to what the scan covers (below).
- `local engine` — a local-tier route.

On the wire the same five are snake_case (`default_unknown` is what a person
reads as `unknown window`).

## Declaring a window

    teton provider add <id> … --max-context 128000 [--context-budget-cap <n>]

`/provider setup` records the recipe's window when the chosen model is that
recipe's example; `config/set` carries both keys. `teton doctor` and `teton
provider list` print a `window:` column, and doctor advises on a provider
declaring none and on a cap at or above its window (inert, not invalid).

`context_budget_cap` is the cost knob: it holds a large window to a smaller
budget. Absent, the declared window is the cap.

A window or cap deriving below **2,048 words / 16,384 bytes** is *floored*,
not honored: that pair is the smallest budget that still holds the harness's
own system prompt, and under it no turn could be assembled at all. The
declaration is recorded, the floor is what runs, `/verbose` adds `floored` to
the bound, and doctor names the pair in force.

## Nothing is clamped in silence

Dropping blocks, eliding one in place, or re-fitting after a mid-turn reroute
emits `context_pressure` and prints one line — `context: 3 older blocks dropped
to fit the 4,096-word budget (bound: local engine)` — whether or not
`/verbose` is on. An elided *newest* message is additionally a notice in the
turn's output: that is where the model would answer a prompt nobody sent. A context the gate could **not** fit — it will neither drop
its last block nor clamp it to nothing — says so under its own name:
`context: could not be fitted to the … budget … — the turn was sent over
budget`.

## What one prompt can cost

The budget bounds a single model call, not a prompt. A prompt may run up to 25
tool iterations, each re-sending the context, so on a 1,000,000-token window
(≈666k words per call) one prompt can carry ≈25 million input tokens. There is
no spend cap; `context_budget_cap` lowers the ceiling and `teton cost` is where
the spend shows up.

Recording a window above 256,000 tokens says so once, where it is recorded:
`/provider setup`'s preview and `teton provider add --max-context` both print
the per-call pair, the 25-call worst case, and the cap key. A notice and
nothing else — no cap is written; the window you declare is still the budget.

## With `[privacy] redact = true`

The scan reads the **whole** outbound body, so a scanned route cannot assemble
one the scan would refuse. Bytes are bounded at ≈89 KB and the bound reads
`redact scan`; the word figure stays window-derived. Only when `redact` is on,
which it is not by default.
