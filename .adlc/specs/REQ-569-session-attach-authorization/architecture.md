# REQ-569 — Architecture: Session attach requires a grant

## Approach

Three layers, applied in order at the daemon's socket seam. Layer 1 is a hard
refusal with no consent path; Layers 2–3 decide who may be *granted* access.

1. **Ancestry exclusion (BR-4).** The daemon reads the peer's kernel-attested
   PID and walks its parent chain. A connection whose process is a descendant of
   this daemon may never attach, declare monitor, or be offered consent — it is
   refused outright. This is the explicit mechanism BR-4 demands: it keys on
   *where the process came from*, not on what it happens to do, so it still holds
   when a future MCP server links the client crate (LESSON-443).
2. **Grant required for cross-session attach (BR-1/BR-2).** A connection may
   attach only to sessions it created or holds an attach-scope grant for; monitor
   needs its own distinct monitor-scope grant. Grants are minted only by an
   explicit user consent decision — never derived from environment, socket path,
   or filesystem access (BR-3).
3. **Consent routing (BR-6/BR-7).** If any connection is already attached to the
   target session, the consent request is routed *there* — the strongest path,
   because an existing attachee is a surface the user already owns. When no
   client is attached (the resume flow), the requesting client renders the
   consent itself; this is sound only because Layer 1 has already excluded the
   daemon's own children. Consent defaults closed on timeout.

Plus two payload/method gates the spec absorbed from REQ-568's verify pass
(BR-9/BR-10) and unguessable ids as defense in depth (BR-8).

## Key decisions

### ADR-A: Process ancestry is the primary mechanism — not code signatures, not keychain ACLs (resolves OQ-1)

Two candidate mechanisms the spec named are **rejected**, on evidence:

- **Runtime code-signature verification.** The repo has *no* runtime signing
  machinery — no Security.framework, no `SecCode`, no signature introspection
  anywhere. Signing exists only in `.github/workflows/release.yml` at package
  time. Building this would add unsafe FFI against a system framework, and — the
  disqualifier — **dev builds are unsigned**, so the guard would be untestable
  in CI and inert for every developer. A security control that cannot run in the
  environment where it is developed is a control nobody verifies (LESSON-443's
  shape: a guard that disables itself).
- **Keychain-held grants ACL'd to the client executable.** Attractive because
  REQ-544 BR-7 already uses keychain ACLs, but it inherits the same
  unsigned-dev-build problem (ADR-007: every rebuild changes executable identity
  and re-prompts), has no Linux equivalent, and makes a stored credential the
  boundary — new persistent attack surface for a guarantee ancestry gives for
  free.

**Ancestry** needs only the peer PID, works identically in dev and release
builds, is symmetric across macOS and Linux, and directly expresses BR-4's
actual claim ("the daemon's own children are excluded").

**Honest limits, recorded not hidden:**
- A daemon-spawned child that double-forks and reparents to `launchd`/`init`
  breaks the ancestry chain. No heuristic is added to chase this (a "reparented
  to init" rule would also catch a legitimate CLI whose terminal closed —
  precisely the incidental-property guard LESSON-443 warns against).
- PID reuse is a narrow race: the peer could exit between `connect` and the
  walk. The credential is a property of the connected socket, so the window is
  small, but it is not zero.
- An arbitrary same-UID process that is *not* a daemon descendant can still
  puppet a legitimate CLI. That is outside the sharp end this REQ names and is
  not defeatable in userland — the same class as the accepted ptrace residual.

Runtime signature verification remains available as **future hardening** that
would raise Layer 3's assurance; it is deliberately not in this REQ.

### ADR-B: Peer PID is obtained per platform, and the platform split is CI-verified (resolves OQ-2)

`auth.rs` currently extracts uid only. Linux's `SO_PEERCRED` **already carries
the pid** — it is simply discarded today. macOS needs
`getsockopt(SOL_LOCAL, LOCAL_PEERPID)`, and the parent-of walk uses
`sysctl(KERN_PROC_PID)` on macOS and `/proc/<pid>/status` on Linux.

Because the mechanism does not depend on signing, **dev and release builds get
identical enforcement** — OQ-2's "weaker posture for unsigned builds" question
dissolves rather than being answered with a compromise. Both platforms are
first-class.

LESSON-433 governs the verification: cfg-gated per-platform code verified on one
platform is false confidence. The peer-identity module must therefore keep its
platform-specific syscalls behind a thin trait with a **platform-free policy
layer** over plain data (the ancestry decision is pure over a `pid -> ppid`
lookup), so the decision is table-testable on every platform, and CI runs the
suite on both macOS and Linux runners (both already exist in the workflow).

