# The context budget

Every turn is assembled under a budget that belongs to the **route it took**,
in two currencies: whitespace words and bytes. A block enters the prompt only
while both fit; past either, the oldest blocks are dropped and the newest is
middle-elided in place.

A remote route derives both from the provider's declared window
(`capabilities.max_context`) less the 1,024 tokens reserved for the reply:
words are `usable × 2/3`, bytes are `usable × 2`. That is ≈3 bytes per word,
and real text is denser — measured with `o200k_base`: prose 4.6 B/token, Rust
4.0, minified JSON and path-heavy shell output 3.6 — so **on a remote route it
is the byte guard that binds** for prose and for code. Random base64 (1.45
B/token) is the one class neither guard covers; the provider's own
"context length exceeded" is the backstop, and it ends the turn with a typed
error instead of retrying or blaming the provider's health.

The local tier, and any route whose provider declares no window, run under the
fixed pair: 4,096 words / 32,768 bytes.

## The bound

Which constraint bound the budget is computed once, where the route is
decided. `/verbose` prints it on the route line —
`· budget 665,984 words / 2 MB (bound: window)` — as one of five:

- `window` — the declared window.
- `unknown window` — none declared, so the default pair. Doctor says so.
- `user cap` — `context_budget_cap` is set below the window.
- `redact scan` — the byte budget is held to what the scan covers (below).
- `local engine` — a local-tier route.

Those are the words the bound is printed in. On the wire the same five are
snake_case (`default_unknown` is what a user reads as `unknown window`), so a
script reading `route_decided` or `context_pressure` matches the wire spelling
and a person reads the words above.

## Declaring a window

    teton provider add <id> … --max-context 128000 [--context-budget-cap <n>]

`/provider setup` records the recipe's window when the model chosen is that
recipe's example model; `config/set` carries both keys too. `teton doctor` and
`teton provider list` print a `window:` column on every row, and doctor advises
on a provider that declares none and on a cap that sits at or above its window
(inert, not invalid). A window *smaller* than the local default is legal and
yields a smaller budget — Ollama's served default is 4,096.

`context_budget_cap` is the cost knob: it holds a large window to a smaller
budget. Absent, the declared window is the cap.

## Nothing is clamped in silence

Dropping blocks, eliding one in place, or re-fitting after a mid-turn reroute
emits `context_pressure` and prints one line — `context: 3 older blocks dropped
to fit the 4,096-word budget (bound: local engine)` — whether or not
`/verbose` is on. An elided *newest* message is additionally a notice in the
turn's own output, because that is the case where the model would answer a
prompt the user did not send.

## What one prompt can cost

The budget bounds a single model call, not a prompt. A prompt may run up to 25
tool iterations and each one re-sends the context, so on a 1,000,000-token
window — ≈666k words per call — a single prompt can carry up to ≈25 million
input tokens. There is no spend cap; `context_budget_cap` is the knob that
lowers the ceiling, and `teton cost` is where the spend shows up.

## With `[privacy] redact = true`

The redact scan reads the **whole** outbound body, so a scanned route cannot
assemble one the scan would refuse. The byte budget is bounded at ≈89 KB and
the bound reads `redact scan`; the word figure stays window-derived. This
applies only when `redact` is on — it is off by default, and the web tier's
own scan covers lookups, not the turn body.
