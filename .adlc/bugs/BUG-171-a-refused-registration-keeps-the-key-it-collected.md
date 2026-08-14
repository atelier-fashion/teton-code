---
id: BUG-171
title: "A refused provider registration keeps the key it collected — and doesn't say so"
status: resolved
severity: medium
created: 2026-08-14
updated: 2026-08-14
component: "cli/provider-add"
domain: "providers"
stack: ["rust", "cli", "keychain"]
concerns: ["security", "credentials"]
tags: ["provider-add", "keychain", "rollback", "orphaned-credential", "req-572", "req-577", "bug-170"]
---

## Description

`teton provider add` collects the API key (echo-off) and stores it in the OS
keychain **before** it asks the daemon to register the provider: `run_provider_add`
calls `build_provider_registration` (which does `keychain.store`) and only then
issues `config/set` (`crates/teton/src/main.rs:1349-1366`). When the daemon
rejects the registration — e.g. `ConfigError::MissingEndpoint` for a remote kind
with no `--endpoint` — the error arm prints only:

```
provider `{id}` registration rejected: {message}
```

The credential stays in the keychain, unreferenced by any config, and the
message neither removes it nor names it. The user is left with residue they
never agreed to and cannot see — the exact class REQ-572 BR-11 eliminated for
`/web setup` ("removes any keychain entry the aborted flow run itself
created"). BUG-170 documents users hitting this sequence since 0.1.13 (the
README's own `anthropic` example walked every reader into it until PR #144
fixed the docs); this bug is the mechanism that made those failures leave a
credential behind.

Two adjacent arms of the same match share the defect's shape:

- A **transport failure** on the `config/set` call (`conn.call(...)?`) exits
  with the key stored and unmentioned — and unlike the rejection, the
  registration may or may not have landed, so the honest answer is `/web
  setup`'s ambiguous-commit treatment (leave it, say where it is), not a
  delete.
- The **`applied: false`** arm ("the daemon did not apply the registration")
  can only follow a stored key when the identical provider already exists, in
  which case the entry is live and the typed key silently *rotated* it —
  worth a sentence, not a delete.

## Reproduction Steps

1. `teton provider add opus --kind anthropic --model claude-opus-5` (no
   `--endpoint`), entering any key at the echo-off prompt.
2. Observe the rejection: `provider 'opus' registration rejected: provider
   'opus' is a remote provider and must set an 'endpoint'`.
3. `security find-generic-password -s teton -a opus` — the entry exists,
   referenced by nothing, and no output from step 2 mentioned it.

## Expected Behavior

A rejected registration takes back the keychain entry it created for that
attempt (or, when the store held a prior credential the attempt displaced,
puts that credential back), and says what it did. A cleanup the keychain
refuses is reported with the `security delete-generic-password` command that
finishes the job — the user is the only one who can act on the keychain by
hand.

## Actual Behavior

The rejection line prints and the flow returns success. The typed key remains
in the keychain under the provider id, unreferenced, unmentioned, invisible.

## Environment

- Platform: macOS (the only target with a real keychain backend)
- Version: since 0.1.13 (`provider add` + keychain landed); still present at HEAD (ec4bf8c)

## Root Cause

Store-then-register with no undo protocol. The keychain trait grew exactly the
needed machinery in REQ-572 (`Keychain::delete` per ADR-3, `Keychain::read` as
the read-half of read-before-write) and `/web setup` built the three-state undo
on top of it (`PriorKey`: *absent* licenses a delete, *present* obliges a
restore, *unreadable* licenses neither — `crates/teton/src/web_setup_ui.rs:517`),
but both were wired to that one caller — the trait docs literally say "exactly
one caller" — and `provider add`, the *other* place a credential is typed,
never adopted it. Blind delete-on-rejection would not be correct here either:
the keychain account is the provider id, so an id chosen to collide with a live
entry (e.g. a provider named `web-search`) would have its displaced credential
destroyed rather than restored — the restore/delete distinction is load-bearing.

## Resolution

The `/web setup` three-state undo (`PriorKey`/`Cleanup`) moved from
`web_setup_ui.rs` into `keychain.rs`, generalized over the account — which is
now captured *inside* `PriorKey` at read time, so the undo cannot be aimed at
a different entry than the one that was inspected. `run_provider_add` adopted
it: the prior state is read immediately before the store, and the outcome
handling moved into a testable `report_registration_outcome` whose rejection
arm runs `PriorKey::undo` — absent → delete the entry this attempt created,
present → restore the displaced bytes, unreadable → touch nothing — and says
what it did, with the `security` command that finishes any cleanup the
keychain refused. Two adjacent arms became honest without deleting anything:
a transport failure (registration may have landed) reports the ambiguity and
names the ref; `applied: false` after a stored key names the ref instead of
leaving the rotation invisible. Success/pending lines for local providers no
longer claim a key was stored under ref `—`.

## Files Changed

- `crates/teton/src/keychain.rs` — `PriorKey`/`Cleanup` moved in (account
  captured at read; redacting `Debug` preserved); `read`/`delete` trait docs
  updated from "exactly one caller"; four new unit tests for the shared undo.
- `crates/teton/src/web_setup_ui.rs` — rewired to the shared machinery
  (`PriorKey::read(keychain, SEARCH_KEY_ACCOUNT)`, `displaced()`); local
  definitions deleted; rendering stays flow-owned.
- `crates/teton/src/main.rs` — `run_provider_add` reads the prior state before
  the store and binds the RPC instead of `?`-ing past it;
  `report_registration_outcome`, `provider_cleanup_line`,
  `registration_unanswered_line` added; eight new unit tests driving every arm
  through `MockKeychain` + `RecordingSurface`.

## Deployment

n/a — plain OSS flow (PR-gated CI on `main`, no staging pipeline); ships in
the next tagged release. Fix PR: #146.

## Lessons

- LESSON-525 — sweep every concern across every surface the REQ already
  enumerated ("exactly one caller" docs are standing audit prompts).
