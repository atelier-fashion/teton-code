---
id: TASK-016
title: "Homebrew formula template, render script, and the atomic bump-formula release job"
status: complete
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

- [x] `render-formula.sh` with a fake version + three fake sha256s produces a formula that passes `ruby -c` (syntax) when ruby is available, and `brew style`-compatible layout (2-space indent, `class Teton < Formula`) — verified locally: `ruby -c` Syntax OK, and `brew style --formula` reported "1 file inspected, no offenses detected"
- [x] Rendering with a missing substitution exits 64 and names the unfilled placeholder — verified for template drift (`{{HOMEPAGE_URL}}`), a missing sha flag, an empty sha value, a non-hex sha, a malformed version, both input modes at once, and a valueless flag; a missing artifact exits 75
- [x] The rendered formula installs BOTH `teton` and `tetond`, declares `service do` with `keep_alive true` and log paths under `var/"log/teton/"`, and its `test do` block checks `teton --version` — verified from Homebrew's own parse, not just the text: the generated launchd plist carries `ProgramArguments=[opt/teton/bin/tetond]`, `KeepAlive=true`, and both log paths under `var/log/teton/`
- [x] The `bump-formula` job is `needs: release`, does not run when `dry_run=true`, runs `brew style`/`brew audit` on the rendered formula pre-push, and its failure fails the whole workflow run (no `continue-on-error`) — the job runs in dry-run mode but renders into a scratch tap and pushes nothing; the clone, the token read, the URL check and the push are all publish-gated (see Deviations)
- [x] The `verify-install` job installs from the live tap post-bump, passes `brew test teton`, and exercises `brew services start` → `teton doctor` (text assertion) → `stop` (AC-5/AC-6 mechanical evidence) — plus `restart`, a stop-actually-stopped assertion, and a BR-6 log-path assertion
- [x] `docs/homebrew-tap-setup.md` documents the PAT scope (tap repo only, contents:write) and the secret name

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

## Deviations from the task spec (implementation findings)

Four, all forced by Homebrew's actual behaviour rather than chosen. Each was
found by running the real commands locally, not by reading docs.

1. **The formula has no `version` stanza.** `brew audit` rejects it:
   `Stable: version 9.8.7 is redundant with version scanned from URL`. The
   audit gate and an explicit version cannot coexist, so `{{VERSION}}` now
   renders into the generated-file header comment and the version itself is
   pinned by the URLs. Because that makes BR-3's third place depend on a
   Homebrew heuristic, the bump job gained a step that asserts
   `brew info --json` resolves the formula at exactly the tag before pushing.
   The scan was checked across 5 versions × 3 targets — all 15 exact.

2. **`bump-formula` runs in dry-run mode instead of being skipped.** The AC's
   intent — a dry run publishes nothing — is preserved exactly: the tap clone,
   the token read, the URL check, the commit and the push are all gated on
   `preflight.outputs.publish`. What a dry run *does* do is render into a
   scratch tap and run the full `brew style` + `brew audit` + BR-3 version
   gate, so a broken template is caught by a dispatch rather than by a tag.
   A skipped job would have proved nothing about the formula.

3. **`bump-formula` runs on `macos-15`, not `ubuntu-24.04`.** `brew style` and
   `brew audit` *are* the gate; putting them on an image where Homebrew is a
   preinstalled afterthought risks the version of this job where brew goes
   missing and a gate that never ran looks exactly like a gate that passed
   (LESSON-443).

4. **Homebrew 6 requires third-party taps to be trusted**
   (`HOMEBREW_REQUIRE_TAP_TRUST` defaults on) — but it treats a fully-qualified
   name on the command line as self-authorizing. `brew install
   atelier-fashion/tap/teton` therefore still needs no extra step and **BR-1
   holds**, which `verify-install` proves by running it as its first action,
   before anything teaches that runner about the tap. Short-name commands
   (`brew services start teton`) do *not* clear the gate, so the job trusts the
   tap before those steps and `docs/homebrew-tap-setup.md` records the fact.
   **This has downstream reach: README and landing-page copy that tells users
   to run `brew services start teton` should mention `brew trust`** (TASK-020 /
   TASK-018 scope, flagged not fixed here).

Also verified beyond the ACs: Homebrew's generated launchd plist for this
formula (`ProgramArguments`, `KeepAlive`, both `Standard*Path` values), and
that a `brew style`/`brew audit` run needs no `brew trust` because the
fully-qualified name is in `ARGV`.
