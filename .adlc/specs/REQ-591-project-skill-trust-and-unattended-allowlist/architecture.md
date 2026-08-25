# REQ-591 — Architecture of the carve-out

## Approach

This is **branch surgery, not feature design.** The code exists as five commits at positions
4, 5, 8, 11 and 22 of the 33 on `feat/REQ-589-over-budget-skill-expansion-offer` — interleaved
with offer commits, not contiguous. The job is to land them on a branch cut from `origin/main`,
rebuild REQ-589's branch without them, and prove both halves still behave.

The ordering principle throughout: **build the new branch first and verify it, then rewrite the
old one, and force-push only after both are green.** Every step before the rewrite is additive
and reversible.

## Key decisions

### ADR-1 — `accept_invocation`'s `async` belongs to REQ-591

`accept_invocation` was `fn` on `origin/main` and became `async fn` in `b4e4b01`, whose own doc
states the reason: *"`async` is the forcing function. The signature change is what makes [a
caller that skips the acknowledgment fail to compile]."* There is exactly **one** `.await`
inside it, and it is the trust gate's (`runtime.rs:3917-3940`).

**Decision:** the async-ness leaves with the trust gate. REQ-589's branch reverts to the
`origin/main` signature — a plain `fn` with a non-awaiting caller.

**Rationale:** the offer's Stage A (`offer_or_refuse_over_budget`) is a separate `async fn`
called from `run_prompt_turn`, not from inside `accept_invocation`. Removing the trust block
leaves a body with no `.await`, so the sync signature is not an adaptation — it is simply what
the function was. ASSUME-C feared this seam; it is the easiest part of the split, and it is easy
*because* TASK-248 put the gate inside the function rather than at the call site.

### ADR-2 — `PermissionSubject::ProjectSkillTrust` stays on `main`; there is no protocol break

The entanglement map rated a protocol-breaking variant removal as the split's only **High**
difficulty. **Refuted:** `ProjectSkillTrust` occurs 11 times on `origin/main`. The variant
predates REQ-589 — the model-invoked door has always used it. Only the `invoked_by` field
(`b071da5`) is new.

**Decision:** the variant is untouched by the carve-out. REQ-591 takes `invoked_by`; REQ-589's
branch returns to the pre-`b071da5` shape. No versioning question arises.

**Recorded because the map was wrong and a reader may find its reasoning first.** The map could
not run git and said so; this ADR is the correction, from evidence.

### ADR-3 — Rebase-drop, not cherry-pick-range

The five commits are at positions 4, 5, 8, 11, 22 of 33. There is no contiguous range to pick.

**Decision:** two independent operations.
- **REQ-591's branch:** cherry-pick the five onto `origin/main` in original chronological order
  (`b4e4b01` → `b071da5` → `4be0c34` → `37a2e6c` → `bda079d`), because each builds on the last.
- **REQ-589's branch:** `git rebase --onto origin/main` dropping exactly those five SHAs.

Not `git revert`: a revert leaves both the change and its undo in history, so REQ-589's branch
would still *contain* the trust work and a reader would have to diff two commits to see it is
gone. The carve-out's point is that the trust work is not there.

### ADR-4 — No later commit carries trust code (verified, not assumed)

`git log -S` over every trust symbol across all 33 commits returns, beyond the five: `84a9d89`
(a `pipeline-state.json` doc commit), `65a66a3`/`ee6c5bf`/`6d4d2fd` (spec docs), and `607cb74`
— an **offer** commit whose only hit is a *comment* mentioning `accept_invocation`, not code.

**Decision:** the five commits are the whole of the trust code. The comment in `607cb74` is
adjusted in place on REQ-589's branch rather than treated as an entanglement.

### ADR-5 — `cli_e2e::a_typed_invocation_names_the_swap…` moves to REQ-591

That test was **broken** by the trust gate and **repaired** by `4be0c34`'s
`spawn_scripted_trusting`. It cannot run a piped project-skill invocation without either the
trust infrastructure, a human at the terminal, or a permission level that does not in fact
clear a shadowing skill.

**Decision:** it moves. On REQ-589's branch it returns to its `origin/main` form, which passes
because the gate that broke it is gone.

**This is the split's sharpest test:** if REQ-589's branch is green with that test in its
original form, the trust work is genuinely absent rather than merely disabled.

### ADR-6 — The force-push is a gated, reversible, owner-confirmed step

