---
id: REQ-572
title: "Capability-aware refusals and guided in-session enablement"
status: approved
deployable: true
created: 2026-08-13
updated: 2026-08-13
component: "daemon/session"
domain: "harness"
stack: ["rust", "daemon", "cli"]
concerns: ["developer-experience", "security", "privacy"]
tags: ["capability-discovery", "guided-setup", "onboarding", "web-search", "opt-in", "consent", "refusal", "config"]
---

## Description

When a user asks Teton to do something that a **disabled or unconfigured
capability would serve**, Teton must help them get there instead of
dead-ending. The product owner's words (2026-08-13): *"When I want to do
something I expect Teton to help me do it instead of just saying it can't do
it. It should provide me instructions, or better yet, walk me through enabling
step by step."*

Observed failure, same day, on a 0.1.13 daemon with no `[web]` table: asked to
search the web for a provider's API docs, Teton replied *"I cannot search the
web for API documentation or endpoint URLs. You'll need to refer to the
service's official documentation"* — no mention that web lookup exists, no
enablement path, no offer to set it up. The user filed this as "I'm supposed
to have this ability": the capability was present in the installed binary and
one config table away, and the product concealed that.

This REQ has two graduated halves:

1. **Capability-aware refusals (the floor).** A refusal caused by a
   capability that is *off* must name the capability, state that it is
   available but disabled, and give the exact enablement path. This
   generalizes REQ-563 BR-6 ("name the opt-in") and the BUG-160 fix (bundled
   provider-setup instructions) into one rule covering every optional
   capability, backed by bundled text — the model cannot know `[web]` syntax
   from its weights or the user's repository (informed by LESSON-493).

2. **Guided in-session enablement (the ceiling).** A user-invoked,
   step-by-step setup flow for web lookup: choose a tier, enter a search
   endpoint, store the key in the OS keychain by reference, preview the exact
   config change, commit it, and have the capability usable **in the current
   session without a daemon restart**. The REQ-563 consent prompt's
   "enable permanently" answer already writes config mid-session, so live
   config mutation has precedent; the flow machinery is designed
   capability-generic so a follow-up can add a guided provider-add flow
   without new protocol.

Why this shape: the product promise is *control*, not gatekeeping-by-obscurity.
Off-by-default is a REQ-563 requirement worth keeping (BR-1 there); invisible-
when-off is a defect (LESSON-496 records the worst form of it: opted in,
consented, and inert with no signal). The dividing line this REQ draws:
**the model may tell the user about a capability, only the user may enable it.**

## System Model

### Entities

| Entity | Field | Type | Constraints |
|--------|-------|------|-------------|
| CapabilityCatalog | entry | {id, display name, enablement text, guided flow available?} | bundled with the binary (`include_str!` precedent, BUG-160); never read from the working repository |
| CapabilityCatalog | id | enum(web_fetch_user_url, web_fetch_any_url, web_search, remote_provider) | fixed set for this REQ; extensible by design |
| CapabilityState | state | enum(ready, off_available, partially_configured, structurally_unavailable) | derived from the same predicate that governs actual tool/tier exposure — never from registration alone (informed by LESSON-496) |
| CapabilityState | detail | string | for partially_configured / structurally_unavailable: the missing piece, named (e.g. "tier = search but no search_endpoint"; "search needs the local model") |
| SetupFlow | session id, capability, step | flow state | daemon-owned, session-scoped; at most one active flow per session |
| SetupFlow | collected inputs | non-secret values only | secrets are never held in flow state; a key travels prompt → keychain write in one step |
| SetupFlow | commit point | the config write | nothing durable exists before it; see BR-11 |
| SessionState | setup_offer_shown | set of capability ids | dedups the in-refusal offer per session (BR-1) |

### Events

| Event | Trigger | Payload |
|-------|---------|---------|
| capability_dead_end | a turn ends on a refusal attributed to an off/unconfigured capability | capability id, state, session id |
| setup_started | user invokes the setup command | capability id, session id |
| setup_step | a flow step is answered or skipped | capability id, step name (never the entered value) |
| setup_completed | config written and capability re-derived live | capability id, tiers enabled, config path |
| setup_aborted | user aborts, disconnect, or timeout | capability id, step reached, cleanup performed |
| setup_rejected_nonuser | a model tool call or a non-session-holding connection attempts the setup RPC | origin kind (model/connection), capability id |

