# The context budget

Every turn is assembled under a budget belonging to the **route it took**, in
two currencies: whitespace words and bytes. A block enters while both fit; past
either, the oldest are dropped and the newest is middle-elided in place.

A remote route derives both from the declared window
(`capabilities.max_context`) less the 1,024 tokens reserved for the reply:
words are `usable × 2/3`, bytes `usable × 2`. That is ≈3 bytes per word, and
real text is denser (`o200k_base`: prose 4.6 B/token, Rust 4.0, minified JSON
and path-heavy shell output 3.6), so **on a remote route the byte guard binds**
for prose and code. Random base64 (1.45 B/token) is the one class neither
covers.

A provider declaring no window gets 4,096 words / 32,768 bytes. The local tier
is not on that pair: it derives from the engine's own window, 21,162 words /
63,488 bytes.

The backstop is the provider's own "context length exceeded" — a typed error
ending the turn rather than retrying or faulting its health. It covers the
vendors whose wording Teton pins: OpenAI-compatible, Anthropic, Moonshot/Kimi,
`llama-server`. **Ollama is not among them** — it truncates an over-long
prompt instead of refusing, so the answer comes from a shortened prompt.

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

A window or cap deriving below **6,250 words / 50,000 bytes** is *floored*,
not honored — that pair is the smallest that holds the system prompt twice
over, repository notes included. The declaration is recorded, the floor runs,
`/verbose` adds `floored` to the bound, and doctor names the pair in force.
A floored route sends more than its window declares, and on an unpinned
provider nothing reports the overflow — which is why those marks exist.

## Nothing is clamped in silence

Dropping blocks, eliding one in place, or re-fitting after a mid-turn reroute
emits `context_pressure` and prints one line — `context: 3 older blocks
dropped to fit the 21,162-word budget (bound: local engine)` — whether or not
`/verbose` is on. An elided *newest* message is additionally a notice in the
turn's output: that is where the model would answer a prompt nobody sent. A
context the gate could **not** fit says so under its own name, once per turn.

## What one prompt can cost

The budget bounds one model call, not a prompt. A prompt runs up to `max_turns`
tool iterations — **12** on the local profile, **40** on a strong model, 25 on a
provider's own native profile — each re-sending the whole context, so a prompt
on a large window can carry tens of millions of input tokens. There is no spend
cap; `context_budget_cap` lowers the ceiling, `teton cost` shows it.

Recording a window above 256,000 tokens says so once, where it is recorded:
`/provider setup`'s preview and `teton provider add --max-context` print the
per-call pair, the 25-call worst case, and the cap key. A notice only — no cap
is written; the window you declare is still the budget.

## Repository notes

`TETON.md` at the session root — or `AGENTS.md` if there is none, never
`CLAUDE.md` — is read at a **project** root and rendered as the last region of
the system prompt: the repository's description of itself, not instructions.
Cap **8,192 bytes**, or a quarter of the route's byte budget where that is
smaller; past it the file is cut at a line boundary under a marker naming the
bytes dropped and the cap. Every route this build derives reaches the full
8,192 — a floored route included, which is what the 50,000-byte floor above
buys — so the quarter rule is a bound, not something you will meet.

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

## When the repository has none, Teton offers to write one

The **first prompt turn** of a session whose root is a project with no
`TETON.md` and no `AGENTS.md` raises one permission prompt: may Teton walk this
tree, spend one model call, and write `TETON.md`. Accepted, the file is written
and loaded on that same turn — the prompt that raised the offer is answered with
the notes already resident. The launch banner (and the `/cd` line) says `no
TETON.md here — Teton will offer to write one on your first prompt`, so the
prompt is not a surprise.

Once per session, per root. A decline is remembered **for that session only** —
Teton never remembers a permission answer across sessions — and a `/cd` into
another project with no notes asks again, for that root. The answer is keyed
`repo_context:generate:<root>`, so one repository's yes never answers for
another, and moving the root forgets it.

Never at session create: a permission prompt needs a turn to ride.

## `[context] generate`

    generate = ask       # offer on the first prompt — the default
    generate = always    # write without asking
    generate = never     # never offer

`ask` is the shipped posture and the only one that ever draws a prompt. The
permission level outranks all three: at `plan` nothing is written and no offer
is raised — one line names `/context init` instead — and at `full` the file is
written unprompted, as `full` runs every mutation unprompted. `guarded` and
`edits` ask.

**`always` is the unattended opt-in, and its breadth is whatever project the
session is in.** An unattended session — piped stdin, any surface taking no
typed input — cannot answer a prompt, so under `ask` it writes nothing and
blocks on nothing: the client refuses the offer **without reading a line**, and
the next line of stdin is still your next prompt. Under `always` that same
session writes `TETON.md` into whichever project it was launched in, at every
level but `plan`, with no prompt and no allowlist between it and the working
tree. It is an automation opt-in with the same character as `[skills]
trusted_project_roots`, and this paragraph is the whole of the small print.

