---
id: REQ-569
title: "Session attach requires a grant: closing the same-UID ambient-attach path"
status: approved
deployable: true
created: 2026-08-11
updated: 2026-08-11
component: "daemon/session"
domain: "privacy"
stack: ["rust", "daemon", "json-rpc", "cli"]
concerns: ["security", "privacy"]
tags: ["session-attach", "capability-tokens", "unguessable-ids", "socket-auth", "same-uid", "monitor", "consent"]
---

## Description

Follow-up to REQ-568's OQ-1. REQ-568 makes event delivery session-scoped and
requires attachment for `session/prompt`/`session/clear`, but leaves
`session/attach` itself — and the monitor declaration — open to any
handshaked same-UID connection. Session ids are sequential and guessable
(`sess-0`), and `session/list` hands them out. So REQ-568 stops *passive*
cross-session exposure while a deliberately malicious same-UID process can
still attach (or declare monitor) and read everything. The sharp end is the
processes the daemon itself spawns: tool children and MCP server subprocesses
inherit an environment that locates the socket, run as the same uid, and are
exactly the code the harness already treats as untrusted (ADR-003 provenance
taint, ADR-009 frame containment) — yet today they hold full client rights
over every session.

This REQ raises session access from "ambient to the uid" to "granted": a
connection may attach only to sessions it created or holds a grant for,
monitor is grant-gated the same way, and the daemon's own spawned
subprocesses are verifiably unable to obtain either. The threat model is
stated honestly: a same-UID adversary with debugger/ptrace capability over
the daemon is beyond any userland perimeter and is out of scope; the
perimeter this REQ builds is against same-UID processes exercising the
*protocol* — which is the entire attack surface the daemon's own children
have.

The hard constraint shaping the design: the everyday resume flow (user quits
the CLI, daemon holds the session per REQ-565/567, user reopens a CLI and
attaches) must keep working when no already-attached client exists to
mediate consent — while an ambient background process must not be able to
walk through the same door silently. Candidate mechanisms (peer-executable
identity via the kernel's pid-behind-the-socket plus the ADR-008 signing
identity; OS-keychain-held grants whose ACLs bind to executable identity as
REQ-544 BR-7 already relies on; consent rendered at a user-owned surface)
are the architecture phase's decision, not this spec's.

## System Model

### Entities

| Entity | Field | Type | Constraints |
|--------|-------|------|-------------|
| SessionGrant | session_id | SessionId | required; the session the grant opens |
| SessionGrant | subject | grant subject (connection or durable principal, per chosen mechanism) | required; never "any same-UID process" |
| SessionGrant | scope | enum: attach, monitor | required; monitor is a distinct, broader scope |
| Session | id | SessionId | non-enumerable/unguessable for new sessions (defense in depth; id knowledge must not itself confer access) |
| ConsentDecision | outcome | enum: granted, denied, timeout | timeout resolves to denied |

### Events

| Event | Trigger | Payload |
|-------|---------|---------|
| attach_consent_requested | a connection without a grant requests attach/monitor | session_id, requester description, scope |
| attach_refused | attach/monitor denied (no grant, denied consent, timeout) | session_id (if any), scope, stable reason code |

### Permissions

