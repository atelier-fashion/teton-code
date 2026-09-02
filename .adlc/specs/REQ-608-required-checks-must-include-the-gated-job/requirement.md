---
id: REQ-608
title: "The one job that guards feature-gated code is the one job that cannot block a merge"
status: complete
deployable: false
created: 2026-09-01
updated: 2026-09-02
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

- [x] BR-1: `feature-gated targets compile (all features)` is among `main`'s
      required status checks. A PR whose `gated` job is red cannot merge.
- [x] BR-2: The relationship between "jobs `ci.yml` defines" and "contexts
      `main` requires" is **asserted from inside the repository**, so a future
      divergence fails a run instead of going unnoticed. Adding one context by
      hand fixes one instance; it does not make the next one visible.
- [x] BR-3: The assertion is **derived, not duplicated**, and the derivation rule
      is written down rather than left implicit. A hand-maintained second list of
      check names is the same drift with an extra place to forget. The rule:
  - A job's check-run context is its rendered `name:` when it declares one, and
    its **job key** when it does not.
  - A single-dimension matrix job appends ` (<value>)` to that context, one
    context per value, in declaration order. A multi-dimension matrix, an
    `include`/`exclude` entry, or a `name:` containing an expression is
    **underivable** by this rule — the check must say so and fail (next bullet)
    rather than guess at the forge's rendering.
  - Every job in `ci.yml` today declares a `name:`, and `check` is the only
    matrix and is single-dimension. So a parser that mishandles either case
    works **now** and fails silently later — which is why the rule is stated
    here, so the next reader can tell a limitation from a bug.
  - Added at verify (2026-09-02), so the three copies of this rule — here,
    ADR-608-4, and the script's header — say the same thing: a job that calls
    a reusable workflow (`uses:`) is underivable (the forge names its runs
    `<caller> / <callee job>`, one per callee job); a `name:` that is empty,
    padded, or carries a control character is underivable; a boolean or float
    matrix value is underivable; and two jobs deriving one context are
    underivable together, both named.
  - Where the derivation cannot produce a context with confidence, the check
    **says so and fails** (BR-5). It must never drop an underivable job from the
    comparison — a job silently excluded is a job silently unrequired, which is
    this REQ's own defect reappearing inside its fix.
- [x] BR-4: The check is **two-directional, and both directions fail the run.**
      `missing` (defined but not required) is today's defect. `stale` (required
      but no longer defined) is the mirror. The decision to fail on both is made
      here, not deferred to `/architect`, and rests on three measured facts:
  - **`missing` self-scopes.** Every job uses plain `actions/checkout@v4` with no
    `ref:` override, and a `pull_request` event checks out the merge ref — so the
    check reads *the PR's* `ci.yml`. A contributor who adds a job sees red on
    their own PR only; unrelated PRs still carry `main`'s `ci.yml`, still match
    protection, and stay green. Failing costs no one else anything.
  - **`stale` costs nothing to fail on.** A required context that can never
    report already blocks every PR — the forge sits at "Expected — waiting for
    status to be reported" indefinitely. The check adds the diagnosis, not the
    blocking.
  - **Scoping the check to PRs that touch `ci.yml` was considered and rejected.**
    It would be blind to divergence introduced from the *protection* side — an
    admin editing required checks — which is precisely how this defect was born:
    protection was written with six contexts and no `ci.yml` change was involved.
    A diff-scoped check would not have caught the bug this REQ exists to close.
  - The one repo-wide failure this decision *can* cause is BR-9's, named there.
- [x] BR-5: The check **fails closed**. If it cannot read the protection API —
      no token, insufficient scope, API error — it refuses and says why. It must
      never treat "could not read" as "matches". A suite that declines is
      recoverable; one that passes against nothing has spent its trust
      (informed by LESSON-510).
- [x] BR-6: The check lands with its own **known-bad in the same commit**: a
      mutation proving the run goes red when a context is removed from the
      expected set, run and its outcome recorded. A control added without one is
      an unguarded guard (informed by LESSON-464).
- [x] BR-7: Adding `gated` to the required set must not deadlock merges. A
      required context that does not report on some PRs blocks them forever, so
      the REQ confirms `gated` runs unconditionally on every `pull_request` —
      no path filter, no `if:` condition, no skip — before the protection change
      lands.
- [x] BR-8: `ci.yml`'s `gated` comment is corrected to cite **BUG-167**, the bug
      it actually describes, in place of BUG-166.
- [x] BR-9: **The one repo-wide failure this check can cause is named, not
      designed around.** If an admin removes a required context, `main`'s
      `ci.yml` and protection disagree, and — because every PR carries that
      `ci.yml` — every PR goes red until the disagreement is resolved. That is
      the correct response to someone silently weakening the merge gate, and it
      is reversible in one edit. The check's failure output must therefore name
      **both** remedies explicitly: revert the protection edit, or update
      `ci.yml` to match the intended set. A wall of red whose cause takes ten
      minutes to work out is a worse outcome than the drift it reports.
