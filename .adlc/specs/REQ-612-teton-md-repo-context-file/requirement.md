---
id: REQ-612
title: "TETON.md — a per-repository context file the session reads at its root and carries as resident data, so a project's shape is known without a walk"
status: approved
deployable: true
created: 2026-09-03
updated: 2026-09-03
component: "daemon/harness"
domain: "harness"
stack: ["rust", "daemon", "cli", "json-rpc"]
concerns: ["developer-experience", "cost", "security", "privacy"]
tags: ["teton-md", "repo-context", "context-file", "agents-md", "claude-md", "system-prompt", "resident-fact", "environment-block", "session-root", "cd", "reload", "context-budget", "prompt-injection", "untrusted", "provenance", "egress", "boundary", "prefix-cache", "self-config-guide", "capability-claim", "claude-code-compat", "dogfood", "adlc"]
---

## Description

A Teton session starts knowing nothing about the repository it is in. The
system prompt (`build_system_prompt`, `crates/tetond/src/harness/turn_loop.rs`)
is a fixed opener, one environment line (root display, kind, project name,
branch, platform — REQ-583 BR-1), the self-configuration guide, and the tool
docs. Nothing in it describes the project: not its layout, not how it is built
or tested, not which directories matter. Teton also performs **no** repository
scan at launch — the project locator's scan is on demand only (REQ-584 BR-3),
the walkers are bounded and on demand (REQ-583 BR-10) — so the absence is not
a cost saving deferred to startup; it is a cost paid on every session, in
tool calls. Every fresh session, every `/clear` and every `/cd` puts the model
back at zero, and a prompt can spend up to 25 tool iterations — each
re-sending the whole context — grepping and reading its way back to an
understanding of the tree before it reaches the task. On the local tier that
rediscovery competes with the task for a 4,096-word / 32,768-byte budget.

