---
id: LESSON-548
title: "A refusal's remedy is a claim about the product's own surface — and the runbook is where you find out it is false"
component: "cli"
domain: "clients"
stack: ["rust", "cli"]
concerns: ["developer-experience", "reliability"]
tags: ["error-messages", "remedy", "runbook", "unattended", "flags", "req-585"]
req: REQ-585
created: 2026-08-20
updated: 2026-08-20
---

## What Happened

REQ-585's pipe rule refuses a skill's dynamic-context consent without reading
stdin, and the refusal names a remedy — a refusal without one is a dead end,
which is `cli_rows::typed_only_line`'s own rule. The line shipped through
implementation and six-reviewer verify reading:

> run the session at `--permissions full`

`teton` has no `--permissions` flag. Its globals are `--yes` and `--verbose`.
That line is the **only** thing an unattended runner sees when its dynamic
context is refused, so the remedy sent the one caller who cannot be asked
anything to a clap parse error.

No test caught it. Every test asserted the refusal's *behaviour* — nothing ran,
no stdin line was eaten — and none read the sentence as an instruction. It was
found in TASK-209, by writing AC-20's by-hand runbook and having to type the
command the message named.

The spec had the same error. AC-20(e) wrote `teton --permissions full`, so the
requirement and the code agreed with each other and both disagreed with the
binary — which is why review kept passing over it.

The corrected line names two things that exist (`/permissions full` piped ahead
of the invocation, and `[permissions] default_level`) and carries a negative
assertion that `--permissions` does not come back.

## Lesson

**Every remedy in an error message is an assertion about a surface, and it is
the one assertion the suite almost never makes.** A test that a refusal fires
proves the guard; it says nothing about whether the sentence sends the user
somewhere real. The two drift apart the moment a flag is renamed, a command
moves, or — as here — the sentence was never true.

Two habits.

**Assert the remedy, not just the refusal.** If the message names a flag, a
config key, or a command, pin it against the thing that defines it:
`Cli::command()` for a flag, the `COMMANDS` table for a row, the config type
for a key. Ten lines, and it turns the whole class red on rename.

**Write the runbook before you believe the message.** A runbook is the cheapest
adversarial reader a surface gets: it forces someone to *execute* the sentence
rather than read it. In REQ-585 that one pass caught this, corrected the AC
carrying the same error, and disproved a stale premise about a character
ceiling — the task brief said the docs-tool description sat at its 120-char
limit, and measuring found 94.

## Why It Matters

A wrong remedy is worse than no remedy: it converts a clean, actionable refusal
into a second failure at the moment the user has least room to investigate —
here, an unattended script where nobody can be asked anything by construction.

It also survives review unusually well. Reviewers read a remedy for tone and
plausibility, not for existence, and a well-formed sentence naming a
plausible-sounding flag reads as correct to everyone who is not currently
holding the CLI's argument parser.

## Applies When

Any error, refusal, or degraded-capability message that names a flag, a config
key, a command, or a file path. Especially on paths reached only by unattended
callers, where nobody will report the parse error back to you.
