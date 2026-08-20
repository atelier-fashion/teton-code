---
id: TASK-196
title: "Protocol: skills/list, the invocation carrier, the consent subject, SkillInvoked, and a distinct refusal code"
status: draft
parent: REQ-585
created: 2026-08-20
updated: 2026-08-20
dependencies: []
---

## Description

Every wire element REQ-585 adds, in one place, additive, with skew asserted in
both directions and `PROTOCOL_VERSION` unmoved. Also carries the two spec
amendments ADR-10 and ADR-11 identified, because an implementer hitting either
would otherwise have to guess.

## Files to Create/Modify

- `crates/teton-protocol/src/methods.rs` — `SkillsListParams`/`SkillsListResult`, `SkillView`, `SkillSkipped`, `SkillSource`, `impl RpcMethod`; `SkillInvocation`; `PromptTurnParams.skill`
- `crates/teton-protocol/src/events.rs` — `PermissionRequest.subject`, `PermissionSubject`, `Event::SkillInvoked` + its `Event::name()` arm, `DynamicOutcomeView`
- `crates/teton-protocol/src/jsonrpc.rs` — `SKILL_EXPANSION_TOO_LARGE = -32023` inside `application_error_codes!`
- `.adlc/specs/REQ-585-user-defined-slash-commands-from-skill-md/requirement.md` — AC-20(d) and AC-12 amendments

## Acceptance Criteria

- [ ] `SkillsListParams::METHOD == "skills/list"`, pinned by an `assert_eq!` on the constant (the `methods.rs:3775` shape), plus a round-trip test.
- [ ] `PromptTurnParams.skill` is `Option<SkillInvocation>` with `#[serde(default, skip_serializing_if = "Option::is_none")]`. `SkillInvocation { name, raw_arguments }` — `raw_arguments` is the rest of the line **verbatim**, never a re-joined token list (BR-4).
- [ ] `PermissionRequest.subject` is `Option<PermissionSubject>`, additive; `PermissionSubject` is `#[serde(tag = "kind", rename_all = "snake_case")]` with `SkillDynamicContext { skill, source, commands: Vec<String> }` and `#[serde(other)] Unrecognized`. A JSON payload with an unknown `kind` deserializes to `Unrecognized` and **does not error** — pinned, because ADR-7's fail-closed rule needs a variant the client can see.
- [ ] `Event::SkillInvoked { name, source, path_display, body_bytes, ignored_keys, outcomes }` and its `Event::name()` arm; the `name()` table's exhaustiveness test still passes.
- [ ] `SKILL_EXPANSION_TOO_LARGE = -32023` is declared inside the `application_error_codes!` macro so it joins `ALL` and the distinctness guard automatically. Its doc says what separates it from `CONTEXT_LENGTH_EXCEEDED = -32022`: that one means a provider refused a turn it saw; this one means Teton refused to send it.
- [ ] Skew, both directions, for **each** new field — copy `events.rs:3386 route_decided_budget_fields_are_additive_in_both_directions` including its four legs: absent keys parse to the default; an unset value emits **no key**, not `null`; the new wire parses through a locally-declared pre-REQ struct; and the non-vacuity assertion that the fixture really carries the new keys.
- [ ] `PROTOCOL_VERSION` is unchanged — asserted, not assumed.
- [ ] Spec amendment 1: AC-20(d)'s `bound: local_engine` becomes `bound: local engine`. It currently prints `wire_name()` where BR-8(a) and AC-16 require `BudgetBound::words()`.
- [ ] Spec amendment 2: AC-12 gains the body case — a `<tool-result>` planted in the skill **body** (not only in a dynamic command's output) must reach the frame neutralized. See ADR-10.

## Technical Notes

- `SkillView` carries `name`, `source`, `description: Option<String>`, `argument_hint: Option<String>`, `shadowed: Option<String>`. Description and hint are **file bytes**: bound them with `teton_core::session_root::bounded_field` (`crates/teton-core/src/session_root.rs:210`) on the daemon side before they go on the wire, and the client defuses again at render (`Surface::line`). Two layers, each where the frame is authored — ADR-009's shape, and LESSON-517's.
- `SkillsListResult` carries `skipped: Vec<SkillSkipped>` so `/help`'s diagnostic line and BR-10's unknown-command hint read the same list.
- Do **not** add a `PromptBlock` variant. `PromptBlock` is `#[serde(tag = "type")]`; an unknown tag is a deserialization failure, not a degrade, and the invocation is not prompt content.
