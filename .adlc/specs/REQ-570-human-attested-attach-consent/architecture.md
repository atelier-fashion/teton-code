# REQ-570 — Architecture

Human-attested attach consent: a surface a headless process cannot satisfy, and
a client that can answer.

## 0. BR-12 first: the spike, and what it actually found

BR-12 makes this the **first** thing architecture does, because OQ-1's entire
decision rests on one claim: that an OS presence prompt — unlike the code
signatures and keychain ACLs REQ-569 ADR-A rejected — is **not inert** in a
plain `cargo run` build. Everything below is written on the far side of that
question being answered empirically rather than assumed.

**Verdict: the claim holds. The REQ proceeds as specified.**

### macOS leg — NOT INERT (the load-bearing result)

A zero-dependency Rust binary doing raw ObjC-runtime FFI into
`LocalAuthentication.framework`, built with `cargo build` and run with
`cargo run`. Its signing posture was confirmed first, because "unsigned" is the
whole variable:

```
CodeDirectory ... flags=0x20002(adhoc,linker-signed)
Signature=adhoc
TeamIdentifier=not set
```

That is exactly the posture ADR-A described as making a signature check inert:
no Developer ID, no team, nothing an OS check could distinguish. Against that
binary:

| Probe | Result |
|---|---|
| `objc_getClass("LAContext")` | OK — framework links and the class resolves |
| `canEvaluatePolicy(deviceOwnerAuthenticationWithBiometrics)` | `true` |
| `canEvaluatePolicy(deviceOwnerAuthentication)` | `true` |
| `evaluatePolicy(deviceOwnerAuthentication, …)` | **blocked for the full 6s** while the runloop was actively serviced |
| `-[LAContext invalidate]` | resolved with `LAError -9` (`LAErrorAppCancel`) |

The runloop was serviced with `CFRunLoopRunInMode` throughout, so "still
pending" cannot be a runloop-starvation artifact — a real prompt was presented
and was waiting on a human. **An inert mechanism returns an error in
milliseconds; this one blocked on a person.** That is the distinction ADR-A's
rejected mechanisms could not make, and it is why an OS presence prompt is a
different shape of control from a signature check.

Two findings fall out that the design below consumes directly:

1. **`deviceOwnerAuthentication` (policy 2) is the right policy**, as OQ-1
   selected: it was evaluable on this hardware, and it degrades to the login
   credential where biometry is absent rather than to nothing.
2. **`LAError` codes give BR-7's distinguishability for free.** Cancellation
   came back as a specific `-9` rather than a generic failure. `-2`
   (`userCancel`), `-4` (`systemCancel`), `-9` (`appCancel`) and the timeout
   arm are separable at the source, so BR-7's "failure, cancellation and
   timeout are distinguishable" is a mapping exercise, not a mechanism we have
   to invent.

### Linux leg — no agent, exactly as BR-11 predicted

Probed in a headless `debian:bookworm-slim` container, which is precisely the
environment BR-11 describes (SSH, containers, CI, a server).

| Probe | Result |
|---|---|
| `/run/dbus/system_bus_socket` | absent |
| polkit binaries | absent in the base image |
| `/usr/share/polkit-1/actions` | present, `root:root drwxr-xr-x` |
| after installing polkit + starting dbus + polkitd, `pkcheck` as **non-root** | `Authorization requires authentication but no agent is available.` (exit 2) |
| `pkexec` as non-root | `Error opening current controlling terminal for the process ('/dev/tty'): No such device or address` |

Three things this pins down:

- BR-11's system-path claim is literally true: the actions directory is
  root-owned, so a user-level install cannot register a policy file there.
- The authority being *on the bus* is **not** the same as an agent being
  reachable. polkit answered; it answered "no agent". So the code must key its
  refusal on the agent-availability condition, not on "is polkit installed",
  which would be a false positive.
- The textual-agent escape hatch requires `/dev/tty`, which headless does not
  have — **and neither does the VS Code extension**, which is the same no-TTY
  constraint that made this REQ hard in the first place. So the textual agent
  does not rescue the degraded case, and BR-11's fail-closed posture is the
  normal path on Linux rather than an edge case.

### What the spike does not claim

It did not complete a successful authentication — no human was at the machine to
touch the sensor, and the spike cancels rather than authenticating. It proves
the prompt is **presented and blocking**, which is the inert/not-inert question
BR-12 asked. Whether a *successful* evaluation returns a trustworthy `true` is
not separately provable by a local run and is carried as a residual in §7.

Spike source is not vendored into the tree: it was a throwaway built outside the
repo, deliberately, so it could not trip BUG-159 (`call_sites.rs` and
`harness/duty.rs` read production source mid-run and panic if `src/` changes).
The transcript above is the record.

---

