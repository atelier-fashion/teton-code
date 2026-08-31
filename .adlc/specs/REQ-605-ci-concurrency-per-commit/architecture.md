# REQ-605 — Architecture

## Approach

Key the CI concurrency group on the **commit under test** instead of the ref
alone:

```yaml
concurrency:
  group: ci-${{ github.ref }}-${{ github.sha }}
  cancel-in-progress: true
```

Two commits are then two groups, so pushing commit *n+1* cannot cancel commit
*n*'s run. `cancel-in-progress` is **kept**, and still does the one job it can
still do: collapse duplicate runs of the *same* commit on the *same* ref (a
re-run, a close/reopen).

That is the whole change to behaviour. The rest of this document is the
mechanism choice OQ-1 asked for, the trade AC-3 asked to be named, and the
measurement AC-2 asked for.

## Key Decisions

### ADR-1: The group is keyed per commit; cancellation is kept for same-commit duplicates

#### Decision

`group: ci-${{ github.ref }}-${{ github.sha }}`, `cancel-in-progress: true`.

#### Why not the alternatives OQ-1 named

**Dropping `cancel-in-progress` (setting it `false`).** This is the option that
looks safest and is actually the worst of the three. `cancel-in-progress: false`
does not mean "run both" — it means **queue**: a second run in the same group
waits for the first to finish. Every commit would still serialize behind its
predecessor, which is exactly the wall-clock cost REQ-600 paid by hand and this
REQ exists to remove. AC-3 worries about "obsolete runs consuming the queue";
`false` is the only candidate here that creates a queue at all.

Removing the `concurrency:` block entirely avoids the queue but discards the
duplicate-collapse too, and buys nothing that per-commit keying does not already
give.

**Gating on event type** (e.g. `cancel-in-progress: ${{ github.event_name == 'push' }}`).
Rejected on evidence, not on taste. `main`'s push runs have the same defect:
run **33445087015** on `main` was cancelled after 219s — a commit on the default
branch left without complete CI evidence. AC-1 says "every commit pushed as a
tip to one branch", and `main` is such a branch. Gating on event type fixes PRs
and leaves `main` broken.

#### Why `github.sha` and not `github.event.pull_request.head.sha`

One expression covers both triggers. On a `pull_request` event `github.sha` is
the synthetic merge commit; on a `push` event it is the pushed commit. Either
way it is **unique per tree under test**, and uniqueness is the only property
the grouping key needs. The `head.sha || github.sha` form would add a dependence
on `||`'s value-returning semantics and on the shape of `github.event`, for no
behavioural gain — more assumptions means more places to be wrong (LESSON-460).

`github.ref` is retained in the group name so the group is still legible in the
UI, and so a SHA that is simultaneously a `main` tip and a PR head cannot
collide across the two refs.

### ADR-2: The trade — a lost result costs more than a wasted run, and the waste is bounded

AC-3 asks which behaviour is traded for which. Stated plainly:

**Given up.** Automatic cancellation of a *superseded* commit's run. If a
force-push replaces commit X with commit Y, X's run is no longer killed; it runs
to its own conclusion on a tree nobody will merge.

**Gained.** Every commit pushed as a tip keeps a complete result on every
runner, `macos-latest` included.

**Why it cannot be had both ways.** GitHub's `concurrency` primitive keys on a
*string*. It has no notion of ancestry, so it cannot tell "commit *n+1* builds on
commit *n*" (where *n*'s result is still wanted) from "commit Y replaces commit
X" (where X's result is worthless). `cancel-in-progress` treats both identically.
AC-1's property and `cancel-in-progress`'s purpose are the same mechanism pointed
in opposite directions; one of them has to be chosen. This REQ chooses the
result, because REQ-599 shows the price of the missing one: two commits with no
macOS evidence, on the single axis that has already produced a real failure here
(LESSON-591's detached-naming race passed 40/40 locally and on `ubuntu-latest`,
and failed only on macOS).