| Action | Roles Allowed |
|--------|---------------|
| session/attach (session the connection created) | creator connection (unchanged) |
| session/attach (any other session) | holder of an attach-scope grant for that session |
| declare monitor | holder of a monitor-scope grant |
| mint a grant | the user, through an explicit, user-visible act — never derivable from environment or filesystem access alone |
| session/list (ids + mode + phase) | any handshaked connection (listing of the id namespace remains open; ids stop being credentials) |
| session/list (title + cwd — prompt-derived / path content) | connection holding an attach/monitor grant for that session; reduced summary otherwise |
| permission/respond, web/override, and any other method that mutates or answers on behalf of a session | connection attached to the target session (resolved from the request's owning session) |

## Business Rules

- [x] BR-1: A connection may attach only to sessions it created or holds an attach-scope grant for; `session/attach` without a grant is refused with a distinct, stable error code (informed by LESSON-484, BUG-155 — enforced at the daemon decision point every client crosses, not in any client). — `GrantRegistry::may_attach` is the single definition, called from `handle_session_attach` below `dispatch`; refusals are `CONSENT_DENIED`/`CONSENT_TIMEOUT` (an ungranted attach raises a prompt rather than answering `NOT_GRANTED`). Pinned by `attach_authorization::a_session_id_read_from_session_list_is_not_standing_to_attach` and `multi_client::knowing_a_session_id_does_not_let_another_connection_attach`.
- [x] BR-2: The monitor declaration is grant-gated with its own scope. A grant that permits attaching to one session never implies monitor, and vice versa — the grant key encodes the whole question (informed by LESSON-495). — `Grant`'s key carries the scope and `may_monitor` reads only it; pinned by `grants::tests::attach_and_monitor_are_answered_by_their_own_scope_and_no_other`, `multi_client::a_monitor_declaration_is_refused_without_a_monitor_scope_grant`, and `multi_client::a_monitor_consent_granted_by_an_attached_client_produces_a_working_monitor` (which also shows the monitor's *own* attach still has to ask).
- [~] BR-3: No grant is ambient: possessing the socket path, the daemon's spawn environment, or same-UID filesystem access must not suffice to mint or exercise a grant. The daemon never writes grant-conferring material into the environment or into files its spawned subprocesses can read (informed by LESSON-432 — the capability derives from an explicit act, not from the shape of the requester's access). — **Partially discharged, and the gap is named.** Nothing grant-conferring is written anywhere: grants are in-memory, keyed to a live connection, minted only by an `attach/consent` decision, and never persisted (ADR-C, `consent.rs`). For the population BR-4 names — the daemon's own children — possession of the socket path demonstrably confers nothing (`attach_authorization::a_client_driven_from_a_genuine_daemon_descendant_is_refused_at_every_door`). **But** for an arbitrary same-UID process that is *not* a daemon descendant, possessing the socket path is enough: when no client is attached to the target session, BR-6's second arm renders the prompt at the requester, which a headless process answers itself. That is the ADR-A residual stated out loud; it is no longer silent (see BR-6) but it is not closed.
- [x] BR-4: The daemon's own spawned tool and MCP subprocesses are verifiably excluded: the exclusion is an explicit mechanism, not a side effect of what those children currently happen to do — it must hold even when a future subprocess links the client crate (informed by LESSON-443). — kernel-attested peer pid + parent-chain walk (`peer.rs`, `DaemonProcess::ancestry_of`), taken once at the handshake and terminal at `session/attach`, the `monitor` declaration, and `attach/consent`. Pinned end to end by `attach_authorization::a_client_driven_from_a_genuine_daemon_descendant_is_refused_at_every_door`, whose client runs in a process the daemon actually spawned (the probe asserts the daemon's pid is in its own parent chain), and at the seam by `multi_client::a_connection_from_the_daemons_own_process_tree_is_refused_attach_and_monitor`.
- [x] BR-5: Grant checks and consent outcomes are decided in the daemon in one place; every refusal carries a stable code distinguishing "no grant" from "consent denied" from "consent timed out", and clients render from codes, never from prose (informed by BUG-152). — four distinct doc-commented codes (`ATTACH_FORBIDDEN`, `NOT_GRANTED`, `CONSENT_DENIED`, `CONSENT_TIMEOUT`), one decision site per question (`grants::may_attach`/`may_monitor`, `ConsentRoute`, `seek_consent`). Asserted at the wire throughout `attach_authorization` and `multi_client`. *Daemon-side only:* no shipped client renders these codes yet, because nothing in `crates/teton` calls `session/attach` — see BR-6.
- [ ] BR-6: An interactive user resuming their own session in a fresh client succeeds with at most one visible consent step, including when no other client is attached. The mechanism that lets them through must be one an ambient background process cannot silently satisfy. — **Deliberately not ticked; two halves, one done.** The daemon half is complete: a resume with nobody attached costs exactly one prompt and one answer, end to end, pinned by `attach_authorization::a_consent_the_requester_granted_itself_is_named_as_such_in_the_daemon_log` (leg 2) and `e2e::conversation_carry::client_bs_prompt_carries_the_conversation_client_a_left_behind`. **Outstanding:** (a) *the client surface* — nothing in `crates/teton` calls `session/attach`, and the CLI renders `attach_consent_requested` as a notice it cannot answer, so no shipped user can walk this flow; (b) *the second sentence* — the self-render arm **is** one an ambient background process can silently satisfy, since the daemon cannot tell a headless same-UID requester from an interactive one. That residual is now announced in the daemon log at the moment it happens (`server::self_approval_line`, pinned by the test above and by `server::tests::a_self_approved_consent_is_named_as_such_and_cannot_forge_a_log_line`), which makes it observable — not closed.
- [x] BR-7: Consent defaults closed: an unanswered consent request resolves to denied after a bounded timeout, and a denied or timed-out request leaves no partial grant state (informed by LESSON-501 — the decision travels with the grant, re-asserted at the seam that stores it). — `PendingConsents::await_decision` folds all three endings in one place and `forget`s the timed-out id; only the `Granted` arm touches the registry. Pinned by `consent::tests::every_ending_leaves_no_residual_entry`, `server::tests::a_denied_or_timed_out_consent_leaves_the_grant_registry_empty` (grant registry inspected, not inferred), and at the wire by the "nothing was left behind" leg of `attach_authorization::a_session_id_read_from_session_list_is_not_standing_to_attach`.
- [x] BR-8: New session ids are unguessable and `session/list` knowledge confers no access — ids are names, grants are credentials. Existing flows that only ever touch the creator connection are behavior-identical. — 128-bit random ids (TASK-104); the grant check precedes `sessions.get`, so a real id and a fabricated one draw the same refusal (no existence oracle). Pinned by `multi_client::knowing_a_session_id_does_not_let_another_connection_attach` and, for the behaviour-identical half, `attach_authorization::the_single_client_create_prompt_stream_flow_asks_for_nothing_new`.
- [~] BR-9: Every session-mutating or session-answering method is attachment-gated, not just `session/prompt`/`session/clear`/`web/override` (which REQ-568 gates). In particular `permission/respond` resolves the request's owning session and requires attachment, so a `monitor` — which sees every session's `permission_request` — cannot answer one. This depends on request ids being session-resolvable; the per-session/daemon-wide id collision that blocks it is tracked as [[BUG-161]] and must be fixed first (informed by LESSON-484 — enforce where the "this names a session" decision is made, across every writer, not just the named methods). — **Partially discharged.** The named half is done: [[BUG-161]] is fixed, `PendingPermissions::owner_of` resolves the owning session, and `permission/respond` requires `may_drive` (not `may_receive`, so a monitor is refused) without consuming the waiter — pinned by `multi_client::an_unattached_connection_cannot_answer_another_sessions_permission_prompt` and `server::tests::a_monitor_may_see_a_permission_prompt_and_may_not_answer_it`. **Outstanding:** TASK-107's dispatch audit found six methods that take no connection at all and affect or expose every session daemon-wide — `config/set`, `model/set`, `model/confirm`, `web/refresh`, `config/get`, `cost/query` — of which `model/confirm` is this same defect one scope up. Filed as [[BUG-162]] and reported rather than fixed here; `session/create` is likewise not ancestry-gated (a daemon child may hold a session of its own, by design).
- [x] BR-10: `session/list` returns the full summary (`title`, `cwd`) only for sessions the connection is attached to or holds a grant for; unattached connections receive a reduced summary (`session_id`, `mode`, `phase`). A session `title` is model-generated from the user's prompt text and `cwd` is an absolute path, so both are boundary content, not mere metadata (informed by LESSON-432 — the leak is in the payload, not only the id). Closes REQ-568's accepted residual on the `session/list` content leak. — `reduce_for` + `may_receive`, values omitted rather than emptied; pinned on the raw wire text by `multi_client::session_list_omits_title_and_cwd_from_unattached_connections`.

