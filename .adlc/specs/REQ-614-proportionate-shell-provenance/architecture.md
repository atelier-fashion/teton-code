---
req: REQ-614
created: 2026-09-04
updated: 2026-09-04
---

# REQ-614 — Proportionate shell provenance: architecture

## The seam this REQ moves

Today `ShellTool::run` ends every arm that spawned a command with
`.with_unknown_provenance()` (four call sites, `harness/tools/shell.rs`
~lines 242–266). `ToolProvenance::Unknown` reaches egress through
`digest::tool_result_provenance` as `Provenance::unknown()`, `inspect`
refuses it whenever `effective_boundaries()` is non-empty, and
`TaintingPrivacySink` / `CarriedTurn::commit`'s `context_is_sensitive` pin
the session. Since REQ-597 put thirteen builtin globs permanently in force,
that chain fires on the first shell command of every session.

This REQ replaces the constant `Unknown` at those four sites with a
**computed verdict**, and gives the pin a cause, a voice and a lift. Nothing
downstream of the verdict is relaxed: `unknown` still maps to
`Provenance::unknown()` and still fail-closes.

## ADR-614-1: The classifier is an allowlist grammar, not a denylist of verbs

**Decision.** `shell_provenance::classify` returns `unknown` unless it fully
understands every token of the command. `rooted` is reachable only through a
path where each segment's verb is in a known table and every remaining token
is either a flag or a path that resolved under the root.

**Why this and not the obvious reading of BR-1(e).** BR-1(e) lists an
opaque-verb set (`sh -c`, `python`, `cargo`, `curl`, …) which reads as a
denylist. Implemented as one, it is a security false negative generator: the
executor is `sh -c <command>` (`run_bounded`, shell.rs:443), so the parse
that decides the command's reach is POSIX `sh`'s, and any hand-rolled
tokenizer diverges from it on exactly the adversarial spellings that matter
(LESSON-494 — one backslash defeated REQ-563's allowlist because the gate and
the socket used two parsers).

The tokenizer already in the file, `command_position_programs`
(shell.rs:689), documents its own misses — indirection through `xargs`, a
quoted env-assignment value — and calls them acceptable because for REQ-607's
withheld advisory "a false negative costs one user the sentence they would
have got". For REQ-614 a false negative costs a **leak**: a verb the
tokenizer failed to see is a command classified `rooted` that could have read
anything. The polarity is inverted, so the same tokenizer cannot be reused
as-is for this decision.

Inverting the default is what makes the tokenizer's imprecision safe: every
miss now lands on `unknown`, which is exactly today's behavior. **The
classifier can only ever be more permissive than today by an amount it can
prove**, which is the property BR-1's first sentence asks for and BR-2
protects.

**Consequences.** Anything containing a quote, a redirection (`>`, `<`,
`>>`), a substitution (`$(`, backtick), a variable (`$`), a glob character in
a path position, a backslash, or a segment whose first word is not in the
known-verb table yields `unknown`. `command_position_programs` is reused for
segment splitting only — never as the sole basis for a `rooted` verdict.

## ADR-614-2: `rooted` additionally requires a `project` root (OQ-1 resolved: yes)

**Decision.** `classify` takes `RootKind` and returns `unknown` for any root
that is not `RootKind::Project`.

**Why.** A home or filesystem root's subtree contains `**/.ssh/**` and
`**/.aws/**` — two of the thirteen builtins — so BR-1(d)'s subtree rule
already forces `unknown` or `boundary_touch` for every content-reading verb
there. What the extra condition buys is the **name-only** verbs: `ls` with no
path at a home root would otherwise be `rooted`, and the value of a
proportionate rule is not worth defending a home-directory listing as
provably in-reach. The spec's own recommendation is yes and REQ-615 makes a
home root loud by other means; the cost is one condition and a test.

`RootKind` is already on `ToolContext` (`kind`, tools/mod.rs:165) and already
read by this file for the macOS timeout hint, so the input costs nothing.

## ADR-614-3: `boundary_touch` is a third `ToolProvenance` variant

**Decision.** `ToolProvenance` (harness/context.rs:87) gains
`BoundaryTouch`. `tool_result_provenance` maps it to `Provenance::unknown()`
— byte-identical egress behavior to `Unknown` — and the taint seam reads the
variant to choose the cause.

**Why not a parallel field.** The alternative was an
`Option<TaintCause>` riding beside the provenance on `ToolOutcome`. Carried
state sheds its invariants silently on a round trip (LESSON-501/502), and
this fact has to survive the tool result → context block → context-provenance
union → carry-commit path. A variant makes the compiler enumerate every
exhaustive match; there is essentially one (`tool_result_provenance`,
digest.rs:95), the other 117 references being constructions and `let-else`
destructures.

**Why the case exists at all.** AC-5 requires `~/.ssh/config` from a project
root to be `boundary_touch`, not `unknown`. That path is **outside the
session root**, so `ProvenanceId::from_resolved` mints nothing for it and no
glob can match it through the ordinary id path — LESSON-623 exactly. The
classifier therefore matches the boundary globs against the **resolved
absolute path with its leading `/` stripped**, so `**/.ssh/**` reaches
`Users/x/.ssh/config`. That spelling MUST be pinned by a test naming AC-5's
literal path: assuming a glob reaches is the LESSON-623 mistake, and the
builtin set is `**/`-prefixed but this has to be demonstrated, not believed.

