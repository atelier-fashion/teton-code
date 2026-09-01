# REQ-607 — Architecture

## Approach

Two changes, deliberately kept apart, because they answer different questions
and one of them must keep working when the other is switched off.

1. **The advisory** (BR-1..BR-4). A failing `shell` command that names a program
   whose failure a withheld variable plausibly explains gets one extra sentence
   on its **tool result**. Nothing is emitted, nothing is logged, nothing new
   crosses egress.
2. **The opt-in** (BR-5..BR-8). A new `[shell] allow_ssh_agent` key, default
   `false`, adds exactly one name to the `shell` path's allowlist **parameter**.
   `compose_child_env` is not touched.

The second is the reason the first is honest rather than smug: BR-1 tells a user
what happened, and BR-5 gives them somewhere to go. Neither depends on the other
to be correct — the advisory fires on any table entry that is genuinely absent,
including entries that will never have a key.

### Where each piece lands

| Piece | File | Why there |
|---|---|---|
| `SSH_AUTH_SOCK` name constant, diagnosis table, `shell_env_allow(bool)`, policy reader | `crates/tetond/src/child_env.rs` | BR-4 makes the *statically documented rejection table* the nameable set, and that table is this file's doc comment. Table and code beside each other is what lets a derived check keep them in step. |
| The sentence, and the decision to speak it | `crates/tetond/src/harness/tools/shell.rs` | The tool's presentation. `TIMEOUT_CONSENT_HINT` is the exact precedent: `run_bounded` reports a fact, the tool words it, and the skill path words the same fact differently. |
| `ShellConfig` / `Config::shell` | `crates/teton-core/src/config.rs` | Where `[privacy]`, `[web]`, `[cost]`, `[permissions]`, `[skills]` already live. |
| One live-config read | `crates/tetond/src/runtime/mod.rs` + `crates/tetond/src/main.rs` | The existing `set_credential_env_names_provider` bootstrap, widened rather than duplicated. |

## ADR-A: The advisory's trigger is a static table, and "withheld" means *actually absent*

`child_env.rs` gains a table beside the rejection table it already documents:

```rust
pub(crate) struct WithheldVar {
    /// Nameable in tool output — it is in this file's rejection table, public
    /// in the source and identical on every install (REQ-596 BR-5 as amended).
    pub name: &'static str,
    /// Programs whose failure this variable plausibly explains.
    pub programs: &'static [&'static str],
    /// The config key that admits it, or `None` where no opt-in exists.
    pub opt_in_key: Option<&'static str>,
}
```

Three predicates must all hold before a sentence is written, and the middle one
is the decision worth recording:

1. The command exited **non-zero** (BR-2).
2. The variable is on the table, **is present in the daemon's environment**, and
   is **absent from the composed child environment**.
3. The command names one of that entry's programs in **command position**.

Predicate 2 is the honest definition of "withheld", and the alternative was
rejected. Firing whenever a table name is merely off the allowlist would put the
sentence on a machine with no `ssh-agent` running — telling a user Teton
withheld something it never had, and pointing them at a config key that would
not have helped. That is BUG-205's failure mode exactly, which BR-1 names as the
thing it exists to avoid.

It also gets three other cases right for free, because it asks the composed map
rather than reasoning about the rules that built it: with `allow_ssh_agent =
true` the variable is present and no sentence appears; with `auth_ref =
"env:SSH_AUTH_SOCK"` the credential removal wins and the sentence *does* appear,
which is true; and a future table entry needs no new plumbing.

**The residual, named rather than glossed.** Predicate 2 reads the live
environment, so the advisory's presence discloses one bit — that this machine has
an ssh-agent socket. Under REQ-596's BR-5 as amended (2026-09-01, this REQ) the
prohibition is on a *name* discovered from the live environment appearing in
output; the name here is drawn from the static table and the live read only gates
whether it is spoken. The bit is the price of BR-1 being true rather than
plausible, and it is disclosed to the model, which is already running arbitrary
commands in that environment. It is recorded here so the boundary is where it
reads, not one step wider.

### Command-position matching, and what it deliberately misses

