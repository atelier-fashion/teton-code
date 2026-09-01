# REQ-606 — Architecture

## The measurement that decides this REQ

The spec asks for a classification, and offers review's prior judgement that
"roughly five earn a name and the rest are transport" as *input, not
conclusion*. Measured, the judgement is wrong in the direction that matters:
**twelve of the fourteen earn their name, and the rule that says so is
mechanical rather than aesthetic.**

The rule is AC-2. Clippy's `too_many_arguments` threshold is **7** — it fires at
8 — and AC-2 forbids the suppression that would silence it. So for an *input*
bundle, "collapse it" is not a free readability choice: it is a signature-width
change with a hard gate on the other side. The arithmetic is the classification.

### Rule A — an input bundle whose collapse exceeds seven arguments is a real cluster

For each input bundle, the consumer's argument count after collapse is
`current_args − 1 + fields`. Measured at this REQ's base (`e3013c6`):

| bundle | fields | consumer | args now | args if collapsed | verdict |
|---|---:|---|---:|---:|---|
| `AttemptInputs<'a>` | 8 | `run_attempts` | 4 | **11** | keep |
| `SessionFacts<'a>` | 6 | `resolve_the_route` | 4 | **9** | keep |
| `TurnRequest<'a>` | 6 | `resolve_the_route` | 4 | **9** | keep |
| `ExpansionInputs<'a>` | 5 | `settle_expansion` | 4 | **8** | keep |
| `TurnProducts` | 4 | `prepare_the_attempts` | 6 | **9** | keep |
| `LoopContext<'a>` | 6 | `serve_tool_call` | 5 | **10** | keep |
| `ModelReply<'a>` | 7 | `serve_tool_call` | 5 | **11** | keep |

Every one exceeds 7. `SessionFacts` and `TurnRequest` collapse *together* into
the same signature — 14 arguments — which is why REQ-600 split them into two
rather than widening one.

**`TurnProducts` is the row that refutes the spec's prediction, and it is
recorded rather than forced.** The Description calls it transport: "built from
four loose locals at the call site and destructured on the callee's first line."
Both halves of that sentence are true and the conclusion still does not follow.
Collapsing it puts `prepare_the_attempts` at 9. The best available reduction —
passing `tctx` and taking `session_id` and `config` off it, the move
`assemble_harness` already makes — reaches 8, still over. This is exactly the
failure mode the Assumptions name: *the cluster is real and the bundle earns its
name after all.*

### Rule R — a return-position bundle is not under Rule A, and justifies on width

`ClaimedTurn`, `AssembledHarness`, `ResolvedRoute` and `PreparedAttempts` are
returned, not passed. Clippy's argument limit does not reach them, so they must
justify themselves on readability alone. The line drawn here, stated in advance:

> **Three or more heterogeneous values keep a named struct; two become a tuple.**

A three-wide tuple makes the caller's destructure positional, and a caller that
transposes two same-shaped fields gets no diagnostic. At two, the binding names
at the call site carry the meaning and Rust idiom is a tuple.

| bundle | fields | verdict |
|---|---:|---|
| `ClaimedTurn` | 3 | keep |
| `AssembledHarness` | 4 | keep |
| `ResolvedRoute` | 5 | keep |
| `PreparedAttempts` | **3 → 2** | **collapse** (see below) |

### Rule I — a bundle whose fields are mutated together carries an invariant

`AttemptState` (7 fields) and `TurnLatches` (2) are `&mut`-lent across a loop.
They are not signature-width devices; they are the mutable state of an
iteration, and the type is what keeps that state in one place instead of in a
row of out-parameters. `TurnLatches` is two `bool`s — *silently transposable*
as two `&mut bool` arguments and not transposable as a struct — which is a
stronger reason to keep it at two fields than Rule R's width test is to collapse
it.

## The two collapses, and why they are the honest ones

### 1. `PreparedAttempts.refit_system` is a round-trip of a value the caller holds

`prepare_the_attempts` takes `system: &str` and its first statement is
`let refit_system = system.to_owned();`. That clone is returned to
`run_prompt_turn`, which then borrows it back as
`AttemptInputs { refit_system: &refit_system, .. }` — while still holding the
original `system` in scope, unmutated, from `AssembledHarness`.

Traced across the whole body, `system` appears five times and is never rebound:

| line | use |
|---|---|
| 354 | `system,` — destructured out of `AssembledHarness` |
| 367 | `system: &system,` — into `ExpansionInputs` |
| 374 | `refit_system,` — destructured out of `PreparedAttempts` |
| 380 | `&system,` — passed *into* `prepare_the_attempts` |
| 425 | `refit_system: &refit_system,` — into `AttemptInputs` |

So `refit_system == system`, always and by construction. The field is transport
of a value that never left. Removing it:

- deletes one `String` allocation per turn,
- drops `PreparedAttempts` to two fields, where Rule R makes it a tuple,
- and **shrinks `run_prompt_turn` by four lines**, which AC-3's twelve-line
  headroom needs.

The local inside `prepare_the_attempts` stays — `skill_refit` needs an owned
`String` — it is simply no longer returned.

### 2. `ToolCallSite` carries two fields already inside a third

The spec calls it "a borrowed re-projection of `ModelReply`: four of its five
fields come straight from it." Half right, and the half that is wrong is the
half that suggests deleting the type.

**What is genuinely wrong** is narrower and provable. `serve_tool_call` builds

```rust
let call = ToolCall { id: …, name: name.clone(), arguments: arguments.clone() };
```

