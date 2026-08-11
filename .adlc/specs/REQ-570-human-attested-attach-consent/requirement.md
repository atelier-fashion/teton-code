---
id: REQ-570
title: "Human-attested attach consent: a surface a headless process cannot satisfy, and a client that can answer"
status: in-progress
deployable: true
created: 2026-08-11
updated: 2026-08-11
component: "daemon/session"
domain: "privacy"
stack: ["rust", "daemon", "json-rpc", "cli"]
concerns: ["security", "privacy", "developer-experience"]
tags: ["consent", "attach", "out-of-band", "user-surface", "human-presence", "touch-id", "polkit", "grant", "monitor"]
---

## Description

REQ-569 built the attach-authorization perimeter — ancestry exclusion, a grant
registry, and a consent flow — and shipped with **three** named gaps it could
not close with the primitives available. This REQ closes all three, and takes in
a fourth defect of the same shape found after it (BUG-162, pulled in by OQ-5).
They are one REQ because they are one problem seen from several sides: **the
daemon has no way to tell a human's answer from a program's, and no shipped
client can give it one.**

**Gap 1 — the self-approval residual (REQ-569 BR-3, BR-6).** When no connection
is attached to the target session, REQ-569 routes the consent prompt back to the
*requesting* connection. For the real resume flow (a user reopens their CLI)
that is correct: the user answers. For a headless same-UID process it means the
"consent" is self-issued with no human involved. REQ-569 accepted this because
refusing instead would break resume — sessions outliving their clients is what
REQ-565/567 exist to provide — and because the alternatives available at the
time (code signatures, keychain ACLs) were inert in dev builds. The residual is
recorded and now announced via a daemon-scoped grant-minted event, but it is not
closed.

**Gap 2 — no client can answer (REQ-569 BR-6, unticked).** Nothing in
`crates/teton` sends `session/attach` or `attach/consent`. The CLI renders an
incoming consent request as a notice it cannot act on, so today every consent
path ends in the 30-second timeout, and REQ-569's own acceptance evidence for
the grant flow depends on a test-harness `with_auto_consent` capability no
shipped client has. The tested flow and the shipped flow diverge until this
lands.

**Gap 3 — `monitor` has no minter.** REQ-569's Phase-5 review found the monitor
consent path was self-serviceable (an attacker's second connection approving its
own request) and removed it. `monitor` remains grant-gated and therefore
unreachable — a REQ-568 capability that is currently dead. It becomes mintable
again only once a human-attested surface exists, because a monitor is a
whole-daemon read and nothing weaker should mint it.

**Gap 4 — the same defect one scope up (BUG-162).** `model/confirm` answers a
broadcast `model_selection_proposed` prompt by `request_id` and takes **no
connection context at all**, so any handshaked same-UID connection can commit a
multi-GB download and a daemon-wide model change on the user's behalf. Six
sibling methods take no connection either. This is structurally what REQ-569
TASK-107 closed for `permission/respond`, at daemon scope rather than session
scope — the same "who may speak for the machine" question, so it is answered
here rather than twice. Note the coupling this creates and the mitigation BR-10
carries: BUG-162 is the highest-severity bug currently open, and folding it into
a REQ gated on a spike (BR-12) would otherwise put its fix behind that spike's
outcome. BR-10's layer (a) is deliberately independent of the attestation work
for exactly that reason.

The unifying requirement: **an approval must be attributable to a human being
present at the machine, verified by something the daemon trusts and a headless
same-UID process cannot satisfy silently.** REQ-569's assumption that "nothing
may assume a TTY" (the VS Code extension is a first-class client) is what makes
this hard and is why an OS-mediated presence check, not a terminal prompt, is
**the selected mechanism** (OQ-1, closed 2026-08-11).

## System Model

### Entities

| Entity | Field | Type | Constraints |
|--------|-------|------|-------------|
| PresenceAttestation | method | enum: os_biometric, os_credential, none | `none` is a distinct recorded value, never a default that silently passes. `out_of_band_code` was dropped when OQ-1 selected an OS prompt — an unreachable variant in a security enum invites a future reader to wire it up |
| PresenceAttestation | verified_at | timestamp | required when method != none; bounds how long one attestation may authorize |
| PresenceAttestation | subject | connection id | the connection whose answer it attests; never transferable |
| SessionGrant | attested_by | PresenceAttestation | required for any grant minted outside the creator path |

### Events

