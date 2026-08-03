---
id: LESSON-468
title: "Homebrew formulae have no uninstall hook — the CLI must own both ends of the lifecycle"
component: "distribution/packaging"
domain: "developer-experience"
stack: ["rust", "homebrew", "launchd", "macos"]
concerns: ["uninstallation", "data-retention", "consent", "service-management"]
tags: ["homebrew", "uninstall-hook", "cask-vs-formula", "self-uninstall", "first-run", "brew-services", "symmetric-lifecycle"]
req: none
created: 2026-08-03
updated: 2026-08-03
---

## What Happened

The user wanted `brew uninstall teton` to run the whole removal chain (stop the
launchd service, delete the 17 GiB model + cost DB, logs, keychain keys, tap).
Verified against Homebrew 6's source on-machine: formulae have **no uninstall
hook** — `uninstall`/`zap` stanzas are cask-only (and casks can't carry our
Linux target), and `brew uninstall` does not even stop a running
`brew services` service. The chain became a `teton uninstall` subcommand
(plan shown up front, size named, default-no confirmation, daemon-down gate
before any deletion). The same reasoning then absorbed the install-side
`brew services start` into a consent-first first-run offer in the CLI, taking
install from three commands to two.

## Lesson

When a package manager's lifecycle hooks are asymmetric, make the CLI the
source of truth for both first-run setup and last-run teardown. The package
manager moves binaries; the application orchestrates its own service
registration, GB-scale state, and credentials — in both directions, with
mirrored consent defaults (benign/reversible offers default yes; irreversible
deletions default no, so neither can happen by pressing return).

## Why It Matters

Without this, `brew uninstall` strands a running daemon, gigabytes of state,
and keychain secrets — users believe the software is gone while its footprint
and credentials remain. And every "single command" promise made at install
time that the formula cannot keep at uninstall time erodes trust in the
packaging story.

## Applies When

Packaging any daemon-shaped tool as a Homebrew formula (or any package manager
without uninstall hooks); designing install UX that currently needs a
`brew services`/systemd step; deciding where teardown logic should live.
