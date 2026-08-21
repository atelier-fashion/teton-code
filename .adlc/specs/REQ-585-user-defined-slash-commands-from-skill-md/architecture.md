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
                                          ├─ expand(registry, name, args)  (BR-4)
                                          ├─ route + budget, over the EXPANSION
                                          ├─ REFUSE if body-only over budget (BR-8d)
                                          ├─ authorize("skill:<src>:<name>")  (BR-6)
   ◄──────────────── PermissionRequest { subject: SkillDynamicContext }
   [ no terminal ⇒ refuse without reading stdin ]                  (BR-11)
permission/respond ─────────────────►
                                          ├─ run_bounded × N, in order   (BR-6)
                                          ├─ fold outcomes → final text
   ◄──────────────── SkillInvoked { … }    │                              (BR-12)
                                          ├─ REFUSE if now over budget   (BR-8d)
                                          └─ CarriedTurn::begin(text, sources) (BR-7)
   ◄──────────────── turn events, cost row
```

**Expansion precedes routing, and that ordering is load-bearing.** The freeform
route is decided by `dispatch_route(..., &prompt)` (`crates/tetond/src/runtime.rs:2830`),
which runs the classifier *over the prompt text*, and `spawn_title_session(...,
&prompt)` (`:2858`) spends the session's one naming attempt on the same string.
Since a skill turn's `prompt` is empty (ADR-3), expanding after routing would
classify and name every skill invocation from `""` — BR-4's "the turn then takes
the same classifier and routing a typed prompt takes" would be false, and on a
machine with per-category bindings `/analyze` could be routed to the local tier
and then refused by its own budget check. The expansion is available before
routing (it needs only the registry and the raw argument string), so it is built
first and handed to both.

Routing sees the **body-only** expansion, before dynamic output is folded in.
That is deliberate: the classifier reads the skill's instructions, which is what
determines the work, and the alternative would make the route depend on the
output of commands the route's own permission level decides whether to run.

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
by a successful call rather than asserted from a version number.

**What this handshake does *not* buy — corrected in Phase 3.** It is tempting to
argue that only a client which asked for skills can receive a skill consent
request. That is false: `permission_request` is delivered to **every** connection
attached to the session, and **any** connection that may drive may answer it
(only monitors are refused). Two attached clients is a supported, consented
topology (REQ-570), so a pre-REQ-585 client attached alongside a new one would
see the request, understand no `subject`, and call `prompter.ask` — on a pipe,
turning the next stdin line into a `y` that authorizes shell commands. ADR-7
therefore enforces the property instead of inheriting it.

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

The daemon rejects `INVALID_PARAMS` when **both** are populated — a combination
that was never valid, so nothing is narrowed. It does **not** newly reject a
both-empty request: `flatten_prompt(&[])` returns `""` and such a turn runs
today, and rejecting it would narrow an existing method for third-party clients
while `PROTOCOL_VERSION` is asserted unchanged.

**Why the client sends `prompt: []`.** The failure mode worth designing against
is a raw `/name args` line reaching a model. It cannot happen, because the CLI
never puts the typed line in `prompt` at all — a dropped `skill` field yields a
visible empty turn, not a leaked command line. The client also never composes
the body, which keeps the untrusted bytes on the side of the seam that sanitizes
them (LESSON-517).

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

**Within one source, `skills/` beats `commands/`.** BR-2 defines precedence for
reserved-vs-skill and project-vs-user and stops there, but the four globs make
`~/.claude/skills/status/SKILL.md` and `~/.claude/commands/status.md` a legal
pair: same name, same source, and — fatally for this ADR — the same key
`skill:user:status`. A remembered grant would authorize whichever file won, and
would silently move to the other if the winner ever changed. `skills/` wins, the
`commands/` entry is listed as shadowed, and REQ-555's "one spelling reaches one
handler" holds.

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

**Delivery is addressed, not broadcast.** A skill consent is delivered **only to
the connection that sent the invocation**, and only that connection may answer
it. This is not a refinement — it is the guard, because an older client attached
to the same session would otherwise receive a request it cannot recognize and
answer it by reading stdin (ADR-2). BUG-177 already established connection-
targeted delivery for the replay path; this reuses that shape. It is also the
right semantics on its own terms: the person who typed `/status` is the person
who should approve its commands.

**Fail-closed on top of that.** `#[serde(other)] Unrecognized` means a future
subject a client does not know maps to a variant it *can* see, and the client
refuses rather than falling through to `prompter.ask`. Both halves are asserted:
that an unaddressed connection never receives the request, and that a recognized-
but-unknown subject is refused.

