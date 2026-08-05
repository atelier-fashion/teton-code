---
id: TASK-054
title: "Route the digest call site through its category instead of hardcoding local"
status: draft
parent: REQ-558
created: 2026-08-05
updated: 2026-08-05
dependencies: [TASK-050]
---

## Description

`summarize_if_large` (`harness/context.rs:660`) is the one harness-known category
with a real call site. It is currently hardcoded to the local engine and does not
route at all. Tag it `digest` and resolve it through the category chain.

## Files to Create/Modify

- `crates/tetond/src/harness/context.rs` — `summarize_if_large` takes a resolved
  route rather than a bare local engine handle
- `crates/tetond/src/harness/turn_loop.rs` — the call site (`:544`) passes the
  `digest` resolution

## Acceptance Criteria

- [ ] `summarize_if_large` resolves through `Category::Digest` and honours a
      per-category override or its `scan` tier binding.
- [ ] The category is tagged **at the call site** — no prompt text is consulted to
      decide that a summarization is a summarization (BR-2).
- [ ] **The mechanical-truncation fallback survives routing failure.** If the
      resolved provider is unavailable or resolution fails, the function still
      bounds its input by truncation and reports the failure on its outcome — it
      does not return the input unchanged (LESSON-447).
- [ ] A test asserts the invariant holds on the failure path: an oversized input
      with an unresolvable `digest` binding still comes back bounded.
- [ ] Session taint still forces local for this call as for any other (BR-7).

## Technical Notes

**This call site guards an invariant** — "nothing oversized enters context" — so
LESSON-447 applies directly: a best-effort step that guards an invariant must
enforce it by degraded means on failure, not skip it. The existing code already
gets this right (it truncates on engine failure). The risk is that adding routing
introduces a *new* failure mode — unresolvable category — whose handler forgets the
invariant the old handler preserved.

**Watch the direction of change.** Today `digest` always runs local, which is free
and private. Making it routable means a user *can* bind it remotely, which sends
file content to a provider. That is intended (it is a `scan` tier duty), but it
interacts with boundaries: a `local-only` file's content must never reach a remote
digest. The egress choke point already covers this; TASK-057 proves it.
