# REQ-604 — Architecture

## Problem restated

`req598_turn_event_order.txt` pins one plain typed turn. REQ-599 AC-6 claimed
coverage of a skill expansion and a consent prompt too; REQ-602 TASK-306
narrowed the criterion to what shipped and filed this REQ for the rest.

The constraint that shapes everything below: **the missing sequences cannot be
recorded at tip.** A golden file computed by the subject it checks is not an
oracle (LESSON-569; the existing fixture's own header says so). They must be
recorded at `17c39ec`, before the turn path was decomposed.

## What was verified before designing

Every load-bearing premise was checked against the tree, not assumed:

| Premise | Status |
|---|---|
| `17c39ec` is an ancestor of tip | Confirmed — 19 commits behind |
| `17c39ec` still carries the pre-split `crates/tetond/src/runtime.rs` | Confirmed — one file, 36,434 lines |
| It carries `carry_runtime`, `prompt`, `one_session`, `await_permission_request`, `Scripted` | Confirmed — all in `mod conversation_carry` |
| **`17c39ec` builds** (the REQ's one Assumption) | **Confirmed** — `cargo build --tests -p tetond` clean; a sample lib test runs green |
| `skill_invoked` and `permission_request` exist as events at `17c39ec` | Confirmed |
| `run_prompt_turn`'s signature is unchanged between `17c39ec` and tip | Confirmed — same 10 positional arguments |
| `req598_event_order` does **not** exist at `17c39ec` | Confirmed — the test was written on the REQ-598 branch; only the fixture was captured at the base |

That last row is why a harness has to be built rather than reused, and the
second-to-last is what makes the harness trustworthy.

## ADR-1: The capture harness is written at `17c39ec`, run there, and never committed

The harness is necessarily new code — no such test existed at that commit. That
is not a provenance violation, because provenance is a property of the
**subject**, not of the observer. What the harness observes is the pre-split
`runtime.rs`; what ships on this branch is the *recorded output* plus a replay
test at tip.

It runs in a detached worktree outside the repo (`git worktree add --detach`
into a scratch directory), so nothing about the capture reaches the branch.

## ADR-2: Both scenarios are driven through `run_prompt_turn`'s unchanged entry point

`DaemonRuntime::run_prompt_turn` takes the same ten positional arguments at
`17c39ec` and at tip. REQ-598's `TurnContext` and REQ-600's stage split were
both internal.

This is the single most important fact for the fixture's validity: **the driver
code is identical at both commits.** A sequence difference therefore cannot be
an artifact of my having driven the turn differently on the two sides — which
would be a subtler version of the oracle problem, and the failure mode a
"capture harness" invites.

## ADR-3: Scenario selection, and why each is the shape it is

**Skill scenario — a *user-authored* skill, not a project skill.** REQ-589
ADR-10 gates *project* skills behind a trust acknowledgment, so a project skill
would raise a `permission_request` and the skill fixture would silently also
pin the consent path. Then a change to consent handling would move *both*
fixtures, and neither would tell you which path broke. A user-authored skill in
`~/.claude/skills/<name>` raises no trust question (pinned at `17c39ec` by
`a_user_authored_skill_raises_no_trust_question`), so the two fixtures stay
independent.

**Consent scenario — a scripted `shell` tool call, answered `allow_once`.**
This is the exact shape of the existing
`a_move_during_an_in_flight_turn_is_refused_and_succeeds_after_it` fixture, so
it is already known to reach the gate and dispatch through it. The turn is
spawned, the harness waits for the `shell` permission request, resolves it, and
joins — no wall-clock sleeps (LESSON-450).

## ADR-4: Detached events are excluded by discriminator, never by position (AC-4)

The REQ-598 normalizer's two rules carry over unchanged:

- `session_titled` — removed by name; published from the detached naming task.
- the title duty's `route_decided` — removed by `category == Category::Title`,
  a field the event carries. Not "the first of the two", which is the position
  the race moves.
- consecutive `session_update` — collapsed, because it is emitted per streamed
  chunk and pinning the count would pin the scripted engine's chunking.

**Any new detached event these scenarios introduce is identified the same way.**
Which events those are is decided *empirically*, not by reading the code: the
harness runs each scenario repeatedly at `17c39ec` and any entry whose position
is not stable across runs is a detached event and must earn a discriminator.
Reasoning about which `tokio::spawn` publishes what is exactly what LESSON-591
records getting wrong — the earlier fix claimed both `route_decided` entries
were published synchronously, and was wrong.

## ADR-5: Non-vacuity is per-scenario, keyed on the event the fixture exists for (AC-5)

The existing test's model is "exactly ONE route decision survives". Each new
fixture gets the same treatment plus the event that is its whole reason to
exist:

- skill fixture — a positive count of `skill_invoked`;
- consent fixture — a positive count of `permission_request`;
- both — exactly one non-title `route_decided`, and a non-empty expected sequence.

A filter that ate everything cannot pass any of these.

## ADR-6: The transposition guard is per scenario (AC-6)

One test per fixture that normalizes the recorded sequence and a copy with two
**adjacent distinct** entries swapped, asserting the two differ. This is what
stops the normalizer being widened into an excuse: every exclusion rule added
under ADR-4 has to leave this red.

## ADR-7: If a sequence does not replay — the AC-3 protocol, decided in advance

Deciding this *after* seeing a red test is how motivated reasoning gets in, so
the rule is fixed now:

1. **Regression** — the ordering was load-bearing and something moved it. Fix
   the code; the fixture stands as captured.
2. **Intended change** — a REQ between `17c39ec` and tip deliberately changed
   it. This requires *naming the REQ and the criterion that authorised it*,
   recording the delta in the fixture header beside its provenance, and pinning
   the new sequence as *captured sequence plus stated delta*. The nineteen
   candidate commits are enumerable, and all four refactors in the range
   (REQ-598, 599, 600, 602) claim to be behaviour-preserving — so an "intended
   change" would have to contradict one of those claims, in writing.
3. **Default: regression.** An unexplained delta is not evidence of intent.

No third option. Regenerating at tip is not on this list.

## ADR-8: Fixture header wording — *runtime* `TurnContext`

The existing fixture's header says it was captured "before any TurnContext
existed". That is true of the runtime type — `git show 17c39ec` finds no
`struct TurnContext` in `runtime.rs` — but the name was already taken at that
commit at the **protocol** level: `ContentClass::TurnContext`, a variant in
`crates/teton-protocol/src/methods.rs` that serializes as `"turn_context"` and
renders as "the whole turn". So the unqualified claim is not quite true. The
new headers say *runtime* `TurnContext`, which is exactly true.
The existing fixture is left alone (Out of Scope, and REQ-606 AC-4 depends on
it replaying unregenerated).

## Placement, and the concurrency constraint

The new test module goes in `crates/tetond/src/runtime/mod.rs`, nested inside
`mod conversation_carry` alongside `mod req598_event_order`. It has to be
in-crate: `session_gates` is private, and the skill scenario installs a gate.

Two concurrent REQs touch the same file. **REQ-603** relocates a
session-lifecycle slice out of `runtime/mod.rs` and merges first — a rebase is
expected. **REQ-606** collapses turn-path parameter bundles and merges after;
its AC-4 requires the REQ-598 fixture to replay unregenerated, which this REQ
does not touch. The new module duplicates the small amount of skill scaffolding
it needs (`user_home`, `install_gate`) rather than reaching into the private
sibling module that holds them, which keeps it robust against either
relocation.

## Task graph

```
TASK-001 (capture harness at 17c39ec, both sequences + stability runs)
    |
TASK-002 (fixture files with provenance headers)
    |
TASK-003 (replay test module at tip: normalizer, non-vacuity, transposition)
    |
TASK-004 (replay; AC-3 disposition if red)
    |
TASK-005 (full verification: suite / clippy / fmt)
```

Strictly sequential — each task's input is the previous one's output. There is
no parallelism to exploit here and claiming some would be false.
