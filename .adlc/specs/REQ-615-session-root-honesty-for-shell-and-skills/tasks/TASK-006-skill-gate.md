---
id: TASK-006
title: "A skill that needs a project is refused outside one, not expanded"
status: draft
parent: REQ-615
created: 2026-09-04
updated: 2026-09-04
dependencies: [TASK-001]
---

## Description

BR-5, plus the `known_projects` plumbing both refusals need (architecture ADR-4,
ADR-5).

## Files to Create/Modify

- `crates/tetond/src/skills/frontmatter.rs` — optional `requires: project`.
- `crates/tetond/src/harness/tools/skill.rs` — `Refusal::NeedsProject`, the gate
  in `invoke`, the event publish.
- `crates/tetond/src/harness/tools/mod.rs` — `ToolContext::with_known_projects`.
- `crates/tetond/src/runtime/turn.rs` — feed the ranked list to the tool context
  from the same expression that feeds the prompt.

## Acceptance Criteria

- [ ] A skill declaring `requires: project` in frontmatter, invoked at a `Home`
      or `FilesystemRoot` root, is refused with reason `needs_project`.
- [ ] A skill with **no** such key whose `!cmd` preamble references `.adlc/` is
      refused the same way — detection is `skills::dynamic::scan` over the body,
      checking the scanned **command texts**, never the prose.
- [ ] A skill whose *prose* mentions `.adlc/` but whose commands do not is **not**
      refused (the benign path that separates a scanner from a substring search).
- [ ] **No preamble command is executed** on the refusal path: assert by a
      preamble whose effect is observable (it creates a marker file) and inspect
      that the marker does not exist.
- [ ] The refusal message names the root display, the kind, and lists the known
      projects each with `/cd <name>`, bounded at REQ-583's ceiling.
- [ ] `skill_refused_needs_project` is published with the spec's payload.
- [ ] **No model turn is spent**: the refusal returns before `acknowledge_project`
      and before `expand_and_fold`, so no expansion, no consent round-trip, no
      fold.
- [ ] At a `Project` root and at a `Plain` root, every shipped ADLC skill expands
      exactly as today (OQ-2 resolved; BR-9) — the existing skill-expansion tests
      are not edited and still pass.
- [ ] `cargo test -p tetond` passes.

## Verification

| rule | kind | artifact | benign_path |
|------|------|----------|-------------|
| BR-5 | test-case | `crates/tetond/src/harness/tools/skill.rs::a_project_needing_skill_is_refused_at_a_home_root` | yes |
| BR-5 | test-case | `crates/tetond/src/harness/tools/skill.rs::the_adlc_token_is_read_from_commands_not_from_prose` | yes |
| BR-5 | test-case | `crates/tetond/src/harness/tools/skill.rs::the_refusal_runs_no_preamble_command` | no |
| BR-9 | test-case | `crates/tetond/src/harness/tools/skill.rs::a_plain_root_still_expands_a_dot_adlc_skill` | yes |
| AC-4 | test-case | `crates/tetond/src/harness/tools/skill.rs::the_refusal_runs_no_preamble_command` | yes |

## Technical Notes

Gate position in `invoke` is load-bearing: **after** `resolve_for_model` (the
body must be known to be scanned) and **before** `acknowledge_project`. Put it
there and say why in the comment, so a later edit that moves it has to argue
with the reason.

`Refusal::NeedsProject { name, root_display, root_kind, known_projects }` follows
the existing `Refusal` shape — a `reason()` arm returning `"needs_project"` and a
`message()` arm composing the sentence. The roster of known projects rides the
refusal value rather than being read at render time, so the message and the event
carry one list.

`with_known_projects` is a builder beside `with_denied_prefix` for the same
reason: every existing `ToolContext::for_root` call site must keep compiling with
an empty list, which is the honest value for a context nobody gave projects to.
