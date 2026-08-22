---
id: REQ-584
title: "A project locator — the session can name this machine's projects without walking the disk, and a bare name moves the root"
status: approved
deployable: true
created: 2026-08-19
updated: 2026-08-22
component: "daemon/harness"
domain: "harness"
stack: ["rust", "daemon", "cli", "json-rpc"]
concerns: ["developer-experience", "privacy", "reliability"]
tags: ["project-locator", "known-projects", "projects-tool", "session-root", "cd", "dev-folders", "registry", "launch-history", "environment-block", "hand-off", "tcc", "macos", "req-583-followup"]
---

## Description

REQ-583 made a search from the home folder *bounded and honest*: a `~`-rooted
`glob`/`grep` now ends within its budget and says so, the media and cache
trees are never entered, and the user is told at launch that they are not in a
project. What it deliberately left open is the question the 2026-08-18
incident actually asked — *"look in my development folder for the Teton
repo"* — and TASK-180's live A/B shows the gap exactly: from `~`, the model
ran `glob dev/**/teton*`, `glob **/teton*` (stopped by the entry budget well
before APFS's hash order reached `Documents/`) and `glob ~/*teton*` (a literal
`~`), then apologised: *"I'm unable to locate the Teton repository."* The
repo sits at `~/Documents/GitHub/teton-code`. Nothing on the machine was
unsafe or slow any more; the model simply had no source of the one fact it
needed, and a walk is the wrong instrument for a question whose answer set is
tiny — the handful of places this machine keeps projects.

This REQ gives the session that source. Three observations shape it:

1. **Teton already knows most of the answer.** Every session is created with a
   root, and REQ-583 derives that root's kind — `project` when a marker is
   present. A directory the user has launched `teton` in, or `/cd`'d to, that
   was a project *is* a known project. Remembering those is a registry, not a
   walk.