## 1. ADR-A: BUG-162's premise does not hold for `model/confirm`, so BR-10(a) is a *standing* rule, not a raiser-identity rule

**This is the one place the design departs from the literal wording of an input
document, so it is recorded before anything is built on it.**

BUG-162 says the fix is to "restrict answering to the connection that raised the
flow", and REQ-570's Permissions table inherits that as "only the connection
that raised the request (BR-10 layer a)". Reading the code, **for
`model/confirm` there is no such connection.**

`model_consent.rs` raises the proposal from the first-run flow, which is
*spawned beside `serve`*, and its own comment says it "may publish before the
daemon accepts its first connection". The `model_selection_proposed` event is
published with `None` scope — daemon-wide — because local model selection is a
machine-wide fact. The raiser is the **daemon**, not a connection. There is no
raiser id to bind to, and inventing one (first-claim-wins) would just hand the
proposal to whichever connection races fastest, which an attacker wins as easily
as a user.

BUG-162 anticipated this in its own *Expected Behavior*, which states the real
minimum bar:

> answerable only by a connection entitled to answer it — **minimally, not by
> the daemon's own spawned children**, which REQ-569 BR-4 otherwise excludes
> from session access.

So BR-10(a) is implemented as a **standing** rule, and the standing that already
exists and is exactly right is REQ-569's ancestry gate:

> **Every daemon-wide method takes `&ConnState` and checks
> `may_hold_session_access()`.**

One rule, one predicate, applied at seven seams. Properties that make this the
right answer rather than a convenient one:

- It needs **no new mechanism**, which is what BR-10(a)'s "MUST be shippable
  without (b)" requires — it ships the moment it is written, independent of the
  attestation work.
- It closes the concrete attack BUG-162 demonstrates: a daemon-spawned tool/MCP
  child is `Ancestry::Descendant`, so it is refused. That child needed no grant
  to reach these methods before.
- It fails closed on `Ancestry::Indeterminate`, inheriting the policy REQ-569
  already argued for at `may_hold_session_access` — a guard whose failure mode
  is open is one an attacker only has to break rather than beat.
- It is the same predicate REQ-569 already uses for session access, so there is
  one definition of "may this connection act on this daemon" rather than two to
  drift apart (LESSON-484).

What it deliberately does **not** claim: it does not distinguish a user's real
CLI from a non-descendant headless same-UID process. That distinction is
unavailable to layer (a) by construction — it is precisely what the attestation
of layer (b) exists to supply. Layer (a) is a real reduction in blast radius
(the daemon's own children lose the capability entirely), not a complete answer,
and it is recorded at that strength rather than oversold.

### The seven seams, and what each gets

| Method | Today | Layer (a) | Layer (b) |
|---|---|---|---|
| `model/confirm` | `handle_model_confirm(daemon, id, params)` — no conn | ancestry gate | **yes** — commits a multi-GB download + daemon-wide model change |
| `model/set` | `handle_model_set(daemon, id, params)` — no conn | ancestry gate | **yes** — same commitment |
| `config/set` | `handle_config_set(daemon, id, params)` — no conn | ancestry gate | no (see note) |
| `config/get` | `handle_config_get(daemon, id)` — no conn | ancestry gate | no |
| `cost/query` | `handle_cost_query(daemon, id)` — no conn | ancestry gate | no |
| `web/refresh` | `handle_web_refresh(daemon, id, params)` — no conn | ancestry gate | no |
| `session/create` | takes `conn`, but is **not** ancestry-gated | ancestry gate | no |

`session/create` is the odd one: it already receives `&ConnState` and simply
never consults the gate, so a daemon descendant that BR-4 forbids from
*attaching* may still create and drive its own session on the user's provider
credits. Same fix, one line, same seam.

`config/set` keeps BUG-162's own downgrade reasoning: config lives at
`base_dir/config.toml`, which any same-UID process can already write directly,
so gating the RPC removes *immediacy*, not a capability. Worth doing as defense
in depth; not load-bearing, and not worth an attestation prompt.

---

## 2. ADR-B: policy is pure, mechanism is gated — the attestation subsystem

Follows the project's established pattern (architecture.md, REQ-564,
LESSON-499): when the interesting logic sits behind a non-default cargo feature
CI never compiles, extract the **decision** into a feature-free module over
plain data and leave the gated module holding only FFI. Otherwise the subtlest
code in the tree ships with the least coverage.

```
crates/tetond/src/attest/
  mod.rs         — the PresenceVerifier seam + AttestationMethod
  policy.rs      — FEATURE-FREE. The registry, binding, single-use, expiry,
                   refusal taxonomy. Every BR-6/BR-7 rule is decided here and
                   is table-testable with no FFI, no daemon, no socket.
  mechanism.rs   — #[cfg(feature = "presence")]. LAContext FFI and nothing else.
                   Holds no policy; it answers "did a human authenticate", and
                   maps LAError to the policy module's refusal enum.
```

