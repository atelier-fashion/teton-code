---
id: REQ-567
title: "Cross-prompt conversation carry in interactive sessions"
status: approved
deployable: true
created: 2026-08-10
updated: 2026-08-10
component: "daemon/session"
domain: "harness"
stack: ["rust", "daemon"]
concerns: ["privacy", "latency", "cost", "reliability", "developer-experience"]
tags: ["conversation-carry", "context-carry", "session-state", "context-manager", "multi-turn", "acp", "prefix-cache", "kv-cache", "compaction", "session"]
---

## Description

An interactive `teton` session forgets the conversation at every prompt
boundary. The daemon builds a fresh `ContextManager` for each `session/prompt`
(`crates/tetond/src/runtime.rs:2139-2141`: `ContextManager::new(system, …)`
then `push_user(prompt)`), so a turn's context is the system head plus the new
prompt and nothing else. Context accumulates only *within* one prompt's agent
loop (model text and tool results per generation); when the turn returns, all
of it is dropped. Nobody else holds it either: the CLI creates one session and
sends only the new prompt text per line (`PromptTurnParams { session_id,
prompt }`), which is the charter's surface-parity rule working as designed —
clients hold no session state the daemon lacks (REQ-544 BR-4) — except the
daemon lacks it too.

Observed product break (2026-08-10 dogfood, sign-off in
`docs/manual-verification.md`): in a six-prompt session, prompt 6 — "recap
what we learned" — was answered with "Could you please share the relevant
files?". The REQ-564 prefix-cache events told the same story from below:
every prompt boundary reused exactly the ~814-token system head, because that
head is all consecutive prompts share.

This statelessness is a build-out gap, not a recorded decision. No spec
declares prompts independent; the session registry describes itself as "the
skeleton's session store" (`crates/tetond/src/sessions.rs`); and three
existing commitments already assume carry:

1. **The protocol.** `teton-protocol` borrows ACP naming precisely so "a
   future ACP compatibility shim is mostly a rename exercise". ACP is explicit
   that sessions "maintain their own context, conversation history, and
   state", and that a client sends another `session/prompt` "to continue the
   conversation, building on the context established in previous turns". A
   stateless daemon makes the promised shim a semantics gap, not a rename.
2. **The charter.** Surface parity (REQ-544 BR-4) plus multi-client sessions
   (REQ-544 AC-6) only mean something if the session a second client attaches
   to *has* a conversation to share.
3. **REQ-564's goal.** "Within a session, a turn whose rendered prompt shares
   a token prefix with the previous turn's prompt prefills only what changed"
   was written over session-scoped turns; its AC-1 test hand-builds a growing
   transcript "exactly as an agent session does". Without carry, the KV cache
   delivers that goal only inside a single prompt's agent loop, and boundary
   reuse is capped at the system head forever.

The system prompt already tells the model that answering "from the
conversation" is a legal ending (BUG-154's fix); this REQ is what makes the
conversation exist for that ending to use. Carry also realizes REQ-564's
boundary payoff: with the conversation retained, a well-behaved prompt
boundary becomes a pure KV extension instead of a `divergent` hit at ~814
tokens.

## System Model

### Entities

| Entity | Field | Type | Constraints |
|--------|-------|------|-------------|
| SessionConversation | session_id | string | required; exactly one conversation per session; owned by the daemon (REQ-544 BR-4 surface parity) |
| SessionConversation | blocks | ordered context blocks | user, assistant, and tool-result blocks **as the harness retained them** (post ReplyScanner cut, post compaction), in turn order |
| SessionConversation | per-block provenance | egress provenance | required on every block; carried unchanged from the turn that produced it (REQ-544 BR-1) |
| SessionConversation | size | tokens and bytes | bounded by the harness context budget; compaction, not failure, is the response to pressure |

### Events

| Event | Trigger | Payload |
|-------|---------|---------|
| context_cleared | user clears the session's conversation | session_id, blocks_dropped (count) |

