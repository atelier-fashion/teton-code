---
id: LESSON-525
title: "Sweep every concern across every surface the REQ already enumerated"
component: "cli/keychain"
domain: "providers"
stack: ["rust", "cli", "keychain"]
concerns: ["security", "credentials", "process"]
tags: ["rollback", "undo", "one-caller", "cross-product", "bug-171", "req-572", "provider-add", "web-setup"]
req: BUG-171
created: 2026-08-14
updated: 2026-08-14
---

## What Happened

REQ-572 hardened credential handling around two concerns — a key echoed into
scrollback, and a key left orphaned when the commit it was collected for is
refused — and it *knew about both surfaces where a credential is typed*: AC-5
explicitly extended the echo-off prompt to `teton provider add`. But the
residue concern got its full treatment (`Keychain::read`/`delete`, the
`PriorKey` three-state undo) on `/web setup` only. The trait docs even recorded
the gap in plain sight: "it has exactly one caller." `provider add` kept
storing keys before a `config/set` the daemon could refuse, with no undo and no
mention — BUG-170's README examples then walked every reader into exactly that
rejection, and users accumulated invisible orphaned credentials from 0.1.13
until BUG-171.

## Lesson

When a change addresses N concerns and has already enumerated M sibling
surfaces, the checklist is the N×M cross product — verify each concern landed
on each surface, not just somewhere. And treat "exists for one caller" /
"exactly one caller" doc comments as standing audit prompts: they are honest
today and a defect marker the moment a sibling call site exists (or already
existed). When building single-caller machinery, grep for the sibling shape
(here: any `keychain.store` followed by a refusable RPC) and either adopt it
everywhere or file the gap explicitly.

## Why It Matters

The half-swept concern is worse than the unswept one: the REQ reads as "done"
(both surfaces appear in its ACs), review passes because the machinery
demonstrably exists and is tested, and the gap only surfaces when users hit
the unprotected sibling — here as security-relevant residue (orphaned API keys
in the OS keychain, unmentioned) that shipped for four releases and took a
separate security re-review to flag.

## Applies When

- A REQ/fix names multiple surfaces in its ACs but implements a cross-cutting
  protection (rollback, validation, redaction, locking) on fewer than all of
  them.
- Writing or reviewing a doc comment that scopes machinery to "one caller" —
  especially undo/cleanup machinery whose absence fails silently.
- Reviewing flows with a store-then-commit shape: anything persisted before a
  refusable operation owes the failure path an undo or an honest sentence.
