---
id: TASK-285
title: "The builtin boundary set and the one function that composes it"
status: pending
parent: REQ-597
repo: teton-code
created: 2026-08-29
updated: 2026-08-29
dependencies: []
---

## Description

The core of the REQ: a shipped `local-only` glob list, an `origin` on the boundary entity, an
explicit opt-out, and the single function that composes user rows with builtin rows. Covers
BR-1, BR-2, BR-2.1, BR-3, BR-4, BR-7.

This task ships **no behaviour change** — nothing calls `effective_boundaries()` yet. That is
deliberate: the composer and its unit tests land alone so their semantics are pinned before
seven call sites depend on them.

## Files to Create/Modify

- `crates/teton-core/src/entities.rs` — new `BoundaryOrigin { Builtin, User }` enum,
  `#[derive(Default)]` with `#[default] User`, `#[serde(rename_all = "kebab-case")]` to match
  `BoundaryMode`'s spelling. Add `origin: BoundaryOrigin` to `PrivacyBoundary` with
  `#[serde(default, skip_serializing_if = "BoundaryOrigin::is_user")]`.
- `crates/teton-core/src/config.rs` — `DEFAULT_BOUNDARIES` const, `Config::effective_boundaries()`,
  `disable_default_boundaries` on `PrivacyConfig`.
- Every in-tree `PrivacyBoundary { .. }` struct literal (37 sites across `teton-core`, `tetond`
  src and tests) — updated to compile. Production sites state `origin` explicitly; test literals
  may use `..Default::default()`.

## Acceptance Criteria

- [ ] BR-1: `DEFAULT_BOUNDARIES` contains exactly the thirteen globs the spec lists, in the
      spec's order, each `BoundaryMode::LocalOnly` and `BoundaryOrigin::Builtin`.
- [ ] BR-2/BR-2.1: `effective_boundaries()` returns user rows **first**, builtin rows appended
      after. A unit test asserts the composed order by index, not merely by set membership.
- [ ] BR-3: `disable_default_boundaries = true` yields the user's rows alone; `false`/absent
      yields the composition. There is no other path to an empty result when user rows exist.
- [ ] BR-7: with a user row `**/.env` declared, `BoundaryMatcher::new(&effective).match_path(".env")`
      returns the **user's** row — asserted on `origin` and `mode`, not on `is_some()`.
- [ ] **No deduplication.** A user row whose glob is byte-identical to a builtin leaves *both*
      rows in the composed list, user first. A unit test asserts the composed length is
      `users + 13` even when a glob collides. BR-7's "one block, not two" is a statement about
      enforcement, which `match_path` already guarantees by returning exactly one row —
      collapsing the pair in the composer instead would break AC-4.1 (which needs both rows
      present to prove which one wins) and would hide a builtin from `boundary list`.
- [ ] A table-driven unit test asserts each of the thirteen globs matches at least one
      repo-root-relative path it is meant to catch (`.ssh/id_rsa`, `.env`, `.env.local`,
      `.aws/credentials`, `.netrc`, `.npmrc`, `.git-credentials`, `.docker/config.json`,
      `.kube/config`, `certs/server.pem`, `a/b/c.key`, `id_rsa`, `id_ed25519.pub`) **and** that
      `src/main.rs`, `README.md`, `env`, and `notes/.envrc` match none of them.
- [ ] `origin` does not serialize for a `User` row: a `PrivacyBoundary` with default origin
      round-trips to TOML containing no `origin` key. This is what protects AC-10 and it is
      asserted here, at the type, not only end-to-end.
- [ ] `PrivacyConfig` remains `Copy` after the new field.
- [ ] `cargo test -p teton-core --no-fail-fast` is green.

## Technical Notes

`Config.boundaries` keeps its exact current meaning — the user's on-disk rows. Do **not**
populate it with builtins at load: `config_doc.rs::canonical_document` diffs `Config` against
the user's real TOML, so a builtin row living in `Config.boundaries` would be written to the
user's file on the next unrelated `config/set` (architecture ADR-1).

`skip_serializing_if` on the `User` origin is load-bearing for the same reason — without it,
`canonical_document` emits `origin = "user"` into every `[[boundaries]]` table. Prove it here
rather than discovering it in TASK-291's AC-10 test.

Append, do not prepend. `BoundaryMatcher::match_path` takes `.min()` over matched indices
(`boundary.rs:72-79`), so earliest declaration wins and appending is what makes BR-7 true.
This is settled by BR-2.1 — do not re-litigate it.
