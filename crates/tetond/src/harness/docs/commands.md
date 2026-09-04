# Commands — the session's built-in `/` commands

These are the commands this session recognises. **You cannot run any of them.**
There is no tool that dispatches a built-in command, and `shell` will not help:
they are not programs, they are the session's own verbs. What you do with this
page is name the one command the user should type, and then stop.

That is the whole protocol, and it is short because the failure it replaces was
long: a model asked whether the transcript was on, had no idea `/transcript`
existed, and spent seven tool calls searching a repository for a setting that is
never in a repository.

## How to answer a question about session state

When a user asks whether something is on — the transcript, repository context,
verbose notices, the effort level, the permission level — the answer is one
sentence naming the command, and no tool call:

> Type `/transcript` — it prints the state and the file's path. I cannot run it.

Do not read a config file to find out. Teton's own configuration lives in its
state directory, never inside the repository you are working in, and a
configuration file you find in a repository belongs to some other tool. Do not
search the tree. Do not guess from a filename.

## The commands

- **`/help`** — list the commands this session knows.
- **`/cost`** — show the cost report for this machine.
- **`/effort`** — show or set the reasoning effort: /effort [low|medium|high|xhigh|max].
- **`/model`** — show the model the local tier is on.
- **`/model set`** — switch the local tier to a catalog model: /model set <name>.
- **`/model list`** — show the model catalog and each entry's fit for this machine. *(same as `teton model list`)*
- **`/model status`** — report the recorded model decision and the weights' install state. *(same as `teton model status`)*
- **`/clear`** — drop this session's retained conversation; the next prompt starts fresh.
- **`/cd`** — move this session's root — the directory tools are scoped to; bare, print it.
- **`/projects`** — list the projects this machine knows, each with the /cd that moves there.
- **`/verbose`** — toggle the routing and turn-end notices for this session.
- **`/transcript`** — record this session to a file, or stop: /transcript [on|off]; bare, show the state.
- **`/context`** — carry this repository's notes in the prompt, or stop: /context [on|off]; bare, show the state.
- **`/context init`** — write this repository's TETON.md now: /context init [--force] (asks first).
- **`/permissions`** — show or set this session's permission level: /permissions [level].
- **`/web setup`** — set up web lookup: pick a tier, name a backend, confirm before anything is written.
- **`/web allow`** — lift this session's web taint restriction; grants no new tier.
- **`/web refresh`** — drop a URL's cached copy so the next lookup re-fetches: /web refresh <url>.
- **`/shell allow`** — lift this session's local-tier pin after an unknown-reach shell command; typed input only.
- **`/provider setup`** — register a provider and route a tier to it: /provider setup [vendor] [tier].
- **`/provider test`** — test a registered provider with one consented call: /provider test <id>.
- **`/provider list`** — list the providers registered on this machine, with what each one calls. *(same as `teton provider list`)*
- **`/provider add`** — register a provider by hand; the key is asked for, never typed on the line. *(same as `teton provider add`)*
- **`/boundary list`** — list the privacy boundaries: path globs whose content never leaves this machine. *(same as `teton boundary list`)*
- **`/boundary add`** — add a privacy boundary over a path glob: /boundary add <glob>. *(same as `teton boundary add`)*
- **`/policy show`** — show the effective routing table and where each tier and category resolves. *(same as `teton policy show`)*
- **`/policy set-tier`** — route a tier to a provider: /policy set-tier <tier> <provider>. *(same as `teton policy set-tier`)*
- **`/policy set-category`** — route one category ahead of its tier: /policy set-category <category> <provider>. *(same as `teton policy set-category`)*
- **`/doctor`** — diagnose the daemon, socket, model state and providers. *(same as `teton doctor`)*
- **`/quit`** — end the session, exactly as Ctrl-D does.
## The `teton` twins

Rows marked *(same as `teton …`)* are literally the same command: the session
row parses and renders through the shell command's own code, so the two cannot
drift. Several others have `teton` equivalents that are not marked here because
the session row predates the shell one or carries a confirmation flow of its
own — `teton --help` is the authority on what the shell offers.

A shell twin is still **the user's** to run. You have `shell`, but running
`teton …` from it would reach a second daemon connection with none of this
session's state, and several of them read a credential. Name the `/` form.

## What is not here

`/name` commands the *user* wrote — skills — are not built-ins and are not on
this page. `/help` lists those alongside these, and `teton_docs skills` explains
where they load from. You can run a skill, through the `skill` tool, and only
through it.
