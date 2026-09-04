---
id: REQ-615
title: "Session-root honesty for the shell tool and skill preambles — `cd` never persists and the tool says so, a home-directory root refuses writes and project skills, and a typed `cd` is offered as `/cd`"
status: complete
deployable: true
created: 2026-09-04
updated: 2026-09-04
component: "daemon/session"
domain: "harness"
stack: ["rust", "daemon", "cli"]
concerns: ["developer-experience", "security", "reliability"]
tags: ["session-root", "cd", "shell", "skills", "project", "system-prompt", "cwd", "environment-block", "preamble", "adlc", "home-directory", "tool-jail", "mkdir"]
---

## Description

REQ-583 gave the session a root, told the model where it is, and gave the user
`/cd`. The 2026-09-04 dogfood session (`sess-23aczryx…`, v0.1.30) shows the
three doors that REQ-583 left open, and the cost of each.

The session was launched from `~`. The user typed **`cd /teton-code`** as a
prompt — not `/cd` — and the model, correctly, treated it as text. It ran
`shell: cd /teton-code && pwd` (no such directory), then `projects`, then
`shell: cd ~/GitHub/teton-code && pwd`, which printed the project path and
exited 0. From that point the model **believed it was in the project.** It was
not: every `shell` command runs as a fresh child with the session root as its
working directory, so `cd` is discarded at the end of the command that ran it.
The model ran `cd ~/GitHub/teton-code && pwd` five times across the session,
was told `/Users/brettluelling/GitHub/teton-code` five times, and every
`ls -la`, `find`, `git status` and `glob` in between ran in `~`. Nothing in the
`shell` tool's description says the cwd does not persist; the environment block
names the root once and is then contradicted by a tool result the model trusts
more.

The consequences compounded:

1. **`/analyze` could not find the ADLC folder.** The skill's `!cmd` preamble
   `cat .adlc/context/architecture.md` ran in `~`, returned the fallback
   *"No architecture context found"* (29 bytes), and the skill body then told
   the model to run `/init`. The model invoked `init` through the `skill` tool
   four times in the session (the fourth refused as repeated). The project's
   `.adlc/` — 59 specs, 162 lessons — was never opened.
2. **The model wrote into the home directory.** `shell: mkdir -p .adlc/context
   .adlc/specs …` ran in `~` and added `~/.adlc/context`. A `~/.adlc/`
   skeleton of six empty directories has existed since 2026-08-19; which
   agent made it is not recorded, but the shape is the `/init` skill's. The
   user's requirement, in effect: the agent must not scatter project
   scaffolding into `$HOME`.
3. **The walk was the whole disk.** `glob .adlc/context/architecture.md`
   stopped after 100,000 entries; `find ~ -name '*.toml'` surfaced gcloud
   credential paths under `~/.config`; a `read` of
   `actions-runner-2/_work/…/CLAUDE.md` pulled another repository's
   instructions into context as if they were this project's.

REQ-583 made the root a fact the prompt states. This REQ makes the fact
*hold* against the two things that override it in practice — a `shell` result
that says otherwise, and a skill body that assumes a project — and closes the
one client-side gap: a `cd` typed as a prompt is almost always a `/cd` (informed
by REQ-583, REQ-587, LESSON-532, LESSON-570).

## System Model

### Entities

