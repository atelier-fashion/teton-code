---
id: REQ-596
title: "A credential-safe environment for the shell tool, and an honest egress claim"
status: approved
deployable: true
created: 2026-08-28
updated: 2026-08-29
component: "daemon/egress"
domain: "privacy"
stack: ["rust", "daemon"]
concerns: ["security", "privacy"]
tags: ["shell-tool", "credentials", "env-scrub", "allowlist", "egress-bypass", "auth-ref"]
---

## Description

The `shell` tool spawns `sh -c <model-supplied command>` with an environment
filtered by `scrub`, whose predicate is `is_secret_var(key, value)` — **two
signals, not one**. The name signal is `is_secret_key`, a case-insensitive
substring denylist (`SECRET`, `PASSWORD`, `PASSWD`, `TOKEN`, `KEY`,
`CREDENTIAL`, plus a delimiter-bounded `PAT`). The value signal is
`looks_like_credential_url`, which catches a `scheme://user:pass@host` URL — the
shape `DATABASE_URL` often takes, which a name-only rule cannot see (REQ-544
MED-1).

Naming both matters, because this REQ replaces the *name* half and must not
quietly discard the *value* half on the way past (BR-8). The gap is in the name
half: a credential whose variable name misses every substring **and** whose
value is a bare token — not a URL — is caught by neither signal and survives
into the child process.

This matters because `auth_ref = "env:<VAR>"` is a **first-class, validated
credential form**: the daemon already knows, at config-load time, exactly which
environment variable names hold provider credentials. It never tells the
scrubber. So `env:DEEPSEEK_AUTH`, `env:MY_LLM_CRED`, and `env:GEMINI_PW` are
configured as credentials by the user and then handed to model-driven code
execution. A single `echo $DEEPSEEK_AUTH` puts the value in tool output, which
is shipped to the remote provider on the next turn.

The fix already exists in this codebase. The MCP spawn path composes its child
environment from a **positive allowlist** (`MCP_BASE_ENV_ALLOW`), and its own
doc comment says it is "stricter than the `shell` tool's denylist scrub." This
REQ closes the gap by making the shell tool at least as strict as the sibling
path that already got it right.

Separately, and in the same area: the egress module's header states that "every
byte that leaves this machine for a remote provider passes through here," and
`architecture.md` states that "a tool that reaches the network is handed
transport; it never constructs one." Neither sentence is true of `shell`, which
can reach `curl`. This REQ does not attempt to sandbox the network — it makes
the documented claim match the code, so the residual is *known* rather than
contradicted. Closing the network path itself is deliberately out of scope and
named below.

## System Model

### Entities

| Entity | Field | Type | Constraints |
|--------|-------|------|-------------|
| `ChildEnvPolicy` | `allow` | set of string | The only variable names admitted to a child by name. Membership is BR-2.1's recorded set. Per BR-7.1 this is a **parameter** of the shared composer, not a global: the shell and MCP call sites each pass their own constant, so widening one can never widen the other |
| `ChildEnvPolicy` | `credential_value_rule` | predicate | `looks_like_credential_url`, applied to what `allow` admits (BR-8). Retained from today's scrub, not superseded by it — `allow` reasons about names, this reasons about values |
| `ChildEnvPolicy` | `credential_env_names` | set of string | Resolved from every configured `auth_ref = "env:<NAME>"`; removed unconditionally |
| `ChildEnvPolicy` | `path_floor` | string | The BUG-174 `PATH` floor, applied after composition |

### Events

| Event | Trigger | Payload |
|-------|---------|---------|
| `shell_env_withheld` | One or more variables were withheld from a shell child | `count` (integer). **Never** the withheld names or values |

### Permissions

| Action | Roles Allowed |
|--------|---------------|
| Invoke `shell` | Unchanged — governed by the existing permission gate |

## Business Rules

