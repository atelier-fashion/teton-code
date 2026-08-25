---
id: REQ-591
title: "The project-skill trust gate and its unattended allowlist"
status: draft
deployable: true
created: 2026-08-25
updated: 2026-08-25
component: "daemon/session"
domain: "harness"
stack: ["rust", "daemon", "cli"]
concerns: ["security", "privacy", "developer-experience"]
tags: ["project-trust", "allowlist", "unattended", "consent", "toctou", "skills", "permissions"]
---

## Description

**This spec is written after its implementation, deliberately, and a reader needs to
know that up front.** The code already exists on `feat/REQ-589-over-budget-skill-expansion-offer`.
This requirement governs how it is *carved out* of REQ-589 and what must be decided before
it merges on its own terms. That inversion is not the house process and is not a precedent —
it is the correction of one.

### Why this REQ exists

REQ-589 was approved as *"offer to proceed when a skill expansion exceeds the route's
context budget"* — a one-word overrun on `/analyze`. During its architecture phase,
exploration falsified BR-6's premise: **there was no project-skill trust gate on the
user-typed `/name` path at all.** The only production caller of
`authorize_project_skill_trust` was the model-invoked tool, so a user who typed `/name` ran
a project-authored skill body with no acknowledgment. The product owner chose (REQ-589 D-10)
to **build** the gate rather than drop the rule and file the gap.

That was a defensible call — it closed a real hole. But it pulled a security feature into a
budget REQ, and then compounded: the new gate blocked piped and unattended sessions
entirely, so D-13 added `[skills] trusted_project_roots`, a durable allowlist letting an
unattended session run a pre-trusted repository's skills. That is a deliberate security
**widening**, traded for automation.

A six-agent review panel then found that **every serious finding in REQ-589 traced to this
trust work, not to the offer.** The offer's consent path was audited clean — no bypass
constructible — its injection surface clean, its refusal invariant intact, `cargo audit`
clean. The trust work produced a TOCTOU, a second ungated durable config write, a model-door
widening, and two behaviour regressions.

So the two are separated. REQ-589 keeps the offer. This REQ takes the trust gate and the
allowlist, so they are reviewed as the security feature they are rather than as a rider on
a budget fix.

### What this feature does

Two things, and the second exists only because the first broke automation:

1. **A project-authored skill body is acknowledged by a human before it runs as
   instructions** — on the typed path as well as the model-invoked one.
2. **An unattended session may run a repository's skills if a human already acknowledged
   that repository durably**, via `[skills] trusted_project_roots`. The unattended path
   *consults* a decision; it never invents one.

## System Model

### Entities

| Entity | Field | Type | Constraints |
|--------|-------|------|-------------|
| `SkillsConfig` | `trusted_project_roots` | `Vec<String>` | durable; absent means empty; each row is a canonical mint (BR-9) |
| `TrustRoot` | `display` | string | home-relative, faithful, for rendering |
| | `durable` | string? | canonical mint, minted at discovery (BR-4); `None` where the root will not canonicalize |
| `ProjectSkillTrust` (subject) | `root` | string | **currently untruncated and not control-stripped — see BR-11** |
| | `invoked_by` | `InvokedBy` | absent means `Model` (the only caller that predates the field) |
| | `skills` | list | bounded by `MAX_LISTED_PROJECT_SKILLS` |

### Permissions

| Action | Who |
|---|---|
| acknowledge a repository for this session | the connection that submitted the turn |
| write a row to `trusted_project_roots` | **OPEN — OQ-1.** Today: whoever can answer the prompt |
| consult a row | **OPEN — OQ-2.** Today: both the typed and the model door |

## Business Rules

- [ ] **BR-1: A project-authored skill body is acknowledged before it runs as instructions,
  on every path that can run one.** Before REQ-589 the typed path had no gate. The
  acknowledgment is the point of the feature; everything else here is about who may answer
  it and how long the answer lasts.

- [ ] **BR-2: The acknowledgment precedes any question about the body's size or content.**
  Nobody authorizes a send from a repository they have not yet said they trust — otherwise
  a file on disk chooses when it gets a consent prompt. On the typed path the gate therefore
  sits inside `accept_invocation`, before the route, the naming duty and both budget stages.

- [ ] **BR-3: The prompt names who actually asked.** A typed invocation says the user asked;
  a model-invoked one says the model did. These are different questions and a human at
  `guarded` is entitled to know which is on screen (REQ-585 BR-5). The clause comes from the
  existing `invoker_clause`, not a second vocabulary.