**Why the waste is bounded, not indefinite.** An abandoned run is not orphaned.
It runs its own jobs to their own conclusions and stops; nothing re-queues it.
The worst case per abandoned tip is one run's job-minutes — measured below — and
GitHub's job timeout caps it absolutely. Critically, nothing *accumulates*:
distinct-SHA groups never queue behind one another, so there is no growing
backlog for stale runs to block. The failure mode AC-3 names is structurally
unreachable under this mechanism, and reachable under `cancel-in-progress: false`.

#### Residual risk (named, not measured)

Concurrent runs still consume **account-level** runner concurrency, which is a
separate limit from workflow `concurrency`. If several tips are pushed in quick
succession, their jobs can queue at the account level — macOS runners have the
tighter cap. GitHub documents specific per-plan limits; that figure is
**second-hand here and deliberately not quoted**, because nothing in this REQ
measured it. It does not change the decision: account-level queueing delays a
run, it does not destroy its result, which is the failure this REQ is about.

**Cargo-cache contention — checked, and the check was weak.** Runs on one ref can
now overlap, so two of them can reach `Post Cache cargo build` at once.
`Swatinem/rust-cache` keys on `prefix-key` + job id + rust-environment hash +
lockfile — **not** the commit SHA — so two runs on different commits with an
unchanged `Cargo.lock` do share a key. The three concurrent runs in this REQ's
own demonstration (R3/R4/R5) all reported `Cache up-to-date` and every job
passed. That is *not* evidence that contention is safe: those commits touched no
Rust code, so no save was attempted, and the observation could not have revealed
a collision either way (LESSON-569 — an oracle that cannot fail is not a pass).
What can be said is narrower: no contention was observed, the path was not
exercised, and `actions/cache` treats a duplicate-key reservation as a non-fatal
warning, so the expected worst case is a warning rather than a red job. Anyone
who sees a cache warning on an overlapping pair should read it as this, not as a
new defect.

**Rate limiting, lost.** The old shared group capped a PR at one in-flight run,
which incidentally bounded how much runner capacity a rapid pusher could hold.
That cap is gone: N quick pushes now mean N concurrent runs. On this public repo
the exposure is small — fork PRs from first-time contributors need approval to
run at all, and the account-level cap above bounds the rest — but it is a real
consequence of the change and not merely a cost.

### ADR-3: Cancellation was already saving almost nothing — the change is close to free

This is the measurement AC-2 asks for, and it is the reason the trade above is
easy rather than close.

#### Counting rules (stated beside every count, per LESSON-593)

- **Rule R — raw job-minutes.** For each job in a run, `ceil(wall-clock
  seconds / 60)`; summed over the run's jobs. Derived from each job's
  `started_at`/`completed_at` in the Actions API. Vendor-neutral.
- **Rule W — weighted job-minutes.** Rule R with GitHub's published per-OS
  multipliers (`ubuntu-latest` ×1, `macos-latest` ×10). **This repo is public,
  so GitHub bills nothing for any of it** — W is a resource-intensity proxy and
  an estimate of what the same workload would cost on a private repo. It is
  not an invoice.

Both rules count all **seven** job runs: `check` on ubuntu + macOS, plus
`gated`, `catalog`, `e2e`, `audit`, `tooling`.

The `timing` endpoint's `billable` field is **not** the source — it returns
`total_ms: 0` for every job on this public repo. Job timestamps are.

#### What was measured

Six real runs on this repo and this workflow — three cancelled under today's
config (REQ-599's branch), three completed (REQ-600's branch, which avoided
cancellation only by waiting):

