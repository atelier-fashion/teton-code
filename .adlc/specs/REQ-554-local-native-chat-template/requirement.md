---
id: REQ-554
title: "Local tier renders prompts through the model's native chat template"
status: complete
deployable: true
created: 2026-08-03
updated: 2026-08-12
component: "inference/local"
domain: "harness"
stack: ["rust", "llama.cpp", "gguf"]
concerns: ["reliability", "latency", "developer-experience"]
tags: ["chat-template", "prompt-format", "tool-calling", "bug-147", "transcript-frames"]
---

## Description

The local tier currently drives llama.cpp with a hand-rolled flat transcript
string — `User:\n…`, `Assistant:\n…`, `Tool (read):\n…` blocks ending in a
bare `Assistant:` cue. Instruct-tuned models were never trained on this
format: it measurably degrades tool-call fidelity, and it is the format the
model learned to *continue* — fabricating fake tool results and future turns —
which is what BUG-147's containment (ReplyScanner stop/cut, StreamGate,
dropped-call notices) now suppresses after the fact (informed by BUG-147,
LESSON-472).

This REQ moves the local tier onto the model's **native chat template**: the
GGUF's embedded template (ChatML-style for the Qwen catalog family) is applied
to the harness's already-role-typed messages — the `PreparedPrompt`
system + user/assistant message shape that REQ-544 M-8 introduced for remote
providers — so the local model sees the exact prompt format it was trained
on. The flat rendering remains only as an explicit, visible fallback for
models without a usable template. Expected payoff: fewer malformed/fabricated
turns at the source (the weak local model is the product's thesis — BR-6),
less wasted local inference on text the containment throws away, and one
prompt-shaping model shared by both tiers.

The BUG-147 containment is **not** removed or weakened by this REQ — it stays
as defense in depth in both rendering modes.

## System Model

### Entities

| Entity | Field | Type | Constraints |
|--------|-------|------|-------------|
| PromptRendering | mode | enum: `native_template` \| `flat` | required; resolved once per loaded engine, not per turn |
| PromptRendering | template_id | string | present iff mode = `native_template`; names the recognized template (e.g. `chatml`) |
| PromptRendering | model_id | string | required; the engine the resolution applies to |

### Events

| Event | Trigger | Payload |
|-------|---------|---------|
| template_fallback | Engine load resolves no usable chat template (absent, unrecognized, or rendering fails a self-check) | model_id, reason (one line, no prompt content) |

(Exact surface — dedicated event vs. a lifecycle/log line — is an
architecture decision; the requirement is that the downgrade is visible,
once per engine load.)

### Permissions

n/a — no user-facing permission changes; removed per template guidance.

## Business Rules

- [ ] BR-1: When the loaded GGUF carries a chat template the engine
      recognizes, every local agent turn is rendered through it — system
      prompt plus alternating role-typed user/assistant messages, with tool
      results riding as user-role turns exactly as the remote path's
      `PreparedPrompt.messages` already does (REQ-544 M-8). The flat
      transcript rendering is never sent to such a model.
- [ ] BR-2: A model with no usable template falls back to the flat rendering,
      and the downgrade is **visible** — reported once per engine load with
      the model id and reason, never silently (informed by LESSON-447,
      LESSON-456). The fallback preserves current behavior exactly (the
      BUG-147-fixed flat path).
- [ ] BR-3: The BUG-147 containment — ReplyScanner stop/cut, StreamGate
      display filtering, dropped-call notices — remains active in **both**
      rendering modes. Native templating reduces fabrication; it does not
      replace the containment (informed by LESSON-472, LESSON-441).
- [ ] BR-4: In native-template mode, the fabrication-marker set matches the
      rendering: the template's own role delimiters (e.g. `<|im_start|>`)
      are turn-boundary/fabrication markers for the scanner, in addition to
      the tool-call stop. Markers for a format the model is not being shown
      must not cause false stops (informed by LESSON-472).
- [ ] BR-5: All context/window budgeting is computed against the **rendered**
      prompt — template token overhead (per-message specials) counts toward
      the byte-denominated budgets, and the budgets remain
      currency-compatible with the engine window (informed by LESSON-446).
      An over-window rendered prompt still refuses with the typed error
      before any FFI call — template application must never make a
      GGML assert reachable (informed by LESSON-444).
- [ ] BR-6: Template resolution and rendering failures are typed errors that
      select the fallback path; they never abort the process and never
      silently produce a half-templated prompt (informed by LESSON-444,
      LESSON-447).
- [ ] BR-7: The local tier's duty prompts (BR-8 summarize/classify calls,
      e.g. `summarize_if_large`) render under the same template when one is
      available — as a single user-message conversation — with the same BR-2
      fallback rules. The instruct model's format benefit applies to duties
      exactly as it does to agent turns; a split where turns are templated
      but duties are not would leave the summarizer on the degraded format.

## Acceptance Criteria

- [x] AC-1: With a template-bearing GGUF (Qwen catalog family), the prompt
      handed to the engine contains the template's role delimiters and does
      NOT contain the flat frame (`User:`, `Assistant:`, `Tool (`) as
      structural markers.
- [x] AC-2: Tool results appear as user-role turns under the template, with
      consecutive same-role messages merged so alternation holds (same
      contract the remote mapping already pins).
