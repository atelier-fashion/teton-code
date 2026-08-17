---
id: BUG-176
title: "The shipped guide told users to put a live API key on the command line"
status: resolved
severity: medium
created: 2026-08-16
updated: 2026-08-16
component: "daemon/harness"
domain: "harness"
stack: ["rust", "llm-providers"]
concerns: ["security", "developer-experience"]
tags: ["credential-hygiene", "self-config", "provider-add", "baseline-finding"]
---

## Description

Found as finding F-3 of REQ-579's live A/B baseline (verification.md §5): on
the shipped v0.1.17 build, asked "set up Kimi for deep reasoning", the local
model's reply included `teton provider add kimi …` followed by *"replace `kimi`
with the actual API key"* — instructing the user to put a live credential on
the command line, where it lands in shell history and `ps`. The guide's own
rule ("never ask the user to type an API key in chat") did not stop the model
composing a shell line that carried one.

## Reproduction Steps

1. v0.1.17, no remote providers configured, interactive session.
2. Prompt: `set up Kimi for deep reasoning`.
3. Read the reply's shell block (recorded verbatim in REQ-579 verification.md §5, rounds B1–B3, 3/3).

## Expected Behavior

The key is never named on a command line; `teton provider add` reads it
echo-off (or from `TETON_PROVIDER_KEY`), and an interactive session is handed
`/provider setup`, which reads it echo-off into the keychain.

## Actual Behavior

The reply told the user to substitute the key into the command.

## Environment

- Platform: macOS 15.6, Apple Silicon; local model qwen3-coder-30b-a3b
- Version: teton 0.1.17 (baseline for REQ-579's A/B)

## Root Cause

The resident guide named the shell command as *the* instruction and left the
model to explain the key step in its own words; a small model fills that gap
with the most familiar shape — a placeholder in the command. See LESSON-532.

## Resolution

Resolved by REQ-579 (PR #161, merged 653ccf5): the guide's step 1 now leads
with `/provider setup <vendor> [tier]` and marks the shell command as
"Shell only: … key via TETON_PROVIDER_KEY or a prompt"; the prohibition line
is shorter and pinned by whole-line equality; and the surface appends a
harness line naming `/provider setup` whenever a TTY reply recites the CLI
(ADR-9). Rounds A1–A3 of the same A/B show the candidate build no longer
produced the placeholder instruction.

## Files Changed

- `crates/tetond/src/harness/self_config.md` — step 1 and the prohibition line (REQ-579 TASK-156 + fix rounds)
- `crates/teton/src/session_ui.rs` — the ADR-9 hand-off line (REQ-579 TASK-159)