- [ ] BR-1: Every environment variable named by a configured `auth_ref = "env:<NAME>"` is absent from the shell child's environment, regardless of whether its name matches any credential-shaped pattern. The credential set is derived from the loaded config, not guessed from the name (informed by LESSON-432 — derive from what a thing *is*, not from the shape of its name).
- [ ] BR-2: The shell child's environment is composed by a **positive allowlist**. A variable not on the allowlist and not explicitly declared is absent. Adding a new variable to the daemon's own environment must not silently widen what the child sees.
- [ ] BR-2.1: The allowlist has a **named starting set**, not an unstated one. It is the MCP path's twelve (`PATH`, `HOME`, `TMPDIR`, `TZ`, `TERM`, `USER`, `LOGNAME`, `SHELL`, `LANG`, `LANGUAGE`, `LC_ALL`, `LC_CTYPE`) plus any addition that meets this criterion and only this one: **a variable an ordinary development command needs in order to run at all, which cannot hold a credential.** `/architect` may extend the set under that criterion and must record each addition's justification; it may not leave the membership implicit. An allowlist nobody wrote down is a denylist with extra steps — the reviewer cannot tell an omission from a decision.
- [ ] BR-3: BR-1 is enforced **after** BR-2, unconditionally. A credential env name that also appears on the allowlist is still removed — the allowlist cannot re-admit it.
- [ ] BR-4: The BUG-174 `PATH` floor is preserved: the child receives a `PATH` that names the package-manager prefixes present on this machine, so user-installed commands remain reachable under launchd.
- [ ] BR-5: No credential *value* and no withheld variable *name* appears in tool output, error text, logs, or any event payload (informed by REQ-562 BR-6 — findings carry spans and kinds, never the matched text).
- [ ] BR-6: `egress/mod.rs`'s module header and `.adlc/context/architecture.md` state the `shell` exception explicitly: the choke point covers provider and MCP traffic, and a shell child can reach the network outside it.
- [ ] BR-7: The composer is a single function shared with, or structurally parallel to, the MCP path's `compose_child_env`. Two independent composers that can drift are not acceptable (informed by LESSON-494 — the gate and the executor must not use two different parsers).
- [ ] BR-7.1: If the shell's allowlist differs from the MCP server's by even one name, the shared composer takes the allowlist as a **parameter**; the two call sites pass their own constant. Sharing must not be achieved by widening `MCP_BASE_ENV_ALLOW` to cover the shell's needs — that would hand every spawned MCP server the increment as a side effect of a change made for the shell tool, a security regression in the path that is currently correct. If the two sets turn out identical, they still pass through the same parameter, so a later divergence is a one-line change at a call site rather than a fork of the composer.
- [ ] BR-8: The **value** signal survives the rewrite. `looks_like_credential_url` is applied to whatever the allowlist admits, so an allowlisted or user-extended name whose value is a `scheme://user:pass@host` URL is still withheld. This half of today's scrub is not superseded by the allowlist: an allowlist reasons about names, and this reasons about values. If `/architect` concludes it is genuinely dead under the composed policy, it is retired **explicitly**, with the reason recorded — never dropped as an artifact of the rewrite. BUG-155 is the mirror of this hazard and the reason it earns a rule: there, a REQ asserted a fallback was deleted when a second copy had survived. A claim about the fate of pre-existing code is checked, in both directions, not asserted.

## Acceptance Criteria

- [ ] AC-1: With `auth_ref = "env:DEEPSEEK_AUTH"` configured and `DEEPSEEK_AUTH` set in the daemon's environment, a `shell` invocation of `env` produces output containing no occurrence of the variable name or its value.
- [ ] AC-2: AC-1 holds for a name matching **no** denylist substring (`MY_LLM_CRED`, `GEMINI_PW`, `LLM_AUTH`) — proving the fix is not the old substring rule in new clothing.
- [ ] AC-3: A variable that is neither on the allowlist nor a credential (`RANDOM_UNRELATED_VAR=1`) is absent from the child, proving BR-2's allowlist direction rather than an extended denylist.
- [ ] AC-3.1: **The positive direction.** Every name in BR-2.1's starting set is *present* in the child with the daemon's value (`PATH` excepted — AC-4 owns it). Without this, AC-3 is satisfiable by an allowlist that admits nothing, and a shell tool that can run no ordinary command would pass every other criterion here.
- [ ] AC-3.2: The allowlist's membership is asserted against BR-2.1's recorded set, so adding a name to the constant without amending the REQ fails the test. The assertion names the expected set literally rather than comparing the constant to itself.
- [ ] AC-4: `PATH` in the child names the machine's package-manager prefixes (BUG-174 regression guard).
- [ ] AC-4.1: **BR-7.1 guard** — a test asserts `MCP_BASE_ENV_ALLOW` is unchanged by this REQ, and that the MCP child's composed environment is byte-identical to its pre-REQ form for a fixed daemon environment. Sharing the composer must be provably free for the MCP path, not merely intended to be.
- [ ] AC-4.2: **BR-8 guard** — a variable on the allowlist whose value is `scheme://user:pass@host` is withheld from the child, proving the value signal still runs after the allowlist. Paired with a same-named variable holding an ordinary non-URL value, which *is* admitted — so the test pins the value rule and not the name.
- [ ] AC-5: **Mutation test** — deleting the BR-1 credential-removal step causes AC-1 and AC-2 to fail; separately, deleting the BR-8 value check causes AC-4.2 to fail. Both mutations are recorded in the tests' doc comments. A test that passes with the guard removed is not evidence the guard works (informed by LESSON-550 — assert the defect's absence, not the remedy's presence; and conventions.md "show the test can fail").
- [ ] AC-6: The test asserts the **absence of the value in captured child output**, not the presence of a scrub call.
- [ ] AC-7: Test fixtures use obviously synthetic sentinels containing `SENTINEL`, never realistic provider key shapes (informed by LESSON-497).
- [ ] AC-8: A source-level check asserts that the shell child's environment is constructed only by the shared composer — a second construction site fails the check. This is a **region check over the source**, not a count of call sites (informed by LESSON-568 as recorded in conventions.md; a relocated call keeps a count identical).
- [ ] AC-9: `architecture.md` and the `egress/mod.rs` header name the shell exception, and a test asserts the architecture doc contains the exception sentence, so the claim cannot silently revert to the false form.