One new RPC, `session/clear` (architecture D-2); `session/prompt`'s wire shape
is unchanged. Carry is visible through existing surfaces: REQ-564's
`prefix_cache_*` events and BR-9 ledger counts at prompt boundaries, and the
assembled-context behavior itself.

### Permissions

| Action | Roles Allowed |
|--------|---------------|
| Clear the conversation | user only, via a client command — never the model, never observed content (REQ-563 permission posture) |

## Business Rules

- [x] BR-1: **A prompt turn begins from the retained conversation.** The
      context for prompt N+1 consists of the current system head, every block
      the harness retained from prompts 1..N — as it kept them: post
      containment cut, post compaction — and the new user message, in order.
      What carries is the harness's retained view, never the model's decoded
      view (the two diverge wherever the harness edits output — see
      LESSON-500). A follow-up that refers to earlier prompts is answerable
      without re-supplying files or restating the conversation.
- [x] BR-2: **The conversation lives in the daemon; the wire shape is
      unchanged.** Clients continue to send only the new prompt in
      `session/prompt`. Every client attached to a session extends one shared
      conversation — a second client's prompt sees the first client's turns.
      Conversations are keyed by session: no block from one session's
      conversation ever appears in another session's context.
      (informed by REQ-544 BR-4/AC-6)
- [x] BR-3: **Provenance survives carry.** Every carried block keeps its
      egress provenance, and the BR-1 choke-point inspection treats carried
      content identically to same-turn content: boundary content read in an
      early prompt produces the same `privacy_block`/reroute when a later
      prompt routes remote. REQ-563's session-taint rules continue to apply
      unchanged — taint is already session-scoped and now guards a
      conversation that actually spans the session. Egress-capture verified,
      not code-inspected. (informed by REQ-544 BR-1/AC-5, REQ-563 BR-13)
- [x] BR-4: **The context budget spans the session.** The token/byte budget
      applies to the carried conversation; under pressure the existing
      compaction machinery runs and the compacted history is what carries
      forward. An over-window rendered prompt is still refused with the typed
      error before any FFI call, on both the KV hit and miss paths (REQ-564
      BR-7 unchanged). A long-lived session degrades to compaction, never to
      a failed turn. A compacted boundary may legitimately reuse less KV than
      the prior turn's prefix; it shows as a `divergent` hit or a miss
      alongside the compaction — never silently. **Scope note (verify,
      2026-08-10):** moving the budget gate to the top of the loop (D-6) makes
      the `compact` duty reachable on the first iteration, so a *tool-free*
      pressured conversation can now egress to a remote `compact` binding —
      previously impossible by construction, since the gate's only call site was
      the tool-result fold. Intended, and named here rather than left implicit:
      the duty egress scoping of REQ-561 BR-7 and the session taint still govern
      what may be sent, so what changed is which turns can reach the duty, not
      what the duty may send. (informed by LESSON-491, REQ-564 BR-7)
- [x] BR-5: **One conversation never interleaves.** Two in-flight
      `session/prompt` calls on one session (possible today — each runs on its
      own task) must not interleave or fork the conversation. Whether the
      second turn queues in arrival order or is refused with a typed
      session-busy error is an architecture decision; the constraints are that
      the transcript stays linear, the outcome is deterministic, and a refusal
      names its cause truthfully rather than surfacing as a generic turn
      error. (informed by LESSON-456, BUG-152)
- [x] BR-6: **A turn's blocks join the conversation atomically, on
      completion.** A turn that fails leaves the conversation exactly as it
      was when the turn started — the next prompt's context contains no
      partial blocks from the failed turn; a turn that panics is a failed turn
      for this purpose. **Out-of-band duty output never joins the
      conversation**: a duty that answers a question *about* a turn — naming
      the session, classifying the route, judging an outbound payload — writes
      nothing into it, because its answer is not something the session said.
      Duties that *rewrite the conversation in place* are a different thing and
      are not excluded: `compact`'s summary and `digest`'s condensed tool result
      **are** the retained view of conversation content (OQ-3). (Cancelled-turn
      retention is OQ-1.)