- [x] BR-10: **The protection read does not widen the workflow's blast radius.**
      `ci.yml` declares `permissions: contents: read` at workflow scope with no
      job-level override, and its own header records that the grant is narrow
      *because* this workflow executes repo-authored shell (`tools/release/selftest.sh`)
      on every PR. Whatever token the read uses, the widened permission is scoped
      to the job that performs it and no other, and no job that executes
      repo-authored shell gains it. If the mechanism chosen needs a long-lived
      PAT rather than a job-scoped `administration: read`, that is a new standing
      secret and the reason it was preferred is recorded in the architecture doc.

## Acceptance Criteria

- [x] AC-1: `main`'s required status checks include `feature-gated targets
      compile (all features)`. Evidenced by the `gh api .../branches/main/protection`
      response before and after, both recorded.
  - **Before, 2026-09-02T21:46:56Z** (`required_status_checks`, `strict: true`):
    `catalog integrity (BR-8/AC-8)`, `fmt · clippy · test (ubuntu-latest)`,
    `fmt · clippy · test (macos-latest)`, `acceptance suite (REQ-544 + REQ-547)`,
    `dependency advisories (cargo audit)`,
    `release tooling (actionlint · shellcheck · selftest)` — six.
  - **After, 2026-09-02T21:46:58Z** (one `PATCH .../protection/required_status_checks`,
    applied through the admin's own `gh` session with recorded consent — TASK-359):
    the six above plus `feature-gated targets compile (all features)` and
    `required checks mirror ci.yml (REQ-608)` — eight. `strict` unchanged
    (`true`); `enforce_admins` unchanged (`false`, OQ-4). The local parity
    check against live `main` went from exit 1 (both new contexts under
    `missing`) to exit 0 across the edit.
- [x] AC-2: **The defect is demonstrated before it is fixed, and the "before"
      half is not negotiable.** A branch whose `gated` job fails — e.g. a
      deliberate `--all-features` compile error behind `#[cfg(feature = "llama")]`
      — is shown to be **mergeable under the current protection**. That half is
      required unconditionally: it is cheap, needs no admin, involves merging
      nothing, and *is* the defect claim this REQ rests on. Asserted on the
      forge's own mergeability verdict, never on a local prediction.
  - The **"after" half** — the same branch shown not mergeable once the context
    is required — is the negotiable one. If it is judged too costly or too risky
    against `main`, say so and record exactly what was checked instead.
  - An escape hatch on the whole criterion would leave the REQ's central
    evidential claim to an unbounded judgment call. Splitting it puts the floor
    where the evidence is cheapest and the claim strongest.
  - **Before-half, measured 2026-09-02T21:38:17Z** (TASK-358). PR #272,
    head `8c51278`, cut from `main` at `467bfa5`, one file changed
    (`crates/tetond/tests/template_smoke.rs`: a `compile_error!` under
    `#![cfg(feature = "llama")]`). Run 33685887754: six checks `SUCCESS`, one
    `FAILURE` — `feature-gated targets compile (all features)`. Forge verdict,
    verbatim: GraphQL `{"mergeStateStatus":"UNSTABLE","mergeable":"MERGEABLE"}`;
    REST `{"mergeable":true,"mergeable_state":"unstable"}`. `UNSTABLE` is
    GitHub's own term for "mergeable with a failing non-required status".
    Note for the next reader: `mergeStateStatus` read `BLOCKED` on the first
    three polls while *required* checks were still pending and flipped to
    `UNSTABLE` only when the last required one landed — the verdict is
    meaningful only after every rollup entry is terminal.
  - **After-half, measured 2026-09-02T21:47:13Z**, same PR #272, same head
    `8c51278`, fifteen seconds after the protection edit: GraphQL
    `{"mergeable":"MERGEABLE","mergeStateStatus":"BLOCKED"}`; REST
    `{"mergeable":true,"mergeable_state":"blocked"}`. The one red job that
    could not block a merge now does. PR #272 was then closed unmerged and its
    branch deleted.
- [x] AC-3: A check inside the repository compares `ci.yml`'s defined job names
      against the forge's required contexts. Both directions (`missing`, `stale`)
      are computed, both are rendered, and **both fail the run** — per BR-4,
      which settles this rather than leaving it to the implementer. Each failure
      names BR-9's two remedies.
- [x] AC-4: **BR-3 guard.** The check derives job names by parsing `ci.yml`,
      matrix legs expanded. Adding a job to `ci.yml` without adding it to
      protection turns the check red with no edit to the check itself. Asserted
      by adding a throwaway job in a fixture or a scratch copy, not by reasoning
      about the parser.
- [x] AC-5: **BR-5 guard.** With the protection read made to fail — a rejected
      credential (401), an endpoint that does not answer, or a response with no
      protection object — the check exits non-zero and its output names the
      cause. It does not pass. Asserted by running it that way, not by reading
      the error branch.
- [x] AC-6: **BR-6 known-bad, run.** Deleting one context from the expected set
      turns the check red; the mutation is executed and what actually went red is
      recorded in the check's doc comment or the PR body.
