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

- [ ] BR-1: **A prompt turn begins from the retained conversation.** The
      context for prompt N+1 consists of the current system head, every block
      the harness retained from prompts 1..N — as it kept them: post
      containment cut, post compaction — and the new user message, in order.
      What carries is the harness's retained view, never the model's decoded
      view (the two diverge wherever the harness edits output — see
      LESSON-500). A follow-up that refers to earlier prompts is answerable
      without re-supplying files or restating the conversation.
- [ ] BR-2: **The conversation lives in the daemon; the wire shape is
      unchanged.** Clients continue to send only the new prompt in
      `session/prompt`. Every client attached to a session extends one shared
      conversation — a second client's prompt sees the first client's turns.
      Conversations are keyed by session: no block from one session's
      conversation ever appears in another session's context.
      (informed by REQ-544 BR-4/AC-6)
- [ ] BR-3: **Provenance survives carry.** Every carried block keeps its
      egress provenance, and the BR-1 choke-point inspection treats carried
      content identically to same-turn content: boundary content read in an
      early prompt produces the same `privacy_block`/reroute when a later
      prompt routes remote. REQ-563's session-taint rules continue to apply
      unchanged — taint is already session-scoped and now guards a
      conversation that actually spans the session. Egress-capture verified,
      not code-inspected. (informed by REQ-544 BR-1/AC-5, REQ-563 BR-13)
- [ ] BR-4: **The context budget spans the session.** The token/byte budget
      applies to the carried conversation; under pressure the existing
      compaction machinery runs and the compacted history is what carries
      forward. An over-window rendered prompt is still refused with the typed
      error before any FFI call, on both the KV hit and miss paths (REQ-564
      BR-7 unchanged). A long-lived session degrades to compaction, never to
      a failed turn. A compacted boundary may legitimately reuse less KV than
      the prior turn's prefix; it shows as a `divergent` hit or a miss
      alongside the compaction — never silently. (informed by LESSON-491,
      REQ-564 BR-7)
- [ ] BR-5: **One conversation never interleaves.** Two in-flight
      `session/prompt` calls on one session (possible today — each runs on its
      own task) must not interleave or fork the conversation. Whether the
      second turn queues in arrival order or is refused with a typed
      session-busy error is an architecture decision; the constraints are that
      the transcript stays linear, the outcome is deterministic, and a refusal
      names its cause truthfully rather than surfacing as a generic turn
      error. (informed by LESSON-456, BUG-152)
- [ ] BR-6: **A turn's blocks join the conversation atomically, on
      completion.** A turn that fails leaves the conversation exactly as it
      was when the turn started — the next prompt's context contains no
      partial blocks from the failed turn. Duty calls (title naming,
      summarize/classify) never contribute blocks to the conversation; they
      are not turns. (Cancelled-turn retention is OQ-1.)
- [ ] BR-7: **Carry is correct independent of the KV cache.** The assembled
      context for any turn is identical with the prefix cache enabled,
      disabled, evicted, or divergent, and identical across a mid-session
      system-head change (tools or route changing between prompts). Cache
      state may only ever change latency — REQ-564 BR-1's posture, extended
      across prompt boundaries. (informed by REQ-564 BR-1)
- [ ] BR-8: **Clearing is explicit, user-only, and visible.** A clear command
      empties the session's conversation, emits `context_cleared`, and the
      next prompt starts from the system head alone. The model cannot invoke
      it, and observed content cannot trigger it. What else clearing resets —
      if anything — is OQ-4; by default it resets the conversation only.
      (informed by REQ-563 BR-13, LESSON-495)
- [ ] BR-9: **The conversation is daemon-lifetime state, honestly.** It dies
      with the daemon — under REQ-565's default policy, when the last client
      disconnects. No persistence, no replay-on-attach in this REQ; a client
      attaching to a live session joins its conversation from now on, and
      nothing claims otherwise. (informed by REQ-565)
