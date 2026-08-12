---
id: TASK-116
title: "`teton effort` and `/effort` through one resolver and one renderer; `teton cost` reports the thinking split"
status: complete
parent: REQ-559
created: 2026-08-11
updated: 2026-08-11
dependencies: [TASK-114, TASK-115]
---

## Description

The user-facing surfaces. BR-9 requires `teton effort` and `/effort` to render
through **one** resolution function shared with the router — two surfaces
describing one setting must not drift (LESSON-456, REQ-555 BR-4). BR-11 requires
`teton cost` to say "unreported" where the thinking split is unknown, because a
`0` standing in for "the provider didn't tell us" is displaying an estimate as
actual (REQ-544 BR-2).

This REQ owns the `/effort` `COMMANDS` row, its bare-argument read path, and its
`/help` entry (BR-9). It does **not** touch the status line or add `/permissions`
— those are REQ-560's.

## Files to Create/Modify

- `crates/teton-protocol/src/methods.rs` — `EffortView` + `ConfigSnapshot.effort`;
  `ConfigUpdate::SetEffort(EffortLevel)` (:997)
- `crates/tetond/src/runtime.rs` — build the `EffortView` via `resolve_effort`;
  handle `SetEffort` (persist + set the session override)
- `crates/teton/src/effort_ui.rs` — **new**, the one render function
- `crates/teton/src/main.rs` — the `Effort { level: Option<...> }` subcommand (:83)
- `crates/teton/src/slash.rs` — the `/effort` `COMMANDS` row (:173) + handler
- `crates/teton/src/cost_ui.rs` — the thinking split / "unreported" rendering

## Acceptance Criteria

- [ ] `ConfigSnapshot.effort: Option<EffortView>` where `EffortView` carries the
      current level plus one row per registered provider: `provider_id` and its
      `ResolvedEffort`. Additive with a default; no new RPC (`config/get` already
      carries `ConfigSnapshot`), as the spec requires.
- [ ] The daemon builds every `EffortView` row by calling **`resolve_effort`** —
      the same function the router calls (ADR-G).
- [ ] **AC-8 / shared-resolver test**: assert the rendered rows are produced by
      `resolve_effort`, **not by string coincidence** — e.g. drive the view and
      the router from one provider set and assert the router's per-call
      `ResolvedEffort` equals the view's row for that provider, for every
      provider and every canonical level. A test that only compares rendered
      strings does not discharge AC-8.
- [ ] `ConfigUpdate::SetEffort(EffortLevel)` persists to `Config.effort` **and**
      sets the session override (ADR-I). It is a user-only action, consistent
      with the other `ConfigUpdate` variants (spec Permissions).
- [ ] **AC-7 / persistence**: `/effort low`, then a full daemon restart and a
      fresh session, shows `low` (BR-8). This is the deliberate asymmetry with
      REQ-560's session-scoped permission level.
- [ ] `teton effort` and `/effort` with **no argument** print the current level
      and one line per registered provider (BR-9), rendered by the single
      `effort_ui` function both call. A test asserts the two surfaces produce
      byte-identical bodies for one snapshot.
- [ ] Rendering per `ResolvedEffort` variant, and it must not display a level the
      provider is not receiving (BR-6):
      - `Effort(level)` → the clamped level, marked as clamped when it differs
        from the requested level
      - `ThinkingFlag` → "thinking on (this provider takes a flag, not a level)"
      - `Omit(ShapeNone)` → **"not applicable"** (AC-5's exact requirement for the
        local provider — not a level)
      - `Omit(EmptyLadder)` → "no supported level (declared ladder is empty)"
      - `Omit(RefusedThisSession)` → "effort refused this session — sending none"
        (ADR-F's visibility condition)
- [ ] `/effort <level>` accepts the five canonical spellings and rejects anything
      else with one line and no RPC, matching the `Args::Required` rejection path
      the table already implements.
- [ ] The `/effort` row appears in `/help`, generated from `COMMANDS` (REQ-555
      BR-7) — no separate `/help` edit. The existing "every table row is
      reachable from parsed input" bidirectional test (slash.rs:1073/1116) covers
      the new row automatically; confirm it does rather than assuming it.
- [ ] **AC-9 / cost rendering**: `teton cost` shows the reasoning-token split
      where known and the literal word **"unreported"** where the value is `None`
      — never `0`, and never a computed percentage of an unknown numerator.

## Technical Notes

**`/effort` must not be added or aliased by REQ-560.** If a merge conflict
appears in the `COMMANDS` array against REQ-560's `/permissions` row, this REQ's
`/effort` row is authoritative and REQ-560's `/permissions` row is additive
beside it — both rows coexist; neither replaces the other (BR-9, spec Out of
Scope).

**Do not add a second render path for the status line.** REQ-560 renders the
effort *value* in its status line and will consume `ConfigSnapshot.effort`. Give
it a clean value to read; do not build the status line here.

**One resolver, two surfaces, one renderer — and the test must prove the
resolver, not the renderer.** The failure mode LESSON-456 describes is two
components classifying the same state differently with nothing observing the
disagreement. A golden-string test on the rendered output would pass while the
router and the surface diverge, because the surface would be self-consistently
wrong. Assert on `ResolvedEffort` values.

**The CLI is a thin client.** `effort_ui` must not clamp, must not consult a
ladder, and must not know `ProviderKind` defaults. It formats an `EffortView` the
daemon already resolved. Any policy in `crates/teton/` is a layering violation
(architecture.md: clients are "thin, stateless renderers").
