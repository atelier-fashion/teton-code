---
id: REQ-585
title: "User-defined slash commands from SKILL.md — the session discovers Claude Code-style skills and runs `/name` as a prompt expansion"
status: draft
deployable: true
created: 2026-08-19
updated: 2026-08-19
component: "cli"
domain: "clients"
stack: ["rust", "cli", "daemon", "json-rpc"]
concerns: ["developer-experience", "security", "extensibility", "privacy"]
tags: ["slash-commands", "slash-command", "skills", "skill-md", "user-defined-commands", "prompt-template", "prompt-expansion", "interactive-session", "repl", "help", "dynamic-context", "prompt-injection", "system-prompt", "self-config-guide", "context-budget", "adlc", "claude-code-compat", "dogfood"]
---

## Description

Teton's in-session `/` commands are a closed table: `COMMANDS` in
`crates/teton/src/slash.rs` both dispatches and generates `/help` (REQ-555
BR-7), and nothing in `teton` or `tetond` reads a `SKILL.md`, a
`.claude/commands/*.md`, or anything else under `.claude/`. REQ-555 named
this gap on purpose — *"ADLC skill-style user-defined commands (a
`/`-command that expands to a prompt template) — different feature, separate
REQ if wanted"* — and the dogfood record now says it is wanted. On 2026-08-19
a user launched `teton` beside the ADLC toolkit (seventeen skills under
`~/.claude/skills/<name>/SKILL.md`), asked the model *"are you able to
leverage the skills and framework available?"*, was told **yes**, typed
`/analyze teton code repo`, and got `unknown command: /analyze`. The day
before, BUG-180's trigger was the same ask — *"show me the skills"*. The
people who reach for Teton already have a shelf of prompt-template commands
they use every day in another agent; today Teton can neither run them nor —
until BUG-181 (fixed 2026-08-19, PR #188) — honestly say it cannot.

This REQ adds **user-defined slash commands discovered from Claude Code-style
skill files** and run as **prompt expansions**. The session looks in four
fixed places — the user's `~/.claude/skills/*/SKILL.md` and
`~/.claude/commands/*.md`, and the session root's `.claude/skills/*/SKILL.md`
and `.claude/commands/*.md` — reads each file's frontmatter (`name`,
`description`, `argument-hint`) and body, and registers one `/name` per file.
Typing `/name <rest>` produces exactly one user-role prompt turn: the body
with `$ARGUMENTS` replaced by `<rest>`, preceded by a one-line preamble
naming the command and where it came from. The turn then takes the same path
as typed text — same classifier and routing, same permission level, same
egress choke point, same cost attribution. The one Claude-Code-specific
behaviour with real security weight, `!`command`` dynamic context (a shell
command whose output is inlined at expansion time; every one of the seventeen
ADLC skills uses it for the ethos include), runs **only through the session's
permission gate under its own key** — never the `shell` tool's: `guarded` and
`edits` ask once per invocation with every command shown verbatim, `plan`
does not run them, `full` runs them, piped stdin refuses them without reading
a line — and a command that is not run leaves an explicit placeholder so the
model is told, not misled.

What this REQ deliberately is **not**: a Claude Code runtime. The body is
passed as written. Teton does not dispatch subagents, does not grow a `Skill`
tool the model can call, does not honor `allowed-tools`, `model`, `context:
fork` or hooks, and does not rewrite a skill's references to tools it lacks.

