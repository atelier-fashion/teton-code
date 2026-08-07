---
id: TASK-058
title: "The shared DutyRoute/Duty seam, with digest migrated onto it"
status: draft
parent: REQ-561
created: 2026-08-07
updated: 2026-08-07
dependencies: []
---

## Description

Build the shared duty seam (BR-6) and migrate `digest` onto it in the same task.
Migrating the one existing caller is what proves the seam is general rather than
a `digest`-shaped hole with new names — if `digest` needs a special case, the
seam is wrong and this task has failed.

Also retrofits `route_decided` emission for `digest` (BR-2), which becomes free
once resolution is shared.

This task is foundational: every other duty task depends on it. It must land
before any of them start.

## Files to Create/Modify

- `crates/tetond/src/harness/duty.rs` — **new**. `DutyRoute`, the `Duty` trait, `LocalDuty`, `RemoteDuty`, ceiling enforcement, egress scoping. The single home for all five concerns.
- `crates/tetond/src/harness/digest.rs` — collapse `DigestRoute`/`Digester`/`LocalDigester`/`RemoteDigester` into the shared seam; keep only the digest prompt builder and `SUMMARIZER_OUTPUT_CONTRACT`-adjacent bits. `tool_result_provenance()` becomes a caller-side helper.
- `crates/tetond/src/harness/mod.rs` — declare and export the `duty` module.
- `crates/tetond/src/runtime.rs` — add the shared `resolve_duty()` helper; rewrite `digest_route()` (line ~1853) as a thin wrapper that keeps its literal category-resolving call (line 1864) and delegates. **`route_decided` is emitted from the shared *perform* path, not from the resolver** — see ADR-8.
- `crates/tetond/src/harness/context.rs` — update `summarize_if_large()` (line ~689) to consume `DutyRoute` instead of `DigestRoute`. Its degraded-fallback behaviour must not change.

## Acceptance Criteria

- [ ] `DutyRoute` is a single non-generic type holding `Arc<dyn Duty>` (ADR-1). No `DigestRoute` type survives; no per-category route type is introduced.
- [ ] `Duty` exposes `category()`, `ceiling_bytes()`, and `async perform(&self, prompt: &str, provenance: &Provenance) -> Result<String, String>` (ADR-2). Note the `&Provenance` signature — **not** `&ToolProvenance`.
- [ ] Exactly one `Egress::scoped(` call exists on the duty path, and exactly one ceiling-enforcement site.
- [ ] `digest` routes through the seam and **every existing digest test passes unmodified**. A test edited to accommodate the refactor is a violation, not an accommodation — if a digest test needs changing, the seam changed behaviour and that is the bug.
- [ ] `digest` emits `route_decided` naming `Category::Digest`, a tier, a provider, and a non-empty reason **when the duty actually performs** (BR-2, part of AC-2, ADR-8). Paired with the negative: a turn where the duty resolves but never performs emits **no** digest `route_decided`. The negative is what pins the design — without it the test passes equally under emit-at-resolve (LESSON-485).
- [ ] `cargo test --workspace --no-fail-fast` is green.

## Technical Notes

Precedent to generalise, all verified: `DigestRoute` at `harness/digest.rs:113`,
`Digester` at `:91`, `DIGEST_OUTPUT_MAX_BYTES` at `:237`, `digest_route()` at
`runtime.rs:1853`, `Egress::scoped()` at `egress/mod.rs:345`,
`emit_route_decided` at `router.rs:638`.

**Keep the literal.** `runtime.rs:1864`'s `router.resolve(Category::Digest)` must
survive verbatim. `call_sites.rs:220` scans for exactly that spelling
(`router.<method>(` + a `Category::X` literal, methods
`["resolve","resolution_for","resolve_judgment"]`). Collapsing resolution into a
single `resolve_duty(category)` taking a variable makes the scan blind — see
ADR-3. The shared helper sits *behind* the literal, not in front of it.

**Ceiling enforcement** goes in the remote duty impl (LESSON-484: enforce where
the decision is made), reading `ceiling_bytes()` from the trait. Follow the
existing `RemoteDigester` pattern at `digest.rs:298-313` — drop `TextDelta`
past the cap, then truncate on a char boundary.

Do not add the four new categories in this task. This is the seam only.
