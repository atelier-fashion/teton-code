---
req: REQ-561
title: "Architecture — wiring triage, shell, title, compact onto a shared duty seam"
created: 2026-08-07
updated: 2026-08-07
---

## Approach

REQ-558 built `digest` as a one-off: a `DigestRoute` enum, a `Digester` trait, a
`digest_route()` resolver, a `LocalDigester`/`RemoteDigester` pair, an egress
scoping call, a ceiling constant, and a LESSON-447 fallback — roughly 260 lines
of machinery for one category. REQ-561 adds four more callers. Copying that
shape four times would produce five parallel implementations of the same five
concerns, which is what BR-6 exists to prevent.

So the work is a **refactor first, then four thin call sites**. `digest` is
migrated onto the shared seam as part of building it, which is what proves the
seam is actually general rather than a `digest`-shaped hole with new names.

**Verified precedent** (file:line, read before designing):

| Concern | Today | File |
|---|---|---|
| Route enum | `DigestRoute::{Serves,Unresolved}` | `harness/digest.rs:113` |
| Duty trait | `Digester::digest(&self, prompt, provenance)` | `harness/digest.rs:91` |
| Resolution | `digest_route()` | `runtime.rs:1853` |
| Category literal | `router.resolve(Category::Digest)` | `runtime.rs:1864` |
| Egress scoping | `Egress::scoped(provenance, ctx)` → `TurnTransport` | `egress/mod.rs:345` |
| Provenance bridge | `tool_result_provenance()` | `harness/digest.rs:70` |
| Ceiling | `DIGEST_OUTPUT_MAX_BYTES` | `harness/digest.rs:237` |
| Fallback | `summarize_if_large()` degrades to `truncate_middle()` | `harness/context.rs:689` |
| Emission | `Router::emit_route_decided(bus, session, &Route)` | `router.rs:638` |

## ADR-1: The seam is one non-generic `DutyRoute` holding `Arc<dyn Duty>`

**Decision.** `DutyRoute` is a single concrete type. It is **not** generic over
the duty (`DutyRoute<T: Duty>`), and there is **no** `TriageRoute`/`ShellRoute`/
`TitleRoute`/`CompactRoute`.

**Why.** A generic `DutyRoute<T>` monomorphises into five distinct types, which
is the same five-parallel-implementations outcome BR-6 forbids, just expressed
in the type system instead of in copied code. AC-8 asks for an assertion that
"no per-category duty plumbing survives" — a generic would make that assertion
unwritable, because five types would be the expected state. `DigestRoute`
already holds a boxed `Arc<dyn Digester>`, so dynamic dispatch is the
established precedent and costs nothing measurable: a duty performs a model
call, so one vtable indirection is free next to the inference.

**Rejected:** the exploration agent proposed four parallel route enums. That is
the pre-REQ-561 shape restated, and it is precisely what this REQ was written
to remove.

## ADR-2: The `Duty` trait returns **bounded text**; the call site interprets it

**Decision.**

```rust
#[async_trait]
pub trait Duty: Send + Sync {
    /// The duty's own category — used for cost attribution and `route_decided`.
    fn category(&self) -> Category;
    /// The harness-owned output ceiling (BR-8). Enforced by the impl, not the provider.
    fn ceiling_bytes(&self) -> usize;
    async fn perform(&self, prompt: &str, provenance: &Provenance) -> Result<String, String>;
}
```

Every duty returns a `String` bounded by its own `ceiling_bytes()`. Structured
decisions are parsed **at the call site**, not inside the seam.

**Why.** `compact` is the awkward case: it decides which conversation blocks to
forget, which sounds like it wants a structured return. But the model returns
text regardless — something has to parse it, and putting the parser inside the
seam would make the seam `compact`-aware and reintroduce per-category plumbing.
Keeping `perform` uniformly text-returning means the ceiling (BR-8) is enforced
in exactly one place for all five duties, and a malformed `compact` response is
a *parse failure at the call site*, which routes straight into that call site's
own BR-3 fallback. The failure lands where the invariant lives.

**Consequence for BR-7.** `perform` takes `&Provenance` — the already-merged
form — not `&ToolProvenance` as `Digester::digest` does today. Each call site
computes the provenance of *the content it is about to send*: `triage` from the
matched files, `shell` from the command's unknown-provenance output, `compact`
from the conversation blocks, `digest` via the existing
`tool_result_provenance()` helper (now demoted to a caller-side convenience).
This makes BR-7's "scoped by the content it sends" a property of the signature
rather than a convention each duty has to remember.

## ADR-3: The category literal stays in a thin per-duty resolver — because the scan requires it

