---
id: REQ-608
title: "The one job that guards feature-gated code is the one job that cannot block a merge"
status: draft
deployable: false
created: 2026-09-01
updated: 2026-09-01
component: "ci"
domain: "developer-experience"
stack: ["github-actions", "ci"]
concerns: ["reliability", "developer-experience"]
tags: ["ci", "branch-protection", "required-checks", "gated-job", "feature-flags", "config-drift", "req-605-followup", "lesson-510", "bug-167"]
---

## Description

`main`'s branch protection requires **six** status checks. `ci.yml` defines
**seven** job runs. The one that is missing is `gated`.

Measured with `gh api repos/atelier-fashion/teton-code/branches/main/protection`
on 2026-09-01, the required contexts are:

```
catalog integrity (BR-8/AC-8)
fmt · clippy · test (ubuntu-latest)
fmt · clippy · test (macos-latest)
acceptance suite (REQ-544 + REQ-547)
dependency advisories (cargo audit)
release tooling (actionlint · shellcheck · selftest)
```

`feature-gated targets compile (all features)` — job key `gated`, `runs-on:
macos-latest`, `cargo clippy --workspace --all-targets --all-features -D
warnings` — is absent. It runs on every PR, reports its result, and **cannot
block a merge**. A PR whose gated targets no longer compile is mergeable while
the red X sits next to it.

**Why that job specifically matters.** Every other job compiles the workspace
with *default* features, so a target behind `llama`, `presence`, `live` or
`test-seam` is a file CI can see but never compiles. `gated` is the only leg
where those cfg branches expand — deliberately on macOS, because `presence` is
cfg-gated to `target_os = "macos"` and an ubuntu leg with `--all-features` would
still strip the LocalAuthentication FFI and claim more than it checked.

That is not hypothetical. **BUG-167** is what the gap looks like in practice:
REQ-564 added a `SessionId` parameter to `LocalEngineSource::new` and updated
every call site the compiler could see; `crates/tetond/tests/template_smoke.rs`
is `#![cfg(feature = "llama")]`, so it sat outside the compiler's sight for that
change and every API pass since, kept passing two arguments, and stayed broken
on `main` with CI green the whole time. It surfaced only under
`--all-features`. (informed by BUG-167, LESSON-510)

**So the guard is itself unguarded** — LESSON-464's shape exactly: a control
that exists, that nothing fails when it is removed, weakened, or repositioned.
Here the weakening already happened, silently, at the moment the protection rule
was written with six contexts instead of seven. (informed by LESSON-464)

**The second half of the problem is that nothing in the repository can see
this.** Branch protection is repo *configuration* held by the forge, not a file
in the tree. No test, lint, or CI step currently reads it, so the set of
required checks can drift from the set of jobs `ci.yml` defines — in either
direction — and no commit, review, or run will say so. A future protection edit
that drops a context is exactly as invisible as this one was. Adding `gated` to
the list fixes today's instance; asserting the relationship is what stops the
next one. (informed by LESSON-459 — a gate proves only what it exercises)

**A defect found while writing this spec.** `gated`'s own comment in `ci.yml`
cites **BUG-166** as its motivating bug. BUG-166 is "a refused commit's one
rejection notice can be spent on a session nobody holds" — unrelated. The bug it
describes is **BUG-167**. The comment that explains why the job exists points at
the wrong evidence.

Discovered during REQ-605's pipeline and deliberately left unfixed there as out
of scope.

## System Model

### Entities

| Entity | Field | Type | Constraints |
|--------|-------|------|-------------|
| `RequiredCheckSet` | `contexts` | set of string | The contexts `main`'s protection requires. Read from the forge, never assumed from a file |
| `DefinedJobSet` | `names` | set of string | The check-run names `ci.yml` produces, matrix legs expanded (`check` yields one name per `os`) |
| `ParityVerdict` | `missing` | set of string | Defined but not required — the defect this REQ closes |
| `ParityVerdict` | `stale` | set of string | Required but no longer defined — a context that can never report, which blocks every merge |

### Events

| Event | Trigger | Payload |
|-------|---------|---------|
| _None._ | This REQ adds no runtime event. The verdict is a CI step's exit status and its rendered output. | |

### Permissions

| Action | Roles Allowed |
|--------|---------------|
| Edit `main`'s required status checks | Repository admin only — never a workflow, never a bot token |
| Read `main`'s protection in CI | The parity check's token; read-only, and scoped no wider than that |

