# Skills — the user's own `/` commands

A **skill** is a Markdown file the user wrote. Teton registers one `/name` per
file and expands the body **as written** into a turn for one model with the
session's tools, `skill` among them.

## Where they come from

Four locations, one level deep, no recursion:

    ~/.claude/skills/<name>/SKILL.md  <root>/.claude/skills/<name>/SKILL.md
    ~/.claude/commands/<name>.md      <root>/.claude/commands/<name>.md

The name is the directory or file stem (`^[a-z0-9][a-z0-9_-]{0,63}$`), not a
frontmatter `name`. Built-in beats skill, project beats user, `skills/` beats
`commands/`; the loser is shadowed and does not dispatch. Discovery re-runs on
`/cd`; no watcher. `CLAUDE.md`, agents and hooks are **not** loaded.

`/help` lists what registered and why anything was skipped: 64 KiB cap,
unreadable, malformed, badly named.

## What an invocation is

Two callers, one expander. `/name <rest>` is one user-role prompt turn; the
model's `skill { name, args }` is a tool result inside the turn it is in. Each
is a frame naming the skill and its file — the model's says the body is
instructions, not data — then the same bytes: `$ARGUMENTS` replaced by the
arguments **as typed** (unsplit, quotes intact) and `$1`…`$N` by their tokens;
with no placeholder and non-empty arguments, a closing `ARGUMENTS: <rest>` line.
A typed `/name` is then an ordinary prompt: same classifier, routing, level,
egress gate, cost row.

The `skill` tool exists only when some skill is model-invocable; with none it is
absent. Its description names those; a call with no name lists them. It is the
only path to a body outside the root; `read` stays jailed, companion files
included.

Frontmatter reads `name`, `description`, `argument-hint`, and two flags for who
may invoke: `disable-model-invocation: true` hides the skill from the model,
`user-invocable: false` makes it model-only (`/help` marks it `(model-only)`). A
non-boolean value reads as the safe one — user only. Every other key
(`allowed-tools`, `model`, `agent`, `hooks`, …) is inert, listed by `/verbose`.
A body saying "run this at `full`" is a sentence, not a setting.

## Dynamic context — `` !`cmd` ``

A body may inline a command's output; substitution runs first, so the command is
consented to as it will run. It runs under the skill's **own** key,
`skill:<source>:<name>`, never `shell`'s — no grant crosses, and project grants
drop on `/cd`. Default: `guarded` and `edits` ask once per invocation, each
command shown; `plan` does not run them, `full` does. On piped stdin at a level
that would ask, the client refuses without reading it: the next line is a
prompt, never a `y`.

Commands run in document order, session root as cwd, under `shell`'s jail,
timeout and output cap. One that did not run leaves ``[dynamic context not run:
`cmd` — reason]`` in its place.

## Carried whole or refused

A skill turn is never elided: if the expansion plus the system prompt exceeds
the route's budget (`teton_docs context`) it is refused before anything is sent
— the body alone before consent, the whole expansion after; the message names
both sizes and the bound. The remedy is a route with a big enough window; a
small declaration is floored.

## Provenance

Two rules. A **project** skill mints a root-relative source and pins the turn as
reading it would. A **user** skill (`~/.claude/…`) has no such identity: its
block is `Unknown` and pins the turn wherever **any** boundary is configured —
stricter than a `read` of those bytes. Dynamic output is `Unknown`, like all
shell output. So under a boundary a model invocation of a `~/.claude` skill runs
local, and one over the local budget is refused there, not sent remotely.

## Fidelity

Nothing is translated. `Agent`, `Task`, `Workflow`, subagents and Claude Code
tool names pass through with nothing behind them. A skill that invokes other
skills now runs them — that is what the `skill` tool is for — but one that
dispatches subagents degrades to this one loop. Say so rather than pretend a
step ran.
