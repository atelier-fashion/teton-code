---
id: TASK-020
title: "README quick-start (brew one-liner) and the release runbook"
status: complete
parent: REQ-548
created: 2026-07-25
updated: 2026-07-25
dependencies: [TASK-016]
repo: teton-code
---

## Description

Make the front door match the new reality (AC-8): README leads with the brew
one-liner and `brew services start teton`, keeps the source build as the
contributor path, and drops the stale "pre-alpha — product spec stage" line.
A release runbook gives maintainers the tag-to-verified checklist, including
the v0.1.0 first-release specifics and the AC-1/AC-2 per-platform sign-off
template (LESSON-433 posture: unrun legs recorded as unrun).

## Files to Create/Modify

- `README.md` — Install section: brew one-liner, `brew services start teton`, first-run consent note (model downloads after you accept, size shown); "Build from source" subsection retains the `cargo build --workspace --release --features tetond/llama` path with the cmake prerequisite; Status section updated to reflect shipped daemon+CLI reality
- `docs/release-runbook.md` — the release checklist: bump `[workspace.package] version`, tag `vX.Y.Z`, what the workflow does (preflight → builds → smoke incl. seam-refusal gate → Release → formula bump), what to verify after (formula version in tap, `brew upgrade` path from N-1 staged per AC-4), first-release extras (HOMEBREW_TAP_TOKEN secret must exist — bump job fails loudly without it; site deploy blocked until OQ-5 secrets; AC-1/AC-2 clean-machine sign-off blocks with platform + date + name)
- `site/index.html` — one copy addition (scope extended post-TASK-016): the next-steps snippet must account for Homebrew 6 tap trust — short-name commands (`brew services start teton`) require the tap trusted even though the fully-qualified install is self-authorizing
- `.gitignore` — add `dist/` (package.sh's local output dir; flagged by TASK-015)

## Acceptance Criteria

- [x] README's install section shows exactly `brew install atelier-fashion/tap/teton` followed by `brew services start teton` and `teton`; the pre-alpha status line is gone; the source-build path remains under a contributor heading
- [x] README makes no claim the binaries lack (BR-10): platform list matches the release matrix, CPU-only noted for x86_64 targets
- [x] `docs/release-runbook.md` contains the full tag-to-verified checklist and an explicit AC-1/AC-2 sign-off template with "unrun" as the recorded default per platform
- [x] Runbook cross-references docs/homebrew-tap-setup.md (secret) and docs/site-deploy-runbook.md (site) rather than duplicating their steps
- [x] All internal doc links resolve (`grep -o '\[[^]]*\](\.\?/\?docs/[^)]*)' README.md docs/release-runbook.md` targets exist)

## Verification (local, 2026-07-25)

- README install block is the three commands verbatim (`README.md:11-15`); no
  `pre-alpha`/`product spec stage` string survives; source build sits under
  `## For contributors` with the cmake prerequisite named.
- Platform table's target triples diffed against `release.yml`'s build matrix —
  identical set; both x86_64 targets marked CPU-only, Windows unsupported,
  Linux `brew services` explicitly not a v1 claim (OQ-2).
- Link-extraction pass over `README.md` + `docs/release-runbook.md`: 17 internal
  link targets, 0 broken.
- `bash site/render.sh 0.1.0` → exit 0, zero `{{` in the output, and a
  `html.parser` tag-balance check on the rendered page passes.
- `git diff site/index.html` is the four-line trust paragraph and nothing else.
- `.gitignore` already ignored `dist/` (bare pattern, matches at any depth);
  the change annotates it as also covering `package.sh`'s staging dir rather
  than adding a duplicate entry.

## Technical Notes

- Keep the README's mountain-range voice; the install block is the hero, the
  metaphor stays but tightens.
- The consent-flow sentence is load-bearing product honesty (REQ-547 BR-2 /
  this REQ's BR-10): name that the daemon proposes a model with size + RAM
  floor and downloads only after acceptance.
- Do not document `brew services` on Linux as supported (OQ-2 default:
  binaries install; systemd unit is the user's own — one sentence, honest).
