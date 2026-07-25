---
id: TASK-016
title: "Homebrew formula template, render script, and the atomic bump-formula release job"
status: draft
parent: REQ-548
created: 2026-07-25
updated: 2026-07-25
dependencies: [TASK-015]
repo: teton-code
---

## Description

The formula source-of-truth (ADR-548-1): a template in this repo rendered
with the tag + per-target sha256s by a `bump-formula` job appended to
`release.yml`. The job clones `atelier-fashion/homebrew-tap` with
`HOMEBREW_TAP_TOKEN`, renders `Formula/teton.rb`, commits, and pushes. A
render or push failure fails the release workflow loudly (BR-4) — a
published Release with a stale formula must be impossible to miss.

## Files to Create/Modify

- `packaging/homebrew/teton.rb.tmpl` — formula template: per-platform `on_macos`/`on_intel`/`on_arm`/`on_linux` url+sha256 slots, installs both binaries, `service do` block running `tetond` foreground with keep_alive and brew var/log paths (BR-6), `test do` asserting `teton --version` matches the formula version
- `tools/release/render-formula.sh` — substitutes version + three (url, sha256) pairs into the template; refuses (exit 64) if any placeholder remains unsubstituted after rendering
- `.github/workflows/release.yml` — append the `bump-formula` job: needs `release`, skipped in dry-run, clones tap with `HOMEBREW_TAP_TOKEN`, runs `brew style --formula` + `brew audit --formula` on the rendered file, commits `teton vX.Y.Z`, pushes; any failure fails the workflow with a named ::error (BR-4). Then a `verify-install` job (macos-15, needs `bump-formula`): `brew install atelier-fashion/tap/teton`, `brew test teton`, `teton --version` equals the tag, `brew services start teton` → `teton doctor` output names the daemon version → `brew services stop teton` — the mechanical evidence for AC-5 and AC-6, against the really-published tap + artifacts
- `docs/homebrew-tap-setup.md` — one-time setup: creating the tap repo, minting the fine-grained PAT (contents:write on the tap only), storing it as the `HOMEBREW_TAP_TOKEN` secret, manual first-render instructions

## Acceptance Criteria

- [ ] `render-formula.sh` with a fake version + three fake sha256s produces a formula that passes `ruby -c` (syntax) when ruby is available, and `brew style`-compatible layout (2-space indent, `class Teton < Formula`)
- [ ] Rendering with a missing substitution exits 64 and names the unfilled placeholder
- [ ] The rendered formula installs BOTH `teton` and `tetond`, declares `service do` with `keep_alive true` and log paths under `var/"log/teton/"`, and its `test do` block checks `teton --version`
- [ ] The `bump-formula` job is `needs: release`, does not run when `dry_run=true`, runs `brew style`/`brew audit` on the rendered formula pre-push, and its failure fails the whole workflow run (no `continue-on-error`)
- [ ] The `verify-install` job installs from the live tap post-bump, passes `brew test teton`, and exercises `brew services start` → `teton doctor` (text assertion) → `stop` (AC-5/AC-6 mechanical evidence)
- [ ] `docs/homebrew-tap-setup.md` documents the PAT scope (tap repo only, contents:write) and the secret name

## Technical Notes

- Template placeholders: `{{VERSION}}`, `{{URL_ARM64_DARWIN}}`,
  `{{SHA_ARM64_DARWIN}}`, `{{URL_X64_DARWIN}}`, `{{SHA_X64_DARWIN}}`,
  `{{URL_X64_LINUX}}`, `{{SHA_X64_LINUX}}` — a trailing `grep -c '{{'` == 0 is
  the refuse-on-unfilled check.
- Formula shape: `on_macos do; on_arm do ... end; on_intel do ... end; end;
  on_linux do; on_intel do ... end; end`. Homebrew on Linux is x86_64-only
  here — no arm64 Linux block (BR-10 honesty: don't stub a target we don't
  build).
- The service block runs `opt_bin/"tetond"` with no args — tetond is
  foreground, resolves `~/Library/Application Support/teton` itself
  (integration probe), so no env/args needed. Second-instance start exits 0
  ("already running"), which brew services tolerates.
- Push with `git -C tap push` over the token remote; do NOT
  `--force` — the tap history is the bump audit trail.