The command text is split at the operators that begin a new command — `|`, `;`,
`&&`, `||`, `(`, newline — and the first word of each segment is compared, after
its directory prefix is stripped, against the entry's `programs`. So
`git push origin main` and `cd x && /usr/bin/ssh host` match; `echo "no ssh
here"` does not.

Two known limitations, both **false negatives**, which is the safe direction for
BR-2: a program reached through `xargs`, `env`, `sudo` or a script is not seen,
and neither is one inside a substitution. A false negative costs the user the
sentence they would have got; a false positive costs every future reader their
trust in it.

## ADR-B: The opt-in is one name on the allowlist parameter, and `compose_child_env` does not learn about it

REQ-596 BR-7.1 made the allowlist a **parameter** so the two spawn paths could
never widen each other. This REQ spends that design rather than amending it:

```rust
pub(crate) fn shell_env_allow(allow_ssh_agent: bool) -> Vec<&'static str> {
    let mut allow = SHELL_ENV_ALLOW.to_vec();
    if allow_ssh_agent {
        allow.push(SSH_AUTH_SOCK);
    }
    allow
}
```

Only the `shell` call site calls it. `mcp::client` still passes
`MCP_BASE_ENV_ALLOW`, and `compose_child_env` has no new argument, no new branch
and no knowledge that a flag exists. **AC-9 is therefore structural rather than
lucky**: there is no path by which the flag can reach the MCP composition, and
the test asserting byte-identity is confirming the shape rather than defending
it.

AC-8 falls out of the same shape. `compose_child_env`'s step 1 is a `contains`
filter over `allow`; two allowlists differing by one name produce composed maps
differing by at most that one entry. The test asserts it as a **two-way set
difference over the whole composed map** — gained and lost — because a one-way
spot check would pass a widened allowlist, which is the failure AC-8 exists to
catch.

## ADR-C: One live-config policy read, not two globals

`run_bounded` has two callers (`ShellTool::run` and the skill path's dynamic
context) and neither holds a `Config`. REQ-596 answered this once already, with
a `OnceLock` closure the daemon installs at bootstrap that reads the **live**
config on every call (LESSON-539 — not a snapshot). This REQ needs a second fact
from that same config at that same moment.

The provider is therefore **widened, not duplicated**:

```rust
pub struct ChildEnvPolicy {
    pub credential_env_names: BTreeSet<String>,
    pub allow_ssh_agent: bool,
}
// set_credential_env_names_provider -> set_child_env_policy_provider
// credential_env_names()            -> child_env_policy()
```

Two globals would make "install one and forget the other" representable, and the
forgotten one would fail silently in the safe direction — which is the worst
kind, because nothing would ever report it. One provider makes the pair
unconstructible apart, the same argument `boundary_posture` makes for reading
both boundary facts under one lock so two readings cannot disagree.

`ChildEnvPolicy::default()` — no names, `allow_ssh_agent: false` — keeps REQ-596's
uninstalled-provider argument intact in both fields: the safe value is the
default, and the daemon, the one context where a config exists, always installs.

## ADR-D: OQ-1 settled — no advisory on a successful command

**No**, and for a stronger reason than BR-2's noise argument.

Detecting "suspicious output" means pattern-matching the command's own stdout,
and this repository has already paid for treating command output as a channel:
`truncation_notice` used to carry the duty's size trigger, and REQ-561's verify
pass found that a `Makefile` or build script printing that exact line could forge
it. Command output is repository-controlled. An advisory keyed on it could be
conjured by any project the agent checks out, onto a **green** command, naming a
config key the user would then set for no reason.

The `git push`-through-a-wrapper case OQ-1 describes is real but rare, and it is
already served: the wrapper's own non-zero exit, when it has one, takes the BR-1
path. Settled no; if dogfooding produces a real instance, it wants an exit-code
fix in the wrapper, not a scanner in Teton.

## ADR-E: OQ-2 settled — config scope, not per-session