## Acceptance Criteria

- [x] AC-1: An e2e test drives a client connection **from a process that is a descendant of the daemon** (the shape a tool/MCP child actually has — amended at architecture 2026-08-11: the original wording said "with the daemon's spawn environment", but ADR-A keys the gate on kernel-attested ancestry, not environment, so an env-only fixture would assert nothing). Its `session/attach`, monitor declaration, and `session/prompt` against another connection's session are all refused with the BR-5 codes, and no consent prompt is offered for any of them. — `attach_authorization::a_client_driven_from_a_genuine_daemon_descendant_is_refused_at_every_door`. A **genuine** descendant, not an injected verdict: the daemon runs a `shell` tool whose command re-executes the test binary as a probe (`tetond` → `sh` → probe), and the probe reports its own parent chain, which the test asserts contains the daemon's pid. Refusals: `ATTACH_FORBIDDEN`, `ATTACH_FORBIDDEN`, `NOT_ATTACHED`. The "no prompt was offered" negative is read off the attached owner's stream — the surface all three would have been rendered at — and bounded by a positive control on the same daemon and the same connection: an ordinary same-UID client then asks and the prompt does appear.
- [x] AC-2: A second legitimate client can attach to a live session through the grant flow; after the grant, REQ-568 delivery rules apply to it as an attached client. — the attach half by `e2e::conversation_carry::client_bs_prompt_carries_the_conversation_client_a_left_behind` (REQ-567 AC-9, un-`#[ignore]`d by TASK-108 and passing *through* the consent flow) and the control leg of `attach_authorization::a_session_id_read_from_session_list_is_not_standing_to_attach`; the delivery half by `attach_authorization::a_client_that_attached_through_the_grant_flow_receives_that_sessions_events`, which decides it by envelope `seq` — the newcomer receives the session-scoped event published after its grant and never the one published before it.
- [x] AC-3: The resume flow — create session, disconnect the only client, reconnect with a fresh client, attach — succeeds with at most one visible consent step, and the test demonstrates the same sequence performed by a non-interactive process is refused. — the resume half by `attach_authorization::a_consent_the_requester_granted_itself_is_named_as_such_in_the_daemon_log` (leg 2: every attached client leaves, a fresh client attaches, exactly one `attach_consent_requested` is asserted); the refusal half by AC-1's descendant test, which performs the same sequence from a process the daemon spawned and is refused before any prompt is raised. The transition between the legs is an ordering marker (the `daemon_lifetime` frame the departing connection's guard publishes *after* its consent surface is released), not a sleep.
- [x] AC-4: A monitor declaration without a monitor-scope grant is refused; an attach-scope grant for one session does not enable monitor (informed by LESSON-495). — `multi_client::a_monitor_declaration_is_refused_without_a_monitor_scope_grant` (no approver → `NOT_GRANTED`), `grants::tests::attach_and_monitor_are_answered_by_their_own_scope_and_no_other` (scope independence in both directions), `multi_client::a_monitor_consent_granted_by_an_attached_client_produces_a_working_monitor` (the grant is reachable, and does not confer attach).
- [x] AC-5: Knowing a session id (via `session/list` or guessing) demonstrably does not enable attach: the test attaches by id without a grant and is refused. — `attach_authorization::a_session_id_read_from_session_list_is_not_standing_to_attach` at the real daemon binary (the id comes from `session/list`; the user says no; the refused peer is then shown to hold nothing) and `multi_client::knowing_a_session_id_does_not_let_another_connection_attach` at the seam (which also pins the no-existence-oracle half: a real id and a fabricated one draw the same code).
- [x] AC-6: Consent timeout resolves to denied within the bounded window, emits `attach_refused` with the timeout code, and leaves no grant state behind. — `server::tests::a_denied_or_timed_out_consent_leaves_the_grant_registry_empty` (both endings; elapsed time bounded by the injected window; the grant registry inspected afterwards rather than inferred from the error code; exactly one `attach_refused` carrying `reason: consent_timeout` reaches the requester) and `consent::tests::every_ending_leaves_no_residual_entry`. Not re-driven at the wire in `attach_authorization`: the shipped window is 30 s and is a fixture knob on an in-process `Daemon`, not an env seam on the spawned binary, so an e2e leg would buy a slower copy of the same assertion — `multi_client::knowing_a_session_id_does_not_let_another_connection_attach` already asserts `CONSENT_TIMEOUT` on a real socket under a shortened window.
- [x] AC-7: The single-client create → prompt → stream flow runs with zero new prompts or consent steps, and the full existing e2e suite passes. — `attach_authorization::the_single_client_create_prompt_stream_flow_asks_for_nothing_new`: the turn completes and streams (asserted on the streamed text, so "nothing was asked" cannot be true of a flow that never ran), and `attach_consent_requested`, `attach_refused` and `permission_request` are each asserted empty. Bounded by a control in the same test — a second client then attaches to that same session and the prompt appears at once on the same connection, so the empty lists are not an event this daemon cannot publish. Full workspace suite green.
- [x] AC-8: Grant enforcement is asserted at the daemon seam by a test driving the RPC surface directly (not through the CLI), so no client-side check can mask a daemon-side gap (informed by BUG-155). — structural: every test in `crates/tetond/tests/attach_authorization.rs` speaks NDJSON JSON-RPC to the daemon's socket, and `multi_client.rs` does the same at the seam. No CLI is involved in any refusal asserted for this REQ.
- [x] AC-9: An unattached (and separately, a monitor-only) connection calling `permission/respond` against another session's pending request is refused with the BR-5 code; after attaching, the same call succeeds. Asserted at the raw RPC surface (BUG-155 pattern). Requires [[BUG-161]] fixed so the request resolves to an owning session. — `multi_client::an_unattached_connection_cannot_answer_another_sessions_permission_prompt` (raw wire; the waiter stays pending and the rightful answer still resolves it) and `server::tests::a_monitor_may_see_a_permission_prompt_and_may_not_answer_it` for the monitor case, which is asserted one layer down because it also asserts the thing a socket test cannot: that the refused connection *did* receive the prompt (TASK-107's recorded deviation).
- [x] AC-10: `session/list` from an unattached connection returns no `title` and no `cwd` for sessions it is not attached to; an attached connection sees the full summary. Asserted on the wire. — `multi_client::session_list_omits_title_and_cwd_from_unattached_connections`, asserted on the raw NDJSON text so the claim is that the characters never left the daemon, not merely that a parsed key was absent (TASK-105).