| Entity | Field | Type | Constraints |
|--------|-------|------|-------------|
| ShellResult | cwd_note | string | a harness-authored line appended to every shell result whose command contains a `cd` token: names the root the command ran in and that the next command starts there again; outside the untrusted frame |
| RootKind (existing, REQ-583) | kind | `project` / `home` / `filesystem_root` / `plain` | unchanged; consumed by the two new gates below |
| WriteGate | verdict | `allowed` / `refused_non_project` | applied to `edit`, and to a `shell` command that satisfies **either** trigger, when `kind` is `home` or `filesystem_root`: (a) its first verb is in the write-verb set (`mkdir`, `touch`, `rm`, `mv`, `cp`, `tee`, `install`, `ln`, `git init`), or (b) the command carries a top-level output redirection (`>`, `>>`, `>|`) outside quotes. The two triggers are independent because a redirection is never a first verb — `echo hi > ~/x` has first verb `echo` — so a single first-verb rule cannot see it |
| SkillPreambleOutcome (existing, REQ-585) | fallback_fired | boolean | `true` when the primary of a `!cmd` exited non-zero. The daemon can tell only because it **splits the command on its top-level `\|\|` and runs the primary itself**, observing that exit code; handing the whole string to one shell yields exit 0 and stdout indistinguishable from a successful primary |
| SkillGate | verdict | `expanded` / `refused_needs_project` | a skill whose frontmatter or body declares a project requirement, invoked while `kind` is not `project` |
| PromptHint | kind | `cd_as_prompt` | the client's pre-send check, against BR-7's regex. The pattern has exactly one spelling and it is stated in BR-7; this row does not restate it |

### Events

