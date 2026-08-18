---
id: ASSUME-013
title: "No mirrored subcommand takes a whitespace-bearing value, so session rows can split arguments on whitespace"
status: validated
req: REQ-582
created: 2026-08-18
resolved: 2026-08-18
---

## Assumption

Every argument a session row forwards to the CLI's own clap grammar —
provider ids, tiers, categories, model names, endpoint URLs, `--mode` enums,
path globs — can be tokenized by splitting on whitespace, with no quote or
backslash interpretation. So the session can rebuild a shell argv from a typed
line without a shell-words tokenizer, and a `/policy set-tier build kimi
--fallback local` is the same argv `teton policy set-tier build kimi
--fallback local` receives (REQ-582 ADR-2, OQ-5).

## Context

REQ-582 mirrors ten shell subcommands as session rows and recognizes typed
`teton …` lines; both paths rebuild argv and hand it to `Cli::try_parse_from`
(BR-3, one grammar). A quote-aware tokenizer would have been a second parser
of the line (LESSON-529) or a new direct dependency (`shell-words` is only in
`Cargo.lock` transitively, via the pty dev-dependency). The `/help` argument
footer states the limit; the CHANGELOG names it as one of two limits to know
before upgrading.

What depends on it: `slash::run_cli_line`, `cli_rows::run_mirrored`, and the
recognition classifier's `after_words`.

## Resolution

**Validated for the shipped surface (2026-08-18).** Every mirrored
subcommand's arguments were enumerated at architecture: ids, tiers,
categories, model names, URLs and enum flags never contain whitespace; the
one legal exception is a `boundary add` glob with a space in it, which stays
a shell command (documented in `/help` and the CHANGELOG). Revisit if such a
glob is ever asked for — the fix is a tokenizer at one seam
(`cli_rows::mirrored_argv`), not a second grammar.
