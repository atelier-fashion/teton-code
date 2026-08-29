---
id: BUG-187
title: "A project skill's `path_display` is absolute when the session root is outside `$HOME`, so the working tree's path rides the wire event and the prompt"
status: resolved
severity: medium
created: 2026-08-21
updated: 2026-08-21
component: "daemon/harness"
domain: "clients"
stack: ["rust", "daemon", "json-rpc"]
concerns: ["privacy", "correctness"]
tags: ["skills", "req-585", "path-display", "session-root", "skill_invoked", "skills-list", "privacy"]
---

## Description

REQ-585 BR-1's entity table promises a skill's path is *"shown home-relative
(the `SessionRoot.display` convention), never as an absolute path that carries
a username into a transcript or a remote payload"*, and `SkillInvoked
.path_display`, `SkillSkipped.path` and `Skill::path_display`'s doc all
restated it. The daemon delivered it with `teton_core::session_root::
display_for`, which can only shorten a path **under the home directory**.

A **project** skill's path is `<session-root>/.claude/{skills,commands}/…`. When
the session root is not under `$HOME` — a CI workspace, a checkout on an
external volume, a `/tmp` fixture, `/srv/src/app` — nothing strips, and the
promised relative spelling came back as the full absolute path:

```
/tmp/tc-4f2a/proj/.claude/skills/validate/SKILL.md
```

That value reached three places: BR-12's `skill_invoked` event (every attached
client, every transcript), the `/verbose` detail line, and — the one that
leaves the machine — **BR-4's preamble**, which is prompt text and therefore
part of every remote payload the turn produces.

The guarantee held for user skills throughout (`~/.claude/skills/x/SKILL.md`),
which is why no test caught it: `skill_turn.rs` asserted the exact `~/…` value
for the user skill and only `ends_with(".claude/skills/reported/SKILL.md")` for
the project one, and `skills_list_contracts.rs` pinned the wire spelling of a
**user** skip only. The project fixtures in both binaries live under `/tmp`,
outside the fixture `HOME` — so the defect was being exercised on every run and
asserted by nothing.

## Impact

Privacy, not correctness of dispatch: the absolute path of the user's working
tree (and, for a repo under a home directory on another machine's layout, a
username) is written into the session transcript and into remote prompt
payloads for any repository checked out outside `$HOME`. No boundary is
crossed that the turn was not already free to cross, and nothing is
misrouted — but the entity table's guarantee simply did not hold there, and a
guarantee that holds only for repos in the home directory is not the one BR-1
states.

Pre-existing since REQ-585 merged (`76fa8f4`, 2026-08-20); not in a tagged
release (user is on 0.1.23).

## Root cause

The display rule needs three inputs — the skill's **source**, the session root
and `HOME` — and it was applied at the *surfaces*, where at most one of them
was in hand. `runtime::accept_invocation` had `HOME` and the path; `server::
skills_list_result` had `HOME` and answers from a **stored registry snapshot**
with no session root at all. So "home-relative" was not a choice among rules,
it was the only rule reachable from where it was being applied.

## Fix

- `teton_core::session_root::display_under(path, base, home)` — the second half
  of the rule: the part below `base` when the path is under it, falling through
  to `display_for` otherwise. Pure, component-wise, `.` for the base itself.
- The base is chosen by the skill's **source**, in `skills::roots` where that is
  known: the session root for a project skill, `None` (→ `~/…`) for a user
  skill. Deliberately *not* "whichever prefix matches" — a session rooted at an
  ancestor of `$HOME` (`/`, `/Users`) would then spell a user skill
  `Users/jane/.claude/…`, reintroducing the username the rule exists to remove.
- Derived **once, at discovery** and carried on the row (`Skill::path_display`,
  `Skipped::path_display`), which is the only place all three inputs exist.
  `Skill::path` stays absolute — it is the local-only fact the expander opens
  and the provenance mint resolves against (ADR-9).
- Bounding is unchanged and still at the surfaces (`bounded_field`,
  `DISPLAY_MAX_CHARS`), and `SkillInvoked` still never carries the body.

BR-1's entity table and ADR-15 are amended to state the two-half rule rather
than the half that was implementable from the surfaces.

## Tests

- `skills_discovery.rs::a_project_skill_outside_home_is_spelled_relative_to_the_session_root`
- `skills_discovery.rs::a_skipped_project_entry_is_spelled_relative_to_the_session_root`
- `skills_discovery.rs::a_user_skill_under_a_session_root_that_contains_home_is_still_spelled_from_home`
- `skills_list_contracts.rs::a_skipped_project_entry_crosses_the_wire_relative_to_the_session_root`
- `skill_turn.rs::the_invocation_event_carries_what_the_daemon_read_off_the_file`
  — the project assertion is now an equality, not an `ends_with` that an
  absolute path satisfies
- `teton-core::session_root::tests::display_under_*` (5)

## Found

REQ-587 (model-invoked skills), while writing a `cli_e2e` `/verbose` leg,
2026-08-21. Pre-existing; deliberately not fixed there to keep that REQ scoped.
