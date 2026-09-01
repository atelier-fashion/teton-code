---
id: REQ-607
title: "A withheld variable should not look like an ssh problem, and the agent should be opt-in-able"
status: draft
deployable: true
created: 2026-08-31
updated: 2026-09-01
component: "daemon/tools"
domain: "developer-experience"
stack: ["rust", "daemon"]
concerns: ["developer-experience", "security", "observability"]
tags: ["shell-tool", "env-allowlist", "ssh-agent", "assume-024", "diagnosis", "opt-in"]
---

## Description

REQ-596 replaced the `shell` tool's credential denylist with a twelve-name
positive allowlist. That was right, and this REQ does not reopen it. It closes
the gap the change left behind, recorded as **ASSUME-024** (`unresolved`).

The assumption's own wording separates two defects:

> `SSH_AUTH_SOCK` is the likeliest to bite: a `git push` over ssh inside a shell
> command will now fail where it previously worked, **and the failure will look
> like an ssh problem rather than a Teton one.**

1. A **capability loss** — some commands genuinely stop working.
2. A **misattributed error** — the user is handed a symptom that names the wrong
   cause, and goes looking in the wrong place.

The second is the one that costs real time, and it is the more general problem:
it applies to *every* variable the allowlist withholds, not only the one
`ASSUME-024` predicted. A user cannot debug an absence they were never told
about.

**The rejection of `SSH_AUTH_SOCK` was reasoned, and stands.** `child_env.rs`
records it: the variable passes BR-2.1's first half (a `git push` needs it) and
fails the second — *"worse than by holding a credential: it is a handle to an
agent that lends them."* Admitting it by default would grant every
model-issued command the ability to authenticate as the user, to any host, for
the life of that command. This REQ keeps the default and makes the consequence
visible and escapable.

## System Model

### Entities

| Entity | Field | Type | Constraints |
|--------|-------|------|-------------|
| `WithheldDiagnosis` | `named` | set of string | Variable names the advisory may mention. **Drawn only from the statically documented rejection table** — never from the daemon's live environment or config (BR-4) |
| `WithheldDiagnosis` | `sentence` | string | The advisory appended to a failing command's output |
| `ShellConfig` | `allow_ssh_agent` | boolean | **New.** Defaults `false`; the single explicit opt-in |

### Events

| Event | Trigger | Payload |
|-------|---------|---------|
| _None._ | REQ-596 OQ-1 settled that no `shell_env_withheld` event is emitted, and that stands — see Assumptions. The advisory is **in-band on the failing call**, not a bus event. | |

### Permissions

| Action | Roles Allowed |
|--------|---------------|
| Set `allow_ssh_agent` | Config author only — never the model, never a tool call |

## Business Rules

- [ ] BR-1: When a `shell` invocation exits non-zero **and** the command names a
      program whose failure is plausibly explained by a withheld variable, the
      tool result carries one additional sentence naming **Teton** as the cause
      and the config key that changes it. A user must be able to reach the
      explanation from the failure alone, without knowing REQ-596 exists.
- [ ] BR-2: The advisory is attached **only to a failing invocation**. A command
      that succeeded is not annotated, and neither is a command whose failure has
      nothing to do with the environment. A notice on every call is noise, and
      noise is how a real notice stops being read.
- [ ] BR-3: The advisory is **in-band on the tool result**, not an event. This is
      not a reversal of REQ-596 OQ-1: that settled that a *bus event carrying a
      bare count* is not actionable and that BR-5 forbids the payload that would
      make it actionable. A targeted sentence on the one call that failed is a
      different mechanism answering a different question.
