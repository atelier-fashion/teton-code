---
id: TASK-001
title: "BR-12 spike: is an OS presence prompt inert in an unsigned dev build?"
status: complete
parent: REQ-570
created: 2026-08-11
updated: 2026-08-11
dependencies: []
---

## Description

BR-12's hard sequencing gate. OQ-1 selected an OS-mediated presence prompt on
the claim that it is **not inert** in a plain `cargo run` build, unlike the code
signatures and keychain ACLs REQ-569 ADR-A rejected. That claim is load-bearing
for the whole REQ, so it is verified empirically **before any other task
starts**. If it fails, the REQ is re-scoped rather than shipped on the belief.

## Files to Create/Modify

- None in-tree. The spike is a throwaway binary built **outside** the repo —
  deliberately, so it cannot trip BUG-159 (`call_sites.rs` and `harness/duty.rs`
  read production source mid-run and panic if `src/` changes under them).
- Output lands in `architecture.md` §0, which is the durable record.

## Acceptance Criteria

- [x] The spike binary's signing posture is confirmed and recorded, since
      "unsigned" is the whole variable.
- [x] macOS: `canEvaluatePolicy` and `evaluatePolicy` are exercised against
      `deviceOwnerAuthentication` and the outcome recorded.
- [x] The inert/not-inert discrimination is made on evidence that cannot be a
      runloop-starvation artifact.
- [x] Linux: the no-agent posture BR-11 asserts is probed rather than assumed.
- [x] Findings recorded in `architecture.md` §0 with the residual stated.

## Implementation Notes (as built)

**PASS on both platforms. The REQ proceeds as specified.**

macOS, against a binary confirmed `adhoc, linker-signed`, `TeamIdentifier=not
set` — exactly ADR-A's inert-signature posture:

- `canEvaluatePolicy(deviceOwnerAuthentication)` = `true`
- `canEvaluatePolicy(deviceOwnerAuthenticationWithBiometrics)` = `true`
- `evaluatePolicy` **blocked for the full 6s** with `CFRunLoopRunInMode`
  servicing the runloop throughout, so "still pending" is not starvation — a
  real prompt was up, waiting on a human. An inert mechanism errors in
  milliseconds.
- `-[LAContext invalidate]` resolved it `LAError -9` (`appCancel`), which is
  the BR-7 distinguishability result: LAError codes separate failure (-1),
  user cancel (-2), system cancel (-4) and app cancel (-9) at the source.

Linux (headless `debian:bookworm-slim`): no system bus socket, no polkit
binaries, `/usr/share/polkit-1/actions` root-owned (BR-11's system-path claim,
literally true). With polkit installed and running, `pkcheck` as **non-root**
returned `Authorization requires authentication but no agent is available.`
(exit 2) — the authority answered, and answered "no agent", so the refusal must
key on agent-availability rather than on "is polkit installed". `pkexec`'s
textual-agent fallback died on a missing `/dev/tty`, which is the same no-TTY
constraint that rules out a terminal prompt for the VS Code client — so the
textual agent does not rescue the degraded case.

Residual: the spike did not complete a *successful* authentication (no human was
present; it cancels by design). It proves the prompt is presented and blocking,
which is the question BR-12 asked. Recorded in architecture.md §7.
