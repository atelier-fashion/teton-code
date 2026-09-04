---
id: REQ-613
title: "Teton writes TETON.md when a project has none — a consented, bounded, provenance-carrying draft the session then loads"
status: complete
deployable: true
created: 2026-09-03
updated: 2026-09-04
component: "daemon/harness"
domain: "harness"
stack: ["rust", "daemon", "cli", "json-rpc", "llm-providers"]
concerns: ["developer-experience", "security", "privacy", "cost"]
tags: ["teton-md", "repo-context", "context-file", "generate", "generation", "draft", "init", "permissions", "consent", "session-grants", "walk-bounds", "project-detection", "session-root", "duty", "digest", "routing", "provenance", "egress", "cost-attribution", "unattended", "automation", "piped-stdin", "no-clobber", "claude-code-compat", "dogfood", "adlc"]
---

## Description

REQ-612 gives a session resident knowledge of its repository from a `TETON.md` at the session
root — *if one exists*. The product owner's decision, reaffirmed on 2026-09-03, is that the
file's existence must not depend on someone having written it: **when a session starts in a
project and there is no `TETON.md` (and no `AGENTS.md`, which REQ-612 also reads), Teton writes
one itself and then loads it.** The first session in any repository should end with the same
resident notes the tenth one has, without a person authoring a file first. REQ-585 named this
feature in its Out of Scope as "probably the next ask", and REQ-612 deferred it under "A
generated file"; this REQ is that file.

The naive version of this — walk the tree at launch, call a model, write the result — breaks
four rules this codebase already holds, and the whole spec is about doing it without breaking
them:

- **It is a write into the user's working tree.** No harness tool creates files (the `edit` tool
  refuses an empty `old_string` by design), and every mutation a model performs goes through the
  permission gate under a key that encodes the question (LESSON-495). This write is a daemon act,
  not a model tool call, and it gets its own key, its own prompt naming the path, and the same
  level table every other mutation obeys: `plan` never writes.
- **Nothing scans at launch.** REQ-584 BR-3 is explicit — a walk of the tree runs only when
  something asked. The evidence walk therefore runs only *after* consent, under the project
  locator's budget rather than a tool walk's, over a fixed evidence set.
- **The draft is a model call carrying repository content.** It routes through the duty
  machinery like any harness-known call, and the evidence's provenance travels with it so a
  `local-only` file's bytes never reach a remote provider — the charter's BR-1, again.
- **The result is user-controlled content that REQ-612 will make resident.** The written file
  is bounded to REQ-612's cap *before* it is written, so the generated file is never one the
  loader has to truncate, and it begins with a line saying Teton wrote it.

**The posture, stated by the owner on 2026-09-03: solid context, not cheap context.** The
draft is produced once per repository and then read on every turn of every session, so the
evidence gathered for it and the model asked to write it are sized for quality. The walk after
consent is a full listing under a tool walk's budget, not the locator's shallow scan; the
manifests and README are read whole (bounded), not as two-kilobyte heads; entry-point files
contribute their headers; and the draft routes by default to the tier the policy reserves for
deep reasoning. Cheapness is enforced only where it protects a rule that is not about cost —
nothing before consent, nothing outside the jail, nothing covered by a boundary.