## External Dependencies

- None. The allowlist model, the `PATH` floor helper, and the `auth_ref` resolver all already exist in-tree.

## Assumptions

- The set of `env:` auth_refs is knowable at shell-spawn time from the loaded config. If a provider is added mid-session, the composer reads the current config rather than a snapshot taken at daemon start (informed by LESSON-539 — a pre-claim snapshot is stale; read the authoritative state at the point of use).
- Withholding an unexpected variable may break a user's shell command. BR-2 accepts that cost; the alternative silently leaks credentials.

## Open Questions

- [ ] OQ-1: Should `shell_env_withheld` be emitted at all? It tells a user why their command lost a variable, but a count alone may be too vague to act on, and anything more specific risks naming a credential.
- [ ] OQ-2: Should the allowlist be user-extensible (`[shell] extra_env = [...]`)? **Partly settled:** a user who allowlists a *configured* credential name cannot defeat BR-1, because BR-3's removal is unconditional and runs last; and a user who allowlists a name holding a credential *URL* is caught by BR-8. What remains genuinely open is the residual neither rule covers — a user allowlisting a name that holds a bare-token secret the daemon was never told about. That is the same class this REQ closes for `auth_ref` credentials, reopened by hand at the user's request, which may be the correct trade. Extensibility is deferred, not refused; if it ships, this residual is what its own ACs must speak to.

## Out of Scope

- Sandboxing the shell child's **network access** (e.g. `sandbox-exec` with no `network*` on macOS). That is the real fix for the egress bypass and deserves its own REQ with its own platform matrix; this REQ only stops the credential from being in the child's hands and makes the doc honest.
- Command filtering or a network-shaped-command consent prompt.
- Changing the MCP path, which already implements the target model.
- The provider-side cleartext endpoint refusal — **shipped separately as
  BUG-202** (merged 2026-08-28). Its shape is precedent for this REQ: a secure
  default with an explicit, greppable opt-out beat both a permissive default and
  a heuristic (informed by LESSON-578).

## Retrieved Context

- LESSON-432 (lesson, score 14): Provenance must derive from what a tool touches, not from an argument name
- LESSON-550 (lesson, score 12): A defect fixed once comes back unless a test asserts the absence
- LESSON-494 (lesson, score 12): A security gate and the client that executes the request must share one parser
- REQ-562 (spec, score 12): redact — a model-based secret and PII scan inside the egress choke point
- LESSON-511 (lesson, score 10): A default trait-method body makes "who forgot to override this" a stale census
- LESSON-492 (lesson, score 10): A composite guard's failure path must not discard established evidence
- REQ-563 (spec, score 9): Opt-in web lookup through the egress choke point
- LESSON-490 (lesson, score 9): A guard that runs on an encoded form is tested against the encoder's output
- BUG-165 (bug, score 8): The search credential only speaks Bearer
- LESSON-497 (lesson, score 8): Plant sentinels, not lookalikes
- REQ-571 (spec, score 14): Canonical provenance identity for privacy-boundary enforcement
- REQ-591 (spec, score 7): The project-skill trust gate and its unattended allowlist
- LESSON-578 (lesson, added post-retrieval): A rule attached to a UI flow guards one of the doors the record can come in through
