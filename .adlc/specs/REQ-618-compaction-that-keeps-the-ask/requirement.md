---
id: REQ-618
title: "Compaction that keeps the ask — the user's prompt and the active skill body survive every compaction, a skill body that cannot fit the route is refused with its size, and every compaction is a transcript record"
status: draft
deployable: true
created: 2026-09-04
updated: 2026-09-04
component: "daemon/session"
domain: "harness"
stack: ["rust", "daemon"]
concerns: ["reliability", "developer-experience", "cost"]
tags: ["compaction", "context-pressure", "transcript", "skill", "budget", "digest", "compact-duty", "pinned-blocks", "user-ask", "context-loss", "retained-context"]
---

## Description

The user's report: *"in other sessions I've seen it lose context from prompt
to the next."* The 2026-09-04 transcript (`sess-23aczryx…`, v0.1.30) shows the
mechanism. Pinned to the local tier (REQ-614) with a 21,162-token budget
(REQ-616), the session ran the `compact` duty ten times across four prompts.
The third prompt was `/analyze`: a 25,252-byte skill body plus a 3,811-byte
ethos include, roughly 8,000 tokens, or 38 % of the budget before a single
tool result. Twenty-six tool calls later the model no longer had the skill's
instructions or the user's intent in context. The fourth prompt, *"where are
the results?"*, was answered with fourteen more directory listings and a
`mkdir`, because the only thing the model still had was its own recent tool
history.

Two facts about compaction made this invisible. First, the `compact` duty
chooses what to keep by recency and size; the block that carried the ask — the
skill expansion, or on an ordinary turn the user's prompt — is the oldest
block in the turn and the first to go. REQ-586 made context pressure loud
(`context_pressure` fires on truncation), but a *compaction* is a duty call,
not a truncation, and the transcript records only its `route_decided` line:
what was dropped, how many bytes, and whether the user's ask survived is
nowhere in the file. Second, the skill expansion was admitted at all: REQ-589
offers to proceed when a body exceeds the budget, and REQ-587 admits an
expansion whole or refuses it, but a body at 38 % of the window is *under*
budget and still leaves the turn no room to work.

This REQ gives compaction an anchor set that is never summarized away, makes a
skill expansion that leaves the turn too little room a typed refusal with the
arithmetic, and writes every compaction to the transcript with what it kept and
what it dropped (informed by REQ-567, REQ-586, REQ-589, LESSON-500, LESSON-446).
REQ-616 removes most of the pressure; this REQ governs the case where pressure
still occurs — a smaller window on a memory-limited machine or provider, a user cap, or a genuinely long
turn.

## System Model

### Entities

| Entity | Field | Type | Constraints |
|--------|-------|------|-------------|
| ContextBlock (existing) | anchor | enum `none` / `user_ask` / `skill_body` / `repo_context` / `system` | set at push time by the harness, never by the model; `user_ask` on the prompt block of the current turn and the most recent prior turn; `skill_body` on the active skill expansion of the current turn |
| AnchorSet | blocks | ordered list | every block with `anchor ≠ none`; compaction and truncation may not summarize, middle-elide or drop a member; if the anchors alone exceed the budget the turn is refused, not compacted |
| CompactionRecord | kept_bytes / dropped_bytes / summarized_bytes | usize | totals per compaction; `anchor_bytes` reported separately |
| CompactionRecord | dropped_blocks | list of `(kind, provenance_class, bytes)` | no content; `provenance_class` is `rooted` / `boundary` / `unknown` so a privacy reader can see what a summary derived from |
| CompactionRecord | route | (provider_id, model) | the duty's route, as `route_decided` already reports |
| SkillFitVerdict | kind | `fits` / `fits_without_room` / `over_budget` | `fits_without_room` when `body_bytes > room_fraction × budget_bytes`; `room_fraction` is a pinned constant (proposed 0.25) |

### Events

