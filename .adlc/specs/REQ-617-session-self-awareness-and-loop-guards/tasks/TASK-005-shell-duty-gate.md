---
id: TASK-005
title: "The shell duty never interprets a failed command, and its prompt stops authorizing instructions"
status: draft
parent: REQ-617
created: 2026-09-04
updated: 2026-09-04
dependencies: []
---

## Description

BR-7 and AC-7/AC-8. Two changes at one seam: the gate inverts (ADR-5) and the
duty prompt loses the clause that authorized *"The agent needs to…"*.

## Files to Create/Modify

- `crates/tetond/src/harness/shell_duty.rs` — `worth_interpreting` becomes
  `!failed && raw > TRIGGER`; `shell_prompt` drops `and what that means for what
  the agent should do next` and gains *"Describe what the output shows. Do not
  tell the agent what to do next."*; the module doc's "when it runs" section is
  rewritten, because it currently names the failed case as primary and would
  otherwise be a paragraph describing behaviour the code no longer has.
- `crates/tetond/src/harness/tools/shell.rs` — emit `shell_duty_skipped` with
  `reason: failed_exit` / `under_size_trigger`.

## Acceptance Criteria

- [ ] AC-7: `shell: cd /nonexistent && pwd` returns exit 1, the raw stderr, the
      `ERROR:` line, and **no** `[shell: …]` prefix; `shell_duty_skipped
      { reason: failed_exit }` is emitted.
- [ ] AC-7 benign half: a **successful** 40 KB output is still interpreted, and
      the duty is still called for it.
- [ ] AC-8: the duty's output over the reference fixtures contains none of
      `should`, `needs to`, `must`, `the agent`.
- [ ] AC-8's mutation: restoring the deleted prompt clause makes the absence
      assertion go red on **at least one** fixture. Recorded in the test's doc
      comment. Without this the assertion is guarding nothing.
- [ ] The module doc no longer describes the failed-command trigger as live.

## Verification

| rule | kind | artifact | benign_path |
|------|------|----------|-------------|
| BR-7 | test-case | `crates/tetond/src/harness/shell_duty.rs::tests::a_failed_command_is_never_interpreted` | yes |
| AC-7 | test-case | `crates/tetond/src/harness/shell_duty.rs::tests::a_failed_command_is_never_interpreted` | yes |
| AC-8 | test-case | `crates/tetond/src/harness/shell_duty.rs::tests::the_duty_never_tells_the_agent_what_to_do` | no |

## Technical Notes

`worth_interpreting` is a `pub const fn` with a two-argument signature that
already carries `failed` — the change is one operator, and its whole risk is
that it is one operator. The mutation in AC-8 and the benign half of AC-7 are
what make the change visible.

ADR-5 records what this costs: a failed `cargo build` with a capped compiler
wall is no longer interpreted. That is BR-7 as written and OQ-3 is where it is
revisited; do not quietly widen the gate here.