## Residuals at close (recorded by TASK-109, 2026-08-11)

Three things this REQ does **not** close, stated here rather than left to be
discovered:

1. **The client surface for BR-6 is outstanding.** The daemon side of the
   attach/consent flow is complete and tested end to end, but nothing in
   `crates/teton` calls `session/attach` and the CLI renders
   `attach_consent_requested` as a notice it cannot answer — so no shipped user
   can walk the resume flow yet. No shipped flow *regressed* (nothing called
   `session/attach` before either), but BR-6 is a user-facing rule and it is
   not user-reachable.
2. **The self-approval arm (BR-3/BR-6's second sentence).** When no client is
   attached to the target session, the requesting connection renders its own
   consent prompt. For a headless same-UID process that is not a daemon
   descendant, that means approving itself with no human involved. The daemon
   cannot tell the two apart, so this is accepted — and, since TASK-109, it is
   **announced**: the daemon logs a distinct sentence naming the grant as
   self-approved at the moment it is minted (`server::self_approval_line`).
   Closing it needs a mechanism outside this REQ's perimeter (a user-owned
   surface the daemon can address, or the runtime signature verification ADR-A
   defers as future hardening).
3. **Daemon-wide methods (BR-9).** `permission/respond` is gated;
   `model/confirm` and five sibling methods that take no connection at all are
   not. Filed as [[BUG-162]] and deliberately not fixed here.