**A refusal carries a reason.** `PermissionOutcome` today is `Selected { option_id }`
or `Cancelled`, and `Cancelled` already means "the user dismissed the prompt"
(it is what EOF on a pipe returns). AC-9 requires the placeholders to say *no
human could be asked*, which the daemon cannot know from either. `PermissionOutcome`
therefore gains `Refused { reason: RefusalReason }` with `NoTerminal` and
`UnrecognizedSubject` — additive, and only ever sent to a daemon that answered
`skills/list`.

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
    // was a unit variant. Two fields, not one: `unknown` cannot be encoded in
    // the set, because the empty set already means *ordinary typed prompt text*
    // — the state every existing `push_user` caller is in. `DroppedProvenance`
    // carries the same pair for the same reason: "unknown" and "these files"
    // are both true at once.
    User { sources: BTreeSet<ProvenanceId>, unknown: bool },
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
be minted sets `unknown: true` on its user block — the turn then fails closed
whenever any boundary is configured, exactly as `shell` output does. That is stricter than the spec's letter and correct in
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

**The duty covers everything the fold splices, not just the body.** The
not-run placeholder embeds the command text verbatim, and the scanner's grammar
puts no restriction on what sits between the backticks — so a project skill
(repo content, which the spec's Assumptions say may be authored by someone other
than the user) carrying a *multi-line* `` !`…` `` whose second line is a
flush-left `</tool-result>` forges the same close. The inversion is what makes
it sharp: `plan` — the level where **no command runs** — is the level where the
raw command bytes reach the model. So `fold` neutralizes envelope tags in every
string it splices, and the echoed command is additionally rendered on one line
and bounded, the way every other file-supplied string on a surface is.

## ADR-11 — Two budget checks, one measurement, one new error code

**Decision.** `error_code::SKILL_EXPANSION_TOO_LARGE = -32023`, distinct from
REQ-586's `CONTEXT_LENGTH_EXCEEDED = -32022`.

- **Stage A** — before consent, over the expansion with a `[dynamic context
  pending]` placeholder in each slot. Refusal says the body alone does not fit.
- **Stage B** — after the outcomes are folded. Refusal says the dynamic output
  pushed it over.

Both measure through **one** function, `ContextManager::would_seed_fit(system,
text, budget) -> Fit`, implemented over the existing `tokens_of`/`bytes_of` —
the same estimators the pressure path uses — and it charges the measurement with
**`truncated = true`**.

That last word is the whole guard. `bytes_of(blocks, truncated)` adds
`TRUNCATION_NOTE_BYTES + CONTINUATION_USER_TURN.len()` = 142 B only when
`truncated` is set (`crates/tetond/src/harness/context.rs:987-1016`). A skill
turn in a session with history passes an un-surcharged check, is replayed, and
then `truncate_to_budget` drops history down to one block and sets `truncated`
— adding 142 B nobody charged. The clamp then fires on the **last** block, which
is the skill expansion, and middle-elides it: exactly what BR-8 forbids
("never middle-elided into something the user did not invoke") and BR-4 forbids
("carried whole or refused"). Charging the surcharge up front closes the band,
and it is the same correction `attempt_compaction` already makes for the same
reason (`context.rs:1184`: measured `with truncated forced true on both sides`).
A second, direct assertion backs it up: the skill block is never clamped. Nothing re-derives a budget:
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

**An empty registry renders no section at all** — no header, no `0 skills` line.
That is the default state of every user with no `~/.claude`, and it is the state
ADR-2 produces against an old daemon, where the claim is that `/help` is
byte-for-byte what it is today. A section announcing nothing would make that
claim false for the majority of users.

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

