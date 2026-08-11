---
id: TASK-005
title: "attach/consent requires a verified attestation; the self-approval residual closes"
status: pending
parent: REQ-570
created: 2026-08-11
updated: 2026-08-11
dependencies: [TASK-004]
---

## Description

Wires attestation into REQ-569's consent flow, closing the BR-3 residual REQ-569
could not. `ConsentRoute` is kept **exactly as-is** — it is pure, table-tested,
and its two arms are still the right routing. What changes is what an *answer*
must carry. See architecture.md §3.

## Files to Create/Modify

- `crates/teton-protocol/src/methods.rs` — `AttachConsentParams` gains an
  optional attestation field.
- `crates/tetond/src/consent.rs` — the attestation gate on the allow path.
- `crates/tetond/src/server.rs` — `handle_attach_consent` consumes an
  attestation before minting; refuses `ATTESTATION_REQUIRED` otherwise.

## Acceptance Criteria

- [ ] AC-1: a headless same-UID process that requests attach to an unattended
      session and answers its own prompt is refused, and **no grant is minted**.
      Asserted at the raw RPC surface, and by inspecting the grant registry.
- [ ] BR-3: the self-render arm survives (resume must keep working) but its
      answer mints nothing without a verified attestation.
- [ ] A **deny** decision requires no attestation. Requiring one would let an
      absent mechanism force a grant to stay pending rather than be refused,
      which is fail-open in the wrong direction.
- [ ] AC-6: failure/cancel/timeout each mint nothing and leave **both** the grant
      and attestation registries empty, asserted by inspection.
- [ ] AC-8: the creator path is untouched — zero new prompts or attestation
      steps for single-client create → prompt → stream, or the creator's own attach.
- [ ] `cargo test -p tetond --no-fail-fast` green.

## Technical Notes

- Keep REQ-569's read-then-consume discipline: `route_of` is a read, so a caller
  about to be **refused** leaves the prompt standing for whoever may rightfully
  answer it. A refusal that consumed the waiter is a denial of service.
- The creator path mints no grant at all, so it needs no attestation — that is
  the Permissions table's "only the session's creator path, which mints no grant".
