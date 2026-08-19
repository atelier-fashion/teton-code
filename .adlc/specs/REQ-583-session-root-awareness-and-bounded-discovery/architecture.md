# REQ-583 — Architecture: session-root awareness and bounded discovery

Status: proposed 2026-08-18 (Phase 2 of `/proceed`). Requirement:
`requirement.md` in this directory (14 BRs, 21 ACs, three legs).

## Approach

One value — **the session root** — is derived in one place, once per turn, and
carried to the three surfaces that today each know nothing about it: the
system prompt (Leg A), the tool jail and its walkers (Leg C), and the client's
launch/`/cd` lines (Leg B). Nothing is *stored*: the registry keeps only the
path it keeps today (`SessionSummary.cwd`); kind, project name, branch and
display are re-derived from that path each time they are needed, so a `/cd`
that rewrites the path moves every consumer on the next turn with no second
source of truth (LESSON-473: workspace paths are session state supplied by the
client, validated and scoped by the daemon).

```
                 SessionSummary.cwd  (registry — the only stored fact)
                          │
             ┌────────────┴────────────┐
             ▼                          ▼
   session/create · session/set_cwd     run_prompt_turn (per turn)
   (server.rs) ── probe ──▶ SessionRoot ◀── probe ── (runtime.rs ~2842)
             │   (tetond::session_root)      │
             ▼                                ├──▶ ToolContext::for_root(root)   → jail refusals name it,
   SessionCreateResult.root                   │      walkers read kind + budget,
   SessionRootChanged event ──▶ CLI notice,   │      shell timeout hint
      transcript line, cached for `/cd` alone └──▶ HarnessConfig.session_root  → environment_block() in
                                                     build_system_prompt (Leg A)
```

The pure half (kind classification, display spelling, field bounding, the
marker table) lives in `teton-core::session_root`; the I/O half (marker probe,
`.git/HEAD` read) lives in `tetond::session_root`; the wire view
(`SessionRoot { display, kind, project_name, vcs_branch }`, `RootKind`) lives in
`teton-protocol` because both the daemon and the CLI speak it, and
`teton-core` already depends on `teton-protocol` (REQ-565), never the reverse.

## Key decisions (ADRs)

### ADR-1: The root is probed per turn from the registry's path — never cached, never client-derived

`run_prompt_turn` already builds a fresh `ToolContext` per turn from
`session_cwd` (`runtime.rs:2838-2842`, BUG-147) and injects the per-turn
`web_capability` fact into `route.harness` right beside it (`:2849-2852`). The
root joins that seam: `let root = session_root::probe(cwd_or_fallback, home)`,
then `ToolContext::for_root(root.clone())` and
`route.harness.session_root = Some(root)`. The probe is a handful of `exists()`
calls plus one small file read; a per-turn cost invisible next to a model call,
and it keeps the branch honest after a checkout between turns.

The CLI does **not** derive kind itself. It sends the path (as today, or from
`--cwd`) and reads the derived view back on `SessionCreateResult.root` and on
the `session_root_changed` event — one derivation, on the side that enforces
the jail (LESSON-473; surface-parity rule: a client holds no session state the
daemon lacks). The one thing the CLI computes locally is the *display*
spelling for the banner's `cwd:` line, which prints before the session exists;
it calls the same `teton_core::session_root::display_for(path, home)` the
daemon uses, so the two spellings cannot drift (REQ-578 ADR-1's "pure slice
linked into a thin client" corollary — `teton` already depends on
`teton-core`).

**Rejected**: storing kind on `SessionRecord` (a second fact to keep in step
with the path); deriving kind in the CLI (two derivations, and the daemon's is
the one that gates walks).

### ADR-2: The environment block is one bounded line of facts, and it is paid for out of the guide's reference data — not out of the ceiling

`build_system_prompt` gains one line right after the opener paragraph, from a
pure `environment_block(&SessionRoot) -> String`:

```
Session root: ~/Documents/GitHub/teton-code (project teton-code, branch main). Platform: macOS.
Session root: ~ (your home folder). Platform: macOS.
Session root: / (the filesystem root). Platform: Linux.
Session root: ~/scratch (not a project). Platform: macOS.
```