**Decision.** `runtime.rs` keeps one small resolver function per duty
(`triage_route`, `shell_route`, `title_route`, `compact_route`, and the migrated
`digest_route`). Each spells `router.resolve(Category::X)` **literally** and then
delegates everything else to one shared `resolve_duty()` helper.

**Why this is forced, not chosen.** `call_sites.rs:220` derives the "has a call
site" marker by scanning daemon source for the literal needle `router.<method>(`
with a `Category::X` literal inside the balanced-paren argument
(`CATEGORY_BEARING = ["resolve", "resolution_for", "resolve_judgment"]`). If the
seam were collapsed to a single `resolve_duty(category)` taking a *variable*,
the scan would find **zero** new call sites and AC-1 would fail — the marker
would keep reporting `declared, no call site yet` for all four categories even
though they were fully wired. The scan is a derived-marker test, so it would
fail in a way that points at the marker rather than at the receiver, which is
exactly the confusing failure mode noted during REQ-558's architecture review.

**This does not violate BR-6.** The per-duty code is bounded to two things: the
one-line resolve call naming the category, and the duty's output-contract
constant. Everything else — the route type, the `Duty` trait, the local/remote
impls, egress scoping, ceiling enforcement, `route_decided` emission, and the
fallback shape — has exactly one implementation. AC-8's assertion is written
against that boundary (see "AC-8's testable boundary" below).

## ADR-4: `compact` runs *ahead of* the hard gate, never *instead of* it

**Decision.** `ContextManager` gains `compact_if_pressured()`, called immediately
**before** the existing `ctx.truncate_to_budget()` — which lives at
`harness/turn_loop.rs:704`, **not** at `harness/context.rs:618` as earlier drafts
of this ADR and TASK-063 both stated. That reference was wrong: `context.rs:618`
is inside `ceil_char_boundary`, and the only production call site in the
workspace is in the turn loop. The error came from transcribing an exploration
agent's "used in turn_loop at line 618" as a `context.rs` line. Corrected after
TASK-063 surfaced it.

That existing line is **not modified, not wrapped, and not made conditional** —
verified: it sits outside any conditional, and the only diff lines mentioning it
are doc comments.

**Why.** This is BR-4a made structural. Because `truncate_to_budget()` still runs
unconditionally afterward, a `compact` duty that hangs, returns garbage, returns
an over-budget result, is never routed, or panics cannot produce an over-budget
context — the thing enforcing the budget was never the duty. The alternative
(replacing the call) makes every safety property depend on `compact`'s own error
handling being exhaustive, which is the class of assumption LESSON-447 was
written about.

AC-14 tests this by stubbing the duty three ways (never returns, returns
garbage, entirely unrouted) and asserting the context is under budget after each.

**Honest limitation, found at TASK-065 and verified by hand.** The mutation this
ADR names as the one that must go red — making the gate
`if compaction.degraded { ctx.truncate_to_budget(); }` — leaves the **entire
suite green**. I applied it myself and confirmed: 1243 passed, 0 failed.

It is an **equivalent mutant at the loop**, not a test gap. Within the turn loop,
every reachable outcome is already under budget: an applied compaction is under
budget by construction (the apply step rejects over-budget candidates), and an
unpressured decline implies under budget. The one arm that would discriminate —
declining for too few blocks while still over budget — is unreachable *from the
loop*, which has already pushed a model block and a tool-result block before
compaction runs. That arm **is** reachable through the public
`compact_if_pressured` (verified at 6,211 B against a 4,000 B budget with
`degraded: false`) and is pinned there.

So the property holds, but **at the loop it currently rests on an equivalence
argument rather than on a discriminating test** — it is true because of an
incidental fact about how many blocks the loop pushes, not because the gate is
structurally unconditional. That is precisely the "almost-true invariant a later
change builds on" shape of BUG-157, and the two are coupled: **BUG-157's fix
changes when the loop appends blocks, which may make this mutation catchable.
Re-run it as part of that fix.**

Recorded rather than papered over, per LESSON-485 — a green mutation is a fact
about the tests, and an equivalent-mutant claim is only worth as much as the
reachability argument behind it.

## ADR-5: `shell` decides on the **raw** output, before truncation

**Decision.** The BR-4b trigger — `exit_code != 0 || raw_len > MAX_OUTPUT_CHARS`
— is evaluated on the raw stdout+stderr, upstream of `render_output()`'s
truncation at `tools/shell.rs:241`.