- [x] AC-3: Loading a model with no recognized template produces exactly one
      visible fallback report (model id + reason) and the session runs on
      the flat rendering with behavior identical to today's.
- [x] AC-4: All BUG-147 containment tests pass unchanged in both modes, and a
      new test pins template-mode fabrication: a reply that emits the
      template's own role header (e.g. `<|im_start|>user`) is cut before
      context and never displayed.
- [x] AC-5: An over-window *rendered* prompt (template overhead included) is
      refused with the existing typed engine error — pinned by a test sized
      to cross the window only when template overhead is counted (informed
      by LESSON-446).
- [ ] AC-6: The feature-gated real-model smoke (`--features llama`,
      `#[ignore]`d) drives one turn through the native template and observes
      a single well-formed tool call — single-platform mock-only green is
      not acceptance for this REQ (informed by LESSON-433, LESSON-448).
- [x] AC-7: Default/CI builds (no `llama` feature) compile and pass: mock and
      scripted engines run under the flat fallback with no template
      machinery required at runtime.
- [x] AC-8: Template-mode rendering is verifiable in default/CI builds: a
      rendered prompt for a known template family can be produced and
      inspected without the `llama` feature, so AC-1/AC-2/AC-5 are pinned by
      the always-on suite, not only by the `#[ignore]`d real-model smoke
      (informed by LESSON-433, LESSON-448 — properties tested only against
      feature-gated paths are properties CI cannot defend).

## External Dependencies

- llama.cpp chat-template support (`llama_chat_apply_template`) as exposed by
  the pinned `llama-cpp-2` 0.1.x binding. Note: llama.cpp implements a fixed
  set of *recognized* templates (ChatML, Llama-style, etc.) by matching, not
  arbitrary Jinja execution — unrecognized templates take the BR-2 fallback.
  If the binding does not expose the API at the pinned version, architecture
  chooses between upgrading the binding and a minimal first-party ChatML
  renderer keyed off GGUF metadata (both satisfy this spec).

## Assumptions

- All four catalog models (Qwen GGUF repos + the unsloth 30B quant, ADR-005)
  embed a ChatML-style template that llama.cpp's matcher recognizes — to be
  verified against the pinned artifacts during architecture, per model, not
  assumed from the family name.
- The remote provider path is unaffected: it already consumes the role-typed
  `PreparedPrompt` mapping (REQ-544 M-8) and never sees the flat rendering.
- The scripted/mock engines used by e2e fixtures continue to run in flat
  mode (they carry no GGUF template), so existing fixtures stay valid
  without rewrites (AC-7).

## Open Questions

- [x] OQ-1 (RESOLVED 2026-08-03, user-confirmed): the tool-call protocol
      stays the harness's inline-JSON contract — this REQ changes prompt
      *framing* only, so the parser/containment surface is unchanged.
      Native tool-calling formats (e.g. Qwen `<tool_call>` tags) are a
      separate follow-up REQ.
- [x] OQ-2 (RESOLVED 2026-08-03, user-confirmed): the template-mode
      fabrication-marker set is a hardcoded list for the recognized
      template families (ChatML covers the entire current catalog);
      deriving markers from the template text is deferred until a
      non-ChatML model enters the catalog.

## Out of Scope

- Native structured tool-calling formats and grammar-constrained sampling
  (OQ-1's follow-up).
- Any change to remote provider prompt shaping (already role-typed).
- Changing the client↔daemon protocol or the scripted-engine transcript
  format used by e2e fixtures.
- Model catalog changes.

## Retrieved Context

- LESSON-456 (lesson, score 7): A `_`-discarded error is a silent downgrade — the daemon knew exactly why, and told the user something else
- BUG-146 (bug, score 7): First prompt after install fails with a message blaming the local engine for a config/timing problem
- LESSON-441 (lesson, score 5): A fix pass is new code — re-verify it adversarially, not by test count
- LESSON-433 (lesson, score 5): Single-platform local verification gives false confidence for cross-platform code
- LESSON-444 (lesson, score 4): A C library's assert is a process abort — validate inputs before the FFI boundary
- LESSON-446 (lesson, score 4): Token budgets that meet at a boundary must share a currency (approx-words ≠ BPE)
- LESSON-447 (lesson, score 4): A best-effort fallback must preserve the invariant it backs up — and fail loudly
- LESSON-448 (lesson, score 4): Test-double speed masks executor blocking — pin async offload with a gated engine on one worker
- LESSON-452 (lesson, score 4): A stateful decoder's lifetime must match the stream it decodes — per-chunk wrappers silently drop bytes
- LESSON-453 (lesson, score 4): A spare-capacity API makes a zero-capacity call a silent no-op — read the callee's buffer contract
- LESSON-457 (lesson, score 3): An executable's filename is a trust surface
- LESSON-443 (lesson, score 3): A guard keyed on a feature's absence disables itself when the feature lands
- LESSON-445 (lesson, score 3): Side effects of a minutes-long operation must be staged, then committed only after re-checking authority
- LESSON-449 (lesson, score 3): A clean-compiling rebase can revert a parallel PR's invariant — compose intents, then run both PRs' tests
- LESSON-450 (lesson, score 3): An event published before the state applies is not a sync point — wait on a state-derived surface

Additionally used (authored this session, below the tag-score cut but
directly motivating): BUG-147 (resolved), LESSON-472 (weak-model turn
containment), LESSON-473 (daemon cwd is not client cwd).