**Config scope.** REQ-596's Permissions row is deliberate that admitting a
credential-adjacent capability is the config author's decision and never the
model's, and this REQ's own Permissions row repeats it. A per-session grant is
indeed the smaller capability, but reaching it means asking someone — which is
the per-invocation consent this REQ puts Out of Scope, with the reasoning that it
earns its machinery only once dogfooding shows the agent is wanted often.

Shipping the config key first is also what *produces* that evidence: ASSUME-024
resolves by dogfooding, and a key nobody sets is itself a finding. If the answer
comes back "often", the Out-of-Scope per-invocation-consent item is where the
session-scoped grant belongs, keyed per `permission_key_for` like every other
graded capability.

## BR-6 — how the opt-in is explicit and greppable (no AC, by design)

BR-6 constrains shape, not behaviour, so it is answered here rather than tested
(the spec records why). The shape, concretely:

- **One key**, `[shell] allow_ssh_agent`, a `bool` defaulting to `false`. It does
  not accept a list, so it cannot become the `extra_env` Out of Scope refuses.
- **One name it admits**, `child_env::SSH_AUTH_SOCK`, a named constant used by
  both `shell_env_allow` and the diagnosis table, so the flag and the sentence
  cannot come to disagree about which variable is at stake.
- **Greppable end to end**: `rg allow_ssh_agent` finds the config field, the
  runtime's read, the allowlist function and the advisory's `opt_in_key` — four
  sites, no fifth.
- **Nothing infers it.** No code reads the daemon's environment, the command
  text, or a prior failure to decide whether the agent is wanted. The only input
  is the key. This is the half BUG-202/LESSON-578 settled: a secure default plus
  one named key beat a permissive default *and* beat a heuristic, and the
  heuristic is the one that looks helpful in review.

## Proposed addition to `.adlc/context/architecture.md`

Under **Key Patterns**:

> - **A withheld capability explains itself at the point it bites** — when a
>   security decision removes something a user's command needs, the resulting
>   failure names the daemon and the key that reverses it, on the failing call
>   and nowhere else. The advisory fires on what the child *actually* lacked,
>   never on what the rules say it should lack, so a machine that never had the
>   variable is not told a story about it; and it is a sentence on a tool result
>   rather than an event, because an event carrying the withheld set is a
>   disclosure surface with no actionable payload (REQ-596 OQ-1, REQ-607 BR-3).

## Test surfaces

| AC | Surface | Note |
|---|---|---|
| AC-1 | `shell.rs` unit — `ShellTool::run` over a failing `git push` in a temp repo with no remote | Asserted on `ToolOutcome::content`, the rendered result (LESSON-519). No network. |
| AC-2 | same, command `exit 1` | The benign path for the detector. |
| AC-3 | same, command `true` | |
| AC-4 | new integration binary `shell_env_advisory.rs` | `run_session_turn_with_source` + `ScriptedSseTransport` + `bus.subscribe`, draining with the `collect_events` shape `remote_loop.rs` uses. Asserts over the **serialized envelopes actually published**, not a type name. Non-vacuous: the drain must be non-empty and the tool result must carry the sentence. |
| AC-5 | same binary | `auth_ref = "env:MY_LLM_CRED_SENTINEL"`. |
| AC-6 | mutation, recorded in the test doc comment | Delete predicates 1+3 so the sentence is appended unconditionally; AC-2 and AC-3 go red. |
| AC-7, AC-8, AC-10, AC-11 | `child_env.rs` unit | AC-8 over the whole composed map, both directions. The fixture **plants** `SSH_AUTH_SOCK` in the daemon vars — with it unset the difference is zero entries and the test would be vacuous. |
| AC-9 | `mcp/client.rs` unit, beside the existing AC-4.1 test | |
| AC-12 | the rejection table's `SSH_AUTH_SOCK` row, plus a derived check that every `WITHHELD_DIAGNOSED` name appears in that doc table | Corpus cut at the first column-0 `#[cfg(test)]` (conventions.md, REQ-600). |
| AC-13 | read `REQ-596/requirement.md` | Already satisfied by the 2026-09-01 amendment; this REQ updates its **Status** bullet, which goes stale the moment the flag ships. |