- [ ] BR-4: **REQ-596 BR-5 is amended, in scope, and the amendment is recorded
      on REQ-596.** BR-5 as shipped is unconditional — *"no withheld variable
      **name** appears in tool output"* — and an advisory naming `SSH_AUTH_SOCK`
      in a tool result is exactly that. Saying this REQ leaves BR-5 untouched
      would be false, and would tell the next reader the boundary is where it is
      not. What BR-5 **protects** is disclosure of facts about *this machine*;
      what it **says** is broader than that. This REQ narrows the letter to the
      rationale and no further:
  - **Nameable:** variables in the statically documented rejection table in
    `child_env.rs`. These are public in the source, identical on every
    installation, and reveal nothing a reader of the repository does not
    already know.
  - **Unnameable, exactly as before:** any variable discovered from the daemon's
    live environment, and any name resolved from a configured
    `auth_ref = "env:<NAME>"`. No credential *value* is nameable under any
    condition — that half of BR-5 is untouched.
  - The amendment is written into REQ-596's BR-5 with its date and this REQ's
    id, per the house pattern REQ-602 TASK-306 used on REQ-599's ACs. A rule
    narrowed in a downstream spec and left unmarked upstream is a rule with two
    readings.
- [ ] BR-5: `[shell] allow_ssh_agent = true` admits `SSH_AUTH_SOCK` to the child
      environment and nothing else. It is not a general escape hatch and does not
      accept a list.
- [ ] BR-6: The opt-in is explicit and **greppable**, the shape BUG-202 settled
      on for `allow_cleartext` (LESSON-578): a secure default plus one named key,
      never a heuristic that guesses when the agent is wanted.
- [ ] BR-7: The opt-in does not weaken REQ-596's guarantees. BR-1's unconditional
      credential removal, BR-3's after-the-allowlist ordering, and BR-8's
      `looks_like_credential_url` value check all still run, and
      `MCP_BASE_ENV_ALLOW` is untouched — turning the agent on for `shell` must
      not turn it on for a spawned MCP server (REQ-596 BR-7.1).
- [ ] BR-8: With `allow_ssh_agent = false`, `SSH_AUTH_SOCK` is absent — the
      default is unchanged by this REQ.

## Acceptance Criteria

- [ ] AC-1: A failing `git push` over ssh in a `shell` call, with the agent not
      admitted, produces a result whose text names Teton and the config key. The
      assertion is on the **rendered tool result**, not on a log line or an
      internal type (LESSON-519).
- [ ] AC-2: A `shell` call that fails for an unrelated reason (`exit 1`) carries
      **no** advisory — BR-2's other half, and the one that keeps AC-1's sentence
      worth reading.
- [ ] AC-3: A `shell` call that **succeeds** carries no advisory.
- [ ] AC-4: **BR-3 guard.** The advisory travels on the tool result and nowhere
      else: for the same failing call as AC-1, the session's event stream is
      drained and carries **no** event describing the withheld set — no count, no
      name, no `shell_env_withheld`-shaped payload. Asserted by draining the
      events the call actually publishes, not by grepping for a type name that a
      later rename would silently defeat. REQ-596 OQ-1 settled that this event is
      not emitted; BR-3 adds a targeted sentence without reopening it, and this
      AC is what keeps the two mechanisms distinct in fact rather than in prose.
- [ ] AC-5: **BR-4 guard.** With `auth_ref = "env:MY_LLM_CRED_SENTINEL"`
      configured and a failing command, the advisory names neither
      `MY_LLM_CRED_SENTINEL` nor any other variable read from the live
      environment. Asserted against the rendered output.
- [ ] AC-6: **Mutation.** Deleting the failure-shape condition so the advisory
      is appended unconditionally turns AC-2 and AC-3 red. Recorded in the test's
      doc comment with what actually went red.
- [ ] AC-7: With `allow_ssh_agent = true`, `SSH_AUTH_SOCK` is present in the
      child environment; with it `false` or absent, it is not. Asserted by
      inspecting the composed child environment, not by observing a command
      succeed.