- [x] BR-7: **Carry is correct independent of the KV cache.** The assembled
      context for any turn is identical with the prefix cache enabled,
      disabled, evicted, or divergent, and identical across a mid-session
      system-head change (tools or route changing between prompts). Cache
      state may only ever change latency — REQ-564 BR-1's posture, extended
      across prompt boundaries. (informed by REQ-564 BR-1)
- [x] BR-8: **Clearing is explicit, user-only, and visible.** A clear command
      empties the session's conversation, emits `context_cleared`, and the
      next prompt starts from the system head alone. The model cannot invoke
      it, and observed content cannot trigger it. What else clearing resets —
      if anything — is OQ-4; by default it resets the conversation only.
      (informed by REQ-563 BR-13, LESSON-495)
- [x] BR-9: **The conversation is daemon-lifetime state, honestly.** It dies
      with the daemon — under REQ-565's default policy, when the last client
      disconnects. No persistence, no replay-on-attach in this REQ; a client
      attaching to a live session joins its conversation from now on, and
      nothing claims otherwise. (informed by REQ-565)
- [x] BR-10: **Duty inputs do not scale with the conversation.** The route
      classifier — and any other fixed-frame duty that reads "this turn's
      prompt" — consumes the new user message inside a fixed frame; its input
      size must not grow with conversation length. Carry changes what the
      agent turn sees, not what the reflex-tier duties pay. (informed by
      REQ-558)

## Acceptance Criteria

- [x] AC-1: **The recap test.** Scripted e2e session of ≥3 prompts: the
      rendered prompt the engine receives for turn N contains turn N-1's user
      message and retained assistant reply, and a final recap-shaped prompt's
      rendered context contains the content it asks about. The received
      context is the scripted leg's evidence; tool-free recap answering is
      only meaningful on AC-8's real-model leg. This test fails against
      today's binary.
- [x] AC-2: Privacy: egress-capture — prompt 1 reads `local-only` boundary
      content (served locally); prompt 2 routes remote; the boundary content
      triggers the same `privacy_block`/reroute as a same-turn inclusion, and
      no boundary bytes appear in any remote payload for the session.
      (informed by REQ-544 AC-5)
- [x] AC-3: Budget: a session driven past the context budget across several
      prompts compacts rather than fails; later prompts succeed; no process
      abort and no over-window error escape; the post-compaction boundary
      emits a `divergent: true` prefix-cache hit reusing the common head
      (REQ-564 AC-3's shape, now across prompts).
- [x] AC-4: Serialization: two concurrent prompts on one session resolve per
      the BR-5 decision — both transcripts linear and deterministic, or the
      second refused with the typed busy error naming the in-flight turn —
      and the conversation afterward contains no interleaved blocks.
- [x] AC-5: Atomicity: a scripted turn that errors after a completed tool
      call leaves the next prompt's context byte-identical to what it would
      have been had the failed turn never run.
- [x] AC-6: Clear: the clear command emits `context_cleared`; the next
      prompt's assembled context contains no prior conversation; and clear is
      unreachable by the model **by construction** — no tool in the registry
      exposes it, asserted by a tool-surface test, so the user-only rule
      (BR-8) is structural rather than checked at call time.
- [x] AC-7: A/B correctness: a fixed-seed multi-prompt scripted session
      produces byte-identical assembled contexts and outputs with the KV
      cache enabled vs disabled (REQ-564 AC-2 extended across boundaries).
- [x] AC-8: Boundary warmth: in a scripted multi-prompt session with caching
      on, each well-behaved prompt boundary emits `prefix_cache_hit` with
      `divergent: false` and `cached_tokens` equal to the full retained prior
      context — not the system head. Ledger rows carry the matching
      cached-vs-processed counts (REQ-564 BR-9). The real-model leg is a
      dogfood follow-up in `docs/manual-verification.md`, superseding the
      2026-08-10 measurement in which every boundary reused only the
      ~814-token head.
- [x] AC-9: Multi-client continuity: client A prompts and disconnects while
      client B stays attached (holding the daemon alive); client B prompts
      the same session and its turn context contains A's conversation.
      (informed by REQ-544 AC-6)
- [x] AC-10: Mutation check: reverting to a per-prompt fresh context (the
      current `ContextManager::new` per dispatch) turns AC-1 and AC-8 red.
      **Executed 2026-08-10** (TASK-096), by two routes to the same end state
      and a third for provenance, each reverted immediately; the observed
      failures are recorded in `crates/tetond/tests/conversation_carry.rs`'s
      module doc. Dropping the dispatch seeding reddened AC-1, AC-11, AC-12,
      BR-1's kept-view test, OQ-1's cancellation test and both e2e legs;
      emptying `commit_conversation` reddened those plus AC-8 and AC-3;
      replaying tool blocks without their provenance reddened AC-2 and put the
      fixture repo's secret on the wire (`BR-1 VIOLATION` from the suite-wide
      egress capture).
