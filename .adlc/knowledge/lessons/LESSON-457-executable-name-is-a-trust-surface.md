---
id: LESSON-457
title: "An executable's filename is a trust surface"
component: "daemon/packaging"
domain: "distribution"
stack: ["rust", "macos", "homebrew"]
concerns: ["trust", "developer-experience", "compatibility"]
tags: ["binary-naming", "permission-dialog", "keychain-acl", "bin-target", "upgrade-compatibility"]
req: REQ-549
created: 2026-07-31
updated: 2026-07-31
---

## What Happened

The daemon shipped as `tetond` — correct Unix convention (`sshd`, `launchd`),
and nobody questioned it in review. But macOS attributes permission dialogs
(Keychain, network) to the requesting process's *executable filename*, and the
daemon is the process that resolves `keychain://` auth references at call time
(REQ-544 BR-7). Users who had only ever typed `teton` hit a security prompt
naming "tetond" and read it as a typo — at the exact moment the product is
asking for trust. The fix (REQ-549, PR #12) renamed the shipped binary to
`teton-code` while keeping three things deliberately stable: the crate name
(`tetond::` imports and `--features tetond/llama` unchanged — a `[[bin]]`
target rename only), the runtime rendezvous filenames (`tetond.sock`/`.lock`/
`.log`, so a new CLI finds the old running daemon across an upgrade instead of
racing it), and the protocol (only `daemon_name` in the handshake changed).

## Lesson

Name every shipped executable as a product-facing surface, because the OS will
show that filename to users in consent dialogs where hesitation is most
costly. When renaming one later: rename the `[[bin]]` target, not the crate
(imports and feature-flag invocations stay valid everywhere), and treat
runtime rendezvous paths (sockets, locks) as a separate compatibility contract
that does NOT follow the rename without its own migration story. Budget for
the OS side effect: Keychain ACL grants bind to executable identity, so a
rename costs every user one re-prompt — and without a stable code-signing
identity, every rebuild costs one anyway.

## Why It Matters

A confusing name in a security dialog trains users to click through prompts
they don't understand — the opposite of the consent legibility the product
promises (REQ-547). And a naive rename that also moves the socket/lock breaks
single-instance across an upgrade: old daemon holds `tetond.lock`, new daemon
claims `teton-code.lock`, and two daemons serve simultaneously.

## Applies When

- Naming any new shipped binary, daemon, helper, or launchd/systemd service.
- Renaming an existing executable (checklist: bin target vs crate, runtime
  paths, handshake identity strings, packaging, release scripts, CI workflow
  YAML, test `CARGO_BIN_EXE_*` references, docs). The REQ-549 sweep covered
  `*.rs/*.sh/*.tmpl/*.md/*.toml` but not `*.yml` — release.yml's
  verify-install job still asserted `tetond.log` and failed the v0.1.1
  release gate until fixed (2fcc5c6, which derives the names from the
  formula rather than restating them — the [[LESSON-455]] shape: state the
  property once).
- Any change that alters executable identity on macOS — expect Keychain
  re-prompts; see REQ-549 OQ-2 (stable signing identity) before users scale.
