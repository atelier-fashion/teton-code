---
id: TASK-351
title: "The diagnosis table, the SSH_AUTH_SOCK constant, and the opt-in allowlist function"
status: complete
parent: REQ-607
created: 2026-08-31
updated: 2026-08-31
dependencies: []
repo: teton-code
---

## Description

Give `child_env.rs` the three static pieces the rest of the REQ reads (ADR-A,
ADR-B), and bring its rejection table into step with them (AC-12).

1. `pub(crate) const SSH_AUTH_SOCK: &str = "SSH_AUTH_SOCK";` — one spelling,
   used by both the allowlist function and the diagnosis table so the flag and
   the sentence cannot come to disagree about which variable is at stake (BR-6).
2. `WithheldVar { name, programs, opt_in_key }` and
   `WITHHELD_DIAGNOSED: &[WithheldVar]` — one row today: `SSH_AUTH_SOCK`,
   programs `ssh`/`git`/`scp`/`sftp`/`rsync`, `opt_in_key:
   Some("[shell] allow_ssh_agent")`. `opt_in_key` is `Option` because BR-1's
   config-key clause is conditional: a future row for a variable with no key
   must not invent one.
3. `shell_env_allow(allow_ssh_agent: bool) -> Vec<&'static str>` — `SHELL_ENV_ALLOW`
   plus, when the flag is on, `SSH_AUTH_SOCK`. Nothing else, ever.

Then update the rejection table's `SSH_AUTH_SOCK` row (AC-12) so it no longer
reads "rejected, full stop", and add a derived check that every
`WITHHELD_DIAGNOSED` name appears in that doc table — BR-4 makes the doc table
the nameable set, so a code table that drifts from it would name something BR-4
does not license.

## Files to Create/Modify

- `crates/tetond/src/child_env.rs` — the constant, `WithheldVar`, `WITHHELD_DIAGNOSED`, `shell_env_allow`, the amended rejection-table row, and the derived check

## Acceptance Criteria

- [ ] `shell_env_allow(false)` equals `SHELL_ENV_ALLOW`; `shell_env_allow(true)`
      equals it plus `SSH_AUTH_SOCK` and nothing else, asserted as a set
      difference in both directions
- [ ] `SHELL_ENV_ALLOW` itself is **unchanged** — REQ-596 AC-3.2's membership
      assertion must stay green (the opt-in adds to a copy, never to the constant)
- [ ] The rejection table's `SSH_AUTH_SOCK` row names `[shell] allow_ssh_agent`
      (AC-12)
- [ ] A derived check asserts every `WITHHELD_DIAGNOSED` name occurs in the
      rejection table's source text, with the corpus cut at the first column-0
      `#[cfg(test)]` and a vacuity floor asserting it saw at least one name
- [ ] `cargo test -p tetond --lib child_env` passes

## Verification

| rule | kind | artifact | benign_path |
|---|---|---|---|
| BR-4 | test-case | `crates/tetond/src/child_env.rs` — `every_diagnosable_name_is_in_the_documented_rejection_table` | no |
| BR-5 | test-case | `crates/tetond/src/child_env.rs` — `the_opt_in_adds_one_name_to_the_allowlist_and_no_other` | no |
| BR-6 | test-case | `crates/tetond/src/child_env.rs` — `the_opt_in_adds_one_name_to_the_allowlist_and_no_other` | no |
| AC-12 | structural-check | `crates/tetond/src/child_env.rs` — `every_diagnosable_name_is_in_the_documented_rejection_table` | no |

## Technical Notes

**The corpus cut is not optional** (conventions.md, REQ-600). The derived check's
own patterns include the literal `SSH_AUTH_SOCK`, which appears in its own test
module — without cutting at the first column-0 `#[cfg(test)]` the check matches
itself and its vacuity floor can never fire.

`shell_env_allow` returns an owned `Vec` rather than a `&'static [&str]` because
the allowlist is now a function of a runtime bool. That is one small allocation
per spawn, beside a process spawn — not a cost worth a `Cow` for.

Do **not** add `SSH_AUTH_SOCK` to `SHELL_ENV_ALLOW` behind a `cfg` or any other
conditional. The constant is REQ-596 BR-2.1's recorded set and REQ-596 AC-3.2
asserts its literal membership; the opt-in is an addition at the call site.