Facts only, in the shape LESSON-532 says a small model *transfers*; the
directive half ("do not search outside it") is enforced by the tools, not
requested. It rides on `HarnessConfig.session_root: Option<SessionRoot>` with
the `web_capability` contract (`None` = not supplied, every existing
two-argument `build_system_prompt` caller unchanged — 30+ sites). Platform is
`cfg!(target_os)` rendered as a word (`service.rs:51` idiom, both branches
compile everywhere).

**Bounding and containment.** The three user-controlled values (root display,
project name, branch) are the same trust class as an MCP tool description
landing in the system prompt (BUG-148, LESSON-477 §2). They are (a) placed
mid-line after a harness label, never at column 0 (so `neutralize_frame_labels`
at assembly — `context.rs:1311` — and `neutralize_control_tokens` at render
cover them by construction, and no new marker set is needed); (b) bounded by
`teton_core::session_root::bounded_field` — control characters and newlines
replaced by `?`, display middle-elided to ≤ 80 chars, name/branch cut to ≤ 32
chars — so no path can blow the ceiling; (c) the same bounded strings are the
ones the jail refusal, notice and `/cd` line print, built once.

**Paying for it.** The resident-prompt ceiling
(`REDACT_BODY_OVERHEAD_BYTES = 9 KiB`, floor `MIN_PROMPT_HEADROOM_BYTES = 48`)
stands **2 bytes** above its floor after REQ-581, and its own note says the
next resident sentence "has to buy itself with the ADR-2 fallback — depth into
a `teton_docs` topic, a resident line only for the fact a model must have
without a tool call" and that "neither this floor nor the 9,216 is to be
moved". The block is exactly such a fact (a model cannot learn where it is by
calling a tool — the tools are jailed to the answer). Its worst case is ~200
bytes (80-char display + 32 + 32 + labels). It is bought by moving **reference
data** from `self_config.md`'s web paragraph into the existing `web` topic
(`harness/docs/web.md`, which already carries all of it): the `[web]` key list
(`search_endpoint`, `search_key_ref …`, `allowed_domains`, `cache_ttl_secs`) and
the "`search` also needs the local model" sentence become a pointer
("the rest: `teton_docs web`"). ASSUME-008 established the split this relies
on — *reference data* moved to a topic transfers; *instructions* do not — and
nothing moved here is an instruction. Every contract-pinned string stays:
the three auth templates and the keyless SearxNG line with
`/search?format=json` (`web_setup_contracts.rs` REQ-573 AC-4), the provider
recipes and URLs (`provider_recipes.rs`, `web_setup_contracts.rs:713/1113`),
the inspect step's session spellings (`cli_rows.rs` guide_tests), step 1's
hand-off (`turn_loop.rs:2641`). If the paragraph does not free enough, the
next source is step 2's `--fallback`/`set-category` clause (the `policy` topic
carries both). Both ceiling sweeps (`redact.rs:2016`, `web.rs:2248`) gain a
row with a 200-character root and the same two assertions; the redact.rs
headroom note gains a REQ-583 paragraph. The task that adds the block runs **after** the task that
rewords the five tool descriptions ("repository" → "session root", a few
bytes each), so the sweep measures the docs actually in the tree — the floor
stays 48 (AC-4) and no allowance is guessed (LESSON-491); the integration task
re-records the final figure at the merged tip.

**Rejected**: moving `REDACT_BODY_OVERHEAD_BYTES` to 10 KiB (the arithmetic
still yields 4 chunks, but the constant's note forbids it and AC-4 pins "the
same ceiling and headroom"); dropping the platform (BR-1 requires it, and it is
the fact behind `sed -i ''` vs `sed -i`).

### ADR-3: One walk policy module; directories match only when the pattern's last segment names them; harness trailer lines are recognised as harness lines

