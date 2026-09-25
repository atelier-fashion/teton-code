---
id: BUG-227
title: "A typed skill refused at a mid-turn reroute says no provider saw a turn one already answered"
status: resolved
severity: medium
created: 2026-09-25
updated: 2026-09-25
resolved: 2026-09-25
component: "daemon/harness"
domain: "harness"
stack: ["rust", "daemon"]
concerns: ["developer-experience", "reliability"]
tags: ["skills", "reroute", "refit", "budget", "privacy-pin", "refusal-wording"]
introduced_by: ["REQ-587"]
attribution: derived
---

## Description

When the reroute guard (`skill_would_not_survive_refit`) refuses a typed
`/name` expansion, it ends the turn with `-32023` and the sentence
BR-8 composes for a typed skill: *"Nothing was sent and no provider saw this
turn — a skill expansion is carried whole or refused…"*

That ending belongs to the two pre-dispatch stages (`server.rs` and the seed
check in `resolve_route`), where it is true. The reroute guard runs **inside
the attempt loop**, after `run_session_turn_with_source` has returned. By then
the earlier route may already have served several model calls. In the reported
session it had served two. The sentence then tells the user something false,
on the one surface meant to say exactly what happened.

## Reproduction Steps

Observed 2026-09-25 in `sess-9vzd1rrt83hysatbr473chkwbg` (teton-code checkout):

1. Type `/analyze`. The route classifier sends the turn to kimi-k3 (think tier,
   666k-token budget), and the expansion (about 58 KB with dynamic context) fits.
2. Kimi runs two rounds (two `cost_recorded` records, ~$0.11) and calls
   `shell` with the step-1.5 delegation-gate command, which is full of quotes
   and `$`.
3. The shell classifier answers `unknown_shell`. The session pins to the local
   tier (21,162-token budget), and the next egress inspection raises
   `PrivacyBlocked`, which reroutes the turn to local.
4. The privacy-reroute arm calls `skill_would_not_survive_refit`. The committed
   `/analyze` block takes more than 25% of the local budget (REQ-618 BR-4), so
   the typed arm ends the turn.

## Expected Behavior

The refusal says what is true: the turn had already started on another route,
it ends here, nothing more is sent, and nothing from the turn is kept.

## Actual Behavior

```
error: prompt failed: `/analyze` fits this route's context budget but would leave the turn no room to work: … Nothing was sent and no provider saw this turn — a skill expansion is carried whole or refused, never shortened into something you did not invoke.
```

Kimi had already seen the turn twice.

## Environment

- Platform: macOS (Darwin 27.0.0)
- Version: teton v0.1.36 (main @ eef23eb)

## Root Cause

`SkillCaller` has two endings, and `skill_refusal` picks one by **caller**
alone (`SkillCaller::consequence`, `harness/budget.rs`). The `User` ending
("Nothing was sent and no provider saw this turn") was written for the stages
that run before `CarriedTurn::begin`. REQ-587 made the caller a parameter so
the reroute guard could name a *model* invocation correctly (BUG-188 closed
the model arm), but a *typed* expansion caught at the reroute still gets the
pre-dispatch ending.

The claim is not guaranteed by either reroute arm in `runtime/turn.rs`:

- **Privacy reroute**: the block fires at the egress inspection of request
  *N*. Nothing was sent only when *N* = 1. In the reported session *N* = 3.
- **Provider-failure fallback**: the failed provider normally received the
  request whose failure triggered the fallback.

The `-32023` protocol doc (`teton-protocol/src/jsonrpc.rs`) and the
`SkillFit::TooLarge` doc make the same "nothing was dispatched" claim for every
`-32023`.

Refusing the turn is correct. Only the wording is wrong.

## Resolution

The reroute guard now composes through its own entry point, `skill_refit`,
which is the same measurement as `skill_fit` closed with a new tail,
`SkillSentence::RefusedAtReroute` → `SkillCaller::reroute_consequence`. The
user arm says what is true on both reroute arms:

> This turn was already under way when it moved to this route, so it ends
> here: nothing more is sent and nothing from this turn is kept — a skill
> expansion is carried whole or refused, never shortened into something you
> did not invoke.

The model arm is byte-identical to before. The pre-dispatch sentence is
unchanged (REQ-589 AC-3). The code stays `-32023`: the refusal is still
Teton's and not a provider's. Its protocol doc now names the reroute as the
one exception to "nothing was dispatched".

`verdict` takes the refusal tail as a parameter. To stay inside clippy's
argument limit without a new suppression (the `suppression_ratchet` test
caught the first attempt), the estimator pair and body size travel as one
private `Candidate` bundle.

The existing test
`a_reroute_after_a_typed_expansion_still_names_it_as_the_slash_command_the_user_typed`
asserted the false clause, even though its own non-vacuity check proves the
provider received the expansion. It now asserts the reroute tail.

Mutations (2026-09-25, re-run after the `Candidate` refactor): pointing the
guard back at `skill_fit` reddens the typed integration test (1 of the
reroute pair). Collapsing `reroute_consequence`'s user arm onto
`consequence` reddens that test and the new unit test. The model sibling
stays green both times.

Tests: `cargo test --workspace --no-fail-fast` passes 4,750 tests with 0
failures. Clippy (`--workspace --all-targets`) and `cargo fmt --check` are clean.

Not fixed here: the step-1.5 delegation-gate command in the ADLC toolkit's
`/analyze` still pins the session (`unknown_shell`) and exceeds the 30 s shell
default. That belongs in the toolkit, not Teton.

## Files Changed

- `crates/tetond/src/harness/budget.rs`: `SkillCaller::reroute_consequence`,
  `SkillSentence::RefusedAtReroute`, `skill_refit`, the `Candidate` bundle
  for `verdict`, and the unit test
  `a_typed_refusal_at_a_reroute_does_not_say_no_provider_saw_the_turn`.
- `crates/tetond/src/runtime/mod.rs`: `skill_would_not_survive_refit` calls
  `skill_refit`.
- `crates/teton-protocol/src/jsonrpc.rs`: the `-32023` doc names the reroute
  exception.
- `crates/tetond/tests/skill_turn.rs`: the typed-reroute test asserts the
  truthful tail and records its mutations.

## Deployment

Merged to `main` as `fe497b6` via
[#330](https://github.com/atelier-fashion/teton-code/pull/330) on 2026-09-25,
with all 8 CI checks green on the head commit. There is no deploy target: the
fix ships in the next tagged release. Lesson: LESSON-661.
