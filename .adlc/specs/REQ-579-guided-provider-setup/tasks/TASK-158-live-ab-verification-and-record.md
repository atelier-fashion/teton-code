---
id: TASK-158
title: "Verification record: live A/B for the model hand-off (AC-1) + REQ verification.md"
status: complete
parent: REQ-579
created: 2026-08-15
updated: 2026-08-15
dependencies: ["TASK-156", "TASK-157"]
---

## Description

AC-1 is a claim about what the local model *says*, and only a live run can make it. Mirror REQ-577's verification: build with `--features tetond/llama`, run an isolated daemon (short `XDG_RUNTIME_DIR`, weights symlinked — see the project memory "Testing a Teton daemon in isolation"), and in a fresh session ask "set up Kimi for deep reasoning" three times against the new build and three against the pre-REQ binary (`main`). Record verbatim replies. Pass = every new-build reply names `/provider setup` (any accepted vendor spelling — ADR-2) and none contains `teton provider add`/`teton policy set-tier` as the primary instruction. Write `.adlc/specs/REQ-579-*/verification.md` with the transcript excerpts, the build SHAs, and the pass/fail per round. If the local model or weights are unreachable in the pipeline environment, record **unrun** with the reason — never assume.

**Covers:** AC-1 (live A/B) and the AC-1..14 accounting table

## Files to Create/Modify

- `.adlc/specs/REQ-579-guided-provider-setup/verification.md` — new; the A/B record (3+3 rounds, verbatim), build SHAs, environment, and the AC checklist (AC-1 through AC-14) with each marked pass / covered-by-test `<test name>` / unrun-with-reason
- `.adlc/knowledge/assumptions/ASSUME-008-front-door-questions-bypass-the-docs-tool.md` — append a dated observation line: whether the model answered the hand-off from the guide (expected) or reached for `teton_docs`

## Acceptance Criteria

- [ ] `verification.md` exists with 3+3 verbatim rounds or an explicit `unrun` + reason
- [ ] Every AC-1..14 row is accounted for by name (a test name, a manual pass, or unrun)
- [ ] ASSUME-008 has the new observation

## Technical Notes

REQ-577's `verification.md` is the format. Do not count a reply that says both "run `/provider setup`" and "or `teton provider add`" as a fail — BR-1 allows naming the CLI as the non-interactive alternative; the fail is reciting the CLI as *the* instruction with no hand-off. If the daemon cannot load the model within ~3 minutes, stop and record unrun.