An **in-root** boundary path needs none of this — it already mints an id,
already matches, already pins permanently with `cause: Boundary`. The variant
exists for the out-of-root case and the compiler keeps the two honest.

## ADR-614-4: The lift is composed into one route predicate, not added at seven call sites

**Decision.** `SessionTaint`'s set becomes a `HashMap<SessionId, TaintCause>`.
A sibling `ShellTaintOverride` (the `WebTaintOverride` shape, `lift` kept
`pub(super)`) holds the lifted ids. A single read — `RoutePin::pins(session)`
— composes them, and the seven sites that today call
`session_taint.is_tainted(session_id)` to force a local route call that
instead: `turn.rs:3272` (`dispatch_route`) and six duty routes in
`runtime/duty.rs` (56, 80, 104, 131, 156, 208).

**Why.** "An invariant with more than one enforcement point needs a sweep,
not a fix" — and where the rule is a property of a method rather than of its
callers, it belongs on the method. Adding `&& !lifted` seven times is seven
chances to miss one, and a missed one is a session that stays pinned after a
lift with nothing failing. Changing the predicate makes the lift true by
construction at all seven.

**The `WebTaintOverride` separation property is preserved.** The taint.rs
docs argue the override must not be a field on `SessionTaint`, because
folding them would let anything that can mark taint unmark its consequence.
`ShellTaintOverride` is likewise a separate type with a non-`pub` setter;
`RoutePin` is a read-only composer holding two `Arc`s, exactly as
`SessionTaintView` already does for the lookup seam. Nothing gains the
ability to unmark.

**AC-8 falls out of this.** `context_is_sensitive` at `carry.rs:510` re-marks
a session whose context still carries an `unknown` block. That is fine and
stays: `try_mark` returns `false` on an already-tainted session so no second
line prints, and the *route* now consults `RoutePin`, which honors the lift.
The carried block is still refused at egress for its own turn (BR-6) because
egress inspects provenance, not the pin.

## ADR-614-5: A truncated subtree scan is `unknown` (spec W1, BR-1(d))

BR-1(d) requires knowing whether any file under a directory argument matches
a boundary glob. The scan runs on the existing budget-capped walker
(`WalkPolicy`, REQ-583 ADR-3). A walk that hit its budget **has not shown the
absence of a boundary file — it stopped looking**, so it yields `unknown`.
Encoded as a return value the caller cannot ignore, not a boolean that reads
the same either way.

## ADR-614-6: `/shell allow` is typed-only; `/web allow` is left alone

Verified during Phase 1: `/web allow` has **no** typed-input gate today
(`slash.rs:753`); only `/model set` (`MODEL_SET_TYPED_ONLY`, slash.rs:217)
and the four mirrored write rows are refused on a non-terminal stdin. BR-5's
typed-only requirement is therefore a deliberate *strengthening* over the
command the spec names as its model. `/shell allow` borrows `/web allow`'s
lift *semantics* (idempotence, the ledger row, session scope, the private
setter) and `/model set`'s *gate*. Whether `/web allow` should gain the same
gate is out of scope and left as a follow-up observation.

## ADR-614-7: The new module keeps the diff off `shell.rs`

The classifier lives in a new file, `harness/tools/shell_provenance.rs`.
`shell.rs` changes at four call sites and one `use`. This is deliberate:
REQ-615 rewrites `shell.rs`'s tool description and adds a cwd note, and
REQ-617 edits `shell_duty.rs`, both concurrently in this sprint. A new file
plus four lines rebases; a 400-line insertion into `shell.rs` does not.

## Announcement wording

`taint_pin_line` (taint.rs:507) today reads "pinned to the local tier **for
the rest of its life** … remote providers will not be used in it again."
That sentence becomes **false** for a liftable `unknown_shell` pin. It is
composed from the cause, with a liftable arm naming `/shell allow` as the
remedy and a permanent arm keeping today's wording — the "compose the
sentence where the facts are" pattern (LESSON-557), one composer, two arms,
both pinned by tests.

## Task graph

```
TASK-001 classifier (pure policy, new file)
    |
    +--> TASK-002 shell.rs wiring + ToolProvenance::BoundaryTouch + ToolContext boundaries
              |
TASK-003 TaintCause + ShellTaintOverride + RoutePin sweep + reason wording
    |         |
    +---------+--> TASK-004 protocol events + shell/override RPC
    |                   |
    +--> TASK-005 ledger shell_overrides table
                        |
                        +--> TASK-006 /shell allow CLI (typed-only) + daemon handler
                        |
                        +--> TASK-007 pin announcement line + doctor surfaces
                                          |
                                          v
                                    TASK-008 end-to-end egress-capture + cost-shape suite
```

TASK-001 and TASK-003 are independent and could run concurrently; this
pipeline runs in subagent mode and executes them in listed order.
