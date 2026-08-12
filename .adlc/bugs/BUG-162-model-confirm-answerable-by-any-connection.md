---
id: BUG-162
title: "model/confirm can be answered by any connection, and six sibling methods take no connection at all"
status: resolved
severity: high
created: 2026-08-11
updated: 2026-08-11
component: "daemon/session"
domain: "harness"
stack: ["rust", "daemon", "json-rpc"]
concerns: ["security"]
tags: ["authorization", "request-id", "broadcast-prompt", "daemon-wide", "dispatch-audit", "REQ-569", "REQ-570"]
---

## Description

`model/confirm` answers a broadcast `model_selection_proposed` prompt by
`request_id` and takes **no connection context at all**, so any handshaked
same-UID connection can answer a proposal it did not raise — committing a
multi-GB model download and a daemon-wide model change on the user's behalf.

This is structurally the **same defect REQ-569 TASK-107 just closed** for
`permission/respond`, one scope up: a broadcast prompt, a `request_id`, and no
check on who answers. It was found by the dispatch-table audit TASK-107 was
asked to run after landing its own fix, not by a test failure.

## Reproduction Steps

1. A same-UID process (including a daemon-spawned tool/MCP child, which needs
   no grant to reach this method) connects and handshakes.
2. The daemon publishes `model_selection_proposed` — it is `None`-scoped
   (daemon-wide), so every connection receives it with its `request_id`.
3. That process sends `model/confirm { request_id, outcome: <accept> }`.
4. The proposal resolves as though the user answered it.

## Expected Behavior

A broadcast prompt is answerable only by a connection entitled to answer it —
minimally, not by the daemon's own spawned children, which REQ-569 BR-4
otherwise excludes from session access.

## Actual Behavior

Any handshaked connection resolves it. `handle_model_confirm` takes no
`&ConnState`.

## Environment

- Platform: all
- Version: `main` @ REQ-569 branch (pre-existing; REQ-569 neither introduced nor
  fixed it)

## Root Cause

The same namespace/authority mismatch as BUG-161 and as `permission/respond`
before TASK-107: a `request_id` is minted in one scope and resolved in a wider
one with no check on the resolver's standing. `model/confirm`'s prompt is
daemon-scoped by design (local model selection is a machine-wide fact), so
unlike `permission/respond` there is no owning *session* to resolve against —
the authorization question is "may this connection speak for the machine?",
which the daemon currently has no notion of.

## Resolution

**Scoped into REQ-570 (2026-08-11, OQ-5).** The design decision this bug needed
is the same one REQ-570 exists to make — "who may speak for the machine" — so it
is answered once, there, rather than twice. REQ-570 BR-10 carries it in two
separable layers:

- **(a) Connection binding** — every one of these methods takes and checks
  connection context. Needs no new mechanism, and **ships independently of
  REQ-570's attestation work** — deliberately, so this high-severity defect is
  not gated on REQ-570 OQ-1's unresolved mechanism choice.

  **Correction (2026-08-11, as built).** This section previously said layer (a)
  would "restrict answering to the connection that raised the flow". **That is
  not implementable for this bug's own headline method, and was not built.**
  `model_selection_proposed` is raised by the daemon's first-run flow — spawned
  beside `serve`, and by its own comment able to publish *before the daemon
  accepts its first connection* — and it is published `None`-scoped because
  local model selection is a machine-wide fact. There is no connection that
  raised it. Inventing one (first-claim-wins) would hand the proposal to
  whichever connection races fastest, which an attacker wins as easily as a
  user.

  What was built is a **standing** rule: every one of the seven methods checks
  `ConnState::may_hold_session_access()` — REQ-569's ancestry gate — which is
  exactly the bar this bug's own *Expected Behavior* section names ("minimally,
  not by the daemon's own spawned children"). Seven separate one-line gates
  rather than one shared check, so REQ-570 AC-11's mutation check can remove a
  *single* method's gate and see a test go red (LESSON-502). See REQ-570
  architecture.md ADR-A.

  **Residual, recorded rather than inherited silently.** REQ-569 ADR-A documents
  that breaking the ancestry chain costs one model-supplied shell word
  (`setsid helper`, `helper &`), so a **non-descendant** same-UID process still
  passes this gate. Layer (a) closes the *ambient* hole this bug reports — any
  handshaked connection, including the daemon's own tool and MCP children, could
  commit a machine-wide model change — and does **not** close the
  determined-adversary case. Layer (b) is what covers that for daemon-wide
  commitments.
- **(b) Attestation for daemon-wide commitments** — a model change or a
  multi-GB download additionally requires a verified presence attestation,
  because its blast radius is the whole machine.

Verified per method by REQ-570 AC-10 (each seam tested, not one representative —
LESSON-502) and guarded by AC-11's mutation check.

Of the options originally listed here, "restrict answering to the connection
that raised the flow" was the intended pick and proved unimplementable for
`model/confirm` (see the correction above). `Ancestry` reuse — originally
rejected on REQ-569 ADR-A's evidence that the chain breaks on one shell word —
is what BR-10(a) actually adopts, with that weakness now recorded explicitly as
the residual rather than treated as a reason to do nothing: a gate that stops
every daemon child is strictly better than no gate, and the shell-word escape is
answered by layer (b)'s attestation for the commitments that matter.
First-answer-wins stays rejected, because recording the answerer documents the
hijack rather than preventing it.

## Related surface — the rest of the dispatch audit

Reported together because they share one shape: **the method takes no
connection**. Severities are my assessment after trying to refute each, not the
raw audit output.

| Method | Assessment |
|---|---|
| `model/confirm` | **High** — this bug. Concrete, demonstrated shape, real cost/state impact. |
| `cost/query` | **Medium** — returns a daemon-wide roll-up spanning every session (phase names, provider ids, token counts) to any connection. A genuine cross-session metadata read, directly adjacent to the BR-10 payload reduction REQ-569 is landing for `session/list`. Arguably should have been in REQ-569's scope. |
| `session/create` (not ancestry-gated) | **Medium** — a daemon descendant that BR-4 forbids from *attaching* may still create and drive its **own** session, spending the user's provider credits. Outside BR-4's literal wording, inside ADR-A's rationale. |
| `config/set`, `model/set` | **Low-Medium, deliberately downgraded.** The audit rated `config/set` highest-impact ("repoint every session's model traffic"). I checked before filing: config lives at `base_dir/config.toml`, a file any same-UID process can already write directly. Gating the RPC therefore removes *immediacy* (no daemon restart needed), not a capability — it is not a new boundary crossing for the adversary in scope. Worth fixing as defense in depth; not the emergency the raw finding suggests. |
| `config/get` | **Low** — exposes provider endpoints and `auth_ref` **names** (not secret material) to any connection; same file-access refutation applies. |
| `web/refresh` | **Low** — evicts a cached document, affecting the next lookup. Nuisance, not disclosure. |

## Files Changed

(none yet)
