---
id: REQ-569
title: "Session attach requires a grant: closing the same-UID ambient-attach path"
status: draft
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

- [ ] BR-1: A connection may attach only to sessions it created or holds an attach-scope grant for; `session/attach` without a grant is refused with a distinct, stable error code (informed by LESSON-484, BUG-155 — enforced at the daemon decision point every client crosses, not in any client).
- [ ] BR-2: The monitor declaration is grant-gated with its own scope. A grant that permits attaching to one session never implies monitor, and vice versa — the grant key encodes the whole question (informed by LESSON-495).
- [ ] BR-3: No grant is ambient: possessing the socket path, the daemon's spawn environment, or same-UID filesystem access must not suffice to mint or exercise a grant. The daemon never writes grant-conferring material into the environment or into files its spawned subprocesses can read (informed by LESSON-432 — the capability derives from an explicit act, not from the shape of the requester's access).
- [ ] BR-4: The daemon's own spawned tool and MCP subprocesses are verifiably excluded: the exclusion is an explicit mechanism, not a side effect of what those children currently happen to do — it must hold even when a future subprocess links the client crate (informed by LESSON-443).
- [ ] BR-5: Grant checks and consent outcomes are decided in the daemon in one place; every refusal carries a stable code distinguishing "no grant" from "consent denied" from "consent timed out", and clients render from codes, never from prose (informed by BUG-152).
- [ ] BR-6: An interactive user resuming their own session in a fresh client succeeds with at most one visible consent step, including when no other client is attached. The mechanism that lets them through must be one an ambient background process cannot silently satisfy.
- [ ] BR-7: Consent defaults closed: an unanswered consent request resolves to denied after a bounded timeout, and a denied or timed-out request leaves no partial grant state (informed by LESSON-501 — the decision travels with the grant, re-asserted at the seam that stores it).
- [ ] BR-8: New session ids are unguessable and `session/list` knowledge confers no access — ids are names, grants are credentials. Existing flows that only ever touch the creator connection are behavior-identical.
- [ ] BR-9: Every session-mutating or session-answering method is attachment-gated, not just `session/prompt`/`session/clear`/`web/override` (which REQ-568 gates). In particular `permission/respond` resolves the request's owning session and requires attachment, so a `monitor` — which sees every session's `permission_request` — cannot answer one. This depends on request ids being session-resolvable; the per-session/daemon-wide id collision that blocks it is tracked as [[BUG-161]] and must be fixed first (informed by LESSON-484 — enforce where the "this names a session" decision is made, across every writer, not just the named methods).
- [ ] BR-10: `session/list` returns the full summary (`title`, `cwd`) only for sessions the connection is attached to or holds a grant for; unattached connections receive a reduced summary (`session_id`, `mode`, `phase`). A session `title` is model-generated from the user's prompt text and `cwd` is an absolute path, so both are boundary content, not mere metadata (informed by LESSON-432 — the leak is in the payload, not only the id). Closes REQ-568's accepted residual on the `session/list` content leak.

## Acceptance Criteria

- [ ] AC-1: An e2e test spawns a process with exactly the daemon's subprocess spawn environment; its `session/attach`, monitor declaration, and `session/prompt` against another connection's session are all refused with the BR-5 codes.
- [ ] AC-2: A second legitimate client can attach to a live session through the grant flow; after the grant, REQ-568 delivery rules apply to it as an attached client.
- [ ] AC-3: The resume flow — create session, disconnect the only client, reconnect with a fresh client, attach — succeeds with at most one visible consent step, and the test demonstrates the same sequence performed by a non-interactive process is refused.
- [ ] AC-4: A monitor declaration without a monitor-scope grant is refused; an attach-scope grant for one session does not enable monitor (informed by LESSON-495).
- [ ] AC-5: Knowing a session id (via `session/list` or guessing) demonstrably does not enable attach: the test attaches by id without a grant and is refused.
- [ ] AC-6: Consent timeout resolves to denied within the bounded window, emits `attach_refused` with the timeout code, and leaves no grant state behind.
- [ ] AC-7: The single-client create → prompt → stream flow runs with zero new prompts or consent steps, and the full existing e2e suite passes.
- [ ] AC-8: Grant enforcement is asserted at the daemon seam by a test driving the RPC surface directly (not through the CLI), so no client-side check can mask a daemon-side gap (informed by BUG-155).
- [ ] AC-9: An unattached (and separately, a monitor-only) connection calling `permission/respond` against another session's pending request is refused with the BR-5 code; after attaching, the same call succeeds. Asserted at the raw RPC surface (BUG-155 pattern). Requires [[BUG-161]] fixed so the request resolves to an owning session.
- [ ] AC-10: `session/list` from an unattached connection returns no `title` and no `cwd` for sessions it is not attached to; an attached connection sees the full summary. Asserted on the wire.

## External Dependencies

- REQ-568 (session-scoped event delivery) must land first: this REQ gates the attach/monitor primitives REQ-568 introduces.
- [[BUG-161]] (permission request_id collision) must be fixed before BR-9/AC-9: `permission/respond` cannot be attachment-gated until a request id resolves to exactly one owning session.

## Assumptions

- The uid check (auth.rs) remains the outer perimeter and is unchanged; this REQ adds authorization *within* it.
- A same-UID adversary with debugger/ptrace capability over the daemon or a signed client is out of threat model — recorded as an accepted residual, consistent with ADR-008's runner-compromise posture.
- ADR-008's code-signing identity is available on macOS release builds if the architecture chooses peer-executable identity; dev builds and Linux need a stated fallback posture (see OQ-2).
- The VS Code extension (phase 2) will use the same grant flow; nothing in this REQ may assume a TTY.

## Open Questions

- [ ] OQ-1: Which grant mechanism: peer-executable identity (kernel pid + signature check), OS-keychain-held grant material (ACL-bound to executable identity, as keychain provider keys already are per REQ-544 BR-7), user-mediated consent routed to an attached client, or a combination? Decide at architecture with a stated posture per platform.
- [ ] OQ-2: What is the posture for unsigned/dev builds and Linux, where executable-identity attestation is weaker or absent? Fail-closed to consent-only, or accept a documented weaker perimeter?
- [ ] OQ-3: Do grants persist across daemon restarts (and where), or is every daemon lifetime a fresh consent? Persistence trades one prompt for a stored credential whose storage becomes attack surface.
- [ ] OQ-4: Should `session/list` redact sessions the caller holds no grant for (titles can carry boundary content), or is listing metadata acceptable while ids stop being credentials?

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