The test double consumes the **same** extracted policy, never a reimplementation
— a double with its own copy of the rule tests only that two implementations
share each other's bugs (LESSON-499).

### The seam that makes AC-7 testable on any platform

```rust
pub trait PresenceVerifier: Send + Sync {
    fn availability(&self) -> MechanismAvailability;
    fn verify(&self, subject: ConnectionId, request: &RequestId) -> Result<PresenceAttestation, AttestationRefusal>;
}
```

`MechanismAvailability::Unavailable { reason }` is a **first-class value**, not
an error path — AC-7 requires the no-mechanism posture to be assertable by
injection on any platform, and AC-7b requires the Linux-no-agent case to name
*that* cause rather than a generic failure. The reason enum carries
`NoPolkitAgent` specifically, keyed on the agent-availability condition the
spike found (§0), not on "polkit absent".

### Types (System Model, made concrete)

```rust
pub enum AttestationMethod { OsBiometric, OsCredential, None }
```

`None` is a distinct recorded value that never silently passes — it exists so
`grant_minted` can report the creator path honestly (AC-9), and the Permissions
table's "mint a grant with `attested_by.method == none` → nothing" is enforced
by the registry refusing to mint on it, not by callers remembering to check.
`out_of_band_code` is deliberately absent: OQ-1 dropped it, and an unreachable
variant in a security enum invites a future reader to wire it up.

```rust
pub struct PresenceAttestation {
    method: AttestationMethod,
    verified_at: Instant,
    subject: ConnectionId,   // never transferable
    request: RequestId,      // BR-6: authorizes exactly one decision
}
```

BR-6 is enforced by the **key**, following LESSON-495 as REQ-569's grants
already do: `(subject, request)` is the whole key, so an attestation cannot
answer a question it was not minted for. Single-use is a *consuming* take from
the registry, not a boolean flag someone must remember to set — the same
`route_of`-read / `resolve`-consume split REQ-569 uses.

### Expiry — closing OQ-3

OQ-3 asks whether one attestation may cover a burst. **It may not**, and BR-6
already says so; the spec elevated the question because with an OS prompt
selected, "strictly one" means a Touch ID prompt per cross-session resume.

Resolved as: **single-use, 60-second expiry, no burst coverage.** The window
exists to bound the gap between the human touching the sensor and the grant
being minted, not to amortize prompts. A burst-covering attestation is a grant
with a time window by another name, which is the exact thing REQ-569 ADR-C
deliberately refused to persist. The UX cost is real and is accepted: the
flooding case BR-6 points at is already bounded by
`MAX_PENDING_CONSENTS_PER_CONNECTION = 3`, so the worst legitimate case is three
prompts, and the ordinary case (a user resuming one session) is one.

---

## 3. Consent, re-wired (BR-1, BR-3)

REQ-569's `ConsentRoute` is kept exactly as-is — it is pure, table-tested, and
its two arms are still the right routing. What changes is what an **answer**
must carry.

```
attach/consent { request_id, decision, attestation? }
   │
   ├─ route_of(request_id)            (read, never consumes — REQ-569's rule)
   ├─ renders_request(conn, attached) (unchanged: may this surface answer at all)
   ├─ NEW: attestation gate
   │    ├─ decision == Deny  → no attestation required. Refusing needs no
   │    │                      proof of presence; requiring one would let an
   │    │                      absent mechanism force a grant to stay pending
   │    │                      rather than be refused, which is fail-open.
   │    └─ decision == Allow → registry.consume(subject=conn, request=request_id)
   │                           must yield a live, unexpired attestation, else
   │                           refuse ATTESTATION_REQUIRED and mint nothing.
   └─ resolve(...)
```

**BR-3, the residual REQ-569 could not close.** The self-render arm
(`TheRequesterItself`) survives — the spec is explicit that refusing it would
break resume, which REQ-565/567 exist to provide — but its answer now mints
nothing without a verified attestation. A headless same-UID process reaching
that arm and answering itself gets `ATTESTATION_REQUIRED`, and the grant
registry stays empty (AC-1, asserted by inspecting the registry, not by reading
the error).

**BR-5 / monitor, re-enabled.** `GrantScope::Monitor` gets a minter again, under
two conditions rather than one:

1. a valid attestation bound to the answering connection and this request, and
2. **the approver is not the requester under any arm** — checked structurally,
   not by construction.

