---
id: REQ-585
title: "Architecture — user-defined slash commands from SKILL.md"
status: approved
parent: REQ-585
created: 2026-08-20
updated: 2026-08-20
---

## Approach

A skill invocation is **one user-role prompt turn that the daemon composes**.
The CLI's only new job is to recognize `/name` against a snapshot of names and
to render what the daemon reports; every rule with teeth — the four globs, the
name contest, the permission gate, the provenance, the budget refusal — lives
in `tetond`, beside the machinery it has to reuse.

The whole feature is four pure functions and one impure edge:

```
              (dir listings, file bytes)  ──►  SkillRegistry            [pure]
              (registry, typed line)      ──►  Expansion<Pending>       [pure]
Expansion<Pending>.commands ──► permission gate ──► run_bounded ──► outcomes   [I/O]
              (Expansion<Pending>, outcomes) ──►  String                [pure]
```

`Expansion<Pending>` is the load-bearing shape. The expander runs **once** and
produces the body with a typed placeholder standing in each `!`cmd`` slot,
plus the ordered command list. That single value is what BR-8(d) measures
before consent is spent, what BR-6 shows the user, and what BR-6's outcomes are
folded back into. Building the expansion twice — once to measure, once to emit
— would be the LESSON-528 mirrored-predicate shape one layer down, and the two
copies would disagree the first time substitution changed.

### The turn, end to end

```
CLI                                     tetond
───                                     ──────
session/create ─────────────────────►
   ◄──────────────── SessionCreateResult { root }
skills/list ────────────────────────►   discover() → SkillRegistry   (BR-1)
   ◄──────────────── SkillsListResult { skills, skipped }
                                        [ METHOD_NOT_FOUND on an old daemon
                                          ⇒ empty snapshot ⇒ pre-REQ behaviour ]
type "/status foo"
classify(line, &snapshot) → Input::Skill  (BR-10)
session/prompt { prompt: [], skill } ►   run_prompt_turn
                                          ├─ route + budget      (REQ-586)
                                          ├─ expand(registry, name, args)  (BR-4)
                                          ├─ REFUSE if body-only over budget (BR-8d)
                                          ├─ authorize("skill:<src>:<name>")  (BR-6)
   ◄──────────────── PermissionRequest { subject: SkillDynamicContext }
   [ no terminal ⇒ refuse without reading stdin ]                  (BR-11)
permission/respond ─────────────────►
                                          ├─ run_bounded × N, in order   (BR-6)
                                          ├─ fold outcomes → final text
                                          ├─ REFUSE if now over budget   (BR-8d)
   ◄──────────────── SkillInvoked { … }    │                              (BR-12)
                                          └─ CarriedTurn::begin(text, sources) (BR-7)
   ◄──────────────── turn events, cost row
```

The refusal sits **between** route resolution and `CarriedTurn::begin`
(`crates/tetond/src/runtime.rs:2935`). That call both pushes the user block and
arms the drop-commit, so a check placed after it has already put the expansion
into the conversation — BR-8(c) is a statement about *that line*, not about a
check somewhere inside the turn loop.

## Module map

New, all in `crates/tetond/src/skills/`:

| module | holds | purity |
|---|---|---|
| `mod.rs` | `Skill`, `SkillSource`, `SkillRegistry`, `Skipped { path, reason }`, `SkipReason`, `permission_key_for`, `RESERVED` derivation | pure |
| `discovery.rs` | the four globs, the `DirLister` seam, symlink and EPERM rules, the `home`-root de-dup, deterministic ordering | pure over the seam |
| `frontmatter.rs` | the narrow flat `key: value` parser; `Parsed { name, description, argument_hint, ignored_keys, body }` | pure |
| `expand.rs` | `Expansion<Pending>`, `$ARGUMENTS`/`$N`, the `ARGUMENTS:` fallback, the preamble, placeholder folding | pure |
| `dynamic.rs` | the `` !`cmd` `` scanner and `DynamicOutcome` | scanner pure; runner is the one I/O edge |

Modified, by layer:

