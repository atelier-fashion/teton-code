---
id: LESSON-587
title: "A default's reach is not its list — it is every predicate that read the list's emptiness"
component: "daemon/config"
domain: "privacy"
stack: ["rust", "daemon"]
concerns: ["security", "privacy", "backward-compatibility"]
tags: ["secure-by-default", "short-circuit", "fail-closed", "blast-radius", "provenance", "req-597"]
req: REQ-597
created: 2026-08-29
updated: 2026-08-29
---

## What Happened

REQ-597 turned on a builtin `local-only` boundary set: thirteen
credential-shaped globs, in force on every machine without configuration. The
spec's blast-radius analysis was about those thirteen patterns — which files
would now be blocked, and the accepted false positives (`*.pem` and `*.key`
match ordinary test fixtures).

The implementation was straightforward and the acceptance tests passed. Then 27
unrelated tests failed, none of them about `.env` or `.ssh/`. They were skill
tests, and the failures said *"this turn's content is under a local-only privacy
boundary, and no local tier is available to serve it"* for skills that touched no
credential file at all.

The cause was one line nobody had listed as a call site:

```rust
pub(crate) fn context_is_sensitive(ctx: &ContextManager, boundaries: &[PrivacyBoundary]) -> bool {
    if boundaries.is_empty() {
        return false;
    }
```

REQ-585 had established that content the daemon **cannot attribute** to a
repo-relative path — output a skill's command produced, a skill file outside the
session root — fails closed. But it only reaches that decision when a boundary
list is non-empty. On a stock machine the list *was* empty, so the whole
fail-closed path was unreachable, and had been since it was written. Turning the
default on did not widen the glob list's effect; it switched on a second,
much broader rule that had been dormant behind an emptiness check.

The reach of the change was therefore not "these thirteen patterns" but
"**anything the daemon cannot pin**" — on every machine, where before it was
only on machines whose owners had configured boundaries themselves.

## Lesson

Before flipping a default from empty to non-empty, grep for every predicate that
branches on the collection being **empty** — not for readers of the collection's
contents. Readers are what a blast-radius analysis naturally enumerates, and they
are the ones the spec will already have covered. An `is_empty()` short-circuit is
invisible to that search: it names no member of the set, matches no glob, and
appears in no list of "what this default will now block".

Each such short-circuit is a dormant rule with a precondition you are about to
satisfy for everybody at once. The rule may well be correct — this one was, and
BR-4 ("a builtin boundary is indistinguishable at enforcement time") required
it — but *correct* and *anticipated* are different claims, and the difference is
what a user feels on upgrade.

The general shape: **a guard gated on "is anything configured?" is a feature
flag whose owner is the emptiness of a collection.** Changing the collection's
default flips the flag, and no test of the collection's contents will tell you.

## Applies To

- Any change turning an opt-in collection into a populated default: boundary
  sets, allowlists, denylists, policy tables, registered rules.
- Any `if xs.is_empty() { return <permissive> }` fast path — the permissive
  return is the tell.
- Reviewing a "secure by default" REQ: ask what *else* becomes reachable, not
  only what the new entries match.

## Evidence

`crates/tetond/tests/skill_turn.rs::a_skill_that_ran_a_command_is_pinned_by_the_default_boundaries`
pins the interaction, with a declined-command control so it is a statement about
unpinnable *output* rather than about skill turns generally. Recorded as REQ-597
OQ-5 so the widening is decided rather than inherited.
