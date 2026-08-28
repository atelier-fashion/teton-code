---
id: REQ-597
title: "Secure-by-default privacy boundaries: the product's central promise, on by default"
status: draft
deployable: true
created: 2026-08-28
updated: 2026-08-28
component: "daemon/config"
domain: "privacy"
stack: ["rust", "daemon"]
concerns: ["privacy", "security", "developer-experience"]
tags: ["defaults", "boundaries", "local-only", "secure-by-default", "br-1", "first-run"]
---

## Description

Teton Code's second headline promise is that "paths marked `local-only` never
leave the machine." That promise is kept — the egress choke point enforces it
and egress-capture tests verify it. The problem is the word *marked*: on a stock
install, **nothing is marked**.

Three defaults compound:

1. `boundaries` is `#[serde(default)]` — an absent `[[boundaries]]` table yields
   an empty `Vec`, so the boundary matcher has nothing to match.
2. `[privacy] redact` defaults to `false`, so the REQ-562 secret scan does not
   run.
3. `read`, `glob`, and `grep` are in `READ_ONLY_TOOLS` and auto-`Allow` at the
   default `guarded` permission level — no prompt.

REQ-583 additionally permits the session root to be `$HOME` or the filesystem
root. Composing these: a default session started in a home directory will read
`~/.ssh/id_rsa`, `~/.aws/credentials`, or a project's `.env` with no prompt and
no boundary check, and ship the contents to whichever remote provider the user
bound. Every mechanism built to prevent that is real, correct, and opted out of.

This REQ ships a default boundary set so the promise holds on first run, and
makes the dangerous-root case visible. It deliberately does **not** flip
`redact` to `true` — that has a latency and local-model-availability cost that
belongs in its own decision.

## System Model

### Entities

| Entity | Field | Type | Constraints |
|--------|-------|------|-------------|
| `PrivacyBoundary` | `glob` | string | Existing type; unchanged |
| `PrivacyBoundary` | `origin` | enum `{ Builtin, User }` | **New.** Distinguishes a shipped default from a user-authored row |
| `DefaultBoundarySet` | `globs` | ordered list of string | The shipped `local-only` set (see BR-1) |
| `PrivacyConfig` | `disable_default_boundaries` | boolean | **New.** Defaults `false`; explicit opt-out |

### Events

| Event | Trigger | Payload |
|-------|---------|---------|
| `boundary_defaults_applied` | Session start where the builtin set contributed at least one glob | `count` (integer) |
| `unbounded_root_warning` | Session start where root is `Home`/`FilesystemRoot` **and** the effective boundary set is empty | `root_kind` |

### Permissions

| Action | Roles Allowed |
|--------|---------------|
| Disable the default boundary set | Config author only — never the model, never a tool call |

## Business Rules

- [ ] BR-1: The daemon ships a builtin `local-only` boundary set covering at minimum: `**/.env`, `**/.env.*`, `**/.ssh/**`, `**/*.pem`, `**/*.key`, `**/id_rsa*`, `**/id_ed25519*`, `**/.aws/**`, `**/.npmrc`, `**/.netrc`, `**/.git-credentials`, `**/.docker/config.json`, `**/.kube/config`.
- [ ] BR-2: The builtin set applies when the user has declared no `[[boundaries]]` rows **and** when they have declared some. A user-authored boundary list **adds to** the builtin set; it does not replace it. Silently losing the builtin protections by writing one unrelated boundary row is the failure this rule prevents.
- [ ] BR-3: The builtin set is disabled only by an explicit `[privacy] disable_default_boundaries = true`. There is no implicit path to an empty boundary set. This is deliberately the shape BUG-202 settled on for `allow_cleartext`: a secure default plus one explicit, greppable opt-out, rather than a permissive default or a heuristic that guesses which cases are safe (informed by LESSON-578).
- [ ] BR-4: A builtin boundary is indistinguishable from a user boundary **at enforcement time** — same matcher, same `privacy_block`, same session taint. `origin` exists for reporting and for BR-6, never to weaken enforcement.
- [ ] BR-5: A session whose root is `Home` or `FilesystemRoot` **and** whose effective boundary set is empty (only reachable via BR-3) emits `unbounded_root_warning` once at session start, on a user-visible surface — not only the daemon log (informed by REQ-571 BR-4: an audit signal that reaches only the log can be suppressed by the party it indicts).
- [ ] BR-6: `teton config show` (or the equivalent surface) reports the effective boundary set with each row's origin, so a user can tell what they are protected by without reading the source.
- [ ] BR-7: Adding the builtin set must not change the *semantics* of an existing user boundary. A user glob that already matches a builtin path keeps its own behavior; duplicate matches resolve to one block, not two.
- [ ] BR-8: The builtin globs match **repo-root-relative canonical forms** produced by the existing provenance mint, never raw model-supplied path strings (informed by REQ-571 BR-1/BR-2 — non-canonical spellings were how boundaries were evaded before).

