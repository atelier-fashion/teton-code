---
id: TASK-195
title: "Skill registry: the four globs behind a recording lister, and a total frontmatter parser"
status: complete
parent: REQ-585
created: 2026-08-20
updated: 2026-08-20
dependencies: [TASK-197]
---

## Description

The pure half of BR-1, BR-2 and BR-5: build a `SkillRegistry` from exactly four
directory listings, one level deep, behind a seam that records every path
opened (AC-7), and parse `SKILL.md` frontmatter with a deliberately narrow flat
parser that either succeeds whole or skips the file with a named reason.

No daemon, no terminal, no clock. Everything here is unit-testable — the registry half of AC-18 and BR-14.

## Files to Create/Modify

- `crates/tetond/src/skills/mod.rs` — `Skill`, `SkillSource`, `SkillRegistry`, `Skipped { path, reason }`, `SkipReason`, `permission_key_for`, `SKILL_MAX_BYTES`, `MAX_ENTRIES_PER_ROOT`, name validation
- `crates/tetond/src/skills/discovery.rs` — `DirLister` trait, `RealFs`, `discover(home, session_root, root_kind, &dyn DirLister) -> SkillRegistry`
- `crates/tetond/src/skills/frontmatter.rs` — `parse(text) -> Result<Parsed, Malformed>`
- `crates/tetond/src/lib.rs` — `pub mod skills;` in alphabetical position (between `session_root` and `sessions`)
- `crates/tetond/tests/skills_discovery.rs` — fixture-driven suite (real temp dirs, symlinks, EPERM)

## Acceptance Criteria

- [ ] `discover` opens **only** `<home>/.claude/skills`, `<home>/.claude/commands`, `<root>/.claude/skills`, `<root>/.claude/commands`, plus one `SKILL.md`/`*.md` read per registered entry. Asserted through a `RecordingFs` that captures every path passed to `list` and `read` — the recorded set is compared for **equality**, not containment (AC-7).
- [ ] A root that is itself a symlink is followed; an **entry** that is a symlink is skipped with reason `symlink not followed`. Fixture: `~/.claude/skills` → a sibling dir, containing `alpha/SKILL.md`, `nested/deep/…`, and `link → /`. `/` is never listed (AC-7, BR-1).
- [ ] `RootKind::Home` (the session root resolves to `$HOME`) skips project discovery entirely; each skill registers once, as `user` (AC-3).
- [ ] Project beats user on a name collision; the loser is retained with `SkipReason::Shadowed` and is listed, not dropped (AC-3, BR-2).
- [ ] **Within one source, `skills/` beats `commands/`.** `~/.claude/skills/status/SKILL.md` and `~/.claude/commands/status.md` are a legal pair the four globs both reach: same name, same source, and the same permission key `skill:user:status`. Without a rule the registry holds two `/status` rows, `/help` lists it twice, REQ-555's "one spelling reaches one handler" is violated, and a remembered grant authorizes whichever file happened to win — moving silently to the other if the winner ever changes. `skills/` wins; the `commands/` entry is listed as shadowed. Its own fixture and test (BR-2, amended by TASK-196).
- [ ] Name validation is `^[a-z0-9][a-z0-9_-]{0,63}$` against the **directory name** (skills) or **file stem** (commands). A frontmatter `name` that differs is recorded as a note and creates no second spelling (BR-2).
- [ ] Every not-registered entry is counted and named: `unreadable (permission denied)`, `over 64 KiB (N B)`, `not UTF-8`, `malformed frontmatter`, `invalid name`, `symlink not followed`, `shadowed by <what>`, `root truncated at 512 entries`. A missing directory and a directory with no `SKILL.md` produce **no** diagnostic (AC-6, BR-1).
- [ ] Frontmatter: no leading `---` ⇒ whole file is the body, zero ignored keys. Unterminated block, an indented continuation, a nested block or a list item ⇒ `Malformed`, file skipped whole — never half-parsed. `name`/`description`/`argument-hint` read; every other key lands in `ignored_keys` (BR-5, AC-13).
- [ ] Entries are **sorted by file name** before `MAX_ENTRIES_PER_ROOT` applies and before the registry is ordered, so behaviour does not depend on APFS vs ext4 listing order (LESSON-540).
- [ ] EPERM leg: a `chmod 000` fixture directory yields `unreadable (permission denied)` and no panic. The test **skips itself when running as root** (`libc::geteuid() == 0`), with the skip stated in the test body.
- [ ] Mutation table in the task's completion note: removing the entry-symlink check, removing the sort, removing the size cap, and removing the `home`-root de-dup each make a named test fail.

## Technical Notes

- Module goes at `crates/tetond/src/skills/`, not under `harness/` — `sessions.rs`, `server.rs` and `runtime.rs` are all callers.
- `DirLister::list` must surface `DirEntry::file_type()` (`lstat` semantics, does **not** follow). Reuse `harness::tools::skip_symlink_entry` (`crates/tetond/src/harness/tools/mod.rs:353`) so the predicate keeps one home — do **not** re-spell `is_symlink()`.
- Do **not** reach for `walk::visit` (`crates/tetond/src/harness/tools/walk.rs:256`). It is a recursive driver, and its `WalkBudget` (100,000 entries / 10 s) would turn AC-7's fixture from a reach test into a budget test. See ADR-4.
- `fs_util::read_regular_file_bounded` (`crates/tetond/src/fs_util.rs:28`) returns `None` for every failure. Discovery needs `EPERM` told apart from missing / oversize / non-UTF-8, so `DirLister::read` returns a typed error. Either widen `fs_util` with a `_typed` sibling or keep the typed read local to `skills` — do not silently duplicate the bounded-read logic.
- Frontmatter parser: copy the posture of `teton_core::config::parse_search_auth` (`crates/teton-core/src/config.rs:551`) — `Option`/`Result` return, no half-parse, and its doc comment's argument for the narrowness. There is **no YAML crate in the workspace** and this is not the REQ that adds one.
- Fixture style: hand-built under `/tmp` with a short name and a `Drop` cleanup (`crates/tetond/tests/e2e/harness.rs:474 Workspace::new`); symlinks via `std::os::unix::fs::symlink` as in `crates/tetond/tests/symlink_posture.rs:78-160`.
- The "root followed, entry not" rule is *narrower* than the walker's blanket refusal, so it gets its own pin in `crates/tetond/tests/symlink_posture.rs` rather than riding an existing walker test.
- **Sequenced behind TASK-197 on purpose.** TASK-197 changes `CarriedTurn::begin`'s signature across six production and three test call sites. Parallel implementers share one worktree (LESSON-541), so a concurrent `tetond` task would see a workspace that does not compile through no fault of its own. TASK-197 is not a functional dependency — it is a compile-stability one, and it is the only such edge in this REQ.