Timing follows from the first rule: a permission prompt rides a turn, and `session/create` is
not one. The offer is therefore raised on the **first prompt turn** in a project whose notes are
absent (the launch banner says so a line early), and on demand through `/context init` and
`teton context init`. Unattended sessions never see a prompt they cannot answer: piped stdin
refuses without reading a line (REQ-585 BR-11's rule), and automation opts in durably.

## System Model

_Shapes are illustrative — names are `/architect`'s; the constraints are the requirement._

### Entities

| Entity | Field | Type | Constraints |
|--------|-------|------|-------------|
| Config `[context]` | generate | enum: `ask` \| `always` \| `never`, default `ask` | durable; `never` means no offer, no walk, no call; `always` means write without the prompt where the level would ask — the unattended opt-in |
| GenerationOffer (per session, per root) | root, path | `SessionRoot`, root-relative `TETON.md` | raised once per session per root when REQ-612's state is `absent` and generation is not `never`; not raised at `plan` |
| GenerationOffer | outcome | enum: `accepted` \| `declined` \| `refused_unattended` \| `denied_level` \| `suppressed` | `declined` is remembered for the session and root only — Teton never remembers a permission answer across sessions |
| EvidenceSet | listing | the root's full hierarchy, breadth-first, root-relative, every depth | REQ-583's skip set and symlink rule; under [`EvidenceBudget`]; rendered for the model as a tree with per-directory file counts by extension (a language profile), so a deep `src/main/java/com/…` layout is seen whole; directory names are metadata (REQ-583 OQ-7) |
| EvidenceSet | documents | (path, whole text ≤ 16 KiB each) for each present member of the closed **EvidenceFile** table | README (`README.md`, `README`, `README.txt`), `CONTRIBUTING.md`, `ARCHITECTURE.md`, `Cargo.toml` and every workspace member's `Cargo.toml`, `package.json` (and workspace members'), `pyproject.toml`, `setup.py`, `go.mod`, `Makefile`, `justfile`, `CMakeLists.txt`, `build.gradle`, `pom.xml`, `Gemfile`, `composer.json`, `mix.exs`, `Package.swift`, `Dockerfile`, `docker-compose.yml`, the names of `.github/workflows/*`, `.adlc/context/project-overview.md` and `.adlc/context/architecture.md`; one table, exercised by name in tests |
| EvidenceSet | entry_points | (path, first ≤ 4 KiB) for each present member of the closed **EntryPoint** table at any depth | `lib.rs`, `main.rs`, `mod.rs`, `index.ts`/`.js`/`.tsx`, `main.ts`/`.js`, `__init__.py`, `main.py`, `app.py`, `main.go`, `App.swift`, `Main.java`, `Program.cs` — the module-doc and import header of each, which is where a codebase says what it is |
| EvidenceSet | provenance | `ToolProvenance::Sources` | never `Unknown`; a member whose identity a boundary covers is **excluded** before the call, and the exclusion is counted |
| EvidenceBudget | max_entries, max_wall | REQ-583's **tool walk** budget (100,000 entries, 10 s) — the walk runs after consent, so REQ-584's launch-scan bound does not apply | a budget stop is stated in the draft's header line and the surface line |
| EvidenceBudget | max_bytes | the draft route's context budget (REQ-586) less the drafting prompt and the answer reservation | the evidence is assembled in priority order — tree, manifests, README, entry points, the rest — and cut at the budget with the cut stated in the header; never middle-elided in silence |
| Draft | text | string | the model's answer, stripped and bounded exactly as REQ-612 bounds a file, at REQ-612's cap **minus** the header line |
| Draft | cost | `CostRecord` | one row, purpose named (`repo_context_draft`, illustrative), visible in `/cost` and `/verbose` |
| GeneratedFile | header | one line | states Teton generated it, when, that it is meant to be edited, and that Teton reads it at session start; counted inside the cap |
| GeneratedFile | write | create-new, no clobber | written only if no `TETON.md` exists at write time; a partial write is removed on failure |
| RepoContextState (REQ-612, extended) | origin | enum: `authored` \| `generated` | the loader learns nothing else from the origin; `/context` shows it |

### Events

| Event | Trigger | Payload |
|-------|---------|---------|
| `repo_context_generation` (new, additive) | each outcome of an offer or an `init`: offered, declined, refused unattended, denied by level, walking, drafted, written, failed | session id, outcome, root display, evidence entries and excluded count, draft bytes, failure reason (bounded) |
| `repo_context_state` (REQ-612) | the written file is loaded, the same turn | unchanged shape, `origin: generated` |
| `permission_requested` / `permission_decided` (existing) | the offer at a level that asks | subject: a new `PermissionSubject` variant naming the path and the evidence budget |

Older clients ignore the unknown event; a REQ-612-vintage client refuses the unknown
`PermissionSubject` variant (its `#[serde(other)] Unrecognized` arm is a refusal, REQ-587's
finding), so on such a client the offer is refused and the session proceeds cold — stated, not
hidden.

### Permissions

