---
id: TASK-104
title: "Unguessable session ids (defense in depth)"
status: draft
parent: REQ-569
created: 2026-08-11
updated: 2026-08-11
dependencies: []
---

## Description

Replace sequential `sess-{n}` ids with random, unguessable ones (BR-8, ADR-H).
This is **defense in depth only** — grants are the access control, ids are
names. Nothing may key a decision on unguessability.

## Files to Create/Modify

- `crates/tetond/src/sessions.rs` — `SessionRegistry::create` mints a random id (128 bits, base32/hex, e.g. `sess-<26 chars>`) instead of `format!("sess-{n}")` off the counter. Keep the `sess-` prefix so logs and existing string handling stay legible. Use the workspace's existing RNG dependency if one is present; if none is, prefer `getrandom` over pulling in a large RNG stack (check `Cargo.toml` first and say which you used and why).
- The `counter` field: if it serves nothing else after this change, remove it rather than leaving a dead monotonic counter (a leftover counter is what BUG-161 was made of). If other code reads it, leave it and note why.
- Test fixtures across the workspace that hardcode `"sess-0"` / `sess-{n}` — grep the whole workspace (`crates/*/tests`, `#[cfg(test)]` mods, `crates/tetond/tests/e2e/*`) and convert each to use the id returned by `session/create`. There are many; this is the bulk of the task.

## Acceptance Criteria

- [ ] Two sessions created in one daemon get ids that are not sequentially related; a test asserts non-adjacency/format rather than exact values.
- [ ] No test anywhere hardcodes a session id literal — all capture the created id. Grep proves zero remaining `"sess-0"`-style literals (excluding deliberate negative fixtures for "unknown session", which must use an obviously-synthetic id like `sess-nonexistent`).
- [ ] `SessionRegistry::get` / turn-begin lookups are unchanged in behavior (exact string match, same `UNKNOWN_SESSION` semantics).
- [ ] Full workspace `cargo test --workspace --no-fail-fast` green.

## Technical Notes

- Do NOT make any authorization decision depend on id entropy (BR-8 is explicit) — this task changes generation only, never a check.
- Ids appear in logs; keep them short enough to stay readable.
- Deliberate negative-lookup fixtures should read as obviously synthetic (LESSON-497's sentinel principle applied to ids).
