---
id: ASSUME-039
title: "REQ-615's write gate is a guard rail against accidental scaffolding, not a sandbox"
component: "tetond/harness"
domain: "security"
stack: ["rust"]
concerns: ["security", "developer-experience"]
tags: ["write-gate", "shell", "tokenizer", "indirection", "sandbox", "residual-risk"]
req: REQ-615
created: 2026-09-04
updated: 2026-09-04
---

## The Assumption

REQ-615 BR-4 refuses `edit`, and a `shell` command whose first command-position
verb is a write verb or which carries a top-level file redirection, when the
session root is `home` or `filesystem_root`.

It is assumed that this is **sufficient for the harm it addresses** — a model
that believed a `cd` had persisted and ran `mkdir -p .adlc/context` in `$HOME` —
and that it is **not** relied on as a containment boundary.

## Why We Believe It

The detector is `command_position_programs`, a whitespace tokenizer, not a shell
lexer. A write reached through indirection is not seen:

- `sh -c 'mkdir x'`, `bash -c …` — the verb is inside a quoted argument;
- `xargs mkdir`, `find . -exec touch {} \;` — the verb is an argument;
- a script, a Makefile target, a `cargo` build script.

Closing those means refusing every interpreter at a non-project root, which is
far wider than this rule and is REQ-614's opaque-verb territory. The gate is
sized to the observed failure — an agent doing the obvious thing in the wrong
directory — not to an adversary.

The module doc states this in the same words, because a documented guarantee
that is false is worse than a narrower one that is true (architecture.md,
REQ-596 BR-6).

## What Would Invalidate It

- A report of `~/.adlc`-style scaffolding created through an indirection the
  gate does not see. That is the assumption failing on its own terms.
- Any document, release note, or prompt sentence describing BR-4 as preventing
  writes outside a project *in general* rather than catching the common
  accidental spelling — the claim would then be false as stated.
- REQ-614 landing an opaque-verb set that this gate could read, which would make
  the interpreter case cheap to close and remove the reason for the narrowness.

## How We'd Find Out

The `write_refused_non_project` event names the tool and the root. A session that
scaffolds into `$HOME` *without* one of those events having fired is the
falsifying observation, and the transcript sink records both.