| Action | Roles Allowed |
|--------|---------------|
| raising the offer | the daemon, on the first prompt turn after create or `/cd` when the state is `absent`, and on `/context init` / `teton context init` |
| answering the offer | the connection that submitted the turn, through the gate, at `guarded` and `edits`; the answer's scopes are once / for this session under the key `repo_context:generate:<root>` (LESSON-495: the key carries the root the human was deciding about) |
| the write at `plan` | never — no offer is raised, one line says how to get one |
| the write at `full` | without asking, as `full` runs every mutation unprompted |
| the write on piped stdin at a level that asks | refused by the client without reading a line; the session proceeds cold |
| the write when `generate = always` | at every level except `plan`, without the prompt — the automation posture |
| the write when `generate = never` | never; `/context init` still works (it is the user's explicit act) |
| the model | cannot raise, answer, force or suppress the offer; no tool reaches any of it |
| the evidence walk | only after consent, under the budget, inside the jail |

## Business Rules

_Leg A — when, and with whose say-so_

- [ ] BR-1: **The offer is raised once per session per root, on a prompt turn, never at
  create.** When REQ-612's state for the root is `absent` (no `TETON.md`, no `AGENTS.md`; an
  empty or whitespace-only file counts as **present** for this rule — it is the documented way
  to stop the offer), the config is not `never`, and the level is not `plan`, the daemon raises
  the offer on the **first prompt turn** after `session/create` or `/cd`, before that turn's
  own model call. A declined offer is not raised again for that root in that session; a `/cd`
  to a different project raises it for the new root. The launch banner (and the `/cd` line)
  says `no TETON.md here — Teton will offer to write one on your first prompt` so the prompt is
  not a surprise. Teton never remembers the answer across sessions; the durable ways to stop
  the offer are `generate = never` and an existing (even empty) file (informed by REQ-583
  BR-5/BR-7, REQ-612 BR-1, LESSON-543).
- [ ] BR-2: **The write goes through the permission gate under its own key, and the prompt
  names what it will do.** The subject names the root-relative path, says Teton will read the
  listing two levels deep and the build manifests, call a model, and write one file; the gate's
  options are the existing once / for this session / no. The key encodes the root
  (`repo_context:generate:<root>`), never the tool name or a skill key, so a remembered answer
  for one repository does not answer for another (LESSON-495). Level table: `guarded` ask,
  `edits` ask, `plan` deny (no offer, one line), `full` allow. On piped stdin, or any
  `NoTerminal` surface, at a level that asks: the **client** refuses without reading a line —
  the next stdin line stays the next prompt — and the session proceeds cold with one line.
  `generate = always` answers the question the prompt would ask, at every level but `plan`
  (informed by REQ-585 BR-6/BR-11, REQ-587 BR-4, REQ-591 BR-5, REQ-560 BR-2/BR-5, LESSON-495,
  LESSON-524).

_Leg B — bounded evidence, never a scan at launch_

- [ ] BR-3: **The evidence is thorough, fixed in kind, bounded in the two currencies, and
  gathered only after consent.** After consent (or `always`/`full`), and never before, the
  daemon reads: the root's **whole hierarchy** breadth-first at every depth, under REQ-583's
  skip set and symlink rule and the **tool walk's** budget (100,000 entries, 10 s — this walk
  was asked for, so the launch-scan bound is not the right one), rendered as a tree with
  per-directory counts by extension; every present member of the closed EvidenceFile table,
  whole, bounded per file; and the header of every present member of the closed EntryPoint
  table at any depth. Nothing outside the two tables is opened; nothing outside the jail is
  listed or followed. The assembled evidence is bounded to the draft route's context budget
  (REQ-586) in a stated priority order — tree first, because the shape is what a description
  most needs — and a cut or a walk-budget stop is written into the file's header and printed
  on the surface, never swallowed. REQ-584 BR-3 holds by construction: no walk runs until a
  human (or a durable `always`) said so; and a repository whose tree alone exceeds the route's
  budget is drafted from a tree cut at a stated depth, not refused (informed by REQ-583
  BR-10/BR-11/BR-13, REQ-584 BR-3, REQ-586 BR-1/BR-7, LESSON-587).

_Leg C — the draft is a model call like any other_

