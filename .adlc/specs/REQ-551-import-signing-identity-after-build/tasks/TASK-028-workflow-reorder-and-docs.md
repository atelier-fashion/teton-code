---
id: TASK-028
title: "release.yml: move the import between build and pack; docs truth-flip"
status: draft
parent: REQ-551
created: 2026-08-01
updated: 2026-08-01
dependencies: [TASK-027]
---

## Description

The workflow half of ADR-551-1: split the darwin "Build and package" step
into "Build (unsigned)" (`package.sh ... build`) and "Sign and package"
(`package.sh ... pack`), move the Import step — content untouched —
between them, and flip the accepted-risk documentation to closed.

## Files to Create/Modify

- `.github/workflows/release.yml` — darwin legs: `package.sh "$TARGET" "$VERSION" dist build` step (no TETON_SIGN_IDENTITY needed) → existing Import step moved verbatim (same name, same body — the ordering assertion keys on the name) → new "Sign and package" step running `package.sh "$TARGET" "$VERSION" dist pack` with the TETON_SIGN_IDENTITY env exactly as today; linux leg keeps one `all` call; update the honest-residual comment near the p12 `rm` (window now excludes all third-party compilation — closed by REQ-551) and any comment stating the import precedes the build; cleanup/smoke/upload untouched
- `docs/release-runbook.md` — §10 residual note flips to closed-by-REQ-551 (one sentence, dated); any keychain-window mention aligned

## Acceptance Criteria

- [ ] actionlint clean; YAML parses; step order on darwin legs is build → import → pack → smoke (AC-1)
- [ ] Import step content is byte-identical to its pre-move body apart from position (diff-verified) — no behavior drift smuggled into the move
- [ ] Linux leg behavior unchanged; dry-run path unchanged (BR-5)
- [ ] No comment anywhere still claims the keychain is open during cargo build (grep "cargo build" near keychain comments)

## Technical Notes

Cleanup already tolerates unset/partial state and reads GITHUB_ENV vars
published at import time — nothing co-moves (integration-explorer
verdict). The env ternaries for TETON_SIGN_IDENTITY/TETON_SMOKE_TEAM_ID
stay on the pack/smoke steps respectively.
