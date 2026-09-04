# REQ-617 — Architecture

## The shape of the change

The REQ's Description already names the shape: *a fact the daemon holds and the
model does not, or a rule the daemon enforces for one tool and not the rest*.
That splits cleanly into three independent mechanisms plus one shared data
source, and the dependency graph follows from which of them writes to the
**resident system prompt** — because the prompt has 733 bytes of margin and
everything that competes for it must be measured last (REQ-583 ADR-2's rule).

```
  TASK-001  the roster, in teton-protocol          ── the shared data source
      │
      ├──► TASK-002  teton_docs: two new topics, two completed   (prompt: +index)
      ├──► TASK-007  the deterministic session-state nudge        (no prompt cost)
      └──► TASK-003  the guide's roster + the margin re-measure   (prompt: resident)
                          ▲ runs after 002, because 002 spends prompt bytes too

  TASK-004  the repeat ledger        ── independent of the prompt entirely
      └──► TASK-008  the AC-10 replay

  TASK-005  the shell duty gate      ── independent
  TASK-006  the route-aware skill cap ── independent
```

## ADR-1: The roster is a derived table in `teton-protocol`, not the CLI's table moved

**Decision.** `teton-protocol` gains `commands.rs`, holding
`SessionCommand { name, effect, user_only }` and a `SESSION_COMMANDS` slice of
29 rows. The CLI's `slash::COMMANDS` stays exactly where it is; the daemon reads
the protocol roster.

**Why not the move the spec first assumed.** Each `CommandSpec` carries
`handler: fn(&mut Connection, &mut UiContext<'_>, &str) -> Result<CommandOutcome>`
and `mirror: Option<Mirror>` — both name CLI types. `teton` depends on `tetond`
(`cli_rows.rs` does `include_str!("../../tetond/src/harness/self_config.md")`),
so moving the table into the daemon's reach would invert or cycle the dependency
graph. What the daemon needs is three strings per row, none of which is a
function pointer. This is the architecture's own "declared identity over derived
identity" rule: the roster *declares* what the daemon may say about a command,
and nothing re-derives it from the other subsystem's table.

**Where drift is caught.** Only the CLI can see both tables, so the guard lives
there: `crates/teton/src/slash.rs`'s test block asserts the two name sets are
**equal** — not that one contains the other. A subset assertion in one direction
is how BUG-149 stayed open, and the same asymmetry would let a command be added
to `COMMANDS` with no roster row (the model never learns it) or a roster row
survive a deleted command (the model names a command that does not exist, which
is worse). AC-2's enumeration is that test.

**Consequence.** Adding a command is a two-sided change — the `CommandSpec` and
the `SessionCommand` — and the test names both files in its failure message.
That is the same two-sided posture ADR-009 imposes on markers and REQ-573 on
seam-pinned catalogs.

## ADR-2: The guide carries names; `teton_docs commands` carries effects

**Decision.** BR-1 asks for "one line per built-in command with its effect" in
the resident guide. Measured, that does not fit and the REQ's own ceiling clause
anticipated it: 29 commands at a usable effect clause is ≈1,300 bytes against
**685** of usable room (`RECORDED_PROMPT_MARGIN_BYTES` 733 −
`MIN_PROMPT_HEADROOM_BYTES` 48). So the roster splits by what each half buys:

- **Resident (the guide)** — the command *names*, grouped by family, plus BR-1's
  closing sentence. This is the half that has to be resident, because the failure
  it fixes is the model not knowing `/transcript` **exists**; LESSON-532's
  measured finding is that a fact in context is retrieved reliably, and a name is
  a fact.
- **Reference (`teton_docs commands`)** — the full roster with effects and the
  `teton` twins. A model that needs the effect can fetch it, which is what the
  topic index is for.

This is the third instance of REQ-583 ADR-2's rule (*a resident fact is bought
with reference data, never with the ceiling*) and it is applied in the direction
that rule prescribes rather than against it.

**Why not raise `REDACT_BODY_OVERHEAD_BYTES`.** A 23 → 24 KiB raise re-derives
`REDACT_TOTAL_CAP_CHUNKS`, `REDACT_INPUT_MAX_BYTES`, `REDACT_SCANNABLE_CONTEXT_BYTES`
and `REDACT_MAX_CHUNKS`, and REQ-615 is concurrently spending from the same
margin. Two REQs each moving the same ceiling in the same sprint is the merge
conflict that ends with one of them silently reverted. Fitting inside the margin
costs this REQ nothing it needs and leaves the ceiling for whoever genuinely
cannot fit.