A new `harness/tools/walk.rs` owns everything the two walkers used to own
twice: `WALK_SKIP_DIRS` (today's `.git`, `target`, `node_modules`, plus the
inert-everywhere `.hg`, `.svn`, `__pycache__`), `HOME_TOP_LEVEL_SKIPS` (the
BR-12 media trees `Library`, `Music`, `Pictures`, `Movies`, `.Trash` and the
dev caches `.cache`, `.cargo`, `.npm`, `.rustup`, `.gradle`, `.m2`, `.nvm` —
applied **only** to a directory that sits directly under a user's home: the
root itself for a `home` kind, `<root>/Users/*` and `<root>/home/*` for a
`filesystem_root` kind), `MEDIA_BUNDLE_SUFFIXES` (`.photoslibrary`,
`.musiclibrary`, pruned at any depth), and `WalkBudget { max_entries: 100_000,
max_wall: 10 s }`. `walk::visit(root, kind, named_prefix, policy, |entry|)`
drives the recursion for both tools and returns a `WalkReport { truncated_by,
unreadable: Vec<String>, unreadable_total }`. `named_prefix` is the pattern's
leading literal segments: a pruned directory is entered when the pattern names
it or something under it (`Library/**/*.plist` from `~` enters `~/Library`;
`Documents/app/Library` is never pruned because it is not under a home
directly). Kind and policy ride on `ToolContext` — `new(path)` keeps its 89
call sites (kind `plain`, default policy), `for_root(root)` is what the
runtime uses, `with_walk_budget`/`with_root_kind` are the AC-14/AC-16 seams
(the `ShellTool::with_timeouts` shape, on the context because the tools are
unit structs invoked as `GlobTool.run(...)` in fifteen places).

**Directory matches (BR-9).** A directory is a match when the pattern's *final*
segment is not `**` and the whole pattern matches the directory's identity.
`**`-terminated patterns (`**`, `secrets/**`) keep enumerating files only —
"everything beneath" is a file enumeration — so `symlink_posture.rs:570` (`**`
lists exactly two files) and `glob.rs` `provenance_is_the_set_of_enumerated_files`
(`secrets/**`) are unchanged, and `**/teton-code` returns the directory. A
directory is listed as `id/` and tagged as `id`: `ProvenanceId::mint` already
elides a trailing `/` (`provenance_id.rs:269`), so the two spellings resolve to
one identity, and the trailing `/` is the conventional marker of a directory
name, not a second name. **OQ-7 resolved (recommendation adopted):** a bare
directory identity carries whatever verdict the matcher gives it —
`secrets/**` covers the files under `secrets/`, not the name `secrets` — so a
listed directory name never taints; the name is metadata, the boundary is
about content, and today's `provenance_egress.rs:395` behaviour is unchanged.
Symlinked directories stay skipped (REQ-571 BR-5 untouched).

**Trailer lines.** Every harness line a walker appends is written by
`walk.rs` and starts with `... (` — the shape grep's cap notice already uses:
`... (stopped after N entries; narrow the pattern, or move the session root
with /cd)`, `... (stopped after N s; …)`, `... (N folder(s) could not be read
(permission denied): a/, b/ and N more)` — on macOS the unreadable line ends
`— macOS may have blocked access to that folder, or be waiting on a consent
dialog for it`. Grep's `split_cap_notice` becomes `split_harness_trailer`:
it peels every trailing `... (` line, and `render_ranked` re-appends the same
lines, so the triage duty ranks matches and only matches (REQ-561's M3
class). Order in a result: matches, then walk trailer lines, then the cap
notice last (unchanged position). The BR-10 "never a silent partial" rule is
one renderer: the stopped line is appended whether zero or many matches were
found. Glob's empty case reads `no matches for `{pattern}`` (it can now match
directories; the one test asserting "no files match" is updated).

**Shell (BR-14).** `ShellTool::run` reads `ctx.root_kind()`; on timeout with
kind `home`/`filesystem_root` **and** `cfg!(target_os = "macos")` it appends
one sentence to the existing message; `measuring(0)` and provenance are
unchanged, so `shell_duty` and the timeout tests survive.