- [x] AC-11: Duty-input bound: across a ≥5-prompt session, the route
      classifier's input length stays fixed while the agent context grows
      (BR-10).
- [x] AC-12: Cross-session isolation: two interleaved sessions each carry
      only their own conversation; a prompt in one session that references
      the other session's content gets a rendered context containing none of
      it (BR-2).

**Where each AC is verified** (TASK-096): the mapping from AC to the test that
holds it is the module doc of `crates/tetond/tests/conversation_carry.rs` — the
three homes are that file (AC-3, AC-7, AC-8), the in-crate
`runtime::tests::conversation_carry` module (AC-1, AC-4, AC-5, AC-6, AC-11,
AC-12, whose claims are about the dispatch and need the crate-private engine
slot), and `crates/tetond/tests/e2e/conversation_carry.rs` over the real socket
(AC-2, AC-9).

**Open for manual verification** (`docs/manual-verification.md`, REQ-567): the
two legs no default build can run, because no model is linked into one — AC-1's
tool-free recap *answering* (the scripted leg proves the recap's material
reached the context, not that a model used it) and AC-8's real-model boundary
measurement (the scripted leg proves the reuse policy, not that llama.cpp
reuses the KV). Both are the same dogfood session; status NOT RUN.

## External Dependencies

- None. `ContextManager` (Clone, block-granular, per-block provenance via the
  existing provenance hooks), the compaction machinery, the egress choke
  point, and REQ-564's prefix cache all exist.

## Assumptions

- **Verified in code (2026-08-10):** `ContextManager` is `Clone` and tracks
  provenance per block (`push_tool_result_prov`, `context_provenance`), so
  daemon-held conversation state with surviving provenance is representable
  today. Whether the daemon stores the post-turn manager or the blocks
  re-headed under the current system prompt is architecture's call — the
  system head is rebuilt per prompt from tools/route and must not be
  fossilized inside the carried state.
- A mid-session system-head change (route, tools, or mode changing between
  prompts) costs KV warmth from the divergence point onward — with carry that
  can be the whole conversation, versus ~814 tokens today. Correctness is
  unaffected (BR-7); the latency cost is accepted and observable through the
  existing `divergent` telemetry. Head stability as a cache-value property is
  noted for architecture, not ruled on here.
- Rendering is deterministic turn-over-turn (REQ-554), so an unchanged
  carried conversation re-renders byte-identical — the property that makes a
  boundary a pure KV extension. REQ-564 assumed this within a turn; carry
  leans on it across turns, and AC-8 measures it rather than trusting it.
- Carried blocks re-render through the same template/neutralization path as
  fresh blocks each turn (sanitization lives at the render layer — the
  LESSON-474 posture), so carry introduces no new injection surface: nothing
  bypasses the authoring-layer sanitizers by having been in context before.
