---
req: REQ-619
created: 2026-09-05
updated: 2026-09-05
---

# REQ-619 — Proportionate skill provenance: architecture

## The seams this REQ moves

Two rules, three call sites, one fold.

1. **Preambles.** `skills::dynamic::run_all` spawns each `!cmd` line after
   `$ARGUMENTS` substitution; nothing classifies the command. The typed path
   (`runtime/turn.rs`, the REQ-585 seam) then ORs `outcomes.iter().any(spawned)`
   into `SkillTurn::unknown`, and the model-invoked path
   (`harness/tools/skill.rs`) maps `(source, spawned)` to `ToolProvenance` with
   `(_, true) | (User, _) => Unknown`. The `shell` tool, one directory over,
   calls `shell_provenance::classify(root, root_kind, boundaries,
   denied_prefixes, command)` **before** `run_bounded` and maps the verdict to
   `Sources` / `BoundaryTouch` / `Unknown` (REQ-614 BR-10; `shell.rs:286-322`).
2. **Identity.** `skills::provenance_of(root, skill)` is
   `ProvenanceId::from_resolved(root, resolved).ok()`, which is `None` for a
   file outside the session root — every user skill — and both call sites
   read that `None` as `unknown`.

The expansion's provenance then travels as `Provenance::User { sources,
unknown }` through three seams REQ-585 ADR-9 pinned — the seeded block
(`context.rs:1239`), the context-provenance union (`completion.rs:933`), and
replay (`context.rs:1679`) — and as `ToolProvenance` on the model-invoked
tool result.

## ADR-619-1: The preamble verdict is taken inside the runner, once per command, before it spawns

**Decision.** `run_all` takes a `Reach` input — `{ root, root_kind,
boundaries, denied_prefixes }`, the four values `ShellTool::run` hands the
classifier — and, for each command it is about to spawn, calls
`shell_provenance::classify` **first**, then `run_bounded`. It returns one
`PreambleRun { verdict: Verdict, outcome: DynamicOutcome }` per command.
`Verdict` is the REQ-614 type, re-exported from `harness::tools` so the
`skills` module names one grammar rather than a copy of it. A command the
door leaves unrun (`NotRun`) still receives a verdict — it is cheap and
content-free — but the fold (ADR-619-4) ignores it, which is BR-2's "a
command that did not run contributes nothing".

**Why inside `run_all` and not at the two callers.** The callers are the
seams that used to disagree (typed path: `spawned` OR; model path:
`(source, spawned)` match). A verdict computed in one place by the same
loop that spawns is what makes REQ-614 BR-10 hold for skills — "no arm can
reach a different answer" — and lets one mutation test (the `classify`
call count, copied from `shell.rs::the_verdict_is_computed_before_measurement`)
guard both callers. The classifier takes no output and no exit status by
signature, so BR-2's "output never changes the verdict" is structural here
as it is for `shell`.

**Consent is untouched.** `authorize_skill` still lists the substituted
commands verbatim; classification reads the same `Command::as_str()` the
consent listed and adds nothing to it (REQ-585 BR-6, REQ-591).

**Where `Reach` comes from.** The model-invoked path has a `ToolContext`
(`ctx.root_kind()`, `ctx.boundaries()`, `ctx.denied_prefixes()`), and the
typed path builds the turn's `ToolContext` before the preamble seam runs
(`.with_denied_prefix(effective_transcript_dir(..))`, `turn.rs` ~1033).
Both derive `Reach` from that one context — never from `config` directly —
so the denied prefix the jail applies is the one the classifier applies.

## ADR-619-2: `Provenance::User` gains `boundary_touch`, and the three seams carry it

**Decision.** `harness::context::Provenance::User { sources, unknown }`
becomes `{ sources, unknown, boundary_touch }`. The union
(`completion.rs::context_provenance`) folds `boundary_touch` through
`tool_result_provenance(&ToolProvenance::BoundaryTouch)` exactly as it folds
`unknown` through `ToolProvenance::Unknown`; the seed
(`push_user_from`) and replay arms carry the third field byte for byte.

**Why a field and not a reuse of `unknown`.** An out-of-root boundary touch
(`cat ~/.ssh/config` in a preamble) mints no id — LESSON-623 — so the only
thing that can carry it is a bit, and that bit must reach egress as
`BOUNDARY_TOUCH_PATH`, because `taint::cause_of` reads the **path** to decide
that the pin is permanent (REQ-614 ADR-614-3). Folding it into `unknown`
would make `~/.ssh/config` liftable. An **in-root** boundary path needs
nothing new: the verdict carries its minted id in `sources`, the glob matches,
and the block names the file.

**Three seams, three tests** (REQ-585 ADR-9, LESSON-501, LESSON-502): the
seed, the union and replay each get a case asserting the bit survives.

## ADR-619-3: A user skill's identity is `~`-scoped — a `ProvenanceId` that begins with the home marker

**Decision.** `ProvenanceId::from_home_resolved(home, resolved)` mints
`~/<path relative to home>` for a canonical file under the user's home;
`from_resolved(root, resolved)` — the repo scope — **refuses** a remainder
whose first segment is `~` with a new `ProvenanceError::ReservedScope`, so the
two scopes cannot produce the same string. `skills::provenance_of` branches
on `Skill::source`: `Project` → `from_resolved(session root, …)` as today;
`User` → `from_home_resolved(home, …)`. A user skill whose canonical path is
not under the home (a symlink out of it) still resolves to `None` → `unknown`,
today's fail-closed answer.

**Why this spelling.** Boundary globs are matched by
`BoundaryMatcher::match_path(&str)`; `**/` at the head of every builtin
matches any prefix, and `~` is an ordinary character to `globset`, so
`**/.ssh/**` matches `~/.ssh/config` and `**/.claude/skills/**` matches
`~/.claude/skills/x/SKILL.md` with no new glob language — OQ-1 resolved: the
ordinary form. It is also the spelling a user recognises in a `privacy_block`
line, and it never prints the home directory's absolute layout into an event
(the disclosure REQ-614 AC-5's sentinel exists to avoid). `mint` accepts it
today; what this ADR adds is the **reservation** on the repo side, which is
the property BR-3 asks for.

**Why not widen `from_resolved`.** REQ-587 ADR-9 refused to invent an
identity the minter has no root for, and that refusal stands: the user
scope has its own root (the home) and its own constructor, minted by
discovery for files discovery listed (BR-4). The jail and `from_resolved`
are untouched; a `read` of `~/.claude/skills/x/SKILL.md` from a repo-rooted
session is refused exactly as before (AC-9).

## ADR-619-4: One fold, two consumers

**Decision.** `skills::provenance::fold_expansion(identity: Option<ProvenanceId>,
runs: &[PreambleRun]) -> ExpansionProvenance { sources, unknown, boundary_touch }`
is the single place a skill's identity and its preambles' verdicts become a
provenance:

| input | effect |
|---|---|
| `identity: Some(id)` | `sources ∪ {id}` |
| `identity: None` | `unknown = true` (today's behaviour for a file that will not mint) |
| `Rooted` verdict on a command that ran | `sources ∪ verdict.sources` |
| `BoundaryTouch` with sources (in-root) | `sources ∪ verdict.sources` |
| `BoundaryTouch` without sources (out-of-root) | `boundary_touch = true` |
| `Unknown` | `unknown = true` |
| any verdict on a `NotRun` command | nothing |

The typed path writes the three fields onto `SkillTurn` (which `expansion_provenance`
already renders into egress `Provenance`); the model-invoked path maps them to
`ToolProvenance` exactly as `ShellTool::run` maps a verdict — `boundary_touch`
→ `BoundaryTouch`, `unknown` → `Unknown`, else `Sources`. The exit-code side
channel REQ-585's verify closed is closed by the table, not by `spawned`: a
content-reading verb on a boundary path is `BoundaryTouch` before it spawns,
whatever it exits.

## ADR-619-5: The verdict rides the `skill_invoked` outcome additively

**Decision.** `DynamicOutcomeView` gains `reach: Option<Reach>` (wire
`rooted` / `boundary_touch` / `unknown`) and `reach_reason: Option<String>`,
both `serde(default)`, both `None` when a daemon predates this REQ. The
reason is the classifier's `&'static str`, so it cannot carry command text or
output (REQ-614's reason rule). The CLI renders the reason only under
`/verbose` and only when the reach is not `rooted`. `DynamicOutcome` (the
wire enum) is unchanged: the verdict is independent of whether the command
ran, and BR-7 says so.

## ADR-619-6: Announcement, pins and lifts are REQ-614's — nothing new is added

A preamble that is `Unknown` produces an expansion block that egress refuses
against `<unknown-provenance>`; `cause_of` records `unknown_shell`; the
tainting sink (BUG-214) publishes `session_pinned` with the `/shell allow`
remedy; the lift reaches the inspection (BUG-215). A `BoundaryTouch` records
`boundary_hit` through the same path. This REQ adds no cause, no event and no
command; AC-4/AC-5/AC-13 assert the existing machinery through the daemon.

## Retired and amended rules

- REQ-585 BR-7 ("an invocation that ran a command pins the turn local") is
  replaced by BR-1/BR-2 here; the tests that assert it flip.
- REQ-587 BR-10 ("a user skill … stricter than a `read`") is retired by BR-6;
  its acknowledgment gate for project skills is untouched.
- `.adlc/context/architecture.md`'s Key Pattern *"A file with no root-relative
  identity — a user skill outside the session root — sets `unknown`"* becomes
  *"has a `~`-scoped identity; a file under neither root sets `unknown`"*.

## Task graph

```
TASK-398 ~-scoped identity (core + discovery)        TASK-399 preamble verdict in run_all
        \                                                   |
         \                                          TASK-400 Provenance::User.boundary_touch + fold
          \                                                 |
           +------------------------------------------------+
                                   |
                     TASK-401 wire both call sites, flip the old rules
                                   |               \
                     TASK-402 event fields + CLI    \
                                   \                 \
                                    +-----> TASK-403 e2e acceptance suite + docs
```

- TASK-398 and TASK-399 are independent (tier 1).
- TASK-400 depends on TASK-399 (it consumes `PreambleRun`).
- TASK-401 depends on TASK-398, TASK-399, TASK-400.
- TASK-402 depends on TASK-399 (the verdict type on the outcome view).
- TASK-403 depends on TASK-401, TASK-402.

## Verification shape

Every egress claim is an egress-capture test through the real daemon with
the leak marker only in `secrets/prod.env` (LESSON-624), because the
in-process seams were green while the daemon disagreed (LESSON-649). The
fold and the minter get unit tests with mutation records; the "verdict
before spawn, exactly once" guard is copied from `shell.rs`. The e2e
`shell_pin_shape::a_typed_user_skill_pins_liftably_and_is_announced` asserts
the behaviour this REQ retires and **flips** to AC-1's shape — a user skill
that leaves — rather than being deleted, so the BUG-214 announcement claims
keep a home in AC-5 and AC-13.
