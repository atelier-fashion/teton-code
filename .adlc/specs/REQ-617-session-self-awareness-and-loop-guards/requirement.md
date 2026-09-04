---
id: REQ-617
title: "The model knows the session's own commands and stops repeating itself — built-in command awareness in the prompt and docs, a repeated-identical-call refusal for every tool, and an honest shell-duty note"
status: complete
deployable: true
created: 2026-09-04
updated: 2026-09-04
component: "daemon/session"
domain: "harness"
stack: ["rust", "daemon", "cli"]
concerns: ["developer-experience", "reliability", "cost"]
tags: ["system-prompt", "teton_docs", "commands", "skills", "loop", "repeated", "shell-duty", "self-config", "transcript", "context", "help", "loop-guard", "idempotent-call"]
---

## Description

The user's report: *"Teton is unaware of its own skills."* The 2026-09-04
transcript (`sess-23aczryx…`, v0.1.30) narrows that to three concrete gaps,
none of which is the skill roster itself — the `skill` tool's description
carried all seventeen ADLC names within its 512-byte ceiling, and the model
called `skill` four times.

1. **The model does not know the session's built-in commands.** Asked *"is
   transcript on?"*, it had no way to answer: the self-config guide names
   `/provider`, `/policy`, `/web`, `/doctor` and `/help`, and `teton_docs`
   has seven topics (`providers, policy, context, web, skills, doctor, cost`),
   but neither names `/transcript`, `/cd`, `/clear`, `/effort`,
   `/permissions`, `/model` or `/boundary`, and no topic covers the
   transcript. It searched for a config file for seven tool calls, then read
   `.claude.json` — a Claude Code file — and reported that
   `tengu_auto_mode_config.jsonlTranscript = true` meant Teton's transcript was
   on. The correct reply was one sentence: *"Type `/transcript` — it prints
   the state and the file's path; I cannot run it."* (informed by REQ-572:
   a refusal names the capability and the path; LESSON-493: Teton's own
   configuration is never in the repository.)
2. **The model repeats a call that already answered.** In one turn it ran
   `ls -la` five times, `cd ~/GitHub/teton-code && pwd` four times, `pwd`
   three times and `projects` four times, each returning the same bytes. The
   `skill` tool already refuses a repeated identical invocation within a turn
   (REQ-587's `refused: repeated`, seen once in the transcript); no other tool
   does. The harness's one-tool-per-reply rule (BUG-147) makes each repeat a
   full model round-trip.
3. **The `shell` duty invents explanations.** A failed `cd /teton-code`
   (exit 1, `No such file or directory`) came back prefixed with
   *"[shell: The command failed because the directory /teton-code does not
   exist on the system. The agent needs to either create this directory
   first…]"*. The duty is meant to interpret output that is hard to read
   unaided (REQ-561); it was applied to a one-line stderr and the local model
   added an instruction the harness never authorized. The model then tried
   `shell: /init`.

Each gap has the same shape: a fact the daemon holds and the model does not,
or a rule the daemon enforces for one tool and not the rest. This REQ closes
all three in the daemon so a small local model gets the fact as data
(LESSON-532: presence in context buys retrieval) and the loop is broken by the
harness, not by hoping the model notices (informed by REQ-587, REQ-572,
LESSON-532, LESSON-570).

## System Model

### Entities

| Entity | Field | Type | Constraints |
|--------|-------|------|-------------|
| SessionCommandRoster | lines | list of `(name, one-line effect, user_only: bool)` | derived from the CLI's command table (`slash.rs` specs) at build time so the prompt and `/help` cannot disagree; rendered into the self-config guide within a pinned byte ceiling |
| DocsTopic (existing, `teton_docs`) | topic | string | gains `commands` and `transcript`; the existing `context` and `skills` topics gain the switch and command facts BR-2 names |
| CallFingerprint | tool, args_hash | (string, u64) | hash of the canonical JSON of the arguments; computed per tool call within one prompt turn |
| RepeatLedger | seen | map CallFingerprint → (count, first_result_len) | per turn; cleared at turn end; a `skill` entry reuses REQ-587's existing counter |
| RepeatVerdict | kind | `first` / `repeated_refused` | refused on the **second** identical call for read-only tools (`read`, `glob`, `grep`, `projects`, `teton_docs`, `shell` with a read-only verb); on the **third** for `shell` with any other verb and for `edit` (a retry after a real change is legitimate once) |
| ShellDutyGate | applies | boolean | the `shell` duty runs only when the result exceeds the existing size trigger **and** the command's exit was 0; a failed result is never interpreted |

### Events

| Event | Trigger | Payload |
|-------|---------|---------|
| `tool_call_repeated` (new) | RepeatVerdict `repeated_refused` | `tool`, `count`, `turn_id`; never the arguments |
| `shell_duty_skipped` (new) | the gate declines to interpret | `reason: failed_exit` / `under_size_trigger` |