- [ ] BR-4: **The draft routes through the duty machinery — to the deep-reasoning tier by
  default — with the evidence's provenance, and covered evidence never reaches the call.** The
  call is harness-known (`CategoryOrigin::HarnessKnown`) under its **own category** (`draft`,
  illustrative), whose default policy binding is the tier the user's policy names for deep
  reasoning (`think`), because the draft is made once and read on every turn afterwards — the
  one place in the harness where the expensive model is the cheap choice. The binding is a
  policy row like any other (`/policy set-category draft <tier>`), so a user who wants it
  local can say so; a machine with no remote provider drafts on the local tier and the header
  says which tier wrote it. Its context carries `ToolProvenance::Sources` of every evidence file —
  never `Unknown`. Before the call, every evidence member whose identity a configured boundary
  covers is **excluded** and counted, so the outbound body holds no covered byte; the
  remaining provenance is judged by egress exactly as a tool-result-bearing turn is. Listing
  names are metadata (REQ-583 OQ-7's rule) and are not excluded. The `[privacy] redact` scan
  runs on the call when configured. This is a charter BR-1 claim and carries an egress-capture
  test (informed by REQ-563 BR-2, REQ-585 BR-7, REQ-612 BR-5, LESSON-432).
- [ ] BR-5: **The cost is one named row.** The draft call lands in the ledger as one
  `CostRecord` with its own purpose, visible in `/cost` and, with `/verbose`, on the turn's
  surface (`context: drafting TETON.md on think — 1 model call, 1,840 entries walked, 41,200 input tokens`). It is attributed
  to the session and the route that served it, never hidden in the prompt's own cost (informed
  by REQ-586 BR-9, REQ-563 BR-7).

_Leg D — the file_

- [ ] BR-6: **Bounded before it is written, headed, and never a clobber.** The model's answer
  is stripped and bounded as REQ-612 bounds a file, at REQ-612's cap less the header, so the
  loader never truncates a generated file; the file begins with one line stating that Teton
  generated it, the date, that it is meant to be edited, and that Teton reads it at session
  start. The write is create-new: if a `TETON.md` exists at write time — a race with the user,
  another session, or a checkout — nothing is written and the line says so. A write that fails
  midway leaves no partial file. `--force` (BR-8) is the only path that replaces an existing
  file, and it confirms first (informed by REQ-612 BR-3/BR-4, REQ-591 BR-4, LESSON-501).
- [ ] BR-7: **Loaded the same turn, exactly as an authored file.** After the write the daemon
  runs REQ-612's loader on the new file — same cap, same frame, same provenance — and the
  turn that raised the offer proceeds with the block resident. `/context` shows
  `origin: generated`. Nothing about the model's answer is treated as instructions to the
  session: it is a file, and REQ-612 frames it as description (informed by REQ-612 BR-4/BR-7).

_Leg E — on demand, failure, and posture_

- [ ] BR-8: **`/context init` and `teton context init [--force]` generate on demand.** Both
  raise the same offer (the gate at levels that ask; `--force` names that an existing file
  will be replaced, and the prompt says so); `init` works even when `generate = never`, because
  it is the user's explicit act; without `--force` an existing file refuses with its size and
  the flag named. The shell form runs against the daemon like `teton transcript` does, so the
  write, the walk and the call are the daemon's and the same tests cover both doors (informed
  by REQ-611 BR-3, REQ-582's one-grammar rule).
- [ ] BR-9: **Failure never fails the session, and never leaves a file.** A budget stop with
  no usable listing, a model error, a refusal at egress, an over-window answer, or a write
  error ends generation with one line naming the cause and the on-demand remedy; no file is
  written (or the partial is removed); the turn's own prompt proceeds cold; the session's state
  records `failed` so the offer is not re-raised that session. A provider failure here does not
  degrade the provider's health any more than a duty failure does today (informed by REQ-586
  BR-2, LESSON-505, REQ-563 BR-9).
- [ ] BR-10: **The unattended posture is one sentence, and it is documented where the gates
  are.** An unattended session (piped stdin, `NoTerminal`) with `generate = ask` never writes
  and never blocks; with `generate = always` or at `full` it writes into whichever project it
  was launched in, and the docs say that plainly beside the setting — a durable `always` is
  the automation opt-in with the same character as `[skills] trusted_project_roots`. The
  self-config guide gains no sentence for this (the model is not a party to the offer); the
  `teton_docs context` topic documents the offer, the setting and both `init` doors so a model
  asked "why is there a TETON.md I didn't write?" answers from a resident fact (informed by
  REQ-591 BR-12, BUG-181, LESSON-543).

## Acceptance Criteria

- [ ] AC-1: In a project with no `TETON.md` at `guarded`, the first prompt raises exactly one
  permission prompt naming the path and the evidence budget; accepting writes the file, loads
  it, and the same turn's request body ends with the block; declining writes nothing and a
  second prompt raises no offer; `/cd` to another project raises it again (daemon unit +
  `cli_e2e`; BR-1, BR-2, BR-7).