- LESSON-500's divergence is expected behavior under carry, not a defect: the
  resident KV holds what was decoded (including fabricated continuations the
  containment cut), the conversation holds what was retained, so a boundary
  after a fabricating turn is a `divergent` hit at the cut point and REQ-564's
  amended BR-2 reuses up to it.
- The REQ-544 retry/fallback known limitation (accumulated context re-sent
  and re-billed on mid-turn retry) now includes carried history and therefore
  grows in cost. Accepted here; cost-neutral retries remain tracked under
  REQ-544's follow-up, not this REQ.
- id allocated with remote verification (no degradation warning from the
  allocator).

## Open Questions

- [x] OQ-1 — RESOLVED (product decision, 2026-08-10): **retain prose, drop
      tool work.** A cancelled turn keeps its streamed text in the
      conversation (the user saw it; ACP replays it) and drops incomplete
      tool calls/results. BR-6's clean-rollback rule applies to *failed*
      turns only; cancellation is its own case.
      **Scope of "retain prose" (verify, 2026-08-10):** it covers *completed*
      generations — text the harness had already folded into the context when
      the turn was cancelled. Prose lost mid-generation, where the abort lands
      between tokens and nothing has been pushed, is **not** retained: the
      harness pushes a model turn when the generation ends, so a partial
      generation is not a block for the commit to keep. The narrower promise is
      the honest one; retaining a half-decoded generation would also mean
      carrying text the containment scanner never got to cut (LESSON-500).
      The dangling call the cancellation *does* have to handle is the one that
      was fully generated and then parked at the permission gate: its assistant
      block is committed with the call trimmed off and the prose ahead of it
      kept. **That, and only that** (verify, second pass): a *remote* turn's
      call is a structured event and is not in the text, so its prose is
      committed verbatim however tool-call-shaped the JSON it quotes; and a call
      whose tool already ran is not incomplete work, so a cancellation after
      dispatch commits the call block as it stands. A committed conversation may
      therefore hold a dispatched-but-unfolded call — the honest record, and
      better than erasing a request whose edit is on the disk. "Incomplete tool
      work" means work that never ran.
- [x] OQ-2 — RESOLVED (architecture D-3, 2026-08-10): **refused, not queued.**
      A second `session/prompt` while a turn is in flight gets a typed
      session-busy error naming the turn that holds the session; the claim
      releases on drop, so an aborted turn cannot wedge it. A queue can be
      layered on later without a wire change (a busy error is retryable).
      Verified by AC-4's test.
