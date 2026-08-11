---
id: TASK-104
title: "Unguessable session ids (defense in depth)"
status: complete
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

- [x] Two sessions created in one daemon get ids that are not sequentially related; a test asserts non-adjacency/format rather than exact values.
- [x] No test anywhere hardcodes a session id literal — all capture the created id. Grep proves zero remaining `"sess-0"`-style literals (excluding deliberate negative fixtures for "unknown session", which must use an obviously-synthetic id like `sess-nonexistent`).
- [x] `SessionRegistry::get` / turn-begin lookups are unchanged in behavior (exact string match, same `UNKNOWN_SESSION` semantics).
- [x] Full workspace `cargo test --workspace --no-fail-fast` green.

## Technical Notes

- Do NOT make any authorization decision depend on id entropy (BR-8 is explicit) — this task changes generation only, never a check.
- Ids appear in logs; keep them short enough to stay readable.
- Deliberate negative-lookup fixtures should read as obviously synthetic (LESSON-497's sentinel principle applied to ids).

## Implementation Notes

**RNG: `getrandom = "0.2"`, direct on `tetond`.** The workspace had no direct RNG
dependency, but `getrandom` 0.2.17 was already compiled into the daemon
transitively (`ring` -> `rustls` -> `reqwest`), so making it direct added **zero
crates** to `Cargo.lock` — one line, no new build graph. It is also the right
shape for the job: a thin `getentropy(2)` wrapper with no generator state to
seed, reseed, or inherit across a fork, against a call site that wants 16 bytes
per session. A `rand`-family dependency would have added a userspace CSPRNG
stack for that.

**Format:** `sess-` + 26 characters of lowercase Crockford base32 = 128 bits
(e.g. `sess-515xssmk6hny5ttjtyjqce6z8m`). Crockford drops `i`/`l`/`o`/`u`, so an
id transcribed off a log line cannot silently become a different valid-looking
id. The `sess-` prefix is kept for log legibility; the entropy is entirely in
what follows.

**The counter is gone.** `SessionRegistry::counter: Arc<AtomicU64>` was read by
nothing else, so it was removed rather than left dead — a leftover monotonic
counter is the BUG-161 shape.

**An entropy failure refuses the create** rather than falling back to a
predictable id, through the `Result` that `create` already returns. A silent
fallback would restore the enumerable namespace exactly where nobody is looking.

**Nothing keys a decision on unguessability** (BR-8, ADR-H). This changed
generation only; `get`, `try_begin_turn`, and every attach/clear gate still do
the same exact string match with the same `UNKNOWN_SESSION` semantics.

**Fixture churn**, in three kinds:

1. *Captured* — six `server.rs` tests created a real session and then named it
   `sess-0`. They now read the id back off the `session/create` response through
   one `created_session_id` helper.
2. *Masked* — `cli_e2e`'s `/quit` vs Ctrl-D test asserts two runs' transcripts
   are byte-identical, and three freshly spawned daemons now mint three
   different ids. A `mask_session_id` helper normalizes just the banner id, so
   the assertion stays a whole-output equality instead of retreating to the
   weaker suffix comparison; it panics when the banner is missing, so the mask
   cannot pass vacuously.
3. *Renamed* — ~100 literals in unit tests that never create a session (egress
   lookup/redaction, cost ledger, completion, protocol serde, CLI rendering)
   were opaque labels, not fixtures. They became obviously-synthetic names
   (`sess-under-test`, `sess-other`, `sess-alpha`/`sess-beta`,
   `sess-redacted`, …) so no test implies ids are a sequence. Negative-lookup
   fixtures use `sess-nonexistent` (LESSON-497's sentinel principle).