- [ ] AC-2: At `plan` no offer is raised and one line names `/context init`; at `full` the file
  is written without a prompt; at `edits` the prompt is drawn (permission matrix test; BR-2).
- [ ] AC-3: On piped stdin at `guarded`, the client refuses without reading stdin — the next
  stdin line is still consumed as the next prompt — one line prints, no file exists; with
  `generate = always` on the same pipe the file is written (`cli_e2e`; BR-2, BR-10).
- [ ] AC-4: Before consent, the injected file reader records **zero** directory listings and
  zero reads beyond REQ-612's own `stat`; after consent a planted six-level tree is listed to
  its leaves with per-directory extension counts, the skip set is honored, a symlinked entry is
  not followed, and a planted tree over the entry budget stops with the stop stated in the
  file's header; a route with a small declared window gets a tree cut at a stated depth and the
  cut is in the header (daemon unit; BR-3).
- [ ] AC-5: Both tables are exercised by name; a present EvidenceFile member contributes its
  whole text up to 16 KiB and an EntryPoint member its first 4 KiB; an absent member costs one
  `stat`; nothing outside the tables is opened; the priority cut drops the lowest class first
  (unit; BR-3).
- [ ] AC-6: Egress-capture: with a `local-only` boundary covering `Cargo.toml` and a marker in
  its bytes, the draft call's request body carries no marker, the excluded count is 1, and the
  provenance union of the call names every remaining evidence file; a mutation that skips the
  exclusion fails (egress-capture; BR-4).
- [ ] AC-7: `/cost` shows one row with the draft purpose after generation; `/verbose` prints
  the drafting line with the entry count (`cli_e2e`; BR-5).
- [ ] AC-8: A model answer of cap + 2,000 bytes is written at exactly REQ-612's cap including
  the header; the loader reports `loaded`, not `truncated`; the first line matches the header
  golden; a `TETON.md` created between consent and write causes no write and one line
  (daemon unit; BR-6).
- [ ] AC-9: A write error after the file is created leaves no file; a model error leaves no
  file, prints one line, and the turn's prompt is answered; the provider's health is unchanged
  (daemon unit; BR-9).
- [ ] AC-10: `/context init` in a project with a file refuses naming the size and `--force`;
  `/context init --force` at `guarded` prompts naming the replacement and, accepted, replaces
  it; `teton context init` on the shell drives the daemon and produces the same bytes as the
  session command for the same evidence (`cli_e2e`; BR-8).
- [ ] AC-11: `generate = never` raises no offer and `/context init` still works; an empty
  `TETON.md` raises no offer and REQ-612 loads no block (daemon unit; BR-1, BR-10).
- [ ] AC-12: The `context` topic documents the offer, the setting, the unattended sentence and
  both `init` doors; `every_topic_serves_its_whole_bundled_body` is green; the README's
  `[context]` paragraph names `generate` (docs; BR-10).
- [ ] AC-13: Dogfood, by hand, in `docs/manual-verification.md`: a fresh clone of this
  repository at `guarded`, first prompt, accept the offer, read the generated file, and record
  whether it names the crates, the daemon/CLI split and the test command — the quality bar is
  "a new contributor would not object", and the result is recorded, not scored (BR-6, BR-7).
- [ ] AC-14: With a policy that binds `think` to a remote provider, the draft call is served by
  that provider and the cost row names it; with `/policy set-category draft local` it is served
  locally; with no remote provider it is served locally and the header names the tier (routing
  matrix test; BR-4).

## External Dependencies

- REQ-612 (the loader, the cap, the frame, the `[context]` table, `/context`). This REQ
  extends every one of them and cannot ship first.

## Assumptions

- ASSUME-1: A frontier-tier model given the full tree, the manifests, the README and the
  entry-point headers produces a description a new contributor would accept. AC-13 is where
  this is learned; if the cap is the limiting factor rather than the model, OQ-5 is the lever.
- ASSUME-2: A generated file will usually be committed and edited, so a plain first line
  rather than an HTML comment is the right header — visible in every renderer, and it costs
  under 150 bytes of the cap.
- ASSUME-3: `AGENTS.md` present means someone already described the repository; generating
  beside it would create two sources of truth, so its presence suppresses the offer.
