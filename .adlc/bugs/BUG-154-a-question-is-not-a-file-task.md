---
id: BUG-154
title: "The system prompt describes no ending for a question that needs no files, so the model searches the repo instead of answering"
status: resolved
severity: medium
created: 2026-08-05
updated: 2026-08-05
component: "tetond/harness"
domain: "harness"
stack: ["rust", "prompt"]
concerns: ["developer-experience", "cost"]
tags: ["system-prompt", "tool-use", "local-tier", "misattribution"]
---

## Description

Asked a question answerable from knowledge — "what is the difference between a
Mutex and an RwLock" — the agent went searching the repository instead of
answering. This is not a model defect and not a setting: `build_system_prompt`
described exactly two endings for a turn, and neither one fit.

- *"reply with ONLY a JSON object"* — a tool call, available immediately.
- *"When the task is complete, reply with a short plain-text summary and NO
  JSON"* — a plain-text ending, but framed as the **terminal** state of work
  that has already happened.

A question needing no files matched neither. Turn one is never "complete", so the
only shape the prompt offered was a tool call, and the model took it. The
adjacent *"use exactly one tool per reply"* reinforced it by describing what a
reply **is**.

This is BUG-153's misattribution family one layer further in: there, the user
addressed the harness and the model answered. Here, the user asked a question and
the harness's framing turned it into a file task.

## Reproduction Steps

1. Run `teton` with the local tier ready (`qwen3-coder-30b-a3b`).
2. Type `What is the difference between a Mutex and an RwLock?`

## Expected Behavior

A direct prose answer, no tool calls.

## Actual Behavior

Observed on 0.1.9 against the real local tier:

- *Mutex vs RwLock* — opened with "I'll explain the difference between Mutex and
  RwLock by examining their implementations in the repository", then `grep
  Mutex|RwLock`, `grep sync`, `glob **/src/**/*.rs`, without answering.
- *"What does HTTP status code 429 mean?"* — answered correctly, **then**
  `glob **/*.py` and read `tools/refresh-catalog.py` before repeating the answer.
- *"In Rust, what does the ? operator do?"* — answered, then closed with "Let me
  search for examples of the ? operator in the repository."

The 429 run is the clearest evidence of the mechanism: the model had already
produced a complete answer and still could not stop, because stopping there was
not a shape the prompt described.

## Environment

- Platform: macOS 26 / Apple Silicon (48 GiB)
- Version: teton 0.1.9, local tier `qwen3-coder-30b-a3b`

## Root Cause

`build_system_prompt` enumerated the endings of a turn and omitted the one a
question needs. Nothing in the prompt said a reply may legally contain no tool
call before any work has been done.

The cost is not only latency: every such turn spends tool calls and context
budget — a real constraint on the default profile's 4,096-token window — on a
search that cannot inform the answer.

## Resolution

- `build_system_prompt` gains a third ending, placed before the tool-call
  format: a question answerable from knowledge or from the conversation is
  answered directly in plain text with no tool, and tools are for what only the
  files can tell you.
- `Work in short steps and use exactly one tool per reply` becomes `When you do
  use tools, work in short steps ...`. Left as it was, it sat directly beside
  the new clause and contradicted it — every reply has exactly one tool versus
  some replies have none — and at the local tier's temperature 0.2 the model
  resolves that contradiction by ignoring the softer instruction. The qualifier
  scopes the constraint without changing what it means when tools are in play.
- Verified by A/B against a live daemon on the same model, prompt as the only
  variable: all three questions above answer directly with zero tool calls,
  while "What version is this crate? Check Cargo.toml." still calls `read` and
  answers correctly — the tool path is unchanged.

## Files Changed

- `crates/tetond/src/harness/turn_loop.rs` — the third ending in
  `build_system_prompt`, the scoping qualifier, and a regression test asserting
  the clause survives on both the default and strong-model profiles (it fails
  with the pre-fix prompt, and its failure message tells a maintainer who
  rewords the clause to update the test rather than delete the assertion)
- `.adlc/bugs/BUG-154-a-question-is-not-a-file-task.md` — this file