- **protocol** — `skills/list`; `PromptTurnParams.skill`; `PermissionRequest.subject`; `Event::SkillInvoked`; `error_code::SKILL_EXPANSION_TOO_LARGE`.
- **daemon-harness** — `permissions.rs` (`is_skill_permission_key`, `authorize_skill`), `context.rs` (`Provenance::User { sources }`, `push_user_from`, `would_seed_fit`), `completion.rs` (`context_provenance` merges user sources), `tools/shell.rs` (`run_bounded` extracted), `budget.rs` (the refusal composer), `self_config.md` (BR-9).
- **daemon-runtime** — `runtime.rs` (ordering), `carry.rs` (`begin` carries sources), `sessions.rs` + `server.rs` (registry lifecycle, `/cd` rebuild, method table).
- **CLI** — `slash.rs` (`classify(input, registry)`, `Input::Skill`, the skills section), `main.rs` (the arm + snapshot refresh), `session_ui.rs` (the pipe rule, echo, `/verbose`), `client.rs` (thread `typed_input`).

---

## ADR-1 — The daemon owns discovery, the registry and the expansion; the CLI owns classification and rendering

**Decision.** `tetond` builds the registry, expands the body, runs dynamic
context and composes the turn. `teton` holds a read-only snapshot of
`(name, source, description, argument_hint, shadowed)` for classification and
`/help`, and renders the events the daemon emits.

**Why.** Three of the fourteen BRs are unimplementable on the client:

- BR-6's gate and jail are daemon-side (`PermissionGate`, `ToolContext::for_root`).
- BR-7's provenance and BR-8's budget are daemon-side by construction.
- Project discovery needs the session root **as a path**. After launch the
  client holds only `SessionRoot.display` — home-relative, middle-elided and
  neutralized (REQ-583) — and an *attached* client never held a path at all
  (`session/attach`; `SessionSummary.cwd` is reduced away for a connection not
  entitled to session content). So a client-owned registry needs a new RPC for
  project skills regardless, and then owns half a feature.

REQ-573's lesson stands behind this: a catalog the CLI owns is one the phase-2
VS Code client re-implements. The daemon-owned shape also makes AC-14 fall out
— `session_root_changed` already reaches the client before the
`session/set_cwd` response, so the snapshot refresh has an event to hang off.

**Cost, recorded.** `/help` now renders from two sources (the `COMMANDS` const
and a snapshot). BR-3's "cannot be dispatchable without appearing in `/help`"
is what keeps that honest, and it gets its own test: for every name the
snapshot classifies as `Input::Skill`, `render_help` prints a row, and vice
versa (LESSON-524 — exposure is not callability, run in both directions).

## ADR-2 — `skills/list` is the version handshake; `PROTOCOL_VERSION` does not move

**Decision.** The CLI queries `skills/list` after `session/create` and after
every `session_root_changed`. `METHOD_NOT_FOUND` yields an **empty** snapshot,
not an error. Every other new wire element is additive
(`#[serde(default, skip_serializing_if = …)]`).

**Why.** An empty snapshot makes `classify` incapable of returning
`Input::Skill`, so a new CLI against an old daemon behaves byte-for-byte as it
does today and never sends `PromptTurnParams.skill`. The capability is proven
by a successful call rather than asserted from a version number — and it is the
same property that makes ADR-6's fail-closed consent sound: the only client that
can *receive* a skill consent request is one that asked for skills.

**Guard.** Skew tests in both directions, copying
`events.rs:3386 route_decided_budget_fields_are_additive_in_both_directions`
(absent keys parse; an unset value emits **no** key, not `null`; the new wire
parses through a locally-declared pre-REQ struct; plus the non-vacuity
assertion that the fixture really carries the new keys). `skills/list` joins the
session-scoped method list in
`server.rs:9943 an_unmintable_session_id_is_refused_by_every_setup_method_before_anything_else`.

## ADR-3 — The invocation crosses the wire as a name, never as an expansion; exactly one of `prompt`/`skill`

**Decision.**

```rust
pub struct SkillInvocation { pub name: String, pub raw_arguments: String }

pub struct PromptTurnParams {
    pub session_id: SessionId,
    pub prompt: Vec<PromptBlock>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub skill: Option<SkillInvocation>,
}
```

The daemon rejects `INVALID_PARAMS` when both are empty **and** when both are
populated.

