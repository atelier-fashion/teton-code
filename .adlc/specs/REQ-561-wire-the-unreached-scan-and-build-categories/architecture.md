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
**before** the existing `ctx.truncate_to_budget()` at `harness/context.rs:618`.
That existing line is **not modified, not wrapped, and not made conditional**.

**Why.** This is BR-4a made structural. Because `truncate_to_budget()` still runs
unconditionally afterward, a `compact` duty that hangs, returns garbage, returns
an over-budget result, is never routed, or panics cannot produce an over-budget
context — the thing enforcing the budget was never the duty. The alternative
(replacing the call) makes every safety property depend on `compact`'s own error
handling being exhaustive, which is the class of assumption LESSON-447 was
written about.

AC-14 tests this by stubbing the duty three ways (never returns, returns
garbage, entirely unrouted) and asserting the context is under budget after each.
The mutation that must go red: making the 100% gate conditional on the duty
having failed.

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
is two plain types (`SessionId`, `String`), so `teton-protocol` gains no
`teton-core` dependency — the layering rule holds.

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
premise. Emit-at-perform leaves the test green untouched — the right outcome
arrived at for the right reason, not a lucky one.

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
