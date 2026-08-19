---
id: REQ-583
title: "Session-root awareness and bounded discovery — the agent knows where it is, the user is told when it is nowhere, and a search cannot become a disk crawl"
status: complete
deployable: true
created: 2026-08-18
updated: 2026-08-19
component: "daemon/harness"
domain: "harness"
stack: ["rust", "daemon", "cli", "json-rpc"]
concerns: ["developer-experience", "security", "reliability", "privacy"]
tags: ["tool-jail", "cwd", "repo-root", "session-root", "system-prompt", "environment-block", "glob", "grep", "shell", "walk-bounds", "tcc", "macos", "banner", "launch", "project-detection", "cd", "re-jail"]
---

## Description

Since BUG-147 every tool a session runs — `read`, `edit`, `glob`, `grep`,
`shell` — is jailed to the **client's working directory** at session creation
(`session/new`'s `cwd`). That fix assumed the working directory is a project.
When it is not, the tools do not fail; they *degrade into a disk crawl* with the
operating system as the only guardrail.

Observed on the user's dogfood of v0.1.22 (2026-08-18, session
`sess-d08dgz…`, verified live via `lsof -d cwd` on the running `teton` /
`teton-code` pair): `teton` was launched from `/Users/<user>` and asked to
*"look in my development folder for the Teton repo"* (the repo lives at
`~/Documents/GitHub/teton-code`). What the user saw was a run of macOS consent
dialogs with no visible pattern — *Media & Apple Music*, then *Photos*, then
*"data from other apps"*, then *Desktop* (granted) — and then no result. The
user's requirement, verbatim: **"We need to improve this user experience."**

Reconstructed from the code, each link in that chain is a defect of its own:

1. **The model is never told where it is.** The system prompt
   (`harness/turn_loop.rs::build_system_prompt`) carries no working directory,
   no "is this a project", no platform. Every tool's one-line doc says
   "repository files" and a jail refusal says only "escapes the repo root" —
   the root is never named. A small local model (the session's later calls
   ran on the local `qwen3-coder-30b-a3b`, per `cost.db`) that is asked about
   "my development folder" while silently jailed to `~` has no fact to reason
   from and no way to say "I can only see under `~`".
2. **The launch says nothing.** The banner prints `cwd: ~` and proceeds. There
   is no signal that the whole home folder just became "the repository", no
   `--cwd`, and no in-session way to move the root; the only recovery is
   quit → `cd` → relaunch.
3. **The walkers are unbounded and one of them cannot find a directory.**
   `glob` and `grep` recurse from the root with no entry budget and no
   wall-clock ceiling; each skips only `.git`, `target`, `node_modules` (two
   private copies of the same list); a `read_dir` error is silently swallowed
   (`Err(_) => return`); and `glob` matches **files only** — a directory named
   `teton-code` can never be returned, so `**/teton-code` answers "no files
   match" while the directory sits right there. A `shell find ~ …` has the
   30 s default timeout, and on macOS every consent dialog *blocks the syscall
   until the user answers* — the command is dead before the user has read the
   third dialog.