## External Dependencies

- REQ-568 (session-scoped event delivery) must land first: this REQ gates the attach/monitor primitives REQ-568 introduces.
- [[BUG-161]] (permission request_id collision) must be fixed before BR-9/AC-9: `permission/respond` cannot be attachment-gated until a request id resolves to exactly one owning session.

## Assumptions

- The uid check (auth.rs) remains the outer perimeter and is unchanged; this REQ adds authorization *within* it.
- A same-UID adversary with debugger/ptrace capability over the daemon or a signed client is out of threat model — recorded as an accepted residual, consistent with ADR-008's runner-compromise posture.
- ADR-008's code-signing identity is available on macOS release builds if the architecture chooses peer-executable identity; dev builds and Linux need a stated fallback posture (see OQ-2).
- The VS Code extension (phase 2) will use the same grant flow; nothing in this REQ may assume a TTY.

## Open Questions

- [x] OQ-1: RESOLVED at architecture (ADR-A) — process ancestry, plus user-mediated consent for everything ancestry does not exclude. Runtime code-signature verification and keychain-held grants were both rejected on evidence (no runtime signing machinery exists, and dev builds are unsigned, so either guard would be inert exactly where it is developed). (Original question: which grant mechanism?)
- [x] OQ-2: RESOLVED at architecture (ADR-B) — the question dissolves rather than being answered with a compromise: ancestry needs only the peer pid, so dev and release builds get identical enforcement, and macOS and Linux are both first class (`LOCAL_PEERPID`/`SO_PEERCRED`, with a platform-free policy layer over a `pid -> ppid` lookup). (Original question: what is the posture for unsigned/dev builds and Linux?)
- [x] OQ-3: RESOLVED at architecture (ADR-C) — no. A grant lives in the daemon's memory for the life of the connection that holds it and is never written to disk or keychain: persistence would buy one avoided prompt and cost a stored credential, which is a bad trade for a control whose whole point is that possession of local state must not confer access (BR-3). (Original question: do grants persist across daemon restarts?)
- [x] OQ-4: RESOLVED 2026-08-11 — answered by BR-10 during the REQ-568 verify pass: `session/list` keeps the id namespace open (ids stop being credentials) but reduces the *payload* — `title` and `cwd` are served only to a connection attached to (or holding a grant for) that session, because a title is model-generated from the user's prompt text and `cwd` is an absolute path. Both are boundary content, not metadata. (Original question: redact unheld sessions, or accept listing metadata?)