| Event | Trigger | Payload |
|-------|---------|---------|
| `write_refused_non_project` (new) | WriteGate refuses | `tool`, `root_display`, `root_kind`, `remedy: "/cd <name>"` |
| `skill_refused_needs_project` (new) | SkillGate refuses | `skill`, `source`, `root_display`, `root_kind`, `known_projects` (bounded, REQ-583's ceiling) |
| `skill_preamble_fallback` (new) | a `!cmd` preamble's fallback branch fired | `skill`, `command_index`, `root_display` — never the output |

### Permissions

| Action | Roles Allowed |
|--------|---------------|
| move the session root | the user, via `/cd` or `--cwd` (unchanged); the model cannot, and the prompt says so |
| write under a non-project root | nobody through `edit` or a write-verb `shell`; the user may still type a `teton` shell command themselves |
| override the WriteGate | none in this REQ (see OQ-1) |

## Business Rules

- [ ] BR-1: **The `shell` tool's description states the cwd contract.** The description carries, verbatim and pinned by a prompt-margin test: *"Each command starts in the session root; `cd` inside a command does not carry to the next one. Only the user can move the root, with `/cd <path>` — say so instead of trying."* (informed by LESSON-570: a prompt sentence must be true after the REQ ships; LESSON-532: a small model transfers data, so the fact must be beside the tool, not only in the environment block).
- [ ] BR-2: **A shell result whose command contained `cd` carries a cwd note.** The note is harness-authored, outside the untrusted frame, and names the root: *"[ran in <root>; the next command starts there again]"*. A `cd` whose target is the session root itself carries no note.
- [ ] BR-3: **When the root is `home` or `filesystem_root`, the environment block dictates the ending.** In addition to REQ-583's fact line, the block says: *"This is not a project. Do not create files or directories here. If the task needs a project, stop and ask the user to run `/cd <name>`; you cannot move the root yourself."* The known-projects list stays within REQ-583's byte ceiling.
- [ ] BR-4: **Writes under a `home` or `filesystem_root` root are refused.** `edit` is refused before dispatch, and so is a `shell` command matching **either** WriteGate trigger — first verb in the write-verb set, **or** a top-level output redirection outside quotes. Both are refused with a typed result naming the root, the kind and the remedy; `write_refused_non_project` is emitted. A `plain` directory root (a non-project folder that is not home) is **not** gated — that is where a user scaffolds a new project, and REQ-613's `TETON.md` write must keep working there.
- [ ] BR-5: **A skill that needs a project is refused outside one, not expanded.** A skill declares the need either by frontmatter `requires: project` or, for the shipped ADLC skills that carry no such key, by a `!cmd` preamble that references `.adlc/` — detected **statically, by scanning the preamble's command text at expansion time, before any preamble command is executed**. Running the preamble to find out would already have run the skill's commands in `$HOME`, which is the harm this rule exists to prevent (Description, consequence 1). Invoked while `kind` is not `project`, the skill is refused with `skill_refused_needs_project`, the tool result (or the typed-command reply) names the root and lists known projects with `/cd <name>`, and **no model turn is spent** on the body (informed by REQ-587: an expansion is admitted whole or refused typed; REQ-589's remedy-table shape).
- [ ] BR-6: **A preamble fallback is reported, not silently folded.** The daemon splits a `!cmd` on its **top-level `||`** (outside quotes) and runs the primary itself; when the primary exits non-zero it runs the fallback, emits `skill_preamble_fallback`, and prefixes the preamble's output with a harness line: *"[preamble <n> fell back: `<primary verb>` failed in <root>]"*. The split is what makes the fact observable — handing `cat X || echo none` to one shell returns exit 0 and the fallback's stdout, which is byte-identical to a primary that succeeded. A command with no top-level `||` runs as today and can still report a fallback by exiting non-zero. The model reads the fallback as what it is rather than as the project's answer.
- [ ] BR-7: **A prompt that is a `cd` is offered as `/cd`.** Before sending, the client matches `^cd(\s+\S+)?\s*$` — the canonical spelling of this pattern, referenced by the PromptHint row and restated nowhere else. It does not send the prompt; it prints *"`cd` is a session command here: `/cd <path>` moves the root (`/cd` alone shows it). Send as a prompt anyway with `//cd …`."* Piped stdin is exempt (the line goes to the model unchanged, as REQ-584's typed-only rule already does for the writing commands).
- [ ] BR-8: **The `projects` tool's result names the mechanism.** Its listing already ends each row with `/cd <name>`; it gains one trailing line: *"Only the user can run `/cd`. Ask them."*
- [ ] BR-9: **None of this changes a project root.** With `kind = project`, no gate fires, no extra block text is emitted beyond BR-1's tool description and BR-2's note, and the shipped ADLC skills expand exactly as today (pinned by the existing skill-expansion tests).

## Acceptance Criteria

- [ ] AC-1: A session rooted at `~` given the prompt `list the files in the teton-code project` ends the turn with a reply that names `/cd teton-code` and runs no `find` or `glob` from `~` (live trial on the shipped local model, three of three, per REQ-572's trial standard).
- [ ] AC-2: `shell: cd ~/GitHub/teton-code && pwd` from a `~` root returns the path **and** the cwd note; the next `shell: pwd` returns `/Users/<user>`. A prompt-margin test pins the tool description's cwd sentence.
- [ ] AC-3: `shell: mkdir -p .adlc/context` from a `~` root is refused before any child is spawned; `~/.adlc` does not exist afterwards (LESSON-519: inspect the artifact, do not infer from the error). `edit` of `~/notes.md` is refused the same way. The same two commands from a `plain` temp-dir root succeed.
- [ ] AC-4: `/analyze` typed at a `~` root is refused with the known-projects list and `/cd`; `cost.db` records no model call for that turn. `/analyze` at the project root expands as before.
- [ ] AC-5: A skill whose preamble is `cat .adlc/context/architecture.md 2>/dev/null || echo "none"` run at a `plain` root (not gated by BR-5's `.adlc/` rule because the root kind is `plain`, see OQ-2) yields `skill_preamble_fallback` and the prefixed line; at the project root with the file present, no event and no prefix.
- [ ] AC-6: Typing `cd /teton-code` in an interactive session sends nothing and prints the BR-7 hint; `//cd /teton-code` sends `/cd /teton-code` as prompt text; `printf 'cd x\n' | teton` sends `cd x` to the model.
- [ ] AC-7: The `projects` tool's result ends with the BR-8 line; a mutation deleting it fails the test.
- [ ] AC-8: The 2026-09-04 transcript's tool sequence, replayed call-for-call against a stub model, is answered by the harness as follows: every `cd`-bearing `shell` result carries the BR-2 note, the `mkdir -p .adlc/context …` call is refused by BR-4 with `write_refused_non_project` and creates nothing, and the `/analyze` invocation is refused by BR-5 with `skill_refused_needs_project`. **The assertions are on the harness's outputs, never on how many calls the stub chose to make** — a blind script's call count is a property of the script, so asserting it would be vacuous (conventions.md: never let the expected value be computed by the subject).

## External Dependencies

- None.

## Assumptions

- The write-verb set is a pinned table like REQ-614's opaque-verb set; a command the tokenizer cannot parse is treated as a write when the root is non-project (fail closed on the gate, not open).
- The shipped ADLC skills (`~/.claude/skills/*`) are not edited by this REQ; the `.adlc/` path-token detection in BR-5 is the compatibility path until they gain `requires: project`.
- BR-1 (a new sentence in the `shell` tool description) and BR-3 (two new
  environment-block lines) both spend system-prompt bytes, and per
  architecture.md a tool description is a **production input** to
  `REDACT_BODY_OVERHEAD_BYTES` and therefore to every redact-scanning route's
  context budget. The composed prompt is therefore **measured** after both land,
  in the task that runs last, rather than derived by addition (REQ-612's rule,
  LESSON-541). REQ-617 moves the same margin concurrently, so whichever of the
  two merges second re-measures rather than trusting its own pre-rebase figure.
- BR-2's "a `cd` whose target is the session root itself carries no note"
  compares the *resolved* target against the root. A target that cannot be
  resolved statically (a variable, a subshell, a glob) **fails toward emitting
  the note** — the note is advisory text, so a spurious one costs a line while a
  missing one restores the defect this REQ exists to close.

## Open Questions

- [x] OQ-1: **Resolved — no.** `full` does not bypass the WriteGate at a home root. The gate is about location, not trust; `full` already skips prompts, and a user at `~` who wants a file written there can type the shell command. Resolved at validation because AC-3 asserts the refusal without qualifying it by level; leaving it open would make the gate's level table unspecified for the one level that could have contradicted the AC.
- [x] OQ-2: **Resolved — only `home` and `filesystem_root`.** BR-5 refuses exactly where BR-4 refuses; a plain folder may be a project-to-be, and `/init` must run there. Resolved at validation because AC-5 already presumes this answer.

## Out of Scope

- Making `shell` a persistent interactive shell whose cwd carries across calls — the jail is a security property (BUG-147, REQ-583).
- Auto-detecting the intended project from a typed `cd` and moving the root without the user (`/cd` stays the user's act).
- Cleaning up `~/.adlc` skeletons that earlier sessions created.

## Retrieved Context

- REQ-583 (spec, score 14): Session-root awareness and bounded discovery
- REQ-591 (spec, score 13): The project-skill trust gate and its unattended allowlist
- REQ-612 (spec, score 12): TETON.md — a per-repository context file the session reads at its root
- REQ-589 (spec, score 12): Offer to proceed when a skill expansion exceeds the route's context budget
- REQ-587 (spec, score 12): Model-invoked skills
- REQ-572 (spec, score 12): Capability-aware refusals and guided in-session enablement
- LESSON-518 (lesson, score 11): A blocking gate's reader-loop freedom is not inherited from the await-based reader-loop tests
- LESSON-519 (lesson, score 11): An 'assert by inspection, not from the error' AC needs the real artifact
- LESSON-520 (lesson, score 11): A gate that fires before deserialization makes an invalid-payload test vacuous
- REQ-613 (spec, score 10): Teton writes TETON.md when a project has none
- REQ-611 (spec, score 10): Daemon-side transcript logging
- LESSON-570 (lesson, score 10): A prompt sentence must be true after the REQ ships, not before it
- REQ-575 (spec, score 10): Presence attestation for the web setup commit
- REQ-576 (spec, score 10): Presence attestation for config/set
- REQ-570 (spec, score 10): Human-attested attach consent