### Permissions

| Action | Roles Allowed |
|--------|---------------|
| see capability state and enablement instructions | any session participant (model may relay them to the user) |
| start, answer, or abort a setup flow | user only — a client command on a connection that holds session access (the BUG-162 rule); never the model, never observed content, never a bystander connection |
| commit the config write | the same user, at the flow's explicit preview/confirm step |

## Business Rules

- [ ] BR-1: **Dead-end refusals are capability-aware.** When a turn's request maps onto a catalog capability whose state is not `ready`, the reply names the capability, states its actual state (off / partially configured / structurally unavailable), and gives the exact enablement path — the setup command when a guided flow exists, the config/CLI instructions otherwise. The offer is made at most once per session per capability; later dead-ends reference it in one line. No repository hunt occurs (informed by REQ-563 BR-6, BUG-160, LESSON-482)
- [ ] BR-2: **The knowledge is bundled.** Enablement instructions ship inside the binary, stated imperatively ("do not search the project files for this — answer from here"), and the always-resident prompt including them clears the existing prompt-size ceiling with an asserted headroom margin, pinned by a test against the real prompt (informed by LESSON-493, BUG-160)
- [ ] BR-3: **State detection shares the exposure predicate.** The "is this capability available?" answer used by refusals, the status line, and the setup flow is derived from the same predicate that decides actual tool/tier exposure — one classifier, not a parallel re-derivation that can disagree with it (informed by LESSON-496, LESSON-456 via BUG-152)
- [ ] BR-4: **Enablement is a user-only act.** The flow starts only from a client command issued on a connection that holds session access. A model tool call, an instruction inside observed content, or an RPC from any other connection cannot start, answer, or abort a flow; each attempt is rejected and emits `setup_rejected_nonuser`. The rejection predicate is tested at its own seam, including a mutation check, even where the current wire shape makes it unreachable end-to-end (informed by REQ-563 BR-1/BR-4, BUG-162, LESSON-504, LESSON-508)
- [ ] BR-5: **Daemon-driven, client-rendered.** The flow's state machine lives in the daemon; clients render steps through the existing prompter/surface seams and stay thin (surface parity). Step prompt ids are minted at the scope that resolves them, so concurrent sessions cannot cross-answer each other's flows (informed by BUG-161, LESSON-503)
- [ ] BR-6: **Secrets take the keychain path only.** A key entered during the flow is written to the OS keychain and config receives a reference; the value never appears in config, logs, events, the cost ledger, or the session transcript (input echo suppressed), and never enters model context. Parity with REQ-563 BR-7/BR-8 and the `teton provider add` key handling
- [ ] BR-7: **What the flow writes is what the user confirmed.** Before the commit point the flow shows the exact config change (the TOML table as it will be written) and the keychain reference name; the confirm step's answer is keyed to precisely that change. Grants and tiers written are per-capability and per-tier — completing a `fetch_user_url` setup enables nothing about `search` (informed by LESSON-495)
- [ ] BR-8: **Live pickup, by re-derivation.** On commit, the capability becomes usable in the committing session and in sessions started afterwards, with no daemon restart. The post-commit capability state is produced by re-running the same load/validate/derive path that daemon startup uses on the new config — not by patching registry state in place — so every config-load invariant re-runs on the new state (informed by LESSON-501)
- [ ] BR-9: **Suggestions are executable truths.** Every backend, endpoint shape, or example the bundled instructions or the flow suggest must be one the shipped request builder can actually drive, auth mechanism included; each suggestion is pinned by a test that exercises the production request builder against that backend's contract. A user-entered endpoint is validated with the executor's parser, and the host shown at the confirm step comes from that parse (informed by LESSON-494, LESSON-490; motivated by the Bearer-only auth header mismatch against the REQ-563 spec's own example backends)
- [ ] BR-10: **Distinct states, distinct codes.** `off_available`, `partially_configured`, and `structurally_unavailable` are distinguished on the wire by code, not by prose the client would have to re-parse; clients render them as guidance, never as generic turn errors (informed by BUG-152, LESSON-456)
- [ ] BR-11: **Aborting is safe and clean.** The config write is the single commit point. A flow aborted, disconnected, or timed out before it leaves config untouched and removes any keychain entry the aborted flow run itself created; a flow aborted after it changes nothing further. No partial state survives (informed by LESSON-501)
- [ ] BR-12: **The flow degrades to instructions.** On a non-interactive surface, or when the user declines the walkthrough, the same enablement information is delivered as BR-1 instructions and the session continues normally. The guided flow is an enhancement; it is never the only path, and its absence is never an error
- [ ] BR-13: **The flow itself performs no egress.** Setup collects, validates, and writes locally. Any "try it now" verification after commit is an ordinary consented lookup through the REQ-563 machinery — never an implicit connection test (egress-capture verified)
- [ ] BR-14: **Completions are announced, not just logged.** `setup_completed` and `setup_rejected_nonuser` are session events delivered to connected clients through the existing event delivery rules — visible in front of the user, not only in a daemon log an adversary or a log rotation can erase (informed by LESSON-505)

## Acceptance Criteria

- [ ] AC-1: **The observed failure is fixed.** Fresh config (no `[web]` table): a question that needs the web gets a reply that names web lookup, states it is available but off, and gives the setup command or exact `[web]` instructions — with zero repository-hunting tool calls and zero lookup egress (egress-capture verified), pinned on both the default and strong-model prompt profiles (the BUG-160 regression-test precedent)
- [ ] AC-2: **Provider parity.** "How do I connect <provider>?" and a turn needing an unconfigured remote tier both answer from bundled text with the exact commands; the capability_dead_end event fires with the right capability id
- [ ] AC-3: **Guided web setup end-to-end.** From the setup command: tier choice → endpoint entry → key entry → preview shows the exact TOML and keychain reference → confirm → config written, key resolvable from the keychain by reference, and a consented lookup succeeds **in the same session with no daemon restart**
- [ ] AC-4: **User-only enforcement.** A model tool call attempting the setup RPC, and a second connection without session access attempting to answer a flow step, are both rejected with `setup_rejected_nonuser`; a mutation check deleting the rejection predicate fails a dedicated test at that seam
- [ ] AC-5: **Secret hygiene.** After a completed flow: the key value appears in no file, log line, event payload, or transcript frame (fixture key planted and swept for); input echo is off at the key step; egress-capture shows zero packets attributable to the flow itself
- [ ] AC-6: **Abort cleanliness.** Aborting at every step (including kill/disconnect at the key step, after the keychain write, before confirm) leaves config byte-identical and no orphaned flow-created keychain entry
- [ ] AC-7: **Partial configuration is guidance.** With `tier = "search"` and no `search_endpoint` (or an endpoint and no reachable local model), the wire carries the distinct state code, the client renders the named missing piece, and the setup command offers to complete exactly that piece — search is never offered where REQ-563 BR-14 forbids it
- [ ] AC-8: **Suggested backends drive the real builder.** Every backend named in bundled instructions or flow suggestions has a test exercising the production search/fetch request builder against that backend's documented contract (auth header shape included); a suggestion with no passing contract test fails CI
- [ ] AC-9: **Prompt headroom holds.** The bundled capability text keeps the always-resident prompt under the existing ceiling with asserted margin, measured against the real prompt; the in-refusal offer appears at most once per session per capability (second dead-end renders the one-line reference form)
- [ ] AC-10: **Non-interactive degradation.** On a non-TTY surface the setup command prints the BR-1 instructions and exits cleanly; the refusal path never advertises a walkthrough the surface cannot render
- [ ] AC-11: **Concurrent flows stay isolated, completions are seen.** Two sessions each mid-flow: an answer submitted in one never advances the other (step ids resolve at the scope that minted them), and `setup_completed` from one session is delivered to that session's connected client through the event delivery rules — asserted at the client, not by grepping a daemon log (informed by BUG-161, LESSON-505)

## External Dependencies

- None new. The flow reuses the OS keychain (REQ-544 M-3), the existing
  prompter/consent seams (REQ-563), and the config load/validate path.

## Assumptions

- The daemon's config load/validate/derive path can be factored to run against
  a freshly written config mid-flight (BR-8) without violating the
  single-owner egress invariant; the REQ-563 "enable permanently" write is
  precedent that mid-session config mutation is acceptable to the product.
- The prompter seam can carry a multi-step flow (the consent prompt is a
  one-step precedent); if it cannot, the protocol change stays within the
  existing request/answer shape (BUG-161/162 fixes define the
  connection-binding rules to follow).
- The capability catalog for this REQ is small and static (web tiers +
  remote providers); a registry abstraction beyond bundled text and one enum
  is not required yet.
- One parallel work item is related but not blocking: the investigation into
  why the shipped BR-6 refusal did not name the opt-in (may land a narrower
  prompt fix this REQ subsumes). BUG-165 (merged to main, `d093ede`) replaced
  the Bearer-only search auth header with a `[web] search_auth` header
  template — BR-9/AC-8 here stay as written: the template makes more backends
  *suggestible*, and each suggestion still needs its contract test.

## Open Questions

- [ ] Should concurrent *already-running* sessions (other than the one that
  completed the flow) also gain the capability live, or only sessions started
  after commit? (BR-8 currently requires the committing session + new
  sessions; extending to bystander sessions touches the BUG-161/162
  cross-session surface and may not be worth it.)
- [ ] Should the flow end by offering a consented "try one lookup now"
  (ordinary REQ-563 Ask prompt), or end silently? BR-13 permits the former;
  product taste call.
- [ ] Command spelling: `/setup web` (capability-generic namespace, matches
  the follow-up provider flow) vs `/web setup` (matches the existing
  `/web refresh` / `/web allow` family). Architecture may decide; naming is
  user-visible so flagged here.

## Out of Scope

- A guided **provider-add** flow (`/setup provider`) — the flow machinery
  this REQ builds is capability-generic so that follow-up is small, but it
  ships separately.
- Changing the REQ-563 tier/consent semantics, the search auth header
  mechanism itself (pending chip; BR-9 only constrains what may be
  *suggested*), or any relaxation of off-by-default (BR-1 of REQ-563 stands).
- The local-model first-run flow (REQ-547) — it already has its own
  consent-and-download walkthrough; this REQ's refusals may *name* it but do
  not restructure it.
- VS Code extension surface (phase 2 client work); BR-12's degradation rule
  is what phase 2 builds on.
- Auto-enabling anything, however strong the signal. No telemetry-driven
  nudges beyond the in-refusal offer.

## Retrieved Context

- LESSON-501 (lesson, score 11): State carried past its creator's lifetime sheds invariants silently
- LESSON-495 (lesson, score 10): A remembered grant answers every question its key matches — so the key must encode the whole question
- BUG-161 (bug, score 9): Permission request_ids collide across concurrent sessions, cross-authorizing tool calls
- BUG-162 (bug, score 9): model/confirm can be answered by any connection, and six sibling methods take no connection at all
- LESSON-504 (lesson, score 8): A gate's precondition is part of its security claim — check whether the adversary can mint it
- LESSON-502 (lesson, score 7): An invariant enforced at several seams needs an adversarial test at each seam
- LESSON-503 (lesson, score 7): An id must be minted at the scope that resolves it
- LESSON-505 (lesson, score 7): An audit control is judged in the adversarial case, not the honest one
- LESSON-494 (lesson, score 7): A security gate and the client that executes the request must share one parser
- BUG-152 (bug, score 7): A prompt typed while the local tier is still loading is reported as an error, not as a wait
- LESSON-510 (lesson, score 6): A harness that checked a binary exists has not checked it is the one under test
- LESSON-508 (lesson, score 6): A redundant guard needs its own test precisely because it is redundant
- LESSON-496 (lesson, score 6): "Cut first under pressure" means "never available" when the limit equals the count
- LESSON-493 (lesson, score 6): A prompt ending is only reachable if its knowledge source exists — bundle what only the product knows
- BUG-160 (bug, score 6): Asked how to hook up external models, the agent searches the user's repo — Teton's own setup instructions are not bundled