## Acceptance Criteria

- [ ] AC-1: On a config with no `[[boundaries]]` table, an egress-capture test proves the bytes of a planted `~/.ssh/id_rsa` sentinel appear in **no** captured remote payload, and a `privacy_block` is emitted. This is an egress-capture integration test, not code inspection (required by conventions.md for any BR-1 claim).
- [ ] AC-2: AC-1 repeated for `.env`, `.aws/credentials`, and `.netrc` sentinels.
- [ ] AC-3: With one unrelated user boundary row declared (`src/vendor/**`), the builtin set still blocks the AC-1 sentinel — proving BR-2's additive semantics.
- [ ] AC-4: With `disable_default_boundaries = true`, the sentinel **is** forwarded (the opt-out genuinely works) and `unbounded_root_warning` fires when the root is `Home`.
- [ ] AC-5: **Mutation test** — removing the builtin set from the composition causes AC-1, AC-2, and AC-3 to fail. The mutation is recorded in each test's doc comment (conventions.md; informed by LESSON-550).
- [ ] AC-6: The AC-1 sentinel is obviously synthetic and contains `SENTINEL`; no fixture resembles a real provider key shape (informed by LESSON-497).
- [ ] AC-7: A test asserts the builtin globs are matched against the canonical provenance form by planting a path that reaches the same file through a symlink and a `..` segment, and asserting both are blocked (informed by REQ-571).
- [ ] AC-8: A source-level region check asserts the builtin set is composed in exactly one place, so a second composition site fails rather than drifting (informed by conventions.md's sweep-not-count rule).
- [ ] AC-9: `config show` output includes each boundary's origin, asserted by inspecting the rendered artifact rather than inferring from an exit code (informed by LESSON-519).
- [ ] AC-10: Upgrade check — a machine with an existing config that declares boundaries gains the builtin set on upgrade without a config rewrite, and its own rows are byte-unchanged on disk.

## External Dependencies

- None. The boundary matcher, provenance mint, and egress-capture harness all exist.

## Assumptions

- Users would rather have a false positive (a `.env` they wanted to send is blocked, with a clear message and an opt-out) than a silent credential leak. This is the inverse of the current default and is the central judgment of this REQ.
- The builtin glob list is small enough that its matching cost is negligible against the existing per-file provenance work. If measurement contradicts this, the list — not the rule — is what changes.
- `RootKind::Home` and `RootKind::FilesystemRoot` remain the two roots worth warning about (REQ-583).

## Open Questions

- [ ] OQ-1: Should `[privacy] redact = true` also become the default once a local tier is installed? It is the natural companion to this REQ but carries a latency cost per egress and a hard dependency on local-model availability. Deliberately deferred, not dismissed.
- [ ] OQ-2: Should BR-5's warning escalate to a **refusal** when the root is `FilesystemRoot` with an empty boundary set? A refusal is safer; it also makes `disable_default_boundaries` a partial trap.
- [ ] OQ-3: Does the builtin set belong in the config document as commented-out rows on first write (discoverable, editable) or purely in code (cannot be silently deleted)? BR-3 currently implies code.

## Out of Scope

- Flipping `[privacy] redact` to `true` (OQ-1).
- Changing which tools are in `READ_ONLY_TOOLS`, or the `guarded` auto-allow policy. That is a permission-model change with much wider blast radius and deserves its own REQ.
- Narrowing what REQ-583 permits as a session root.
- Any scanning of file *contents* to infer sensitivity — this REQ is path-shaped only.

## Retrieved Context

- REQ-571 (spec, score 14): Canonical provenance identity for privacy-boundary enforcement
- LESSON-432 (lesson, score 14): Provenance must derive from what a tool touches, not from an argument name
- LESSON-550 (lesson, score 12): A defect fixed once comes back unless a test asserts the absence
- LESSON-494 (lesson, score 12): A security gate and the client that executes the request must share one parser
- REQ-562 (spec, score 12): redact — a model-based secret and PII scan inside the egress choke point
- LESSON-511 (lesson, score 10): A default trait-method body makes "who forgot to override this" a stale census
- LESSON-492 (lesson, score 10): A composite guard's failure path must not discard established evidence
- REQ-563 (spec, score 9): Opt-in web lookup through the egress choke point
- REQ-568 (spec, score 9): Session-scoped event delivery and bounded request frames
- LESSON-490 (lesson, score 9): A guard that runs on an encoded form is tested against the encoder's output
- BUG-165 (bug, score 8): The search credential only speaks Bearer
- REQ-569 (spec, score 8): Session attach requires a grant
- REQ-570 (spec, score 8): Human-attested attach consent
- LESSON-497 (lesson, score 8): Plant sentinels, not lookalikes
- REQ-591 (spec, score 7): The project-skill trust gate and its unattended allowlist
- LESSON-578 (lesson, added post-retrieval): A rule attached to a UI flow guards one of the doors the record can come in through