**Rejected**: putting the budget on `GlobTool`/`GrepTool` (fifteen unit-struct
call sites); pruning media names at any depth (hides `~/Documents/GitHub/app/Library`
— the spec's own counter-example, AC-16); a `.gitignore`-aware walk (a
dependency, and out of scope).

### ADR-4: `/cd` is `session/set_cwd`, modelled on `session/clear`, and it lives outside `harness/`

A new RPC `session/set_cwd { session_id, cwd }` → `{ root, blocks_dropped }`
(teton-protocol, additive; no `PROTOCOL_VERSION` bump). Server handler beside
`handle_session_clear` (`server.rs:4050`): parse → `refuse_unmintable_session_id`
→ `conn.may_drive` → runtime. `DaemonRuntime::set_session_cwd` (`runtime.rs`,
beside `clear_session` at 3104): take the turn claim (`try_begin_turn`, id
`cd-N`, so a running turn refuses `SESSION_BUSY` and the tool jail cannot move
under a turn), validate with the **one** validator extracted from
`handle_session_create` (`server.rs:3211-3226` — `is_absolute`, `is_dir`; its
two refusals now name the path, BR-6), `SessionRegistry::set_cwd` (a
`set_title`-shaped mutator — none exists today), `clear_conversation`, publish
`Event::ContextCleared { blocks_dropped }` **and** `Event::SessionRootChanged {
previous_display, root }` (both `Some(session_id)`-scoped — the display is
content-class on the wire, `server.rs:1364-1373`, so `forward_events` filters
them for monitors), then answer. Events precede the response (module rule,
`server.rs:4032-4037`). Nothing under `harness/` names `clear_session`,
`SessionClearParams` or the new method — `no_tool_can_clear_a_session_and_no_mcp_wiring_path_could`
(`tools/mod.rs:1224-1281`) scans that tree, and a model must never be able to
move its own jail (the same posture that keeps clearing off the tool surface).

Permission levels gate **tools**, not RPC dispatch (`permissions.rs`; `server.rs`
names `PermissionLevel` only in tests), which is why `/clear` works at `plan`
and why `/cd` is level-independent by construction — AC-10 asserts that
nothing *else* gates it, iterating `PermissionLevel::ALL` (LESSON-524's
"exposure is not callability", the `a_bundled_docs_read_is_allowed_at_every_level`
template).

The disposition is **clear** (OQ-2 resolved as recommended): every carried
block's provenance identity is root-relative, and a carried identity judged
under a new root names a different file. The CLI reports it with the
`context_cleared` line it already renders (`format_context_cleared`), and the
new `session_root_changed` arm prints `session root is now ~/x (project x,
branch main)` — or, from another client, `session root moved in another
session (<id>)` — then re-fires the BR-5 notice content when the kind is not
`project` (BR-8). `/cd` alone prints the root the CLI last heard
(`SessionState.root`, filled from `SessionCreateResult.root` and every
`session_root_changed` — a cache of a daemon fact, not client state).

**`--cwd`** is a top-level, **non-global** clap arg on `Cli` (`global = true`
would be silently dropped by every mirrored row — `cli_rows.rs:415-421`,
`LEADING_GLOBAL_FLAGS` pin `slash.rs:4468`). Resolved client-side: `~`
expansion, relative → joined onto the shell cwd, no canonicalization (the
daemon validates); threaded to both `SessionCreateParams` sites (`main.rs:914`,
`:2160`); the banner's `cwd:` line prints `display_for(session_root)`. A refused
create prints the daemon's path-naming refusal and exits **non-zero** — today's
`return Ok(())` at `main.rs:918-927` reads as success to a script and BR-6 says
"never a session that starts and then fails" — and prints nothing else. `--cwd`
and `/cd` share `teton_core::session_root::resolve_cwd_argument(raw, shell_cwd,
home)`, so one grammar table drives both tests (AC-12).

**Rejected**: a `session_root_changed` event *instead of* `context_cleared`
(BR-7 says the existing shape; and every attached client already renders it);
carrying the conversation with re-minted ids (unsafe in general — OQ-2).