### ADR-C: Grants are daemon-lifetime and in-memory (resolves OQ-3)

A grant lives in the daemon's memory for the life of the daemon and is keyed by
`(subject, session, scope)`. Nothing is persisted to disk or keychain.

Rationale: persistence buys one avoided prompt and costs a stored credential
whose storage becomes attack surface — a bad trade for a control whose entire
purpose is that possession of local state must not confer access (BR-3). Because
the daemon outlives clients (REQ-565/567), the *session* survives a client
restart anyway; only the grant is re-established, which is exactly the one
consent step BR-6 budgets for.

### ADR-D: The grant subject is the connection, and scope is graded

`SessionGrant { subject: ConnectionId, session_id, scope }` where scope is
`Attach | Monitor`. Monitor is a strictly separate scope: an attach grant for
one session never implies monitor, and a monitor grant never implies drive
rights (LESSON-495 — the key encodes the whole question; REQ-568 already split
`may_receive` from `may_drive` for exactly this reason). Grants die with the
connection.

### ADR-E: Consent reuses the permission-request *shape*, not its plumbing

The consent flow mirrors the proven `PendingPermissions` pattern — publish an
event carrying a request id, await a `oneshot`, resolve on an RPC reply, default
closed on timeout/drop — but gets its **own** registry and RPC method
(`attach/consent`), not a reuse of `PendingPermissions`. Reasons: the permission
registry is session-scoped by construction and an attach request by definition
has no attachment yet; and overloading `permission/respond` would put an
ungated method in the consent path while BR-9 is busy gating it. Request ids are
minted daemon-wide from a single counter — the BUG-161 shape is not to be
reintroduced (LESSON-503: mint at the scope that resolves).

### ADR-F: `permission/respond` resolves its owning session, and gating lives there (BR-9)

BUG-161's fix made request ids daemon-unique; this REQ adds the *owning session*
alongside the waiter so `permission/respond` can resolve "which session is this
answer for" and require attachment. A `monitor` — which sees every session's
`permission_request` — therefore cannot answer one. Enforced in the handler,
below `dispatch`, so the raw-RPC tests exercise the real gate (LESSON-484,
BUG-155).

### ADR-G: `session/list` returns a reduced summary to unattached connections (BR-10)

`SessionSummary` splits presentation: ids/mode/phase always; `title` and `cwd`
only for sessions the connection is attached to or holds a grant for. A title is
model-generated from the user's prompt text and `cwd` is an absolute path — both
are boundary content, not metadata (LESSON-432: the leak is in the payload, not
only the id). The wire type gains no new field; the *values* are omitted, which
is already expressible (`Option`, `skip_serializing_if`).

### ADR-H: Unguessable session ids are defense in depth only (BR-8)

Ids become random (128-bit, base32) rather than `sess-{n}`. This is explicitly
**not** the access control — BR-8 states ids are names and grants are
credentials — so no code may key a decision on id unguessability. Its value is
narrowing the blind-guess surface and removing the enumeration oracle
`session/list` otherwise hands out. Test fixtures that hardcode `sess-0` must
move to captured ids.

## Task graph

```
Tier 1 (parallel, disjoint):
  TASK-103 peer identity: PID + ancestry, policy pure & table-tested   [tetond auth.rs + new peer module]
  TASK-104 unguessable session ids (BR-8)                              [tetond sessions.rs + fixtures]
  TASK-105 session/list reduced summary (BR-10, AC-10)                 [tetond server.rs + protocol]
Tier 2:
  TASK-106 grant model + ancestry hard gate on attach/monitor (BR-1/2/4)  deps: 103
  TASK-107 permission/respond owning-session gate (BR-9, AC-9)            deps: 105-independent; needs BUG-161 (landed)
Tier 3:
  TASK-108 consent flow: event, attach/consent RPC, timeout (BR-6/7, AC-3/6)  deps: 106
Tier 4:
  TASK-109 e2e: spawned-child refusal, grant flow, resume (AC-1..AC-8)       deps: 107, 108
```

Tier-1 tasks touch disjoint files; 106/108 both touch the grant seam and are
sequenced. TASK-109 is the cross-cutting acceptance evidence, including the
AC-1 test that spawns a process **as a daemon descendant** (not merely with the
daemon's environment — ancestry, not env, is what the gate keys on).
