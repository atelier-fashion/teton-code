---
id: LESSON-578
title: "A rule attached to a UI flow guards one of the doors the record can come in through"
component: "daemon/config"
domain: "privacy"
stack: ["rust", "daemon"]
concerns: ["security", "privacy", "developer-experience"]
tags: ["config-validate", "cleartext", "auth-ref", "bug-202", "enforcement-point", "sweep", "escape-hatch", "secure-default"]
req: BUG-202
created: 2026-08-28
updated: 2026-08-28
---

## What Happened

`Config::validate()` refuses a `[web]` `search_key_ref` beside a cleartext
`http://` endpoint — a hard, fail-closed error that gates daemon startup. The
identical hazard for a **provider** was not refused. A config pairing
`endpoint = "http://api.example.com"` with `auth_ref = "keychain://teton/x"`
loaded cleanly and put an `x-api-key` header on the open wire on every turn.

There *was* a check. It lived in `provider_setup_warnings`, reachable only from
the guided `teton provider add` flow, and it was a warning. That flow is one of
**three** ways a provider record comes into being; the other two — a hand-edited
`config.toml` and a config migrated from an older schema — had no check at all.

The predicate's own doc comment is what settled the root cause. It records that
`is_cleartext_to_a_remote_host` was made public "so `teton provider add` can
warn." The provider case had been *considered*. It was attached to the UI that
happened to be under construction, rather than to the document the rule is about.

A second finding arrived only because the fix was attempted. The obvious fix —
mirror the `[web]` rule exactly — was written, tested, and **reverted**:
`is_cleartext_to_a_remote_host` exempts only *loopback*, so it also rejects
`http://10.0.1.50:8000`, a self-hosted model server on a trusted LAN. That is a
legitimate topology for this product's audience, and no reliable rule
distinguishes `models.corp.example.com` from `models.example.com`. The
asymmetry with `[web]` was therefore not purely an oversight: every `[web]`
backend is public SaaS, where cleartext is almost certainly a mistake; provider
endpoints are not.

## Lesson

**Attach a rule to the artifact it constrains, not to the flow that happens to
create the artifact today.** A check in a setup wizard covers the users who used
the wizard. The config document is what every path converges on, so a rule about
the document's validity belongs in the document's validator. When you find a
guard inside a UI flow, ask which *other* doors reach the same state — a
hand-edited file and a migration are doors.

Two corollaries, both of which cost real work here:

1. **Before mirroring a sibling rule, check whether the sibling's domain
   assumptions hold.** `[web]` can be absolute because its endpoints are always
   public. Providers cannot, because theirs are not. Copying the rule verbatim
   would have broken working installs on upgrade — the most expensive kind of
   regression, because it fires on machines nobody touched.
2. **When the safe default has a legitimate exception you cannot detect, make
   the default safe and the exception explicit.** `allow_cleartext = true` beats
   both a permissive default and a heuristic that guesses at DNS names. The
   flag is greppable exactly where somebody chose to turn protection off, which
   is worth more than any inference. A refusal must also **name its own escape
   hatch**, or it is a dead end for the person holding a legitimate case.

## Why It Matters

The gap sent a live API credential across the public internet in cleartext on
every turn, for any user whose config was hand-written or migrated — which is to
say, the users least likely to have been walked through a warning. It survived
four months and a green 3,997-test suite, and was found by audit rather than by
CI.

The reason CI could not see it is worth stating on its own: the `[web]` test
covering the identical invariant **stays green** when the provider guard is
deleted. Two enforcement points of one rule, and a full suite passing tells you
nothing about the second. That is `conventions.md`'s "an invariant with more
than one enforcement point needs a sweep, not a fix," demonstrated rather than
asserted — and the mutation that proves it is recorded in the new test's doc
comment.

The near-miss on the fix matters as much as the bug. Shipping the obvious mirror
would have traded a credential leak for a startup refusal on every self-hosted
LAN install.

## Applies When

- Adding a validation rule that already exists for a sibling config section, and
  reaching for copy-then-adapt.
- Finding a security or correctness check inside a setup wizard, guided flow, or
  CLI subcommand rather than in a validator, parser, or constructor.
- Deciding between fail-closed and warn for a config record that is coherent but
  unsafe — see `conventions.md` on config validity vs usability.
- Any rule whose predicate cannot distinguish a legitimate case from a dangerous
  one (private vs public hosts, internal vs external names). Prefer an explicit
  opt-out over a heuristic.
- Adding a field that an existing RPC rebuilds a record from: check whether the
  rebuild preserves it, or a later unrelated write will silently clear it
  (BUG-155, and reproduced here on `allow_cleartext`).
