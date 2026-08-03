---
id: LESSON-470
title: "Prompt defaults mirror reversibility — and interactive offers must be TTY-gated"
component: "cli/prompts"
domain: "developer-experience"
stack: ["rust"]
concerns: ["consent", "destructive-operations", "defaults", "scriptability"]
tags: ["default-yes", "default-no", "is-terminal", "piped-stdin", "decline-marker", "consent-matrix"]
req: none
created: 2026-08-03
updated: 2026-08-03
---

## What Happened

Two consent prompts shipped in one arc with opposite defaults, deliberately:
`teton uninstall` (irreversible 17 GiB deletion) defaults **no** — empty
answer and EOF both cancel, matching the existing over-RAM model confirmation;
the first-run launchd service offer (benign, reversible) defaults **yes** —
return accepts, matching the first-run model proposal. The service offer is
additionally gated on `std::io::IsTerminal` for stdin, because under piped
input (e2e suites, `echo ... | teton`, `curl | sh`-style flows) a prompt would
consume a line of stdin meant for the session. An explicit decline is
persisted as a marker file in the daemon state dir so the offer never nags —
and the marker lives where `teton uninstall` already sweeps.

## Lesson

Calibrate the default to the cost of a wrong answer: irreversible actions
require an explicit "y" (default no), reversible conveniences accept on return
(default yes) — and keep each new prompt consistent with the codebase's
existing prompts of the same class. Any interactive offer must check stdin is
a terminal before asking, and a permanent "don't ask again" belongs in state
that uninstall already cleans.

## Why It Matters

A default-yes on deletion loses data by accident; a default-no on setup adds
friction to every install. An un-gated prompt silently eats piped stdin and
breaks scripts and test suites in ways that look unrelated to the prompt.
Garbage answers deserve neither action nor permanent recording.

## Applies When

Adding any interactive confirmation to a CLI; deciding where to persist a
user's "no"; anything that might run under pipes, CI, or `--yes` automation.