`never` stops the offer — no prompt, no walk, no model call — and stops nothing
else: `/context init` still writes, because that is your own act rather than
Teton's offer. An **empty** (or whitespace-only) `TETON.md` is the other durable
stop: the loader counts it present, so no offer is raised and no block is
resident. `touch TETON.md` is a supported way to say "not here".

An older client that does not know the offer's permission subject refuses it
without asking anyone, and the session proceeds cold — stated on the surface,
never a write nobody approved.

## Writing one on purpose

`/context init [--force]` inside a session; `teton context init [--force]` from
a shell, which creates a one-shot session at your cwd, answers the gate on its
own terminal and closes. One pipeline behind two doors, so both produce the same
bytes from the same evidence.

`init` runs even when `generate = never`. It still asks — explicit is not the
same as consented, and a `plan` session is still refused. Without `--force` an
existing file is left alone and one line names its size and the flag; with it
the prompt says **replace** rather than write, and the replacement is a temp
file and a rename, never a truncate-and-hope.

## What the draft is made of

After consent, and never before — nothing scans at launch:

- the **whole tree**, breadth-first at every depth, under the tool walk's budget
  (100,000 entries, 10 seconds), rendered as a listing with per-directory file
  counts by extension, so a deep `src/main/java/com/…` layout is seen whole;
- every present member of a closed table of documents, **whole to 16 KiB**
  each: the README, `CONTRIBUTING.md`, `ARCHITECTURE.md`, one build manifest per
  ecosystem (`Cargo.toml`, `package.json`, `pyproject.toml`, `go.mod`,
  `Makefile`, `pom.xml`, `Dockerfile`, …), every workspace member's manifest
  found in the listing, the **names** of `.github/workflows/*`, and
  `.adlc/context/*`;
- the **first 4 KiB** of every present member of a closed table of entry-point
  file names, at any depth — `main.rs`, `lib.rs`, `mod.rs`, `index.ts`,
  `__init__.py`, `main.go`, `App.swift`, `Main.java` and their kin, which is
  where a codebase says what it is.

Nothing outside those two tables is opened, and nothing outside the jail is
listed or followed. Assembly is in priority order — tree, manifests, README,
entry points — and stops at the route's byte budget. A walk that hit its budget
or a tree cut at a depth is written into the file's first line and printed on
the surface; it is never swallowed.

Every file read mints a provenance id, so the drafting call is judged at egress
exactly as a tool-bearing turn is. A file a privacy boundary covers is
**dropped before the call** and counted, so no covered byte is in the outbound
body. Directory and file names are metadata and are not excluded.

## The call, its tier, and its cost

The draft is a duty under its own category, `draft`, bound to the **`think`**
tier at compile time — the one place in the harness where the expensive model is
the cheap choice, because the file is written once and then read at the start of
every session afterwards. Move it like any other row: `/policy set-category
draft local`. A machine with no remote provider drafts on the local tier, and
the header says which tier wrote it.

**One model call per repository.** It lands in the ledger as one cost row under
the `draft` category, so `/cost` shows it on its own line rather than folded
into the prompt that triggered it; `/verbose` prints the drafting line with the
tier, the entries walked and the input tokens.

## What a generated file says about itself

The first line, counted inside the 8,192-byte cap:

    > Generated by Teton on 2026-09-03 (think tier; tree cut at depth 6). Edit freely — Teton reads this file at every session start.

The date, the tier that served the draft, whatever the walk had to leave out,
and the invitation — which is the load-bearing half. The body follows a fixed
section order (Purpose, Layout, Build & test, Conventions, Where to look), so
generated files look alike across repositories, and it is bounded to the cap
*less* the header before it is written, so the loader never truncates a file
Teton wrote. `/context` reports `origin: generated`; nothing else in the loader
branches on that, and Teton never re-marks a file it once generated.

The write is create-new, mode `0644`, and a symlink at the path is refused. A
`TETON.md` that appeared between consent and write — a checkout, another
session — is not clobbered: nothing is written and one line says so. There is no
regeneration and no freshness check; the file is yours from the moment it lands.

## When generation fails

Walking, drafting, writing, reading back — a failure in any of the four ends
generation with **one line naming the stage and the cause**, leaves **no file**
(a partial write is removed), and lets the turn's own prompt proceed cold. The
session records the failure so the offer is not raised again that session, and
the line names `/context init` as the way to retry. A provider error here
degrades that provider's health no more than any other duty failure does.

## With `[privacy] redact = true`

The scan reads the **whole** outbound body, so a scanned route cannot assemble
one the scan would refuse: bytes are bounded at 184,265, the word figure stays
window-derived. The bound is the chunk cap less the body's overhead — which the
notes raised to 23 KiB, taking the chunk cap 3 → 4, so it *rose* from 141,224
and a full body costs up to 5 scan calls. Only when `redact` is on, which it is
not by default.