### ADR-5: The notice is pure content under the banner; kind comes back from the daemon

`teton::banner::root_notice(root: &SessionRoot) -> Option<String>` returns
`None` for `project` and one line otherwise ("Not inside a project — tools are
scoped to your whole home folder: every search walks all of it, and privacy
boundaries declared for a project do not apply here. Run teton from the
project, `teton --cwd <path>`, or `/cd <path>` here."). It is drawn with
`surface.line(LineKind::Notice, …)` right after `session/create` answers and
before the `ready (freeform)` line, inside the same `if interactive` gate as
the banner — so piped output is byte-identical (ADR-007's TTY clause; the
`banner_text_…fits_a_narrow_terminal` ≤ 60-char rule applies to `banner::lines()`
only, which is why the notice is its own function, not a banner line). BR-8's
re-announce after `/cd` reuses the same function from the event arm.

## Data model / protocol changes (all additive)

| Where | Change |
|---|---|
| `teton-protocol::methods` | `SessionRoot { display, kind: RootKind, project_name: Option<String>, vcs_branch: Option<String> }`, `RootKind { Project, Home, FilesystemRoot, Plain }` (serde `snake_case`); `SessionCreateResult.root: Option<SessionRoot>` (`default`, `skip_serializing_if`); `SessionSetCwdParams { session_id, cwd: PathBuf }` / `SessionSetCwdResult { root: SessionRoot, blocks_dropped: u64 }`, `METHOD = "session/set_cwd"`. |
| `teton-protocol::events` | `Event::SessionRootChanged(SessionRootChanged { previous_display: String, root: SessionRoot })`, wire name `session_root_changed`, no `session_id` field (flatten rule). |
| `teton-core::session_root` (new) | `PROJECT_MARKERS: &[&str]` (`.git`, `.hg`, `.svn`, `Cargo.toml`, `package.json`, `pyproject.toml`, `go.mod`, `pom.xml`, `build.gradle`, `Gemfile`, `mix.exs`, `.adlc`), `classify(path, home, has_marker) -> RootKind`, `display_for(path, home) -> String`, `bounded_field(s, max) -> String`, `middle_elide`, `resolve_cwd_argument(raw, shell_cwd, home) -> Result<PathBuf, CwdArgError>`. Pure, no I/O. |
| `tetond::session_root` (new) | `probe(path, home) -> SessionRoot`: marker probe (`.git` as **file or dir** — this repo's worktrees have a `gitdir:` file), branch from `.git/HEAD` following a `gitdir:` pointer, `None` on detached/unreadable. |
| `tetond::harness::turn_loop::HarnessConfig` | `session_root: Option<SessionRoot>` (default `None`); `environment_block()`; the block after the opener. |
| `tetond::harness::tools::ToolContext` | carries `root: SessionRoot`-ish (path + display + kind) and `WalkPolicy`; `new`, `for_root`, `with_root_kind`, `with_walk_budget`; `root_display()`, `root_kind()`; jail refusal ``path `{raw}` is outside the session root {display}``. |
| `tetond::harness::tools::walk` (new) | as ADR-3. Added to `boundary_coverage.rs` `TOOL_SOURCES` (no `impl Tool`). |
| `tetond::sessions::SessionRegistry` | `set_cwd(&SessionId, PathBuf) -> bool`. |
| CLI | `Cli.cwd: Option<PathBuf>`; `SessionState.root`; `/cd` row (`Args::Optional`) beside `/clear`; `session_ui` arm; `banner::root_notice`. |

## Blast radius (files)

Modify: `crates/teton-protocol/src/{methods,events}.rs`;
`crates/teton-core/src/lib.rs`; `crates/tetond/src/{lib.rs or main.rs mod list,
server.rs, runtime.rs, sessions.rs}`; `crates/tetond/src/harness/{turn_loop.rs,
self_config.md}`; `crates/tetond/src/harness/tools/{mod.rs, glob.rs, grep.rs,
shell.rs, read.rs, edit.rs}`; `crates/tetond/src/egress/redact.rs` (sweep +
note); `crates/tetond/src/harness/tools/web.rs` (sweep test only);
`crates/tetond/tests/{boundary_coverage.rs, symlink_posture.rs, e2e/harness.rs}`;
`crates/teton/src/{main.rs, banner.rs, slash.rs, session_ui.rs, client.rs}`;
`crates/teton/tests/cli_e2e.rs`; `docs/manual-verification.md`; `README.md`.
Create: `crates/teton-core/src/session_root.rs`, `crates/tetond/src/session_root.rs`,
`crates/tetond/src/harness/tools/walk.rs`, `crates/tetond/tests/session_root.rs`,
`crates/tetond/tests/e2e/session_root.rs`.

Tests known to move (and why): `tools/mod.rs:977` + `symlink_posture.rs`
447/480/827/840/855/906 (jail wording); `glob.rs:211` ("no files match");
`grep.rs` `split_cap_notice` test + `match_lines` helper (trailer shape);
`slash.rs:2362` promised list (`cd`); `events.rs` name table +
`methods.rs` METHOD pin list; `server.rs:9707` unmintable-id sweep;
`session_ui.rs` exhaustive match (compile-forced arm); the two ceiling sweeps.

## Applicable lessons

LESSON-473 (cwd rides the protocol), LESSON-477/474 (containment at the
authoring layer; values mid-line, existing neutralizers), LESSON-532/493
(facts not directives; the fact a model needs without a tool call is what earns
a resident line), LESSON-496 (assert headroom, not adjacency — the ≥ 80-byte
margin rule), LESSON-481 (pure content, gated bytes), LESSON-524 (assert
callability at every level), LESSON-491 (measure the rendered prompt at the
consumer — the sweeps, not a formula), REQ-571 BR-1/2/5 (identity minted from
the resolved path; walkers skip links), REQ-582 (one grammar for two
spellings), BUG-147/LESSON-473 (the predecessor).

## Proposed additions to `.adlc/context/architecture.md` (at wrapup)

- **A session's ground is a derived value with one probe and three consumers** —
  the jail root's kind, display and project facts are derived from the stored
  path at every use (turn, create, `/cd`), never cached; the prompt states them
  as data, the tools enforce them, the client renders what the daemon derived.
- **A walker's harness lines wear one prefix and are peeled by one splitter** —
  every non-match line a search tool appends starts `... (`; the duty that
  ranks matches strips them all, so a new harness line is never ranked as a
  match by accident.
- **A resident fact is bought with reference data, never with the ceiling** —
  ADR-2 here is the third instance of the redact.rs rule.

## Task graph

```
T0  TASK-174 protocol types + session/set_cwd + session_root_changed
T1  TASK-175 session-root value: teton-core pure module, tetond probe, ToolContext carries it,
             jail refusals name it, HarnessConfig field (unrendered)          ← 174
T2  TASK-176 walk.rs + glob/grep/shell + five tool docs + AC-6/13..19        ← 175      (owns tools/*.rs except mod.rs's non-slot lines, boundary_coverage.rs)
    TASK-178 daemon wiring: create/set_cwd/registry/runtime + e2e AC-10       ← 174,175  (owns server.rs, runtime.rs, sessions.rs, tetond/tests/e2e)
    TASK-179 CLI: --cwd, banner, notice, /cd, events + AC-8/9/11/12 units     ← 174,175  (owns crates/teton/src/*, README)
T3  TASK-177 environment block + ceiling + guide trim + AC-1..4               ← 176      (owns turn_loop.rs, self_config.md, redact.rs, web.rs test)
T4  TASK-180 integration: cli e2e, headroom record, docs runbook, live A/B    ← 177,178,179
```

Tier-2 tasks run in parallel in **one** worktree, so file ownership is
disjoint by construction (listed per task); an implementer that sees a
compile error in a file it does not own waits and retries rather than
"fixing" it. TASK-177 follows TASK-176 deliberately (ADR-2: the ceiling is
measured against the reworded docs, not an allowance); it may run alongside
a still-running TASK-178/179 since their files are disjoint.