- [ ] AC-8: **BR-5's "and nothing else".** The shell child's composed environment
      with the flag on differs from its value with the flag off by **exactly one
      entry**, `SSH_AUTH_SOCK`. Asserted as a set difference in **both**
      directions — nothing gained beyond that one name, and nothing lost — over
      the whole composed map, not by spot-checking the names the test happens to
      think of. AC-7 shows the flag admits the variable; this shows it admits
      only the variable, which is the half that keeps `allow_ssh_agent` from
      becoming the general escape hatch Out of Scope refuses.
- [ ] AC-9: **BR-7 guard.** With `allow_ssh_agent = true`, a spawned MCP
      server's composed environment is byte-identical to its value with the flag
      off. The two paths share one composer through a parameter (REQ-596
      BR-7.1); this asserts the parameter is doing its job.
- [ ] AC-10: With `allow_ssh_agent = true`, a variable named by a configured
      `auth_ref = "env:<NAME>"` is **still** absent — REQ-596 BR-3's unconditional
      removal is not reachable through the new flag.
- [ ] AC-11: Test fixtures use obviously synthetic sentinels containing
      `SENTINEL` (LESSON-497).
- [ ] AC-12: `child_env.rs`'s rejection table is updated: `SSH_AUTH_SOCK`'s row
      records that it is now reachable by opt-in, so the table does not read as
      "rejected, full stop" once the flag exists.
- [ ] AC-13: **BR-4's upstream half.** REQ-596's BR-5 carries a dated amendment
      naming this REQ and stating the narrowing, so the rule reads the same from
      either spec. Checked by reading REQ-596, not by asserting it here.

## External Dependencies

- None. REQ-596 shipped the composer, the parameterised allowlist, and the
  credential provider this builds on.

## Assumptions

- **The misattributed error is the expensive half.** A user who is told Teton
  withheld something can act; a user handed an ssh error cannot. This is why
  BR-1 comes first and is not gated on the opt-in existing.
- REQ-596 OQ-1's settlement stands for what it settled — a bus event carrying a
  count. BR-3 states the distinction rather than assuming a reader will draw it.
- `SSH_AUTH_SOCK` is the variable worth naming, but ASSUME-024's resolution
  criterion is **dogfooding**, not prediction. BR-1 is written to make *any*
  withheld-variable failure self-describing, so a session that turns up
  `CARGO_HOME` instead is served by the same mechanism.

## Open Questions

- [ ] OQ-1: Should the advisory also fire on a **successful** command that
      produced suspicious output (a `git push` printing "Permission denied
      (publickey)" and exiting 0 via a wrapper)? Cheap to add, and a false
      positive here costs a confusing sentence on a working command — BR-2 says
      no for now.
- [ ] OQ-2: Does `allow_ssh_agent` want per-session scope rather than config
      scope? A session-scoped grant is the smaller capability, but REQ-596's
      Permissions row is deliberate that this class of decision is the config
      author's and never the model's.

## Out of Scope

- **A general `[shell] extra_env = [...]`** — REQ-596's OQ-2, still open, and
  deliberately not answered here. It trades a narrow known risk for a broad
  unknown one: a user allowlisting a name that holds a bare-token secret
  reopens exactly the class REQ-596 closed. BR-3 and BR-8 narrow the residual
  but do not remove it. That belongs in its own decision.
- **Per-invocation consent for the agent** (asking at the permission gate when a
  command would need it). The better security/UX trade and much more machinery;
  it earns its cost only once dogfooding shows the agent is wanted often.
- Sandboxing the shell child's network access — still REQ-596's named residual.
- Changing the twelve-name allowlist itself.

## Retrieved Context

- ASSUME-024 (assumption, unresolved): a twelve-name allowlist is enough for ordinary shell commands
- REQ-596 (spec): the credential-safe shell environment this builds on
- LESSON-578 (lesson): a secure default plus one explicit, greppable opt-out beat a permissive default and a heuristic
- LESSON-519 (lesson): an "assert by inspection" AC needs the real rendered artifact
- LESSON-497 (lesson): plant sentinels, not lookalikes
- BUG-205 (bug): a refusal that names a remedy no command can reach — the failure mode BR-1 exists to avoid