- [x] AC-7: **BR-7 guard.** `gated` is confirmed to run on every `pull_request`
      event with no path filter or conditional skip, stated with the evidence,
      before AC-1's protection change is applied.
  - Evidence (quoted from `ci.yml` at `5bc3d36`, before the PATCH): the
    workflow trigger is `on: pull_request: branches: [main]` / `push: branches:
    [main]` with no `paths`/`paths-ignore` (`grep -nE '^\s*paths(-ignore)?:'`
    → nothing); the `gated` job is `gated:` / `name: feature-gated targets
    compile (all features)` / `runs-on: macos-latest` with no `if:` (`grep -n
    '^\s*if:' .github/workflows/ci.yml` → nothing, file-wide). Green on `main`
    at run 33679995402 and on PR #271's run 33687008449.
- [x] AC-8: `ci.yml`'s `gated` comment cites BUG-167. The claim it makes about
      `template_smoke.rs` is checked against BUG-167's own description rather
      than carried over.
- [x] AC-9: **BR-10 guard.** The widened permission is scoped to the job that
      performs the protection read: `ci.yml`'s workflow-level grant is still
      `contents: read`, and no job that runs repo-authored shell carries the
      wider grant. Asserted by reading the merged workflow, and stated as a
      diff of what each job's effective permissions were before and after.
  - Diff: **none, for every job.** No permission widened. The read path is the
    public branch endpoint (External Dependencies), so the `parity` job runs
    under the unchanged workflow-level `permissions: contents: read` — `grep -n
    'permissions:' .github/workflows/ci.yml` returns exactly the one
    workflow-level line (38) before and after. Effective permissions: `check`
    (both legs), `gated`, `catalog`, `e2e`, `audit`, `tooling`: `contents:
    read` → `contents: read`; `parity` (new): `contents: read`. `GITHUB_TOKEN`
    is passed to the check only to lift the anonymous rate limit.
- [x] AC-10: **BR-9 guard.** The check's failure output, for both directions,
      names both remedies — revert the protection edit, or update `ci.yml`.
      Asserted against the rendered failure text, not the source of the message
      (LESSON-519).
- [x] AC-11: `.github/workflows/*` still passes `actionlint`, and the full suite
      is green, grepped for `FAILED`.

## External Dependencies

- **Repository admin access** to edit `main`'s branch protection. This is the
  only step in the REQ that a workflow cannot perform and that the pipeline
  cannot self-serve; it needs a human with the right role.
- **A read path for `main`'s required contexts from CI.** Measured 2026-09-02
  (Step 0 of `/proceed`): on this public repository an **unauthenticated**
  `GET /repos/atelier-fashion/teton-code/branches/main` already returns
  `protection.required_status_checks.contexts` (and `enforcement_level`), while
  `/branches/main/protection` answers 401 without admin scope. So the read needs
  no `administration: read` and no PAT — the workflow's own `GITHUB_TOKEN` at
  `contents: read` is enough, and is used only to lift the anonymous rate limit
  shared by every GitHub-hosted runner IP. This narrows OQ-3 to a non-decision;
  BR-10 holds by construction. `/rules/branches/main` returned an empty list —
  classic branch protection is the sole source today.

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
  merge the moment the rule lands — check before applying, not after. Measured
  2026-09-02: run 33679995402 on `467bfa5`, all seven jobs `success`, `gated`
  included.
- The forge is GitHub and the protection API is the source of truth. If the repo
  later moves to rulesets rather than classic branch protection, the read has to
  follow; the property this REQ asserts does not change.

## Open Questions

- [x] OQ-1 (resolved, ADR-608-1: its own job, `required checks mirror ci.yml
      (REQ-608)` — a step in `tooling` would make that job's red misleading and
      its enumerating name a lie): **Where does the parity check run?** A CI step in the existing
      `tooling` job is the cheap answer and keeps network access out of the Rust
      test suite. A workspace test is more discoverable but would put a
      network-and-token dependency inside `cargo test`, which no other test has.
      `/architect` decides and records why.
- [x] OQ-2 (resolved by BR-4: the check is an equality, so every job `ci.yml`
      defines is required — an advisory-only job is not a shape this repo has,
      and adding one would be a spec change to BR-4, not a protection edit):
      **Should every job be required, or only these seven?** This REQ
      closes the gap for `gated` specifically. Whether "every job `ci.yml`
      defines is required" is the rule — making BR-2's check an equality rather
      than a subset test — is a broader policy call that affects any future job
      added as advisory-only.
- [x] OQ-3 (resolved, ADR-608-2: the workflow's own `GITHUB_TOKEN` at
      `contents: read`, rate limit only): **Which token mechanism?** Narrowed, not settled. BR-10 now rules
      the part that is a constraint rather than a choice: the widened permission
      is job-scoped, and no job running repo-authored shell gains it. What
      remains open is the mechanism — a job-scoped `administration: read` on the
      existing job, a separate minimal job that does only the read, or a PAT.
      `/architect` picks one and records why; if it picks the PAT, BR-10 requires
      the reason to be written down, because a long-lived standing secret is a
      different class of thing from a scoped grant.
      **Narrowed further 2026-09-02:** the branch endpoint exposes the required
      contexts without any admin scope (External Dependencies). The remaining
      choice for `/architect` is only *which job* performs the read.
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