## ADR-3: `MAX_DESCRIPTION_CHARS` does not move; the frame pays again

The `teton_docs` description is at **108 of 120** characters. Two names cost
`", commands"` (10) and `", transcript"` (12) — 22 against 12 left. The frame
sentence pays, exactly as it did for `context` and `skills`:
`Read Teton's own docs, bundled in this binary. ` → `Teton's own docs, bundled in this binary. `
recovers 4, and dropping `in this binary` a further 15. `bundled` is the
load-bearing word (LESSON-493: Teton's configuration is never in the repository)
and it survives; `Read` is implied by the tool's name and its schema.

The ceiling stays at 120. The doc comment's running ledger gains this REQ's line,
because that ledger is the only reason anyone can tell whether a description grew
by decision or by accident.

## ADR-4: The repeat gate sits at the one point a dispatch is about to happen

**Decision.** `harness/repeat.rs` holds `CallFingerprint`, `RepeatLedger` and
`RepeatVerdict`. The gate runs in `run_the_allowed_tool`, immediately **before**
`tools.dispatch(name, tool_ctx, arguments)` and immediately **after** the
permission decision — the same slot REQ-587's over-budget skill refusal already
occupies.

**Three properties, each load-bearing.**

1. **Before dispatch, after the gate.** Before dispatch is what makes BR-4's "the
   refusal costs no tool execution and no duty call" true by construction rather
   than by a later check: `refine` is reached only through the dispatch this arm
   returns before. After the permission gate is what keeps a refused repeat from
   also being an un-asked permission question — the model asked for the same
   thing twice and the second ask is refused, not silently allowed.
2. **Outside the untrusted frame (BR-5).** The refusal is pushed with
   `ctx.push_tool_result(name, None, with_dropped_calls_notice(error_result(&refusal), dropped_calls))`
   — the exact call the over-budget refusal uses. That is BR-5's "same slot
   BUG-147's dropped-calls notice uses", and it is right for the same reason the
   loop's other two sentences are: this text is composed from integers the daemon
   measured and a tool name the registry validated, and it ends by asking the
   model to act, which the envelope's closing sentence would contradict.
3. **The ledger rides `TurnLatches`.** It is per-turn mutable state that both
   `serve_tool_call` and `run_the_allowed_tool` must reach, and `TurnLatches` is
   already the `&mut` value threaded to exactly those two, constructed once per
   turn in `run_session_turn_with_pressure_policy`. Adding a sixth parameter
   instead would land on `serve_tool_call`, which is at five and whose next
   `#[allow(clippy::too_many_arguments)]` the suppression ratchet refuses on the
   grounds that it is an unnamed cluster. The struct's doc comment is rewritten
   to say it is now the turn's mutable state rather than two latches, because a
   name that has stopped describing its contents is how the next reader is
   misled.

**The fingerprint is the canonical JSON (BR-6).** `serde_json::Value` is built on
`BTreeMap` for objects with the `preserve_order` feature off, so serializing it
yields key-sorted output and `{"a":1,"b":2}` and `{"b":2,"a":1}` fingerprint
identically — which is right, they are the same call. `ls -la` and `ls -la .` are
different strings and therefore different calls, which BR-6 requires. The hash is
of `(tool_name, canonical_args_json)`; the ledger never stores the arguments
themselves, so `tool_call_repeated`'s payload cannot leak them.

**The verb table.** `read`, `glob`, `grep`, `projects`, `teton_docs` and a
read-only-verb `shell` refuse on the **second** call; `edit` and every other
`shell` refuse on the **third**. Unknown verbs count as write-capable, which is
the fail-safe direction: mis-classifying a write as read-only would refuse a
legitimate retry after a real change. `skill` is **not** in the table — it keeps
REQ-587's own counter, as the REQ's System Model says.

## ADR-5: The shell duty's gate inverts, and the cost is recorded rather than hidden

**Decision.** `worth_interpreting(failed, raw_output_chars)` goes from
`failed || raw > TRIGGER` to `!failed && raw > TRIGGER`, and `shell_prompt`
loses `and what that means for what the agent should do next` and gains
*"Describe what the output shows. Do not tell the agent what to do next."*

**This is a narrowing that costs something real, and it is written down here
because nothing else would notice.** REQ-561's module doc names two triggers and
calls the failed one primary: *"the command failed — the output is a stack trace
or a compiler wall, and what the model needs out of it is one sentence: what
broke."* BR-7 deletes that trigger outright. After this REQ, a `cargo build` that
fails with 40 KB of compiler output comes back **uninterpreted**, while the same
command succeeding with 40 KB comes back interpreted. AC-7 states both halves, so
this is the spec's decision and not a reading of it — but the consequence is
recorded as **OQ-3** below, because the observed defect was a *one-line* stderr
and a size gate alone would have fixed that case without paying this one.

The prompt sentence is the half that fixes the actual harm. The duty invented
*"The agent needs to either create this directory first…"* because the prompt
asked it to say "what that means for what the agent should do next" — the
harness authorized the imperative and then was surprised by it (LESSON-570: a
prompt sentence must be true after the REQ ships). AC-8's absence assertion over
the reference fixtures is what keeps it gone, and its mutation — restoring the
deleted clause — must go red on at least one fixture or the assertion is
guarding nothing (conventions.md: show the test can fail).

## ADR-6: BR-3's guarantee is a deterministic nudge, not a sentence in the guide

Settled at spec validation and restated here because it is the REQ's most
load-bearing reversal. LESSON-532 measured **0/3** across three rounds of
moving, dictating and isolating exactly the kind of guide sentence BR-3
originally proposed, while the *data* in the same guide crossed perfectly every
round. So the guide keeps the sentence (it is free, and the data half is what
makes the model able to name `/transcript` at all), and the guarantee moves to
`session_ui.rs` beside REQ-579 ADR-9's hand-off nudge, which is the shipped,
tested instance of this exact shape.

The nudge's predicate has the two halves ADR-9 learned to keep symmetric:

- **recital** — the reply reaches for a way to discover session state that the
  model cannot use (a config-file read, a repository search), and
- **dormancy** — the reply already names the command and nothing else, in which
  case the harness stays silent.

Both halves read the **same backtick-stripped text**, because fencing is a
markdown accident and a matcher whose answer depends on it is a matcher whose
behaviour depends on how the model felt about code spans (REQ-579's verify pass
found exactly that hole). And a reply that names the command *and* recites the
unusable path still earns the line — the dormancy hole REQ-579 had to close, and
there is no reason to reopen it here.

## ADR-7: The `skill` cap becomes a route property

`PER_TURN_INVOCATION_CAP` is a `pub const` read by `skill.rs`, two integration
tests and a CLI e2e test that hardcodes `12` with a comment naming the constant.
It becomes `per_turn_invocation_cap(route) -> usize` — 12 remote, 3 local — with
the constant retained as the remote value so the e2e's comment stays true. The
route fact is already in the loop's hand (`config.budget` carries the route's
identity since REQ-586), so this is a read, not a new plumbing path.

