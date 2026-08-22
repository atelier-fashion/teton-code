---
id: BUG-181
title: "The model affirms capabilities Teton does not have: asked whether it can use the skills it just read on disk, it says yes"
status: resolved
severity: medium
created: 2026-08-19
updated: 2026-08-19
component: "daemon/harness"
domain: "harness"
stack: ["rust", "daemon", "cli"]
concerns: ["developer-experience", "correctness"]
tags: ["system-prompt", "self-config-guide", "capability-claim", "skills", "skill-md", "claude-code", "honesty", "hallucination", "slash-commands", "unknown-command", "dogfood", "adlc"]
---

## Description

Teton's system prompt tells the model what it *is* ("a coding agent that
reads, edits, and verifies files using tools"), what tools it has, and — in
the bundled self-configuration guide — how the user configures providers and
web lookup. It tells the model nothing about what the *session* can do: which
`/` commands exist, that they are fixed, and that the files other agents load
as capabilities (`~/.claude/skills/*/SKILL.md`, `.claude/commands/*.md`,
`CLAUDE.md`, agent and hook definitions) are **not** loaded by Teton. So when
a user asks a capability question — "are you able to leverage the skills and
framework available?" — the model has nothing to answer from except what it
has just read on disk, and it answers from that: it sees seventeen skills
and a `CLAUDE.md` describing them and says **yes**. The user then types the
skill's name as a command, the closed command table (REQ-555 BR-7) correctly
answers `unknown command`, and the product has contradicted itself inside
two lines.

This is the same failure class the self-configuration guide was created to
prevent (BUG-160: the model hunted the repository for Teton's own config and
invented answers). The guide fixed it for *setup* questions by giving the
model facts about Teton to answer from; *capability* questions have no such
facts, so the model confabulates in the direction the on-disk evidence
points. It is not a provider- or tier-specific defect: any model will answer
"can you use X?" with "yes" when the only thing in front of it is X.

The fix is not to make the model able to run skills — that is REQ-585, a
feature with its own spec — but to make it **honest** about the session it is
in, in the one place Teton already keeps facts about itself. REQ-585 BR-9
amends the sentence this bug adds so it stays true once skills *are* loaded;
the two are coupled on purpose and the bug lands first.

## Reproduction Steps

1. On a machine with the ADLC toolkit installed (`~/.claude/skills/<name>/SKILL.md`
   × 17, `~/.claude/CLAUDE.md` describing `adlc-read`/`adlc-write`), launch
   `teton` in any repository (the teton-code repo reproduces; the 2026-08-19
   session was launched beside the toolkit and the model read
   `~/.claude/CLAUDE.md` and the `.adlc/` tree with `read`/`shell`).
2. Ask: *"It's agentic development lifecycle. Are you able to leverage the
   skills and framework available?"*
3. Read the answer.
4. Type `/analyze teton code repo`.

