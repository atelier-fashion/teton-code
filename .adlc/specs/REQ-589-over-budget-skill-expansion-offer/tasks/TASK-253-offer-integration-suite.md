---
id: TASK-253
title: "Integration suite for the offer"
status: complete
parent: REQ-589
created: 2026-08-24
updated: 2026-08-24
dependencies: [TASK-247, TASK-248, TASK-250]
---

## Description

A dedicated suite — the ACs cut across too many existing files to bolt onto `skill_turn.rs`. Drive everything end-to-end from a real turn (LESSON-544/552).

## Files to Create/Modify

- `crates/tetond/tests/skill_over_budget_offer.rs` (new)
- `crates/tetond/tests/egress_capture.rs` — the choke-point half of BR-11
- `crates/tetond/tests/skill_tool_loop.rs` — AC-5, model path never offered

## Acceptance Criteria

- [x] AC-1 reproduces the reported failure (4,097 words vs 4,096, `bound: local engine`) and accepting dispatches the expansion whole
- [x] AC-3, AC-4, AC-5, AC-6, AC-7, AC-7a, AC-7b, AC-10, AC-11, AC-18 each have a named test
- [ ] AC-9, AC-22, AC-23, AC-24 — **not closed here**; see *Gaps* below. AC-9 is half-covered (the trust refusal wins and no budget question is reached); AC-22/23/24 depend on TASK-249, which is still `draft`
- [x] Every new wire fact is driven from a real turn, never a struct literal; mutating a producer line reddens the suite (AC-12, LESSON-544/552)
- [x] Only reachable (bound, verdict) cells are exercised — no vacuous tests (LESSON-520)
- [x] Run with `--no-fail-fast`; build the workspace before any targeted `-p tetond --test` run
- [x] **AC-15**: the dogfood runbook is authored below

## Technical Notes

> **`budget_inputs_for` is `pub(crate)` and invisible from `crates/tetond/tests/`
> (TASK-259).** If this suite needs a router's declared window, reach it through a daemon
> surface — do not widen the visibility to suit a test. The `pub(crate)` boundary is itself
> pinned by `the_inputs_accessor_stops_at_the_crate_boundary`.


> **Inherited from TASK-245 — read before writing the AC-16 leg.** "No history block is
> dropped" is exact only for the **prompt** and the **refusal** path. A suspended turn that
> *succeeds* still passes through the un-suspended `EndTurn` gate after the model's answer is
> appended, which can trim context to bound what the NEXT turn carries — that is D-7 working,
> not a BR-12 violation. Drive the **refusal** case (which returns via `?` before any
> post-gate runs, leaving the block list genuinely untouched), or assert against the
> **assembled prompt** rather than the post-turn block list. Asserting the post-turn block
> list on a succeeding turn will read as a BR-12 breach when nothing is wrong.

Fixture gap: `skill_turn.rs`'s Harness cannot build LocalEngine/UserCap/RedactScan routes. Use `context_pressure.rs`'s spawned-daemon pattern (:1095 is the only existing local-route skill refusal) and `budget.rs`'s `remote(window, cap, redact_scan)` for the remote bounds.

## Implementation notes (2026-08-24)

**The fixture is neither of the two the task file named, and both of those notes
were wrong about what was reachable.**