- [ ] **BR-4: The trust identity is minted from the resolution the bodies were read under.**
  Skill bodies are read eagerly at discovery (session create and `/cd`) and frozen into a
  registry. If the identity is minted later, from an unresolved path, a symlink re-pointed
  in between lets an unattended session run *unlisted* bodies under a *listed* root's trust.
  The window is the whole session lifetime, not a race. `read_under` closes this.

- [ ] **BR-5: An unattended session consults a decision; it never invents one.** A root that
  is not listed still refuses. Without that, the gate is decorative and BR-1 bought nothing.

- [ ] **BR-6: A durable row is matched exactly, never by prefix, and fails closed.** Trusting
  `~/dev/repo` says nothing about `~/dev/repo/vendor/other` — a different repository,
  possibly placed there by a dependency update. A root that will not canonicalize mints
  nothing and matches nothing.

- [ ] **BR-7: The consent label names the write it actually performs.** LESSON-495 records
  the precedent directly: REQ-563's `enable_permanent` promised `[web] tier = "…"` while
  writing a different key entirely — *"a prompt describing a write that provably could not
  happen."* Derive the label from the effect and pin both with one test.
  (informed by LESSON-495)

- [ ] **BR-8: A row's breadth is a property of its key, and the key must carry every
  dimension the human was deciding about.** LESSON-495's rule, stated for this feature:
  *"A remembered answer is not attached to the question that produced it; it is attached to
  its key. Every later request whose key matches inherits that answer, whether or not a human
  would call it the same question."* Today a row is keyed by root alone, so it answers for
  **both** the typed and the model door. Whether that is correct is OQ-2 — but whichever way
  it is decided, the key must encode the decision rather than leaving it implicit.
  (informed by LESSON-495)

- [ ] **BR-9: The durable form is verified separately from the session form.** They are
  different code paths and can disagree about breadth; *"a test that only exercises the
  in-session grant proves nothing about what a restart honors."* Every durable claim is
  checked by reading the config file **and** re-parsing it, paired with a refusal leg on the
  same fixture proving nothing was written. (informed by LESSON-495, LESSON-519, LESSON-520)

- [ ] **BR-10: What the surface says happened is what happened.** Today an unattended session
  at a *listed* root prints `… was refused without asking …` and then runs the skill: the
  client refuses `NoTerminal` and the daemon rewrites the settlement to `Allowed`. A
  log-scraper cannot tell a genuine refusal from a successful trusted run. This is the same
  class as BR-7 — a surface describing an outcome that did not occur.

- [ ] **BR-11: A repository-authored string on the wire is bounded and control-stripped at
  the door that mints it.** `ProjectSkillTrust::root`'s contract claims it is `display_for`-
  minted and *"bounded"*. Neither is true: it is `trust_root_name`, deliberately untruncated,
  and a directory name containing a newline or ESC is valid UTF-8 and passes through. The
  shipped CLI defuses at render, so there is no exploit today — but a third-party client
  reading the contract at face value would render it raw, and with the allowlist that same
  string also becomes an option label and a `config.toml` row. Either bound and strip it
  daemon-side or correct the contract. (informed by BUG-181)

- [ ] **BR-12: The model is documented about the gates that refuse it.** `skills.md` is 4,092
  bytes against a 4,096 ceiling and documents neither the acknowledgment — which now gates
  *both* doors — nor `trusted_project_roots`. A model refused by a gate it has no
  documentation for will answer from whatever it can see instead. Pay for the words by
  cutting elsewhere, or raise the ceiling deliberately. (informed by BUG-181)

## Acceptance Criteria

- [ ] AC-1 (BR-1, BR-2): A typed project-sourced skill raises the acknowledgment; a user-authored one does
  not. **The ordering is asserted from the raw prompt log, not a filtered view** — reversing
  the two gate calls must redden (this was vacuous in REQ-589 and is now mutation-verified).
- [ ] AC-2 (BR-3): The typed prompt says the user asked; the model-invoked prompt is byte-identical
  to its pre-REQ-589 form.
- [ ] AC-3: The BR-4 attack is reproduced and refused: bodies read from an unlisted tree, the
  session-root symlink re-pointed at a listed tree, unattended run **refuses**.
- [ ] AC-4 (BR-5): An unattended session at an **unlisted** root refuses. This is the criterion that
  keeps the gate meaningful and it must be mutation-sensitive.