A second shape, one day earlier (BUG-180's trigger): ask *"show me the
skills"*; the model lists `~/.claude/skills` as though they were its own.

## Expected Behavior

The model answers a capability question about Teton from facts about Teton:
that the session's commands are the ones `/help` lists, that it cannot
invoke them itself, and that the skill/command/`CLAUDE.md` files it found
belong to another tool and are not loaded here — so the user is told *before*
typing `/analyze` that it will not be a command, and is pointed at `/help`.
Something like: *"Those are Claude Code skills — prompt templates that tool
loads as `/` commands. Teton doesn't load them; the commands this session
knows are listed by `/help`. I can read a SKILL.md and follow its
instructions as a prompt if you paste or point me at one."*

## Actual Behavior

> Yes, I can leverage the skills and framework available in the ADLC
> toolkit. The repository contains an `.adlc` directory with subdirectories
> for bugs, knowledge, partials, specs, templates, and workflows … I can use
> these tools to analyze, generate, and manage code and documentation within
> this structured environment.

followed, on the next line the user types, by

```
error: unknown command: `/analyze` — type /help for the commands this session knows.
```

The `unknown command` line is correct (the table is closed by design); the
defect is the sentence above it.

## Environment

- Platform: macOS (Apple Silicon), teton 0.1.23 (shipped build; also
  reproduces on `main` at 7e6e09b — the system prompt has no such clause at
  any version)
- Session: `permissions: guarded · effort: high`; the answering tier is not
  recorded in the screenshot (the 2026-08-19 session had Kimi `kimi-k3` bound
  to `build` and the local `qwen3-coder-30b-a3b` below it; BUG-180's twin of
  this ask ran on the local tier) — the defect is tier-independent
- Files present on disk: `~/.claude/skills/*/SKILL.md` (17), `~/.claude/CLAUDE.md`,
  `~/.claude/adlc/config.yml`; the launch directory had an `.adlc/` tree

## Root Cause

`build_system_prompt` (`crates/tetond/src/harness/turn_loop.rs:1396`)
composes: the opener ("You are Teton Code, a coding agent that reads, edits,
and verifies files using tools…"), the REQ-583 environment block (where the
session is), the optional verification and web clauses, the bundled
`SELF_CONFIG_GUIDE` (`crates/tetond/src/harness/self_config.md`, eight
lines), and the tool docs. Nothing in that composition names the session's
`/` commands or states that Teton loads no skills, plugins, commands,
`CLAUDE.md`, agents or hooks. The guide's first line already establishes the
pattern the fix needs — *"Teton's own configuration is never inside the
repository you work in, so do not search the project files for it; answer
setup questions from here"* — and its third line already says *"You cannot
run these commands yourself; hand them to the user"*, but both are scoped to
provider/web setup. There is no equivalent fact for capabilities, so a
capability question is answered from repo contents. The slash-command table
lives in the `teton` client (`crates/teton/src/slash.rs`), the daemon builds
the prompt, and no line of the prompt bridges the two.

Contributing, not causal: the only places in the tree that mention
`~/.claude/skills` at all are test fixture strings
(`crates/tetond/src/harness/completion.rs:1099`), and REQ-555 explicitly
deferred skill-style commands — so the gap is known on the roadmap side and
invisible on the prompt side.

## Resolution

One sentence, one raised ceiling, one pinning test.

1. `crates/tetond/src/harness/self_config.md` gains a fourth line, directly
   after "You cannot run these commands yourself; hand them to the user.":

   > Teton loads nothing from `.claude/` or `~/.claude` (no skills, commands,
   > CLAUDE.md, agents or hooks); the session's commands are exactly those
   > `/help` lists, and only the user runs them.

   186 bytes, resident in every turn of every tier; no "ask" (the guide may
   have exactly one line that mentions asking — the credential prohibition —
   and `the_system_prompt_forbids_asking_for_a_credential_in_the_conversation`
   pins that); names no `teton …` shell form (the `cli_rows.rs` cross-check);
   sits before step 1 so a top-down reader has it before the first recipe.
2. The resident prompt had **1 byte** of headroom above
   `MIN_PROMPT_HEADROOM_BYTES` under the 9 KiB `REDACT_BODY_OVERHEAD_BYTES`
   assumption (measured: worst prompt 5,891 B + 3,276 B escaping = 9,167 of
   9,216). Every other line of the guide is pinned by whole-line or
   per-segment assertions tuned by live A/B (REQ-579), so paying for the
   sentence by trimming would have meant re-tuning wording that works. The
   documented alternative was taken — the one REQ-577 took for `teton_docs`:
   the test-only overhead assumption moves 9→10 KiB and the arithmetic that
   derives `REDACT_TOTAL_CAP_CHUNKS` is re-stated with the new term
   (2×(32768+10240)=86016 ÷ 27070 = 3.18 → still 4 chunks;
   `REDACT_INPUT_MAX_BYTES` unchanged at 108,280; the 48-byte floor untouched).
   `docs.rs`'s "tens of bytes of headroom" sentence is updated to say so.
3. `the_system_prompt_states_what_the_session_can_run_and_from_where`
   (`turn_loop.rs`, beside the other guide tests) pins: exactly one guide line
   names `/help`; that line says `.claude/`, `~/.claude` and "only the user
   runs"; it says "loads nothing from" (asserted separately, because REQ-585
   BR-9 re-words that phrase and must update this assertion rather than
   delete the test); it precedes step 1; and it is present in
   `build_system_prompt`'s output for both harness shapes. Mutation-checked:
   removing the line, dropping `/help`, and moving it below step 1 each fail
   the test with the message that names the cause.

Verification: `cargo fmt --all --check` clean; `cargo clippy --workspace
--all-targets -- -D warnings` clean; `cargo test --workspace --no-fail-fast`
3080 passed, 0 failed (2026-08-19); CI all green on the PR (both OS legs,
acceptance suite, catalog integrity, audit, release tooling, feature-gated
targets). No protocol change, no client change, no behaviour change for any
prompt that is not a capability question.

## Deployment

- Merged: PR #188 → `main` at `7796dca` (2026-08-19, squash).
- Release: not yet in a tagged release at merge time (latest tag v0.1.23); the
  sentence ships with the next `chore(release)`. This repo has no Cloud Run /
  staging pipeline — plain OSS flow, so "deployed" means "in a tagged release".
- Live A/B (by hand, after the user's `brew upgrade`): re-ask *"are you able
  to leverage the skills and framework available?"* beside the ADLC toolkit
  and confirm the answer names `/help` and says the skills are not loaded —
  OUTSTANDING, recorded in `docs/manual-verification.md` when run.
- Lesson: LESSON-543.

## Files Changed

- `crates/tetond/src/harness/self_config.md` — the capability sentence (line 4)
- `crates/tetond/src/harness/turn_loop.rs` — the pinning test
  `the_system_prompt_states_what_the_session_can_run_and_from_where`
- `crates/tetond/src/egress/redact.rs` — `REDACT_BODY_OVERHEAD_BYTES` 9→10 KiB
  and the three doc blocks that state its arithmetic
- `crates/tetond/src/harness/tools/docs.rs` — the headroom sentence in the
  module docs