**Why.** `serde` ignores unknown fields, so a middle box or an older daemon that
drops `skill` would otherwise silently send whatever was in `prompt`. Requiring
`prompt` to be empty for a skill turn makes that failure loud: the request dies
as invalid instead of shipping the raw `/name args` line to a model. The client
never composes the body, which also keeps the untrusted bytes on the side of the
seam that sanitizes them (LESSON-517).

`raw_arguments` is the rest of the line **verbatim** — this is the one place the
session does not use REQ-582 ADR-2's tokenization (BR-4), so it must not be
re-joined from tokens anywhere on the path.

## ADR-4 — Discovery is a purpose-built bounded lister behind a recording seam, not `walk::visit`

**Decision.** A new `DirLister` trait:

```rust
pub trait DirLister {
    fn list(&self, dir: &Path) -> Result<Vec<Entry>, ListError>;   // one level, no recursion
    fn read(&self, file: &Path) -> Result<String, ReadError>;      // bounded at SKILL_MAX_BYTES
}
```

`RealFs` in production; `RecordingFs` in tests records every path opened, which
is precisely the seam AC-7 asks for. `Entry` carries the `file_type()` from
`DirEntry` (`lstat` semantics — it does **not** follow).

**Why not the walker.** `walk::visit` is a recursive driver; BR-1 forbids
recursion. Reusing it would also inherit `WalkBudget::DEFAULT_MAX_ENTRIES =
100_000` / `DEFAULT_MAX_WALL = 10s`, turning AC-7's fixture (`skills/link → /`,
ten thousand files) from a *reach* test into a *budget* test — the thing it
exists to prove would stop being asserted. REQ-583 shipped **policy** seams
(`WalkPolicy`, `WalkBudget`), never an observation seam; this is new, and it is
new on purpose.

**Symlink rule, stated as the narrowing it is.** The four **roots** are opened
without a symlink check — the dogfood machine's `~/.claude/skills` *is* a
symlink. Every **entry** returned by `list` is refused if
`file_type.is_symlink()`, reusing `tools::mod::skip_symlink_entry` so the
predicate has one home. This is *narrower* than the walker's blanket rule, so it
gets its own pin in `crates/tetond/tests/symlink_posture.rs` rather than
riding the walker's.

Because entries are refused, `skills/link → /` is never enumerated: `/` is not
reached by the entry rule, not by a budget.

**Bounds and determinism.** `MAX_ENTRIES_PER_ROOT = 512`, and entries are
**sorted by file name before the cap applies**. Sorting is not cosmetic: APFS
lists in hash order and ext4 does not, so an unsorted cap (and an unsorted
`/help`) would be a platform-flaky test (LESSON-540). Hitting the cap is a named
diagnostic, never a silent truncation.

**Failure taxonomy.** `fs_util::read_regular_file_bounded` returns `None` for
every failure; BR-1 needs `EPERM`/TCC told apart from missing, oversize and
non-UTF-8. `ReadError` is therefore typed, and `SkipReason` renders it:
`unreadable (permission denied)`, `over 64 KiB (67,184 B)`, `not UTF-8`,
`malformed frontmatter`, `invalid name`, `symlink not followed`,
`shadowed by <what>`. A missing directory is the normal case and produces no
diagnostic; a directory with no `SKILL.md` is not a skill and produces none
either (BR-1).

## ADR-5 — Frontmatter is a narrow flat parser, modeled on `parse_search_auth`; malformed is total

**Decision.** No YAML dependency (there is none in the workspace and this is not
the REQ that adds one). The parser:

1. A file that does not begin with `---\n` has **no** frontmatter — the whole
   file is the body, zero ignored keys. (`.claude/commands/*.md` routinely has
   none; refusing them would refuse the common case.)
2. Otherwise scan to the next line that is exactly `---`. No closing delimiter
   ⇒ malformed.
3. Every line between is blank, a `#` comment, or `key: value` where key matches
   `^[a-z][a-z0-9-]*$`. Anything else — an indented continuation, a nested
   block, a list item — is **malformed**, and the file is skipped whole.
4. `name`, `description`, `argument-hint` are read. Every other key lands in
   `ignored_keys` and is inert (BR-5).
5. A `name` that differs from the directory/stem is recorded as a note and
   **does not create a spelling** (BR-2: one spelling reaches one handler).

