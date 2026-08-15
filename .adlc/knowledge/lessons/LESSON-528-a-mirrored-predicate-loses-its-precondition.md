---
id: LESSON-528
title: "A mirrored private predicate inherits the code but not the precondition"
component: "daemon/config"
domain: "providers"
stack: ["rust", "daemon", "cli"]
concerns: ["security", "reliability"]
tags: ["predicate-mirroring", "crate-boundaries", "preconditions", "cleartext", "shape-validation", "req-578"]
req: REQ-578
created: 2026-08-15
updated: 2026-08-15
---

## What Happened

REQ-578's cleartext-credential notice needed
`teton_core::config::is_cleartext_to_a_remote_host`, which was private, so
the fix pass mirrored it into the CLI as `cleartext_remote_host` — a faithful
copy, edge cases carried over, second-copy comment naming the original. The
Step-D security re-review then found `http:/host`, `http:\\host`, `http:/\host`
and `http:\/host` all bypass it: url 2.5.8 accepts every one as `http://host`
and dials the remote, while the mirror's literal `http://` prefix check saw
nothing to warn about — no cleartext notice, no echo, key shipped in
plaintext to a silent screen. The copy was accurate; what it lacked was the
original's **precondition**: in `teton-core` the predicate only ever runs on
values `is_absolute_http_url` has already accepted, and provider endpoints
had no such gate. A second latent cost: the original being private meant no
test could pin the two copies' agreement, so they were free to drift forever.

## Lesson

A predicate's correctness lives half in its body and half in what its callers
have already established. Mirroring the body across a crate boundary silently
drops the second half. The fix that closes the whole class: **export and
reuse the original, and install its precondition at the new seam** — REQ-578
made `is_absolute_http_url` and `is_cleartext_to_a_remote_host` `pub`, gated
the registration seam on the shape check (which also closed the
backslash-authority and hostless shapes in the same move), and deleted the
mirror. If a predicate is worth copying, it is worth exposing; if it cannot
be exposed, the mirror needs a bridge test over a shared case table
(the `endpoint_composition_bridge` pattern) — an unpinned mirror is a drift
instrument.

## Why It Matters

The failure mode is a security control that reports success while a
one-character input variant walks around it — worse than absent, because the
warning's existence is what reviewers checked. And the gap is invisible at
the mirror's own call site: both copies pass their own tests, because each is
correct *under its own caller's assumptions*.

## Applies When

Copying any validation/classification function across a module or crate
boundary — first ask what its existing callers guarantee about the input;
reviewing a "mirrored from X" comment — the comment is the tell, demand the
export-or-bridge; adding a check to a seam whose input is less constrained
than the original's (registration argv vs validated config is exactly that
gap). See [[LESSON-523]] (two spellings of one contract) and [[LESSON-499]]
(a double with its own copy of the rule tests only shared bugs).