## Business Rules

- [ ] BR-1: `feature-gated targets compile (all features)` is among `main`'s
      required status checks. A PR whose `gated` job is red cannot merge.
- [ ] BR-2: The relationship between "jobs `ci.yml` defines" and "contexts
      `main` requires" is **asserted from inside the repository**, so a future
      divergence fails a run instead of going unnoticed. Adding one context by
      hand fixes one instance; it does not make the next one visible.
- [ ] BR-3: The assertion is **derived, not duplicated**. It reads the job names
      out of `ci.yml` (matrix legs expanded) and compares them against what the
      forge reports. A hand-maintained second list of check names is the same
      drift with an extra place to forget.
- [ ] BR-4: The check is **two-directional**. `missing` (defined but not
      required) is today's defect. `stale` (required but no longer defined) is
      the mirror, and it is worse: a required context that can never report
      blocks every merge until an admin intervenes. Both are reported; the REQ
      decides in `/architect` whether both fail the run.
- [ ] BR-5: The check **fails closed**. If it cannot read the protection API —
      no token, insufficient scope, API error — it refuses and says why. It must
      never treat "could not read" as "matches". A suite that declines is
      recoverable; one that passes against nothing has spent its trust
      (informed by LESSON-510).
- [ ] BR-6: The check lands with its own **known-bad in the same commit**: a
      mutation proving the run goes red when a context is removed from the
      expected set, run and its outcome recorded. A control added without one is
      an unguarded guard (informed by LESSON-464).
- [ ] BR-7: Adding `gated` to the required set must not deadlock merges. A
      required context that does not report on some PRs blocks them forever, so
      the REQ confirms `gated` runs unconditionally on every `pull_request` —
      no path filter, no `if:` condition, no skip — before the protection change
      lands.
- [ ] BR-8: `ci.yml`'s `gated` comment is corrected to cite **BUG-167**, the bug
      it actually describes, in place of BUG-166.

## Acceptance Criteria

- [ ] AC-1: `main`'s required status checks include `feature-gated targets
      compile (all features)`. Evidenced by the `gh api .../branches/main/protection`
      response before and after, both recorded.
- [ ] AC-2: **The defect is demonstrated before it is fixed.** A branch whose
      `gated` job fails — e.g. a deliberate `--all-features` compile error behind
      `#[cfg(feature = "llama")]` — is shown to be mergeable under the current
      protection, and not mergeable after. Asserted on the forge's own
      mergeability verdict, not on a local prediction. If demonstrating this on a
      real PR is judged too costly or too risky against `main`, say so and record
      what was checked instead — do not tick this from reasoning alone.
- [ ] AC-3: A check inside the repository compares `ci.yml`'s defined job names
      against the forge's required contexts and fails on a mismatch. Its two
      directions (`missing`, `stale`) are both computed and both rendered.
- [ ] AC-4: **BR-3 guard.** The check derives job names by parsing `ci.yml`,
      matrix legs expanded. Adding a job to `ci.yml` without adding it to
      protection turns the check red with no edit to the check itself. Asserted
      by adding a throwaway job in a fixture or a scratch copy, not by reasoning
      about the parser.
- [ ] AC-5: **BR-5 guard.** With the protection read made to fail (no token, or
      a token without the scope), the check exits non-zero and its output names
      the cause. It does not pass. Asserted by running it that way, not by
      reading the error branch.
- [ ] AC-6: **BR-6 known-bad, run.** Deleting one context from the expected set
      turns the check red; the mutation is executed and what actually went red is
      recorded in the check's doc comment or the PR body.
- [ ] AC-7: **BR-7 guard.** `gated` is confirmed to run on every `pull_request`
      event with no path filter or conditional skip, stated with the evidence,
      before AC-1's protection change is applied.
- [ ] AC-8: `ci.yml`'s `gated` comment cites BUG-167. The claim it makes about
      `template_smoke.rs` is checked against BUG-167's own description rather
      than carried over.
- [ ] AC-9: `.github/workflows/*` still passes `actionlint`, and the full suite
      is green, grepped for `FAILED`.

## External Dependencies

- **Repository admin access** to edit `main`'s branch protection. This is the
  only step in the REQ that a workflow cannot perform and that the pipeline
  cannot self-serve; it needs a human with the right role.