## Open Questions

- [ ] **OQ-3 (new, raised by ADR-5).** BR-7 keys the shell duty's gate on
      *failure*; the observed defect was *size* (a one-line stderr). Keying on
      size alone would have fixed the reported case and kept REQ-561's primary
      one. Should a failed command whose output is **capped** be interpreted
      again, under the new imperative-free prompt? Recommended: yes, as a
      follow-up REQ, once AC-8's absence assertion has a release of evidence
      behind it. Not taken here, because BR-7 and AC-7 both state the narrowing
      explicitly and a runner is not the place to overrule a stated rule.
- [ ] **OQ-1 / OQ-2** — unchanged from the requirement; both recommended
      answers stand and neither is implemented here.

## Concurrency with REQ-614 / REQ-615 / REQ-618

All four touch `crates/tetond/src/harness/`. The collisions this REQ can predict:

| File | Also touched by | Shape |
|---|---|---|
| `self_config.md` | REQ-615 (BR-1's cwd contract) | both append; both move the margin constants |
| `tools/shell.rs` | REQ-614, REQ-615 | REQ-617 touches it only for the read-only verb table |
| `shell_duty.rs` | REQ-614 (BR-10 verdict ordering) | REQ-617 changes the gate; REQ-614 changes what feeds it |
| `tools/docs.rs` | none known | index + two topics |

`RECORDED_PROMPT_MARGIN_BYTES` is the one that cannot merge mechanically: it is a
single integer two REQs both re-measure. Whichever lands second **re-measures**
rather than resolving the conflict textually — the number is an observation, and
taking either side of a conflict on an observation records a measurement nobody
made.