**The scope decision, resolved (OQ-0, 2026-08-19): the big skills are the
point — they are what automation runs.** The harness applies one context
budget on every tier — 4,096 whitespace tokens / 32 KiB, system prompt
included (`HarnessConfig` default; `from_harness_profile` inherits it for
remote tiers; the provider's `max_context` never reaches the harness) — and
today an oversized prompt is **middle-elided in place, silently**
(`ContextManager::truncate_to_budget`). Measured against that budget with the
~850-word system prompt and the 599-word ethos include every ADLC skill
inlines, ten of the seventeen skills fit and **seven do not on any tier**
(`/spec` 2,717 words, `/manifest`, `/analyze`, `/template-drift`, `/wrapup`,
`/sprint`, `/proceed` 7,222). The product owner's answer was that those seven
are the ones that matter. So **REQ-586 — a route-aware context budget — lands
first**: a remote route gets its provider's window, an unknown window is
stated rather than silently defaulted, and nothing is clamped in silence on
any tier. On top of it, this REQ's success bar is **every one of the
seventeen expands on a remote route whose window is declared** (`/proceed` at
8,671 words with ethos and system prompt needs roughly a 16k-token window
after REQ-586's safety ratio; the dogfood Kimi route has 128k) and **ten of
them expand on the local tier**; a skill turn that still does not fit its
route — the local tier, an unknown window, a redact-scan-bound route — is an
explicit refusal naming the skill, its size, the budget and the bound (BR-8),
never an elision.

**Automation, stated honestly.** "Automation" means unattended sessions —
piped stdin, a script driving `teton`. Two things follow. First, the
permission posture: at `full` (the level an unattended runner chooses)
dynamic context runs on a pipe without a prompt; at `guarded`/`edits` on a
pipe it is refused without reading a line (BR-6, BR-11), so a runner gets the
ethos include only by choosing `full` — the same choice it makes for every
`shell` call. Second, what this REQ does *not* buy: `/proceed` and `/sprint`
are written to **invoke other skills** at each gate ("invoke the actual
`/validate`… `/architect`…") and to **dispatch subagents**; Teton has neither
a model-invocable skill surface nor a subagent, and a repo-rooted session
cannot even `read` `~/.claude/skills/validate/SKILL.md` (the tool jail). So
after REQ-586 and this REQ, `/proceed REQ-xxx` *expands* and a single model
follows as much of it as one loop with five tools can — it will stall at the
first "invoke the skill" step. The two follow-ups that close that gap —
**model-invoked skills** (a `skill` tool that expands a registered skill into
the turn, honoring `disable-model-invocation`) and **subagent dispatch** — are
named in Deferred with a recommendation to spec them next; they are not
folded in here because each changes the security posture in a way this
validated v1 should not absorb silently.

Why the shape matters for this product in particular: a skill file is text a
user wrote on their own machine, so it is **user-role content and nothing
more** — it never enters the system prompt, it passes the same input guards
as a pasted paragraph, and a skill file under a `local-only` boundary pins
the turn exactly as reading that file would (BR-1 of the product charter is
not suspended because the text arrived through a `/` line). And discovery
is **four globs, one level deep** — REQ-583 just taught this codebase what a
search that becomes a disk crawl costs; a skill loader that walks `~/.claude`
would be the same incident with a different name.

This REQ is coupled to **BUG-181** (fixed): its sentence in the bundled
self-configuration guide says Teton loads nothing from `.claude/` or
`~/.claude`; this REQ amends that sentence so it stays true once skills *are*
loaded (BR-9), inside the constraints the guide's pinning tests impose.

## System Model

_Shapes below are illustrative — the field names and variant names are what
`/architect` decides; the constraints are the requirement._

### Entities

| Entity | Field | Type | Constraints |
|--------|-------|------|-------------|
| Skill (discovered command) | name | string | the directory name (`skills/<name>/SKILL.md`) or file stem (`commands/<name>.md`); must match `^[a-z0-9][a-z0-9_-]{0,63}$`; a frontmatter `name` that differs creates no second spelling |
| Skill | source | `user` / `project` | which of the two roots it was found under; `project` is derived from the session root (REQ-583's `SessionRoot`) and re-derived when it changes; when the project root resolves to the user root (a `home`-kind session), project discovery is skipped rather than duplicating every skill |
| Skill | path | string | the file actually read; shown home-relative (the `SessionRoot.display` convention), never as an absolute path that carries a username into a transcript or a remote payload |
| Skill | description | string | from frontmatter; one line, truncated to 200 chars after sanitization (5 of 17 ADLC descriptions are longer); rendered only through the `Surface` seam |
| Skill | argument_hint | string? | from frontmatter `argument-hint`; shown in `/help` |
| Skill | body | string | everything after the frontmatter; ≤ 64 KiB or the skill is skipped with a reason |
| Skill | dynamic_context | Command[] | every `!`command`` occurrence in the body, in document order, command text verbatim (after `$ARGUMENTS`/`$N` substitution — BR-4 precedes BR-6) |
| Skill | permission_key | string | the gate key the skill's dynamic context runs under — per skill, never `shell` (e.g. `skill:<name>`, illustrative) |
| Skill | ignored_keys | string[] | frontmatter keys Teton does not honor (`allowed-tools`, `model`, `effort`, `disable-model-invocation`, `user-invocable`, `context`, `agent`, `hooks`, …); listed in `/verbose`, otherwise inert |
| Skill | shadowed_by | `builtin` / `project` / none | set when a reserved name or a project skill owns the same name; a shadowed skill is listed, not dispatchable |
| SkillRegistry | skills | Skill[] | the full set, including shadowed and skipped entries; a pure function of the files read |
| SkillRegistry | skipped | (path, reason)[] | unreadable (incl. `EPERM`/TCC), malformed frontmatter, invalid name, oversized — counted and named, never silent; a directory under a root with no `SKILL.md` (the toolkit's `agents/`, `partials/`, `templates/`, …) is not a skill and not a diagnostic |
| SkillInvocation | name, raw_arguments | string, string | `raw_arguments` is the remainder of the typed line after the name with interior whitespace preserved and the line's edges trimmed as the classifier trims today; no quote interpretation |
| SkillInvocation | expansion | string | preamble line + substituted body (+ `ARGUMENTS:` line when the body has no placeholder) + dynamic-context results or placeholders |
| SkillInvocation | dynamic_results | (command, outcome)[] | per command, a typed outcome — ran (stdout, truncated?) / not run (reason) / failed (status) / timed out — never prose a renderer parses |
| SkillInvocation | provenance | source set | the skill file's path (new: prompt text carries no file provenance today) plus the dynamic commands' provenance, which is the `shell` tool's (`Unknown`) |

### Events

None required by the rules below. A skill invocation *is* a prompt turn, and
the existing turn/tool events carry it. If OQ-1 resolves to daemon-owned
expansion, one additive event — a `skill_invoked`-shaped notice with name,
source, bytes and dynamic-command count (illustrative) — is the natural
carrier for `/verbose` and for the cost ledger's attribution, and must be
ignorable by older clients; if it resolves to client-owned expansion, the
client's own echo line is the record.

### Permissions

| Action | Roles Allowed |
|--------|---------------|
| invoke `/name …` | the user at the session surface, TTY or piped stdin alike (REQ-555 BR-9: it produces a prompt turn, as typed text does) |
| model invoking a skill | not possible — no tool, no RPC, no recognition of a `/name` line inside a model reply (out of scope; see BR-9 for what the model *is* told) |
| dynamic-context commands (`!`cmd``) | the session's permission level decides, through the permission gate under the skill's **own key** (the level table's default posture: `guarded` ask, `edits` ask, `plan` deny, `full` allow) — never the `shell` tool's key, so a remembered `shell` grant does not un-ask skill context and a remembered skill grant does not free model-issued shell commands; the consent lists every command of the invocation, once |
| dynamic-context commands — on piped stdin | refused by the **client** without reading a line from stdin (new client-side rule; the REQ-555 BR-9 / REQ-582 write-gate precedent): the next stdin line stays the next prompt line and can never become a `y` |
| dynamic-context commands — where and how | session root as cwd, the `shell` tool's jail, timeout and output cap; outputs enter the expansion as untrusted content inside the existing tool-result envelope (envelope tags and frame labels neutralized there) |
| skill file contents reaching the system prompt, permission level, routing, effort, config, boundaries | never — a skill is user-role content; every frontmatter key other than `name`, `description`, `argument-hint` is inert |
| discovery reading the filesystem | the four fixed locations only, one level deep; nothing else is opened, listed or followed |

## Business Rules

- [ ] BR-1: **Discovery is four globs, one level deep, and nothing else.** The
  registry is built from exactly `~/.claude/skills/*/SKILL.md`,
  `~/.claude/commands/*.md`, `<session-root>/.claude/skills/*/SKILL.md` and
  `<session-root>/.claude/commands/*.md`. No recursion, no walk, no `..`.
  A root (or the `skills`/`commands` directory under it) may itself be a
  symlink and is followed — the dogfood machine's `~/.claude/skills` *is* a
  symlink into `~/Documents/GitHub/adlc-toolkit` — but a symlinked **entry**
  under a root is not followed. Because a root can resolve into a
  consent-guarded tree (`~/Documents` on macOS), an `EPERM`/TCC refusal is a
  named skip reason, not a crash and not silence; a missing directory is the
  normal case and costs nothing. When the project root resolves to the same
  path as the user root (a `home`-kind session, REQ-583), project discovery
  is skipped rather than registering every skill twice. Discovery runs at
  session start and again whenever the session root changes (`/cd`); there is
  no file watcher. Every entry that is found but not registered — unreadable,
  malformed frontmatter, invalid name, over 64 KiB — is counted and named with
  its reason in the registry's diagnostics; nothing is dropped silently, and
  a directory with no `SKILL.md` is simply not a skill (informed by REQ-583,
  LESSON-481).
- [ ] BR-2: **A skill's name is where it lives, and reserved names always
  win.** The name is the directory name (skills) or file stem (commands),
  validated against `^[a-z0-9][a-z0-9_-]{0,63}$`; a frontmatter `name` that
  differs does not create a second spelling (one spelling reaches one handler
  — REQ-555's rule). The reserved set is every built-in row and alias
  (`help`, `cost`, `quit`, `exit`, `model`, …), **the first word of every
  multi-word row** (`web`, `provider`, `boundary`, `policy` — a skill named
  `provider` would otherwise take `/provider foo` and lose `/provider list`
  to longest-match), and `teton` (claimed by REQ-582's `cli_line` recognition
  before the table is consulted). A skill with a reserved name is never
  dispatchable: the built-in runs, byte-for-byte as today, and the skill is
  listed as shadowed. Between a project skill and a user skill of the same
  name, the project skill wins and the user skill is listed as shadowed
  (informed by REQ-582, LESSON-537).
- [ ] BR-3: **`/help` lists skills from the registry that dispatches them.**
  REQ-555 BR-7 extends: a skill cannot be dispatchable without appearing in
  `/help`, and `/help` cannot list a dispatchable skill the table does not
  resolve. Skills appear in their own section after the built-in rows, one
  line each — `/name [argument-hint] — description (user|project)` — with
  shadowed entries marked and a closing diagnostic line (`N skills (user A,
  project B); M skipped: …`). The description and hint are file contents and
  therefore untrusted for the terminal: they render only through the
  `Surface` sanitizer, bounded to one line (informed by LESSON-517). The
  built-in section's bytes are unchanged from today (informed by REQ-555,
  REQ-582).
- [ ] BR-4: **An invocation is one prompt turn on the same path as typed
  text.** `/name <rest>` becomes exactly one user-role prompt turn whose text
  is: one preamble line naming the command and its home-relative source (`The
  user invoked /name (a command defined in <display path>); the instructions
  below are that command's body.`); the body with every `$ARGUMENTS` replaced
  by `<rest>` (interior whitespace preserved, no quote interpretation — the
  one place the session does *not* use REQ-582 ADR-2's tokenization) and
  `$1`…`$N` replaced by the whitespace-split tokens of `<rest>`; and, when
  the body contains no placeholder and `<rest>` is non-empty, a final line
  `ARGUMENTS: <rest>` (this is what makes `/proceed REQ-xxx` work — the
  shipped `proceed` skill has no `$ARGUMENTS`). Substitution happens **before**
  dynamic-context execution, so a `$ARGUMENTS` inside a `!`…`` runs
  substituted and the consent shows the command as it will run. No model call
  happens at expansion time. The turn then takes the same classifier and
  routing, the same permission level, the same egress choke point and the
  same cost row a typed prompt takes — "same path", not "indistinguishable":
  BR-7, BR-8 and BR-12 require the daemon to know it is a skill turn
  (informed by REQ-555, REQ-582).
- [ ] BR-5: **Skill content is user-role content and can change nothing
  about the session.** The expansion never enters the system prompt, and it
  passes the same input guards as any prompt text — control tokens and frame
  labels neutralized where the frame is written (ADR-009; a typed prompt's
  guards, no more and no less — an envelope tag in a skill *body* is prompt
  text, exactly as it would be if pasted; dynamic-context **output** goes
  inside the tool-result envelope, where envelope tags are neutralized too).
  Nothing in a skill file can change the permission level, the routing
  policy, the effort level, config, or a privacy boundary: every frontmatter
  key other than `name`, `description` and `argument-hint` is inert
  (`allowed-tools`, `model`, `effort`, `disable-model-invocation`,
  `user-invocable`, `context`, `agent`, `hooks` in particular) and is listed
  as ignored in `/verbose`. A skill body that says "run this at `full`" is a
  sentence the model reads, not a setting (informed by REQ-560, REQ-563 BR-5).
- [ ] BR-6: **Dynamic context runs only through the permission gate, under
  the skill's own key, and a command that did not run says so.** Each
  `!`command`` in the body is a shell command whose provenance is the skill
  file. It runs through the permission gate under a key that is **per skill
  and is not `shell`** — the gate today keys grants by tool name, so reusing
  `shell` would let "allow for this session" on one skill prompt free every
  later model-issued shell command, and an earlier allow-always on `shell`
  silently un-ask skill context (LESSON-495: the remembered key must encode
  the whole question; the question here is "may this skill's dynamic context
  run?"). The key takes the level table's default posture (`guarded` ask,
  `edits` ask, `plan` deny, `full` allow). At `guarded` and `edits` the
  session asks **once per invocation** with every command of the invocation
  listed verbatim (a prompt storm is REQ-560 BR-2's anti-pattern); the
  answer's options are the gate's existing ones (once / for this session /
  never), remembered under the skill's key; at `plan` they are not run; at
  `full` they run without asking. Commands run sequentially in document
  order, with the session root as cwd and the `shell` tool's jail, timeout
  and output cap; stdout enters the expansion as untrusted content inside
  the existing tool-result envelope. A command that is not run, fails, or
  times out leaves an explicit placeholder in its place — `[dynamic context
  not run: `<command>` — <reason>]` — so the model is told what it does not
  have and may ask for it with the `shell` tool under that tool's own gate; a
  command's failure never fails the invocation (informed by REQ-560,
  LESSON-495, LESSON-537).
- [ ] BR-7: **Provenance travels with the expansion; the charter's privacy
  boundary is not suspended by a `/`.** *New machinery, stated as such:*
  prompt text carries no file provenance today (`Provenance::User`), so the
  expansion must carry the skill file's path as a source — a skill file under
  a `local-only` boundary then pins the turn exactly as a `read` of that file
  would. Dynamic-context output carries what `shell` output carries today:
  `Unknown` provenance, which the egress inspector fails closed on whenever
  any boundary is configured. The consequence is stated rather than hidden:
  **on a boundary-configured machine, every skill invocation that ran a
  dynamic command pins its turn local** — all seventeen ADLC skills run the
  ethos include, so on such a machine they all run on the local tier. The
  egress choke point sees the expansion as it sees any prompt text (redact
  scan where configured). This is a BR-1-of-the-charter claim and carries an
  egress-capture test (informed by REQ-563).
- [ ] BR-8: **Bounded, and never silently truncated — on REQ-586's budget.**
  A body over 64 KiB is skipped at discovery with its reason (the largest
  ADLC skill, `proceed`, is 49.8 KiB — a cap that admits the real corpus and
  refuses a pasted transcript). A skill turn whose expansion plus the system
  prompt exceeds **the route's budget** (REQ-586 BR-1: the provider's window
  on a declared remote route, the default on the local tier or an unknown
  window, the scannable bound when the redact scan applies) is **refused
  before any model call** with a message naming the skill, its size, the
  budget and REQ-586's bound — `bound: default_unknown — set
  capabilities.max_context for <id>` is the one a new user will meet — and is
  never middle-elided into something the user did not invoke. Typed prompts
  keep REQ-586 BR-7's loud elision; the refusal is for skill turns only.
  Depends on REQ-586.
- [ ] BR-9: **The model is told the truth about commands, in one place, inside
  the guide's constraints.** The bundled self-configuration guide carries
  BUG-181's sentence — *"Teton loads nothing from `.claude/` or `~/.claude`
  (no skills, commands, CLAUDE.md, agents or hooks); the session's commands
  are exactly those `/help` lists, and only the user runs them."* — and this
  REQ amends it so it remains true: skills from those places *are* loaded and
  listed by `/help`; `CLAUDE.md`, agents and hooks still are not; the model
  still cannot invoke any command and should name it to the user. The
  amendment inherits the pinning test's constraints (one `/help` line, both
  paths named, "only the user runs", the "loads nothing from" phrase re-worded
  and the assertion updated — not deleted) and the guide's: one sentence, no
  second line containing "ask", no `teton …` shell form, and the resident
  prompt's byte headroom (`MIN_PROMPT_HEADROOM_BYTES`; BUG-181 had 1 byte to
  spare and raised the test-only ceiling 9→10 KiB — this amendment must fit
  in what that bought or pay for itself). The skill roster itself is **not**
  in the system prompt in v1 (OQ-2, bounded by the same headroom); the
  invocation preamble (BR-4) is how the model learns it is running one
  (informed by REQ-579 — the model hands off, the surface runs; BUG-181).
- [ ] BR-10: **No second parser, no change to what is not a skill.**
  `classify` stays pure and total, now over `(input, registry)`: a `/` line
  whose first token names a registered, unshadowed skill is a skill
  invocation; every other `/` line classifies exactly as today (built-in row,
  misuse, or unknown), `//` stays the escape hatch, and a plain line still
  reaches the model byte-identically. The unknown-command hint gains one
  case: when the typed name matches a **skipped** entry, the hint says why
  (`/analyze is a skill that was skipped: <reason>`); with no such entry the
  hint's bytes are unchanged. (A *shadowed* name never reaches the hint — the
  built-in or the project skill runs — so there is no shadow branch to word.)
  If the registry lives in the daemon (OQ-1), the client holds a snapshot
  refreshed when the session root changes; purity holds over the snapshot
  (informed by REQ-555, REQ-582, LESSON-537).
- [ ] BR-11: **Pipe-friendly, with one stated narrowing — and an explicit
  unattended posture.** Invocation is identical on a TTY and on piped stdin
  (REQ-555 BR-9), and the only difference is BR-6's: with no terminal and a
  level that would *ask*, the **client** answers the dynamic-context consent
  with a refusal **without reading stdin** — today the shell consent on a
  pipe is answered from the next stdin line, and a pasted second line must
  not become a `y` (LESSON-537's shape) — so the commands are not run and
  their placeholders say no human could be asked. At `full` there is nothing
  to ask: dynamic context runs on a pipe exactly as on a TTY, which is the
  automation posture — an unattended runner that wants the ethos include
  chooses `full` for the session, the same choice it makes for every `shell`
  call, and `plan` refuses on a pipe as it does on a TTY.
- [ ] BR-12: **Observable, not noisy.** Every invocation echoes one line
  naming the skill, its source and size, and how many dynamic commands ran
  (`/status → skill status (user, 5.3 KB, 4 dynamic commands)`); the body is
  never printed (it is in the file). `/verbose` adds the home-relative path,
  the ignored frontmatter keys and each dynamic command's typed outcome. The
  turn appears in `/cost` as the prompt turn it is.
- [ ] BR-13: **The body is passed as written; fidelity is stated, not
  faked.** Teton does not rewrite a skill's references to tools it lacks
  (`Agent`, `Task`, `Skill`, `Workflow`, subagents) and does not translate
  Claude Code tool names to its own. The product documentation and this
  spec's Assumptions record the consequence: every skill expands on a sized
  route (REQ-586); prompt-template skills work; skills that invoke other
  skills or dispatch subagents (`/proceed`, `/sprint`, `/analyze`) degrade to
  what one model can do with `read/edit/glob/grep/shell` — and stall at a
  "invoke the skill" step — until the two Deferred follow-ups land.
- [ ] BR-14: **Discovery and expansion are pure functions.** `(files read) →
  SkillRegistry` and `(registry, typed line, dynamic outcomes) → expansion`
  have no terminal, no clock and no daemon in them — running the commands is
  I/O that produces the outcomes the expander consumes — so every rule above
  is unit-testable without a pty, and the TTY-gated pieces (the consent
  prompt, the echo line) are the thin bytes around them (informed by
  LESSON-481).

## Acceptance Criteria

- [ ] AC-1: With a fixture HOME holding `skills/alpha/SKILL.md` and
  `commands/beta.md`, and a session root holding `.claude/skills/gamma/SKILL.md`,
  `/help` lists `/alpha`, `/beta` and `/gamma` in the skills section with
  `(user)`/`(project)` sources, the argument hint where one is declared, and
  the diagnostic line `3 skills (user 2, project 1); 0 skipped`; the built-in
  section is byte-identical to the pre-REQ golden. (unit + `cli_e2e`; BR-1,
  BR-3)
- [ ] AC-2: Fixture skills named `cost`, `exit` (an alias), `provider` (a
  family word) and `teton` are each listed as shadowed and never dispatch:
  `/cost`, `/exit`, `/provider list` and a typed `teton provider list` behave
  byte-identically to today. (unit + `cli_e2e`; BR-2)
- [ ] AC-3: A user skill and a project skill both named `analyze`: `/help`
  shows the project one and marks the user one shadowed; `/analyze` expands
  the project file. A session whose root is `$HOME` registers each skill
  once, as `user`. (unit; BR-1, BR-2)
- [ ] AC-4: `/alpha teton  code "repo"` (two interior spaces, quotes)
  produces exactly one prompt turn whose text is the preamble line (with a
  home-relative path) followed by the body with `$ARGUMENTS` replaced by
  `teton  code "repo"` — interior whitespace and quotes preserved — and no
  model call precedes it; the echo line names the skill, source and size.
  (unit + `cli_e2e`; BR-4, BR-12)
- [ ] AC-5: A body with no placeholder and non-empty arguments gets a final
  `ARGUMENTS: <rest>` line; a body with `$ARGUMENTS` and no arguments gets the
  empty string; `$1`/`$2` receive whitespace-split tokens; a `$ARGUMENTS`
  inside a `!`…`` is substituted before the command runs and the consent
  text shows the substituted command. (unit; BR-4, BR-6)
- [ ] AC-6: Fixtures with malformed frontmatter, an unreadable file
  (`EPERM`), a name `Bad Name!`, a 65 KiB body, and a root entry that is a
  symlink each appear in the diagnostics with their reason (the symlinked
  entry as "not followed"); a directory with no `SKILL.md` produces no
  diagnostic; the remaining fixtures register and dispatch normally; nothing
  panics. (unit; BR-1, BR-8)
- [ ] AC-7: Discovery against a fixture HOME whose `.claude/skills` is
  itself a symlink to a sibling directory (the dogfood shape), containing a
  deep tree under `skills/alpha/nested/…`, a directory symlink
  `skills/link → /` and ten thousand files under `~/.claude/other`, opens only
  the four globbed locations one level deep — asserted through a
  filesystem-listing seam the registry builder takes (new, in the style of
  REQ-583's walker seams) that records every path opened — and completes in
  bounded time. (unit; BR-1)
- [ ] AC-8: At `guarded`, a skill with three `!`…`` commands produces one
  consent prompt listing all three verbatim under the skill's key (not
  `shell`); declining leaves three `[dynamic context not run: … — declined]`
  placeholders; accepting "for this session" answers the next invocation of
  the *same* skill without asking, leaves a *different* skill asking, and
  leaves a model-issued `shell` call asking; a prior allow-always on `shell`
  does not un-ask skill context. Accepting inlines each command's stdout
  inside the tool-result envelope, and a planted `<|im_start|>` / `User:` /
  `<tool-result>` in one command's output reaches the frame neutralized.
  (daemon unit for the gate and frame; pty for the prompt bytes; BR-5, BR-6)
- [ ] AC-9: At `plan` the commands are not run and the placeholders name
  the level; at `full` they run with no prompt; on piped stdin at `guarded`
  the client refuses without reading stdin — a `y` fed as the next stdin line
  is delivered as the next prompt line, not consumed as an answer — and the
  placeholders say no human could be asked. (`cli_e2e`; BR-6, BR-11)
- [ ] AC-10: A dynamic command that sleeps past the `shell` tool's timeout
  yields a timed-out placeholder and the invocation still produces its turn;
  a command that exits non-zero yields a failed placeholder. (daemon unit;
  BR-6)
- [ ] AC-11: Egress-capture: with a remote provider bound to the tier the
  turn routes to, (a) a skill file under a `local-only` boundary pins the
  turn local and nothing leaves the machine — exactly as a `read` of that
  file would pin it; (b) with a boundary configured anywhere and a skill that
  ran any dynamic command, the turn is pinned local because that output's
  provenance is `Unknown`, exactly as a `shell` result's is; (c) with no
  boundary configured, a skill that ran dynamic commands reaches the remote
  provider and the payload is the expansion. (egress-capture; BR-7)
- [ ] AC-12: A skill body that plants `User:`, `Assistant:` and `<|im_start|>`
  reaches the frame neutralized by the guards a typed prompt gets; a
  `<tool-result>` planted in a dynamic command's *output* reaches the frame
  neutralized by the envelope; a test removing any one guard fails. (daemon
  unit; BR-5)
- [ ] AC-13: A skill whose frontmatter says `allowed-tools: Bash(*)`,
  `model: opus`, `effort: max` registers; the session's permission level,
  effort and the turn's route are exactly what they would be for typed text;
  `/verbose` lists the three keys as ignored. (unit; BR-5)
- [ ] AC-14: `/cd` to a root with different `.claude/skills` re-derives the
  project skills and leaves the user skills as they were; `/help` reflects
  it without a restart. (`cli_e2e`; BR-1)
- [ ] AC-15: The guide's capability sentence is amended per BR-9 and
  BUG-181's pinning test (`the_system_prompt_states_what_the_session_can_run_and_from_where`)
  is updated, not deleted: still one `/help` line, both paths, "only the user
  runs"; the "loads nothing from" assertion re-worded with the sentence; the
  `asking`-line count still 1; no `teton …` form; the two prompt-margin tests
  green without moving the ceiling again. The skill roster is **not** in the
  system prompt. (unit; BR-9)
- [ ] AC-16: A skill whose expansion plus system prompt exceeds the
  **route's** budget is refused before any model call with a message naming
  the skill, its size, the budget and REQ-586's bound; the refusal is a typed
  outcome, not a clamped turn; a typed oversized prompt still elides (loudly,
  REQ-586 BR-7) — pinned so the refusal is seen to apply to skill turns only;
  removing the check makes the test fail. Measured against the real corpus:
  on the local route `/status` expands and `/proceed` is refused with `bound:
  local_engine`; on a route with `max_context = 128000` `/proceed` expands; on
  a remote route with `max_context = 0` `/proceed` is refused with `bound:
  default_unknown` and the message names `capabilities.max_context`. (daemon
  unit; BR-8)
- [ ] AC-17: `/analyze` with an `analyze` entry that was skipped prints the
  skipped reason; with no entry at all prints the pre-REQ
  `unknown command: `/analyze`` bytes — pinned in `cli_e2e` beside the
  existing unknown-command test. (`cli_e2e`; BR-10)
- [ ] AC-18: The registry builder and the expander are exercised by unit
  tests with no pty and no daemon; the pty suite covers only the consent
  prompt bytes; `cli_e2e` pins the surface bytes (`/help`, echo line, hints).
  (BR-14)
- [ ] AC-19: The invocation's turn appears in `/cost` with the same
  attribution a typed prompt gets, and `/verbose` shows the invocation line
  with path and per-command outcomes. (`cli_e2e`; BR-12)
- [ ] AC-20: **Dogfood, by hand, recorded in `docs/manual-verification.md`:**
  in the teton-code repo with the ADLC toolkit installed (its
  `~/.claude/skills` a symlink) and the Kimi provider given `max_context =
  128000` (REQ-586 AC-14), (a) `/status` expands, the ethos include and its
  `ls`/`grep` commands run under one `guarded` consent, and the model
  produces a status report using `read`/`glob`/`shell`; (b) `/validate
  REQ-585` expands and the model validates this spec's own file; (c)
  `/analyze teton code repo` expands on the Kimi route and the model performs
  a read-based audit (one model, no subagents — the documented fidelity
  caveat); `/proceed REQ-585` expands and the point at which it stalls (the
  first "invoke the skill" step) is recorded as the Deferred follow-ups'
  evidence; (d) on the local tier `/analyze` is refused with the BR-8 message
  naming `bound: local_engine`; (e) unattended: `printf '/status\n' | teton
  --permissions full` runs the dynamic context without a prompt and produces
  the report, and the same at `guarded` produces placeholders and still
  completes; (f) if the machine has a `local-only` boundary configured, (a)
  and (b) run on the local tier and the runbook says why (BR-7). (manual;
  BR-8, BR-11, BR-13)

## External Dependencies

- None new. The frontmatter is a flat `key: value` block of three string
  keys; no YAML library is required or wanted for it (a full parser is an
  attack surface the feature does not need — see Assumptions).
- **REQ-586 (route-aware context budget) lands first** — BR-8 and the
  success bar stand on its derived budget, its `bound` fact and its
  `context_pressure` surface. Spec drafted 2026-08-19 alongside this
  revision.
- Sequencing: BUG-181 is merged (`main` at 7796dca, 2026-08-19) and its
  sentence is the one BR-9 amends. The ADLC toolkit on the dogfood machine,
  and its Kimi record carrying `max_context = 128000`, are AC-20's
  preconditions.

## Assumptions

- The Claude Code skill format Teton targets is the one observed in the
  user's ADLC toolkit on 2026-08-19: frontmatter `name` + `description` in
  17/17 files, `argument-hint` in 16/17 (`proceed` has none); `$ARGUMENTS` in
  16/17 bodies (`proceed` has none — BR-4's `ARGUMENTS:` fallback carries
  `/proceed REQ-xxx`); `!`command`` dynamic context in 17/17 (every one the
  ethos include; the rest `git branch --show-current`, `pwd`, `ls`, `cat`,
  `grep`, `test -f`, and — in `/canary` only — `gcloud … list` queries, which
  are read-only *network* calls, not disk reads); no `$1`…`$N` and no
  `${CLAUDE_*}` variables in any shipped skill; 5/17 descriptions exceed 200
  characters and are truncated, not skipped. Other Claude Code features
  (`context: fork`, `agent`, `hooks`, plugin skills, `${CLAUDE_SESSION_ID}`)
  are real and deliberately out of scope.
- A flat `key: value` frontmatter parser is sufficient: the three honored
  keys are single-line strings; a multi-line or nested value under one of
  them is treated as malformed and skipped with a reason, never half-parsed.
- The permission gate keys grants by tool name and gives an unknown key the
  level table's default posture (ask / ask / deny / allow across guarded /
  edits / plan / full); BR-6's per-skill key rides that and invents no new
  remembering granularity. Per-command-string remembering would be new and
  is not needed.
- Prompt text carries no file provenance today and shell output carries
  `Unknown` provenance; BR-7's skill-file provenance is new machinery, and
  the `Unknown`-pins-local consequence for dynamic context is accepted for v1
  and stated to the user in the runbook and `/verbose`.
- The harness context budget is 4,096 whitespace tokens / 32 KiB on every
  tier **today**, with the system prompt (~850 words) charged against it and
  the estimate counting whitespace-separated words (`estimated_tokens`).
  REQ-586 replaces that with a per-route budget. Measured against the
  corpus: on the local tier (budget unchanged by REQ-586) body + ethos (599
  words) + system prompt ≤ 4,096 admits `/optimize` (416 words), `/reflect`,
  `/validate`, `/status`, `/review`, `/canary`, `/adversary`, `/init`,
  `/architect`, `/bugfix` (2,427) — the last three only when their other
  dynamic commands stay small (`/architect`'s `cat .adlc/context/architecture.md`
  alone is ~6,400 words in this repo and will push it over) — and refuses
  `/spec` (2,717), `/manifest`, `/analyze`, `/template-drift`, `/wrapup`,
  `/sprint`, `/proceed` (7,222); on a remote route with a declared window of
  16k tokens or more, all seventeen expand (`/proceed` is 8,671 words with
  ethos and system prompt, ≈ 13k tokens at REQ-586's working ratio). On a
  machine with `[privacy] redact = true`, REQ-586 BR-4's scannable bound
  (≈ 89 KB with today's constants) still admits every skill — `/proceed`'s
  expansion is ≈ 61 KB with ethos and system prompt — so the success bar
  holds there too. The 64 KiB body cap admits every shipped skill; it exists
  to refuse a mistaken file, not to ration real skills.
- Fidelity: the agent-fleet and skill-invoking skills (`/proceed`,
  `/sprint`, `/analyze`) will degrade under a single model with five tools
  even once the budget admits them, and `/proceed`/`/sprint` will stall at
  their first "invoke the skill" step because the model has no way to invoke
  one (and cannot `read` a SKILL.md outside the jail). That is accepted,
  documented, and is the evidence for the two Deferred follow-ups; the REQ's
  success bar is "every skill expands on a sized route".
- Project-level skills are repository content and therefore may be authored
  by someone other than the user. In v1 the permission gate is the trust
  boundary: at the default `guarded` every dynamic command is shown and asked
  about on every invocation under the skill's own key, and the body itself is
  prompt text the model reads under the same permission level as a typed
  prompt (OQ-7 records the residual).
- On macOS, reading a root that resolves under `~/Documents`/`~/Desktop`
  etc. may raise a one-time consent dialog for the daemon or CLI process
  (the REQ-583 incident); discovery treats the refusal as a skip reason and
  does not retry in a loop.
- REQ id allocated with remote verification (`ADLC_ALLOC_DEGRADED=0`,
  2026-08-19).

## Open Questions

- [x] OQ-0: **Route-aware context budget first, or this REQ first?** —
  **Resolved 2026-08-19 by the product owner: "big skills are the point, we
  need them for automation."** REQ-586 (route-aware context budget) is
  drafted and lands first; this REQ's BR-8 and success bar stand on it (see
  Description). The two further automation gaps (model-invoked skills,
  subagent dispatch) are named in Deferred as the next specs.
- [ ] OQ-1: **Who owns discovery and expansion — the CLI or the daemon?**
  Client-owned keeps the protocol unchanged for *user* skills and keeps
  `/help` next to the table that generates it — but not for project skills:
  after launch the CLI knows the session root only as `SessionRoot.display`
  (home-relative, middle-elided, neutralized — REQ-583), and an attached
  client never had a path, so project discovery on the client would need a
  new RPC anyway. BR-6's gate and jail, BR-7's provenance and BR-8's refusal
  all live in the daemon, and REQ-573's lesson is that a catalog the CLI owns
  is one a phase-2 client re-implements. *Lean (strengthened):* the daemon
  owns the registry and the expansion (an additive `skills/list`-style query
  for `/help`, and the invocation carried as prompt content the daemon
  expands), the CLI owns classification over a snapshot and the rendering.
  `/architect` decides.
- [ ] OQ-2: **Should the model see the skill roster** (names + one-line
  descriptions) in the system prompt so it can say "type `/status`" when a
  user asks for status? Cost: roughly 20 tokens per skill per turn on every
  tier, and the resident prompt's headroom is measured in hundreds of bytes
  after BUG-181 (BR-9). *Lean:* no in v1 — `/help` is the roster and BR-9's
  sentence points at it; revisit with the REQ-582 hand-off-nudge pattern if
  dogfood shows users asking the model what it can run.
- [ ] OQ-3: **Is launch + `/cd` enough re-scanning for v1**, or does authoring
  a skill mid-session need a `/skills` row (list with sources and
  diagnostics; `/skills reload`)? *Lean:* ship without; a `/skills` row is
  cheap and likely wanted, so it is listed under Deferred rather than
  refused.
- [ ] OQ-4: **`.claude/commands/*.md` and subdirectory namespacing.** Flat
  `commands/<name>.md` is in (same body rules, file stem as name);
  subdirectories (`commands/frontend/component.md`) are not. Confirm that
  flat-only is acceptable.
- [ ] OQ-5: **`model:` / `effort:` frontmatter as routing hints?** A skill
  that says `model: opus` could map to the `think` tier. *Lean:* no — cost
  control is policy-owned and a file on disk must not be able to escalate
  spend; the user has `/effort` and `/policy`.
- [ ] OQ-6: **Per-skill key or one key for all dynamic context?** BR-6 says
  per skill ("may *this* skill's dynamic context run?" is the question a
  remembered grant answers — LESSON-495); one category-wide key would make
  "allow for this session" on `/status` free `/canary`'s `gcloud` calls.
  *Lean:* per skill, as written. Confirm.
- [ ] OQ-7: **Should project-level skills require a one-time trust
  acknowledgment** (Claude Code's workspace-trust shape) before their dynamic
  context can run, on top of the permission gate? *Lean:* not in v1 — at
  `guarded` every command is shown and asked every time; record the residual
  and revisit if a `full`-by-default user base appears.
- [ ] OQ-8: **Dynamic context on a boundary-configured machine** pins every
  ADLC skill local (BR-7). Should the consent offer "run without dynamic
  context" so the turn can still route remote? *Lean:* not in v1; the model
  can run the commands itself with `shell` (same pin) and the user can `/cd`
  — state it in the runbook and revisit with dogfood.

## Out of Scope

- Model-invoked skills (a `Skill` tool, Claude Code's
  `disable-model-invocation`/`user-invocable` semantics); the model cannot
  run a skill, only name it.
- Subagents, `Agent`/`Task`/`Workflow` tools, `context: fork`, `agent:`
  frontmatter, hooks, `allowed-tools` — no runtime for them and no
  translation of a body that references them (BR-13).
- The route-aware (per-provider) context budget itself and how a typed
  oversized prompt is handled — REQ-586 (this REQ depends on it).
- Plugin or marketplace skill directories; anything outside the four
  locations in BR-1; subdirectory namespacing under `commands/`.
- `${CLAUDE_*}` variable substitution (`CLAUDE_SESSION_ID`,
  `CLAUDE_SKILL_DIR`); `$ARGUMENTS[N]` indexing.
- Project instruction files (`CLAUDE.md`, `AGENTS.md`) — related, probably
  the next ask, and a different feature (system-prompt content with its own
  trust and size questions); separate REQ.
- Tab completion of skill names, file watching, hot reload (OQ-3).
- Dynamic-context execution on a surface other than the session (the shell
  `teton` subcommands do not expand skills).
- The VS Code extension (phase 2 client; it inherits whatever OQ-1 puts in
  the daemon).

## Deferred

- ~~Route-aware context budget~~ — promoted to **REQ-586**, which lands
  first (OQ-0 resolved).
- **Model-invoked skills** (recommended next spec): a `skill` tool that
  expands a registered, non-`disable-model-invocation` skill into the turn as
  a tool result, under the same registry, provenance and dynamic-context
  rules as this REQ — what `/proceed`'s "invoke the actual skill at each
  gate" needs. Its own REQ because a model-triggered inlining of a file is a
  different trust posture from a user-typed `/name`.
- **Subagent dispatch** (recommended after that): a bounded child turn-loop
  the model can hand a task to and get a result back from — what `/analyze`'s
  four auditors and `/sprint`'s parallel pipelines assume. Large; its own
  REQ.
- A `/skills` row listing sources and diagnostics and re-scanning on demand
  (OQ-3).
- The skill roster in the system prompt / a hand-off nudge that names a
  skill matching the user's ask (OQ-2).
- A trust acknowledgment for project-level skills (OQ-7); a "run without
  dynamic context" consent option (OQ-8).
- `docs/manual-verification.md` REQ-585 runbook — AC-20 needs a release and
  the ADLC toolkit on the user's machine.

## Validation

`/validate` ran 2026-08-19 on the first draft: 1 Blocker (the context
budget is one local-engine-sized budget on every tier and oversized prompts
are clamped in place — F-1), 10 Warnings, 5 Info. All sixteen findings are
applied in this revision: BR-8 and the success bar re-scoped with the
measured corpus (F-1, F-12); BR-6 moved to a per-skill permission key and
Assumption 3 / OQ-6 corrected (F-2); the pipe rule made an explicit
client-side refusal (F-3); BR-7 marked as new machinery with the
`Unknown`-pins-local consequence stated (F-4); BR-1 made symlink-root- and
TCC-aware and de-duplicated for `home` roots (F-5, F-6); the unreachable
shadow hint dropped (F-7); the reserved set widened to family words and
`teton` (F-8); BR-4 narrowed from "indistinguishable" to "same path", the
classifier's registry input and trimming stated (F-9); AC-12's body case
corrected (F-10); OQ-1 updated with the client's display-only root (F-11);
BR-14/AC-18 reworded (F-13); `$ARGUMENTS`-in-`!` defined (F-14); BR-9
inherits the guide's constraints and the preamble path is home-relative
(F-15); System Model shapes labelled illustrative (F-16). OQ-0 was then
decided by the product owner ("big skills are the point, we need them for
automation"): REQ-586 was drafted the same day, and this spec was re-scoped
onto it — Description, BR-8, BR-11 (unattended posture), BR-13, AC-16, AC-20,
External Dependencies, Assumptions, Deferred.

## Retrieved Context

- REQ-582 (spec, score 19): Every session-meaningful `teton` command runs from the session — no shell round-trip
- LESSON-537 (lesson, score 14): A second surface inherits every grammar and gate it touches — parse before you gate, confirm before you read a secret
- REQ-555 (spec, score 14): In-session slash commands for the teton interactive CLI
- REQ-581 (spec, score 12): A first-class provider connection test: `/provider test <id>` makes one consented call and reports a typed outcome
- REQ-579 (spec, score 12): Guided in-session provider setup: `/provider setup` collects, the daemon commits, the model hands off
- REQ-560 (spec, score 11): Named permission levels and the interactive session status line
- REQ-573 (spec, score 10): Daemon-owned web-setup suggestion catalog in web/setup_plan
- REQ-556 (spec, score 10): Live model-loading progress in the interactive session
- REQ-583 (spec, score 9): Session-root awareness and bounded discovery — the agent knows where it is, the user is told when it is nowhere, and a search cannot become a disk crawl
- LESSON-529 (lesson, score 9): A display helper is a second parser — render the host the request will reach
- LESSON-517 (lesson, score 9): A sanitizing seam owns the styling too — and the seam is the only ground truth for parity
- LESSON-481 (lesson, score 9): A gate that hides a feature from users also hides it from the test suite — split the logic out from under the gate
- LESSON-535 (lesson, score 8): A probe is a billed call and a preview is a surface — four verify-phase catches on REQ-581 and the audit prompts they leave behind
- REQ-563 (spec, score 8): Opt-in web lookup through the egress choke point
- REQ-570 (spec, score 8): Human-attested attach consent: a surface a headless process cannot satisfy, and a client that can answer

(Spec filter admitted `status: complete`, this repo's terminal status — the
skill's `approved|in-progress|deployed` filter matches zero specs here, as
REQ-579 noted. The delegate's body-read returned 10 of 15 blocks; LESSON-481,
LESSON-517, LESSON-535, REQ-563 and REQ-570 were read directly. The ADLC
skill inventory in Assumptions — frontmatter keys, `$ARGUMENTS`, `!`…``
commands, word and byte counts, subagent references — was taken from
`~/.claude/skills/*/SKILL.md` on 2026-08-19, not from retrieval; BUG-181 and
LESSON-543 post-date retrieval and were read directly. LESSON-495 was cited
by the validator and read for BR-6.)