- [ ] AC-5: An unattended session at a **listed** root proceeds with no prompt drawn.
- [ ] AC-6 (BR-9): The durable write is verified by reading the config **file** and re-parsing it,
  paired with a refusal leg on the same fixture proving nothing was written (BR-9).
- [ ] AC-7 (BR-8): A row granted for one door does not silently answer for the other — or, if OQ-2
  decides it should, a test asserts that breadth deliberately and the label says so (BR-8).
- [ ] AC-8: No surface claims a refusal that did not happen (BR-10). The existing test that
  pins the contradictory line is corrected, not preserved.
- [ ] AC-9: `cargo audit` clean; the previously verified non-exploitable vectors stay
  non-exploitable — symlink at a listed path, `..` traversal, percent-escape collisions,
  home-prefix confusion, case-insensitive and firmlinked filesystems.
- [ ] AC-12 (BR-6): Trusting `~/dev/repo` does NOT authorize `~/dev/repo/vendor/other`. The
  exact-match rule has no test today; a prefix match would let a dependency update place a
  tree inside a listed root and inherit its trust. Paired with a positive leg on the same
  fixture so neither passes by accident.
- [ ] AC-13 (BR-7): The option label names the write it performs, and a test pins label and
  effect together so they cannot drift — the failure LESSON-495 records is a prompt
  describing a write that provably could not happen.
- [ ] AC-14 (BR-11): A directory name containing a newline or ESC does not reach a client
  raw. Either the daemon bounds and strips it at the minting door (and a test asserts that),
  or the wire contract is corrected to say the client must defuse — but not the present
  state, where the contract claims a bounding that does not happen.
- [ ] AC-15 (BR-12): `skills.md` documents the acknowledgment and `trusted_project_roots`,
  and the existing byte-ceiling test still passes. The commit states what was cut to pay for
  the words.
- [ ] AC-10: The split itself is clean: REQ-589's offer behaves identically before and after
  the carve-out, and neither REQ answers OQ-1 differently from the other.
- [ ] AC-11: Dogfood — a real unattended invocation against a listed root, and against an
  unlisted one, on a machine where `$HOME` is not the daemon's launch environment (OQ-4).

## External Dependencies

- None. Every seam exists: `authorize_project_skill_trust`, `persist_web_tier` as the
  durable-write precedent, `config/set`, the discovery registry, `invoker_clause`.

## Assumptions

- [ ] ASSUME-A: The implementation on the REQ-589 branch is behaviourally what this spec
  describes. It was reviewed by six agents and the suite is green at 68 targets / 3,864
  tests, but **this spec was written from that review rather than from a fresh reading of
  the diff**, so a divergence between spec and code is possible and the carve-out should
  check rather than assume.
- [ ] ASSUME-B: `persist_web_tier` is an accepted precedent for a durable config write
  performed from inside a consent answer. If OQ-1 decides that precedent is itself wrong,
  this REQ inherits a larger blast radius than its own two seams.
- [ ] ASSUME-C: Splitting the branch is mechanically feasible without rewriting history the
  offer depends on. The trust commits (`b4e4b01`, `4be0c34`, `b071da5`, `bda079d`, `37a2e6c`)
  are separable by path, but `b4e4b01` made `accept_invocation` async and that signature
  change reaches callers the offer also touches.

## Open Questions

- [ ] **OQ-1: Does the durable trust write pass the daemon-wide commitment gates?**
  `persist_trusted_project_root` passes neither `refuse_daemon_wide` nor
  `refuse_unattested_commitment`. On a `--features presence` build, `config/set` demands a
  verified human; this write demands only a `permission/respond` frame, and
  `handle_permission_respond` performs no presence check. On a **shipped** build (presence
  non-default) it adds zero authority — the same actor can call `config/set` directly with
  strictly more power — so the exposure is confined to presence builds.
  **BUG-162 frames the question better than "is the write gated":** a `request_id` minted in
  one scope and resolved in a wider one, where the real question is *"may this connection
  speak for the machine?"* A trust row is a machine-wide fact answered through a
  session-scoped consent. *Lean:* fix it — the presence check needs only the verifier handle
  and the addressee `ConnectionId`, both already at the gate, **not** `&Daemon`/`&ConnState`
  threaded into a turn. **REQ-589's remedy write is the other half of this seam; the two
  REQs must not answer it differently.** (informed by BUG-162)

