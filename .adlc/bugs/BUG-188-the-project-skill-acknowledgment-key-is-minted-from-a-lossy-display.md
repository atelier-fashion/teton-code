---
id: BUG-188
title: "The project-skill acknowledgment key is minted from a lossy display, so two repositories can share one grant"
status: open
severity: medium
created: 2026-08-21
updated: 2026-08-21
component: "permissions"
domain: "daemon"
stack: ["rust"]
concerns: ["correctness", "security", "privacy"]
tags: ["permission-key", "session-root", "utf8", "req-587", "adr-7", "assume-017", "br-4"]
---

## Description

`project_skill_trust:<root>` (REQ-587 BR-4, ADR-7) was minted at
`harness/tools/skill.rs` from `teton_core::session_root::display_for`, which
ends in `Path::display`. `Path::display` renders every byte outside a valid
UTF-8 sequence as `U+FFFD`.

Two distinct repository roots differing only in such bytes therefore render
**identically** and mint **one** key. A session grant answered for root A then
answers for root B — exactly what `project_skill_trust_key`'s own doc forbids,
and the same collapse that doc refuses to introduce by truncation ("two long
roots sharing a prefix must not collapse onto one key"), arriving through the
input instead.

`display_for` has **two** lossy exits, not one: the `~/{rest}` home-relative
branch and the absolute fallback. Both minted the defect.

## Impact

A grant to "may the model run this repository's skills as instructions" could be
spent on a repository the user never answered for — the whole harm the per-root
scope exists to prevent. Bounded in practice by
`expires_on_session_root_change`, which drops the key when the root moves, so
the key does not outlive the root it was answered for.

Rated medium rather than high on that bound, and because reaching it needs a
root whose path is not valid UTF-8.

## Fix (in flight — PR #198, not yet merged)

`session_root::key_form_for` — `display_for`'s home-relative shape made
injective. Every byte a UTF-8 decode rejects becomes `%XX`; a literal `%`
becomes `%25`, so a path that spells an escape is never confused with one
holding the byte. Distinct roots give distinct keys.

The two spellings **coincide** for any path with no `%` and no invalid byte, so
the ordinary key stays the readable `project_skill_trust:~/dev/teton` a client
can print, and no existing test churned. Both renderings are minted at one call
site from one path, so they cannot drift (ASSUME-017).

**Escaped rather than hashed, deliberately.** A hash or hex of the raw `OsStr`
bytes settles identity but puts the *absolute* path — and the username — back
into a key that can reach a client's refusal line, contradicting REQ-585 BR-1's
entity table and the pinned assertion that the key carries no `/Users/`. A hash
also cannot be *shown*, and trades an exact answer for a collision probability
this needs no part of. Escaping keeps identity exact, the username out, the key
showable, and adds no dependency to `teton-protocol`, a documented pure leaf.

The client needed no change: `SessionGrants` only stores and expires the
daemon's string, never mints, and `is_project_acknowledgment_key` still
recognizes the escaped form.

## Interim mitigation this replaces

REQ-587's verify pass could not fix this — `harness/tools/skill.rs` was owned by
a concurrent agent — and landed a fail-closed refusal instead:
`authorize_project_skill_trust` returned `SkillConsent::Unanswerable` for any
root whose display carried `U+FFFD`, pinned by
`a_root_whose_display_cannot_name_it_is_not_acknowledged`.

That was fail-closed, not correct: a repository with a non-UTF-8 path simply
lost model-invocable project skills. **With BUG-188 fixed, the refusal, its
test, and the "The display is lossy" section of `project_skill_trust_key`'s doc
are all to be deleted** — the merge resolution is a deletion, not a merge. The
relaxed `debug_assert` in `permissions.rs` is the hunk to watch: keeping both
sides yields an assert that contradicts the mitigation directly above it.

## Found

REQ-587 Phase 5 verify (deferred as unfixable under concurrent file ownership),
2026-08-20; fixed 2026-08-21 in PR #198, stacked on #196.