**Why total.** `parse_search_auth` (`teton-core/src/config.rs:551`) is the
shipped precedent and its doc carries the argument: a template is a shape, and
half-parsing is how a value that looks accepted behaves differently than it
reads. A half-parsed skill would register under a name whose body the user did
not sanction.

## ADR-6 — The grant key is `skill:<source>:<name>`, and project grants die at `/cd`

**Decision.** `permission_key_for(skill) == format!("skill:{source}:{name}")`
with `source ∈ {user, project}`. The key takes the level table's **default**
posture — `guarded` ask, `edits` ask, `plan` deny, `full` allow — so
`table_for`/`READ_ONLY_TOOLS` are not touched. On `/cd`, every remembered grant
whose key begins `skill:project:` is dropped.

**Why the source is in the key (OQ-6, extended).** LESSON-495's rule is that the
remembered key must encode the whole question. `skill:analyze` does not: after
`/cd` the same string names a different file, so a grant remembered in one repo
would silently authorize another repo's commands. Encoding the source narrows
the collision to project-vs-project, and dropping project grants at `/cd` closes
it — the grant map is carried state, and carried state sheds its invariants
silently (LESSON-501).

**What must not happen**, each with its own test: a `shell` allow-always must not
answer a skill request; a skill allow-always must not answer `shell`; a grant on
one skill must not answer another; `authorize`'s `debug_assert!` against web
keys must still fire for web keys and must not fire for skill keys.

## ADR-7 — One consent, one structured subject; the client selects on a protocol value, never on the key string

**Decision.** `PermissionRequest` gains

```rust
#[serde(default, skip_serializing_if = "Option::is_none")]
pub subject: Option<PermissionSubject>,

#[serde(tag = "kind", rename_all = "snake_case")]
pub enum PermissionSubject {
    SkillDynamicContext { skill: String, source: SkillSource, commands: Vec<String> },
    #[serde(other)]
    Unrecognized,
}
```

`authorize_skill` asks **once per invocation** with every command in
`commands`, in document order, substituted (BR-4 puts substitution before
execution, so the consent shows what will run).

**Why a structure and not a string.** Two reasons, both mechanical.
(a) `Surface::line` destroys newlines (`defused` → `neutralized(text, false)`),
so "three commands listed verbatim" cannot ride `description: Option<String>`
— the client must render one line per command. (b) BR-11 requires the client to
recognize the request **without parsing the key**; `OPTION_ID_ENABLE_PERMANENT`
is the shipped precedent for "the one value a client may select by string, and
everything else by typed kind", and its doc says why.

**Fail-closed.** `#[serde(other)] Unrecognized` means a future subject a client
does not know maps to a variant it *can* see, and the client refuses rather than
falling through to `prompter.ask`. An *older* client sees no field at all — and
under ADR-2 an older client never asks for skills, so it can never receive this
request. Both halves are asserted.

## ADR-8 — The pipe rule is a pure two-input predicate consulted before `prompter.ask`

**Decision.** `resolve_permission` gains the terminal fact
(`UiContext.typed_input`, already computed at `main.rs:1060` and today not
threaded through). A pure classifier decides first:

```rust
fn consent_gate(subject: Option<&PermissionSubject>, typed_input: bool) -> ConsentGate
// Answerable | RefuseNoTerminal | RefuseUnrecognized
```

`RefuseNoTerminal`/`RefuseUnrecognized` return a rejection **without calling
`prompter.ask`**.

**Why before, not inside.** `StdinPrompter::ask` reads a line unconditionally;
a refusal computed after the call has already eaten it, and a pasted second line
becomes a `y` — the exact LESSON-537 shape BR-11 names. `cli_rows::write_gate`
is the shipped truth-table predicate to copy, including its unit tests.

The pin is a *negative* one and needs to be written as such: feed `/status\ny\n`
on a pipe at `guarded`, and assert `y` arrives as the **next prompt line**, not
as an answer (`cli_e2e.rs:1830` is the existing "does not eat a line" template).

## ADR-9 — `Provenance::User` carries sources; three seams, three tests

**Decision.**

```rust
enum Provenance {
    System,
    User { sources: BTreeSet<ProvenanceId> },   // was a unit variant
    Model,
    Tool { tool: String, provenance: ToolProvenance },
}
```

