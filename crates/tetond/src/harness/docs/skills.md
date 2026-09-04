# Skills — the user's own `/` commands

A **skill** is a Markdown file the user wrote: one `/name` per file, expanded
**as written** into a turn for one model with the session's tools, `skill`
among them.

## Where they come from

Four locations, one level deep, no recursion: `skills/<name>/SKILL.md` and
`commands/<name>.md`, each under `~/.claude/` and `<root>/.claude/`.

The `skill` tool is the only way **you** run one. There is no other route: not
`shell`, not a built-in command, not naming the file to `read`. A built-in `/`
command is the other way round — only the user runs those, and
`teton_docs commands` lists them.

The name is the directory or file stem (`^[a-z0-9][a-z0-9_-]{0,63}$`), not
frontmatter `name`. Built-in beats skill, project beats user, `skills/` beats
`commands/`; the loser is shadowed and does not dispatch. Discovery re-runs on
`/cd`; no watcher. `CLAUDE.md`, agents and hooks are **not** loaded.

`/help` lists what registered, and why not: 64 KiB cap, unreadable, malformed,
badly named.

## What an invocation is

Two callers, one expander. `/name <rest>` is one user-role prompt turn; the
model's `skill { name, args }` is a tool result inside the turn it is in. Each
is a frame naming the skill and its file — the model's says the body is
instructions, not data — then the same bytes: `$ARGUMENTS` replaced by the
arguments **as typed** (unsplit, quotes intact) and `$1`…`$N` by their tokens;
with no placeholder and non-empty arguments, a closing `ARGUMENTS: <rest>`
line.

The `skill` tool exists only when some skill is model-invocable; with none it
is absent. A call with no name lists them. It is the only path to a body
outside the root; `read` stays jailed.

Frontmatter reads `name`, `description`, `argument-hint`, and two flags for who
may invoke: `disable-model-invocation: true` hides the skill from the model,
`user-invocable: false` makes it model-only. A non-boolean value is safe **per
key**: unreadable `disable-model-invocation` hides it, unreadable
`user-invocable` changes nothing, so it stays invocable by **both**. Every
other key (`allowed-tools`, `model`, `agent`, `hooks`, …) is inert, listed by
`/verbose`. A body saying "run this at `full`" is a sentence, not a setting.

## Trusting a repository

Before a **project** skill expands, its repository is acknowledged once per
session, on both doors; declining refuses the turn. A user skill asks nothing;
`full` asks only on a shadow. With no terminal this and the command prompt
below refuse unread: the next line is a prompt, never a `y`. A **typed** run
needs the root in `[skills] trusted_project_roots`: matched whole, never
written unattended. No row answers for the model's door: an unattended model
reaches no project skill.

## Dynamic context — `` !`cmd` ``

A body may inline a command's output; substitution runs first, so what is
consented to is what runs. It runs under the skill's **own** key,
`skill:<source>:<name>`, never `shell`'s — no grant crosses, and project grants
drop on `/cd`. `guarded` and `edits` ask once per invocation, each command
shown; `plan` does not run them, `full` does.

Commands run in order, session root as cwd, under `shell`'s jail, timeout and
output cap. One that did not run leaves ``[dynamic context not run: `cmd` —
reason]``.

## Carried whole or refused

A skill turn is never elided: expansion plus system prompt over the route's
budget (`teton_docs context`) is refused before anything is sent — the body
alone before consent, the whole expansion after; the message names both sizes
and the bound. The remedy is a route with a bigger window.

## Provenance

Two rules. A **project** skill mints a root-relative source and pins the turn
as reading it would. A **user** skill (`~/.claude/…`) has no such identity: its
block is `Unknown` and pins the turn wherever **any** boundary is configured.
Dynamic output is `Unknown`, like all shell output. So under a boundary a model
invocation of a `~/.claude` skill runs local, and one over the local budget is
refused there.

## Fidelity

Nothing is translated: `Agent`, `Task`, `Workflow`, subagents and Claude Code
tool names pass through with nothing behind them. A skill that invokes other
skills now runs them, but one that dispatches subagents degrades to this one
loop. Say so rather than pretend a step ran.