and then hands the next stage `ToolCallSite { call: &call, name: &name,
arguments: &arguments, request, dropped_calls }`. `call.name` **is**
`name.clone()` and `call.arguments` **is** `arguments.clone()` — the same two
values, reachable through a field already in the bundle. Neither is rebound
between the two statements. So the bundle drops to three fields and
`run_the_allowed_tool` re-derives the two locals at the top:

```rust
let ToolCallSite { call, request, dropped_calls } = site;
let (name, arguments) = (call.name.as_str(), &call.arguments);
```

`name: &str` and `arguments: &serde_json::Value` — **the same types the
destructure produced before**, so none of the 41 use sites below change.

**Why the full collapse is refused, recorded as a finding.** Deleting the type
and passing `&ModelReply` fails twice over. `serve_tool_call` *moves* `text` out
of the reply to build the pushed block, so no whole `&ModelReply` survives to
lend; keeping one costs a `String` clone on every tool call. And destructuring
through a reference rebinds `name` as `&String` and `dropped_calls` as `&u32`,
rippling deref and comparison changes through a 956-line body on a REQ whose
AC-4 is "behaviour unchanged". The re-projection is what the ownership
transition costs, not a redundancy.

## One rename, on a defect found while classifying

`runtime/turn.rs` declares `struct TurnRequest<'a>` and opens with
`use super::*;`. `runtime/mod.rs` imports `teton_providers::TurnRequest` — the
provider-facing request type, constructed at `mod.rs:5629`. A locally-declared
item **shadows a glob import silently**: no warning, no error. Inside the turn
path, `TurnRequest` means the local six-field bundle and the provider type is
unreachable by that name.

Renamed to **`PromptRequest`**, which is also the more accurate name: its six
fields are what one prompt asked for, and "turn request" is the thing that goes
*to a provider*. Rule A keeps the type; this only stops it colliding.

## What is NOT changed, deliberately

- **The module-map table in `.adlc/specs/REQ-599-…/architecture.md`.** The
  dispatch anticipated an edit here and the measurement says otherwise:
  `runtime_module_map.rs` tolerates **10% drift**, `turn.rs` is documented at
  3,407 and measures 3,407, and this REQ removes on the order of 30 lines —
  0.9%. Editing the table would create a needless conflict with REQ-603 and
  REQ-604, which touch the same file. Left alone on a stated rule, not by
  oversight.
- **The REQ-598 event fixture.** AC-4 requires it to replay unregenerated.
- **`run_prompt_turn`'s existing `#[allow(clippy::too_many_arguments)]`.** It is
  pre-existing (the RPC entry point's parameters are the wire's), not added
  here. AC-2 forbids *adding* one; `suppression_ratchet.rs`'s recorded figure is
  therefore unchanged.

## ADR-1 — Update the guards with the code, then re-run their mutations

Three guards read the source and name these types:

| guard | reads | affected by |
|---|---|---|
| `the_claim_is_taken_before_the_registry_is_re_read` | `body.contains("ClaimedTurn {")` — a vacuity floor | nothing (kept) |
| `the_permission_gate_is_fetched_before_the_invocation_is_accepted` | `struct SessionFacts<'a> {`, `facts: SessionFacts<'_>` | nothing (kept) |
| `runtime_module_map` | `turn.rs` production count | 0.9% drift, inside tolerance |

The two collapses touch none of their patterns, which is a property of the
choice rather than luck — the collapses are in `PreparedAttempts` and
`ToolCallSite`, and no guard names either. **That is not evidence the guards
still work.** LESSON-598 and this REQ's AC-4 both say the same thing: a guard
that has stopped covering its subject looks exactly like a guard that passes. So
every mutation is re-run against the changed tree and its observed output
recorded, including the ones expected to be unaffected.

## ADR-2 — The mutation evidence is only valid on the tree that merges

This REQ is third in an overlap cluster: REQ-603 and REQ-604 merge ahead of it
and it rebases onto both. That interacts with AC-4 in a way worth stating,
because getting it wrong reproduces the exact defect AC-4 exists to catch.

REQ-603 relocates session-lifecycle production code out of `runtime/mod.rs`.
Four of the five invariant guards live in that file's test module and read the
turn path *by source scan*. A mutation re-run before the rebase is evidence
about a tree that will not be merged — and a guard whose subject moved under it
still passes, silently, which is the whole of LESSON-598.

So TASK-005's mutations are re-run **after the final rebase**, not before, and
the verification record states the commit they were run against. Running them
earlier is fine as a working check; it is not the evidence AC-4 asks for.

The file-level conflict risk is separately low: this REQ touches
`runtime/turn.rs` and `harness/turn_loop.rs`; REQ-603 touches `runtime/mod.rs`
and adds `runtime/session.rs`; REQ-604 adds fixtures. The one shared file would
have been REQ-599's module-map table, which this REQ does not edit.

## Task graph

```
TASK-001  classify all fourteen; write the stated reason into each type's doc   (AC-1)
   │
   ├── TASK-002  collapse PreparedAttempts to (AttemptState, usize)             (AC-1, AC-3)
   ├── TASK-003  narrow ToolCallSite to three fields                            (AC-1)
   └── TASK-004  rename TurnRequest -> PromptRequest                            (AC-1)
                          │
TASK-005  re-run the five invariant mutations + fixture; measure AC-3           (AC-3, AC-4, AC-5)
```

TASK-002, 003 and 004 are independent of each other and all depend on the
classification being written down first, because the classification is what
decides whether each is done at all.