- ASSUME-4: One offer per session per root is the right cadence — a session that declines
  wants to be left alone, and a new session is a new decision; this mirrors REQ-587's
  once-per-session acknowledgment rather than a durable "never for this repo" row, which
  `generate = never` and an empty file cover.

## Open Questions

- [ ] OQ-1: **Breadth of `always`.** Should `generate = always` write into *any* project an
  unattended session lands in, or only into roots listed in `[skills] trusted_project_roots`?
  Recommendation: any project, documented plainly — the write is a bounded, headed markdown
  file, not code that runs, and coupling it to the skill allowlist would make the automation
  opt-in depend on a security setting for a different question.
- [x] OQ-2: **Category.** Resolved 2026-09-03 (product: "solid context, not cheap context"):
  a new `draft` category, default-bound to the deep-reasoning tier (BR-4), so the once-per-repo
  call gets the best model the policy has and `digest`'s local pin does not drag it down.
- [x] OQ-5: **Is REQ-612's cap the right size for a solid file?** Resolved 2026-09-03: the
  owner raised REQ-612's cap to 8 KiB, route-aware (a quarter of the route's byte budget, up to
  8 KiB). The draft is bounded at 8 KiB less the header.

  **Amended the same day**, by the owner's second decision: rather than let a floored route
  carry half a block, REQ-612 raised the daemon's budget floor
  (`budget::MIN_BUDGET_BYTES`) from 16,384 to a pinned 50,000 bytes, whose quarter (12,500)
  is above the 8 KiB cap. So **every route the daemon can derive carries the whole 8 KiB**,
  a floored one included, and a generated draft written at 8 KiB less the header is resident
  whole on every route. The route-aware truncation path still exists and still applies to any
  future budget below the floor; it is simply not reachable today, so a draft sized to the cap
  is not silently halved by a provider fallback.
- [ ] OQ-3: **The draft prompt's shape** is architecture's, but one product question stands:
  should the draft be asked to follow a fixed section order (Layout, Build, Test, Conventions)
  so generated files look alike across repositories? Recommendation: yes.
- [ ] OQ-4: **Should a `.gitignore`d `TETON.md` be offered?** Teton does not read `.gitignore`
  today. Recommendation: no change; the file is meant to be committed, and a user who ignores it
  has said what they want.

## Out of Scope

- Nested or per-directory files, and any `@import` mechanism (REQ-612's exclusions hold).
- Regenerating when the repository changes, or any freshness check on a generated file.
- Scoring, ranking or validating the draft's quality beyond the cap and the header.
- Changing REQ-612's cap, frame, or loader rules.
- A model tool that creates files. The write stays a daemon act under its own key.
- Windows.

## Retrieved Context

Retrieval query: component `daemon/harness`, domain `harness`, stack `[rust, daemon, cli,
json-rpc, llm-providers]`, concerns `[security, privacy, cost, developer-experience]`, tags as
in the frontmatter. 262 candidates, 252 scored, 58 of 58 specs admitted (statuses `approved`,
`complete`). The delegate body-read returned all 15 blocks; no direct reads were needed.

- REQ-587 (spec, score 30): Model-invoked skills — a `skill` tool lets the model expand a registered skill into its own turn
- REQ-612 (spec, score 26): TETON.md — a per-repository context file the session reads at its root and carries as resident data
- REQ-586 (spec, score 21): A turn's context budget follows its route
- REQ-583 (spec, score 18): Session-root awareness and bounded discovery
- REQ-585 (spec, score 16): User-defined slash commands from SKILL.md
- REQ-563 (spec, score 15): Opt-in web lookup through the egress choke point
- LESSON-495 (lesson, score 15): A remembered grant answers every question its key matches — so the key must encode the whole question
- REQ-591 (spec, score 14): The project-skill trust gate and its unattended allowlist
- LESSON-552 (lesson, score 14): A test that hands the minter its input never exercises the derivation that got it wrong
- REQ-584 (spec, score 14): A project locator — the session can name this machine's projects without walking the disk
- BUG-181 (bug, score 12): The model affirms capabilities Teton does not have
- REQ-572 (spec, score 12): Capability-aware refusals and guided in-session enablement
- REQ-560 (spec, score 12): Named permission levels and the interactive session status line
- BUG-190 (bug, score 11): A `$ARGUMENTS` splice puts the caller's bytes inside the region the frame certifies as instructions
- BUG-176 (bug, score 11): The shipped guide told users to put a live API key on the command line