`feat/REQ-589-over-budget-skill-expansion-offer` is pushed (remote tip `9067374`). Rewriting it
requires `--force-with-lease`, and it is the one step in this REQ that can lose work.

**Decision, in order and non-negotiable:**
1. Record the pre-rewrite SHA in `pipeline-state.json` **and** in a local tag
   (`req589-pre-carveout`) so the old branch is recoverable by name, not by reflog archaeology.
2. REQ-591's branch must be green first.
3. REQ-589's rebuilt branch must be green **locally** before anything is pushed.
4. The push uses `--force-with-lease`, never `--force`, so a concurrent remote update aborts it.
5. A task **surfaces it for owner confirmation** and does not perform it autonomously.

**Rationale:** every other step here is additive. This one is not, and the pipeline's own
autonomy contract does not extend to rewriting shared history on the user's behalf.

### ADR-7 — Manual removal, not cherry-pick residue, in `permissions.rs`

Dropping the trust commits leaves `PermissionGate` with `trusted_project_roots` and
`project_trust_persistence` fields that nothing reads. A cherry-pick would leave them
initialized to empty and "work".

**Decision:** remove them. Dead fields on a permission gate are exactly the surface a later
reader mistakes for a live control.

### ADR-8 — ADR-4 checked one direction only (correction, from TASK-264)

ADR-4 verified by `git log -S` that **no offer commit carries trust code**. It never asked the
reverse: whether **trust code compiles against offer code**. `4be0c34` does.

Two seams from `e8b1bfb` (TASK-244, an offer commit) had to travel, narrowed:

- **`Question`** — `4be0c34` constructs `Question::ProjectTrust { durable_root }` and reads
  `durable_project_root()`. On `origin/main`, `settle`/`interpret` take a bare
  `web: Option<WebTier>`. Carried with **two** variants (`Standard`, `ProjectTrust`), leaving
  `OverBudget`/`consults_grants`/`remedy_offered` behind.
- **The addressed-route test double + `wired()`** — all eight of `4be0c34`'s D-13 tests are
  written through them. Carried, renamed `OverBudgetRoute` → `AddressedRoute`. Also needed
  `grant_keys()` from `a23c9f2`.

**Consequence, and it is a real one:** when TASK-266 rebases REQ-589 onto `origin/main`,
`e8b1bfb` re-introduces both — so **each branch will define its own `Question` and route
double**. That is a merge-time reconciliation, not a split defect, but ADR-3 did not predict it
and whoever merges second inherits it. It belongs under AC-10.

**The lesson for any future carve-out in this repo:** "does A carry B's code" and "does A
compile against B's code" are different questions, and only the first is answerable by
`git log -S`.

### ADR-9 — `37a2e6c` does not travel, and AC-1's ordering leg is authored here

Both of `37a2e6c`'s tests live in `skill_over_budget_offer.rs` — a file created by offer commit
`53f1c71` — and both assert a prompt log containing the **budget** question. On a branch with no
budget question, `["project trust", "over-budget offer"]` degenerates to a one-element list and
its sibling to an empty one. **Ported, they would pass while asserting nothing** — the vacuity
the criterion exists to prevent.

**Decision:** `37a2e6c` stays with REQ-589. REQ-591's AC-1 ordering leg is **authored fresh**
against the two gates this branch actually has (trust, then `authorize_skill`). TASK-268 owns it.

The ordering is not unwitnessed in the meantime: `b4e4b01` carried
`declining_the_repository_refuses_the_turn_and_asks_no_budget_question`, which asserts the same
order from the engine's prompt list and reddens under the skip-the-trust-block mutation.

**Also corrected here:** the TASK-264 brief said "AC-9 ordering". REQ-591's AC-9 is
`cargo audit`; the ordering is **AC-1**. REQ-589's AC-9 was the ordering. Two numbering schemes,
one conflated instruction — caught by the implementer, not by me.

## What the split must preserve

- **REQ-589 behaves identically** — this is the constraint that decided ADR-1, and AC-10.
- **Every mutation-verified test stays mutation-verified.** Three exist: the AC-9 ordering
  assertion (verified by skipping the trust block), TASK-256's three guard seams, and the
  TOCTOU attack reproduction. Moving a test does not preserve its bite; re-running the mutation
  does.
- **The three open questions stay open.** OQ-1 (daemon-wide gates), OQ-2 (does one row answer
  both doors), OQ-3 (`plan`) gate the *merge*, not the split. Nothing here designs around a
  guess at them.
