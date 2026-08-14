---
id: TASK-134
title: "Docs: README web-setup section, manual-verification steps"
status: draft
parent: REQ-572
created: 2026-08-13
updated: 2026-08-13
dependencies: ["TASK-132"]
---

## Description

User-facing documentation for the new surface, and the manual gates for the
model-behavior legs automation cannot pin.

## Files to Create/Modify

- `README.md` — a "Turning on web lookup" section: `/web setup` walkthrough, the `[web]` table for hand-editing, keychain-reference rule, `search_auth` template with the Brave/Kagi/SearxNG examples (mirrors the bundled guide — the AC-8 contract tests are the consistency anchor), and the off-by-default posture sentence.
- `docs/manual-verification.md` — REQ-572 section: (1) live AC-1 probe — fresh config, ask a web-needing question, expect the refusal to name web lookup and `/web setup` with zero tool calls (the BUG-160 verification pattern); (2) AC-9 dedup — ask two web-needing questions, expect the second refusal to reference, not repeat; (3) real-keychain flow once on macOS — store, commit, `security find-generic-password -s teton -a web-search` exists, abort path removes it.

## Acceptance Criteria

- [ ] README section exists and the commands/keys in it appear verbatim in the bundled guide (drift check note pointing at the AC-8 enumeration helper)
- [ ] manual-verification steps are copy-pasteable and name their expected outputs
- [ ] Both docs state the user-only rule: the model can name the opt-in but only the user can run it

## Technical Notes

Keep README additions inside the existing "bring your own models" narrative
flow (BUG-160 added the provider commands there — this extends the same
section family, not a new top-level).
