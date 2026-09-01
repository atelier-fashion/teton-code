---
id: TASK-355
title: "Prove the advisory travels in-band only, and close BR-4's upstream half"
status: draft
parent: REQ-607
created: 2026-08-31
updated: 2026-08-31
dependencies: [TASK-353, TASK-354]
repo: teton-code
---

## Description

Two things that can only be checked from outside the tool.

**AC-4 (BR-3).** Drive a real prompt turn that dispatches the failing `shell`
call, subscribe to the session's event bus, drain it, and assert **no published
event describes the withheld set** — no count, no name, no withheld-shaped
payload. Asserted over the envelopes the call actually publishes, **not** by
grepping for a type name a rename would silently defeat. REQ-596 OQ-1 settled
that this event is not emitted; this is what keeps the two mechanisms distinct in
fact rather than in prose.

**AC-5 (BR-4).** In the same binary, with `auth_ref = "env:MY_LLM_CRED_SENTINEL"`
configured and a failing command, assert the rendered advisory names neither that
variable nor any other name read from the live environment.

**AC-13 (BR-4's upstream half).** REQ-596's BR-5 already carries the dated
amendment naming this REQ. Its **Status** bullet says REQ-607 is `draft` and that
BR-5 is enforced as originally written until it ships — which stops being true the
moment this PR merges. Update it.

## Files to Create/Modify

- `crates/tetond/tests/shell_env_advisory.rs` — new integration binary, AC-4 and AC-5
- `.adlc/specs/REQ-596-credential-safe-shell-environment/requirement.md` — BR-5's Status bullet
- `.adlc/context/architecture.md` — the Key Patterns addition proposed in this REQ's architecture.md

## Acceptance Criteria

- [ ] AC-4: the drained event stream carries no event naming `SSH_AUTH_SOCK`, no
      count of withheld variables, and no withheld-shaped payload, asserted by
      serializing each published envelope and searching **its content**
- [ ] AC-4 is non-vacuous, on both halves: the drain is asserted **non-empty**
      (events really were published for this turn), and the tool result is
      asserted to **carry** the advisory (so the absence in events is a contrast,
      not an artefact of nothing having happened)
- [ ] AC-5: the rendered advisory contains neither `MY_LLM_CRED_SENTINEL` nor any
      variable name taken from the daemon's live environment
- [ ] AC-13: REQ-596's BR-5 Status bullet reflects that REQ-607 has shipped and
      that the narrowed rule is the one now enforced
- [ ] `.adlc/context/architecture.md` carries the new Key Patterns bullet
- [ ] `cargo test -p tetond --test shell_env_advisory` passes

## Verification

| rule | kind | artifact | benign_path |
|---|---|---|---|
| BR-3 | test-case | `crates/tetond/tests/shell_env_advisory.rs` — `the_advisory_rides_the_tool_result_and_no_event_describes_the_withheld_set` | no |
| BR-4 | test-case | `crates/tetond/tests/shell_env_advisory.rs` — `the_advisory_names_no_variable_read_from_the_live_environment` | no |
| BR-4 | test-case | `crates/tetond/src/harness/tools/shell.rs` — `a_failing_ssh_command_names_teton_and_the_key_that_admits_the_agent` | yes |
| AC-4 | test-case | `crates/tetond/tests/shell_env_advisory.rs` — `the_advisory_rides_the_tool_result_and_no_event_describes_the_withheld_set` | no |
| AC-5 | test-case | `crates/tetond/tests/shell_env_advisory.rs` — `the_advisory_names_no_variable_read_from_the_live_environment` | no |
| AC-13 | structural-check | `.adlc/specs/REQ-596-credential-safe-shell-environment/requirement.md` — BR-5 amendment + Status bullet | no |

## Technical Notes

**Model the harness on `crates/tetond/tests/remote_loop.rs`**, which already has
every piece: `ScriptedSseTransport` scripting a `shell` tool call, a real
`EventBus`, `bus.subscribe(256)`, and a `collect_events` drain helper with a
50 ms timeout. Integration binaries share no modules in this workspace, so the
scripted vendor is copied — a smaller copy, with only the verbs these two claims
need.

**AC-4's assertion must be over serialized content.** Serialize each
`EventEnvelope` to JSON and search the string for `SSH_AUTH_SOCK` and for a
withheld-count field. Matching on `Event` variant names would be exactly the
type-name grep the AC rules out.

Note the one structural fact that makes AC-4 satisfiable at all, in the doc
comment: `SessionUpdatePayload::ToolCall` carries a `title` and
`ToolCallUpdate` carries only a status — **no event carries tool result
content**. If that ever changes, this test is the thing that should notice.

**BR-4's benign path is the row above marked `yes`, and it is not ceremony.** BR-4
is a rule about what may *not* be named; validated only against forbidden names,
an implementation that names nothing at all would pass every one of its
assertions and fail BR-1 silently. AC-1's test is the case where the rule
*permits* naming, and it is what stops that.

`.adlc/` edits do not need to be a separate commit from the test, but the
architecture.md bullet should be worded exactly as this REQ's `architecture.md`
proposes it, so the two do not drift on day one.