`push_user(text)` keeps its signature and seeds an empty set — every existing
caller is byte-identical. `push_user_from(text, sources)` is the new one.

**The three seams** (LESSON-502 — a multi-seam invariant needs a test at each):

1. `DroppedProvenance::absorb` — today it early-returns on any non-`Tool` block
   ("user and model text carries no file provenance of its own"). That comment
   becomes false; the arm must absorb user sources or a *dropped* skill block
   loses its pin.
2. `completion::context_provenance` — matches only `CtxProvenance::Tool`. The
   test named `context_provenance_unions_tool_result_paths_only` **is** the
   claim BR-7 breaks; it is renamed and re-asserted, not deleted.
3. `ContextManager::replay` — its `Provenance::User => self.push_user(text)` arm
   drops the sources on every later turn. This is LESSON-501 exactly: the
   round-trip test (`per_block_provenance_survives_the_commit_and_replay_round_trip`)
   is extended to a user block.

**The id-minting gap, stated rather than assumed.** `ProvenanceId::from_resolved`
is root-relative and **refuses** a path outside the root — deliberately, with no
fallback (ADR-B of REQ-571). A *project* skill is under the root and mints
cleanly. A *user* skill at `~/.claude/skills/x/SKILL.md` in a repo-rooted
session has no repo-relative identity, so `from_resolved` returns
`NotUnderRoot` and there is nothing to match a boundary glob against.

**Decision:** do not widen `from_resolved` and do not reach for `claimed` (its
doc says appearing in a first-party path is a bug). A user skill whose id cannot
be minted is recorded as **`ToolProvenance::Unknown`-equivalent** for the user
block — i.e. the turn fails closed whenever any boundary is configured, exactly
as `shell` output does. That is stricter than the spec's letter and correct in
the charter's direction: the alternative is a file outside the root silently
counting as unpinnable. BR-7's "exactly as a `read` would" therefore holds
literally for project skills and *more strictly* for user skills; AC-11(a) is
re-worded to say which, and the residual is recorded in the runbook.

## ADR-10 — The expander is a frame author, so it neutralizes envelope tags in the body

**Decision.** `expand.rs` runs `render::neutralize_envelope_tags` over the
**body** before splicing any dynamic-context envelope into it.

**Why (a real gap, found in Phase 2).** ADR-009 defuses each layer where the
frame is authored. `neutralize_frame_labels` deliberately skips `<`-prefixed
markers, because by assembly time the harness's own envelope is inside the block
and indistinguishable from a forged one; envelope defusing happens one layer
earlier, in `frame_untrusted_builtin`. That layering assumes a block's text was
authored by exactly one author.

A skill expansion breaks the assumption: one user block containing
file-supplied prose **concatenated with** a harness-authored `<tool-result>`
envelope. A flush-left `</tool-result>` in the body is touched by neither
transform, and closes the envelope of the dynamic block spliced after it.

BR-5's "an envelope tag in a skill body is prompt text, exactly as it would be
if pasted" is true of a pasted paragraph — which is never concatenated with an
envelope inside one block. The expander is a new frame author and takes the
frame author's duty. **AC-12 is amended** to cover the body case, not just the
dynamic-output case.

## ADR-11 — Two budget checks, one measurement, one new error code

**Decision.** `error_code::SKILL_EXPANSION_TOO_LARGE = -32023`, distinct from
REQ-586's `CONTEXT_LENGTH_EXCEEDED = -32022`.

- **Stage A** — before consent, over the expansion with a `[dynamic context
  pending]` placeholder in each slot. Refusal says the body alone does not fit.
- **Stage B** — after the outcomes are folded. Refusal says the dynamic output
  pushed it over.

Both measure through **one** function, `ContextManager::would_seed_fit(system,
text, budget) -> Fit`, implemented over the existing `tokens_of`/`bytes_of` —
the same estimators the pressure path uses. Nothing re-derives a budget:
`Router::budget_for` stays the single `budget::derive` caller.

**Why Stage A does not charge the worst case.** It could reserve `MAX_OUTPUT_CHARS`
per command and refuse everything that would not fit at maximum output — no user
would ever approve a turn that is then refused. It does not, because every ADLC
skill's dynamic context is an `ls`/`grep`/`cat` producing tens of bytes, and
reserving 8,000 characters each would refuse the entire real corpus on a small
route. BR-8(d) describes the after-refusal path explicitly, so paying it in the
rare case is the spec's own choice, and Stage B's message says which stage refused.