**Why.** `MAX_OUTPUT_CHARS` (8,000, `tools/shell.rs:36`) is what makes the output
unreadable in the first place. Evaluating the size arm *after* truncation would
compare a post-truncation length against the cap that produced it, so the
oversize arm could never fire — a guard condition that disables itself
(LESSON-443). The raw length must be captured before the cap is applied.

## ADR-6: `session_titled` is a new protocol event; `SessionSummary.title` already exists

**Decision.** Add `Event::SessionTitled { session_id, title }` to
`teton-protocol/src/events.rs`. Do **not** add a new title field to session
state — `SessionSummary.title: Option<String>` already exists at
`teton-protocol/src/methods.rs:77`.

**Why.** The spec's Entities table lists `Session.title` as **new**; the code
says otherwise. The field is already on the wire type and is simply never
populated. So this REQ populates an existing field and adds the event that
announces it, which is strictly less change than the spec assumed. The payload
carries plain types only, so `teton-protocol` gains no `teton-core` dependency —
the layering rule holds, and is now pinned by
`the_protocol_crate_depends_on_no_other_teton_crate`.

**Amendment (found during TASK-059).** This ADR originally specified the payload
as `SessionTitled { session_id, title }`. That shape is **unrepresentable**:
`Event` is internally tagged (`#[serde(tag = "event")]`) and flattened into
`EventEnvelope`, which already carries `session_id`. A payload field of the same
name emits the key twice and fails to deserialize —
`Error("duplicate field 'session_id'", line: 1, column: 64)`, observed, not
reasoned about.

The shipped payload is `SessionTitled { title }`, with the session named by
`EventEnvelope.session_id` exactly as `route_decided`, `privacy_block`, and
`phase_transition` already do. **The wire object is unchanged from what this ADR
specified** — `{"session_id":…,"seq":…,"event":"session_titled","title":…}` —
because the envelope assembles it. So the deviation is representational, not
contractual, and `session_titled_round_trips_under_its_wire_name` asserts the
full envelope shape including `session_id` to keep it that way.

`CostRecord` carries its own `session_id` only because `cost_recorded` *nests*
its payload under a `record` key rather than flattening; it is not a
counter-example.

**Consequence for TASK-062**: the emitter must scope the envelope with
`Some(session_id)`, because the payload no longer self-identifies. A `None` there
produces an event nobody can attribute.

`title` is `reflex`-tier and therefore local (REQ-558: `reflex` never inherits
`default_provider`), so its duty has no remote impl at all. It still routes
through the shared seam so that BR-2's `route_decided` and BR-3's fallback come
for free.

## AC-8's testable boundary

AC-8 asks for an assertion that one seam serves all five duties. Stated
precisely, the test asserts:

1. Exactly one `impl` of the route enum and exactly one `Duty` trait definition.
2. Exactly one local-duty impl and exactly one remote-duty impl (generic over
   the prompt/contract they carry, not over the category).
3. Exactly one call to `Egress::scoped(` on the duty path.
4. Exactly one ceiling-enforcement site.
5. Per-category source is limited to: a `router.resolve(Category::X)` line, an
   output-contract constant, and a prompt builder.

Anything beyond (5) appearing per-category is the regression AC-8 catches.

## Applicable lessons

- **LESSON-447** — a best-effort step guarding an invariant must enforce it by
  degraded means on failure. Drives BR-3 and ADR-4.
- **LESSON-443** — never condition a guard on the absence of the thing it
  guards. Drives ADR-5.
- **LESSON-485** — a fixture that cannot reach the discriminating state is not a
  test. Every egress and taint test needs its non-vacuity pair (AC-4).
- **LESSON-483** — mutate the inner fallback too: `compact`'s chain is
  duty → parse → `truncate_to_budget`, so each link needs its own mutation.
- **LESSON-484** — enforce the rule where the decision is made. The ceiling is
  enforced in the duty impl, not at each call site.
- **LESSON-432** — provenance comes from files, not argument names. ADR-2's
  `&Provenance` signature.

## ADR-8: `route_decided` is emitted when a duty **performs**, not when it resolves

**Decision.** `DutyRoute::Serves` carries the `RouteDecided` payload, and the
shared seam publishes it at the moment `perform` is actually invoked — once per
invocation. Emission does **not** happen in `resolve_duty()`.

**Why — this corrects an error in the original ADR set.** The first draft of
TASK-058 specified emission from the shared *resolver*. That was wrong, and
implementing it turned three tests red, which is how it was caught:

`digest_route()` is called unconditionally once per turn attempt, whether or not
any tool result ever crosses the summarization threshold. Emitting at resolution
therefore announces a routing decision for a model call that usually never
happens. BR-2 exists to make a new **egress path** visible; a path that never
fires produced no egress, so a resolution-time event observes a *resolution*, not
an egress. It also scales badly in exactly the direction this REQ moves: with all
five duties resolved eagerly, every turn would carry five spurious
`route_decided` events for calls that mostly never occur.

