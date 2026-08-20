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
- `.adlc/specs/REQ-585-user-defined-slash-commands-from-skill-md/requirement.md` — the five amendments below

## Acceptance Criteria

- [ ] `SkillsListParams::METHOD == "skills/list"`, pinned by an `assert_eq!` on the constant (the `methods.rs:3775` shape), plus a round-trip test.
- [ ] `PromptTurnParams.skill` is `Option<SkillInvocation>` with `#[serde(default, skip_serializing_if = "Option::is_none")]`. `SkillInvocation { name, raw_arguments }` — `raw_arguments` is the rest of the line **verbatim**, never a re-joined token list (BR-4).
- [ ] `PermissionRequest.subject` is `Option<PermissionSubject>`, additive; `PermissionSubject` is `#[serde(tag = "kind", rename_all = "snake_case")]` with `SkillDynamicContext { skill, source, commands: Vec<String> }` and `#[serde(other)] Unrecognized`. A JSON payload with an unknown `kind` deserializes to `Unrecognized` and **does not error** — pinned, because ADR-7's fail-closed rule needs a variant the client can see.
- [ ] `Event::SkillInvoked { name, source, path_display, body_bytes, ignored_keys, outcomes }` and its `Event::name()` arm; the `name()` table's exhaustiveness test still passes.
- [ ] `SKILL_EXPANSION_TOO_LARGE = -32023` is declared inside the `application_error_codes!` macro so it joins `ALL` and the distinctness guard automatically. Its doc says what separates it from `CONTEXT_LENGTH_EXCEEDED = -32022`: that one means a provider refused a turn it saw; this one means Teton refused to send it.
- [ ] Skew, both directions, for **each** new field — copy `events.rs:3386 route_decided_budget_fields_are_additive_in_both_directions` including its four legs: absent keys parse to the default; an unset value emits **no key**, not `null`; the new wire parses through a locally-declared pre-REQ struct; and the non-vacuity assertion that the fixture really carries the new keys.
- [ ] `PROTOCOL_VERSION` is unchanged — asserted, not assumed.
- [ ] `PermissionOutcome` gains `Refused { reason: RefusalReason }` with `NoTerminal` and `UnrecognizedSubject`. Today the only outcomes are `Selected { option_id }` and `Cancelled`, and `Cancelled` already means "the user dismissed the prompt" — it is what EOF on a pipe returns. Without a reason channel the daemon cannot produce AC-9's required placeholder text ("no human could be asked") and would have to conflate it with a decline. Additive, and only ever sent to a daemon that answered `skills/list`.
- [ ] `SkillSkipped.path` is bounded and home-relative on the daemon side, exactly as `SkillView`'s description is. BR-1's entity table says the path is never shown as an absolute path carrying a username into a transcript, and AC-6 puts skipped entries on a user-visible surface.
- [ ] Spec amendment 1: AC-20(d)'s `bound: local_engine` becomes `bound: local engine`. It currently prints `wire_name()` where BR-8(a) and AC-16 require `BudgetBound::words()`.
- [ ] Spec amendment 2: AC-12 gains the body case **and** the command-text case — a `<tool-result>` planted in the skill **body**, and one planted inside a multi-line `` !`…` `` that the fold echoes into a not-run placeholder, must both reach the frame neutralized. See ADR-10.
- [ ] Spec amendment 3: AC-11(a) says a boundary pins the turn "exactly as a `read` of that file would". Per ADR-9 that is literally true for a **project** skill; a user skill outside the root has no repo-relative identity and is pinned by the stricter unknown rule. Name which.
- [ ] Spec amendment 4: BR-2 gains the `skills/`-beats-`commands/` precedence rule for a within-source name collision, which the four globs make reachable and which BR-2 as written does not cover.
- [ ] Spec amendment 5: BR-7's parenthetical that on a boundary-configured machine "all seventeen ADLC skills run on the local tier" is false — seven of them exceed the local budget and are **refused** there per BR-8 and the spec's own Assumptions. It says pinned, not run.

## Technical Notes

- `SkillView` carries `name`, `source`, `description: Option<String>`, `argument_hint: Option<String>`, `shadowed: Option<String>`. Description and hint are **file bytes**: bound them with `teton_core::session_root::bounded_field` (`crates/teton-core/src/session_root.rs:210`) on the daemon side before they go on the wire, and the client defuses again at render (`Surface::line`). Two layers, each where the frame is authored — ADR-009's shape, and LESSON-517's.
- `SkillsListResult` carries `skipped: Vec<SkillSkipped>` so `/help`'s diagnostic line and BR-10's unknown-command hint read the same list.
- Do **not** add a `PromptBlock` variant. `PromptBlock` is `#[serde(tag = "type")]`; an unknown tag is a deserialization failure, not a degrade, and the invocation is not prompt content.