2. **Projects live in conventional places.** GitHub Desktop clones to
   `~/Documents/GitHub` (where this user's repo is); developers keep
   `~/Developer`, `~/Projects`, `~/src`, `~/code`, `~/dev`, `~/repos`,
   `~/work`. Scanning those — and the *parents* of already-known projects,
   which are dev folders by evidence — two levels deep for REQ-583's project
   markers is a bounded, cheap, on-demand operation. It is not a walk of `~`.
3. **A small model transfers data, not directives** (LESSON-532, ASSUME-008).
   So the knowledge is delivered as *facts in context* where they fit — the
   names of known projects, on the environment line REQ-583 added, for a
   non-project root — and as a read-only tool for the rest (paths, the dev
   folders, a query); and the hand-off to the user is a surface line the
   harness prints when a match is found, not a sentence the model is asked to
   say (REQ-579 ADR-9, BUG-176).

And one boundary that does not move: **the model never moves the jail.**
REQ-583 ADR-4 keeps `session/set_cwd` off the tool surface and scan-pinned;
this REQ tells the model *where* a project is and tells the *user* how to move
there (`/cd <name>`). A bare project name becomes a `/cd` argument; the
registry is what resolves it.

Two legs, one thesis — **"where is my X repo?" is answered from what the
machine already knows, in one turn, with no walk and no dialog:**

- **Leg A — a registry of known projects, learned from use and from the usual
  places.** Recorded when a session is created at, or moved to, a project
  root; completed on demand by a bounded scan of conventional and learned dev
  folders; pruned when a path is gone; stored beside the daemon's own state;
  never leaving the machine except as the content of a turn the user sends.
- **Leg B — three surfaces that read it.** A `projects` tool the model can
  call (list, dev folders, `query`); known project *names* on the environment
  line for a non-project root, inside the byte budget REQ-583 already pays;
  and on the CLI `/projects`, a bare name for `/cd`, the launch notice naming
  a few projects, and a surface line printed whenever the tool found a match.

## System Model

### Entities

| Entity | Field | Type | Constraints |
|--------|-------|------|-------------|
| KnownProject | path | absolute path | required; unique; a directory that held a REQ-583 project marker when recorded |
| KnownProject | name | string | the path's basename, bounded like the REQ-583 project name; not unique (two `api/` dirs may both be known — disambiguated by path) |
| KnownProject | source | enum: `launched` \| `scanned` | `launched` = a session was created at or moved to it; `scanned` = found by the dev-folder scan; a scanned entry becomes `launched` the first time it is used |
| KnownProject | first_seen / last_seen | timestamps | `last_seen` updated on every session create / `/cd` that lands there; ordering key |
| KnownProject | uses | integer ≥ 0 | session creates + `/cd`s that landed there; secondary ranking key |
| ProjectRegistry | entries | list of KnownProject | capped (LRU by `last_seen`); persisted in the daemon's per-user state directory with the state dir's permissions; entries whose path no longer exists or no longer holds a marker are dropped at read and at write |
| DevFolder | path | absolute path | a conventional dev folder that exists (`~/Documents/GitHub`, `~/Developer`, `~/Projects`, `~/projects`, `~/src`, `~/code`, `~/dev`, `~/repos`, `~/work`, `~/workspace`, `~/GitHub` — one table), or the parent of any `launched` project; the table is the single source |
| ProjectScan | budget | entries + wall clock | bounded like a REQ-583 walk (a smaller budget than the tool walkers); depth ≤ 2 below each dev folder; never enters REQ-583's skip set, home-top-level set or media bundles; never follows symlinks; runs **on demand only** |
| ProjectQuery | query | string? | optional; case-insensitive; matched against `name` (exact > prefix > substring), then against path segments; results ranked by match class, then `last_seen`, then `uses`; result count bounded |
| LocatorView | … | | what the tool returns and the CLI renders: for each match — name, display path (REQ-583 `display_for`, bounded), source, last_seen (relative, e.g. "2 h ago"), and the recipe `/cd <name>`; plus the dev folders that exist with their project counts |

### Events

| Event | Trigger | Payload |
|-------|---------|---------|
| (registry write, not a bus event) project recorded | `session/create` or `session/set_cwd` lands on a `project`-kind root | path, name, now |
| (tool result) `projects` | the model calls the tool | the LocatorView as text; provenance: none (metadata about the machine, not file content) |
| (surface line) project match hand-off | a turn in which the `projects` tool returned ≥ 1 match | one line on the session surface naming the best match's `/cd <name>` recipe, printed by the harness/CLI regardless of the model's prose |

### Permissions

| Action | Roles Allowed |
|--------|---------------|
| call `projects` | the model, at **every** permission level including `plan` — it is read-only metadata about the machine, like `teton_docs` (informed by LESSON-524) |
| `/projects`, `/cd <name>` | the session's client, at every level (the `/cd` half is REQ-583 BR-7) |
| read/write the registry file | the daemon only, in its state dir; the CLI reads it through the daemon, never the file |

## Business Rules

_Leg A — the registry_

- [ ] BR-1: **Use is the first source of truth.** Every `session/create` and
  every `session/set_cwd` whose root kind is `project` records that root in
  the registry (path, basename, `last_seen = now`, `uses += 1`,
  `source = launched`). Roots of kind `home`, `filesystem_root`, `plain` are
  never recorded. The CLI's `--cwd` counts as a create.
- [ ] BR-2: **The registry forgets what is gone.** An entry whose path no
  longer exists, or no longer holds a REQ-583 project marker, is dropped at
  read and at write; no stale entry is ever shown. The registry is capped
  (LRU by `last_seen`); the cap is an architecture constant and its
  exhaustion is silent — the oldest entry goes.
- [ ] BR-3: **The scan is on demand, bounded, and never a walk of home.** The
  dev-folder scan runs only when something asks for projects (the `projects`
  tool, `/projects`, a bare-name `/cd` that the registry cannot resolve) —
  never at launch, never on a timer, never during a turn that did not ask. It
  visits each DevFolder at most two levels deep, under an entry + wall-clock
  budget smaller than a tool walk's, reusing REQ-583's skip set, home-top-level
  set and bundle suffixes, never following a symlink, and it reports a budget
  stop the way a tool walk does. Scanned projects enter the registry as
  `scanned` (they do not count as `launched` until used). On macOS the scan
  may be the first thing to touch `~/Documents` (`~/Documents/GitHub` is a
  DevFolder) — that is the ordinary "Terminal would like to access files in
  your Documents folder" dialog, raised by a question the user asked, not by
  a launch; it is named in the runbook, never suppressed.
- [ ] BR-4: **Dev folders are one table plus evidence.** The conventional list
  is a single table (the System Model's DevFolder row), exercised by name in
  tests; the parent directory of every `launched` project is a DevFolder too.
  A DevFolder that does not exist is skipped silently.
- [ ] BR-5: **Nothing leaves the machine on its own.** The registry lives in the
  daemon's state dir with its permissions; its content reaches a remote
  provider only as the text of a `projects` tool result inside a turn the user
  sent, and as the bounded project *names* the environment line carries for a
  non-project root (BR-7) — the same metadata class as the REQ-583 session
  root display. No file content, ever.

_Leg B — the surfaces_

- [ ] BR-6: **One read-only tool, `projects`, on every profile that has
  `glob`.** Arguments: an optional `query`. Result (bounded text): the known
  projects matching the query — or all of them when there is none — each as
  *name, display path, source, when last used*, ranked as the ProjectQuery
  says; then the DevFolders that exist, each with its project count; and, on
  every result, the one recipe that moves the session: `/cd <name>` (or
  `/cd <path>` when the name is ambiguous). When the registry is empty and the
  scan finds nothing, the result says so and names the DevFolders it looked
  in. The tool is callable at every permission level (LESSON-524) and is not
  displaced by the degraded-profile tool cap (REQ-563 LESSON-496 rule: an
  explicit exemption with a stated rationale, or membership in the mandatory
  set — architecture decides, the test asserts the headroom).
- [ ] BR-7: **Known project names ride the environment line for a non-project
  root — inside the byte budget REQ-583 already pays.** When the session root's
  kind is `home`, `filesystem_root` or `plain`, the REQ-583 environment line
  carries a clause of the form `Known projects: a, b, c (more: the projects
  tool; /cd <name> moves there).`, listing names by `last_seen`, *as many as
  fit* so that the line stays at or under the byte cost of REQ-583's
  worst-case project row. The worst row of both resident-ceiling sweeps is
  therefore unchanged and the constants do not move (REQ-583 ADR-2, AC-4).
  Names are bounded (REQ-583's name bound) and neutralised like every other
  user-controlled value on that line. A `project`-kind root carries no such
  clause (it is already somewhere). This is data, not a directive (LESSON-532,
  ASSUME-008): the model learns *that these projects exist* with no tool
  call; the tool is for paths and queries.
- [ ] BR-8: **`/cd` accepts a project name — after the shell's own reading.**
  An argument that is not a path spelling (contains no `/`, does not start
  with `~`, `.` or `-`) is tried first exactly as REQ-583 reads it — a
  directory of that name under the current root wins, so `/cd src` still
  means `./src` and REQ-583's behaviour is unchanged wherever it applied.
  Only when no such directory exists is the argument resolved against the
  registry: a unique name match moves there (`/cd teton-code`); an ambiguous
  one prints the candidates with their display paths and moves nowhere; no
  match at all → the refusal names both readings ("no directory `x` under
  the session root, and no known project named `x`"). `--cwd` keeps path
  semantics only (OQ-3).
- [ ] BR-9: **`/projects` lists them.** Bare `/projects` renders the LocatorView
  (registry first, then the scan on demand, with the budget-stop line when
  it hit one); `/projects <query>` filters like the tool. Each row ends with
  its `/cd` recipe. One renderer for the tool text and the CLI rows' content
  (the same facts, REQ-582's one-renderer rule) — the CLI may style, not
  restate.
- [ ] BR-10: **The launch notice names a few.** REQ-583's non-project notice
  gains a clause listing up to N known project names with `/cd <name>` (TTY
  only, like the notice; no ceiling cost). Empty registry → no clause; the
  notice does not trigger the scan (BR-3).
- [ ] BR-11: **A found project is handed off at the surface, not in prose.**
  When a turn's `projects` call returned at least one match, the session
  prints one line at turn end — `→ /cd <best name>  (<display path>)` — from
  the tool outcome, independent of what the model said (REQ-579 ADR-9's
  surface guarantee; LESSON-532). Dormant when the tool was not called or
  found nothing; mutation-testable.

## Acceptance Criteria

_Leg A_

- [ ] AC-1: A session created at `<tmp>/repo` (a `.git` project) writes a
  registry entry `{path, name: repo, source: launched, uses: 1}`; a second
  create there bumps `uses` and `last_seen`; `session/set_cwd` to another
  project records it too; creates at `$HOME`, `/`, and a marker-less directory
  record nothing.
- [ ] AC-2: An entry whose directory is removed, or whose marker is removed, is
  absent from the next `projects` result and from the next registry write; a
  registry over the cap drops the oldest `last_seen` entry.
- [ ] AC-3: The conventional DevFolder table is enumerated by name in a test
  (BR-4). The scan, with a test-injected DevFolder table pointing at a
  fixture, finds projects at depth 1 and 2 and not at depth 3; does not enter
  `Library/`, `node_modules/`, a `.photoslibrary`, or a symlinked directory
  planted in the fixture; records finds as `scanned`; stops at an injected
  budget and says so; and a `launched` project's parent is scanned as a
  DevFolder even when it is not in the table.
- [ ] AC-4: No scan runs at `session/create`, at daemon start, or during a turn
  that makes no `projects` call — asserted through a seam that records whether
  the scanner ran, across a full session create and one unrelated turn.
- [ ] AC-5: The registry file lives in the state dir with the state dir's
  permissions; a `projects` result for a project whose name is a frame label
  (`User:`) or a bidi string renders neutralised and bounded (REQ-583's
  bounding); no tool result carries file content.

_Leg B_

- [ ] AC-6: `projects` with no query lists every known project ranked by
  `last_seen` then `uses`, then the existing DevFolders with counts, and ends
  with the `/cd <name>` recipe; `projects {query: "teton"}` ranks `teton-code`
  (prefix) above `my-teton-notes` (substring) above a path-segment match; an
  ambiguous name yields `/cd <path>` recipes; an empty machine yields "no
  known projects; looked in: …".
- [ ] AC-7: `projects` is allowed at every `PermissionLevel` (the full
  enumerated set, `plan` included) with zero pending events (the LESSON-524
  template); it is exposed on every profile that exposes `glob`, and the
  degraded-cap headroom assertion still holds.
- [ ] AC-8: The environment line for a `home` root carries `Known projects:`
  with names ordered by `last_seen`, truncated so that the rendered line's
  byte length ≤ REQ-583's worst-case project row; both resident-ceiling
  sweeps pass with constants unchanged and their worst prompt is still the
  project row; a `project` root carries no clause; an empty registry carries
  no clause; a name with a newline/bidi char renders neutralised.
- [ ] AC-9: `/cd teton-code` with no such subdirectory and one registry
  match moves the session (the REQ-583 `context cleared; …` + `session root
  is now …` lines follow); `/cd src` when `./src` exists under the root moves
  to `./src` even if a known project is also named `src` (REQ-583's reading
  wins); `/cd api` with no such subdirectory and two registry matches prints
  both candidates with display paths and moves nowhere; `/cd nothing-known`
  with neither yields the two-reading refusal; `/cd ~/x`, `/cd ./x`,
  `/cd /abs` keep REQ-583's behaviour byte-for-byte (its grammar table is
  re-run).
- [ ] AC-10: `/projects` renders the same facts the tool returns (a test diffs
  the content through one renderer); `/projects teton` filters; the scan's
  budget-stop line appears when the injected budget is hit; nothing is
  scanned when `/projects` is not typed.
- [ ] AC-11: The non-project launch notice lists up to N known names with the
  `/cd <name>` recipe; with an empty registry it is REQ-583's notice
  unchanged; piped output stays byte-identical (TTY gate).
- [ ] AC-12: A turn in which `projects` returned a match ends with the surface
  line `→ /cd <best name>  (<display>)`; a turn without the call, or with no
  match, prints no such line; deleting the harness-side append makes the test
  fail (mutation check).
- [ ] AC-13: **Live A/B on the local tier** (the mandatory real-model check):
  with `teton-code` in the registry (one prior launch from it), `cd ~ &&
  teton`, ask *"look in my development folder for the Teton repo"*. Guarantees
  at the surface: the `→ /cd teton-code` line appears if the tool was called,
  and the environment line carried `teton-code` either way; the turn ran no
  `glob`/`grep` over `~` — or, if it did, it ended by budget as REQ-583
  guarantees. The model's prose (ideally naming `~/Documents/GitHub/teton-code`
  and `/cd teton-code`) is recorded as an observation, not asserted
  (LESSON-532). Repeat with an empty registry: the scan finds
  `~/Documents/GitHub/teton-code` on this machine.
- [ ] AC-14: `docs/manual-verification.md` gains the AC-13 runbook, including
  the macOS note that the first scan may raise the Documents dialog.

## External Dependencies

- None. The registry is a small document in the existing state dir; the
  scan reuses REQ-583's walk policy; no new crate.

## Assumptions

- A-1: Project names and display paths are acceptable metadata to place in the
  prompt and in tool results — REQ-583 already places the current root there
  (ADR-2, security review M6); this REQ widens it to *names of other
  projects* for non-project roots. If a user considers project names
  sensitive, the `local-only` boundary does not cover them today (they are not
  file content) — see OQ-5.
- A-2: Two levels under a dev folder cover the common layouts
  (`~/Documents/GitHub/<repo>`, `~/src/<org>/<repo>`, `~/Developer/<repo>`);
  deeper monorepo sub-projects are reached by launching from them once (BR-1).
- A-3: REQ-583's worst-case project row (203 bytes) leaves enough room on a
  `~` root's line (`Session root: ~ (your home folder). Platform: macOS.` is
  ~55 bytes) for at least three bounded names plus the clause's fixed words;
  a long `plain` display may leave room for none — the clause then shrinks to
  its fixed pointer or disappears (architecture decides the exact shrink
  order; AC-8 pins the byte rule).
- A-4: A cold registry is the common first state; the conventional scan is
  what makes the very first "where is X" answerable, and the user's own
  machine has the repo under `~/Documents/GitHub` (GitHub Desktop's default),
  which is in the table.
- A-5: The daemon runs as the user, so the state dir and the DevFolders are
  readable with the user's own permissions; on macOS the Documents/Desktop
  dialogs attribute to the terminal the daemon was spawned from (REQ-583 ADR-007
  note) — expected, and named in the runbook.

## Open Questions

- [ ] OQ-1: **Ranking of `launched` vs `scanned`.** Should a `scanned` entry
  that matches a query by name outrank a `launched` one that matches only by
  substring? Recommendation: match class first (exact > prefix > substring),
  then `launched` before `scanned`, then recency.
- [ ] OQ-2: **How many names on the environment line and in the notice?** The
  environment line is bounded by bytes (BR-7); the notice by a count N
  (BR-10). Recommendation: N = 5 for the notice; the line takes what fits.
- [ ] OQ-3: **Should `teton --cwd <name>` accept a registry name?** Recommendation:
  no — `--cwd` is a path flag and a shell has completion; `/cd` is where the
  name is worth it. Revisit if dogfood shows people typing names there.
- [ ] OQ-4: **`/projects forget <name>` / an edit path?** Recommendation: not
  in this REQ — BR-2 already drops dead entries, and the cap bounds growth;
  file it when someone wants a project hidden.
- [ ] OQ-5: **Should a `local-only` boundary be able to hide a project's name
  from the environment line and tool?** Today boundaries are file globs
  relative to the session root (REQ-583 OQ-1 territory). Recommendation: out
  of scope here; note it beside OQ-1's boundary-anchoring follow-up.
- [ ] OQ-6: **Dev-folder table per platform.** Linux adds nothing beyond the
  common names; Windows is out of MVP scope. Recommendation: one table,
  `$HOME`-relative, platform-agnostic.

## Out of Scope

- Content indexing or fuzzy search across the whole disk — the locator knows
  *projects*, not files.
- Any tool path that moves the jail (REQ-583 ADR-4 stands; the model gets
  `/cd <name>` as a recipe for the user).
- Consent-aware walks in Teton's own voice (the 2026-08-18 recommendation 5)
  and boundary re-anchoring at the project root (REQ-583 OQ-1) — separate.
- Syncing the registry across machines; editing it by hand; a "recent
  projects" picker UI beyond `/projects`.
- Windows.

## Retrieved Context

- LESSON-524 (lesson, score 11): Exposure is not callability — a capability asserted present must be asserted usable at every permission level
- LESSON-532 (lesson, score 10): Presence in context is not instruction-following — a small model transfers data, not directives
- LESSON-495 (lesson, score 10): A remembered grant answers every question its key matches — so the key must encode the whole question
- LESSON-539 (lesson, score 9): Claim first, then re-read — session state snapshotted before the turn claim is stale by construction
- LESSON-496 (lesson, score 9): "Cut first under pressure" means "never available" when the limit equals the count
- BUG-146 (bug, score 9): First prompt after install fails with a message blaming the local engine for a config/timing problem
- LESSON-540 (lesson, score 8): A fixture that names "the first listed entry" or writes stdin after spawn is a platform test in disguise
- BUG-176 (bug, score 8): The shipped guide told users to put a live API key on the command line
- LESSON-515 (lesson, score 8): A feature-gated target is invisible to every refactor
- LESSON-518 (lesson, score 8): A blocking gate's reader-loop freedom is not inherited from the await-based reader-loop tests
- LESSON-519 (lesson, score 8): An "assert by inspection, not from the error" AC needs the real artifact — add a refusing test seam to reach it
- LESSON-520 (lesson, score 8): A gate that fires before deserialization makes an invalid-payload test vacuous — use a persistable payload + a refuse/accept pair
- BUG-167 (bug, score 8): The llama-gated template smoke no longer compiles
- LESSON-510 (lesson, score 8): A harness that checked a binary exists has not checked it is the one under test
- BUG-168 (bug, score 8): The web-off clause loses both its duties on the local tier — the opt-in is never named, and the hunt it forbids is the hunt it causes

(REQ-583's requirement/architecture and ASSUME-008 are cited from their
in-context text — this REQ is their direct follow-up; BUG-168 was read
directly for its root cause, which is why BR-7 carries names as facts rather
than a "use the projects tool" directive.)
