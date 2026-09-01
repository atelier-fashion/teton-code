---
id: TASK-353
title: "Carry the withheld fact out of the spawn and word it on the failing tool result"
status: draft
parent: REQ-607
created: 2026-08-31
updated: 2026-08-31
dependencies: [TASK-351, TASK-352]
repo: teton-code
---

## Description

BR-1, BR-2, BR-3 — the advisory itself.

**`run_bounded` carries the fact.** It composes the child environment, so it is
the only place that knows what the child actually lacked. It computes, for each
`WITHHELD_DIAGNOSED` row: present in `std::env::vars()` **and** absent from the
composed map (ADR-A predicate 2). That set rides out on
`BoundedRun::Completed { withheld, .. }`.

**`render_output` words the sentence.** The tool's presentation applies the other
two predicates — non-zero exit (BR-2) and a command-position program match
(ADR-A) — and appends one sentence naming Teton and, when `opt_in_key` is
`Some`, the config key; otherwise the rejection table in `child_env.rs`.

This split is conventions.md's "Compose the sentence where the facts are" rule,
and the reason it matters here is BR-2's other half: the skill path's dynamic
context is `run_bounded`'s second caller and must **not** be annotated. It reads
`Completed` and ignores `withheld`, exactly as it already ignores the timeout
consent hint.

Also switch the shell call site from `SHELL_ENV_ALLOW` to
`shell_env_allow(policy.allow_ssh_agent)`.

## Files to Create/Modify

- `crates/tetond/src/harness/tools/shell.rs` — `withheld` on `BoundedRun::Completed`, the command-position matcher, the advisory composer, the `render_output` call, and the `shell_env_allow` call site
- `crates/tetond/src/skills/dynamic.rs` — destructuring updated for the new field; no behaviour change

## Acceptance Criteria

- [ ] AC-1: a failing `git push` in a temp repo with no remote, with
      `SSH_AUTH_SOCK` planted in the daemon environment and the flag off,
      produces a `ToolOutcome` whose **`content`** names Teton and
      `allow_ssh_agent`. Asserted on the rendered result, not a log line or an
      internal type (LESSON-519)
- [ ] AC-2: the same fixture with command `exit 1` carries no advisory
- [ ] AC-3: the same fixture with command `true` carries no advisory
- [ ] With the flag **on**, the failing `git push` carries no advisory — the
      variable was not withheld, so there is nothing to explain
- [ ] The skill path's dynamic context is unchanged: no advisory appears in a
      skill's inlined command output
- [ ] AC-6: the mutation is **run** and recorded in the test's doc comment with
      the count and text of what went red
- [ ] `cargo test -p tetond --lib` passes

## Verification

| rule | kind | artifact | benign_path |
|---|---|---|---|
| BR-1 | test-case | `crates/tetond/src/harness/tools/shell.rs` — `a_failing_ssh_command_names_teton_and_the_key_that_admits_the_agent` | no |
| BR-2 | test-case | `crates/tetond/src/harness/tools/shell.rs` — `an_unrelated_failure_carries_no_advisory` | yes |
| BR-2 | test-case | `crates/tetond/src/harness/tools/shell.rs` — `a_successful_command_carries_no_advisory` | yes |
| AC-1 | test-case | `crates/tetond/src/harness/tools/shell.rs` — `a_failing_ssh_command_names_teton_and_the_key_that_admits_the_agent` | no |
| AC-2 | test-case | `crates/tetond/src/harness/tools/shell.rs` — `an_unrelated_failure_carries_no_advisory` | yes |
| AC-3 | test-case | `crates/tetond/src/harness/tools/shell.rs` — `a_successful_command_carries_no_advisory` | yes |
| AC-6 | test-case | `crates/tetond/src/harness/tools/shell.rs` — doc comment on `an_unrelated_failure_carries_no_advisory` | no |

## Technical Notes

**The AC-6 mutation, concretely**: delete the exit-code and program-match
predicates so the sentence is appended to every `Completed` result. AC-2 and AC-3
must go red. Record the actual count and text — do not predict it (LESSON-598:
re-run the mutation after any change to program structure; a guard that has
stopped covering its subject looks exactly like a guard that passes).

**The fixture must plant `SSH_AUTH_SOCK`** or predicate 2 is false and AC-1 is
vacuously red. Use the `ENV_MUTATION` mutex already in this module's tests to
serialize the `set_var`, and a sentinel value (`/tmp/SENTINEL-agent-<pid>.sock`)
per AC-11 / LESSON-497.

**`git push` with no remote exits non-zero locally and touches no network.** Do
not build a fixture that needs an ssh server. If `git` is absent from the CI
image, the test must skip loudly with a named reason rather than pass silently.

**Command-position matching** — split on `|`, `;`, `&&`, `||`, `(`, newline; take
each segment's first word; strip its directory prefix; compare. Document the two
false-negative limits (`xargs`/`sudo`/script indirection, and substitutions) in
the function's doc comment. False negatives are the safe direction for BR-2.

Adding a field to `BoundedRun::Completed` is a breaking change for its two
callers only. Do **not** give it a `Default` — the required-field-with-no-Default
pattern (architecture.md) is what makes "the path forgot to compute it" a compile
error rather than a silent empty set.
