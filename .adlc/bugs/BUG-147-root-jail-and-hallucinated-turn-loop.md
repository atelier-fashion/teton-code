---
id: BUG-147
title: "Agent session unusable: tools jailed to /, hallucinated tool results streamed raw, dropped calls loop"
status: resolved
severity: critical
created: 2026-08-03
updated: 2026-08-03
component: "tetond/harness"
domain: "agent-loop"
stack: ["rust", "tetond", "teton-inference", "teton-protocol", "teton"]
concerns: ["ux", "correctness", "performance"]
tags: ["turn-loop", "cwd", "repo-root", "stop-sequences", "hallucination", "streaming"]
---

## Description

A first-run `teton` session against the local tier is effectively unusable. One
user transcript exhibits four compounding defects (one symptom cluster, four
root causes — fixed together because the fixes interlock in the same turn-loop
code):

1. **Tool jail is `/`, not the user's repo.** Every tool call runs against the
   filesystem root: `read README.md` fails, `find . -name "*.rs"` crawls the
   whole disk until the shell timeout kills it, `pwd` prints `/`.
2. **No generation stop → the model hallucinates the rest of the session.**
   The local engine continues past its own turn, fabricating `Tool (read):`
   results (plausible-looking README/main.rs content for files that do not
   exist), echoing the untrusted-content framing, and queueing more tool calls
   until the 256-token cap cuts it mid-JSON.
3. **The polluted reply is streamed raw to the UI and pushed back into
   context.** The user sees raw tool-call JSON and fake results interleaved
   with status lines; the full reply (fake results included) is folded into
   context, teaching the model to fabricate more each turn.
4. **Extra tool calls are silently dropped, causing a retry loop.** Only the
   first JSON object per reply is dispatched; the model is never told the rest
   were ignored, so it re-emits them turn after turn.

## Reproduction Steps

1. Install teton via Homebrew; `brew services start` runs `tetond` under
   launchd (cwd `/`, no `TETON_REPO_ROOT`).
2. `cd` into any repo, run `teton`, ask anything that triggers a tool call
   (e.g. "are you up and running?").
3. Observe: shell/read tools fail or hang against `/`; raw JSON and fabricated
   tool results stream into the transcript; identical reads repeat.

## Expected Behavior

Tools run in the repo the CLI was launched from; the model's turn ends at its
first tool call; the user sees prose and clean tool status lines, not raw
JSON or fabricated results; unexecuted tool calls are surfaced back to the
model so it doesn't loop.

## Actual Behavior

See Description — session burns all 12 turns producing no real work.

## Environment

- Platform: macOS (darwin), tetond via launchd (brew services)
- Version: v0.1.5 (main @ b956c73)

## Root Cause

1. **`/` jail**: `Runtime::from_env` (`crates/tetond/src/runtime.rs:533`)
   resolves the tool jail from `TETON_REPO_ROOT` else the **daemon's** cwd —
   which is `/` under launchd. The ACP `session/new` request carries no cwd,
   so the client's directory never reaches the daemon; `ToolContext` is built
   once from the daemon-global root (`runtime.rs:1161`).
2. **No stop**: `GenParams` (`crates/teton-inference/src/engine.rs:11`) has
   only `max_tokens`/`temperature`; nothing halts generation when the first
   top-level JSON tool call completes or when the model starts fabricating the
   transcript frame (`Tool (...):` / `User:`) that `ContextManager::assemble`
   (`crates/tetond/src/harness/context.rs:382`) taught it.
3. **Raw stream + context pollution**: `LocalEngineSource::produce_turn`
   streams every token to `agent_message`, and the turn loop pushes the entire
   reply into context (`crates/tetond/src/harness/turn_loop.rs:448`).
4. **Silent drop**: `parse_turn` returns only the first valid call; the loop
   folds the tool result with no mention of the discarded calls
   (`turn_loop.rs`), and the remote path likewise ignores parallel calls
   (`crates/tetond/src/harness/completion.rs:309`).

## Resolution

Four interlocking fixes, one per root cause:

1. **Per-session tool jail**: `session/create` now carries the client's `cwd`
   (absolute, must exist — validated at create time); the CLI sends its
   terminal directory, the registry stores it on the session, and
   `run_prompt_turn` builds `ToolContext` from it. The daemon-global
   `repo_root` is only the fallback for clients that send none.
2. **Generation stops at the turn boundary**: `Engine::complete`'s `on_token`
   callback now returns `bool` (continue/stop). A new `ReplyScanner`
   (`harness/reply.rs`) ends local generation at the first complete top-level
   JSON tool call or at a fabricated transcript-frame marker (`User:`,
   `Assistant:`, `Tool (`, `<tool-result` at a line start, outside JSON), and
   the reply is cut there before parsing. Agent turns get a 1,024-token
   budget (the 256 default was the summarize/classify budget and cut calls
   mid-JSON).
3. **Clean stream, clean context**: a `StreamGate` between the token stream
   and `agent_message` events streams prose live but withholds tool-call JSON
   (the tool status line presents the call) and suppresses fabricated frames;
   the context now folds only the cut reply (prose + the dispatched call),
   never the hallucinated tail.
4. **Dropped calls are surfaced**: extra tool calls in one reply (local parse
   or remote parallel calls) are counted and a harness note rides the executed
   call's result telling the model only the first ran — ending the silent-drop
   re-emit loop.

## Files Changed

- `crates/teton-protocol/src/methods.rs` — `SessionCreateParams.cwd`,
  `SessionSummary.cwd` (optional, wire-compatible), round-trip tests
- `crates/teton/src/main.rs` — CLI sends `std::env::current_dir()` on
  session create
- `crates/tetond/src/sessions.rs` — registry stores the session cwd
- `crates/tetond/src/server.rs` — validates the cwd (absolute + exists),
  passes it into the prompt turn
- `crates/tetond/src/runtime.rs` — per-session `ToolContext` jail with
  daemon-root fallback; `ScriptedFileEngine` honors early stop
- `crates/teton-inference/src/engine.rs` — `Engine::complete` callback
  returns continue/stop; `MockEngine`/`LlamaEngine` honor it, text reflects
  what was emitted
- `crates/teton-inference/src/benchmark.rs`, `tests/llama_smoke.rs` —
  callback signature updates
- `crates/tetond/src/harness/reply.rs` — **new**: `ReplyScanner`,
  `parse_reply` (dropped-call counting, clean cut), `StreamGate`; 21 tests
- `crates/tetond/src/harness/completion.rs` — local source scans/stops/cleans;
  `SourceTurn.dropped_calls`; remote source counts dropped parallel calls
- `crates/tetond/src/harness/turn_loop.rs` — stream-gate wiring, dropped-call
  notice folded onto results, 1,024-token agent turns, parser moved to
  `reply.rs`
- `crates/tetond/src/harness/context.rs`, `harness/mod.rs`,
  `tests/offline_session.rs`, `tests/nonblocking_inference.rs` — signature
  and module updates

## Deployment

n/a — plain OSS flow (PR-gated CI on `main`, no staging/production service
pipeline). The fix ships in the next tagged release; local users pick it up
via `brew upgrade teton`.
