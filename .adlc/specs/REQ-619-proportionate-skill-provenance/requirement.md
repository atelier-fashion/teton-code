---
id: REQ-619
title: "Proportionate skill provenance — a skill pins the session only when its body or its preamble output could have touched a boundary"
status: draft
deployable: true
created: 2026-09-05
updated: 2026-09-05
component: "daemon/egress"
domain: "privacy"
stack: ["rust", "daemon"]
concerns: ["privacy", "routing", "developer-experience"]
tags: ["skills", "provenance", "shell-provenance", "unknown-provenance", "taint", "boundary", "user-skill", "preamble", "session-pinned", "local-pin", "skill-md", "dynamic-context"]
---

## Description

A typed `/skill` or a model-invoked `skill` expansion enters the conversation
as a block with provenance, and egress judges that block exactly as it judges
a `read`. Two rules from REQ-585 and REQ-587 were written when the set of
privacy boundaries was empty on most machines, and both became far stricter
than intended the moment REQ-597 put thirteen builtin `local-only` globs
permanently in force:

1. **Preamble commands.** A skill body's `` !`cmd` `` lines run before the
   expansion is seeded, and REQ-585 BR-7 marks the whole expansion
   unknown-provenance whenever *any* command spawned — parity with the
   `shell` tool as it was before REQ-614. REQ-614 then gave the `shell` tool
   a classifier that proves `cat .adlc/context/architecture.md` touched
   nothing protected; the skill preamble path was never moved onto it, so a
   skill whose three preambles are three `cat`s of in-root files pins the
   session on its first send while the same three commands typed by the model
   through `shell` do not.
2. **User skills.** A skill under `~/.claude/skills` or `~/.claude/commands`
   has no repo-relative provenance identity in a repo-rooted session, so
   REQ-585 BR-7 / REQ-587 BR-10 mark its block unknown and it fails closed
   wherever any boundary is configured. That was a deliberate, documented gap
   (REQ-587 ADR-9: do not widen the id minter to invent an identity it has no
   root for). With the builtins always on, its consequence is that **every**
   user-authored skill pins **every** repo-rooted session on **every** machine
   — the 2026-09-05 `/analyze` session (BUG-214), whose skill file matched no
   boundary and read nothing.

BUG-214 fixed the announcement and the recorded cause (fix A), and BUG-215
made the lift reach the inspection. What remains is that the pin is still
taken far more often than the boundary guarantee requires. This REQ makes a
skill's provenance **proportionate**, on the model REQ-614 established for
`shell`: the daemon proves what it can and fails closed only on what it
cannot.

Three things this REQ does **not** relax: a preamble or a skill file that
names a boundary file still pins the session for good; a preamble the
classifier cannot prove the reach of still pins the session, liftably; and
the boundary guarantee itself — content under a `local-only` glob never
leaves the machine, including derived content — holds at every seam it holds
at today (REQ-544 C-1/C-2, LESSON-432).

## System Model

### Entities

| Entity | Field | Type | Constraints |
|--------|-------|------|-------------|
| PreambleVerdict | kind | enum `rooted` / `boundary_touch` / `unknown` | the same three verdicts and the same grammar a `shell` invocation gets (REQ-614 BR-1); computed from the command text, its resolved cwd and its path arguments **before** the command spawns, never from its output or exit status |
| PreambleVerdict | sources | set of ProvenanceId | the repo-relative ids of every path argument that resolved inside the session root; empty unless `rooted` |
| PreambleVerdict | reason | string | one content-free sentence naming why; never command text, never output |
| SkillExpansionProvenance | sources | set of ProvenanceId | the skill file's identity (project or user scope, BR-3) unioned with every `rooted` preamble's sources |
| SkillExpansionProvenance | unknown | boolean | true iff at least one preamble that ran was `unknown` |
| SkillExpansionProvenance | boundary_touch | boolean | true iff at least one preamble that ran was `boundary_touch` |
| UserSkillIdentity | scope | string, constant | a named identity scope for files discovered under a **user** skills root; distinct from the repo-relative scope so the two can never collide |
| UserSkillIdentity | path | string | the skill file's path relative to its user skills root, in the canonical form REQ-571 requires of every id |

