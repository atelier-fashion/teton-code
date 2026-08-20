---
id: TASK-209
title: "Say what it does and what it does not: the docs topic, the README, the runbook, the caveat"
status: complete
parent: REQ-585
created: 2026-08-20
updated: 2026-08-20
dependencies: [TASK-205, TASK-208]
---

## Description

BR-13's fidelity statement and AC-20's by-hand runbook. The honest half matters
most: `/proceed` and `/sprint` expand and then **stall** at their first "invoke
the skill" step until the two Deferred follow-ups land, and that is recorded as
evidence, not hidden.

## Files to Create/Modify

- `crates/tetond/src/harness/docs/skills.md` — new `teton_docs` topic
- `crates/tetond/src/harness/tools/docs.rs` — the `TOPICS` row, `TOPIC_INDEX` entry, and the tool description
- `README.md` — the session command table and the "Two limits" paragraph (`:119-124`)
- `CHANGELOG.md` — `## [Unreleased] → ### Added`
- `docs/manual-verification.md` — the AC-20 runbook, legs (a)–(f)
- `.adlc/context/architecture.md` — Key Patterns

## Acceptance Criteria

- [ ] The new topic fits under `MAX_TOPIC_BYTES` (4,096) and the `TOPIC_INDEX`/`TOPICS` agreement test passes. Note the ceiling pressure: `context.md` is at 4,056 of 4,096 and `providers.md` at 4,076 — BR-8's refusal prose belongs in the **new** topic, not squeezed into either.
- [ ] The `teton_docs` tool description is at its 120-char ceiling as recorded; a sixth topic name buys its place by shortening the sentence in front of the index, not by moving the ceiling.
- [ ] README's "Two limits" paragraph states the whitespace-split/no-quotes rule as universal, which BR-4 makes false for skill rows. Qualify it there as TASK-206 qualifies `ARGUMENT_FOOTER` — the two statements must agree.
- [ ] CHANGELOG entry is upgrade-relevant: a machine with `~/.claude/skills` gains commands it did not have, and dynamic context asks at `guarded`.
- [ ] AC-20 runbook, all six legs, with the provider window used recorded (`max_context = 1000000` from the shipped Kimi recipe, or a hand-lowered `128000` — say which). Leg (c) records the exact step at which `/proceed REQ-585` stalls.
- [ ] Leg (f) and OQ-8's residual are stated plainly **and correctly**: on a boundary-configured machine every skill that runs a dynamic command is **pinned** to the local tier — which for seven of the seventeen ADLC skills means **refused** there, not run, because they exceed the local budget (BR-8 and the spec's own Assumptions). BR-7's parenthetical says "run"; TASK-196 amends it, and this runbook must not restate the wrong version. The consent offers no "run without dynamic context" in v1.
- [ ] OQ-7's residual is stated: project skills get no separate trust acknowledgment in v1; at `guarded` every command is shown and asked every time.
- [ ] `.adlc/context/architecture.md` Key Patterns gains three entries: user-authored prompt text as a first-class provenance source; a remembered grant scoped by source as well as name; and bounded discovery applied to a second, non-recursive lister.

## Technical Notes

- BR-13 is a documentation requirement with teeth. Do not translate Claude Code tool names, and do not rewrite a body's references to `Agent`/`Task`/`Skill`/`Workflow` — the caveat is the deliverable.
- The headroom table in `docs/manual-verification.md` was re-measured by TASK-199; do not re-measure it here, cite it.