Condition 2 is why REQ-569 removed the path rather than re-predicating it: an
attacker holding two connections took the peer arm with two distinct
`ConnectionId`s, so it did not even read as self-approval. The attestation is
what breaks that attack — the second connection cannot produce one without a
human at the machine — and condition 2 remains as a defense-in-depth invariant
with its own regression test (AC-2), because LESSON-502 says an invariant
enforced at several seams needs a test at each seam. AC-2b asserts the positive
direction, so `monitor` is observed *working* and not only being refused; a
capability only ever seen refused is indistinguishable from the dead code Gap 3
describes.

---

## 4. The CLI can answer (BR-4, AC-3, AC-4)

Today `crates/teton` renders an incoming consent request as a notice it cannot
act on, so every consent path ends in the 30-second timeout and REQ-569's own
acceptance evidence leans on a test-harness `with_auto_consent` no shipped
client has.

The CLI gains: render the request through the existing `Surface`/`Prompter`
seams (ADR-007's rule — the future ratatui front-end inherits it by implementing
the same seams), take a decision, trigger the presence prompt, send
`attach/consent`.

**It must never auto-answer.** A non-interactive invocation does not approve —
it declines, and says why. This is asserted (AC-4), because "never auto-answers"
is a claim about the path nobody exercises. Note the deliberate asymmetry with
`teton --yes`: `--yes` is consent to *this user's own* pending action, and
carries no authority to admit a **different** connection into a session.

OQ-4 (an explicit `/attach` command) is left open and out of this REQ's build:
consent stays reactive to a daemon prompt. Adding a command is additive and does
not change any rule here.

---

## 5. Task graph

TASK-001 is done — §0 is its output. TASK-002 is deliberately unblocked by it
(BR-10's two-layer split), so the high-severity BUG-162 fix is not gated on the
spike's outcome.

```
TASK-001 BR-12 spike ......................... COMPLETE (§0)
   │
   │  (BR-10 split: 002 does NOT depend on 001)
TASK-002 BR-10(a) connection binding, 7 seams ... independent ── AC-10(a)
   │
TASK-003 attestation policy core (pure) ......... AC-5, AC-6
   ├── TASK-004 verifier seam + macOS FFI + posture ... AC-7, AC-7b
   │      └── TASK-005 consent requires attestation .... AC-1, AC-3(daemon half)
   │             ├── TASK-006 monitor re-minting ....... AC-2, AC-2b
   │             ├── TASK-007 grant_minted carries method  AC-9
   │             └── TASK-009 BR-10(b) daemon-wide commitment  AC-10(b)
   └── TASK-008 CLI answers consent ................... AC-3, AC-4
TASK-010 mutation check + AC-8 regression bar ......... AC-11, AC-8
```

Ordering rationale: TASK-002 first because it closes an open high-severity bug
and depends on nothing. Then the pure policy core (TASK-003), which is the
feature-free half everything else consumes and CI actually compiles. The FFI
(TASK-004) lands behind it so the policy is tested before the mechanism exists.

---

## 6. Error codes

New, stable, distinct — BR-7 requires failure/cancel/timeout to be
distinguishable, and AC-6 asserts each mints nothing:

| Code | Meaning |
|---|---|
| `ATTESTATION_REQUIRED` | an allow-decision arrived with no valid attestation (BR-1, BR-3) |
| `ATTESTATION_FAILED` | the human was asked and did not authenticate (`LAError -1`) |
| `ATTESTATION_CANCELLED` | the human or the app dismissed it (`LAError -2/-4/-9`) |
| `ATTESTATION_TIMEOUT` | the window elapsed with no answer |
| `ATTESTATION_UNAVAILABLE` | BR-8/BR-11 posture: no usable mechanism on this platform |
| `SELF_APPROVAL_REFUSED` | the approver is the requester on a monitor-scope request (BR-5) |

`ATTESTATION_UNAVAILABLE` carries the *named* cause (AC-7b) — the Linux
no-polkit-agent case says so specifically rather than degrading into a generic
failure.

---

## 7. Residuals, recorded honestly

- **A successful evaluation's trustworthiness is not locally provable.** §0
  proved the prompt is presented and blocks; it did not complete an
  authentication. We trust `LAContext` to return `true` only after a real
  presence check — which is the OS's contract, and is the same trust every
  Keychain-gated app extends. Recorded rather than implied.
- **Layer (a) does not distinguish a user's CLI from a non-descendant headless
  process** (§1). That is layer (b)'s job, by construction.
- **Linux cross-session attach does not work headless.** BR-11's stated posture,
  now empirically confirmed (§0). Documented, not discovered.
- **REQ-569 ADR-C's approver-propagation residual** (a grant holder can approve
  a second connection, leave, and hand the approver role on) is *narrowed* by
  attestation — the second connection still needs a human — but the chain
  itself is unchanged and stays owned here as a known shape.
- **A ptrace-capable same-UID adversary remains out of model**, and this REQ
  leans on that for nothing (the spec is explicit that REQ-569's framing of this
  was found misleading).
