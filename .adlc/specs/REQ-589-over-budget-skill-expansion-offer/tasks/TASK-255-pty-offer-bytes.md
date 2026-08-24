---
id: TASK-255
title: "PTY leg pinning the offer's rendered bytes"
status: draft
parent: REQ-589
created: 2026-08-24
updated: 2026-08-24
dependencies: [TASK-251]
---

## Description

AC-14 / BUG-191. Assert what the terminal actually prints, not what a structure says it would print.

## Files to Create/Modify

- `crates/teton/tests/pty_e2e.rs`

## Acceptance Criteria

- [ ] The offer's question, figures, and remedy line are asserted against the transcript verbatim
- [ ] Answering each of the four options completes the turn as expected
- [ ] Uses `wait_for` polling with the existing deadline — never a fixed sleep (LESSON-450)

## Technical Notes

`the_acknowledgment_prompt_names_the_root_its_skills_and_what_it_left_out` (pty_e2e.rs:1521) is the pattern: script the local engine via TETON_LOCAL_SCRIPT, wait for the rendered marker, assert `.contains` on the transcript.
