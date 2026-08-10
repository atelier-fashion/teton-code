---
id: TASK-090
title: "Packaging: drop keep_alive, hard-wire never, caveats + docs for the migration"
status: pending
parent: REQ-565
repo: teton-code
created: 2026-08-10
updated: 2026-08-10
dependencies: [TASK-088]
---

## Description

Stop the shipped install from resurrecting the daemon, make the `brew services`
opt-in pair with the `never` policy so launchd and the daemon cannot fight, and
tell existing users how to migrate (BR-5, AC-5, AC-6).

## Files to Create/Modify

- `packaging/homebrew/teton.rb.tmpl` — service block: remove `keep_alive true`; change `run` to `[opt_bin/"teton-code", "--shutdown-policy", "never"]`; update the block comment (its current text explains keep_alive's interaction with a second instance and is now wrong). Add a `caveats` block with the one-time `brew services stop teton` migration.
- `.github/workflows/release.yml` — re-point the `verify-install` smoke (the "services start · doctor · restart · stop (AC-6 / REQ-548 BR-6)" step, ~line 2099) to prove the **new** lifecycle claim.
- `README.md` — rewrite the lifetime story (lines ~16–27 first-run offer, ~56–71 upgrading).
- `docs/` — a section explaining old vs new lifecycle and the always-on opt-in.

## Acceptance Criteria

- [ ] The rendered formula's service block carries **no** `keep_alive`, and the
      default `brew install` performs no boot-time start (AC-5).
- [ ] The service block passes `--shutdown-policy never` explicitly, so the
      always-on opt-in cannot flap against the daemon's self-exit (BR-5, OQ-2).
- [ ] The release smoke proves the new claim — install → CLI round-trip → the
      daemon process is **gone** — rather than being deleted or weakened
      (AC-5; LESSON-459: a gate proves only what it exercises). The `never` +
      `brew services` path keeps its own assertion so AC-8's always-on mode
      stays covered.
- [ ] The smoke controls for the D-3 deferral: a round-trip that triggers a model
      download/load legitimately keeps the daemon alive, so the assertion must run
      against a daemon with no install in flight, or wait for it. A smoke that
      flakes here is worse than no smoke.
- [ ] Caveats instruct existing users to run `brew services stop teton` once, and
      say why (AC-6).
- [ ] README no longer claims "nothing is silently wrong" about a stale daemon
      (lines ~68–71) — that is true only for disjoint *protocol* ranges, and the
      v0.1.12/v0.1.13 case in REQ-565's Description is exactly the same-protocol
      case it does not cover. Point at the new build-skew warning instead.
- [ ] The first-run `brew services` registration offer (`crates/teton/src/service.rs`)
      is reconciled with the new default: registering the service is now the
      **always-on opt-in**, not the recommended default path. Its sentence must
      not promise reboot survival as if it were the norm.

## Technical Notes

- `brew services` generates `RunAtLoad`; that is the opt-in path and is fine.
  AC-5's "no boot-time start" is satisfied by `brew install` not registering a
  service at all — verify that is actually true of the rendered formula rather
  than assuming it.
- The formula template is the source of truth (ADR-006); the tap is a publish
  target and is never hand-edited.
- Existing installs cannot be migrated automatically (spec Assumptions) —
  caveats and docs are the whole mechanism.