- [ ] **OQ-2: Should one row authorize both doors?** Today `durable_root` is `None` for
  `InvokedBy::Model` — correctly withholding the *offer to write* — but the *consultation*
  passes the raw root unconditionally, so a row answers for both. A user who adds a row so
  `teton --skill deploy` works in CI simultaneously grants an injected model standing
  permission to invoke **any** project skill in that tree, unattended, forever; chained with
  `permission_level = full` that reaches unattended arbitrary shell from repository-authored
  text. It is a widening relative to REQ-587 on the model door specifically; on the typed
  door it is strictly stronger than the pre-REQ-589 status quo. *Lean:* scope the row by
  invoker — LESSON-495's prescription is to make the key a function of the dimension so
  adding one is a compile error rather than a silent grant. (informed by LESSON-495)

- [ ] **OQ-3: Should `plan` refuse a typed project skill outright?** It now does, where it
  previously ran the body with *"not run at plan"* in its command slots. The counter-argument:
  `plan` is the level users select to explore a repository **read-only**, so refusing to
  expand that repository's own instructions is the most restrictive outcome at the safest
  level — inverted. *Lean:* no lean. This one genuinely needs the owner.

- [ ] **OQ-4: Is a `$HOME`-relative durable identity right for a row that outlives its
  session?** A row's meaning depends on `$HOME` at consult time, so a daemon later launched
  with a different `HOME` silently trusts a tree nobody named. Not an escalation on its own
  (an actor who can rewrite the daemon's environment can rewrite `config.toml`), but the row
  is documented as naming *a tree*, and a `$HOME`-relative string does not.

- [ ] **OQ-5: Should `trusted_project_roots` be validated?** No entry-length cap, no list
  cap, no `Config::validate` rule. Impact is negligible today — mismatches fail closed — but
  a security allowlist with no validation pass is worth a rule, if only to reject a row that
  is not a well-formed canonical mint.

## Out of Scope

- **Everything REQ-589 keeps**: the window-verdict classifier, the BR-7 remedy table, the one
  composer and its three sentences, the four-option single-select, the pressure suspension,
  the observed-rejection memo, the withdrawal, the `/doctor` pre-flight, and the remedy
  writes and their ordering.
- **Defending against a tree replaced *at* a listed path.** Accepted residual, documented: no
  name for a location can distinguish this, and `[web] permission_allow` has the same
  character. Worth re-reading before merge because the amplification is larger here — an
  attacker who gains write access to a listed repo (a merged PR, a postinstall script, a
  vendored dependency) gets arbitrary skill instructions into unattended sessions.
- **Re-litigating `persist_web_tier`'s own posture** (ASSUME-B).
- **`TETON_CONFIG` pointing at a repository-controlled file.** Not a defect of this work — a
  repo-controlled config can already set `permission_level = "full"`, strictly worse — but
  D-13's automation story is exactly where that invocation shape is attractive, so it earns
  one sentence in the docs rather than a rule here.

## Retrieved Context

- REQ-589 (spec, score 13): Offer to proceed when a skill expansion exceeds the budget
- LESSON-552 (lesson, score 11): Test the derivation, not the minter
- LESSON-518 (lesson, score 11): Reader-loop freedom needs a parked verifier
- LESSON-519 (lesson, score 11): "Inspect, not infer" needs the real artifact
- LESSON-520 (lesson, score 11): A gate before parse makes an invalid-payload test vacuous
- LESSON-501 (lesson, score 11): Carried state sheds its invariants silently
- LESSON-495 (lesson, score 11): A grant is only as narrow as its key
- LESSON-539 (lesson, score 10): Claim first, then re-read session state
- BUG-190 (bug, score 9): The arguments splice is not sub-framed
- LESSON-524 (lesson, score 9): Exposure is not callability
- BUG-161 (bug, score 9): Permission request ids collide across concurrent sessions
- BUG-162 (bug, score 9): model/confirm answerable by any connection
- BUG-189 (bug, score 8): A refusal that names no registered skill is silent on the surface
- BUG-191 (bug, score 8): No PTY leg for the acknowledgment prompt bytes
- BUG-181 (bug, score 8): The model affirms capabilities Teton does not have

*Retrieval note: the delegate body-read timed out on this 14-doc corpus (as it did on
REQ-589's Phase-5 diff), so LESSON-495 and BUG-162 — the two load-bearing for OQ-1 and OQ-2 —
were read directly and the remainder were used from their frontmatter and prior-session
context. That is a partial fallback, stated rather than hidden.*
