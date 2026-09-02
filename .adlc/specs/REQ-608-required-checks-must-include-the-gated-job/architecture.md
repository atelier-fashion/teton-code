# REQ-608 — Architecture

## Approach

Two changes, one measured admin edit.

1. **A parity check inside the repository** — `tools/ci/required-checks-parity.py`,
   run by a new `ci.yml` job. It derives the set of check-run contexts `ci.yml`
   produces (BR-3), reads the set `main`'s protection requires from the forge,
   computes `missing` and `stale`, renders both, and fails the run on either
   (BR-4). It fails closed on any read it cannot complete (BR-5), and every
   failure names BR-9's two remedies. Its unit tests run in the same job, before
   the live read, so the known-bads (BR-6) execute on every PR rather than once.
2. **The `gated` comment cites BUG-167** (BR-8) — a one-line edit in the same
   `ci.yml` diff.
3. **`main`'s required contexts gain two entries** — `feature-gated targets
   compile (all features)` and the new parity job's own context. That is a
   repository-admin action outside the tree (spec: Permissions). The pipeline
   prepares the exact command and the before/after evidence; a human applies it.

Everything runs with the workflow's existing `permissions: contents: read`. The
Step-0 measurement (spec, External Dependencies) showed the branch endpoint
already exposes the required contexts on this public repo with no admin scope,
so BR-10 holds by construction: no job's permissions change.

## Data model / API / service layer

No runtime change. No Firestore, no endpoints, no daemon code. The only new
"API" is a script's exit status and rendered output, consumed by a CI job.

## Key decisions

### ADR-608-1: The check is its own job, not a step in `tooling` (resolves OQ-1)

**Decision**: a new job `parity`, `name: required checks mirror ci.yml
(REQ-608)`, `runs-on: ubuntu-latest`, no `if:`, no path filter. It runs the unit
tests, then the live check.

**Rationale**: OQ-1 named a step in `tooling` as the cheap answer. Two facts
rule it out.

- A job's `name:` *is* its required-check context. `tooling`'s name enumerates
  its contents (`actionlint · shellcheck · selftest`); adding a parity step
  either makes the name a lie or forces a rename — and a rename is a coordinated
  protection edit, exactly the class of change this REQ exists to make visible.
- BR-9 requires that a repo-wide red be diagnosable in seconds. A red on
  "release tooling" caused by an admin editing branch protection is the
  ten-minute mystery BR-9 forbids. A red on "required checks mirror ci.yml" is
  self-describing.

The cost is one more ubuntu runner spin-up (well under a minute; no cargo, one
pip install, one API call) and one more context in protection — which BR-4's
equality already demands for any job.

**Consequence — the job guards itself.** If an admin un-requires the parity
context, the check reports `missing: required checks mirror ci.yml (REQ-608)`
on every PR. It cannot *block* at that point (it is no longer required), but it
is loud where the original defect was silent. That is the LESSON-464 shape
closed as far as a check can close it from inside the tree.

### ADR-608-2: Read path is the public branch endpoint with the workflow's own token (closes OQ-3)

**Decision**: `GET /repos/{owner}/{repo}/branches/{branch}` with
`Authorization: Bearer $GITHUB_TOKEN`, `X-GitHub-Api-Version: 2022-11-28`. The
required set is `protection.required_status_checks.contexts`. `owner/repo` comes
from `GITHUB_REPOSITORY`; the branch defaults to `main`. No `administration:
read`, no PAT, no job-level permission override.

**Rationale**: measured at Step 0 — the endpoint answers unauthenticated on a
public repository; `/branches/main/protection` answers 401. The token is used
only to lift the anonymous 60/hour rate limit, which is shared across every
GitHub-hosted runner behind the same egress IP and would make an anonymous read
flake. `GITHUB_TOKEN` is already present in every job; nothing widens.

**Fail-closed contract (BR-5)**, in order:

| Condition | Exit | Message names |
|---|---|---|
| `GITHUB_REPOSITORY` unset and no `--repo` | 75 | the missing input |
| non-2xx, transport error, timeout, non-JSON body | 75 | status/exception class and the URL |
| `protected` absent or `false` | 75 | "branch is not protected — nothing to compare against" |
| `protection.required_status_checks` absent | 75 | the missing key |
| `GET /repos/{o}/{r}/rules/branches/{b}` non-empty | 75 | "rulesets present; this check reads classic protection only — extend it" |
| `ci.yml` unparseable / job underivable (ADR-608-4) | 75 | the file, or the job key and why |
| `missing` or `stale` non-empty | 1 | both sets and both remedies (BR-9) |
| parity | 0 | both sets |

Rulesets are **detected, not parsed** (row 5). The spec's assumption says the
read has to follow if the repo ever moves to rulesets; writing that parser now
against an endpoint that returns `[]` here would be a fixture written from
imagination (LESSON-460). Detecting non-emptiness is trivially verified
(`[]` was observed) and keeps the check honest until a real ruleset exists to
test against. If the rulesets read itself fails, that is also 75 — a
could-not-check must never be downgraded to "no rulesets".

### ADR-608-3: Exit taxonomy 0 / 1 / 75, with a top-level handler (LESSON-442)

**Decision**: `0` parity, `1` drift (a real disagreement), `75` could not
check (nothing learned). A top-level `except Exception` routes anything
unforeseen to 75 with the exception class in the message. The CI step branches
on the code and renders `::notice::` / `::error::` with a title, in the shape
the `catalog` and `tooling` jobs already use.

**Rationale**: Python's default for an uncaught exception is 1, which would be
read as drift. LESSON-442 is the same bug in `refresh-catalog.py`. `75` follows
`tools/release/`'s `EXIT_UNCHECKED` and the two existing jobs, so a reader who
knows one taxonomy knows this one. All three non-zero paths fail the job; the
code only changes what the log says happened.

### ADR-608-4: Derivation rule is exact for what exists and refuses the rest (BR-3)

**Decision**, applied per job under `jobs:` in declaration order:

- context = `name` when it is a string containing no `${{`; otherwise the job
  key. A `name` containing `${{` is underivable (its rendering depends on
  runtime context) → 75 naming the job.
- `strategy.matrix` absent → one context.
- `strategy.matrix` a mapping with **exactly one** key whose value is a list of
  scalars, and no `include`/`exclude` → one context per value, `f"{ctx} ({v})"`,
  in list order.
- any other matrix shape (two or more dimensions, `include`/`exclude`, an
  expression string) → underivable → 75 naming the job and the shape.
- a job carrying `if:`, or a workflow whose `on.pull_request` carries
  `paths`/`paths-ignore`, is rendered as a `::warning::` (BR-7's hazard — a
  required context that may not report) but does not by itself change the exit
  code; the policy of "every job is required" (BR-4) already makes such a job a
  merge deadlock that a human will see on the first PR it skips.

**Rationale**: the spec's BR-3 states the single-dimension rule and routes
everything else to "say so and fail". A parser that guesses GitHub's rendering
for a shape the repo does not use would work today and drift silently. Every
underivable job is *named* so the reader can tell a limitation from a bug.

### ADR-608-5: PyYAML, pinned, imported lazily, missing → 75

**Decision**: the job runs
`python3 -m pip install --user 'PyYAML==6.0.2'` guarded by
`python3 -c 'import yaml'`, and the script imports `yaml` inside `main()` so a
missing module is reported as 75 with the remedy, never as a traceback exit 1.

**Rationale**: `ci.yml` uses anchors-free plain YAML today, but BR-3 says the
derivation must be a real parse — a hand-rolled subset reader is the "works now,
fails silently later" shape BR-3 names. The runner image ships `python3` (the
`catalog` job already relies on it) but nothing in this repo has assumed PyYAML;
the guard makes the dependency explicit and the pin makes it reproducible. A
pip fetch failure fails the job — a gate that waives itself on a transient error
is not a gate (LESSON-447, same rule as the actionlint download).

The script is **Python 3.9-compatible** (no `match`, no `X | Y` unions at
runtime): the dogfood machine runs 3.9, and the tests must run locally.

### ADR-608-6: The live read is a seam; tests never touch the network

**Decision**: `compare(defined, required)`, `derive_contexts(workflow_dict)`,
and `read_required(fetch, owner_repo, branch)` are pure or take an injected
`fetch(url, token) -> (status, body)` callable. `main()` wires the real
`urllib` fetcher. Unit tests (`unittest`, stdlib) inject fakes and assert on
**rendered output** and exit codes, not on internal structures.

**Known-bads run and recorded (BR-6, AC-4, AC-5, AC-6, AC-10)**:

| Mutation | Expected |
|---|---|
| a throwaway job added to a fixture copy of `ci.yml` | exit 1, `missing:` names it, both remedies present |
| one context deleted from the fake required set | exit 1, `missing:` names it |
| one context added to the fake required set | exit 1, `stale:` names it |
| fake fetch returns 401 | exit 75, output contains `401` and the URL |
| fake fetch raises `OSError` | exit 75, output names the class |
| `protected: false` | exit 75, "not protected" |
| rulesets fetch returns a non-empty list | exit 75, "rulesets" |
| two-dimension matrix | exit 75, names the job |
| the real `ci.yml` against a fake required set equal to its derived contexts | exit 0 (**benign path**) |

The job asserts the unit run executed a non-zero number of cases (grep of
`Ran N tests`, N ≥ 1) — a green suite that ran nothing is the vacuous-run
blocker `/validate` names.

### ADR-608-7: AC-2's forge verdict is measured on a throwaway PR, in both halves

**Decision**: a branch `chore/REQ-608-ac2-evidence` carrying a deliberate
`compile_error!` inside `#![cfg(feature = "llama")]`
(`crates/tetond/tests/template_smoke.rs`), opened as a **non-draft** PR titled
so nobody merges it. Evidence is `gh pr view --json mergeable,mergeStateStatus`
after CI settles: **before** the protection edit the expected verdict is
`MERGEABLE` / `UNSTABLE` (a non-required check failing); **after**, `BLOCKED`.
Both are recorded verbatim in the REQ's AC list and PR #271's body. The PR is
closed and the branch deleted afterwards.

**Rationale**: a draft PR reports `DRAFT` and hides the verdict. `UNSTABLE` is
GitHub's own word for "mergeable with a failing non-required status" — it is
the defect stated by the forge, not a local prediction. The "after" half costs
one more poll on the same PR once the admin edit lands, so it is not the
expensive step the spec allowed skipping; it is done.

### ADR-608-8: Ordering of the admin edit (BR-7, and the one repo-wide red)

The protection edit adds two contexts and must land **after** this PR's
`ci.yml` is final and **before** it merges:

1. `gated` is confirmed unconditional (AC-7): no `if:`, no `paths`, top-level
   `on.pull_request.branches: [main]` — quoted from `ci.yml`. Green on `main`
   at run 33679995402.
2. AC-2 "before" verdict captured on the throwaway PR.
3. Admin adds both contexts (`PATCH .../protection/required_status_checks` with
   the full eight-context list; recorded before/after).
4. PR #271's parity job — which already defines both — is green and the PR is
   mergeable. Any *other* PR opened in the window between (3) and the merge
   waits on a parity context that its `ci.yml` does not define; that window is
   minutes and the manifest shows no other in-flight work.
5. AC-2 "after" verdict captured; throwaway PR closed.

**Runbook for the next job add or rename** (goes to `conventions.md`): open the
PR (its own parity job goes red — `missing` — and only it); ask an admin to add
the new context (and, for a rename, keep the old one until the merge); merge;
then remove the old context. The red between the admin's second edit and the
merge is bounded to the PR itself; the red after a rename's merge until the old
context is removed is every PR (`stale`), and the message says which edit
resolves it.

## Proposed additions to `.adlc/context/architecture.md`

Under **Key Patterns**:

> **A merge gate is asserted from inside the tree, in both directions, and the
> assertion reads the forge rather than a copy** (REQ-608). Branch protection is
> configuration the repository cannot see, so the set of required checks drifts
> from the set of jobs silently — in *either* direction, since a required
> context nobody defines blocks every merge while a defined job nobody requires
> blocks none. The check derives the job set by parsing the workflow (single
> rule, stated; anything it cannot derive fails by name), reads the required set
> from the forge with the workflow's own token, and fails on `missing` and
> `stale` alike, naming the two remedies. It is its own job so that its red is
> self-describing, and so that un-requiring it is itself reported. The `gated`
> job it exists to protect was green on every PR for months while unable to
> block one (BUG-167, LESSON-464).

## Lessons applied

- LESSON-442 — exit-code collision: top-level handler, 75 for the unforeseen.
- LESSON-464 — a new guard needs its own known-bad in the same commit; the job
  guards itself (ADR-608-1) and the mutations are executed (ADR-608-6).
- LESSON-459 / LESSON-510 — a gate proves only what it exercises; read the
  forge, never a file that claims to mirror it.
- LESSON-460 — no fixture written from imagination: rulesets detected, not
  parsed.
- LESSON-461 — a conflicted PR is CI-silent; the AC-2 wait polls `mergeable`
  alongside checks.
- LESSON-462 — public-repo evidence: the protection response contains no
  secret, but the before/after is recorded as the context lists only, not the
  full payload.
- LESSON-447 — a gate that waives itself on a transient error is not a gate:
  the pip install and the API read both fail the job when they fail.