The decisive evidence was the third failure. `routing_categories::
a_tainted_session_stays_local_and_the_pre_taint_turn_proves_it_would_not_have`
is a REQ-544 privacy test whose premise is that a category-less `route_decided`
naming `local` **is** the taint pin. A resolution-time digest event violates that
premise. Emit-at-perform left the test green untouched at TASK-058.

**Amended at TASK-062.** "Left it green untouched" was true of `digest` and did
not survive `title`. `title` performs on the **first turn of every session**, so
it contributes a legitimate local announcement naming its own category before any
taint occurs — "names `local`" simply stopped being the same set as "is the pin".
The test's premise needed splitting, into two halves neither of which is vacuous:
the pin's category-less announcement must really be present (no tier, non-empty
reason), **and** nothing local may announce the category the pin overrode.

The load-bearing privacy property is unchanged and was re-verified by hand:
removing the turn's taint pin still turns the test red, and it fails at the
*captured-route* assertion (line 176 — the post-taint turn reaches `frontier`
again) before ever reaching the rewritten event-shape checks. The
`assert_no_boundary_bytes()` claim was not touched at all.

The general point for the duties still to come: **wiring a duty that fires
unconditionally changes what every event-shape assertion elsewhere means.**
`triage` and `shell` escaped this only because they do not fire in these
fixtures. `compact` (TASK-063) fires under context pressure, so check the same
class of assertion before assuming green.

**On the apparent cost.** Moving emission into the seam was raised as putting "an
emission concern inside the seam". That is where it belongs: the seam is the one
place all five duties share, so a single emission site there is precisely BR-6's
intent — strictly better than five call sites each remembering to emit.

**Testing consequence.** The positive test (a performed duty announces its route)
is not sufficient on its own. It must be paired with the negative — a turn where
the duty resolves but never performs emits **no** `route_decided` for that
category. Without the negative, nothing distinguishes this design from the one it
replaced (LESSON-485).

## ADR-9: Never write the literal `router.resolve(Category::X)` in prose

**Decision.** The spelling `router.<method>(Category::X)` must not appear in doc
comments, module docs, or any non-code text inside `crates/tetond/src/`.

**Why.** `call_sites.rs` derives the call-site marker by scanning production
source as **text**, not as parsed Rust. A doc comment containing that spelling
registers as a call site and turns
`the_unreached_marker_matches_the_daemons_actual_call_sites` red before the
described code exists. This was hit during TASK-058 — the marker test failed on
a comment. Refer to it descriptively in prose ("resolve the `digest` category")
and keep the literal spelling to actual call sites only. The next four duty tasks
all write similar documentation and would each rediscover this.

## ADR-10: Tool-owned duties hang off an async `Tool::refine`, not off `Tool::run`

**Decision.** `Tool` gains an async provided method:

```rust
async fn refine(&self, args: &Value, request: &str,
                duties: &ToolDuties<'_>, outcome: ToolOutcome) -> RefinedOutcome
```

Default is identity. Only the tool that owns a duty overrides it. The turn loop
calls it for **every** tool result. `ToolDuties` carries the resolved routes down
from the runtime, one field per tool-owned duty. `RefinedOutcome { outcome,
duty_error }` puts the degradation on the value, mirroring `SummarizeOutcome`.

**Why — TASK-060 and TASK-061's stated call sites are unreachable as written.**
Both task files say to call the duty inside `GrepTool::run` / `ShellTool::run`.
`Tool::run` is **synchronous**, and `ToolRegistry::dispatch` is invoked directly
from the async turn loop (`turn_loop.rs:525`), so a `block_on` there panics with
*"Cannot start a runtime from within a runtime"* on the exact path it runs on.
The MCP tool's `block_in_place` + `Handle::block_on` bridge (`tools/mcp.rs:174`)
is not an escape either: it parks a runtime worker for the length of an
inference — precisely what `LocalDuty::perform`'s `spawn_blocking` exists to
avoid — and it panics outright on the current-thread runtime `#[tokio::test]`
defaults to.

**Why not `if name == "grep"` in the turn loop.** That is verbatim what BR-1
forbids ("no tool name may assign one") and what AC-10 asserts against. With
`refine`, the category is never derived from a name: `ToolRegistry::refine` uses
the name only for **instance lookup** — the same dispatch that already routes
`run` — and the `GrepTool` impl reaches `duties.triage`, a route resolved in
`runtime.rs` from a literal category. Polymorphic dispatch, not a name→category
map.

