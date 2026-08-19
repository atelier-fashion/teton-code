---
id: LESSON-542
title: "A grammar you teach the model must be read on every path it can answer through — and output hidden 'because something else shows it' needs that something on every decision"
component: "daemon/harness"
domain: "providers"
stack: ["rust", "openai-compatible", "anthropic"]
concerns: ["correctness", "tool-calling", "developer-experience", "diagnosability"]
tags: ["text-form", "tool-call", "parse_reply", "stream-gate", "remote-source", "empty-turn", "silent-success", "one-grammar", "kimi", "routing", "cost-ledger"]
req: BUG-180
created: 2026-08-19
updated: 2026-08-19
---

## What Happened

The harness teaches every model one way to call a tool: the system prompt
says *"reply with ONLY a JSON object on its own: {"tool": …, "arguments":
…}"*, and since BUG-178 the carried history renders every prior call —
local or remote — in that same grammar. The loop has two sources of model
turns. The local one reads the grammar (`parse_reply`). The remote one read
only the provider's *native* call events and treated everything else as
prose. Nobody had decided the remote source should ignore the text grammar;
it was simply written when "a remote call is a structured event" was the
whole truth, and the prompt and history went on teaching the text grammar
to remote models anyway.

On 2026-08-19 Kimi K3 — `edit`-routed by the local classifier, the only
category bound off the local tier on this machine — obeyed the prompt and
wrote `{"tool": "shell", …}` as `content`. The remote source called that
the turn's answer. The display gate, which holds back tool-shaped JSON
*because the tool status line will present it*, had no status line coming
— the decision was `EndTurn` — and so it hid the answer. The turn
"succeeded": `StopReason::EndTurn`, no tool, no text, no error, a blank
line on the CLI, and the bare JSON committed as the assistant's block. The
user asked again; the follow-up classified off `edit`, ran locally, parsed
the same grammar, and worked — which is what made the first turn look like
a whim rather than a defect.

Nothing on the machine recorded the reason. There is no transcript on disk;
the daemon log had no line; `route_decided` is verbose-only. The cost
ledger was the only witness: one row on a different provider with a
different category and 161 output tokens, and a ~372-token gap between the
neighbouring prompts that added up to the hidden reply. The fix was four
lines at the remote source — when no native call arrived, read the text
with the same `parse_reply` the local tier uses — plus tests that go red
without them.

## Lesson

1. **Whatever the prompt teaches, every source must read.** If the system
   prompt or the rendered history shows the model a grammar, the model may
   use it on any turn, through any source. Audit is mechanical: list the
   readers of the grammar (`parse_reply`'s callers) and list the
   `CompletionSource`s; the sets must match. A source that reads only its
   own structured channel is a source that will silently mis-file the
   prompt's own instructions as prose. (BUG-178 made this worse without
   meaning to: rendering remote calls into the history in the text grammar
   taught remote models that grammar by example. A fix that adds a place
   the grammar appears should add a place it is read.)
2. **"Hidden because X presents it" is a pairing, and it must hold on every
   decision path, not just the one it was written for.** The gate hides a
   tool-shaped object on the premise that the tool status line shows the
   call. That premise is true for `ToolCall` and false for `EndTurn`. When
   you suppress output on the strength of another surface, enumerate the
   decisions the suppressed output can ride out on and check that the
   presenter exists on each — or make the suppression conditional on the
   decision rather than on the shape.
3. **A successful turn with nothing shown is a defect signal, and it needs a
   witness.** `EndTurn` + non-empty `final_text` + zero bytes displayed
   should not be able to happen without *something* saying so — a
   content-free stderr line, a reason on the turn result, anything the
   operator can find. Silent success is the most expensive failure shape:
   it looks like the model declining, it is reproducible only by the
   model's mood, and the user's natural response (ask again) erases the
   evidence by moving the conversation on.
4. **Keep a per-call ledger you can read back, and know how to read it.** The
   cost ledger's `category` and `provider_id` columns, and the token deltas
   between adjacent rows, reconstructed a turn that no log recorded. That is
   not a substitute for diagnosability (lesson 3) but it is the floor: when
   a turn's only trace is a usage row, make sure that row carries enough —
   provider, category, tokens, reasoning split — to say which path it took.
5. **Intermittent-by-routing looks like intermittent-by-model.** The defect
   fired only on `edit`-classified turns because `edit` was the only
   category leaving the local tier. When a symptom comes and goes across
   turns, check *where each turn went* before checking *what the model did*
   — `--verbose`'s `route_decided` or the ledger's `provider_id` answers it
   in one look.

## Why It Matters

The shape — one component teaches, a second component reads only its own
channel, a third hides the difference — is available whenever there are two
implementations of one seam. Here it cost the user a turn and a round trip
to a paid provider and left the conversation carrying a dangling call that
the next model had to reason around. With no local model (remote-only
machines), *every* text-form call would have ended this way, and the user's
"ask again" would have produced the same blank turn. The fix is small; the
search for it was not, because the failure left no trace by design of
three separately-correct pieces.

## Applies When

- Adding or changing a `CompletionSource`, a provider adapter, a prompt
  instruction about output format, or anything that renders the model's
  own past actions back to it — ask which readers must now understand the
  new shape, and add the reader with the writer.
- Suppressing, holding, or cutting model output for display or context
  (`StreamGate`, `ReplyScanner`, redaction, frame containment): name the
  surface that stands in for the hidden bytes, and check it exists on
  every `TurnDecision` the bytes can end the turn under.
- Investigating a turn that "did nothing": read the cost ledger first —
  provider, category, output tokens, and the prompt-size delta to the next
  row — before reasoning about the model; then check routing before
  checking behaviour.
- Reviewing a fix in the BUG-147/148/178 family (what the model may emit,
  what reaches the user, what reaches context): the three must be decided
  by one grammar; a heuristic on one side and a parser on the other is the
  gap the next report will come from (BUG-180's noted follow-up).
