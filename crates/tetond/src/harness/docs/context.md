# The context budget

Every turn is assembled under a budget belonging to the **route it took**, in
two currencies: whitespace words and bytes. A block enters while both fit;
past either, the oldest are dropped and the newest is middle-elided in place.

A remote route derives both from the declared window
(`capabilities.max_context`) less the 1,024 tokens reserved for the reply:
words are `usable × 2/3`, bytes `usable × 2`. That is ≈3 bytes per word, and
real text is denser (`o200k_base`: prose 4.6 B/token, Rust 4.0, minified JSON
and path-heavy shell output 3.6), so **on a remote route the byte guard
binds** for prose and code. Random base64 (1.45 B/token) is the one class
neither covers.

The backstop is the provider's own "context length exceeded" — a typed error
ending the turn rather than retrying or faulting its health. It covers the
vendors whose wording Teton pins: OpenAI-compatible, Anthropic, Moonshot/Kimi,
`llama-server`. **Ollama is not among them** — it truncates an over-long
prompt instead of refusing, so the answer comes from a shortened prompt.

The local tier, and any route whose provider declares no window, run under a
fixed pair: 4,096 words / 32,768 bytes.

## The bound

Computed once, where the route is decided; `/verbose` prints it on the route
line (`· budget 665,984 words / 2 MB (bound: window)`). One of five:

- `window` — the declared window.
- `unknown window` — none declared, so the default pair; doctor says so.
- `user cap` — `context_budget_cap` below the window.
- `redact scan` — bytes held to what the scan covers.
- `local engine` — a local-tier route.

On the wire they are snake_case (`default_unknown` reads as `unknown window`).

## Declaring a window

    teton provider add <id> … --max-context 128000 [--context-budget-cap <n>]

`/provider setup` records the recipe's window when the chosen model is that
recipe's example; `config/set` carries both keys. `teton doctor` and `teton
provider list` print a `window:` column; doctor advises on a provider
declaring none and on a cap at or above its window (inert, not invalid).

`context_budget_cap` is the cost knob — it holds a large window to a smaller
budget. Absent, the declared window is the cap.

A window or cap deriving below **2,048 words / 16,384 bytes** is *floored*,
not honored — that pair is the smallest that still holds the system prompt.
The declaration is recorded, the floor runs, `/verbose` adds `floored` to the
bound, and doctor names the pair in force.
A floored route sends more than its window declares, and on an unpinned
provider nothing reports the overflow — which is why those marks exist.

## Nothing is clamped in silence

Dropping blocks, eliding one in place, or re-fitting after a mid-turn reroute
emits `context_pressure` and prints one line — `context: 3 older blocks
dropped to fit the 4,096-word budget (bound: local engine)` — whether or not
`/verbose` is on. An elided *newest* message is additionally a notice in the
turn's output: that is where the model would answer a prompt nobody sent. A
context the gate could **not** fit says so under its own name, once per turn.

## What one prompt can cost

The budget bounds one model call, not a prompt. A prompt may run up to 25 tool
iterations, each re-sending the context, so on a 1,000,000-token window (≈666k
words per call) one prompt can carry ≈25 million input tokens. There is no
spend cap; `context_budget_cap` lowers the ceiling, `teton cost` shows it.

Recording a window above 256,000 tokens says so once, where it is recorded:
`/provider setup`'s preview and `teton provider add --max-context` print the
per-call pair, the 25-call worst case, and the cap key. A notice only — no cap
is written; the window you declare is still the budget.

## With `[privacy] redact = true`

The scan reads the **whole** outbound body, so a scanned route cannot assemble
one the scan would refuse: bytes are bounded at ≈89 KB, the word figure stays
window-derived. Only when `redact` is on, which it is not by default.