| run | outcome | job-sec | Rule R | Rule W | job cancelled |
|---|---|---:|---:|---:|---|
| 33338739669 | cancelled | 482 | 11 | 47 | `fmt · clippy · test (macos-latest)` |
| 33338614984 | cancelled | 466 | 11 | 47 | `fmt · clippy · test (macos-latest)` |
| 33328885941 | cancelled | 535 | 13 | 67 | `fmt · clippy · test (macos-latest)` |
| 33444782077 | success | 486 | 12 | 57 | — |
| 33442340561 | success | 515 | 12 | 57 | — |
| 33441618721 | success | 499 | 12 | 57 | — |

Mean cancelled: **11.7 R / 53.7 W**. Mean completed: **12.0 R / 57.0 W**.

#### The finding

**A cancelled run costs almost exactly what a completed one costs.** In all
three cancelled runs the kill lands inside the `Tests` step, *after*
`Set up job`, `Checkout`, `Install pinned Rust toolchain`, `Cache cargo build`,
`Formatting` and `Clippy (warnings denied)` have every one succeeded. The run has
already paid for the toolchain install, the cache restore and a full clippy
compile; cancellation destroys the one step that produces the evidence, and
returns roughly ten seconds of macOS time:

| | cancelled macOS `check` | completed macOS `check` |
|---|---:|---:|
| durations | 175s, 162s, 221s | 184s, 226s, 179s |
| mean | 186s | 196s |

There is a second, hidden cost on the cancelled side: `Post Cache cargo build`
is **skipped** when the job is cancelled, so the run does not save its cargo
cache — making the *next* run slower. Cancellation is not merely a poor trade,
it is mildly negative.

#### Before and after, for a representative multi-commit PR

Representative = **7 commits pushed as tips**, the shape REQ-599 actually had.
There are two honest "before" baselines, because the two prior REQs used
different disciplines:

| | Rule R | Rule W | commits with complete macOS evidence | wall-clock |
|---|---:|---:|---:|---|
| **Before-A** — push freely (REQ-599's discipline): 6 cancelled + 1 complete | **82.2** | **379** | 1 of 7 | unserialized |
| **Before-B** — wait for each (REQ-600's discipline): 7 complete | **84.0** | **399** | 7 of 7 | serialized |
| **After** — this change: 7 complete | **84.0** | **399** | 7 of 7 | unserialized |

Read across:

- **Against Before-A the change costs +1.8 R (+2%) / +20 W (+5%)**, and buys six
  macOS results that previously did not exist.
- **Against Before-B the change costs exactly nothing** — the same runner
  minutes — and hands back the wall-clock. REQ-600's measured serialization was
  10 runs across a 2h01m span for 34m of actual run time.

The change is a strict improvement over the discipline REQ-600 actually used,
and a ~2% premium over the discipline REQ-599 used, which is the one that lost
the evidence.

## Blast radius beyond `ci.yml` — the prose

`ci.yml` is the only file whose *behaviour* changes, but it is not the only file
that must change. Two sibling workflows document themselves **by contrast with
`ci.yml`**, and both sentences become false the moment this lands:

- `.github/workflows/release.yml:22` — "Unlike ci.yml, an in-flight run is NOT
  cancelled by a newer one."
- `.github/workflows/deploy-site.yml:60` — "Unlike ci.yml, an in-flight run is
  NOT cancelled by a newer one: …"

(Both line numbers are pre-change, locating the sentences this REQ found. The
rewritten comments sit a few lines lower — `release.yml:27` and
`deploy-site.yml:62` — because the replacements are longer than what they
replaced.)

After this change `ci.yml` also does not cancel on a newer push, so "unlike"
is wrong. The distinction is still real but has moved: `release.yml` and
`deploy-site.yml` **queue** same-group runs (`cancel-in-progress: false`) because
a half-published release or half-rolled revision is worse than waiting; `ci.yml`
runs distinct commits **concurrently** and cancels only same-commit duplicates.
Both comments are rewritten to say that. This is LESSON-599's hazard exactly —
a change whose compiler-invisible half is the prose.

Three other `ci.yml` references were checked and are **unaffected**, stated
explicitly rather than silently skipped:

- `release.yml:218` — about action major-tag pinning, not concurrency.
- `docs/release-runbook.md:405` — about the `tooling` job's existence.
- `tools/release/verify-version.sh:23` — about the exit-code taxonomy.

## How AC-1 is demonstrated

AC-1 requires a real sequence of pushes with run ids and conclusions, and the
demonstration has an ordering trap: until this merges, `main` carries
`cancel-in-progress: true`. Because `pull_request` workflows run from the PR's
merge ref, **the config in force for a run is the one on the branch at that
commit** — so the branch itself can show both configurations, provided each
observation says which one it was made under.

The commit sequence is therefore chosen so the demonstration falls out of the
work rather than being staged separately:

| commit | contains | config in force | expected |
|---|---|---|---|
| A | `architecture.md` | old (ref-only group) | run R1 starts |
| B | `tasks/` | old | R2 starts; **R1 cancelled** — the before observation |
| C | the `ci.yml` change | new (per-commit group) | R3 starts; R2 in a different group, so untouched (mixed — not counted as evidence) |
| D | sibling-workflow comments | new | R4 starts; **R3 survives** — after observation 1 |
| E | recorded evidence | new | R5 starts; **R4 survives** — after observation 2 |

Each push happens while the previous run is still in flight (verified via
`gh run view` before pushing, not assumed) — otherwise the observation is vacuous:
a run that had already finished proves nothing about cancellation. A
single-commit push proves nothing either, which is why AC-1 forbids it.

**LESSON-461 applies to the whole sequence.** A `CONFLICTING` PR has no merge
ref and therefore produces *no* `pull_request` runs at all — silence that reads
as "not started yet". If an expected run does not appear, `gh pr view --json
mergeable` is checked before anything is retriggered or blamed on this change.

## The property is not guarded, and that is a deliberate choice

Nothing fails if someone reverts `group:` to the ref-only form. `actionlint` is a
syntax check and would pass either way; no test reads this file. This repo does
build derived guards for exactly this shape — `runtime_module_map.rs` asserts a
doc against disk — so the omission is worth stating rather than leaving implicit.

It is left unguarded on the grounds that a guard here would be weaker than the
symptom. A regression does not hide: it re-manifests the moment two commits are
pushed in sequence, as a `cancelled` conclusion on the exact required check
(`fmt · clippy · test (macos-latest)`) that branch protection already enforces,
which is how REQ-602 found it in REQ-599. A one-line YAML assertion would also
need a vacuity floor and a recorded mutation to be worth anything under this
repo's own testing conventions — more machinery than the invariant carries.

The comment at the block is doing the real work: it names the trade and the two
rejected alternatives, so a future editor tempted to "simplify" it has the
reasoning in front of them rather than in a REQ they would have to find.

## Out of scope, confirmed

Per the requirement: no change to which jobs run or to the matrix (the seven job
runs are untouched), and no new trigger to give intermediate commits of a
**batched** push their own run. The latter is a real gap — under any concurrency
setting those commits never start a run, because `ci.yml` triggers only on
`pull_request` and on `push: branches: [main]` — but closing it needs a trigger
change, which the requirement excludes and which deserves its own decision.

## Task Graph

```
TASK-314  key the concurrency group per commit  (.github/workflows/ci.yml)
    |
    +--> TASK-315  correct the two sibling-workflow comments
    |               (release.yml, deploy-site.yml)
    |
    +--> TASK-316  demonstrate on a real push sequence, record the evidence
                    (requirement.md verification section)
                    also depends on TASK-315
```

Strictly sequential, and deliberately so despite the ethos preference for
parallelism. TASK-315's comments describe the behaviour TASK-314 creates, and
TASK-316 cannot observe the new behaviour until TASK-314 is on the branch. The
ordering is also the demonstration: each task is one commit, and the pushes
between them are the observations AC-1 requires.