- [ ] BR-10: **Duty inputs do not scale with the conversation.** The route
      classifier — and any other fixed-frame duty that reads "this turn's
      prompt" — consumes the new user message inside a fixed frame; its input
      size must not grow with conversation length. Carry changes what the
      agent turn sees, not what the reflex-tier duties pay. (informed by
      REQ-558)

## Acceptance Criteria

- [ ] AC-1: **The recap test.** Scripted e2e session of ≥3 prompts: the
      rendered prompt the engine receives for turn N contains turn N-1's user
      message and retained assistant reply, and a final recap-shaped prompt's
      rendered context contains the content it asks about. The received
      context is the scripted leg's evidence; tool-free recap answering is
      only meaningful on AC-8's real-model leg. This test fails against
      today's binary.
- [ ] AC-2: Privacy: egress-capture — prompt 1 reads `local-only` boundary
      content (served locally); prompt 2 routes remote; the boundary content
      triggers the same `privacy_block`/reroute as a same-turn inclusion, and
      no boundary bytes appear in any remote payload for the session.
      (informed by REQ-544 AC-5)
- [ ] AC-3: Budget: a session driven past the context budget across several
      prompts compacts rather than fails; later prompts succeed; no process
      abort and no over-window error escape; the post-compaction boundary
      emits a `divergent: true` prefix-cache hit reusing the common head
      (REQ-564 AC-3's shape, now across prompts).
- [ ] AC-4: Serialization: two concurrent prompts on one session resolve per
      the BR-5 decision — both transcripts linear and deterministic, or the
      second refused with the typed busy error naming the in-flight turn —
      and the conversation afterward contains no interleaved blocks.
- [ ] AC-5: Atomicity: a scripted turn that errors after a completed tool
      call leaves the next prompt's context byte-identical to what it would
      have been had the failed turn never run.
- [ ] AC-6: Clear: the clear command emits `context_cleared`; the next
      prompt's assembled context contains no prior conversation; and clear is
      unreachable by the model **by construction** — no tool in the registry
      exposes it, asserted by a tool-surface test, so the user-only rule
      (BR-8) is structural rather than checked at call time.
- [ ] AC-7: A/B correctness: a fixed-seed multi-prompt scripted session
      produces byte-identical assembled contexts and outputs with the KV
      cache enabled vs disabled (REQ-564 AC-2 extended across boundaries).
- [ ] AC-8: Boundary warmth: in a scripted multi-prompt session with caching
      on, each well-behaved prompt boundary emits `prefix_cache_hit` with
      `divergent: false` and `cached_tokens` equal to the full retained prior
      context — not the system head. Ledger rows carry the matching
      cached-vs-processed counts (REQ-564 BR-9). The real-model leg is a
      dogfood follow-up in `docs/manual-verification.md`, superseding the
      2026-08-10 measurement in which every boundary reused only the
      ~814-token head.
- [ ] AC-9: Multi-client continuity: client A prompts and disconnects while
      client B stays attached (holding the daemon alive); client B prompts
      the same session and its turn context contains A's conversation.
      (informed by REQ-544 AC-6)
- [ ] AC-10: Mutation check: reverting to a per-prompt fresh context (the
      current `ContextManager::new` per dispatch) turns AC-1 and AC-8 red.
- [ ] AC-11: Duty-input bound: across a ≥5-prompt session, the route
      classifier's input length stays fixed while the agent context grows
      (BR-10).
- [ ] AC-12: Cross-session isolation: two interleaved sessions each carry
      only their own conversation; a prompt in one session that references
      the other session's content gets a rendered context containing none of
      it (BR-2).

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
- [ ] OQ-2: BR-5 queue-vs-refuse for concurrent turns on one session —
      architect's latitude with a UX voice; queuing matches user expectation,
      refusal is simpler and honest if the error is typed.
- [ ] OQ-3: Confirm at architecture time that no duty output (titles,
      summaries, redaction verdicts) should ever join the conversation —
      BR-6 assumes never; is there a future duty that legitimately should?
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