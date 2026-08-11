---
id: TASK-008
title: "The shipped CLI renders a consent request, takes a decision, and answers"
status: pending
parent: REQ-570
created: 2026-08-11
updated: 2026-08-11
dependencies: [TASK-004]
---

## Description

Gap 2, BR-4. Nothing in `crates/teton` sends `session/attach` or
`attach/consent`; the CLI renders an incoming consent request as a notice it
cannot act on, so today **every** consent path ends in the 30-second timeout —
and REQ-569's own acceptance evidence for the grant flow leans on a test-harness
`with_auto_consent` capability no shipped client has. The tested flow and the
shipped flow diverge until this lands.

## Files to Create/Modify

- `crates/teton/src/session_ui.rs` — render the request, take a decision,
  trigger the presence prompt, send `attach/consent`.

## Acceptance Criteria

- [ ] AC-4: the CLI renders an incoming consent request, takes a decision, and
      sends `attach/consent`.
- [ ] AC-4: it **never** auto-answers — asserted, including that a
      **non-interactive** invocation does not auto-approve. It declines and says why.
- [ ] AC-3: a user resuming their own session in a fresh CLI succeeds with
      exactly **one** visible consent step, end to end through the shipped
      client — no test-harness auto-consent anywhere in the path.

## Technical Notes

- Render through the existing `Surface`/`Prompter` seams (ADR-007's rule: the
  future ratatui front-end inherits both by implementing the same seams).
- Note the deliberate asymmetry with `teton --yes`: `--yes` is consent to *this
  user's own* pending action and carries **no** authority to admit a different
  connection into a session. Do not wire `--yes` into this path.