| Event | Trigger | Payload |
|-------|---------|---------|
| attach_consent_requested | unchanged from REQ-569 | request_id, session_id, scope, requester |
| presence_challenge_issued | the daemon requires attestation before accepting an answer | request_id, method, deadline |
| grant_minted | a grant is recorded (carried over from REQ-569's visibility fix) | scope, requester, attestation method, self_approved flag |

### Permissions

| Action | Roles Allowed |
|--------|---------------|
| answer `attach/consent` for an attach-scope request | a connection presenting a valid, unexpired presence attestation bound to itself |
| answer `attach/consent` for a monitor-scope request | same, and the answering connection must not be the requester under any routing arm |
| mint a grant with `attested_by.method == none` | nothing — only the session's creator path, which mints no grant at all |
| answer a broadcast daemon-wide prompt (`model/confirm` and siblings) | only the connection that raised the request (BR-10 layer a) |
| commit a daemon-wide change (model switch, multi-GB download) | same, and additionally presenting a valid presence attestation (BR-10 layer b) |

## Business Rules

- [ ] BR-1: A grant is minted only when the answering connection presents a presence attestation the daemon verified; an answer without one is refused with a distinct, stable code. The creator path (a connection attaching to a session it made) is unchanged and requires no attestation (informed by REQ-569 BR-1).
- [ ] BR-2: The attestation mechanism must be one a headless same-UID process cannot satisfy without a human acting at the machine. A mechanism whose success depends only on the requesting process's own cooperation does not qualify — that is the REQ-569 residual restated, and re-implementing it would be a regression dressed as a feature.
- [ ] BR-3: The self-render routing arm survives only under attestation. When no connection is attached to the target session, the requester may still be the surface that *renders* the prompt, but its answer mints nothing without a verified attestation (closes REQ-569's BR-3 residual).
- [ ] BR-4: The shipped CLI can answer a consent request — render it, take a decision, and send `attach/consent` — so REQ-569 BR-6's "at most one visible consent step" becomes true of a real client and not only of a test harness. The CLI must never auto-answer (informed by REQ-547's consent UX: the pick is shown, the user decides).
- [ ] BR-5: `monitor` becomes mintable again, and only under attestation, with the approver never being the requester under any arm. The two-connection self-approval attack REQ-569 found must be re-tested as a regression, not merely avoided by construction (informed by LESSON-502 — an invariant needs a test at each seam).
- [ ] BR-6: An attestation authorizes exactly one decision: it is bound to a connection and a request id, is single-use, and expires. It is never cached into a durable credential without a separate, explicit user act (informed by LESSON-495 — the key must encode the whole question; REQ-569 ADR-C deliberately kept grants unpersisted).
- [ ] BR-7: Attestation failure, cancellation, and timeout are distinguishable, fail closed, and leave no partial grant or attestation state (informed by LESSON-501, REQ-569 BR-7).
- [ ] BR-8: Platforms without a usable attestation mechanism degrade to a stated, fail-closed posture — cross-session attach is refused rather than silently falling back to self-approval. Which platforms those are is an architecture output, not an assumption (informed by LESSON-443 — a control must not disable itself where the mechanism is absent).
- [ ] BR-9: Every grant mint remains observable via the daemon-scoped `grant_minted` event REQ-569 added, now carrying the attestation method, so an operator can tell an attested grant from a creator-path attach.
- [ ] BR-11: **macOS is the supported attestation platform in v1; Linux states a posture rather than pretending.** polkit needs a policy file at a system path a user-level install may never write, and headless Linux — SSH, containers, CI, a server — has no polkit agent at all. Where no agent is reachable, BR-8's fail-closed refusal is the **normal** path, not an edge case: cross-session attach does not work there, the refusal names that reason, and the limitation is documented rather than discovered. A degraded platform must never quietly fall back to the REQ-569 self-approval residual this REQ exists to close.
- [ ] BR-12: **The unsigned-dev-build behaviour is verified empirically before any other task starts.** OQ-1 rests on the claim that an OS presence prompt — unlike the code signatures and keychain ACLs REQ-569 ADR-A rejected — is *not inert* in a plain `cargo run` build. That claim is load-bearing for the whole REQ, so architecture opens with a spike that runs the prompt from an unsigned binary on both platforms and records what actually happened. If it fails, the REQ is re-scoped rather than shipped on the belief. (informed by REQ-569 ADR-A, LESSON-443 — a control that cannot run where it is developed is a control nobody verifies)
- [ ] BR-10: The daemon-wide methods BUG-162 names — `model/confirm` chief among them, plus the six siblings that take no connection context at all — come under this REQ's authorization model in **two separable layers**. **(a)** Every such method takes and checks connection context, so only the connection that raised a request may answer it; this is the REQ-569 TASK-107 pattern applied one scope up and involves no attestation. **(b)** A daemon-wide *commitment* — a model change, a multi-GB download — additionally requires a presence attestation, because its blast radius is the whole machine rather than one session. **Layer (a) is a prerequisite for (b) and MUST be shippable without it**: BUG-162 is high severity and open, and gating its fix on OQ-1's unresolved mechanism would leave the hole open for the length of that decision. (informed by LESSON-503 — an id must be checked at the scope that resolves it)

## Acceptance Criteria

- [ ] AC-1: A headless same-UID process that requests attach to an unattended session and answers its own prompt is refused, and no grant is minted — the REQ-569 residual, closed. Asserted at the raw RPC surface.
- [ ] AC-2: The REQ-569 two-connection monitor attack (conn A creates a throwaway session, conn B requests monitor, A approves) is refused as a named regression test.
- [ ] AC-2b: `monitor` is **mintable again** — a connection presenting a valid attestation, answering a monitor-scope request it did not raise, receives the grant and can then monitor. Asserted positively: AC-2 proves the attack is refused, and a capability that is only ever observed being refused is indistinguishable from the dead code Gap 3 describes. (BR-5)
- [ ] AC-3: A user resuming their own session in a fresh CLI succeeds with exactly one visible consent step, end to end through the shipped client — no test-harness auto-consent anywhere in the path.
- [ ] AC-4: The CLI renders an incoming consent request, takes a decision, and sends `attach/consent`; it never answers without user input (asserted, including that a non-interactive CLI invocation does not auto-approve).
- [ ] AC-5: An attestation is single-use and expires: replaying it, or using it for a different request id or connection, is refused.
- [ ] AC-6: Attestation failure/cancel/timeout each produce a distinct code, mint nothing, and leave the grant and attestation registries empty (asserted by inspecting the registries, not inferred from the error).
- [ ] AC-7: On a platform with no usable mechanism, cross-session attach is refused with the BR-8 posture code — never self-approved. Asserted by an injected "no mechanism available" seam so it is testable on any platform.
- [ ] AC-7b: The Linux-without-a-polkit-agent case specifically reaches AC-7's refusal and names *that* cause — not a generic failure — and the refusal is reachable in CI on a headless Linux runner, which is the environment it actually describes. (BR-11)
- [ ] AC-8: The single-client create → prompt → stream flow, and the creator's own attach, run with zero new prompts or attestation steps (the REQ-569 AC-7 regression bar, re-asserted).
- [ ] AC-9: `grant_minted` carries the attestation method and is delivered to every handshaked connection.
- [ ] AC-10: `model/confirm` and **each** of its six ungated siblings refuse an answer from a connection that did not raise the request — asserted per method at the raw RPC surface, not for one representative method (LESSON-502: an invariant enforced at several seams needs a test at each seam). A daemon-wide commitment additionally refuses when no valid attestation is presented. Layer (a) of this criterion passes independently of the attestation mechanism. (BR-10)
- [ ] AC-11: Mutation check — making the attestation verifier return success unconditionally, dropping the single-use/expiry binding (BR-6), restoring the unattested self-approval routing arm (BR-3), or removing a method's connection check (BR-10a), each makes at least one test red. This REQ ships mostly *refusals*, and a refusal is verified only by proving it is load-bearing. (informed by LESSON-441)

## External Dependencies

- REQ-569 must land first: this REQ consumes its grant registry, consent flow, ancestry gate, and `grant_minted` event.
- BUG-162 is resolved **by** this REQ (BR-10), not alongside it — its Resolution section records the disposition and points here.
- **An OS presence mechanism — selected, not conditional** (OQ-1). macOS `LocalAuthentication` (`LAContext`, policy `deviceOwnerAuthentication` so biometry-absent hardware degrades to the login credential rather than to nothing); Linux polkit where an agent is reachable. This is the project's first runtime FFI dependency of this kind and it lands in a security-critical path — BR-12 gates it behind an empirical spike before any other task starts.

## Assumptions

- The uid perimeter (auth.rs) stays unchanged; this adds authorization within it.
- A ptrace-capable same-UID adversary remains out of model. Note REQ-569's Phase-5 review found the *previous* framing of this assumption misleading — the residual it excused needed no ptrace at all — so this REQ must not lean on it to excuse anything.
- The VS Code extension (phase 2) uses the same flow; nothing here may assume a TTY, which is precisely why a terminal-typed code is likely insufficient on its own.
- REQ-569's ancestry exclusion continues to carry the daemon's-own-children case; this REQ addresses the *non-descendant* same-UID process it does not cover.
- **To be verified by BR-12's spike, not assumed:** that `LAContext` presents a real prompt and returns a trustworthy result from an unsigned `cargo run` binary. The entire OQ-1 decision rests on this being true — it is why an OS prompt is not the inert-in-dev-builds shape REQ-569 ADR-A rejected. If it turns out false, the REQ's shape changes and BR-11's fail-closed posture becomes the primary path on both platforms, not just Linux.

## Open Questions

- [x] OQ-1 — **closed 2026-08-11: an OS-mediated presence prompt**, macOS-first (see BR-11, BR-12). Rejected alternatives and why:
      - *A daemon-issued out-of-band code.* Fails the no-TTY constraint that makes this REQ hard in the first place: a code needs a surface to be typed into, and the VS Code extension is a first-class client. The one constraint that rules out a terminal prompt rules out a terminal-typed code with it.
      - *A user-owned helper process.* Moves the question rather than answering it — the daemon then has to decide whether to trust the helper, which is the same "is a human behind this" problem with an extra hop and a new persistent surface.
      - *Reusing REQ-569's ancestry gate.* Already recorded as insufficient in ADR-A: breaking the chain costs one model-supplied shell word (`setsid helper`).

      **On REQ-569 ADR-A's rejection of FFI — it does not transfer, and the distinction is the whole basis of this decision.** ADR-A rejected code signatures and keychain ACLs because unsigned dev builds make those checks **inert**: they cannot distinguish anything in the environment where the code is developed. An OS presence prompt is not inert in an unsigned build — it still puts a real prompt in front of a real human. That claim is the load-bearing one, so BR-12 makes verifying it the first thing architecture does rather than an assumption anyone carries.
- [ ] OQ-2: Does the attestation bind to the *answering* connection only, or must the answering connection also prove it is the same human that owns the session? The latter is stronger and probably unavailable.
- [ ] OQ-3: What is the expiry window, and may one attestation cover a burst of requests raised together (the flooding case REQ-569 capped) or strictly one? **Elevated by OQ-1's resolution — this is now the question that decides whether the feature is pleasant or hated.** BR-6 makes an attestation single-use and expiring, so with an OS prompt selected, "strictly one" means a Touch ID prompt on every cross-session resume. Weigh against BR-6's security intent, which is real: a burst-covering attestation is a grant with a time window by another name.
- [ ] OQ-4: Should the CLI expose an explicit `/attach` command, or is consent only ever reactive to a daemon prompt? An explicit command makes the resume flow discoverable; it also gives a scripted client an obvious entry point.
- [x] OQ-5 — **closed 2026-08-11: yes, pulled in.** BUG-162's daemon-wide methods come under this REQ's model via BR-10, because they are the same question at a wider scope and answering it twice invites two answers. The coupling risk — a high-severity open bug waiting on OQ-1 — is mitigated by BR-10's two-layer split, which makes the connection-binding fix shippable on its own.

## Out of Scope

- Cross-UID access and any change to the uid/socket-permission perimeter.
- Defending against a ptrace-capable same-UID adversary.
- REQ-569's ancestry exclusion, grant registry, and consent plumbing — consumed here, not redesigned.
- Daemon-wide methods beyond the seven BUG-162 enumerates. BR-10 covers that set; auditing the whole dispatch table for the same shape is a separate piece of work. (BUG-162 itself is now **in** scope — OQ-5.)
- Durable/persisted grants surviving a daemon restart (REQ-569 ADR-C deliberately kept grants in-memory; revisiting that is its own REQ).

## Retrieved Context

- REQ-568 (spec, score 13): Session-scoped event delivery and bounded request frames
- LESSON-502 (lesson, score 10): An invariant enforced at several seams needs an adversarial test at each seam
- REQ-563 (spec, score 10): Opt-in web lookup through the egress choke point
- LESSON-503 (lesson, score 9): An id must be minted at the scope that resolves it
- LESSON-501 (lesson, score 9): State carried past its creator's lifetime sheds invariants silently
- REQ-567 (spec, score 9): Cross-prompt conversation carry in interactive sessions
- LESSON-494 (lesson, score 9): A security gate and the client that executes the request must share one parser
- LESSON-495 (lesson, score 8): A remembered grant answers every question its key matches
- REQ-562 (spec, score 8): redact: a model-based secret and PII scan inside the egress choke point
- REQ-547 (spec, score 8): First-run local model consent: show the hardware-based pick, let the user override, then install
- LESSON-432 (lesson, score 8): Provenance must derive from what a tool touches, not from an argument name
- BUG-161 (bug, score 7): Permission request_ids collide across concurrent sessions
- LESSON-497 (lesson, score 7): A test fixture that looks like a real credential blocks the push that ships it
- LESSON-490 (lesson, score 6): A guard that runs on an encoded form is tested against the encoder's output
- LESSON-492 (lesson, score 6): A composite guard's failure path must not discard evidence a completed pass established
