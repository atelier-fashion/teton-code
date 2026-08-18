---
id: LESSON-537
title: "A second surface inherits every grammar and gate it touches — parse before you gate, confirm before you read a secret, and validate with the one grammar first"
component: "cli"
domain: "clients"
stack: ["rust", "cli", "json-rpc", "clap"]
concerns: ["developer-experience", "security", "testing"]
tags: ["slash-commands", "cli-parity", "recognition", "write-gate", "help", "secret-entry", "stdin-paste", "one-grammar", "verify-phase", "mirrored-rows"]
req: REQ-582
created: 2026-08-18
updated: 2026-08-18
---

## What Happened

REQ-582 gave the interactive session ten rows that mirror shell subcommands
and taught it to recognize a typed `teton …` line. The implementation was
disciplined about the one thing the spec shouted — one grammar (`Cli::
try_parse_from` over a rebuilt argv), one renderer, one daemon method — and
the Phase 5 verify still found four Majors, all of the same shape: **a new
surface quietly inheriting a posture from the surface it mirrored.**

1. **The write gate ran before the parse.** `run_mirrored` gated
   typed-input-only rows first, then parsed. Correct for a write; wrong for
   `/policy set-tier --help` on a pipe, which was refused instead of shown —
   a shell prints help regardless of stdin. Help never writes; the parse is
   what tells you whether a line *is* a write.
2. **Recognition resolved to *every* table row.** `cli_line` mapped a typed
   path to any `COMMANDS` row, so `teton effort max` on a piped session
   reached `/effort`'s ungated set path (a persisted config write with no
   typed-input gate — REQ-559 chose pipe-friendliness for the slash spelling
   and recognition inherited it silently), and `teton model set qwen --yes`
   handed `qwen --yes` to `/model set`'s hand grammar as the catalog name. Two
   parsers of one string, one layer above the layer BR-3 had closed.
3. **`/provider add` had no confirm between the key read and the keychain
   store.** The entry loop's framed prompter and the dialogue prompter's
   `ask_secret` read the *same* `io::stdin()` buffer, so a multi-line paste
   whose first line was the exact recipe every doc prints would run the row
   and consume the *next pasted line as the API key* — echo-off, straight
   into the OS keychain, registered. The shell twin never had this problem
   because a shell does not have a next line waiting.
4. **The composed store path was untested.** `provider_add_on` built
   `keychain::default_keychain()` internally, so no test could drive
   read → store → `config/set` without a real keychain; the pty test
   stopped one step short and the AC read as covered.

Each fix was small once named: parse → render help → gate → run; validate a
recognized line for a non-mirrored row with the one grammar first and derive
the row's argument from the parsed `Command`; a default-no `[y/N]` before the
key is read (session-side only — the shell keeps its bytes); thread the
keychain as `&dyn Keychain` and prove the composed path against the mock. A
closing round then found the *test* half of (3): fixtures that scripted a
key-shaped answer on the dispatcher path that reaches the real keychain —
harmless as written, a real-key clobber under the very mutation they
guarded. Script an empty answer; never a key.

## Lesson

- **Parse before you gate.** A gate that fires before the grammar sees the
  line has to guess what the line is; `--help`/`--version` are the standing
  counter-examples. Order: parse → help/version render → gate → run.
- **Recognition is an allow-list, not a name match.** When a new entry point
  maps typed text onto an existing table, every row it can reach carries its
  own argument grammar and its own gate posture into the new context.
  Restrict to rows that opted in, or validate with the one grammar first and
  derive the row's argument from the parse; record any inherited exception
  (here: `effort` set stays pipe-friendly by REQ-559 BR-9) where the
  Permissions table lives.
- **A command that reads a secret confirms before it reads** — because the
  session's entry input and its dialogue prompts share one stdin, a paste is
  a queue of answers. Default no; anything but `y` declines; the session's
  own `--yes` may pre-answer, a flag typed on the line may not.
- **Thread the effectful dependency; never construct it inside the body.**
  A body that builds its own keychain/transport is a body no test can drive
  end to end without the real thing — and the AC that "covers" it is
  covering a re-enactment (LESSON-529's corollary).
- **A test on a real-effect path scripts the refusing answer.** If a fixture
  can reach a real keychain (or socket, or filesystem) under a mutation, the
  scripted input must be the one that refuses before the effect — an empty
  key, an "n" — not a plausible secret.

## Why It Matters

Every one of these was invisible to a single-surface test suite: the shell
twin behaved, the slash row behaved, and only the *composition* — recognition
into a row, a paste into a confirm-less secret prompt, help behind a gate —
misbehaved. The costs ranged from a wrong catalog name to a credential stored
from a paste. Mirroring a surface is cheap; the inherited postures are where
the next REQ's Critical hides.

## Applies When

- Adding a second surface (session row, TUI action, editor command, MCP tool)
  for a command that already exists elsewhere.
- Adding any classifier that maps free text onto an existing dispatch table.
- Any interactive command that reads a credential or other secret after other
  input on the same stdin.
- Any body that constructs its own keychain, transport, or filesystem handle
  — thread it, and mock it in the composed test.
- Writing fixtures for tests whose code path can reach a real side effect.
