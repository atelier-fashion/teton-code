# REQ-613 — Architecture

## Approach

REQ-613 is a pipeline of five acts the daemon already knows how to do one at a time — ask a
human, walk a tree, run a duty, write a file, load a file — joined in one new module and raised
from one seam. Each act reuses the precedent that owns it: the offer is a fourth entry point on
`PermissionGate` shaped like `authorize_project_skill_trust` (REQ-587 BR-4, REQ-591); the walk is
`walk::visit` under the tool walk's `WalkBudget` (REQ-583 ADR-3); the draft is a `DutyKind` under
a new `Category::Draft` resolved by the one resolver (REQ-558 ADR-D, REQ-561's `DutyRoute`); the
write is a create-new file beside the transcript writer's `O_NOFOLLOW`/mode discipline (REQ-611);
the load is REQ-612's loader, unchanged.

Four things the survey settled, which the tasks depend on:

1. **"First prompt turn" is a session fact, not the daemon's counter.** `turn_counter` on
   `DaemonRuntime` (`runtime/mod.rs:2072`) is daemon-wide and mints ids like `cd-N`; it cannot say
   whether *this session* has prompted. The offer therefore rides a `GenerationState` on the
   session record, set to `Pending` at create and `/cd` when REQ-612's state is `absent`, and
   consumed once by the `assemble` stage (ADR-1).
2. **A gate can be awaited inside the turn path.** `runtime/turn.rs:2231` awaits
   `authorize_project_skill_trust` mid-turn; the offer is awaited at the same kind of seam, before
   the turn's own model call (ADR-1).
3. **There is no create-new precedent in the tree.** The transcript writer opens with
   `create(true).append(true).mode(0o600).custom_flags(O_NOFOLLOW)` (`writer.rs:497`); the lock
   file uses `create(true)`. The no-clobber write is new and small: `create_new(true)` plus the
   writer's `O_NOFOLLOW` and an explicit cleanup on failure (ADR-5).
4. **A cost row already names its category.** `CostRecord` carries `category: Option<Category>`
   and `/cost` groups by it, so "one named row" (BR-5) is `category: Some(Draft)` with no new
   field.

## Key decisions

### ADR-1 — The offer is a session-record state consumed once in `assemble`, awaited before the turn's model call

`SessionRegistry`'s record gains `generation: GenerationState` (`Pending | Offered | Declined |
Generated | Failed | Suppressed`). `store_session_repo_context` (REQ-612 TASK-374) sets `Pending`
when it stores an `absent` state at create or `/cd`, and `Suppressed` when the config says
`never`, the level is `plan`, or an `AGENTS.md`/empty file is present. In `assemble_harness`,
after REQ-612's refresh and before `build_system_prompt`, a `Pending` state runs
`repo_context::generate::offer_and_run` (ADR-6), which awaits the gate, then the pipeline, and
stores the terminal state. `/context init` calls the same function with `force` and
`explicit: true`, from `session_context`.

**Rationale.** A prompt needs a turn (finding 2), and the state has to survive the gap between
create and the first prompt, and between turns after a decline (BR-1). Putting it on the record
beside `skills` and REQ-612's `repo_context` means `/cd`'s rebuild resets all three in one place.
`Declined` is session-scoped by construction and is never written anywhere — Teton never
remembers a permission answer across sessions.

**Alternatives rejected.** Raising the offer inside `session/create` (no turn to ride). A daemon
counter (finding 1). A durable "declined for this repo" row (the spec's BR-1 chose `never` and an
empty file as the two durable stops).

### ADR-2 — A fourth gate entry point, keyed by the durable root, expiring with the root

`PermissionGate::authorize_repo_context_generation(key, root: TrustRoot, replace: bool,
addressee)` mirrors `authorize_project_skill_trust`'s shape: it publishes a
`PermissionSubject::RepoContextGeneration { root, path: "TETON.md", replace }` and awaits
`permission/respond` with the gate's once / for-this-session scopes. The key is
`repo_context:generate:<durable root>`, minted from the canonical resolution REQ-591 BR-4 uses
for trust rows, so two spellings of one directory share one answer and two directories never do
(LESSON-495). The key's predicate `is_repo_context_generate_key` lives in `teton-protocol` beside
`is_project_skill_key`, and the combined root-scoped predicate `expires_on_session_root_change` (`methods.rs:672`) gains it
as a third disjunct — so the daemon's `PermissionGate::drop_project_skill_grants`
(`permissions.rs:2610`) and the CLI's `SessionGrants::forget_root_scoped_grants`
(`session_ui.rs:107`), which both read that predicate, expire it on `/cd` with no new code at
either store — ASSUME-017's rule: a decision with two stores needs one invalidation predicate
above both.

The level table is the gate's own: `guarded`/`edits` ask, `plan` denies, `full` allows. Two
short-circuits sit **before** the gate, where the config is: `generate = never` → `Suppressed`;
`generate = always` at a level that would ask → treated as `AllowOnce` with the event saying so.
`plan` is decided before the gate too, so no prompt is drawn for an act the level will refuse
(LESSON-524's shape, inverted: do not ask what you will deny).

**Rationale.** REQ-587 BR-4 and REQ-591 BR-1 established that repository-touching acts with no
human typing a name get their own key and their own entry point rather than widening `authorize`.
`replace` is in the subject because `--force` asks a different question (BR-8) and the human
must see which one is on screen.

### ADR-3 — The evidence gatherer is one bounded function over the walker and two closed tables

`repo_context/evidence.rs`: `gather(root: &ProbedRoot, reader: &dyn RepoFileReader, matcher:
&BoundaryMatcher, budget: EvidenceBudget) -> Evidence`. It calls `walk::visit` once with
`WalkBudget::default()` (100,000 / 10 s) and the root's `WalkPolicy`, collecting every entry with
its depth (derived from the root-relative path's component count) into a `Tree`; renders the tree
breadth-first with per-directory extension counts; reads each present member of `EVIDENCE_FILES`
whole to 16 KiB and each present member of `ENTRY_POINTS` (matched by file name at any depth,
from the same walk) to 4 KiB; mints every read file's `ProvenanceId` and drops any the matcher
covers, counting them; and assembles the prompt body in priority order — tree, manifests, README,
entry points — under `budget.max_bytes`, recording a `Cut { class, depth }` when it stops. Its
provenance is `ToolProvenance::Sources(read files)`; listing names contribute no identity (REQ-583
OQ-7). Workspace-member manifests are found from the tree (any `Cargo.toml`/`package.json` below
the root), not by parsing the root manifest.

**Rationale.** One walk, not one per table — the entry-point match rides the listing already in
hand, so the walk budget is the only walk cost. The two tables are `const` arrays exercised by
name (REQ-584 BR-4's pattern). The cut is recorded, never silent (REQ-586 BR-7), and lands in the
header line (ADR-5). `RepoFileReader` is REQ-612's injection seam, so every rule here is
unit-tested against an in-memory tree.

### ADR-4 — `Category::Draft` is a twelfth category bound to `Think`, with its own duty and prompt

`teton-core`: `Category::Draft` joins the enum and `ALL` (12), `tier()` returns `Think`,
`ConfigurableCategory::Draft` exists so `/policy set-category draft <tier>` works, and the
REQ-558 ADR-A unreached-set test marks it *reached* because `harness/draft.rs` is its call site.
`harness/draft.rs`: `DRAFT_DUTY = DutyKind::new(Category::Draft, DRAFT_OUTPUT_MAX_BYTES)` where
the ceiling is REQ-612's cap; `build_prompt(evidence) -> String` asks for a fixed section order
(Purpose, Layout, Build & test, Conventions, Where to look — OQ-3 resolved *yes*) and states the
byte budget; `bound_answer` strips and cuts as REQ-612's renderer does, at cap minus header. The
duty is resolved by the one resolver and performed through `Duty::perform(prompt, provenance)`.

**Rationale.** The product decision: a once-per-repository draft gets the best model the policy
has. `Think` is the compile-time default the way `Design`/`Debug`/`Review` are; the user's policy
row overrides it. `digest` was rejected because its local default is right for digests and wrong
here (REQ-613 OQ-2). A privacy-blocked draft (a covered source that slipped exclusion) is a
`privacy_block` on the duty exactly as on any duty, and does not degrade provider health
(REQ-561's rule, kept).

### ADR-5 — The write is create-new with `O_NOFOLLOW`, headed, cleaned up on failure; `--force` is a temp file and a rename

`repo_context/write.rs`: `write_new(root, body) -> Result<Written, WriteFailure>` opens
`<root>/TETON.md` with `write(true).create_new(true).mode(0o644).custom_flags(O_NOFOLLOW)`;
`AlreadyExists` is the no-clobber outcome (BR-6), a symlink at the path is refused by the flag;
the buffer is written whole and on any error the file is removed. `--force` writes
`TETON.md.<pid>.tmp` the same way and `rename`s over the target — an atomic replace, never a
truncate-then-write window. The header is one line composed in `render.rs` from the tier that
served the draft, the date, and the cut/stop facts: `> Generated by Teton on 2026-09-03 (think
tier; tree cut at depth 6). Edit freely — Teton reads this file at every session start.` and it
counts inside the cap.

**Rationale.** Mode `0o644`, not the transcript's `0o600`: this file is repository content meant
to be committed, not a private record. `O_NOFOLLOW` is kept because a symlink at `TETON.md`
pointing outside the jail is exactly the shape the jail refuses everywhere else. A partial file
would be loaded by REQ-612 on the next turn as if authored, which is why cleanup is part of the
rule, not a nicety.

### ADR-6 — One pipeline function, one event, one loader call

`repo_context/generate.rs::offer_and_run(ctx) -> GenerationOutcome` runs: short-circuits (ADR-2)
→ gate → `gather` (ADR-3) → `DRAFT_DUTY.perform` (ADR-4) → `bound_answer` → `write_new` or
`replace` (ADR-5) → REQ-612's `RepoContext::load` on the new file → store `Generated`. Every
stage publishes `Event::RepoContextGeneration { outcome, root, entries, excluded, draft_bytes,
tier, reason }` with one of `offered | declined | refused_unattended | denied_level | suppressed |
walking | drafted | written | replaced | failed`; the CLI renders one line per event and the
`/verbose` drafting line. Failure at any stage returns `Failed { stage, reason }` with the file
absent, and the caller's turn proceeds; the reason is composed at the surface from typed facts
(LESSON-557), and the sentence names `/context init` as the remedy.

**Rationale.** One function is what lets the first-turn hook and `/context init` be one code
path with two `explicit`/`force` flags, and what lets AC-8's "same bytes from both doors" hold by
construction rather than by test.

### ADR-7 — `teton context init` is a one-shot session; `teton context generate <mode>` is a config write

The shell `init` creates a session at the shell's cwd exactly as `teton` does (same root probe,
same level), sends `session/context { action: Init { force } }`, answers the gate on its own TTY
through the ordinary prompter (or refuses on a pipe as the session would), prints the outcome
lines, and closes the session. `teton context generate ask|always|never` is
`ConfigUpdate::SetRepoContextGenerate { mode }` through `config/set`, inheriting its gates —
the transcript twin's shape.

**Rationale.** A daemon-wide "generate here" method would need its own root probe, its own gate
addressee and its own attestation; a session already has all three. One grammar, two spellings.

### ADR-8 — Open questions resolved for v1

| OQ | Decision | Why |
|---|---|---|
| OQ-1 `always` breadth | any project the session is in | a bounded, headed markdown file; coupling it to the skill allowlist would hang an automation opt-in on a different question's setting; the docs say it plainly |
| OQ-3 fixed sections | yes | ADR-4's prompt; generated files look alike across repositories |
| OQ-4 `.gitignore` | not consulted | the file is meant to be committed; a user who ignores it has said so |

## Component map

| Layer | File | Change |
|---|---|---|
| Core config | `crates/teton-core/src/config.rs` | `ContextConfig.generate: GenerateMode` (`ask` default) |
| Core category | `crates/teton-core/src/category.rs` | `Category::Draft`, `ALL` (12), `tier()`, `ConfigurableCategory::Draft`, tests |
| Protocol | `crates/teton-protocol/src/methods.rs` | `ContextAction::Init { force }`, result fields, `ConfigUpdate::SetRepoContextGenerate`, `is_repo_context_generate_key` |
| Protocol | `crates/teton-protocol/src/events.rs` | `PermissionSubject::RepoContextGeneration`, `Event::RepoContextGeneration`, `RepoContextState.origin` |
| Daemon (new) | `crates/tetond/src/repo_context/{evidence,write,generate}.rs` | gatherer, writer, pipeline |
| Daemon harness (new) | `crates/tetond/src/harness/draft.rs` | `DRAFT_DUTY`, prompt, bounding |
| Daemon harness | `crates/tetond/src/harness/permissions.rs` | `authorize_repo_context_generation`, key, expiry |
| Daemon runtime | `crates/tetond/src/runtime/duty.rs` | resolve `Draft` |
| Daemon runtime | `crates/tetond/src/runtime/session.rs` | `GenerationState` at create/`/cd`; `session_context` handles `Init` |
| Daemon runtime | `crates/tetond/src/runtime/turn.rs` | `assemble` consumes `Pending` |
| Daemon runtime | `crates/tetond/src/runtime/mod.rs`, `config_document.rs` | `SetRepoContextGenerate` persistence, render `generate` |
| Daemon sessions | `crates/tetond/src/sessions.rs` | `generation` field and accessors |
| CLI | `crates/teton/src/slash.rs`, `main.rs`, `session_ui.rs`, `status.rs` | `/context init [--force]`, `teton context init\|generate`, prompt and event rendering, doctor |
| CLI | `crates/teton/src/session_ui.rs` (`SessionGrants`) | no change if `forget_root_scoped_grants` reads `expires_on_session_root_change`; a test proves the generation key is forgotten |
| Docs | `crates/tetond/src/harness/docs/context.md`, `README.md`, `docs/manual-verification.md`, `.adlc/context/architecture.md` | offer, setting, doors, unattended sentence, dogfood leg, patterns |
| Tests | `crates/tetond/tests/repo_context_generation.rs` (new), `skill_consent_matrix.rs`, `duty_matrix.rs`, `routing.rs`, `egress_capture.rs`, `cost_attribution.rs`, `config_preservation.rs`, `crates/teton/tests/cli_e2e.rs` | acceptance |

## Risks and accepted consequences

**The first prompt of a fresh clone is slower and costs a frontier call.** By design (the product
posture). The offer sentence names the budget and the tier so the human decides with the facts;
`generate = never` and `plan` are the two ways to opt out of ever seeing it.

**A 100,000-entry walk after consent can take up to 10 s.** Stated as the tool walk's own bound;
the event stream shows `walking` so the wait is not silent, and the stop is written into the
header.

**Older clients refuse the new subject.** A REQ-612-vintage `teton` reads `Unrecognized` and
refuses; the daemon records `refused_unattended` and proceeds cold. Two clients on one session is a
consented topology (REQ-570); stated in the docs.

**Evidence can carry secrets a boundary does not name.** A `.env` is not in either table and the
tree lists only names; a manifest with an inline token is the user's own file. The `[privacy]
redact` scan runs on the duty call when configured; nothing here widens what leaves beyond what a
`read` of those files would send.

**Applied lessons.** LESSON-495 / ASSUME-017 (key encodes the root, one expiry predicate above
both stores), LESSON-524 (do not draw a prompt the level will deny), LESSON-501 (state on the
record, terminal on every path), LESSON-540 (tree fixtures order-independent; sort before
rendering), LESSON-557 (typed failure, sentence composed at the surface), LESSON-519/520 (the
durable `generate` write verified on disk with a refuse/accept pair), LESSON-587 (default `ask`
introduces no emptiness predicate), LESSON-624 (egress markers only in evidence file bytes),
REQ-558 ADR-A/ADR-D (declare the category, one resolver, derived reached-set test).