* `budget.rs`'s `remote(window, cap, redact_scan)` lives inside `#[cfg(test)] mod
  tests` and does **not** exist when an integration test links the `tetond`
  rlib. It cannot be called from `crates/tetond/tests/` at all.
* `context_pressure.rs`'s spawned-daemon pattern *can* reach a local route, but
  its `Client` auto-answers every `permission_request` with `allow_once` — which
  is not one of the four over-budget ids — and has no seam for answering one
  prompt with a chosen id while the turn that raised it is still blocked. Every
  AC below turns on *which* option was selected, so that harness settles none of
  them. (This also explains why the three pre-REQ-589 refusal tests in that file
  still pass: `allow_once` falls through `interpret_over_budget`'s `_ =>` arm to
  "not sent".)

**What shipped instead: `DaemonRuntime::from_env` over a real `config.toml`.**
`from_env` reads `<base_dir>/config.toml`, so one temp tree per fixture buys every
bound — `[privacy] redact = true` for `RedactScan`, `capabilities.context_budget_cap`
for `UserCap`, a `kind = "local"` provider for `LocalEngine` — plus **a config file
on disk to read the durable remedy back from** (LESSON-519). `TETON_LOCAL_SCRIPT`
gives the binary a real local tier, which is what makes the reported route
reachable in-process at all; a scripted engine is exempt from the first-run
consent flow, so no fixture has to answer a model proposal first. The client is a
local `AddressedPermissionDelivery` that selects **by option id**.

**AC-1's body is calibrated from a real turn, not written.** Stage A measures the
body *with the system prompt*, inside the user frame, so no literal body can name
its own measured figures — and the **root path length reaches the system prompt**,
so the overhead constant is per-tree. The test therefore runs one turn in its own
tree to read the overhead off the wire, rewrites `SKILL.md`, and asserts the
second turn measures exactly `(4,097 words, 31,000 bytes)` against
`(4,096 / 32,768)`. A change to the system prompt moves the calibration instead of
quietly ending the reproduction.

**Mutations run, and what each reddened:**

| Mutation | Result |
|---|---|
| `interpret_over_budget`'s `_ =>` arm → `PermissionDecision::Allowed` | **all 11** tests in the new file fail |
| `Remedy::for_bound`'s `RedactScan` arm → `RaiseWindow { .. }` | exactly `every_bound_offers_exactly_the_remedy_the_table_names` fails |
| `turn_loop.rs:1505` `SkillCaller::Model` → `SkillCaller::User` | exactly the AC-5 test fails |

Each was applied to a working copy, run, and reverted with a checksum check
either side (siblings were live in `runtime.rs`/`server.rs` at the time).

**`.window` vs `.cap` (ADR-15) is pinned arithmetically rather than by a run**,
because `runtime.rs` was sibling-owned. The `UserCap` + `FitsWindow` cell declares
`max_context = 200000` under `context_budget_cap = 6000` and measures 6,970 words /
31,768 B → `claimed_provider_tokens` = 15,884. Against the window that is
`FitsWindow`, which the test asserts; against the cap it is `ExceedsWindow`. The
swap flips the assertion by construction.

**`RedactScan` needed `$ARGUMENTS`.** The redact clamp is a fixed 88,196 bytes and
discovery caps one `SKILL.md` at 64 KiB, so no skill **body** can press that bound
alone. The fixture pushes the expansion past it with an argument string, which is
bounded only by the RPC frame.

## Findings for verify

1. **`unanswerable` still publishes `skill_over_budget_offered`.** The publish sits
   above the gate call, so a question that was *put* but could not be *delivered*
   is recorded as offered; only `invoker: None` and a trust refusal publish
   nothing. That is defensible — the daemon did put the question — but it means a
   reader of the record cannot tell "asked and declined" from "asked and nobody
   could answer" without also reading the absence of an accept. Named, not
   changed.
2. **`OverBudgetOffer::accepted_record` still has no wire surface** (TASK-247's own
   flag). AC-11 is therefore asserted over the RPC result and every published
   event, paired against the declined leg. Whoever gives that record a surface
   should extend the assertion to it.
3. **`ObservedWindowRejections::mark()` has no production caller.** AC-23's arm is
   unreachable from a real turn until TASK-249 lands.

## Gaps — not closed by this task

- **AC-9** is half-covered. `every_not_sent_path_reaches_no_provider_and_spends_nothing`'s
  `trust declined` leg proves the trust refusal wins and that no budget question is
  reached; the *positive* half — a **user**-authored skill reaching the budget
  question with no acknowledgment raised — is asserted only incidentally (the
  user-sourced fixtures raise exactly one prompt). A named AC-9 test belongs with
  TASK-248's own coverage.
- **AC-22, AC-23, AC-24** are not written. All three need TASK-249
  (withdraw-on-context-failure), which is still `draft`: `mark()` has no production
  caller, so AC-23's observed-rejection lead cannot be driven from a real turn, and
  AC-22's withdrawal has no trigger. Writing them now would mean hand-marking the
  memo from the test — a struct-literal test in everything but name (LESSON-544).
- **`RedactScan` + `ExceedsWindow`** is reachable and deliberately unwritten: it
  says nothing about BR-7b the `FitsWindow` cell does not, and that fixture is the
  expensive one.
- **`UserCap` + `ExceedsWindow`** *is* covered, but note it needs a *small*
  declared window (8,000) beneath the cap; on a large declaration a 64 KiB body
  cannot reach the window at all.
- **The runbook below is not in `docs/manual-verification.md`.** This task's file
  ownership did not extend there. Transcribe it verbatim at wrapup.

## AC-15 — the dogfood runbook

*Transcribe the block below into `docs/manual-verification.md`, in the form its
other sections take.*

---

# Manual verification runbook — REQ-589 (the over-budget skill offer)

## What this proves that CI does not

CI proves the parts, and proves them from real turns: the question is asked on
every reachable (bound, verdict) cell, declining is byte-identical to today's
refusal, all four answers are honored independently, the durable write lands on
disk, and no not-sent path reaches a provider. What no fixture can settle is
**ASSUME-A**: whether a *real* local engine — llama.cpp at `n_ctx = 16,384` —
actually serves a prompt this daemon measured at 4,097 whitespace words.

The suite's local tier is a **scripted** engine that answers whatever it is
handed, so "accepting dispatches the turn" is proved and "the turn completes" is
not. AC-1 stops at *dispatch* for exactly that reason.

This runbook is **the first real data point REQ-590 needs**. Record the measured
pair, the verdict, and the outcome — not just pass/fail. A `context_length_exceeded`
here is a *result*, not a failure: it is BR-12 and D-3 working, and it is the
number that tells REQ-590 how much headroom the local pair really has.

**ASSUME-A is not symmetric, and the two legs below are why.** 4,096 words at the
3/2 safety ratio claims ≈6,144 provider tokens against a 16,384-token window, so
the word half has real headroom. The byte half is the whole window
(32,768 B = 16,384 × 2 B/token) with **no** generation reservation subtracted. A
byte-half overrun is materially more dangerous than a word-half one, and leg (b)
exists to find out how much.

## Prerequisites

- The shipped binary or a `--release` build, with `TETON_TEST_SEAMS` unset.
- **A local tier that actually loads.** This is the whole point: a scripted or
  absent engine makes every leg vacuous. Check with `/verbose` that the route
  reads `bound: local engine` before typing anything.
- A scratch repository with a project marker (`Cargo.toml`, `.git`, …), so the
  root probes as `project` and `.claude/skills/` is discovered.
- Nothing in `~/.claude/skills/` named `analyze`, or the project skill will be
  shadowed and the trust prompt will not appear.

## Procedure

### Setting up the route

1. In the scratch repo, write a config binding **all four tiers** to the local
   provider, so whichever category the expansion classifies to is the route under
   test:

   ```toml
   [[providers]]
   id = "local"
   kind = "local"

   [[tiers]]
   tier = "reflex"
   provider_id = "local"
   # …and scan, build, think, all "local"
   ```

2. Confirm with `/verbose` that the route line reads `bound: local engine` and the
   budget reads `4,096 words / 33 KB`. **If it does not, stop** — every figure
   below is about that pair.

### Leg (a) — the reported failure: one word over, bytes to spare

3. Generate a body that is over on the **word** half and under on the byte half:

   ```
   python3 - <<'EOF'
   import pathlib
   words, target_bytes = 3200, 23000   # tuned in step 5
   base = target_bytes - (words - 1)
   out, extra = [], base % words
   for i in range(words):
       out.append("a" * (base // words + (1 if i < extra else 0)))
   body = " ".join(out)
   p = pathlib.Path(".claude/skills/analyze/SKILL.md")
   p.parent.mkdir(parents=True, exist_ok=True)
   p.write_text("---\ndescription: the dogfood fixture\n---\n\n" + body + "\n")
   EOF
   ```

4. `teton` in that repo, then type `/analyze`.
5. **Answer the project-skill trust prompt** (`permission requested:
   project_skill_trust:`) with `y`. Then read the offer's first sentence. It names
   the measured pair — e.g. `comes to about 4,431 words / 30 KB`. Subtract to get
   the overhead, adjust `words` in step 3 by the difference, and repeat until the
   sentence reads **`about 4,097 words`** with a byte figure **below 33 KB**. Two
   iterations is normal.
6. Record the whole offer verbatim: the measured pair, the budget pair, the bound,
   the verdict clause, the remedy line, and the option rows.
   - The verdict clause on this route must be *"This route declares no context
     window, so this daemon cannot promise the send will fit…"* — `WindowUnknown`.
   - With **no** remote provider registered the prompt carries **two** options and
     the daemon prints a stderr line saying the rebind option was withheld
     (BR-9/ADR-12). With **exactly one** registered it carries four.
7. Choose **1** — *Send it whole this once*.
8. **Record the outcome.** One of:
   - the turn completes and the model answers → ASSUME-A holds at a one-word
     overrun;
   - the turn ends with a **context-length error** → the typed
     `context_length_exceeded` outcome ADR-3 built for the local tier, which is
     BR-12 and D-3 working as designed;
   - anything else (an `INTERNAL_ERROR`, a hang, a silently shortened answer) →
     that is a defect, and the sentence it printed is the report.
9. Note whether the conversation is intact afterwards (BR-12: consenting to an
   oversized send is not consenting to losing history).

### Leg (b) — the dangerous half: over on bytes, under on words

10. Regenerate with `words = 3000, target_bytes = 40000` and re-tune until the
    offer reads a word figure **at or below 4,096** and a byte figure **above
    33 KB**. This is the leg ASSUME-A says is materially more dangerous, because
    the byte pair has no generation reservation subtracted from it.
11. Accept, and record the same three things.

### Leg (c) — the decline, and that it is today's refusal

12. Invoke `/analyze` again and choose the last option — *Do not send it*.
13. The error must be `-32023` and must end with *"Nothing was sent and no
    provider saw this turn — a skill expansion is carried whole or refused, never
    shortened into something you did not invoke."* Confirm no verdict clause, no
    remedy line, and no question mark survive into it.

### Leg (d) — the remedy closes the circle (AC-24)

14. Register **exactly one** remote provider (`teton provider add …`), leaving all
    tiers on `local`.
15. Invoke `/analyze` again. The prompt now carries four options, and the remedy
    names *both* halves — bind the tier **and** declare that provider's
    `capabilities.max_context` — plus the cost consequence of moving a whole
    category's spend.
16. Choose **3** — *Do not send it, but …*. The turn must still refuse.
17. Read `config.toml` **on disk**. It must carry both the tier binding and the
    declared window. A file with a newly-bound remote tier and `max_context = 0`
    is the original circle and is a defect (ADR-5 orders the writes so that state
    is unreachable).
18. Invoke `/analyze` once more. On the now-remote route it should **fit**, and no
    offer should appear at all. That is the end-to-end proof the reported circle
    is closed.

## Sign-off

```
Date / build / commit                              :
Local tier really loaded (model id)                :
Route line read `bound: local engine`              : yes / no
(a) measured pair as offered                       :        words /        KB
(a) budget pair as offered                         : 4,096 words / 33 KB   (yes/no)
(a) verdict clause                                 : window_unknown / other:
(a) options offered                                : 2 / 4        (remote providers registered:   )
(a) outcome after choosing 1                       : completed | context_length_exceeded | other:
(a) if completed — did the answer look sane?       :
(a) conversation intact afterwards (BR-12)         : yes / no
(b) measured pair as offered                       :        words /        KB
(b) outcome after choosing 1                       : completed | context_length_exceeded | other:
(c) decline returned -32023                        : yes / no
(c) refusal ended with the "nothing was sent" clause: yes / no
(c) refusal carried no verdict/remedy/question     : yes / no
(d) four options, remedy named both halves         : yes / no
(d) cost consequence stated in the same sentence   : yes / no
(d) config on disk carried BOTH writes             : yes / no
(d) `max_context = 0` on a newly-bound remote tier : yes / no   ← must be no
(d) the next invocation reached no offer at all    : yes / no
Notes / findings                                   :
```

---
