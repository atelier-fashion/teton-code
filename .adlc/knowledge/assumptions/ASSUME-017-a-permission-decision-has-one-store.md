---
id: ASSUME-017
title: "A permission decision the daemon expires is expired everywhere it is remembered"
status: invalidated
req: REQ-585
created: 2026-08-20
resolved: 2026-08-20
---

## Assumption

ADR-6 gave a skill's dynamic-context grant a scope: the key carries the source
(`skill:<source>:<name>`), and `/cd` drops the project-scoped ones, because a
project skill's name means a different file in a different repo. The assumption
underneath it — never written down, which is why it went unexamined — was that
expiring the grant on the **daemon** expired it, full stop.

`PermissionGate::drop_project_skill_grants` was written, called from inside
`set_session_cwd`, and pinned by
`skill_consent_matrix.rs::a_project_grant_is_dropped_when_the_session_root_moves`.
That test passes.

## What actually happened

The CLI keeps its own `SessionGrants` memo, keyed by the identical string, and
consults it **before** drawing any prompt. Nothing cleared it on
`session_root_changed`.

So the reachable sequence was: approve `/deploy`'s commands "for this session"
in repo A → `/cd` to repo B → the daemon forgets and re-asks → the client
auto-answers from repo A's approval → one `auto-allow` line goes by, repo B's
commands are never shown, and the daemon **re-remembers** the grant under the
new root.

That is verbatim the harm ADR-6 exists to prevent, moved one hop across the
seam. It was invisible to the daemon-side test by construction: by the time the
client answers, the request never reaches a human.

Found by the Phase 5 reflector, 2026-08-20.

## Consequence

`is_project_skill_key` and the key's spelling now live in `teton-protocol`
above both crates, and `SessionGrants::forget_project_skills` runs at the same
moment the daemon's drop does, under the same own-session condition. The
predicate is shared rather than re-spelled, so the two stores cannot drift
about *which* keys expire.

The general form is the part worth keeping: **a security decision with two
stores needs one invalidation rule, and the rule belongs above both.** It is
LESSON-494's shape (one parser for the gate and the executor) with a cache in
place of a parser. Any future grant, capability, or consent that the daemon
learns to expire should be checked for a client-side memo of the same fact
before the expiry is called done.

## Related

- ADR-6 (REQ-585 architecture) — the grant key and the `/cd` drop
- LESSON-495 — a grant is only as narrow as its key
- LESSON-501 — carried state sheds its invariants silently
- LESSON-494 — a gate and the client that executes the request must share one parser
