---
id: TASK-206
title: "classify takes the snapshot, built-ins match first, and /help grows a section that cannot contradict its footer"
status: complete
parent: REQ-585
created: 2026-08-20
updated: 2026-08-20
dependencies: [TASK-196]
---

## Description

BR-2, BR-3 and BR-10 in `slash.rs`. The rule that makes reserved names win is
**structural**: a built-in match returns before the snapshot is consulted, so a
skill can only be reached by a name no built-in claims.

## Files to Create/Modify

- `crates/teton/src/slash.rs` — `classify(input, registry)`, `Input::Skill`, `SkillSnapshot`, `reserved_names()`, `render_help`'s skills section, `ARGUMENT_FOOTER`, the unknown-command skipped case
- `crates/teton/src/cli_rows.rs` — the bundled-guide pin at `:1831` if BR-9's wording moved it

## Acceptance Criteria

- [ ] `classify(input: &str, registry: &SkillSnapshot) -> Input<'_>` stays pure and total. Order inside: `//` escape → `cli_line` (REQ-582's `teton …`) → `split_name(rest, COMMANDS)` → the snapshot. `Resolution`/`resolve` stay built-in-only and keep returning `&'static CommandSpec`.
- [ ] Nothing is leaked to satisfy a lifetime. A leaked registry survives `/cd` and would dispatch a skill the session no longer has.
- [ ] `reserved_names()` is **derived** from `COMMANDS` — every spelling (name + aliases), plus the first word of every multi-word row — plus `teton`. Never hand-listed. Its own test asserts the derivation matches what `classify` actually does, in both directions (LESSON-546).
- [ ] AC-2: fixture skills named `cost`, `exit`, `provider` and `teton` are listed as shadowed and never dispatch. `/cost`, `/exit`, `/provider list` and a typed `teton provider list` are byte-identical to today.
- [ ] An **empty** registry renders no section at all — no header, no `0 skills` line. That is the default state of every user with no `~/.claude`, and the state ADR-2 produces against an old daemon where `/help` is claimed to be byte-for-byte what it is today. Its own test.
- [ ] AC-1: with a non-empty registry, `/help` order: built-in rows → blank → `skills — arguments are passed through as typed:` → one row per skill (`/name [hint] — description (user|project)`, shadowed marked) → the diagnostic line (`N skills (user A, project B); M skipped: …`) → blank → `ARGUMENT_FOOTER` → `ESCAPE_FOOTER`.
- [ ] `ARGUMENT_FOOTER` (`slash.rs:134`) is qualified to name the built-in rows it describes. **Append** the qualification rather than rewriting the subject: `crates/teton/tests/cli_e2e.rs:4993` asserts the substring `Command arguments are split on whitespace and quotes are not interpreted`, so prefixing it (`Built-in command arguments…`) breaks a test in a file TASK-208 is separately editing. It currently says arguments split on whitespace and quotes are not interpreted, which BR-4 makes false for skill rows; unqualified, it would sit two lines from the rows it contradicts.
- [ ] `help_renders_every_table_row_and_the_escape_footer` (`:2849`) is **widened, not relaxed**: the `COMMANDS.len() + 2` count and the row zip are re-scoped to the built-in **prefix slice**; `ESCAPE_FOOTER`-last (`:2880`) and `ARGUMENT_FOOTER`-second-last (`:2884`) keep asserting over the **whole** rendered list, or a skills section could slip below them.
- [ ] `help_family` (`:1304`) never sees skills. A skill named `provider` must not re-group the four built-in `/provider` rows.
- [ ] BR-3's both-directions pin, scoped to **unshadowed** rows: every name the snapshot classifies as `Input::Skill` appears as a `/help` row, and every **unshadowed** `/help` skill row classifies as `Input::Skill`. BR-3's claim is about *dispatchable* skills; an unqualified pin is false for the shadowed rows AC-2's fixture mandates, and the likely repair for a red test is to relax the pin rather than scope it (LESSON-524).
- [ ] AC-17: `/analyze` with a **skipped** `analyze` entry prints the reason; with no entry at all prints the pre-REQ ``unknown command: `/analyze` `` bytes plus `HELP_HINT`, unchanged. A *shadowed* name never reaches the hint — the built-in or project skill runs — so there is no shadow branch.
- [ ] Description and hint are rendered through `Surface::line` (`defused`); they arrive already bounded from the daemon (TASK-203).
- [ ] The ~40 `classify(...)` call sites in `slash.rs`'s test module are updated to pass an empty snapshot, and `a_line_not_opening_with_a_slash_is_a_byte_identical_prompt` (`:2775`) and `the_double_slash_escape_collapses_only_the_leading_pair` (`:2790`) stay green unmodified in substance.
- [ ] Mutation table: consulting the snapshot before `split_name`, hand-listing the reserved set, and dropping the footer qualification each fail a named test.

## Technical Notes

- `split_name` (`:1123`) is typed `&'static [CommandSpec]` and yields `&'static str` spellings; a `String`-backed skill name does not fit. Match built-ins first and check the snapshot in a separate pass — do not widen `split_name`.
- `ECHO_MAX_CHARS = 40` (`:1209`) bounds an echoed name while BR-2 allows 64. AC-17's "pre-REQ bytes unchanged" therefore holds for names under 40; note the residual rather than changing the constant.
