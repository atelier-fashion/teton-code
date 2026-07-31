---
id: REQ-549
title: "Daemon process identity (teton-code) and interactive startup UX"
status: complete
deployable: true
created: 2026-07-31
updated: 2026-07-31
component: "cli/ux"
domain: "distribution"
stack: ["rust", "homebrew", "ansi-terminal"]
concerns: ["developer-experience", "trust", "compatibility"]
tags: ["daemon-rename", "permission-dialog", "keychain", "banner", "prompt-frame", "tty-gating"]
---

> **Backfill note.** This requirement was written retroactively on 2026-07-31,
> after the work merged as
> [PR #12](https://github.com/atelier-fashion/teton-code/pull/12)
> (commits `830bc34`, `9d7742b`, `2cd4130`). The pipeline was not run for this
> work: no pre-implementation spec, no architect phase, no multi-agent review,
> and the fmt gate was first caught by CI rather than by a local verify pass.
> The BR/AC checkboxes below are checked against evidence gathered during and
> after implementation, not against a phased pipeline run. See
> `pipeline-state.json` for the honest phase record.

## Description

macOS attributes permission dialogs — Keychain access, network/firewall
prompts — to the requesting process's executable name. The daemon is the
process that resolves `keychain://` auth references at call time (BR-7 of
REQ-544), so it is the daemon's name users see when the OS asks for consent.
It shipped as `tetond`: correct Unix convention (`sshd`, `launchd`), but users
who never typed that name read it as a typo of "teton" and hesitated at a
security prompt — the exact moment hesitation is most costly for trust.

Separately, the interactive session opened with bare log lines: no product
identity, no visually distinct place to type. Claude Code's startup — logo,
version/cwd block, clean entry area — is the reference experience.

This REQ (a) renames the shipped daemon executable to `teton-code` so every
OS attribution surface names the product, and (b) gives the interactive
session a startup banner (the outline of the Teton range) and a framed entry
area, both strictly gated so that non-interactive output is byte-identical.

## System Model

### Entities

| Entity | Field | Type | Constraints |
|--------|-------|------|-------------|
| DaemonBinary | name | string | `teton-code`; `[[bin]]` target rename only — the crate stays `tetond` |
| Crate(tetond) | name | string | unchanged; `tetond::` imports and `--features tetond/llama` remain valid |
| RuntimePaths | socket/lock/log | filenames | unchanged (`tetond.sock`/`.lock`/`.log`) — upgrade-compatibility invariant (BR-3) |
| Handshake | daemon_name | string | `teton-code`; surfaces in `teton doctor` |
| Banner | skyline | const art | upper envelope of five unit-slope peaks (Cathedral Group); ≤60 cols uncolored |
| EntryFrame | rule width | usize | terminal width via `TIOCGWINSZ`; fallback 80 |

### Events

| Event | Trigger | Payload |
|-------|---------|---------|
| permission_dialog_shown | daemon touches Keychain / network | OS attributes to executable name `teton-code` |
| banner_rendered | interactive `teton` start, stdout is a TTY | skyline, version, tagline, `~`-abbreviated cwd |
| entry_frame_drawn | each entry prompt, TTY only | rule / input row / rule; cursor repositioned into frame |

## Business Rules

- [x] BR-1: OS permission dialogs attribute daemon activity to a
      product-recognizable executable name: the shipped daemon binary is
      `teton-code`, and every surface that names the executable agrees —
      `--version`, startup banner line, refusal messages, handshake
      `daemon_name`, CLI autostart messages, packaging, release scripts,
      README.
- [x] BR-2: The rename is a `[[bin]]` target rename only. The crate keeps its
      internal name: `tetond::` imports and the `--features tetond/llama`
      flag are untouched, so no dependent build invocation changes.
- [x] BR-3: Runtime rendezvous filenames (`tetond.sock`/`.lock`/`.log`) are
      deliberately unchanged: a stable socket path means a newly-installed
      CLI finds an already-running old daemon across an upgrade instead of
      racing a second daemon against it. Renaming them is a separate,
      compatibility-breaking decision this REQ explicitly declines.
- [x] BR-4: The banner and entry frame render only when stdout is a
      terminal; piped output, subcommands, and the e2e suites see a
      byte-identical stream. Colour is a second, independent gate honouring
      `NO_COLOR` and `TERM=dumb`.
- [x] BR-5: Only the main entry prompt is framed. Permission questions and
      model proposals keep the plain dialogue prompter — they are dialogue,
      not entry, and framing them would bury the consent flow's legibility
      (REQ-547 BR posture).
- [x] BR-6: All new UI renders through the existing `Surface`/`Prompter`
      seams — no direct-to-stdout side channel — so the anticipated ratatui
      front-end inherits the banner and frame as a new `Surface`/`Prompter`
      impl.

## Acceptance Criteria

- [x] AC-1: The workspace builds no `tetond` artifact; `teton-code --version`
      prints `teton-code X.Y.Z`; the handshake reports
      `daemon_name: "teton-code"` and `teton doctor` displays it.
      *(Verified: cargo build output, version.rs test, live pty run.)*
- [x] AC-2: Homebrew formula installs/services `teton-code`; release
      `package.sh`/`smoke.sh`/`selftest.sh` build, tar, and assert on the new
      name. *(Verified: selftest 98/98; release-tooling CI gate green.)*
- [x] AC-3: Interactive `teton` on a TTY shows the skyline banner, bold
      product/version line, dim `~`-abbreviated cwd, then a framed entry
      area — dim full-width rules above and below with the cursor placed
      inside; Enter and Ctrl-D both land subsequent output cleanly below the
      frame. *(Verified: live pty capture of both paths.)*
- [x] AC-4: Non-TTY invocations are byte-identical to the previous release's
      output. *(Verified: both e2e suites pass unmodified.)*
- [x] AC-5: Full CI green on macOS and Linux — fmt, clippy (`all = deny`),
      workspace tests, acceptance suite, catalog integrity, cargo audit,
      release tooling. *(Verified: PR #12 checks, second run after fmt fix.)*

## External Dependencies

- New CLI-crate dependency: `libc` (already a daemon dependency) for
  `TIOCGWINSZ`. No new third-party crates otherwise.

## Assumptions

- A one-time Keychain re-prompt on upgrade is acceptable: the ACL grant is
  bound to the executable identity, so the renamed binary re-asks once. (The
  deeper fix — a stable code-signing identity so rebuilds/upgrades keep the
  grant — is out of scope here and pre-existing.)
- The upgrade story for the name change rides the existing documented
  behavior (README: restart the daemon after upgrade; `brew services
  restart teton`).

## Open Questions

- [ ] OQ-1: Should the runtime filenames (`tetond.sock`/`.lock`/`.log`)
      eventually follow the rename? Requires a migration story (old daemon
      holding the old lock/socket while a new one claims the new paths —
      single-instance invariant would no longer span versions).
- [ ] OQ-2: Signed/stable executable identity (Developer ID) so Keychain
      grants survive upgrades — overlaps REQ-548's deferred
      provenance/signing finding; should be one combined effort.

## Out of Scope

- Renaming the `tetond` crate, its feature flags, or the runtime filenames.
- A raw-mode/ratatui TUI (the frame's known wrap-over-the-bottom-rule
  limitation on overlong input lines is accepted and documented in
  `prompt.rs` until then).
- Code signing / notarization (OQ-2, deferred with REQ-548's findings).

## Retrieved Context

- LESSON-433 (single-platform verification): Linux CI leg exercised the
  `TIOCGWINSZ` portability claim rather than extrapolating from macOS.
- REQ-544 BR-7 (daemon-side keychain resolution) — the reason the daemon,
  not the CLI, is the name users see in Keychain dialogs.
- REQ-547 consent-flow legibility posture — why permission dialogue stays
  unframed (BR-5).
