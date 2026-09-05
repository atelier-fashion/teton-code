---
id: BUG-214
title: "A typed `/skill` on a boundary-configured machine pins the session permanently and silently — the prompt-turn egress never gets the tainting sink, so every turn-path pin is recorded as `boundary_hit`"
status: resolved
severity: high
created: 2026-09-05
updated: 2026-09-05
component: "daemon/privacy"
domain: "devtools"
stack: ["rust", "daemon"]
concerns: ["privacy", "routing", "developer-experience"]
tags: ["taint", "session-pinned", "unknown-provenance", "skills", "preamble", "req-614", "req-585", "req-587"]
introduced_by: ["REQ-614"]
attribution: none
---

## Observed

Session `sess-sphx3g1a2axx3qg739zmy2wx60`, 2026-09-05T13:29Z, daemon 0.1.31,
transcription on, root `~/GitHub/teton-code` (kind `project`), the 13 builtin
boundaries applied, `build`/`think` bound to `kimi`.

| n | record | fact |
|---|--------|------|
| 7 | `route_decided` | turn 2 ("I want to work on Teton Code") → `kimi`, served, one cost row |
| 225 | `prompt_submitted` | `/analyze the code base` — a **user** skill at `~/.claude/skills/analyze/SKILL.md` |
| 227 | `skill_invoked` | three `!cmd` preambles ran: `sh .adlc/partials/ethos-include.sh …`, `cat .adlc/context/architecture.md …`, `cat .adlc/context/conventions.md …` |
| 228 | `route_decided` | → `kimi` |
| 229 | `privacy_block` | 6 ms later: path `<unknown-provenance>`, `rerouted_to_local`, provider `kimi` |

`tetond.log` line 1063: `pinned to the local tier for the rest of its life
(boundary_hit); remote providers will not be used in it again.`

The transcript ends at n=229. There is **no `session_pinned` record**, no local
`cost_recorded`, and no further `session_update`. No `tool_call_input` ever
occurred — no `shell` tool ran in this session.

## Three causes, stacked

### 1. The expansion is unknown-provenance twice over

- **User skill.** `turn.rs` `SkillTurn` construction: `skills::provenance_of`
  returns `None` for a file outside the session root, so `unknown = true`
  (REQ-587 ADR-9's decided gap; test
  `a_user_skill_outside_the_root_pins_the_turn_wherever_any_boundary_exists`).
  Since REQ-597 made the 13 builtin globs always-on, this means **every skill
  under `~/.claude/skills` pins every repo-rooted session on every machine**,
  whether or not it has preambles. That is stricter than a `read` of the same
  bytes, by design — but the design predates always-on boundaries.
- **Preambles.** `turn.rs` `skill.unknown |= outcomes.iter().any(DynamicOutcome::spawned)`
  (REQ-585 BR-7) — parity with the *pre-REQ-614* `shell` tool, where every
  spawned command was `Unknown`. REQ-614 gave `ShellTool::run` a classifier
  (`shell_provenance::classify`) that proves `cat .adlc/context/architecture.md`
  is `Rooted`; the skill preamble path was not updated to use it.

Either alone is enough to block the first remote send.

### 2. The prompt-turn egress has no tainting sink, so the backstop is the only marker — and it hard-codes `boundary_hit`

`crates/tetond/src/runtime/turn.rs` `run_one_attempt` builds the remote choke point as

```rust
let mut egress = Egress::new(transport, boundaries, events.clone())
```

`events` is the plain `EventBus`, whose `PrivacyEventSink` impl only publishes
`privacy_block`. Only the **duty** path (`runtime/duty.rs`) wraps the bus in
`TaintingPrivacySink::for_turn_path(..).with_local_budget(..)` — the one place
that reads the block's path into a `TaintCause` (`cause_of`) and publishes
`session_pinned`.

So on a prompt turn the block error propagates to the arm in `run_prompt_turn`
that the REQ-614 comment calls a *backstop*:

```rust
// The egress sink has already marked this session with the cause read off the
// block's path … So this value is only reachable if that ordering ever stopped holding
let backstop = match detail {
    BlockDetail::Redaction => TaintCause::RedactionFinding,
    BlockDetail::Boundary | BlockDetail::ScanUnavailable => TaintCause::BoundaryHit,
};
```

The ordering never held on this path. Consequences:

- The pin is recorded as **`boundary_hit`** — permanent. `/shell allow` answers
  `was_pinned: true, lifted_now: false, cause: boundary_hit`.
- **No `session_pinned` event** reaches the client (only `TaintingPrivacySink`
  publishes it). REQ-614 BR-7's standing pin line — the fix for the
  2026-09-04 "65 pinned turns and nobody the wiser" session — does not fire on
  the path that produced that session.
- Very likely (not run): the `shell` tool case REQ-614 was built for takes the
  same arm. A `curl` result enters context mid-turn, the loop's next remote
  send is blocked, the error reaches the same backstop → `boundary_hit`, not
  `unknown_shell`. `carry.rs`'s commit-time `context_taint_cause` would record
  `unknown_shell`, but `mark` keeps the first cause and the backstop runs first.

### 3. The tests that would have caught it are named but not present

