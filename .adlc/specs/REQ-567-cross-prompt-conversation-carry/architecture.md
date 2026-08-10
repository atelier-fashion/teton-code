# REQ-567 — Architecture: cross-prompt conversation carry

## Approach

The change is state management, not harness mechanics. Today
`run_prompt_turn` (`runtime.rs:2051`) builds a fresh `ContextManager`
(`runtime.rs:2139-2141`) per `session/prompt`; the turn loop mutates it and it
dies with the turn. After this REQ, the **session registry owns each
session's conversation** — the ordered blocks the harness retained — and the
per-prompt dispatch becomes: rebuild the system head for *this* prompt,
replay the session's committed blocks into a fresh manager, push the new user
message, run the turn, and **commit the manager's post-turn blocks back to
the registry only when the turn completes**. The turn loop (`turn_loop.rs`)
does not change; everything it pushes into the manager during the turn is
exactly what commit persists.

The conversation lives in `sessions.rs` (the session store), not in a sixth
runtime side-map: a conversation is canonical session state — the thing ACP
says a session *is* — while the runtime's existing per-session maps
(`session_taint`, `session_gates`, `session_user_urls`) are cross-cutting
concerns. The store follows their lifetime discipline anyway: created on
first use, never pruned within a daemon lifetime (a `SessionId` is not
reused, and under REQ-565 the daemon exits with its last client, which
bounds the growth).

## Data model

```rust
// sessions.rs
pub struct Conversation {
    /// Ordered blocks as the harness retained them (post cut, post
    /// compaction), with per-block role + provenance — the same
    /// `ContextBlock` the ContextManager holds.
    blocks: Vec<ContextBlock>,
}
```

Registry surface (all methods on the existing `SessionRegistry`, using the
`claim_title` lock discipline — atomic check-and-act inside one lock):

- `conversation_snapshot(&self, id) -> Vec<ContextBlock>` — what dispatch
  replays before `push_user`.
- `commit_conversation(&self, id, blocks: Vec<ContextBlock>)` — whole-vector
  replacement; the atomic unit of BR-6. No partial appends, ever.
- `clear_conversation(&self, id) -> usize` — empties, returns blocks dropped
  (the `context_cleared` payload).
- `try_begin_turn(&self, id, turn_id) -> Result<TurnClaim, InFlight>` /
  release on drop — the BR-5 serialization gate (D-3).

**The system head is never stored** (spec assumption): `blocks` excludes the
system prompt. Each dispatch rebuilds the head from the current
tools/route (`build_system_prompt`) and replays blocks under it — a
mid-session head change re-renders the same conversation under a new head,
which is BR-7's cache-independence requirement met by construction.

## Decisions

### D-1: Commit protocol — whole-vector replace on completion; drop-guard for cancellation

- **Success** (turn returns `Ok`): dispatch calls
  `commit_conversation(id, ctx.into_blocks())`. What the manager holds at
  turn end *is* the retained view: post-`ReplyScanner`-cut model text
  (BUG-147), post-compaction blocks — commit is a move, not a re-derivation.
- **Failure** (turn returns `Err`): no commit. The registry still holds the
  pre-turn vector; BR-6/AC-5 hold by construction because commit is the only
  writer and it never ran.
- **Cancellation** (OQ-1, product decision: retain prose, drop incomplete
  tool work): there is no cancel RPC today — cancellation is the server
  aborting the spawned turn task on client disconnect
  (`server.rs:254-400`). An aborted future never reaches either branch
  above, so OQ-1 is implemented as a **commit-on-drop guard**: dispatch
  wraps the manager in a guard armed between `push_user` and turn
  completion; if the guard drops while armed (task abort), it commits the
  blocks the manager holds at that moment. Every block in the manager is
  complete by construction — text and tool results are pushed only on
  completion of each generation/tool — so "retain prose, drop incomplete
  tool work" falls out: the pending tool call that never finished was never
  pushed. Success and error paths disarm the guard first (success commits
  explicitly; error must not commit).

### D-2: `/clear` requires one new RPC — the spec's "no new RPCs" line is amended

The spec's System Model asserted "No new RPCs". That sentence was written
against today's architecture, where there is nothing daemon-side to clear.
Under carry the conversation is daemon state, and the daemon is the only
place it can be cleared; a client-local `/clear` would be a lie (the next
prompt would still carry). The method surface (`methods.rs`: create, attach,
prompt, permission/respond, model/confirm, web/refresh, web/override,
model/status, cost/query) has no session-scoped verb that can carry this
without overloading `session/prompt` — which BR-2 forbids changing.

