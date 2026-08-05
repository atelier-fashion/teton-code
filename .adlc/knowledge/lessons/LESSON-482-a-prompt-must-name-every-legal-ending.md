---
id: LESSON-482
title: "A prompt that enumerates a turn's legal endings must name every one — the model can only stop in a way it was told about"
component: "tetond/harness"
domain: "harness"
stack: ["rust", "prompt"]
concerns: ["developer-experience", "cost", "test-coverage"]
tags: ["system-prompt", "tool-use", "local-tier", "affordance", "prompt-regression", "bug-154"]
created: 2026-08-05
updated: 2026-08-05
---

## What Happened

Asked "what is the difference between a Mutex and an RwLock", the agent
announced it would examine their implementations in the repository and spent
`grep`, `grep`, `glob` without answering. The first instinct was to blame the
local model or look for a setting. It was neither: `build_system_prompt`
enumerated exactly two endings for a turn — a tool call now, or a plain-text
summary "when the task is complete" — and a question needing no files matched
neither. Turn one is never "complete", so the only shape on offer was a tool
call.

The clearest evidence was a question the model *did* answer: "what does HTTP
status code 429 mean" produced a complete, correct answer and then went looking
through the repo's Python files anyway. The model had the answer and could not
stop, because stopping there was not a shape the prompt described.

## Lesson

When a prompt enumerates how a turn may end, the enumeration is a closed set —
the model picks from what it was given, not from what would be sensible. A
missing ending is not a soft preference the model can infer past; it is a state
the model cannot reach. Adding one line naming the missing ending fixed all
three reproductions.

Two corollaries earned the hard way:

- **A new clause must not sit beside one that contradicts it.** `use exactly
  one tool per reply` had to become `when you do use tools, ...`. Left as it
  was, it declared every reply contains a tool while the new clause said some
  contain none, and at the local tier's temperature 0.2 the model resolves that
  contradiction by ignoring the softer instruction.
- **Prompt text needs a test like any other behavior.** Nothing in 913 tests
  asserted the prompt offered a no-tool ending, so its absence was invisible.
  The regression test now pins the clause on both harness profiles and fails
  with a message telling a maintainer who rewords it to update the test rather
  than delete the assertion.

## Why It Matters

The cost is not only the wasted latency. Every needless turn spends tool calls
and context budget — a real constraint against the default profile's 4,096-token
window — on a search that cannot inform the answer. And the failure is a quiet
one: the agent looks busy and productive while doing nothing useful, so it reads
as slowness rather than as a bug worth filing.

This is the same misattribution family as BUG-153, one layer in. There the user
addressed the harness and the model answered; here the user asked a question and
the harness's framing turned it into a file task. Both are the harness's shape
leaking into what the user meant.

## Applies When

- Editing `build_system_prompt` or any prompt that tells a model how a turn ends
  — check the enumeration is exhaustive before adding rules to it.
- Adding a constraint next to an existing one: read them as a pair and ask
  whether a literal-minded reader at low temperature can satisfy both.
- Reviewing a prompt change with no test. Assert the load-bearing clause exists;
  a passing suite otherwise proves only that nothing else broke.
- Behavior-testing a prompt change against the real local tier. Build with
  `--features tetond/llama` (a default build has no inference engine and every
  session silently goes remote-only), isolate with a **short** `XDG_RUNTIME_DIR`
  — the socket must fit `SUN_LEN`, ~104 bytes — and symlink the weights dir so
  the second daemon shares the mmap'd inode instead of re-downloading 17 GiB.
  A/B it: the baseline is what proves the prompt caused the change.