### Permissions

| Action | Roles Allowed |
|--------|---------------|
| run a built-in session command | the user only; the model names it (unchanged, made explicit in the prompt) |
| bypass the repeat refusal | none; the refusal names what to do instead (change the arguments, or finish) |

## Business Rules

- [x] BR-1: **The self-config guide carries the session command roster.** One line per built-in command with its effect and the marker `(user runs this)`, generated from the same table the CLI registers commands from, within a pinned byte ceiling paid for by a reviewed prompt-margin move (REQ-612's rule; BUG-193: pin the margin with `assert_eq!`). The roster sentence ends: *"You cannot run any of these. Name the one the user should type, then stop."*
- [x] BR-2: **`teton_docs` gains the two missing topics and completes two existing ones.** New: `commands` (the roster with the shell twins) and `transcript` (the two switches and their lifetimes, the directory, that the model's tools cannot read it). Completed: `context` must name `/context on|off|init` and `[context] repo_file`; `skills` must name the four load globs and that `skill` is the only way the model runs one. The topic index in the tool description is updated in the same change (the existing test pins index and topics to one spelling). Each topic is bundled in the binary, never read from the repo (REQ-572 BR: enablement text ships bundled).
- [x] BR-3: **The dictated ending for a session-state question, with a deterministic backstop.** When the user asks whether a session switch is on (`transcript`, `context`, `verbose`, `effort`, `permissions`), the guide dictates the reply shape: name the command, say the model cannot run it, call no tool. **Corrected at validation (2026-09-04):** the guide's sentence is the *data* half and is the reliable half — LESSON-532 measured that facts placed in context cross perfectly. The *directive* half does not: three rounds of moving, dictating and isolating exactly such a sentence scored 0/3, and an AC resting on it alone would be an AC the project's own evidence predicts will fail. So the guarantee moves to where a test can pin it, reusing REQ-579 ADR-9's shipped shape (`session_ui.rs`'s hand-off nudge): when a reply to a session-state question **does not** name the command — or names it while also reciting a way to discover the state that the model cannot use (a config-file read, a repository search) — the harness appends one deterministic `>>` line naming the command and saying only the user can run it. The nudge is keyed on the model's own text, matched backtick-agnostically on both halves like its predecessor, and is what AC-1's CI-testable half asserts; the live trial corroborates it and does not carry it.
- [x] BR-4: **A repeated identical call is refused by the harness.** Per BR-table above: the second identical read-only call, or the third identical write-capable call, in one prompt turn is refused before dispatch with a typed tool result: *"repeated: this exact call already ran in this turn and returned <n> bytes; the result is above. Change the arguments or finish."* `tool_call_repeated` is emitted; the refusal costs no tool execution and no duty call.
- [x] BR-5: **A refused repeat is not a lost call.** The refusal text rides outside the untrusted frame, on the result the model receives, in the same slot BUG-147's dropped-calls notice uses, so the model can tell a refusal from a loss (REQ-587 AC on `refused: repeated` extends to every tool).
- [x] BR-6: **The repeat ledger is per turn and identical means identical.** The fingerprint is the canonical JSON of the arguments; `ls -la` and `ls -la .` are different calls. A new prompt turn starts an empty ledger. A `shell` whose output changed between calls is not exempt: the rule is about the call, and the model that wants fresh output can change the command (`ls -la; date`).
- [x] BR-7: **The `shell` duty never interprets a failed command.** A non-zero exit, a timeout, or a jail refusal returns the raw result with the harness's own `ERROR:` line and nothing else; `shell_duty_skipped` records why. The duty's prompt gains the sentence *"Describe what the output shows. Do not tell the agent what to do next."* and a test asserts the absence of the imperative forms (`should`, `needs to`, `must`) in the duty's output on the reference fixtures (LESSON-550: assert the absence).
- [x] BR-8: **The per-turn invocation cap for `skill` drops on the local tier.** `PER_TURN_INVOCATION_CAP` becomes a route property: 12 on a remote route, 3 on the local route, so a small model cannot spend a 21K-token window re-expanding the same 16 KB body (REQ-587's cap is a constant today; REQ-616 raises the local window to 262,144, and this rule still holds against it).

## Acceptance Criteria

- [x] AC-1 (a), **CI-testable, blocking**: against the scripted stand-in, a reply to *"is transcript on?"* that omits `/transcript`, and one that names it while also reciting a config-file read, both earn BR-3's deterministic `>>` line naming `/transcript` and saying only the user runs it; a reply that names `/transcript` and nothing else earns no line (the dormancy half). The same three cases for *"what skills do you have?"* against `/help`. Both halves read the same backtick-stripped text.
- [ ] AC-1 (b), **live trial, recorded not blocking**: prompt *"is transcript on?"* on the shipped local model, three of three trials reply naming `/transcript`, say the model cannot run it, and call no tool; the same for *"what skills do you have?"* naming `/help` or calling `skill` with no name — either is accepted; a repository search is a failure. **Corrected at validation (2026-09-04):** this needs the `llama` feature and several GB of downloaded weights, neither of which is present in an unattended pipeline, so it is recorded in the REQ's verification notes as deferred-to-a-machine-with-weights, exactly as REQ-616 AC-12 is. It corroborates AC-1 (a); it does not gate the merge, because a criterion nothing in CI can run gates nothing.
- [x] AC-2: `teton_docs commands` lists every built-in `/` command with its `teton` twin where one exists; a test enumerates the CLI's command table and asserts every registered name appears, so a new command cannot be added without its docs line.
- [x] AC-3: A prompt-margin test pins the self-config guide's byte length after the roster is added, with `assert_eq!` against a recorded constant (BUG-193).
- [x] AC-4: In one turn against a stub model that emits `shell: ls -la` five times, the harness dispatches once, refuses four times with `tool_call_repeated`, and the four refusals carry the BR-4 sentence; `cost.db` shows no duty call for the refusals.
- [x] AC-5: Two identical `edit` calls dispatch twice; the third is refused. Two identical `read` calls: the second is refused. `ls -la` then `ls -la .`: both dispatch.
- [x] AC-6: A new prompt turn after a refusal dispatches the same call again (ledger cleared).
- [x] AC-7: `shell: cd /nonexistent && pwd` returns a non-zero exit, the raw stderr, the `ERROR:` line, and no `[shell: …]` interpretation; `shell_duty_skipped { reason: failed_exit }` is emitted. A successful 40 KB `cargo test` output is still interpreted. **Corrected at verification (2026-09-04):** the original wording pinned *exit 1* and the string `No such file or directory`, which are readings of one `/bin/sh` — a failed `cd` exits 1 under bash (macOS's) and 2 under dash (Ubuntu's), whose diagnostic reads `can't cd to /nonexistent`. Written that way the test asserted which shell the runner ships, and it went red on the Linux leg while the macOS leg passed. BR-7's claim is that the command's own failure reaches the model unedited, so the test parses the `(exit N)` marker and asserts it non-zero, and matches the `[stderr]` line on the builtin and the path — the pair every `/bin/sh` puts in it.
- [x] AC-8 (a), **CI-testable, blocking**: the duty **prompt** built over each reference fixture carries *"Describe what the output shows. Do not tell the agent what to do next."* and does **not** carry the clause that authorized the invented instruction (*"what that means for what the agent should do next"*). Two mutations, both run red: reverting the sentence, and appending the old clause alongside the new one. **Corrected at validation (2026-09-04):** the original wording asserts on the duty's *output*, which is model text — a scripted engine's output is whatever the script says, so asserting the absence of imperatives in it would assert a property of the fixture, not of the product (LESSON-569). The prompt is the input the output comes from, is deterministic, and is where the defect actually was (LESSON-570: the harness authorized the imperative and was then surprised by it).
- [ ] AC-8 (b), **live check, recorded not blocking**: the duty's *answer* on the reference fixtures contains none of `should`, `needs to`, `must`, `the agent`. Deferred to a machine with the shipped local model, alongside AC-1 (b). The fixtures and the forbidden-form list are `pub(crate)` constants (`shell_duty::REFERENCE_FIXTURES`, `shell_duty::FORBIDDEN_IMPERATIVES`) precisely so this check has the same material to run against rather than a second copy of it.
- [x] AC-9: On the local route, a fourth `skill` invocation in one turn is refused with `cap: 3`; on a remote route the cap stays 12.
- [x] AC-10: The 2026-09-04 transcript's third prompt (`/analyze` at a home root) replayed against a stub model that re-emits its recorded calls completes in at most 9 dispatched tool calls instead of the recorded 26. **Corrected at validation (2026-09-04):** the transcript file itself is not in the repository and is not on the pipeline's machine (REQ-611 writes transcripts to the state directory, which is deliberately outside any tree a tool may read). The replay fixture is therefore **hand-authored in the test from the call multiset this REQ's own Description records** — `ls -la` ×5, `cd ~/GitHub/teton-code && pwd` ×4, `pwd` ×3, `projects` ×4, and ten further distinct calls totalling 26 — and the fixture's own total is asserted to be 26 so the baseline cannot drift from the number the AC names. Replaying a hand-authored multiset is weaker evidence than replaying the file, and the test's doc comment says so.

## External Dependencies

- None.

## Assumptions

- **Corrected at validation (2026-09-04).** The CLI's `COMMANDS` table cannot
  itself move to `teton-protocol`: each `CommandSpec` carries a
  `handler: fn(&mut Connection, &mut UiContext, &str) -> Result<CommandOutcome>`
  and a `Mirror` into `cli_rows`, both of which name CLI types the daemon has no
  dependency on (`teton` depends on `tetond`, not the reverse — a move would be a
  cycle). What moves is a **derived roster** — `(name, effect, user_only)` triples
  with no function pointers — living in `teton-protocol` and consumed by *both*
  the CLI's table and the daemon's guide/docs generator, so BR-1's roster still
  cannot drift from `/help`. The drift guard is a CLI-side test asserting the
  table's names and the roster's names are the same set (AC-2's enumeration),
  because only the CLI can see both.
- A read-only `shell` verb set (`ls`, `pwd`, `cat`, `head`, `tail`, `git status`, `git log`, `find`, `grep`, `wc`, `echo`) is a pinned table shared with REQ-615's write-verb set; unknown verbs count as write-capable (third call refused). REQ-615 runs concurrently; whichever REQ lands first owns the table and the other consumes it.
- **The resident prompt has 733 bytes of margin and 685 of usable room**
  (`RECORDED_PROMPT_MARGIN_BYTES` 733, `MIN_PROMPT_HEADROOM_BYTES` 48). A
  29-command roster carrying a full effect clause per line does not fit; BR-1's
  "pinned byte ceiling" is therefore a real design constraint the architecture
  resolves, not a formality. REQ-615 spends from the same margin concurrently.

## Open Questions

- [ ] OQ-1: Should the repeat refusal apply across the whole *session* for `projects` and `teton_docs`, whose results do not change within a session? Recommended: no; per turn is enough and keeps one rule.
- [ ] OQ-2: Should BR-8's local cap also apply to model-invoked `/init` specifically, which is the one skill that mutates the tree? Recommended: covered by REQ-615's project gate, no special case.

## Out of Scope

- Letting the model run built-in session commands (they stay the user's).
- A general "the model is stuck" detector beyond identical-call repetition.
- Changing the one-tool-per-reply harness rule (BUG-147).

## Deferred

Nothing was descoped. Two acceptance criteria shipped **unverified by design**,
both marked so at validation and both still unticked above:

- **AC-1 (b)** — the live trial of the session-state reply shape on the shipped
  local model.
- **AC-8 (b)** — the duty's *answer* on the reference fixtures carrying none of
  `should`, `needs to`, `must`, `the agent`.

Both need the `llama` feature and several GB of downloaded weights, which exist
neither in this pipeline nor in CI. Each corroborates a CI-testable half that
*is* verified and blocking — AC-1 (a) and AC-8 (a) — so the guarantee is
carried by a test either way; the live halves would raise confidence, not the
guarantee. The material they need is `pub(crate)` for exactly this reason
(`shell_duty::REFERENCE_FIXTURES`, `shell_duty::FORBIDDEN_IMPERATIVES`): run
them on a machine with weights and no new fixtures have to be written.

**AC-7's wording was corrected during verification**, not deferred — it pinned
`exit 1` and `No such file or directory`, which are properties of one `/bin/sh`
rather than of the product. See the criterion for the reasoning.

## Retrieved Context

- REQ-589 (spec, score 12): Offer to proceed when a skill expansion exceeds the route's context budget
- REQ-600 (spec, score 11): Decompose run_prompt_turn into a stage sequence
- REQ-598 (spec, score 11): TurnContext: dissolve the parameter clump
- REQ-599 (spec, score 11): Decompose the turn path and split runtime.rs
- LESSON-518 (lesson, score 11): A blocking gate's reader-loop freedom is not inherited from the await-based reader-loop tests
- LESSON-519 (lesson, score 11): An 'assert by inspection, not from the error' AC needs the real artifact
- LESSON-520 (lesson, score 11): A gate that fires before deserialization makes an invalid-payload test vacuous
- REQ-567 (spec, score 11): Cross-prompt conversation carry in interactive sessions
- LESSON-570 (lesson, score 10): A prompt sentence must be true after the REQ ships, not before it
- REQ-591 (spec, score 10): The project-skill trust gate and its unattended allowlist
- REQ-611 (spec, score 9): Daemon-side transcript logging
- BUG-193 (bug, score 9): The prompt-margin ledger drifts silently while its test stays green
- LESSON-551 (lesson, score 9): When a test disagrees with the product, suspect the instrument first
- BUG-184 (bug, score 9): Skill discovery runs on the connection's synchronous reader loop
- BUG-188 (bug, score 9): A model-invoked expansion caught at a mid-turn reroute ends the turn instead of relaying