`path_display` is BR-1's display spelling, bounded with `bounded_field`. The
body is never printed. The **skipped** entries' paths are spelled and bounded
the same way — an unreadable `/Users/jane/.claude/skills/broken/SKILL.md` must
not carry a username into a transcript (BR-1's entity table).

**Amended 2026-08-21 (BUG-187).** As shipped this said `session_root::
display_for` (home-relative) and nothing else, and that helper can only shorten
a path under `$HOME`: a **project** skill in a session root outside the home
folder — a CI workspace, a checkout on an external volume, `/tmp` — rendered as
its full absolute path on the wire event, on the `/verbose` detail line and in
BR-4's preamble (hence in every remote payload the turn produced). The rule now
has two halves, chosen by the skill's `source`: a project skill is spelled
relative to the **session root** (`teton_core::session_root::display_under`), a
user skill relative to `$HOME` (`display_for`, unchanged). It is derived **once,
at discovery** and carried on the registry row (`Skill::path_display`,
`Skipped::path_display`) rather than at each surface, because it needs the
source, the session root and `HOME` together and no consumer holds all three —
`skills/list` answers from a stored snapshot with no root at all, which is how
the gap survived review. `Skill::path` itself stays absolute: it is the
local-only fact the expander opens and the provenance mint resolves against
(ADR-9).

**The event is published before Stage B, not after.** A turn where the user
approved four commands, watched them run, and was then refused is precisely the
turn whose record matters most; emitting after the refusal would leave it with
no echo line and no `/verbose` outcomes, while BR-12 says *every* invocation
echoes one.

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

Each is a place the spec as written cannot be implemented as written. All are
carried by TASK-196, which is the one task that edits `requirement.md`:

1. **AC-20(d)** says `bound: local_engine`; BR-8(a) and AC-16 say the bound is
   spoken. It becomes `bound: local engine`.
2. **AC-12** covers a `<tool-result>` planted in a dynamic command's *output*.
   Per ADR-10 it must also cover one planted in the **body**, and one planted in
   a multi-line `` !`…` `` command that the fold echoes into a placeholder.
3. **AC-11(a)** says a skill file under a `local-only` boundary pins the turn
   "exactly as a `read` of that file would". Per ADR-9 that holds literally for
   a **project** skill; a user skill outside the root has no repo-relative
   identity and is pinned by the stricter unknown rule. The AC names which.
4. **BR-2** defines precedence for reserved-vs-skill and project-vs-user but not
   for `skills/` vs `commands/` **within one source**, which the four globs make
   reachable. `skills/` wins; see ADR-6.
5. **BR-7's** parenthetical that on a boundary-configured machine "all seventeen
   ADLC skills run on the local tier" is false and contradicts BR-8 plus the
   spec's own Assumptions: seven of the seventeen exceed the local budget and
   are **refused** there, not run. The sentence says pinned, not run.

## What Phase 3's adversary pass changed

Eleven findings survived refutation; nine changed this document. The four that
changed a decision rather than a detail:

- **Expansion now precedes routing.** The freeform classifier and the session
  namer both read `&prompt`, ~100 lines before where the expansion was going to
  be built — so every skill turn would have been classified and named from `""`.
- **The consent is addressed, not broadcast.** `permission_request` reaches every
  attached connection and any driver may answer it, so the handshake could not
  carry the fail-closed property ADR-7 was resting on it.
- **`would_seed_fit` charges `truncated = true`.** A 142-byte surcharge appears
  only after replay, and the block it pushes over the line is the skill
  expansion itself.
- **`Provenance::User` needs two fields.** The empty set already means ordinary
  typed text, so it could not double as the unpinnable marker without pinning
  every prompt on every boundary-configured machine.

## Risks

| Risk | Mitigation |
|---|---|
| A green suite that cannot see the feature (LESSON-481) | AC-7's recording seam, AC-18's structural pin, and a mutation table per task — every guard shown to die under the mutation it claims to catch. |
| Daemon-side producers unguarded (LESSON-544, REQ-586's six-Major finding) | `SkillInvoked`, `PermissionSubject`, the refusal message and `SkillView` are each asserted against the value the **daemon emitted**, never a hand-built literal. |
| The 868 B resident-prompt headroom is shared with REQ-584 and REQ-587 | BR-9's amendment is measured, and `docs/manual-verification.md`'s headroom table is re-measured in the same task that edits the guide. |
| REQ-584 (`/projects`, PR #185) also edits `/help` | BR-3's byte-identity is scoped to *this REQ's merge base*; the built-in assertion is a prefix-slice zip against `COMMANDS`, which stays true when a row is added. |
| Platform-dependent listing order (LESSON-540) | Entries sorted by name before the cap and before rendering; the EPERM fixture skips when running as root. |
