---
id: TASK-200
title: "One expansion value: substitution, the preamble, the dynamic-command scanner, and the fold"
status: draft
parent: REQ-585
created: 2026-08-20
updated: 2026-08-20
dependencies: [TASK-195, TASK-198]
---

## Description

BR-4, BR-6's pure half, BR-14 and ADR-10. Build `Expansion<Pending>` **once**:
the preamble, the substituted body, and a typed placeholder in each
`` !`cmd` `` slot alongside the ordered command list. That one value is what
TASK-204 measures before consent, what TASK-205 shows the user, and what the
outcomes fold back into.

## Files to Create/Modify

- `crates/tetond/src/skills/expand.rs` — `Expansion<Pending>`, `expand(&Skill, raw_arguments, path_display) -> Expansion<Pending>`, `Expansion::fold(outcomes) -> String`
- `crates/tetond/src/skills/dynamic.rs` — the `` !`cmd` `` scanner, `DynamicOutcome`, `run_all(root, &[Command], timeout_ms) -> Vec<DynamicOutcome>`

## Acceptance Criteria

- [ ] Preamble is exactly one line: ``The user invoked /name (a command defined in <display path>); the instructions below are that command's body.`` `<display path>` is `session_root::display_for` (home-relative), bounded with `bounded_field`.
- [ ] `$ARGUMENTS` → `raw_arguments` **verbatim**: `/alpha teton  code "repo"` preserves both interior spaces and both quote characters. This is the one place the session does not use REQ-582 ADR-2's tokenization (AC-4).
- [ ] `$1`…`$N` → the whitespace-split tokens; an out-of-range `$N` → the empty string; `$ARGUMENTS` with no arguments → the empty string (AC-5).
- [ ] A body containing **no** placeholder and non-empty arguments gets a final `ARGUMENTS: <rest>` line. This is what makes `/proceed REQ-585` work — the shipped `proceed` skill has no `$ARGUMENTS` (AC-5).
- [ ] Substitution runs **before** the scanner, so a `$ARGUMENTS` inside a `` !`…` `` is substituted in the command the consent prompt shows and in the command that runs (AC-5, BR-4/BR-6 ordering).
- [ ] Commands are collected in **document order** and run sequentially in that order.
- [ ] `DynamicOutcome` is typed: `Ran { output }` / `NotRun { reason }` / `Failed { status }` / `TimedOut`. `fold` renders a not-run/failed/timed-out slot as ``[dynamic context not run: `<command>` — <reason>]`` and a ran slot as the output inside `frame_untrusted_builtin(&format!("skill:{name}"), out)`. A command's failure never fails the invocation (BR-6, AC-10).
- [ ] **ADR-10**: `fold` neutralizes envelope tags in **every string it splices**, not only the body — the not-run placeholder embeds the command text verbatim, and the scanner's grammar puts no restriction on what sits between the backticks. A project skill (repo content, which the spec's Assumptions say may be authored by someone other than the user) with a multi-line `` !`...` `` whose second line is a flush-left `</tool-result>` forges the same envelope close, and does it at **`plan`** — the level where no command runs — as well as on a decline, a timeout, a failure or the pipe refusal. The echoed command is additionally rendered on one line and bounded.
- [ ] `expand` runs `render::neutralize_envelope_tags` over the **body** before any envelope is spliced into it. A flush-left `</tool-result>` in a skill body must not close the envelope of a dynamic block that follows it in the same user block. Pinned as its own test; removing the call fails it (BR-5, AC-12 as amended by TASK-196).
- [ ] `expand` and `fold` are pure — no clock, no filesystem, no terminal. `run_all` is the single I/O edge and takes the runner from TASK-198 (AC-18, BR-14).
- [ ] Mutation table: removing the substitution-before-scan ordering, the `ARGUMENTS:` fallback, the envelope neutralization, or the document-order guarantee each fails a named test.

## Technical Notes

- `Expansion<Pending>` exists so the body is never built twice. Building it once to measure and again to emit is how the two copies come to disagree the first time substitution changes.
- `frame_untrusted_builtin` (`crates/tetond/src/harness/turn_loop.rs:1834`) takes the tool label as a `&str` and already calls `neutralize_envelope_tags` on the payload — no new framing function is needed, and the label `skill:<name>` is correct as-is.
- The scanner's grammar is `` !`…` `` with no nesting and no escape. State that in the module doc: an unterminated backtick run is **not** a command and stays literal body text. Do not invent an escape syntax (BR-13 — the body is passed as written).