| Event | Trigger | Payload |
|-------|---------|---------|
| `context_compacted` (new) | every `compact` duty completion, or the mechanical-truncation fallback when the duty fails (LESSON-447) | the CompactionRecord fields; `fallback: true` when mechanical |
| `context_pressure` (existing) | any truncation | gains `anchors_intact: true` — by construction always true when emitted; its presence is the invariant's witness |
| `skill_refused_no_room` (new) | SkillFitVerdict `fits_without_room` | `skill`, `body_bytes`, `budget_bytes`, `room_fraction`, `route`, remedy (a larger route via `/policy`, or REQ-616's window) |
| `turn_refused_anchors_exceed_budget` (new) | AnchorSet alone exceeds the budget | `anchor_bytes`, `budget_bytes`, the anchor kinds |

### Permissions

| Action | Roles Allowed |
|--------|---------------|
| mark a block as an anchor | the harness, from block kind and turn position; never a tool result, a skill body, or `TETON.md` text asking to be kept |
| accept a `fits_without_room` expansion anyway | the user, through REQ-589's existing offer (`proceed once`); the model cannot |

## Business Rules

- [ ] BR-1: **The ask is an anchor.** The current turn's user prompt block and the previous turn's user prompt block carry `anchor: user_ask`. A compaction that would need to summarize or drop them is not performed; instead the oldest non-anchor blocks are dropped until the budget fits, and if it still does not, the turn is refused with `turn_refused_anchors_exceed_budget` (REQ-586: nothing is clamped in silence).
- [ ] BR-2: **The active skill body is an anchor for its turn.** A skill expansion carries `anchor: skill_body` for the turn in which it was expanded; on the next prompt turn it is an ordinary block. A model-invoked skill (REQ-587) anchors the same way; two anchored bodies in one turn are refused at the second expansion with the arithmetic (this is the case BR-4 governs).
- [ ] BR-3: **Anchors are harness-assigned and provenance-carrying.** The anchor flag is set from the block's kind at push time; nothing in the block's text can request it (LESSON-624: markers in content are content). An anchored block keeps its provenance and is inspected at egress like any other; an anchor never exempts a block from a `local-only` refusal.
- [ ] BR-4: **A skill that fits the budget but leaves no room is refused with numbers.** `SkillFitVerdict` is computed before expansion: `fits_without_room` when the body exceeds `room_fraction` of the route's byte budget. The refusal is typed, names the body size, the budget, the fraction and the remedy, and goes through REQ-589's offer so the user may proceed once. On a route at REQ-616's local window the shipped ADLC bodies all fit; the rule exists for the routes that are not there.
- [ ] BR-5: **Every compaction is a transcript record.** `context_compacted` is published on the bus and therefore reaches the transcript tap; it carries the byte totals, the dropped block list without content, and the route. The mechanical-truncation fallback emits the same record with `fallback: true` (LESSON-447: degrade loudly, never fold silently).
- [ ] BR-6: **The compaction summary says what it replaced.** The summary block the `compact` duty produces opens with a harness-authored line: *"[summary of <n> earlier blocks, <bytes> bytes, from turns <a>–<b>; the user's prompts are kept verbatim below]"*. The line is outside the untrusted frame, so the model can distinguish a summary from a tool result (LESSON-500: what the cache holds is not what context holds; the model must be told the same).
- [ ] BR-7: **A summary of unknown or boundary provenance keeps it.** Unchanged from today, restated because BR-5's `provenance_class` field makes it visible: a compaction over an `unknown` block yields an `unknown` summary (REQ-544 C-2).
- [ ] BR-8: **`RetainedContext` carries anchors across prompts.** REQ-567's carry keeps the previous turn's `user_ask` anchor so the next prompt's model sees the last thing the user asked even after a compaction between the two prompts; the anchor lapses one turn later (two prompts back is ordinary history).

## Acceptance Criteria

- [ ] AC-1: With a stub engine sized at 21,162 tokens, a turn that expands a 25 KB skill body and then receives 40 KB of tool results triggers compaction; after it, the user's prompt block and the skill body are byte-identical to what was pushed, and the dropped blocks are all tool results (`inspect, don't infer` — the retained context is read from `into_retained`, LESSON-519).
- [ ] AC-2: A turn whose anchors alone exceed the budget is refused with `turn_refused_anchors_exceed_budget` naming both figures; nothing is sent to the model.
- [ ] AC-3: A skill body at 30 % of a route's byte budget (with `room_fraction = 0.25`) yields `skill_refused_no_room` and REQ-589's offer; `proceed once` expands it and anchors it; `decline` ends the turn with no model call.
- [ ] AC-4: The transcript of AC-1's session contains one `context_compacted` record per compaction, each with `kept_bytes + dropped_bytes + summarized_bytes` equal to the pre-compaction total and `anchor_bytes ≤ kept_bytes`.
- [ ] AC-5: When the `compact` duty's engine fails, the mechanical fallback emits `context_compacted { fallback: true }` and the anchors are still intact.
- [ ] AC-6: A tool result whose text contains `anchor: user_ask` or a `[summary of …]` line is pushed with `anchor: none` and inside the untrusted frame (LESSON-550: assert the absence of the effect, not the presence of the sanitizer).
- [ ] AC-7: Across two prompts with a compaction between them, the second prompt's request body contains the first prompt's text verbatim; on the third prompt it may be summarized.
- [ ] AC-8: The 2026-09-04 transcript's third and fourth prompts replayed against a stub model with the original 21,162-token budget: the fourth prompt's request body contains the `/analyze` prompt line and the user's *"where are the results?"* verbatim.
- [ ] AC-9: A summary block derived from an `unknown` provenance block is refused at remote egress; `privacy_block.path` is `<unknown-provenance>`.

## External Dependencies

- None.

## Assumptions

- `room_fraction = 0.25` is a starting value; the shipped ADLC bodies (largest 25 KB) fit any route at or above REQ-616's 262,144-token local window, and the fraction only bites on routes below it.
- The `context_compacted` record fits the transcript's `max_record_bytes` default (65,536) because it carries counts and kinds, not content.

## Open Questions

- [ ] OQ-1: Should the `repo_context` block (`TETON.md`, REQ-612) be an anchor? Recommended: yes — it is resident data the user asked for, and it is already capped at 8 KiB.
- [ ] OQ-2: Should a compaction ever be triggered *between* prompts (idle compaction) rather than only when a turn needs room? Out of scope here; noted for REQ-567's successor.

## Out of Scope

- Raising any window or budget (REQ-616).
- Changing the `compact` duty's route or the digest thresholds.
- A user-facing `/compact` command.

## Retrieved Context

- REQ-567 (spec, score 12): Cross-prompt conversation carry in interactive sessions
- REQ-600 (spec, score 11): Decompose run_prompt_turn into a stage sequence
- REQ-598 (spec, score 11): TurnContext: dissolve the parameter clump
- REQ-599 (spec, score 11): Decompose the turn path and split runtime.rs
- REQ-589 (spec, score 11): Offer to proceed when a skill expansion exceeds the route's context budget
- LESSON-518 (lesson, score 11): A blocking gate's reader-loop freedom is not inherited from the await-based reader-loop tests
- LESSON-519 (lesson, score 11): An 'assert by inspection, not from the error' AC needs the real artifact
- LESSON-520 (lesson, score 11): A gate that fires before deserialization makes an invalid-payload test vacuous
- REQ-611 (spec, score 10): Daemon-side transcript logging
- REQ-586 (spec, score 10): A turn's context budget follows its route
- BUG-193 (bug, score 9): The prompt-margin ledger drifts silently while its test stays green
- LESSON-570 (lesson, score 9): A prompt sentence must be true after the REQ ships, not before it
- REQ-591 (spec, score 9): The project-skill trust gate and its unattended allowlist
- BUG-184 (bug, score 9): Skill discovery runs on the connection's synchronous reader loop
- BUG-188 (bug, score 9): A model-invoked expansion caught at a mid-turn reroute ends the turn instead of relaying