**Decision**: add `session/clear` — `{ session_id } -> { blocks_dropped }` —
the daemon clears the conversation and emits `context_cleared`. The spec's
sentence is corrected in the same branch (System Model note now reads "one
new RPC, `session/clear`; `session/prompt`'s wire shape is unchanged").
ACP mapping: none (ACP has no clear; bespoke additions are ADR-002's
expected shape). The CLI `/clear` slash command (REQ-555 pattern) issues it;
AC-6's "unreachable by the model by construction" holds because slash
dispatch happens before prompt construction and no tool in the registry
wraps the RPC.

### D-3: Concurrent turns on one session are refused, not queued (OQ-2)

`try_begin_turn` claims the session for one turn (the `claim_title`
atomic-claim discipline); a second `session/prompt` while a turn is in
flight gets a **typed session-busy error naming the in-flight turn id** —
never a generic turn failure (LESSON-456, BUG-152's transient-state
precedent). Refusal over queueing because: the refusal is honest and
immediate where a queue is silent and unbounded; the CLI prompter is
sequential so a single client never sees it; and a queued design can be
layered on later without changing the wire (a busy error is retryable).
The claim guard releases on drop, so an aborted turn cannot wedge the
session (the same drop path that runs D-1's commit guard).

### D-4: Provenance and egress — replay through the same authoring seams

Carried blocks keep their `ContextBlock` provenance verbatim; replay uses
the same push methods (`push_model`, `push_tool_result_prov`) the live turn
used, so `context_provenance(ctx)` and the egress choke point see carried
content exactly as they saw it same-turn (BR-3). Sanitization lives at the
render layer (ADR-009, LESSON-474): carried blocks re-render through
`assemble`/`prepare` neutralization every turn, so carry adds **no new
injection surface** — nothing bypasses the authoring-layer sanitizers by
having been in context before. Session taint (REQ-558/REQ-563) is already
session-scoped and unchanged; `clear_conversation` does NOT touch it
(OQ-4 product decision: conversation only).

### D-5: Prefix cache — no coupling, no clear-time eviction

Carry changes what the harness feeds the engine, nothing else (spec
out-of-scope). After `/clear`, the resident KV describes a conversation
that no longer exists; the next prompt's probe simply finds the system head
as the longest common prefix — a `divergent` hit at ~head size, exactly
today's boundary shape — so correctness needs no eviction and REQ-564's
eviction surface stays untouched. A well-behaved carried boundary becomes a
pure extension (`divergent: false`, AC-8); a boundary after a fabricating
turn diverges at the cut point (LESSON-500, expected); a post-compaction
boundary diverges at the rewrite (AC-3). BR-7's A/B (cache on/off/evicted,
identical assembled context) holds because the conversation store never
reads anything cache-side.

### D-6: Compaction commits what it produced

`compact_if_pressured` (REQ-561 duty, with deterministic-truncation
failsafe) runs during turn assembly as today; since commit persists the
manager's post-turn blocks, a compaction that fired mid-turn is what
carries forward — BR-4 with no new machinery. The classifier's input stays
the new user message under its existing `CLASSIFIER_INPUT_MAX_BYTES = 2048`
head/tail cap (`classify.rs:71-81`); BR-10/AC-11 pin that the cap-site
input is the prompt text, not the assembled context, so duty cost cannot
scale with conversation length.

## Blast radius

| File | Change |
|---|---|
| `crates/tetond/src/sessions.rs` | `Conversation`, registry methods, turn claim |
| `crates/tetond/src/harness/context.rs` | `into_blocks()` / `replay_blocks()` seams on `ContextManager` |
| `crates/tetond/src/runtime.rs` | dispatch seeding, commit/disarm paths, busy refusal, `session/clear` handler |
| `crates/tetond/src/server.rs` | route `session/clear`; (abort path unchanged — guard rides the task) |
| `crates/teton-protocol/src/methods.rs` | `SessionClearParams`/`Result` |
| `crates/teton-protocol/src/events.rs` | `Event::ContextCleared` + wire test |
| `crates/teton/src/slash.rs` | `/clear` command row + handler |
| `crates/teton/src/session_ui.rs` | render `context_cleared` |
| `.adlc/specs/REQ-567-*/requirement.md` | D-2 spec erratum (System Model note) |
| tests | see task files (carry e2e beside `prefix_cache_session.rs`, CLI e2e beside REQ-555's, wire shape beside `events.rs` tests, egress-capture beside `e2e/harness.rs` users) |

## Task graph

```
TASK-092 (store + ContextManager seams)
   ├── TASK-093 (dispatch wiring: seed/commit/guard/busy)
   └── TASK-094 (session/clear RPC + context_cleared event + spec erratum)
             └── TASK-095 (CLI /clear + rendering)
TASK-096 (acceptance matrix: privacy, cache warmth, A/B, classifier pin,
          multi-client, mutation check; spec checkbox flips; dogfood
          follow-up section) — depends 093, 094, 095
```

TASK-093 ∥ TASK-094 after 092; TASK-095 after 094; TASK-096 last.

## Lessons applied

LESSON-456/BUG-152 (typed busy refusal, honest degradation), LESSON-474 /
ADR-009 (sanitize at render; carry adds no injection surface), LESSON-495
(clear does not widen grants/taint), LESSON-500 (kept-vs-decoded divergence
is expected telemetry), LESSON-446/491 (budgets unchanged, enforced at the
last transform), LESSON-448 (no new blocking on the async path — the store
is a fast Mutex, never held across a turn), ASSUME-005 (multi-session
thrash: carry makes `session_switch` misses *more* expensive to ignore —
noted as evidence for its validation, no change here).