**TASK-061 copies this exactly**: `ShellTool` overrides `refine`, `ToolDuties`
gains a `shell` field. No new parameter on `run_session_turn_with_source`, no new
route type. TASK-062 (`title`) and TASK-063 (`compact`) are **not** tool-owned
and do not use this seam — they hang off session creation and the context
manager respectively.

## ADR-11: A duty may decline to run, and the threshold is a named constant

**Decision.** `triage` introduces `TRIAGE_MIN_MATCHES`: ranking a single match is
a model call bought for nothing, so below the threshold the duty is not invoked
at all. The threshold is a **public named constant with a zero-call test**, never
an inline literal.

**Why record it.** This was not in the spec — it is an addition made during
TASK-060, and it is disclosed rather than smuggled. It is also the same shape as
BR-4b's resolved `shell` trigger (fire on failure or oversize, not on every
result), so the pattern is consistent rather than ad hoc: **a duty that cannot
add value does not run, and the condition under which it declines is legible and
testable.** `digest` already had this shape via its size threshold.

The rule for the remaining duties: if a duty declines under some condition, that
condition is a named constant and a test asserts the zero-call case. A hidden
threshold is a cost surprise.

## ADR-12: A compaction summary inherits the provenance of the blocks it replaces

**Decision.** `compact` answers with **both** a `FORGET:` block list and a
`SUMMARY:` paragraph that replaces them, and the inserted summary block carries
the **merged `ToolProvenance` of every block it elides** — `Unknown` if any
elided block was `Unknown`, otherwise the union of their sources.

**Why this is the most safety-critical decision in the REQ.** Without the
inherited provenance, compaction is a **laundering path**: a summary of a
`local-only` file read would re-enter the context as ordinary model prose with
clean provenance, and the next remote turn would send it. The boundary would hold
on the original read and leak on the summary of it. This is BUG-156's exact
shape — a path that re-derives its target instead of carrying it — and it is
worse here, because the laundering is silent and permanent: the original blocks
are gone.

Verified by mutation: replacing the merged provenance with an empty source set
turns `a_compaction_inherits_the_provenance_of_what_it_replaces` and
`a_compaction_of_unknown_provenance_stays_unknown` red.

**Why summary-plus-indices rather than indices alone.** Two constraints pulled
apart: "decide which blocks to forget" suggests an index list (output measured in
bytes), while BR-8's own wording gives `compact` the loosest ceiling of the five
because "a compaction is a conversation". Answering with both resolves it, and
buys two things beyond consistency:

1. **The over-budget rejection stops being near-vacuous.** An index list is
   almost incapable of busting a budget, so a test asserting "an over-budget
   response is rejected" would pass for want of a way to fail. A replacement
   paragraph has real size and can genuinely fail to fit, which makes
   `an_over_budget_compaction_is_rejected_rather_than_rescued` a live check.
2. **"Does not drop the blocks it managed to parse" becomes load-bearing in two
   independent ways** rather than one.

The cost — model prose entering history — is mitigated by putting the
replacement through `summarize_if_large`'s control-token cut, and by ADR-12's
provenance inheritance above.

## ADR-7: The four duty tasks are chained, not parallel

**Decision.** TASK-060 → 061 → 062 → 063 run in sequence, not concurrently.

**Why.** Every duty task modifies the same three files: `runtime.rs` (its
resolver), `call_sites.rs` (its `has_call_site()` arm), and `harness/mod.rs` (its
module declaration). The dependency edges encode a real conflict, not process
ceremony.

This is recorded because the opposite call was made during REQ-558 and was
wrong: two tasks with disjoint *primary* files were dispatched in parallel into a
shared worktree, and neither agent could run the workspace test suite while the
other was mid-edit — disjoint writes are not disjoint builds. TASK-059 is the one
genuinely parallel-safe task here, because it touches only `teton-protocol`.

## Risk register

| Risk | Mitigation |
|---|---|
| `compact` silently corrupting later turns | ADR-4's unconditional hard gate; AC-14's three stub failures |
| A duty consuming a `ScriptedFileEngine` block and desynchronising fixtures | BR-10: each duty ships its contract constant + recognition arm **in its own task**, with its own AC-12 test — not deferred to the test task |
| The derived-marker test failing confusingly | ADR-3 keeps the literal where the scan looks |
| `shell` firing on every command | ADR-5 + AC-13's negative case (exit 0, small output ⇒ zero calls) |
| Migrating `digest` regressing REQ-558 behaviour | `digest`'s existing tests are not edited; they must pass unmodified against the new seam |