## Out of Scope

- Cross-UID access and any change to the uid/socket-permission perimeter (auth.rs).
- Defending against a same-UID adversary with debugger/ptrace capability — out of userland reach, recorded as residual.
- Network transport, remote clients, and the ACP compatibility shim.
- Revoking or expiring grants mid-session beyond daemon-lifetime semantics (revocation UX is future work unless OQ-3 pulls it in).
- REQ-568's delivery filtering, fence semantics, and frame cap — landed there, only consumed here.

## Retrieved Context

- LESSON-501 (lesson, score 9): State carried past its creator's lifetime sheds invariants silently
- LESSON-494 (lesson, score 9): A security gate and the client that executes the request must share one parser
- LESSON-432 (lesson, score 8): Provenance must derive from what a tool touches, not from an argument name
- LESSON-495 (lesson, score 6): A remembered grant answers every question its key matches — so the key must encode the whole question
- LESSON-490 (lesson, score 6): A guard that runs on an encoded form is tested against the encoder's output
- LESSON-492 (lesson, score 6): A composite guard's failure path must not discard evidence a completed pass established
- LESSON-497 (lesson, score 5): A test fixture that looks like a real credential blocks the push that ships it
- BUG-152 (bug, score 4): A prompt typed while the local tier is still loading is reported as an error, not as a wait
- LESSON-443 (lesson, score 4): A guard keyed on a feature's absence disables itself when the feature lands
- LESSON-445 (lesson, score 4): Side effects of a minutes-long operation must be staged, then committed only after re-checking authority
- LESSON-484 (lesson, score 3): Enforce a rule where the decision is made, not where it was convenient to write
- BUG-155 (bug, score 3): REQ-557's deleted provider-id fallback was only relocated, and three other defects it shipped
- REQ-557 (spec, score 3): Provider model identity and an explicit default provider
- LESSON-479 (lesson, score 3): A subset invariant is only tested in the direction your loop iterates — write the equation down, then check which half you wrote
- BUG-151 (bug, score 3): The frame-marker coverage invariant only holds in one direction
