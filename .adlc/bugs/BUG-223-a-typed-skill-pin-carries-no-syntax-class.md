---
id: BUG-223
title: "A typed `/skill` pin carries no syntax class, while the same preamble through the model's door does"
status: open
severity: low
created: 2026-09-09
updated: 2026-09-09
component: "tetond/harness/provenance"
domain: "privacy"
stack: ["rust", "daemon"]
concerns: ["developer-experience", "privacy"]
tags: ["shell-provenance", "skill-provenance", "session-pinned", "unknown-shell", "reach-reason", "req-614", "req-619", "req-620"]
introduced_by: ["REQ-620"]
attribution: manual
---

## Description

REQ-620 BR-6 made the `unknown` verdict name the syntax class that refused a
command — one content-free sentence per class — and carried it to
`session_pinned.reason` and the CLI's pin notice, so a user (and a model) can
see *which byte to change* rather than only that something was refused.

It reaches one of the two doors. A `shell` call and a **model-invoked** skill
enter context as `Provenance::Tool` blocks carrying a `ToolProvenance` that
holds the reason, so their pins render it. A **typed** `/skill` enters as
`Provenance::User`, which carries three bits — `sources`, `unknown`,
`boundary_touch` — and no reason field, so the fold can only contribute
`ToolProvenance::unknown()`. The session still pins, with the same cause
(`unknown_shell`), the same liftability and the same `/shell allow` remedy; the
notice is the pre-REQ-620 sentence with no class.

The class is not lost from the turn: `skill_invoked.outcomes[].reach_reason`
names it for every preamble (REQ-619 BR-7), and that surface is published by the
same turn. What is inconsistent is the **pin notice**, which is where a user
looks after being moved to the local tier, and which now answers differently
depending on which door the identical preamble came through.

## Reproduction Steps

1. On a machine with any privacy boundary configured (the builtin set suffices),
   start a session on a remote route.
2. Type a `/skill` whose preamble carries a quoted string — e.g.
   ``!`echo "hello"` `` — so the classifier refuses it with the `quote` class.
3. Read the pin notice the CLI prints.
4. Invoke the same skill from the model instead (or run the same command through
   the `shell` tool) and read that notice.

## Expected Behavior

Both notices name the class: `cause: unknown_shell — the command uses a quoted
string this classifier does not model. \`/shell allow\` lifts it …`

## Actual Behavior

The model-invoked (and `shell`) notice names the class. The typed `/skill`
notice stops at the cause. `skill_invoked.outcomes[].reach_reason` on the same
turn carries the sentence the notice omitted.

## Environment

- Platform: macOS and Linux (not platform-specific)
- Version: the release carrying REQ-620 (branch
  `feat/REQ-620-harmless-redirects-in-the-shell-grammar`)

## Root Cause

The seam is the shape of `Provenance::User`
(`crates/tetond/src/harness/context.rs:288`): `sources: BTreeSet<ProvenanceId>`,
`unknown: bool`, `boundary_touch: bool`. REQ-619 gave it three fields for
reasons that still hold (an empty set means *ordinary typed text*, and a
boundary touch decides the pin's permanence), but none of them can hold an
`Option<&'static str>` reason.

`context_provenance`'s `User` arm in
`crates/tetond/src/harness/completion.rs` (the `CtxProvenance::User` match arm)
records the gap deliberately in a comment — "REQ-620 BR-6: no class,
deliberately" — because closing it inside REQ-620 would have meant a fourth
field on `User` threaded through the three seams REQ-619 ADR-619-3 pins
(dropped-block absorb, the context-provenance union, replay), each of which
needs its own test (LESSON-501, LESSON-502).

Two candidate fixes, neither chosen here:

- **Widen the type.** `Provenance::User` gains `unknown_reason:
  Option<&'static str>` beside the bit it explains (ADR-620-4's own rule,
  LESSON-653), written by the skill fold that already produces `reach_reason`,
  and re-asserted at all three seams.
- **Read the other surface.** The notice for a typed `/skill` pin is composed
  from `skill_invoked.outcomes[].reach_reason` instead, which would leave the
  wire shape alone but give the notice two sources for one sentence.

Severity is low because nothing about the *pin* differs — same cause, same
permanence, same lift — only the sentence that explains it.

## Resolution

(filled after fix)

## Files Changed

(filled after fix)
