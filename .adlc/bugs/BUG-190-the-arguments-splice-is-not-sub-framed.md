---
id: BUG-190
title: "A `$ARGUMENTS` splice puts the caller's bytes inside the region the frame certifies as instructions"
status: open
severity: medium
created: 2026-08-22
updated: 2026-08-22
component: "daemon/harness"
domain: "harness"
stack: ["rust", "daemon"]
concerns: ["security", "privacy"]
tags: ["skills", "skill-tool", "prompt-injection", "frame", "envelope", "arguments", "req-587-residual"]
---

## Description

REQ-587 BR-4's frame vouches for a skill body as instructions to follow. The
`ARGUMENTS:` **trailer** — the path 16 of the 17 shipped ADLC skills take — is
wrapped in `<skill-arguments>` and the closing sentence vouches only for *the
file's own text*.

The **splice** is not. A body that names `$ARGUMENTS`/`$N` has the caller's bytes
written into it verbatim and unmarked, inside the region the frame certifies.

## Reproduction Steps

1. Install a user skill whose body interpolates, e.g. `Scope: $ARGUMENTS`.
2. Have the model invoke it with `args` carrying instruction-shaped text.
3. Read the rendered frame: the injected text sits inside `<skill-body>` with no
   marker separating it from the file's own prose.

## Expected Behavior

Caller-supplied bytes are marked as data wherever they land, as the trailer's
are.

## Actual Behavior

They are indistinguishable from the file's text inside the vouched region. The
outer sentence scopes the vouch, which limits the harm, but nothing marks *which*
bytes are the file's.

## Root Cause

Three mechanisms defeat drawing the marker at the splice, all properties of the
pipeline rather than of the renderer — established by prototype, not argument:

1. Both `<skill-arguments` spellings are in `render`'s `UNTRUSTED_ENVELOPE_TAGS`,
   so `defuse` `_`-prefixes any flush-left occurrence in the string the expander
   returns — **including the expander's own marker**. Exempting the pair from
   that pass hands the caller a forgeable `</skill-arguments>`, the one close
   whose forgery puts the rest of a payload back under the outer frame's
   sentence.
2. A flush-left marker at a mid-line splice means injecting newlines into the
   file's prose, and every shipped skill that names `$ARGUMENTS` names it
   mid-line, several inside code spans.
3. Substitution runs **before** `dynamic::scan` by design (BR-4 precedes BR-6),
   so injected newlines would land inside an interpolating `` !`cmd` `` — and
   which `$` sites are command-interior is not decidable at substitution time,
   because an argument can introduce the `` !` `` opener itself.

## Resolution

Move the sub-framing to a stage that knows both the line structure and the
command spans. **Do not "just exempt the marker"** — mechanism 1 is the reason
that is worse than the disease.

## Files Changed

- `crates/tetond/src/skills/expand.rs` — reasoning recorded on `substitute`'s doc
- `crates/tetond/src/harness/tools/skill.rs` — `SkillFrame`, the trailer's sub-frame
- Recorded in `.adlc/specs/REQ-587-model-invoked-skills/requirement.md` Deferred