- [x] OQ-3 — RESOLVED (2026-08-10): **it depends on what the duty is for, and
      the line is already load-bearing.** Two kinds of duty exist and BR-6's
      original sentence collapsed them.
      *Out-of-band duties* answer a question **about** a turn and produce
      nothing the session said: `title` names the session, `classify` picks a
      route, `redact` judges an outbound payload. None of their output joins
      the conversation, now or in a future duty of the same shape — an answer
      about the conversation is not part of it, and letting one in would put
      harness-authored narration in the model's transcript with nobody having
      said it.
      *In-band duties* rewrite conversation content **in place**, and their
      output is already in the conversation by design: `compact`'s replacement
      summary is what stands in for the blocks it elides, and `digest`'s
      condensed tool result is the form in which an oversized result enters
      context at all. Both are the retained view (BR-1: "as the harness kept
      them"), both inherit the provenance of what they replace, and both are
      framed as untrusted data on the way in. Excluding them would mean either
      carrying the pre-compaction history the budget just dropped or carrying
      nothing where a tool result was.
      So the rule is not "duty output never joins" but "**output that is about
      the conversation never joins; output that is a rewrite of the
      conversation is the conversation**". BR-6 is reworded to say that.
- [x] OQ-4 — RESOLVED (product decision, 2026-08-10): **conversation only.**
      Clear empties the conversation and nothing else — REQ-563 session
      taint, the user-pasted-URL set, and session permission grants all
      survive. A routinely-typed clear must never silently widen egress or
      consent (a grant is only as narrow as its key — LESSON-495).

## Out of Scope

- ACP `session/load`-grade conversation replay to clients, and any
  cross-restart persistence — the conversation dies with the daemon
  (REQ-565's session-resume out-of-scope note stands; separate REQ if
  wanted).
- Remote-provider prompt caching (Anthropic prompt caching has its own
  billing semantics; unchanged by this REQ).
- Retry cost-neutrality (REQ-544 known limitation; grows with carry, tracked
  there).
- KV cache policy changes — single slot, eviction rules, and BR-2/BR-3 of
  REQ-564 are unchanged; this REQ only changes what the harness feeds it.
- Compaction algorithm redesign — the existing machinery is reused, now
  firing across prompts.
- Structured-mode phase semantics: whether a `phase_transition` clears or
  carries the conversation is a phase-gate question for the structured-mode
  REQ that lands that machinery; until then structured sessions carry at
  prompt level exactly like freeform ones.

## Retrieved Context

- REQ-563 (spec, score 8): Opt-in web lookup through the egress choke point
- REQ-554 (spec, score 7): Local tier renders prompts through the model's native chat template
- REQ-564 (spec, score 6): Persistent llama context: prefix-cached KV across agent turns
- LESSON-456 (lesson, score 6): A `_`-discarded error is a silent downgrade — the daemon knew exactly why, and told the user something else
- BUG-146 (bug, score 6): First prompt after install fails with a message blaming the local engine for a config/timing problem
- LESSON-498 (lesson, score 5): A !Send FFI handle bound to a borrow wants a thread, not a struct field
- LESSON-493 (lesson, score 5): A prompt ending is only reachable if its knowledge source exists — bundle what only the product knows
- BUG-160 (bug, score 5): Asked how to hook up external models, the agent searches the user's repo
- LESSON-482 (lesson, score 5): A prompt that enumerates a turn's legal endings must name every one
- BUG-154 (bug, score 5): The system prompt describes no ending for a question that needs no files
- LESSON-474 (lesson, score 5): If the tokenizer treats a string as frame, so must your renderer
- REQ-544 (spec, score 5): Teton Code — hybrid local/remote AI coding agent with workflow-aware model routing
- REQ-565 (spec, score 4): On-demand daemon lifetime: exit with the last client
- LESSON-494 (lesson, score 4): A security gate and the client that executes the request must share one parser
- LESSON-495 (lesson, score 4): A remembered grant answers every question its key matches

Notes on this retrieval: the spec-status filter treats `complete` as the
local spelling of `deployed`, per the precedent recorded in REQ-557's
Retrieved Context (this project's shipped specs all carry `status:
complete`). The Step-1.6 delegated body-read was invoked and failed
(empty completion, exit 1); the documented `api-error` fallback ran and the
top-15 bodies were read directly.

Additionally read in-conversation (below the tag-score cut but directly
motivating): LESSON-500 — what the cache holds is not what context holds
(its tags spell `kv-cache`/`prefix-reuse`, so it scored 3 against this
query); and the 2026-08-10 investigation of `runtime.rs:2139`,
`sessions.rs`, `teton-protocol` (ACP mapping comments), the ACP
prompt-turn/session-setup documentation, and the REQ-564 dogfood sign-off in
`docs/manual-verification.md`, from which this REQ's Description derives.

**Consolidation note (2026-08-10):** this spec absorbs REQ-566
("Cross-prompt conversation carry in interactive sessions", specced in
parallel from the same dogfood finding). Folded in from REQ-566: the
duty-input bound (BR-10/AC-11, from its BR-6/AC-6), the explicit
cross-session isolation sentence and test (BR-2/AC-12, from its BR-4/AC-4),
the compacted-boundary loudness note on BR-4 (from its amended BR-7), and
the AC-1 scripted-leg evidence scoping (from its `/validate` pass). REQ-566
is retired: its branch is deleted, nothing merged, the id abandoned.