**Why a separate code.** `-32022` means *a provider refused this turn*; the
remedy is the provider's window. `-32023` means *Teton refused to send it*; the
remedy is a smaller skill or a bigger declared window. Collapsing them would
make AC-16's "a typed outcome, not a clamped turn" uncheckable and would tell a
user their provider rejected something it never saw.

**The message** composes in `budget.rs`, beside `big_window_notice`, and reads
`BudgetBound::words()` (never `wire_name()`), `thousands()` and `bytes_figure()`
— all three already imported there. It carries the `floored` clause whenever
`RouteBudget.floored` is set, because `bound` alone cannot say a ceiling is not
in force.

> **Spec correction.** AC-20(d) says `bound: local_engine` — the wire spelling,
> contradicting BR-8(a) and AC-16. It reads `bound: local engine`. Fixed in the
> spec by TASK-196.

**Silence.** The refusal returns before `CarriedTurn::begin`, so no
`context_pressure` of any kind is emitted. The assertion is a drain-and-assert-
empty, copying
`runtime.rs:27362 a_context_length_refusal_changes_no_health_and_degrades_nothing`
— which is also the model for the other three properties: ahead of every
`Remote` arm, no `record_health`, no `on_provider_failure`, no retry.

## ADR-12 — `/help`: skills are their own section, and `ARGUMENT_FOOTER` is qualified

**Decision.** Render order: built-in rows (bytes unchanged from this REQ's merge
base) → blank → `skills — arguments are passed through as typed:` → one row per
skill → the diagnostic line → blank → `ARGUMENT_FOOTER` → `ESCAPE_FOOTER`.
`ARGUMENT_FOOTER` is qualified to name the built-in rows it describes.

**Why both.** `ARGUMENT_FOOTER` says quotes are not interpreted and arguments
split on whitespace; BR-4 makes that false for skill rows. Leaving it unqualified
puts a contradiction two lines from the rows it contradicts. Qualifying it is one
word; the section header carries the positive statement.

**Test consequences, spelled out** so they are widened rather than relaxed:
`help_renders_every_table_row_and_the_escape_footer` asserts
`lines.len() == COMMANDS.len() + 2`, zips rows against `COMMANDS`, and indexes
the footers from the end. The zip and the count are re-scoped to the built-in
**prefix slice**; `ESCAPE_FOOTER`-last and `ARGUMENT_FOOTER`-second-last keep
asserting over the **whole** rendered list, or a skills section could slip below
them. `cli_e2e.rs:4967`'s family-contiguity loop walks every `/`-prefixed line
and must stop at the skills header, or `/alpha` reads as a family.

`help_family` never sees skills — BR-2's reserved set stops a skill named
`provider` from dispatching, but only a separate section stops it from
re-grouping the four built-in `/provider` rows.

## ADR-13 — `classify` takes the snapshot; built-ins are matched first