### Events

| Event | Trigger | Payload |
|-------|---------|---------|
| `skill_invoked` (existing) | a typed or model-invoked skill expanded | unchanged; each command outcome additionally carries its verdict kind and content-free reason |
| `privacy_block` (existing) | a skill turn's send is refused | unchanged: the path named is the boundary file a preamble or the skill file itself matched, or the unknown-provenance sentinel |
| `session_pinned` (existing) | a skill turn pins the session | unchanged (BUG-214): cause `boundary_hit` for a boundary touch, `unknown_shell` for an unprovable preamble |

### Permissions

| Action | Roles Allowed |
|--------|---------------|
| classify a preamble as `rooted` | the daemon, from the command text, resolved cwd and arguments alone — never from the skill author's claims, the output, or the exit status |
| mint a user-skill identity | the daemon, only for a file the skill registry discovered under a user skills root — never for an arbitrary path a body or a tool names |
| lift an `unknown_shell` pin taken by a preamble | the user, typed `/shell allow` (REQ-614 BR-5); a skill body cannot |

## Business Rules

- [ ] BR-1: **A preamble command is classified with the shell grammar, before it spawns.** Each `` !`cmd` `` line of an expansion that is about to run is given the REQ-614 BR-1 verdict — `rooted` only when every token is recognized and every path it names resolves under the session root without matching a boundary glob; `boundary_touch` when a path it names matches one; `unknown` for everything else, including every opaque verb (`sh`, `bash`, `python`, `xargs`, …), every substitution and every spelling the grammar does not model. The verdict is computed from the command text after argument substitution, the session root as cwd, and the resolved path arguments, and it is computed **before** the command runs — the same rule REQ-614 BR-10 gives the `shell` tool, so no arm can reach a different answer (informed by REQ-614, LESSON-494).
- [ ] BR-2: **The verdict folds into the expansion's provenance, and replaces the "any spawn" rule rather than loosening it.** A `rooted` preamble contributes its resolved sources; a `boundary_touch` preamble pins the session permanently exactly as a `read` of that file would; an `unknown` preamble pins it liftably with cause `unknown_shell`. A command that did not run — declined at consent, or held unrun at `plan` — contributes nothing. **Output and exit status never change the verdict**: a `rooted` command that timed out is still `rooted`, and an `unknown` command that printed nothing still pins. The exit-code side channel REQ-585's verify found — `` !`grep -q AKIA secrets/prod.env && exit 1 || exit 2` `` — is closed by the *verdict* (a content-reading verb given a boundary path is `boundary_touch` before it spawns), not by asking whether anything printed (informed by REQ-585, REQ-614 BR-8).
- [ ] BR-3: **A user skill carries an identity.** A skill file the registry discovered under a user skills root (`~/.claude/skills`, `~/.claude/commands`) mints an identity in a **named user-skill scope** — its path relative to that root, in the canonical form every id has — rather than setting `unknown`. Egress compares it against the boundary globs as it compares a repo-relative id: a user skill whose file matches no glob reaches the wire like a project skill does, and one whose file matches a glob the user configured to cover their skills directory is refused naming that file. The scope is distinct from the repo-relative scope so a user skill at `skills/x/SKILL.md` and a project file at the same relative path can never share an id (informed by REQ-571, REQ-585 ADR-9, REQ-587 BR-10).
- [ ] BR-4: **The identity is minted only where discovery happened, and widens nothing else.** Only the registry mints user-skill identities, for files it listed under a user root; `ProvenanceId::from_resolved`'s refusal to name a path outside the session root is unchanged, the tool jail is unchanged, and a `read` of a file under `~/.claude` from a repo-rooted session is refused as it is today. A user skill's identity exists because the *user installed the file*, which is the same fact that makes a project skill's file readable (informed by LESSON-623, REQ-591).
- [ ] BR-5: **The identity survives derivation.** The user-skill id rides the expansion block through every seam a repo-relative id rides today — the seeded block, the naming duty's copy, the dropped-block absorb, the context-provenance union, compaction and replay — and is asserted at each, because carried state sheds its invariants silently on the round trip (informed by REQ-585 ADR-9, LESSON-501, LESSON-502).
- [ ] BR-6: **Typed and model-invoked skills are one rule.** A skill the model expands through the `skill` tool (REQ-587) gets the same preamble classification and the same identity as the same skill typed as `/name`; REQ-587 BR-10's "stricter than a `read`" clause is retired by this REQ and its acknowledgment gate for project skills is untouched.
- [ ] BR-7: **The reason is content-free and the command is never in an event.** A preamble's verdict reason is a sentence from a closed set, carried on the `skill_invoked` outcome beside the existing `ran` / `failed` / `timed out` facts; neither the command's output nor the substituted command text is added to any event by this REQ (informed by REQ-614's reason rule, REQ-585 ADR-15).
- [ ] BR-8: **A pinned skill turn is announced as a pinned shell turn is.** The `session_pinned` event, the standing client line and the `/shell allow` remedy sentence are the ones REQ-614 BR-7 and BUG-214 established; this REQ adds no new announcement surface.
- [ ] BR-9: **The no-boundary machine is unchanged.** With `disable_default_boundaries = true` and no user rows nothing pins, exactly as today (REQ-614 BR-9, REQ-597).
- [ ] BR-10: **Project skills are unchanged.** A project skill still mints its repo-relative id and still pins as a `read` of its file would; only its preambles gain BR-1's classification.

## Acceptance Criteria

Every egress claim below is an **egress-capture** test — a mock transport asserting on the bytes that reached it, with the leak marker living only in the guarded file (LESSON-624) — driven through the daemon's prompt path, because the in-process seams were green while the daemon disagreed (LESSON-649).

- [ ] AC-1: builtin boundaries in force, `build` routed to a remote mock, a **user** skill with no preambles and a body that names no file: the typed `/name` turn's request **body leaves**, carrying the expansion; no `privacy_block`, no `session_pinned`; a second prompt on the session routes remote (BR-3).
- [ ] AC-2: the same user skill invoked by the model through the `skill` tool: the next send leaves, no pin (BR-6).
- [ ] AC-3: a user skill whose preambles are `cat README.md` and `ls -la` of the session root: the send leaves, the expansion carries both outputs, no pin (BR-1, BR-2).
- [ ] AC-4: a user skill with the preamble `cat secrets/prod.env` under a `secrets/**` boundary: the send is refused naming `secrets/prod.env`, `session_pinned` carries `boundary_hit`, `/shell allow` is refused naming the cause, and **no later request leaves the machine** — asserted as an absence by counting captured requests (LESSON-550) (BR-2).
- [ ] AC-5: a user skill with the preamble `` sh -c 'echo x' ``: the send is refused against `<unknown-provenance>`, `session_pinned` carries `unknown_shell` with the `/shell allow` remedy, and after `/shell allow` the next prompt's request leaves (BR-2, BUG-215).
- [ ] AC-6: the exit-code channel: a preamble `` grep -q MARKER secrets/prod.env && exit 1 || exit 2 `` pins with `boundary_hit` **and** the placeholder the fold writes into the prompt never reaches the mock — the verdict is taken before the spawn, so the turn is refused whatever the exit code was (BR-1, BR-2).
- [ ] AC-7: a user-configured boundary glob covering the user skills directory: the user skill is refused naming its own file, pinned `boundary_hit`; the same skill with the glob absent leaves (BR-3, BR-4).
- [ ] AC-8: a user skill's identity survives compaction and a client re-attach: after the conversation is compacted and carried into a second client's prompt, the block still carries the user-skill id and a boundary glob added over the skills directory mid-session refuses the next send naming the file (BR-5).
- [ ] AC-9: a `read` of `~/.claude/skills/x/SKILL.md` from a repo-rooted session is still refused by the jail exactly as today — the identity exists for discovered skills only (BR-4).
- [ ] AC-10: `disable_default_boundaries = true`, no user rows: a user skill with a `sh` preamble is sent and nothing pins (BR-9).
- [ ] AC-11: a project skill with a `cat <in-root file>` preamble leaves; with a `cat <boundary file>` preamble it is refused naming that file (BR-10).
- [ ] AC-12: the `skill_invoked` record for a run carries each command's verdict kind and a content-free reason, and carries neither the substituted command text nor its output beyond what it carries today (BR-7).
- [ ] AC-13: the BUG-214 shape end to end: a user skill whose preambles are `sh <script>`, `cat <in-root>`, `cat <in-root>` pins liftably (`unknown_shell`, from the `sh` alone — the two `cat`s are `rooted`), is announced once, and after `/shell allow` the next prompt leaves.

## External Dependencies

- None. The classifier, the id minter, the taint register and the lift all exist (REQ-614, REQ-571, BUG-214, BUG-215).

## Assumptions

- The thirteen builtin boundary globs match no path of the form `<skills root>/<name>/SKILL.md` or `<commands root>/<name>.md`, so a user skill routes remote by default. AC-1 asserts this against the shipped set rather than assuming it.
- The verdict grammar REQ-614 shipped is sufficient for the preambles skill authors actually write (`cat`, `ls`, `git status`, `sh <partial>`); a preamble the grammar does not model pins liftably, which is today's behavior for that command, not a regression.
- The `/analyze` skill's `sh .adlc/partials/ethos-include.sh` preamble stays `unknown` under this REQ; rewriting the toolkit partials to `cat` is the toolkit's follow-up, not this REQ's.

## Open Questions

- [ ] OQ-1: Should the user-skill scope have a boundary-glob spelling of its own (so a user can write `local_only = ["user-skills/**"]`), or is the ordinary path form — the absolute skills directory with its leading `/` stripped, as REQ-614 AC-5 matches out-of-root boundary touches — the right surface? Recommended: the ordinary form, one glob language.
- [ ] OQ-2: Should a `sh <in-root script>` preamble be classified by reading the script? Recommended: no — REQ-614 ADR-614-1 made the classifier an allowlist grammar over the command, and a script's reach is the halting problem in a different hat; `unknown` with a lift is the honest answer.

## Out of Scope

- Changes to REQ-614's classifier grammar or verb tables.
- Changes to the tool jail, `ProvenanceId::from_resolved`, or what a `read` may open.
- Rewriting the ADLC toolkit's partials so `/analyze` and its siblings run without a `sh` preamble.
- A per-skill or per-command lift; `/shell allow` remains session-wide (REQ-614 OQ-2).
- Any change to the consent flow for preambles (REQ-585 BR-6, REQ-591).

## Retrieved Context

- REQ-614 (spec, score 20): Proportionate shell provenance
- BUG-214 (bug, score 13): A typed `/skill` pins the session permanently and silently
- LESSON-550 (lesson, score 12): A defect fixed once comes back unless a test asserts the absence
- BUG-215 (bug, score 11): `/shell allow` moves the route but not the egress verdict
- LESSON-623 (lesson, score 11): A boundary glob cannot protect a path the provenance seam never names
- LESSON-624 (lesson, score 11): An egress-leak marker must live only in the file's bytes
- REQ-587 (spec, score 11): Model-invoked skills
- REQ-571 (spec, score 10): Canonical provenance identity for privacy-boundary enforcement
- REQ-563 (spec, score 10): Opt-in web lookup through the egress choke point
- REQ-562 (spec, score 10): redact — a model-based secret and PII scan inside the egress choke point
- LESSON-432 (lesson, score 10): Provenance must derive from what a tool touches, not from an argument name
- LESSON-650 (lesson, score 9): A lift composed into one predicate still has to reach every reader
- REQ-612 (spec, score 9): TETON.md — a per-repository context file
- REQ-596 (spec, score 9): A credential-safe environment for the shell tool
- REQ-585 (spec, score 9): User-defined slash commands from SKILL.md
