# Skills — the user's own `/` commands

A **skill** is a Markdown file the user wrote. Teton registers one `/name` per
file and expands it into a prompt turn — the body passed **as written** to one
model with five tools.

## Where they come from

Four locations, one level deep, no recursion:

    ~/.claude/skills/<name>/SKILL.md    <root>/.claude/skills/<name>/SKILL.md
    ~/.claude/commands/<name>.md        <root>/.claude/commands/<name>.md

The name is the directory name or file stem (`^[a-z0-9][a-z0-9_-]{0,63}$`),
never a frontmatter `name`. A built-in row beats a skill, a project skill beats
a user one, `skills/` beats `commands/`; the loser is listed as shadowed and
does not dispatch. Discovery re-runs on `/cd`; there is no watcher. `CLAUDE.md`, agents
and hooks are **not** loaded.

`/help` lists what registered, with sources, and names anything skipped with its
reason (oversize past the 64 KiB cap, unreadable, malformed, badly named).

## What an invocation is

`/name <rest>` is exactly one user-role prompt turn: a preamble naming the
command and its file, then the body with `$ARGUMENTS` replaced by `<rest>` **as
typed** (not split, quotes not interpreted) and `$1`…`$N` by its
whitespace-split tokens; with no placeholder in the body and a non-empty
`<rest>`, a closing `ARGUMENTS: <rest>` line. From there it is an ordinary
prompt: same classifier, routing, permission level, egress gate, cost row.

Frontmatter reads only `name`, `description`, `argument-hint`; every other key
(`allowed-tools`, `model`, `agent`, `hooks`, …) is inert and listed by
`/verbose`. A body saying "run this at `full`" is a sentence, not a setting. The
model cannot invoke a skill: name it and let the user type it.

## Dynamic context — `` !`cmd` ``

A body may inline a command's output at expansion time; substitution runs first,
so the command is consented to as it will run. It runs under the skill's **own**
permission key, `skill:<source>:<name>`, never `shell`'s — no grant crosses
between the two, and project grants are dropped on `/cd`. Default posture:
`guarded` and `edits` ask once per invocation with every command shown verbatim;
`plan` does not run them; `full` runs them. On piped stdin at a level that would
ask, the client refuses without reading stdin: the next line is the next prompt,
never a `y`.

Commands run in document order, session root as cwd, under the `shell` tool's
jail, timeout and output cap. One that did not run leaves
``[dynamic context not run: `cmd` — reason]`` in its place, so the turn says
what it lacks; it can be asked for with `shell` under `shell`'s own gate.

## Carried whole or refused

A skill turn is never elided. If the expansion plus the system prompt exceeds
the route's budget (`teton_docs context`) it is refused before anything is sent
— the body alone is checked before consent is asked, the whole expansion again
after, and the message says which:

    `/proceed` does not fit this route's context budget: … about 9,000 words /
    60.6 KB against 4,096 words / 32 KB (bound: local engine).

The remedy is a route with a declared window.

## Provenance

The skill file rides the turn as a source: under a configured privacy boundary a
project skill pins the turn as reading it would, a user skill outside the root
under the stricter unknown rule. Dynamic-context output carries `Unknown`
provenance, as all shell output does — so where a boundary is configured **any**
invocation that ran a command pins its turn local, and one too large for the
local budget is refused there, not served remotely.

## Fidelity

Nothing is translated or rewritten. References to `Agent`, `Task`, `Skill`,
`Workflow`, subagents or Claude Code tool names pass through with nothing behind
them. Prompt-template skills work. A skill that invokes other skills or
dispatches subagents degrades to what one model with five tools can do and
**stalls** at its first "invoke the skill" step. Say so plainly rather than
pretend a step ran.