**Decision.** `classify(input: &str, registry: &SkillSnapshot) -> Input<'_>`, and
the order inside is: `//` escape → `cli_line` (REQ-582's `teton …`) →
`split_name(rest, COMMANDS)` → **then** the snapshot. `Input` gains
`Skill { name: String, raw_arguments: String }`.

**Why in that order.** It makes BR-2's "reserved names always win" structural
rather than a list that has to stay in sync: a built-in match returns before the
snapshot is consulted, so a skill can only be reached by a name no built-in
claims. The reserved *set* still exists — the daemon needs it to mark a skill
shadowed in `/help` — and it is **derived** from `COMMANDS` (every spelling, plus
the first word of every multi-word row) plus `teton`, never hand-listed. It gets
its own test that the derivation matches what `classify` actually does, in both
directions (LESSON-546 — a one-home rule needs a test, not a grep).

`Resolution`/`resolve` stay built-in-only and keep returning
`&'static CommandSpec`. Nothing is leaked to satisfy a lifetime: a leaked
registry would survive `/cd` and dispatch a skill the session no longer has.

## ADR-14 — The dynamic runner is a second caller of an extracted spawn body

**Decision.** Extract `ShellTool::run`'s spawn body into
`harness::tools::shell::run_bounded(root, command, timeout_ms) -> BoundedRun`
— jail canonicalize, `scrub` + `apply_path_floor`, `process_group(0)`,
`stdin(null)`, timeout, `SIGKILL` to `-pgid`, `MAX_OUTPUT_CHARS`. `ShellTool::run`
becomes its first caller, `skills::dynamic` its second.

`Tool::refine` is **not** on the skill path: it fires the `shell` duty, which is
a model call, and BR-4 says no model call happens at expansion time.

Output enters through `frame_untrusted_builtin(&format!("skill:{name}"), out)`,
which already neutralizes envelope tags — the label is a `&str`, so no new
framing function is needed. Provenance is `Unknown`, exactly as `shell`'s is.

A command that is not run, fails, or times out leaves
`[dynamic context not run: `<cmd>` — <reason>]`. A failure never fails the
invocation.

## ADR-15 — The echo line is rendered from a typed event, not composed twice

**Decision.** The daemon emits `Event::SkillInvoked { name, source, path_display,
body_bytes, ignored_keys, outcomes }`; the CLI renders BR-12's one line and
`/verbose`'s detail from it.

**Why.** The CLI knows name and source from the snapshot but not size, ignored
keys or outcomes, so *some* event is required. Making it typed rather than a
pre-rendered string is LESSON-544: a test that builds the wire value by hand
leaves the producer unguarded — the assertion must run against the value the
daemon actually emitted.

`path_display` is `session_root::display_for` (home-relative), bounded with
`bounded_field`. The body is never printed.

---

## Open questions, resolved

| OQ | Resolution |
|---|---|
| OQ-1 | **Daemon-owned** (ADR-1). |
| OQ-2 | **No roster in the system prompt** in v1. `/help` is the roster; BR-9's sentence points at it. Revisit with REQ-587, which puts a roster in a tool description — AC-15 is already worded so that landing does not fail it. |
| OQ-3 | **No `/skills` row** in v1; launch + `/cd` only. Stays in Deferred. |
| OQ-4 | **Flat `commands/<name>.md` only.** A subdirectory under `commands/` is not descended and produces no diagnostic (it is not an entry with a `.md` stem). |
| OQ-5 | **No routing hints.** `model:`/`effort:` are inert and listed as ignored. A file on disk must not be able to escalate spend. |
| OQ-6 | **Per skill**, and the key carries the source; project grants are dropped at `/cd` (ADR-6). |
| OQ-7 | **No separate trust acknowledgment** in v1. Recorded as a residual in the runbook. |
| OQ-8 | **No "run without dynamic context" option** in v1. The runbook states the consequence: on a boundary-configured machine every skill that runs a dynamic command pins its turn local. |

## Amendments this design makes to the spec

Both are contradictions the implementer would otherwise have to guess at, and
both are carried by TASK-196:

1. **AC-20(d)** says `bound: local_engine`; BR-8(a) and AC-16 say the bound is
   spoken. It becomes `bound: local engine`.
2. **AC-12** covers a `<tool-result>` planted in a dynamic command's *output*.
   Per ADR-10 it must also cover one planted in the **body**, because the
   expander concatenates file text with a harness-authored envelope inside one
   block.

## Risks

| Risk | Mitigation |
|---|---|
| A green suite that cannot see the feature (LESSON-481) | AC-7's recording seam, AC-18's structural pin, and a mutation table per task — every guard shown to die under the mutation it claims to catch. |
| Daemon-side producers unguarded (LESSON-544, REQ-586's six-Major finding) | `SkillInvoked`, `PermissionSubject`, the refusal message and `SkillView` are each asserted against the value the **daemon emitted**, never a hand-built literal. |
| The 868 B resident-prompt headroom is shared with REQ-584 and REQ-587 | BR-9's amendment is measured, and `docs/manual-verification.md`'s headroom table is re-measured in the same task that edits the guide. |
| REQ-584 (`/projects`, PR #185) also edits `/help` | BR-3's byte-identity is scoped to *this REQ's merge base*; the built-in assertion is a prefix-slice zip against `COMMANDS`, which stays true when a row is added. |
| Platform-dependent listing order (LESSON-540) | Entries sorted by name before the cap and before rendering; the EPERM fixture skips when running as root. |
