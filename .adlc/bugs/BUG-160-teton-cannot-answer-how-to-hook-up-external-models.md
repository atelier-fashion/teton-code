---
id: BUG-160
title: "Asked how to hook up external models, the agent searches the user's repo — Teton's own setup instructions are not bundled"
status: open
severity: medium
created: 2026-08-08
updated: 2026-08-08
component: "tetond/harness"
domain: "harness"
stack: ["rust", "prompt"]
concerns: ["developer-experience", "cost"]
tags: ["system-prompt", "providers", "self-configuration", "onboarding", "local-tier"]
---

## Description

Asked how to hook up external models, the agent immediately starts hunting
through the user's local files for instructions instead of answering. Teton's
own configuration surface (`teton provider add`, `teton policy set-tier`, the
keychain rule, where config.toml lives) is not bundled anywhere the model can
see it, so the question is unanswerable from knowledge — and the prompt's own
instruction, *"Use tools to find out what only the files can tell you"*, then
actively routes it into a file hunt that cannot succeed: Teton's configuration
is never in the repository being worked on.

This is the gap the BUG-154 fix does not close. That fix added the no-tool
ending for "a question answerable from what you already know" — but a question
about Teton itself is answerable from neither the model's weights nor the
user's files. The only place the answer can come from is text bundled with
Teton, and no such text exists: `build_system_prompt` is the entirety of what
the model is told (frame + tool docs), and greps for provider/config
self-documentation in the prompt come up empty.

## Reproduction Steps

1. Run `teton` with the local tier ready.
2. Type `How do I hook up external models?` (or "how do I connect Claude /
   an OpenAI-compatible endpoint?").

## Expected Behavior

A direct prose answer from bundled instructions: register the provider with
`teton provider add <id> --kind anthropic|openai-compatible --model <model>`
(key via `TETON_PROVIDER_KEY` or prompt, stored in the OS keychain), then route
work to it with `teton policy set-tier <tier> <provider>`; inspect with
`teton policy show` / `teton provider list`. No tool calls.

## Actual Behavior

The agent opens with file-search tool calls (grep/glob/read) over the user's
repository looking for configuration documentation that is not there, spending
turns and context budget before producing — at best — a guessed answer.

## Environment

- Platform: macOS 26 / Apple Silicon
- Version: teton 0.1.11, local tier

## Root Cause

`build_system_prompt` (`crates/tetond/src/harness/turn_loop.rs`) is the sum
total of what the model knows about Teton, and it contains nothing about
Teton's own configuration surface. The binary bundles no self-documentation at
all (the only `include_str!` assets are prices, model catalog, and ADLC
templates), user-facing docs never state the provider commands, and there is no
docs tool or meta-question route. So "how do I hook up external models?" falls
into a hole: not in the weights, not in the files — and the prompt clause
*"Use tools to find out what only the files can tell you"* makes searching the
repo the model's only legal move.

## Resolution

- A bundled self-configuration guide (`crates/tetond/src/harness/self_config.md`,
  compiled in with `include_str!` — the `structured/templates.rs` precedent:
  a fresh install needs nothing on disk) is appended to every system prompt by
  `build_system_prompt`. It tells the model that Teton's own configuration is
  never inside the repository being worked on — do not search the project
  files for it — and carries the accurate setup surface: `teton provider add`
  (remote kinds must declare `--model`; key via `TETON_PROVIDER_KEY` or
  prompt, stored in the OS keychain, never a file), `teton policy
  set-tier <reflex|scan|build|think>` / `set-category` / `show`,
  `teton provider list`, `teton doctor`, and the `config.toml` /
  `TETON_CONFIG` location.
- Sized at 1,012 bytes so the always-resident prompt (~3.5 KB total) clears
  the `REDACT_BODY_OVERHEAD_BYTES` ceiling that
  `the_total_cap_clears_the_harness_context_budget_with_margin` measures
  against the real prompt (limit ≈ 4.9 KB).
- A regression test mirrors BUG-154's: the guide's command surface and the
  "never inside the repository" clause are pinned on both the default and
  strong-model profiles, with a failure message telling a rewording maintainer
  to update rather than delete.
- The README's "bring your own models" promise now shows the actual commands
  (it previously never printed `provider add` at all).
- Verified live against an isolated daemon (release build with
  `tetond/llama`, `qwen3-coder-30b-a3b`): "How do I hook up external models?"
  answers directly with the correct commands and **zero** tool calls, while
  "What version is this crate? Check Cargo.toml." still calls `read` and
  answers correctly — the tool path is unchanged.

## Files Changed

- `crates/tetond/src/harness/self_config.md` — the bundled guide (new)
- `crates/tetond/src/harness/turn_loop.rs` — `SELF_CONFIG_GUIDE` const,
  injection in `build_system_prompt`, regression test
- `README.md` — "Hooking up an external model" section with real commands
- `.adlc/bugs/BUG-160-teton-cannot-answer-how-to-hook-up-external-models.md` —
  this file