Every comparable agent solves this with a file at the repository root that the
tool reads at startup and keeps resident: Claude Code's `CLAUDE.md`, the
vendor-neutral `AGENTS.md`. REQ-585 named them in its Out of Scope as a
related, separate REQ; this is that REQ. Teton reads neither — the self-configuration guide
says so in a sentence pinned by test (BUG-181, amended by REQ-585 BR-9 and
REQ-587 BR-8: *"Teton loads skills and commands from `.claude/` and `~/.claude`
but nothing else there (no CLAUDE.md, agents or hooks)"*). LESSON-543 names the
general rule this REQ applies: a fact the model is asked about repeatedly
should be resident in the prompt, not rediscovered from whatever is in front
of it.

This REQ adds **`TETON.md`**: a markdown file at the session root that the
daemon reads when the session is created and when its root moves, bounds to a
pinned byte cap, and places in the system prompt as **repository-authored
data about the project** — resident in every turn of every tier, like the
environment line, and unlike it, multi-line and written by the repository.
That last clause is the whole difficulty, and the rules below are mostly
about it:

- it is **user-controlled content landing in the system prompt** — the trust
  class BUG-148 named for a server-supplied tool description, and LESSON-477's
  authoring-layer split applies: every structural guard the tool-result
  envelope gets, this block gets, where its frame is written;
- it is **repository content that egresses** on every remote turn, so it has
  to carry the file's provenance the way a `read` of it would — today a
  system-prompt block contributes *nothing* to egress provenance
  (`context_provenance` ignores `CtxProvenance::System`), which would make a
  resident file the one path around a `local-only` boundary;
- it is **resident bytes on a budget that has 129 bytes of recorded margin**
  (`RECORDED_PROMPT_MARGIN_BYTES`), so the block cannot land without the
  reviewed ceiling move BUG-181 and REQ-587 each took, and the local tier's
  32 KiB window makes the cap a product decision, not a constant.

What this REQ deliberately is **not**: a `CLAUDE.md` runtime. There are no
`@file` imports, no nested per-directory files, no user-level
`~/.teton/TETON.md`, no instructions the file can give the harness (permission
level, routing, effort, boundaries — inert, as REQ-585 BR-5 makes skill
frontmatter inert), and no acknowledgment prompt: the file is framed as
*description*, not *instructions*, precisely so it needs none (see BR-4 and
OQ-2 for the argument, and Out of Scope for the deferred instruction-bearing
variant).

## System Model

_Shapes are illustrative — names are `/architect`'s; the constraints are the
requirement._

### Entities

| Entity | Field | Type | Constraints |
|--------|-------|------|-------------|
| RepoContextFile | path | root-relative path | exactly `TETON.md` at the session root (fallback names per OQ-1); never a parent directory's, never a subdirectory's, never followed through a symlinked entry (REQ-571 BR-5's walker rule) |
| RepoContextFile | source_kind | enum: `teton_md` \| `agents_md` | which name was read; the frame names it (BR-4) |
| RepoContextFile | bytes_on_disk | usize | as read; a file over the read ceiling (illustratively 256 KiB) is not read past it |
| RepoContextFile | mtime, len | timestamp, usize | the staleness key BR-6 compares at the start of a prompt turn; one `stat`, no content read unless they differ |
| RepoContextFile | provenance | `ProvenanceId` | minted from the root-relative resolution the file was read under, exactly as `read` mints one — the identity egress and the boundary matcher judge (BR-5) |
| RepoContextBlock | text | string | the rendered block: harness frame line, the file's text (sanitized, bounded), harness closing line; a pure function of (RepoContextFile, cap) |
| RepoContextBlock | cap | pinned constant, bytes | the resident byte cap (BR-3); measured at the cap by both resident-ceiling sweeps |
| RepoContextBlock | truncated | bool | true when the file exceeded the cap; the block ends with a harness marker naming the cap and the bytes dropped, and the surface says so (BR-3) |
| RepoContextState (per session) | state | enum: `absent` \| `loaded` \| `truncated` \| `withheld_boundary` \| `withheld_off` \| `unreadable` | what `/context` reports; `withheld_*` and `unreadable` name their reason |
| Config `[context]` (durable) | repo_file | bool, default `true` | the durable switch; `false` means the file is never opened (the REQ-611 BR-1 posture: off means the mechanism does not run) |
| Session switch | `/context on\|off` | session-scoped | overrides the durable default for this session only, never written (REQ-611 BR-2's two-switch shape) |

### Events

| Event | Trigger | Payload |
|-------|---------|---------|
| `repo_context_state` (new, additive) | the file was read into the resident block at `session/create`, in `/cd`'s rebuild, or by BR-6's staleness re-read — **or** it exists and was not made resident (boundary, switch off, unreadable) | session id, state (`loaded` \| `truncated` \| `absent` \| `withheld_boundary` \| `withheld_off` \| `unreadable`), source_kind, bytes_on_disk, resident bytes, truncated, bounded reason |

_Architecture (ADR-6) folded the two events this table first named — `repo_context_loaded`
and `repo_context_withheld` — into one event whose `state` field carries the distinction; the
ACs below that say "loaded" mean this event with a `loaded`/`truncated` state._
| `route_decided` (existing) | unchanged; `/verbose` may append the resident context bytes to its budget line | — |

Older clients ignore unknown events (the REQ-573 additive rule); the CLI
renders each as one line.

### Permissions

| Action | Roles Allowed |
|--------|---------------|
| the daemon reading `TETON.md` at the session root | automatic at every permission level, including `plan` — it is a read of a file inside the jail, the posture `read` already has (LESSON-524); no prompt |
| the file's text reaching the system prompt | only through the RepoContextBlock renderer, sanitized and bounded (BR-4); never verbatim |
| the file changing permission level, routing, effort, config, a boundary, the cap, or the switch | never — it is data; nothing in it is parsed as a setting |
| `/context on\|off`, `/context` | the user at the session surface, TTY or pipe; unreachable from a tool call or observed content (REQ-611 BR-3, REQ-572 BR-4) |
| `[context] repo_file` | the user, through the same config gate every durable write meets |
| the model reading `TETON.md` with `read` | unchanged — allowed as any in-jail file; a duplicate of the resident text is the model's to spend |

## Business Rules

_Leg A — what is read, and when_

- [ ] BR-1: **One file, one place, read at two moments and never walked
  for.** The daemon reads exactly `<session-root>/TETON.md` (and OQ-1's
  fallback name when it is absent) — no parent-directory search (a parent is
  outside the jail and a file outside the jail is never read), no
  subdirectory files, no imports, no recursion. The read happens at
  `session/create` and again after every `session_root_changed` (`/cd`), the
  same seam skill discovery runs at (REQ-585 BR-1); there is no file watcher.
  A symlinked entry is not followed (REQ-571 BR-5); an `EPERM`/TCC refusal is
  a named `unreadable` state, not a crash and not silence; a missing file is
  the normal case, costs one `stat`, and produces a system prompt
  **byte-identical to today's** apart from BR-8's amended sentence. Only a
  `project`-kind root is read (REQ-583 BR-4); whether `TETON.md` itself
  should be a project marker is OQ-3 (informed by REQ-583, REQ-585, REQ-584
  BR-3).
- [ ] BR-2: **Off is a switch, and off means unopened.** `[context]
  repo_file = false` (durable, default `true`) means the daemon never opens
  the file: zero reads, no block, no event. `/context off` does the same for
  the current session only and is never written; `/context on` re-reads at
  once. Bare `/context` prints the state — file, source, bytes on disk,
  resident bytes, cap, and which of `loaded` / `truncated` / `absent` /
  `withheld …` / `unreadable` applies — and works on a pipe (REQ-560 BR-10's
  rule: a fact the status line might show has a non-visual read path)
  (informed by REQ-611 BR-1/BR-2, REQ-560).

_Leg B — bounded, and the bound is stated_

- [ ] BR-3: **The resident block has a pinned byte cap, the cap is paid for
  with a reviewed ceiling move, and exceeding it is never silent.** The block
  is bounded by a pinned constant chosen so that the widest system prompt
  this build produces **plus a block at the cap** still leaves a floored
  route (2,048 words / 16,384 bytes — REQ-586's smallest pair, "the smallest
  that still holds the system prompt") room for a prompt and a reply. **The cap
  is 8 KiB** (product decision 2026-09-03: solid context over cheap context),
  and it is **route-aware**: the effective cap on a route is the smaller of
  8 KiB and a quarter of the route's byte budget (REQ-586's per-route pair),
  so the local tier carries the full 8 KiB (its budget is 63,488 bytes since
  REQ-590, so the notes are about an eighth of it), a floored 16,384-byte
  route carries 4 KiB, and no route ever spends more than a quarter of its
  context on the notes. The effective cap is derived where the route is decided, like
  the budget itself, and `/verbose` prints it beside the resident bytes. A file over the cap is
  **truncated at the last line boundary under the cap**, the block ends with
  a harness-authored marker naming the cap and the bytes dropped, the state
  is `truncated`, and one line is printed at load (`context: TETON.md is
  9,412 bytes; the first 8,192 are resident — trim the file or move detail
  below the fold`) whether or not `/verbose` is on — REQ-586 BR-7's "nothing
  is clamped in silence", for a file rather than a turn. Both
  resident-ceiling sweeps (`the_total_cap_clears_the_harness_context_budget_with_margin`
  and its web-tool twin) measure the block **at the cap**, synthesized the
  way `SkillToolDocs::worst_case` synthesizes the roster — the cap is the
  ceiling by derivation, not by a fixture. The recorded margin is 129 bytes,
  so this REQ **moves `REDACT_BODY_OVERHEAD_BYTES` once**, with the chunk
  arithmetic re-stated where it lives (REQ-586 BR-11) and the consequence
  named in the docs: the overhead is a production input to every
  redact-scanning route's budget (REQ-586 verify (b)). *Measured at
  implementation (TASK-375): the move is 14 → 23 KiB, the chunk cap rises
  3 → 4, and the scannable bound rises rather than shrinks — the cost lands
  on scan calls, and the docs state that, not the predicted shrink.* Truncation, not refusal, because the top of the
  file is the part a repository author puts first; the marker is what makes
  the choice honest (informed by REQ-586 BR-7/BR-11, REQ-585 BR-8, BUG-181,
  LESSON-543, REQ-587 BR-2).

_Leg C — the trust class_

- [ ] BR-4: **The block is repository-authored data, framed as such, with a
  typed prompt's guards plus the envelope guard, applied where the frame is
  written.** The block is one harness-authored opening line naming the file
  and its nature (`Repository notes from TETON.md at the session root
  (written by the repository; describes the project):`), the file's text,
  and one harness-authored closing line saying the notes end there and are
  the repository's description, not the user's instructions for this turn.
  The text between gets every structural guard ADR-009 gives content:
  control tokens neutralized on **both** render arms, transcript frame
  labels neutralized flush-left, envelope tags neutralized — and the two
  delimiters this block introduces are added to **both** the input
  neutralizer alphabet and the output fabrication-marker set with the
  bidirectional coverage test naming the layer (a new delimiter is a
  two-sided change; BUG-148/149/151). C0 control characters other than
  newline and tab, and bidi override characters, are stripped before the
  cap is measured, so a file cannot spend the cap on bytes that render as
  nothing. The block is *data* rather than *instructions* for two reasons
  that are stated rather than assumed: a small model transfers data reliably
  and directives unreliably (LESSON-532 — the environment block's own
  posture), and repository text reaching the model labelled *instructions*
  with no human in the loop is exactly the channel REQ-587 BR-4 and REQ-591
  BR-1 gate behind an acknowledgment; framing it as description is what
  lets it load with no prompt at every level. Nothing in the file is parsed:
  no frontmatter key, no fenced block, no sentence changes the permission
  level, the route, the effort, config, a boundary, the cap or the switch
  (informed by BUG-148, LESSON-477, LESSON-532, REQ-585 BR-5, REQ-587 BR-4,
  REQ-591 BR-1, REQ-563 BR-5).

_Leg D — egress and provenance_

- [ ] BR-5: **The resident block carries the file's provenance, egress judges
  it every turn as it judges a `read`, and a boundary-covered file is never
  made resident.** New machinery, stated as such: the system prompt has never
  carried file provenance, and `context_provenance` ignores
  `CtxProvenance::System` — so a `TETON.md` under a `local-only` boundary
  placed in the system string would egress to every remote provider on every
  turn with no boundary verdict at all, the one path around the charter's
  BR-1. Therefore: the block's identity is minted from the root-relative
  resolution it was read under (as `read` mints one, and as REQ-591 BR-4
  mints trust from the resolution the bodies were read under), it joins the
  context-provenance union every turn, and the egress inspector applies the
  boundary matcher to it as to any block. At load, a file whose identity the
  configured boundaries cover is **not made resident** (`withheld_boundary`),
  one line says so (`context: TETON.md is inside a local-only boundary and
  was not loaded — a session-long pin is not what a boundary means`), and the
  model may still `read` it, pinning that one turn as any read would. A
  boundary configured mid-session that comes to cover the resident file is
  judged by egress like every block — the turn pins local, and the pressure
  line names the file (OQ-4 asks whether to drop it instead). The
  `[privacy] redact` scan sees the block as it sees the rest of the body.
  This is a charter BR-1 claim and carries an egress-capture test (informed
  by REQ-585 BR-7, REQ-591 BR-4, REQ-583 OQ-1, REQ-560 BR-3, LESSON-501).

_Leg E — reload semantics_

- [ ] BR-6: **Fresh at the start of a prompt turn, stable inside one, moved
  with the root.** At the start of every prompt turn the daemon `stat`s the
  file (one syscall; the session-root pattern of deriving at every use,
  REQ-583 ADR-1) and re-reads it only when `mtime` or `len` differ from the
  loaded copy — so an edit to `TETON.md`, by the user in another window or by
  the model's own `edit` tool, is resident on the next prompt without a
  command. Never mid-turn: a prompt's tool loop runs its iterations under one
  system prompt. `/cd` re-reads under the new root (BR-1) and, because it
  clears the conversation (REQ-583 BR-7), the new block starts the new
  conversation; `/clear` keeps the block — it is system prompt, not
  conversation. The local tier's prefix cache is keyed by the system prompt
  and invalidates only when the block's bytes change, which is the same
  cadence the skill roster already has ("launch, `/cd`" — REQ-587 BR-2) plus
  the user's own edits; a `/verbose` session prints one line when a re-read
  happens (informed by REQ-583 ADR-1/BR-7, REQ-585 BR-1, REQ-587 BR-2,
  REQ-564/REQ-567 BR-7).

_Leg F — cost, and what the model is told_

- [ ] BR-7: **Resident bytes are visible and attributed, and the worst case
  is stated.** `/verbose`'s route line names the resident context bytes
  beside the budget; `/context` names them on a pipe; `teton doctor`
  advises on a truncated or withheld file. The docs (`teton_docs context`
  and the README) state the cost shape: the block rides every model call of
  every iteration, so a prompt that runs to its `max_turns` (12 on the local
  profile, 40 on the strong-model profile — `teton_docs context`'s "25" is a
  stale figure this REQ corrects in passing) carries an 8 KiB block 12 to 40
  times, and on the local tier it is about an eighth of the byte budget
  (a quarter on a floored route) —
  which is why the cap is small and why the file should hold the facts a
  session needs every time (layout, build and test commands, conventions)
  and not the ones it needs once (informed by REQ-586 BR-9, LESSON-543).
- [ ] BR-8: **The guide tells the truth about the file, in the one sentence
  that already names the negative space, inside its constraints.** The
  self-configuration guide's capability sentence is amended so it stays true
  in both directions: Teton loads skills and commands from `.claude/` and
  `~/.claude` **and the repository notes from `TETON.md` at the session
  root**, and still no `CLAUDE.md`, agents or hooks (or, if OQ-1 admits a
  fallback, the sentence names it). The pinning test
  `the_system_prompt_states_what_the_session_can_run_and_from_where` is
  amended with its needles — re-worded, never deleted, the BUG-181 →
  REQ-585 BR-9 → REQ-587 BR-8 lineage — and the sentence keeps the guide's
  constraints: one `/help` line, both paths named, the scoped who-runs
  clause, no second "ask" line, no `teton …` shell form, and the headroom
  floor (`MIN_PROMPT_HEADROOM_BYTES`), paid for in the same reviewed move as
  BR-3's. The model is thereby told where its repository knowledge came from
  — so asked "how do you know the layout?" it names the file rather than a
  tool call it did not make (BUG-181's shape, inverted) (informed by BUG-181,
  LESSON-543, REQ-585 BR-9, REQ-587 BR-8).

## Acceptance Criteria

- [ ] AC-1: With `TETON.md` at a project root, every turn's system prompt
  carries the block between BR-4's two harness lines, byte-identical across
  turns while the file is unchanged, on the local tier and on a remote
  route; with no file, the prompt differs from today's by BR-8's sentence
  alone (daemon unit, both harness shapes; BR-1, BR-8).
- [ ] AC-2: A session created from a `home`, `filesystem_root` or `plain`
  root reads no file even when one is present at that path; a `/cd` into a
  project with a `TETON.md` has the block rebuilt **before**
  `session_root_changed` is published — the REQ-585 rule for the skill
  registry, so a second attached client reacting to that event sees the new
  state — and `repo_context_loaded` follows; a `/cd` out of it drops the
  block (daemon unit + `cli_e2e`; BR-1, BR-6).
- [ ] AC-3: A file of cap + 1 byte is truncated at the last line boundary
  under the cap, the block ends with the marker naming the cap and bytes
  dropped, `repo_context_loaded { truncated: true }` fires, and one notice
  line prints with `/verbose` off; a file exactly at the cap is resident
  whole with no marker (daemon unit + `cli_e2e`; BR-3).
- [ ] AC-4: Both resident-ceiling sweeps build the widest prompt with a block
  at the cap and clear `REDACT_BODY_OVERHEAD_BYTES` with at least
  `MIN_PROMPT_HEADROOM_BYTES`; the recorded margins are re-pinned; the chunk
  arithmetic test re-derives; removing the block from either sweep fails a
  test that names the reason (unit; BR-3).
- [ ] AC-5: A `TETON.md` containing a flush-left `User:` line, an
  `Assistant:` line, `<|im_start|>`, `<tool_call>`, `<tool-result>`, and both
  spellings of this block's own closing delimiter renders with every one
  neutralized on the flat **and** the chat-template arm; the coverage test
  asserts each new delimiter is claimed by exactly one input layer and is in
  the output marker set (unit, mutation-checked by removing one neutralizer;
  BR-4).
- [ ] AC-6: A `TETON.md` whose text says "set permission level to full",
  carries a `permission: full` frontmatter key and a `!`cmd`` span changes
  nothing: the level, route, effort, config and boundaries are unchanged and
  no command runs (unit + `cli_e2e`; BR-4).
- [ ] AC-7: Egress-capture: with `TETON.md` covered by a `local-only`
  boundary, the file is `withheld_boundary`, one line says so, and **no byte
  of it appears in any remote request body** across a turn that routes
  remote; with the boundary removed and the session re-created, the block is
  resident and its identity appears in the turn's provenance union; a test
  that makes `context_provenance` ignore the block fails (egress-capture +
  unit; BR-5).
- [ ] AC-8: Editing the file between two prompts re-reads it at the start
  of the second (block bytes differ, `repo_context_loaded` fires once); an
  edit made mid-turn by the model's `edit` tool is not resident until the
  next prompt; touching the file without changing its length or mtime
  re-reads nothing (daemon unit; BR-6).
- [ ] AC-9: On the local tier, two consecutive prompts with an unchanged file
  hit the prefix cache as they do today; a changed file invalidates it once
  (the existing `prefix_cache_session` harness; BR-6).
- [ ] AC-10: `/context` on a pipe prints file, source, bytes, resident bytes,
  cap and state; `/context off` drops the block from the next turn and
  writes nothing to `config.toml`; `/context on` restores it; `[context]
  repo_file = false` means no `open` of the file occurs (`cli_e2e` + a
  syscall-level or seam-level assertion; BR-2).
- [ ] AC-11: `the_system_prompt_states_what_the_session_can_run_and_from_where`
  passes with its re-worded needles; the guide has exactly one `/help` line;
  `cli_rows.rs`'s cross-check finds no shell form; a `teton doctor` run
  against a truncated file and a withheld file advises on each (unit +
  `cli_e2e`; BR-8, BR-7).
- [ ] AC-12: `teton_docs context` (or a new topic — architecture) documents
  the file, the cap, the switch, the reload rule and the cost shape; the
  README's session-command table gains the `/context` row; the docs state
  the overhead consequence for redact-scanning routes (docs; BR-7).
- [ ] AC-13: Dogfood, by hand, recorded in `docs/manual-verification.md`: in
  this repository with a `TETON.md` describing the crate layout, the first
  prompt of a fresh session that asks "where does the system prompt get
  built?" answers from the notes with **zero** `glob`/`grep` calls on the
  local tier; the same prompt without the file makes at least one (BR-7,
  LESSON-543).

## External Dependencies

- None new. The `AGENTS.md` convention (OQ-1) is a file-name convention, not a
  library.

## Assumptions

- ASSUME-1: A small local model uses resident repository notes the way it
  uses the environment line — as facts it transfers into its answers — rather
  than ignoring them; LESSON-532's data-over-directives finding is the
  evidence, and AC-13 is the check. If the local tier does not reach for the
  notes, the cap is spent for nothing on that tier and BR-2's switch is the
  remedy until a better frame is found.
- ASSUME-2: An 8 KiB cap holds a solid description of a repository (layout,
  build/test commands, conventions, the things a new contributor asks first).
  This repository's own overview is `.adlc/context/project-overview.md` at
  roughly 3 KiB of prose, so 8 KiB is about twice the calibration point; the
  route-aware quarter rule is what keeps it honest on a small window.
- ASSUME-3: Loading the file with no acknowledgment at every permission level
  is sound **because** the block is framed as data and structurally
  neutralized — the same argument under which `read` returns repository text
  at `plan`. If a later REQ lets the file carry instructions, that REQ owes
  the REQ-591 gate (Out of Scope).
- ASSUME-4: The per-turn `stat` (BR-6) is negligible against a turn's other
  syscalls; REQ-583 already re-derives the root's facts at every use.
- ASSUME-5: A `TETON.md` at the root of a project is not, in practice, under a
  `local-only` boundary — boundaries are `secrets/**`-shaped — so BR-5's
  withheld state is a correctness backstop, not a common path.

## Open Questions

- [ ] OQ-1: **Fallback names.** Read `AGENTS.md` when `TETON.md` is absent?
  Read `CLAUDE.md`? Recommendation: precedence `TETON.md` → `AGENTS.md`, and
  **no** `CLAUDE.md` in v1. `AGENTS.md` is the vendor-neutral convention and
  is written as a description of the repository; `CLAUDE.md` files are
  written for Claude Code and name its tools, hooks and `/` commands, which
  is BUG-181's failure shape delivered by the repository — a model beside one
  affirms capabilities Teton lacks. If `CLAUDE.md` is wanted, make it an
  explicit `[context] fallbacks = [...]` opt-in and keep the guide's sentence
  true for the default. Whichever is chosen, the frame line names the file
  actually read (BR-4) and the guide names it (BR-8).
- [ ] OQ-2: **Data or instructions?** BR-4 frames the block as description
  and therefore loads it with no acknowledgment. The alternative is
  `CLAUDE.md`'s posture — instructions to follow — which would require the
  REQ-591 project-trust acknowledgment (once per session per root, with the
  durable `trusted_project_roots` allowlist for unattended sessions) before
  the block is resident, and would put a consent prompt at the start of
  every guarded session in every untrusted checkout. Recommendation: data in
  v1; revisit when the ADLC dogfood shows a fact the file cannot carry as a
  description.
- [ ] OQ-3: **Is `TETON.md` a project marker?** A directory holding only a
  `TETON.md` and no VCS or manifest is `plain` today and would not be read
  (BR-1). Recommendation: add `TETON.md` to REQ-583's ProjectMarker table —
  a person who wrote one is telling Teton this is a project — and accept that
  it also flips the launch notice and the locator's registry for that
  directory.
- [ ] OQ-4: **A boundary that comes to cover the resident file mid-session.**
  BR-5 leaves egress to pin the turn and name the file. Should the daemon
  instead drop the block at the next turn start (BR-6's `stat` could also
  re-check the boundary) and say so? Recommendation: yes, at the turn-start
  seam, with the same withheld line — a session-long silent pin is what
  BR-5's load-time rule exists to prevent.
- [ ] OQ-5: **Command name.** `/context` collides in spirit with the
  `teton_docs context` topic (the budget). Alternatives: `/notes`,
  `/repo`. Architecture/product; the rules above use `/context`
  illustratively.
- [x] OQ-6: **Should the cap be a config knob** (`[context] max_bytes`)?
  Resolved 2026-09-03: pinned at 8 KiB, route-aware by the quarter rule (BR-3).
  A user knob that can exceed the floored pair would reopen the silent
  overflow REQ-586 closed; the quarter rule gives the big-window user nothing
  extra because the file, not the window, is the limit.

## Out of Scope

- **An instruction-bearing variant** (`CLAUDE.md` semantics: "always run X
  before committing") and the REQ-591 acknowledgment it would need. Separate
  REQ if the description posture proves insufficient.
- `@file` imports, nested per-directory context files, and a user-level
  `~/.teton/TETON.md` (or `~/.claude/CLAUDE.md`) merged with the project's.
- A generated file: `teton context init` writing a starter `TETON.md` from
  the tree, or any repository scan to synthesize one. In this REQ the file is
  authored; **REQ-613** generates one when a project has none and depends on
  this REQ's loader, cap, frame and `[context]` table.
- Any change to the environment line's format or bound (REQ-583 BR-1,
  REQ-584 BR-7), to the skill roster, or to `teton_docs` topic ceilings.
- Honoring frontmatter, YAML, or any structured key inside the file.
- Windows.

## Retrieved Context

Retrieval query: component `daemon/harness`, domain `harness`, stack
`[rust, daemon, cli, json-rpc]`, concerns `[security, privacy,
developer-experience, cost]`, tags as in the frontmatter. 261 candidates,
251 scored, 57 of 57 specs admitted by the status filter (all `complete`).

- REQ-587 (spec, score 28): Model-invoked skills — a `skill` tool lets the model expand a registered skill into its own turn as a tool result
- REQ-585 (spec, score 19): User-defined slash commands from SKILL.md — the session discovers Claude Code-style skills and runs `/name` as a prompt expansion
- REQ-583 (spec, score 19): Session-root awareness and bounded discovery — the agent knows where it is, the user is told when it is nowhere, and a search cannot become a disk crawl
- REQ-584 (spec, score 16): A project locator — the session can name this machine's projects without walking the disk
- REQ-586 (spec, score 16): A turn's context budget follows its route — remote tiers get the provider's window, bounded by what the redact scan can cover, and nothing is clamped in silence
- REQ-563 (spec, score 15): Opt-in web lookup through the egress choke point
- BUG-181 (bug, score 14): The model affirms capabilities Teton does not have
- LESSON-543 (lesson, score 13): A model answers 'can you do X?' from whatever is in front of it — every class of question a user asks about the product needs its own resident fact
- BUG-190 (bug, score 12): A `$ARGUMENTS` splice puts the caller's bytes inside the region the frame certifies as instructions
- REQ-560 (spec, score 12): Named permission levels and the interactive session status line
- LESSON-495 (lesson, score 12): A remembered grant answers every question its key matches — so the key must encode the whole question
- REQ-591 (spec, score 11): The project-skill trust gate and its unattended allowlist
- LESSON-552 (lesson, score 11): A test that hands the minter its input never exercises the derivation that got it wrong
- REQ-572 (spec, score 11): Capability-aware refusals and guided in-session enablement
- REQ-611 (spec, score 10): Daemon-side transcript logging: an opt-in, per-session JSONL record

*Retrieval note: the delegate body-read (`adlc-read`) timed out after 300 s on
this 15-doc corpus, as it did for REQ-591; the fallback was taken and recorded
as `api-error` in telemetry. All five lesson and bug bodies were read in full;
the ten specs were read as their Description, System Model and Business Rules
sections (the acceptance-criteria tails were cut by the output cap). Stated
rather than hidden.*
