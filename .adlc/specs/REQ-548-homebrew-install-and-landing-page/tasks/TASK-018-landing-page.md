---
id: TASK-018
title: "The tetoncode.ai landing page: static, dependency-free, version-injected"
status: complete
parent: REQ-548
created: 2026-07-25
updated: 2026-07-25
dependencies: []
repo: teton-code
---

## Description

A single static page: what Teton Code is (the two product promises — cost
control and privacy boundaries — in the README's voice), the one-command
install, and honest platform notes (BR-10). No framework, no build system —
one HTML file with inline CSS, plus a tiny render script that stamps the
current release version and install command into placeholders at deploy time
(BR-8: the page derives from release metadata, never hand-edited).

## Files to Create/Modify

- `site/index.html` — the page: hero (name, one-line pitch, install command in a copyable block), the two promises, how-it-works (base camp / summits metaphor from the README), platform honesty section (macOS arm64 Metal; macOS x86_64 + Linux x86_64 CPU-only; model downloads on first run with consent, size shown before download), footer (GitHub, MIT). `{{VERSION}}` and `{{INSTALL_COMMAND}}` placeholders; light/dark via `prefers-color-scheme`
- `site/render.sh` — replaces placeholders from `$1` (version) into `site/dist/index.html`; exits 64 if placeholders remain after render (same refuse-on-unfilled shape as the formula render)
- `.gitignore` — add `site/dist/`

## Acceptance Criteria

- [ ] `site/index.html` is fully self-contained (no external scripts, fonts, or stylesheets; no analytics), renders legibly with CSS disabled, and contains the literal install command `brew install atelier-fashion/tap/teton` only via the `{{INSTALL_COMMAND}}` placeholder path
- [ ] `bash site/render.sh 0.1.0` produces `site/dist/index.html` with zero remaining `{{` placeholders and the version visible on the page; rendering with a missing arg exits 64
- [ ] The page's claims match BR-10: CPU-only stated for x86_64 targets, Windows absent, first-run model download with consent and size named — no capability the shipped binaries lack
- [ ] `python3 -c "import html.parser"`-based well-formedness check (or `tidy` if available) passes on the rendered output
- [ ] `site/dist/` is gitignored; the unrendered template is what's committed

## Technical Notes

- Keep copy short and factual — pull phrasing from README.md's "What it is"
  and the two promises; the honesty section mirrors the consent-flow posture
  (REQ-547): the page says what happens on first run, before it happens.
- Placeholders use the same `{{NAME}}` convention as the formula template so
  the two render scripts share the refuse-on-unfilled idiom.
- No JS needed; the copy button can be a `<button onclick>`-free `<code>`
  block (selectable text) to keep the page CSP-trivial for any GCP surface.