- **A token that can read branch protection from CI.** `ci.yml` currently
  declares `permissions: contents: read`, which is not sufficient — reading
  protection needs `administration: read` or a PAT. Whether that widening is
  acceptable is OQ-3.

## Assumptions

- **The id was allocated by hand, not by `adlc_alloc_id` — verify before PR.**
  The allocator issued three colliding ids on 2026-08-31: teton-code's REQ-600,
  REQ-601 and REQ-602 duplicate ids already held by `atelier-fashion` (REQ-600,
  created 08-28), `infrastructure` (REQ-601, created 08-28, merged as PR #302)
  and `adlc-toolkit` (REQ-602, created 08-30) in the same global namespace.
  REQ-608 was chosen against a directly measured global high-water of 606 across
  all six repositories; 607 went to renumbering the REQ-601 collision, applied in
  the same commit as this spec. Using the allocator in its current state would
  have risked a fourth collision. The teton-code REQ-600 and REQ-602 duplicates
  are left in place deliberately — both are shipped, their ids appear in code
  comments, test names and merged commit messages, and LESSON-599 records that a
  bulk rename does not stop at code.
- `gated` is currently green on `main`. If it is not, requiring it blocks every
  merge the moment the rule lands — check before applying, not after.
- The forge is GitHub and the protection API is the source of truth. If the repo
  later moves to rulesets rather than classic branch protection, the read has to
  follow; the property this REQ asserts does not change.

## Open Questions

- [ ] OQ-1: **Where does the parity check run?** A CI step in the existing
      `tooling` job is the cheap answer and keeps network access out of the Rust
      test suite. A workspace test is more discoverable but would put a
      network-and-token dependency inside `cargo test`, which no other test has.
      `/architect` decides and records why.
- [ ] OQ-2: **Should every job be required, or only these seven?** This REQ
      closes the gap for `gated` specifically. Whether "every job `ci.yml`
      defines is required" is the rule — making BR-2's check an equality rather
      than a subset test — is a broader policy call that affects any future job
      added as advisory-only.
- [ ] OQ-3: **What token, and is widening the permission acceptable?** Reading
      protection needs more than `contents: read`. The workflow's own comment
      notes that its permissions are declared narrowly on purpose because it
      executes repo-authored shell on every PR. Widening the workflow-wide grant
      to read protection would relax that deliberately-tight boundary; a
      job-scoped widening, or a separate minimal job, may be the better shape.
- [ ] OQ-4: `main` currently has `required_pull_request_reviews: false` — no
      review approval is required to merge. That is a separate and larger
      decision than this REQ, noted here only because it was measured at the same
      time and a reader of the protection response will see it.

## Out of Scope

- **Changing which jobs run, what they do, or their matrix.** This REQ changes
  what can block a merge, not what CI checks.
- **Requiring review approvals** (OQ-4). Measured and recorded, deliberately not
  decided here.
- **Renumbering the three colliding REQ ids.** Named in Assumptions because it
  determined this REQ's own id; the fix belongs in its own work item, along with
  the allocator bug that produced them.
- **Auditing the other repositories' branch protection.** The same gap may exist
  in `infrastructure`, `atelier-fashion` and the rest. Worth knowing, not worth
  bundling — this REQ's ACs are all measured against `teton-code`.

## Retrieved Context

- LESSON-510 (lesson, score 5): a harness that checked a binary exists has not checked it is the one under test — the "existence is not freshness" shape `gated` exists to close, and the source of BR-5's fail-closed rule
- LESSON-464 (lesson, score 5): a control added during a fix pass is unguarded by default — new guards need their own known-bads in the same pass (BR-6)
- LESSON-459 (lesson, score 4): a gate proves only what it exercises — reachability is not correctness, and a green run hides the platform that is not covered
- LESSON-443 (lesson, score 3): a guard keyed on a feature's absence disables itself when the feature lands — the adjacent failure shape for predicates that pass for incidental reasons
- BUG-167 (bug, score 4): the llama-gated template smoke no longer compiles — the concrete instance of what `gated` catches and default-feature CI cannot
- BUG-166 (bug, score 1): a refused commit's one rejection notice can be spent on a session nobody holds — retrieved only because `ci.yml` mis-cites it; unrelated to this REQ's subject
- REQ-605 (spec, score 4): let every commit's CI finish — the REQ whose pipeline surfaced this gap, and the most recent prior art for changing `ci.yml` under an actionlint gate