4. **The dialogs are the walk order.** APFS returns directory entries in
   hash order, so `~/Music`, `~/Pictures`, `~/Library/Containers`, `~/Desktop`
   arrive in an order that looks random. Because the daemon is now spawned by
   the CLI as a child of the terminal (REQ-565's on-demand lifetime), the
   dialogs name **Terminal**, and a grant made there widens every CLI the user
   will ever run — not Teton (ADR-007's attribution note, one process up).
5. **A privacy consequence the user cannot see.** `local-only` boundaries are
   repo-relative globs anchored at the *session root*
   (`teton_core::boundary` — `secrets/**` matches only paths beginning
   `secrets/`). With the root at `~`, the same file's identity is
   `Documents/GitHub/teton-code/secrets/prod.env`, which the project's boundary
   does **not** cover. Launching from an ancestor of the project silently
   narrows every boundary declared for it. This REQ makes that fact *visible*
   at launch; re-anchoring boundaries is OQ-1.

This REQ is the first of a three-part response (the other two — a project
locator so a "where is X" request never needs a walk, and macOS-consent-aware
walks that ask in Teton's own voice — are separate REQs; see Out of Scope).
Three legs, one thesis — **a session always knows, and can say, what ground it
stands on, and no tool can be made to crawl the disk from there:**

- **Leg A — the agent knows where it is.** Every turn's prompt states the
  session root, what kind of place it is, and the platform; every jail refusal
  names the root.
- **Leg B — the user is told when the root is nowhere.** Launching from the
  home folder, the filesystem root, or a directory that is not a project is a
  first-class, named state with a one-line notice and two remedies
  (`--cwd`, `/cd`) that do not require quitting.
- **Leg C — a search cannot become a disk crawl.** Walking tools find
  directories, run under an entry and wall-clock budget whose exhaustion is
  stated, share one skip set, never enter the user's media and library trees
  from a home-kind root unless named, and report the folders they could not
  read instead of pretending they were empty.

## System Model

### Entities

| Entity | Field | Type | Constraints |
|--------|-------|------|-------------|
| SessionRoot | path | absolute path | required; the tool jail (today's `session_cwd`); validated as `session/new` validates `cwd` today (absolute, exists, is a directory) |
| SessionRoot | display | string | home-relative (`~`, `~/Documents/GitHub/teton-code`) or absolute when not under `$HOME`; the one spelling used by the banner, the notice, the environment block, jail refusals and `/cd` |
| SessionRoot | kind | enum: `project` \| `home` \| `filesystem_root` \| `plain` | derived, never stored: `home` when path == `$HOME`; `filesystem_root` when path == `/`; `project` when the directory holds a project marker; `plain` otherwise |
| SessionRoot | project_name | string? | present iff `kind == project`; the root directory's basename |
| SessionRoot | vcs_branch | string? | present iff the project is a git checkout and the branch can be read without invoking git; absent otherwise (never a guessed value) |
| ProjectMarker | name | string | one closed table of marker names (a VCS directory or a top-level build manifest); the table is the only place the list lives |
| EnvironmentBlock | text | string | pure function of (SessionRoot, platform); bounded — must clear the resident-prompt ceiling with the same headroom rule the self-config guide obeys; contains no file contents |
| WalkBudget | max_entries | integer | > 0; default comfortably covers a large repository (this workspace is ~2.5k entries outside the skip set) and stops a home-folder crawl long before a shell timeout |
| WalkBudget | max_wall | duration | > 0; a walk that outlives it stops and says so |
| WalkOutcome | truncated_by | enum? : `entries` \| `wall_clock` | present when a budget was hit; the tool result states it in words |
| WalkOutcome | unreadable_dirs | list of display paths (capped) + total count | directories whose `read_dir` failed with a permission error; reported, never swallowed |
| WalkSkipSet | names | list of directory names | one definition shared by every walking tool; today's `.git`, `target`, `node_modules` plus platform noise |
| MediaTreeSet | names | list of directory names / bundle suffixes | the user's media and library trees (macOS: `Library`, `Music`, `Pictures`, `Movies`, `.Trash`, `*.photoslibrary`, `*.musiclibrary`); not entered from a `home` or `filesystem_root` kind root unless the pattern's leading literal segment names them |

### Events

| Event | Trigger | Payload |
|-------|---------|---------|
| `session_root_changed` | a successful `/cd` (session root moved on a live session) | session id, previous root display, new root display, new kind, context disposition (blocks dropped — the `context_cleared` shape) |
| (tool result, not a bus event) walk truncated | a `glob`/`grep` hits `max_entries` or `max_wall` | stated in the tool's own result text |
| (tool result, not a bus event) unreadable directories | a walker meets a permission error on `read_dir` | count + up to N display paths in the tool's own result text |

### Permissions

| Action | Roles Allowed |
|--------|---------------|
| read the environment block | every session (it is prompt content) |
| `teton --cwd <path>` | the launching user (a client-side flag; the daemon validates the path exactly as it validates `session/new` `cwd`) |
| `/cd <path>` | the client that owns the session, at **every** permission level including `plan` — it moves the jail, it does not mutate files (informed by LESSON-524) |

## Business Rules

_Leg A — the agent knows where it is_

- [ ] BR-1: **Every turn's system prompt carries an environment block.** The
  block states, as plain facts and in this order: the session root's display
  path; its kind in words a user would use ("a project", "your home folder",
  "the filesystem root", "a directory that is not a project"); for a project,
  its name and — when readable without invoking git — its branch; and the
  platform (macOS / Linux). It is one block of bounded size, resident in every
  turn, and it names *what is*, never what the model should do about it — a
  small model transfers data reliably and directives unreliably (informed by
  LESSON-532, LESSON-493). Adding it must not push the resident prompt past
  the same ceiling-with-headroom the self-config guide is tested against
  (informed by BUG-160, LESSON-493).
- [ ] BR-2: **A jail refusal names the root.** Every "outside the jail"
  refusal from any tool reads, in one shape: the caller's own path, the words
  "is outside the session root", and the root's display path — and nothing
  else (no directory listing, no suggestion of other paths). The root display
  is a value the model already holds from BR-1, so naming it leaks nothing new.
- [ ] BR-3: **One term for the jail.** Tool descriptions, jail refusals, the
  launch notice, and `/cd` all call it the *session root*, unconditionally —
  tool descriptions do not vary with the root's kind. Only the environment
  block (BR-1) and the notice (BR-5) describe *what kind* of place the root is,
  and "project"/"repository" appears there only when the kind is `project`.

_Leg B — the user is told when the root is nowhere_

- [ ] BR-4: **Root kind is derived from one marker table.** `home` (== `$HOME`),
  `filesystem_root` (== `/`), `project` (the directory contains a name from the
  ProjectMarker table), else `plain`. The table is the single source for
  "what makes a directory a project" and is exercised by name in tests.
- [ ] BR-5: **A non-project root is announced at launch, once, in one line.**
  When the kind is not `project`, the CLI prints one notice under the banner
  that (a) names the root, (b) states the consequence in the user's terms
  ("tools are scoped to your whole home folder — every search walks all of
  it, and privacy boundaries declared for a project do not apply here"), and
  (c) names both remedies: `teton --cwd <path>` and `/cd <path>` from inside
  the session. It is a notice, not a gate: the session proceeds. The
  notice's *content* is a pure function of the SessionRoot, unit-tested with
  no TTY; only its bytes are TTY-gated, and non-interactive output stays
  byte-identical (informed by LESSON-481's pattern via REQ-560 BR-8; ADR-007's
  TTY clause).
- [ ] BR-6: **`--cwd <path>` sets the session root without moving the shell.**
  A relative path resolves against the shell's working directory; `~` expands.
  The daemon validates it exactly as it validates `session/new` `cwd` today
  (absolute after resolution, exists, is a directory) and a refusal naming
  the path and the reason is printed before any session output — never a
  session that starts and then fails on every tool (informed by BUG-147). The
  banner's `cwd:` line shows the **session root**, not the shell's directory,
  whenever the two differ.
- [ ] BR-7: **`/cd <path>` moves a live session's root.** Same path grammar
  and the same validation as BR-6 (one grammar, two spellings — informed by
  REQ-582's one-grammar rule). On success the transcript states the new root
  and its kind, the next turn's environment block reflects it, and the
  session's conversation context is **cleared** and reported in the existing
  `context_cleared` shape — because every carried block's provenance identity
  is relative to the root it was minted under, and a carried identity judged
  under a new root names a different file. On refusal, the root is unchanged
  and the daemon's reason is printed. `/cd` with no argument prints the
  current root and kind. Available at every permission level.
- [ ] BR-8: **A moved root re-announces.** After `/cd`, the BR-5 notice fires
  again if the new kind is not `project` — the user who `/cd`s to `~` gets the
  same one line they would have gotten at launch.

_Leg C — a search cannot become a disk crawl_

- [ ] BR-9: **`glob` finds directories.** A directory whose root-relative
  path matches the pattern is listed (distinguishably from a file, e.g. a
  trailing `/`) and tagged in the outcome's provenance exactly as a matched
  file would be — the listed name and the tagged identity remain one value
  (REQ-571 BR-1/BR-2). A `**/teton-code` from an ancestor root returns the
  directory. A directory reached through a symlink stays skipped (REQ-571
  BR-5 is unchanged: walkers never follow links). What boundary verdict a
  bare directory identity carries is OQ-7.
- [ ] BR-10: **Every walk runs under a budget, and exhaustion is stated.**
  `glob` and `grep` stop at `max_entries` visited or `max_wall` elapsed,
  whichever first, and the result then ends with a line saying which budget
  stopped it and what to do ("narrow the pattern, or move the session root
  with `/cd`"). A budget hit is never a silent partial: the same words appear
  whether zero or many matches were found before the stop. Existing result
  caps (200 matches) stay as they are and are reported as they are today.
- [ ] BR-11: **One skip set, one media set, one budget — defined once.**
  `glob` and `grep` (and any future walker) read the same WalkSkipSet,
  MediaTreeSet and WalkBudget; there are no private copies. A test proves the
  two tools agree by reading the shared definition, not by comparing two
  lists.
- [ ] BR-12: **From a home-kind root, the media and library trees are not
  entered unless named.** When the session root's kind is `home` or
  `filesystem_root`, a walk does not descend into a MediaTreeSet directory
  **sitting directly under a user's home directory** (macOS: `~/Library`,
  `~/Music`, `~/Pictures`, `~/Movies`, `~/.Trash`; from `/` the same names
  under each `/Users/<name>/`), nor into a media *bundle* by suffix at any
  depth (`*.photoslibrary`, `*.musiclibrary`) — *unless* the pattern's leading
  literal segments name that tree (`Library/**/*.plist` from `~` enters
  `~/Library`). The position matters: a `Library/` inside
  `~/Documents/GitHub/my-app/` is project content and is walked. Those
  top-level trees hold no source, and on macOS they are exactly the ones the
  OS gates behind a consent dialog; not entering them is what keeps a search
  from raising a dialog the user cannot connect to anything they asked for.
  From a `project` or `plain` root the rule is inert.
- [ ] BR-13: **Unreadable directories are reported, never swallowed.** A
  `read_dir` that fails with a permission error is counted and the result
  ends with "N folder(s) could not be read (permission denied): a/, b/, …"
  (display paths, capped at a handful, then "and N more"). On macOS the line
  adds that the OS may have blocked access to that folder or be waiting on a
  consent dialog for it. Other `read_dir` errors (vanished directory, I/O)
  keep today's skip-and-continue behaviour but are counted in the same line.
- [ ] BR-14: **A shell timeout from a home-kind root hints at the dialog.**
  When a `shell` command is killed at its deadline *and* the session root's
  kind is `home` or `filesystem_root`, the timeout message the model receives
  appends one sentence: on macOS a consent dialog for a protected folder
  holds the command until it is answered — narrow the command to a project
  path or move the session root. From a `project` root the message is
  unchanged (no noise where the cause is implausible).

## Acceptance Criteria

_Leg A_

- [ ] AC-1: For a session created with `cwd = <tmp>/repo` where `<tmp>/repo`
  contains a `.git` whose `HEAD` names branch `main`, the built system prompt
  contains an environment block stating the root display, the word
  "project", the name `repo`, the branch `main`, and the platform. The block
  is asserted by content, not by position (informed by LESSON-482's
  regression-test-the-load-bearing-clause rule).
- [ ] AC-2: With `cwd = $HOME` the block says the root is the user's home
  folder; with `cwd = /` the filesystem root; with a marker-less directory,
  "not a project". No branch is stated for any of these (never a guessed
  branch — BR-1).
- [ ] AC-3: With a `.git/HEAD` that is a detached SHA or unreadable, the block
  states the project and omits the branch; the prompt still builds.
- [ ] AC-4: The resident-prompt ceiling test that today bounds the self-config
  guide also passes with the environment block present for a 200-character
  root path; the assertion is on the same ceiling and headroom, not a new,
  looser one.
- [ ] AC-5: `read` of `../outside.txt` and `edit` of `/etc/hosts` from a
  jailed context each yield an error containing the caller's path, the words
  "outside the session root", and the root display; a test asserts the two
  errors share one shape.
- [ ] AC-6: No tool description or jail refusal contains the word
  "repository" (a rendered-output assertion over the registry's docs and the
  enumerated refusal shapes); the notice and the environment block never
  render `project <name>`/`repository` for a non-project root. (Reworded at
  verify: the mandated notice opens "Not inside a project" and the plain
  kind's phrase is "not a project", so "contains the word only for a project
  root" could not hold as written — the invariant is that a non-project root
  is never *called* a project.)

_Leg B_

- [ ] AC-7: The kind derivation returns `home`, `filesystem_root`, `project`,
  `plain` for the four fixture roots, and `project` for a directory holding
  each name in the ProjectMarker table (the test enumerates the table — BR-4).
- [ ] AC-8: The notice content function returns `None` for a `project` root
  and, for each other kind, one line naming the root display, the consequence,
  and both `--cwd` and `/cd`. The TTY-gated printer emits it under the banner
  on a TTY; the non-TTY byte stream is unchanged (a snapshot/parity test in
  the style of the existing banner tests).
- [ ] AC-9: `teton --cwd <abs>` creates a session whose tools resolve paths
  under `<abs>` (a `read` of a file that exists only there succeeds);
  `--cwd rel` resolves against the shell cwd; `--cwd ~/x` expands; `--cwd
  /nope` prints a refusal naming the path and the reason (not a directory /
  does not exist) and no session output follows; the banner's `cwd:` line
  shows the session root (BR-6).
- [ ] AC-10: `/cd <abs>` in a live session: the transcript states the new root
  and kind; the next prompt's environment block names the new root; a `read`
  that succeeded under the old root and names a file absent under the new one
  now fails with the BR-2 shape; the `context_cleared` line reports the blocks
  dropped; `/cd /nope` leaves the root and context unchanged and prints the
  refusal; `/cd` alone prints the current root and kind. Asserted at every
  permission level — the full enumerated set, `plan` included, not a sample
  (informed by LESSON-524).
- [ ] AC-11: `/cd ~` from a project fires the BR-5 notice line (BR-8).
- [ ] AC-12: `--cwd` and `/cd` accept and reject the same path spellings —
  one grammar table drives both tests (informed by REQ-582).

_Leg C_

- [ ] AC-13: `glob "**/teton-code"` over a fixture with `a/teton-code/` (a
  directory) and `a/teton-code/x.rs` returns the directory (marked as one) and
  its provenance tags contain the directory's identity; `glob "**/*.rs"` still
  returns only files.
- [ ] AC-14: With a test-configurable `max_entries` (an injected budget, not
  a sleep or a giant fixture), a fixture with more entries than the budget
  makes `glob` and `grep` each end with the stopped-by-entries line; a fixture
  under the budget produces no such line. The same for `max_wall` with a
  test-configurable ceiling.
- [ ] AC-15: The stopped line appears with zero matches found and with matches
  found (BR-10's "never a silent partial").
- [ ] AC-16: With the root kind forced to `home` (an injected kind, or a
  fixture root passed as `$HOME` to the derivation), `glob "**/*.rs"` and
  `grep` do not enter the fixture's top-level `Library/`, `Music/`,
  `Pictures/`, nor `x.photoslibrary/` at any depth (a `.rs` planted inside each
  is not found), **do** enter `Documents/app/Library/` (its `.rs` is found),
  and `glob "Library/**/*.rs"` finds the one under the top-level `Library/`.
  With kind `project`, all are found.
- [ ] AC-17: A fixture directory made unreadable (mode `000`; test skipped when
  running as root) makes `glob` and `grep` each end with the "1 folder could
  not be read (permission denied): secrets/" line; matches elsewhere are still
  returned. On macOS the line carries the consent-dialog sentence (asserted
  on macOS only; asserted absent on Linux).
- [ ] AC-18: A test reads the shared WalkSkipSet/MediaTreeSet/WalkBudget
  definition and asserts both walkers are built from it (BR-11); a
  source-scan-style test proves no walker declares a private skip list.
- [ ] AC-19: A `shell` command that times out under a `home`-kind context
  receives the appended consent-dialog sentence; the same command under a
  `project`-kind context receives today's message byte-for-byte.
- [ ] AC-20: **Live A/B on the local tier** (mandatory real-model check —
  informed by BUG-154, LESSON-482): from `cd ~ && teton`, ask "look in my
  development folder for the Teton repo". The guarantees are at the surface,
  not in the model's prose: (a) the launch notice appeared; (b) no walker
  entered `~/Library`, `~/Music`, `~/Pictures` (no macOS consent dialog for
  Media, Photos, or "data from other apps" appears — recorded in
  `docs/manual-verification.md` as a by-hand step); (c) any walk that ran
  ended within budget with the stopped line rather than hanging. The model's
  answer is recorded in the verify notes as an observation, not asserted
  (LESSON-532).
- [ ] AC-21: `docs/manual-verification.md` gains the by-hand runbook for
  AC-20 (the dialog non-appearance cannot be automated).

## External Dependencies

- None. Branch detection must not require `git` on `PATH` or a git library
  (ADR-001's lean-binary stance; BUG-174's lesson that the daemon's `PATH` is
  only as good as what started it); how the branch is read is an architecture
  decision. No new crate for the walk budget or the marker table.

## Assumptions

- A-1: The branch name can be determined without invoking git in the common
  case (an ordinary checkout on a named branch, including a linked worktree);
  detached-HEAD and unreadable cases degrade to "no branch stated" and that
  is acceptable (AC-3).
- A-2: A default `max_entries` in the tens of thousands comfortably covers
  every realistic single repository (this workspace is ~2.5k entries outside
  the skip set) while stopping a home-folder crawl well inside a shell
  timeout. The final numbers are an architecture decision (OQ-4) and are
  verified against this workspace and a synthetic large tree.
- A-3: Not entering the media and library trees is what prevents the macOS
  consent dialogs; Teton does not, and need not, query the OS for consent
  state. Confirmed by the incident's own shape (dialogs arrived in walk order)
  and by ADR-007's attribution note.
- A-4: The incident's tool sequence is reconstructed (no transcript is
  persisted; `cost.db` shows two `kimi-k3` `edit` turns then a local turn,
  and the live process's cwd was `~`). Every link in the reconstruction is a
  defect on its own terms whether or not the model followed that exact
  sequence.
- A-5: The `context_cleared` shape and the session's carry rules (REQ-567) are
  reusable for the `/cd` disposition without protocol change; if a new event
  is needed it is additive.
- A-6: The launch notice is a notice, not a confirmation gate; users who
  deliberately work in `~/scratch`-style non-project directories are not
  interrupted (OQ-5).

## Open Questions

- [ ] OQ-1: **Boundary anchoring.** `local-only` globs are anchored at the
  session root, so a project's `secrets/**` does not cover
  `Documents/GitHub/project/secrets/prod.env` when the root is `~`. This REQ
  makes it *visible* (BR-5). Should boundaries instead be anchored at the
  nearest enclosing project root, or matched with an implicit `**/` prefix?
  Recommendation: file it as its own BUG/REQ (it is a privacy-boundary
  narrowing, and the fix touches the egress choke point) — do not fold it in
  here.
- [ ] OQ-2: **`/cd` disposition.** Clear the conversation (recommended, BR-7 —
  provenance identities are root-relative) versus carry it with re-minted
  identities. Carrying is only safe if every carried identity can be
  re-resolved under the new root, which is false in general.
- [ ] OQ-3: Should the environment block also carry a "known projects" line
  (the launch-history registry)? Deferred to the project-locator REQ; the
  block's format should leave room for one more line without re-testing the
  ceiling.
- [ ] OQ-4: Numbers: `max_entries`, `max_wall`, the cap on named unreadable
  dirs, and the exact MediaTreeSet per platform (Linux has no consent dialogs;
  is `~/.cache` in the skip set or the media set?). Architecture decides;
  A-2 sets the constraint.
- [ ] OQ-5: Should a `filesystem_root` kind (`/`) be a confirmation gate
  rather than a notice? Recommendation: still a notice; `/` is almost always
  an accident but a gate is a second prompt in a flow that already has one.
- [ ] OQ-6: Windows path display and marker table are out of scope (Windows is
  out of MVP scope) — but should the kind derivation refuse to compile rather
  than misclassify there? Architecture.
- [x] OQ-7: **What does a bare directory identity taint?** A `glob` that lists
  `secrets/` surfaces a *name*, not content, and today's matcher says
  `secrets/**` covers the files under it, not the bare `secrets` (the
  boundary module's documented semantics). Recommendation: keep the matcher's
  verdict as-is (a listed directory name does not taint) and say so in the
  architecture — but confirm, because it is a privacy-adjacent product call
  and REQ-571's fail-closed posture could argue for treating a directory whose
  subtree is covered as covered. *Status:* the architecture adopted the
  recommendation (ADR-3, "OQ-7 resolved"), and a `provenance_egress` case
  now pins it — a listed directory name under a covered subtree does not
  taint. **Resolved 2026-08-19 (product): confirmed** — a listed directory
  name never taints; files under the boundary still do; the
  `provenance_egress` pin is the record.
- [x] OQ-8: **Do session-scoped AllowAlways permission grants survive a
  `/cd`?** Today they do: a move clears only the conversation (BR-7, OQ-2),
  and the session's grants, taint pin and pasted URLs stay as they do across
  `/clear`. LESSON-495 (a remembered grant answers every question its key
  matches, so the key must encode the whole question) is the frame, and it
  cuts both ways: the grant's key never encoded the root — a grant is "this
  tool, this session", not "this tool under this directory" — so under
  today's key a move has nothing to invalidate; but if the user was really
  deciding "allow `shell` in *this project*", the key is missing a noun and
  the grant should die with the root it was given under. Decision pending;
  until it is made, the `/cd` line says the conversation was cleared and
  nothing about grants, which is what is true.
  **Resolved 2026-08-19 (product): keep.** Session-scoped grants survive a
  `/cd`, as they survive `/clear` — a grant is "this tool, this session"; the
  `/cd` line and the re-fired notice are what tell the user the ground moved.

## Out of Scope

- **Project locator** (recommendation 4 of the 2026-08-18 assessment): a
  registry of launch directories plus a shallow scan of conventional dev
  folders (`~/Documents/GitHub`, `~/Developer`, `~/Projects`, …), exposed as an
  environment line and/or `/open <name>`. Separate REQ.
- **Consent-aware walks in Teton's own voice** (recommendation 5): asking the
  user *before* a walk would touch a protected folder, and any daemon
  packaging change that would make `teton-code` its own consent-attributed
  process. Separate REQ; ADR-007's residual.
- **Boundary re-anchoring** (OQ-1). Separate BUG/REQ.
- **Triage** classifying "find my repo" as `edit` (recommendation 6).
- The `shell` tool's cwd jail semantics (absolute paths inside a command stay
  outside the tool's reach, as today); only the timeout *message* changes.
- Any change to the 200-result caps of `glob`/`grep`, or to `read`/`edit`
  beyond the refusal wording.
- Windows.

## Deferred

- AC-20 (b) — the by-hand check that no Media / Photos / "data from other
  apps" consent dialog appears during a `~`-rooted search — is OUTSTANDING
  in `docs/manual-verification.md` (REQ-583 runbook); tracked as ASSUME-014.
  Everything automatable in AC-20 ran against the real local model
  (TASK-180).
- Recommendations 4 (project locator) and 5 (consent-aware walks in Teton's
  voice) and the OQ-1 boundary-anchoring narrowing remain unfiled follow-ups
  (see Out of Scope).

## Retrieved Context

- LESSON-524 (lesson, score 13): Exposure is not callability — a capability asserted present must be asserted usable at every permission level
- LESSON-495 (lesson, score 12): A remembered grant answers every question its key matches — so the key must encode the whole question
- LESSON-532 (lesson, score 10): Presence in context is not instruction-following — a small model transfers data, not directives
- BUG-176 (bug, score 10): The shipped guide told users to put a live API key on the command line
- LESSON-518 (lesson, score 10): A blocking gate's reader-loop freedom is not inherited from the await-based reader-loop tests
- LESSON-519 (lesson, score 10): An "assert by inspection, not from the error" AC needs the real artifact — add a refusing test seam to reach it
- LESSON-520 (lesson, score 10): A gate that fires before deserialization makes an invalid-payload test vacuous — use a persistable payload + a refuse/accept pair
- LESSON-474 (lesson, score 10): If the tokenizer treats a string as frame, so must your renderer — sanitize where the parser is, not where the format is
- BUG-168 (bug, score 9): The web-off clause loses both its duties on the local tier — the opt-in is never named, and the hunt it forbids is the hunt it causes
- LESSON-496 (lesson, score 9): "Cut first under pressure" means "never available" when the limit equals the count
- LESSON-493 (lesson, score 9): A prompt ending is only reachable if its knowledge source exists — bundle what only the product knows
- BUG-160 (bug, score 9): Asked how to hook up external models, the agent searches the user's repo — Teton's own setup instructions are not bundled
- LESSON-482 (lesson, score 9): A prompt that enumerates a turn's legal endings must name every one — the model can only stop in a way it was told about
- BUG-154 (bug, score 9): The system prompt describes no ending for a question that needs no files, so the model searches the repo instead of answering
- LESSON-515 (lesson, score 8): A feature-gated target is invisible to every refactor

(BUG-147 — the `/`-jail predecessor, score 6, below the top-15 cut — was read
directly as the load-bearing precedent for BR-6 and the Description; REQ-582
and REQ-571 are cited from their already-in-context rules, not re-read.)
