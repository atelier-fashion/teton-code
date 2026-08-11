---
id: TASK-002
title: "BR-10(a): every daemon-wide method takes and checks connection context"
status: complete
parent: REQ-570
created: 2026-08-11
updated: 2026-08-11
dependencies: []
---

## Description

Closes BUG-162 (high severity, open). Seven daemon-wide methods take no
connection context at all, so any handshaked same-UID connection — **including a
daemon-spawned tool/MCP child that REQ-569 BR-4 otherwise excludes from session
access** — can commit a multi-GB download and a daemon-wide model change.

Deliberately **depends on nothing**, per BR-10's two-layer split: this must ship
without waiting on the attestation mechanism, or a high-severity hole stays open
for the length of that work.

Read `architecture.md` §1 (ADR-A) before starting. It records why this is a
**standing** rule and not the raiser-identity rule BUG-162's wording implies:
`model_selection_proposed` is raised by the *daemon* at startup, possibly before
the first connection exists, so there is no "connection that raised the request"
to bind to. The standing that already exists and is exactly right is REQ-569's
ancestry gate.

## Files to Create/Modify

- `crates/tetond/src/server.rs` — `dispatch` passes `conn` to all seven
  handlers; each checks `conn.may_hold_session_access()` and refuses
  `ATTACH_FORBIDDEN` if it fails. The seven:
  `model/confirm`, `model/set`, `config/set`, `config/get`, `cost/query`,
  `web/refresh`, and `session/create` (which already receives `conn` and simply
  never consults the gate).
- Tests: raw-RPC, **one per method** — AC-10 is explicit that a representative
  method is not sufficient (LESSON-502: an invariant enforced at several seams
  needs a test at each seam).

## Acceptance Criteria

- [x] Each of the seven methods refuses a connection failing the ancestry gate,
      asserted **per method at the raw RPC surface**, not for one representative.
- [x] `Ancestry::Indeterminate` is refused alongside `Descendant`, inheriting
      `may_hold_session_access`'s fail-closed policy.
- [x] The happy path is untouched: an ordinary non-descendant CLI still reaches
      all seven.
- [x] AC-8 regression bar: single-client create → prompt → stream, and the
      creator's own attach, gain **zero** new prompts or refusals.
- [x] Mutation check (AC-11, BR-10a arm): removing any one method's gate makes
      at least one test red. See it fail, restore.
- [x] `cargo test -p tetond --no-fail-fast` green.

## Technical Notes

- Gate **in the handler**, below `dispatch`, so raw-RPC tests exercise the real
  gate (LESSON-484, BUG-155) — not in `handle_client`, not in the CLI.
- Reuse `ATTACH_FORBIDDEN` / `ATTACH_FORBIDDEN_MESSAGE`. Do not mint a new code
  for the same condition: the connection learns the same thing either way, and a
  second spelling of one refusal is a second thing to keep in step.
- Do **not** run mutation checks concurrently with edits to `src/` — BUG-159
  panics `call_sites.rs` and `harness/duty.rs` when production source changes
  mid-run. If you see that panic, it is BUG-159, not your change.