TASK-397 (`status: complete`) lists
`provenance_egress.rs::curl_pins_liftably_and_shell_allow_restores_routing`
(AC-3), `e2e/shell_pin_shape.rs::pinned_session_records_the_two_events_in_order`
(AC-11) and `…::the_2026_09_04_cost_shape_cannot_recur` (AC-12). None of those
symbols or files exist in the tree; no test outside `taint.rs` unit tests
mentions `unknown_shell`, and the only test touching `session_pinned` is an
abort-path test. The existing skill-pin tests drive `Egress::send` with a
`CapturingSink` directly, so they never see which cause the daemon records.

## What the user saw

The CLI's block line — `privacy: <unknown-provenance> would have reached kimi —
call re-routed to the local tier` — with no pin notice, and then nothing usable.
The reroute produced no local cost row, which is consistent with the expansion
(25 KB body + ~21 KB of preamble output) being refused at the refit to the local
tier's 21 162-token budget (`SKILL_EXPANSION_TOO_LARGE`); the transcript carries
no error record so this last step is inferred, not observed.

## Proposed fix

**A. Wire the tainting sink into the prompt turn** (the defect; small, no spec
change). In `run_one_attempt`, build the sink exactly as `duty.rs` does:

```rust
let sink = Arc::new(
    TaintingPrivacySink::for_turn_path(events.clone(), Arc::clone(&self.session_taint))
        .with_local_budget(Some(router.budget_for(None).budget_tokens as u64)),
);
let mut egress = Egress::new(transport, boundaries, sink) …
```

The backstop arm then really is a backstop. Add the tests TASK-397 promised,
driven through the daemon's prompt path, asserting: (i) a first-send block on
`<unknown-provenance>` records `unknown_shell`, (ii) one `session_pinned`
precedes the first pinned `route_decided` in the transcript, (iii) `/shell allow`
lifts it and the next prompt leaves the machine. A source-region check that
every `Egress::new` on a session path takes a `TaintingPrivacySink` (the
`/provider test` and lookup sites are the documented exceptions) closes the
class.

**B. Classify preambles with REQ-614's classifier** (turn.rs, replace the
`spawned` OR). Before `run_all`, call `shell_provenance::classify(root,
root_kind, boundaries, denied_prefixes, command)` per command — verdict before
spawn, as BR-10 requires — and fold: `Rooted` sources merge into
`skill.sources`; `BoundaryTouch` → its sources or the boundary-touch sentinel;
`Unknown` → `unknown = true`. `cat .adlc/context/architecture.md 2>/dev/null ||
echo "…"` becomes `Rooted`; `sh .adlc/partials/ethos-include.sh` stays `Unknown`
(opaque verb), so `/analyze` would still pin **liftably** until that partial is
rewritten as a `cat`. The `spawned`-with-no-output side channel REQ-585 closed
stays closed: a non-`Rooted` verdict pins regardless of output.

**C. Give user skills an identity** (spec-level; the biggest practical win).
Today every `~/.claude/skills` skill pins every session. Options: mint a
`ProvenanceId` under a second scope (the user skills home) so the body is
compared against the boundary globs like any read — none of the 13 builtins
match `*/SKILL.md`, so the skill would route remotely as a project skill does;
or treat a user-skill body as `unknown_shell`-class (liftable) rather than
`boundary_hit`-class. Recommend a REQ; it reopens REQ-587 ADR-9.

Order: A first (it is what makes every later pin visible and liftable), then B,
then C. Do not treat B or C as a substitute for A — without A a `Rooted`
preamble and a minted user skill would still fall to the backstop the moment
anything else in the turn is blocked.

## Workaround

None in-session: the cause is recorded as permanent. Start a new session. A
project-local copy of the skill under `.claude/skills` avoids cause 1a but not
1b; a `/model`-pinned local session avoids the block entirely at the local
tier's budget.

## Fix A landed (2026-09-05)

`run_one_attempt` now builds its choke point on
`TaintingPrivacySink::for_turn_path(..).with_local_budget(..)`, exactly as
`duty.rs` does. Pinned by:

- `runtime/taint.rs::shell_pin::the_prompt_turn_egress_installs_the_tainting_sink`
  — a source scan over `turn.rs` and `duty.rs`: no session-scoped
  `Egress::new` takes the bare bus, both read the local budget through
  `Router::budget_for(None)`.
- `tests/e2e/shell_pin_shape.rs` — three tests against the real daemon:
  an opaque `shell` result pins with `unknown_shell`, liftable, remedy
  `/shell allow`, announced once and ahead of the pinned local route, lifted
  once by `shell/override`; a `Rooted` result pins nothing and its next send
  leaves (the control); a typed **user** skill — this bug's exact shape —
  pins on its first send with the liftable cause and is announced.

Mutation run: reverting the sink line turns the source scan and both claim
tests red (no `session_pinned` arrives; `shell/override` answers
`boundary_hit`) and leaves the control green.

The probe that was meant to become AC-3's routing half found that the lift
does not reach the egress inspection — the prompt after `/shell allow` is
routed remote, blocked again, and served local. Filed and fixed as BUG-215 on
the same branch. Causes 1a and 1b above (fixes B and C) are unchanged and
still open.

## Deployment

- Merged to `main` as `34d4fde` via [PR #300](https://github.com/atelier-fashion/teton-code/pull/300) on 2026-09-05.
- Staging / production: n/a — this repo ships through PR-gated CI on `main` and the release runbook; no deploy pipeline.
