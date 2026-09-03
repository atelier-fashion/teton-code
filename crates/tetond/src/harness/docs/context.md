# The context budget

Every turn is assembled under a budget belonging to the **route it took**, in
two currencies: whitespace words and bytes. A block enters while both fit; past
either, the oldest are dropped and the newest is middle-elided in place.

A remote route derives both from the declared window
(`capabilities.max_context`) less the 1,024 tokens reserved for the reply:
words `usable × 2/3`, bytes `usable × 2` — ≈3 bytes per word, and real text is
denser, so **the byte guard binds on a remote route**. A provider declaring no
window gets 4,096 words / 32,768 bytes; the local tier derives its pair from the
engine's window (21,162 / 63,488).

The backstop is the provider's typed "context length exceeded", ending the turn
without faulting its health; Teton pins that wording for OpenAI-compatible,
Anthropic, Moonshot/Kimi and `llama-server`. **Not Ollama** — it truncates an
over-long prompt instead, so the answer comes from a shortened one.

## The bound

`/verbose` prints it on the route line, one of five: `window`; `unknown window`
(none declared); `user cap` (`context_budget_cap` below the window); `redact
scan` (bytes held to what the scan covers); `local engine`.

## Declaring a window

    teton provider add <id> … --max-context 128000 [--context-budget-cap <n>]

`teton doctor` and `teton provider list` print a `window:` column, and doctor
advises on a provider declaring none. `context_budget_cap` is the cost knob;
absent it, the window is the cap.

A window or cap deriving below **2,048 words / 16,384 bytes** is *floored* — the
smallest pair that still holds the system prompt. `/verbose` says `floored` and
doctor names the pair; such a route sends more than its window declares, and an
unpinned provider reports nothing.

## Nothing is clamped in silence

Dropping blocks, eliding one in place, or re-fitting after a mid-turn reroute
emits `context_pressure` and prints a line naming what was dropped and the
budget it was fitted to, whether or not `/verbose` is on. An elided *newest*
message is also a notice in the turn's output: that is where the model would
answer a prompt nobody sent.

## What one prompt can cost

The budget bounds one model call, not a prompt. A prompt runs up to `max_turns`
tool iterations — **12** on the local profile, **40** on a strong model — each
re-sending the whole context, so a prompt on a large window can carry tens of
millions of input tokens. No spend cap: `context_budget_cap` lowers the ceiling,
`teton cost` shows it.

## Repository notes

`TETON.md` at the session root — or `AGENTS.md` if there is none, never
`CLAUDE.md` — is read at a **project** root and rendered as the last region of
the system prompt: the repository's description of itself, not instructions.
Cap **8,192 bytes**, or a quarter of the route's byte budget where that is
smaller (a floored route: 4,096); past it the file is cut at a line boundary
under a marker naming the bytes dropped and the cap.

`[context] repo_file = false` turns it off durably — the file is never opened.
`/context on|off` is session-scoped and never written; bare `/context` reports
the state, the file and its resident bytes. A file a privacy boundary covers is
not loaded, and the state says so.

Re-read at the **start** of a prompt turn when `mtime` or `len` changed, never
mid-turn; `/cd` re-reads under the new root, `/clear` keeps it — system prompt,
not conversation.

It rides every call, so a prompt carries it up to `max_turns` times: put in it
what a session needs every time — layout, build and test commands, conventions —
not what it needs once.

## With `[privacy] redact = true`

The scan reads the **whole** outbound body, so a scanned route cannot assemble
one the scan would refuse: bytes are bounded at 184,265, the word figure stays
window-derived. The bound is the chunk cap less the body's overhead — which the
notes raised to 23 KiB, taking the chunk cap 3 → 4, so it *rose* from 141,224
and a full body costs up to 5 scan calls. Only when `redact` is on, which it is
not by default.
