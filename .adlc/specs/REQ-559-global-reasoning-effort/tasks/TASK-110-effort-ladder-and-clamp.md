---
id: TASK-110
title: "Canonical effort ladder, bitset EffortLadder, and the pure down-then-up clamp"
status: complete
parent: REQ-559
created: 2026-08-11
updated: 2026-08-11
dependencies: []
---

## Description

The pure core of the REQ: a new `teton-core::effort` module holding the
canonical five-level ladder (BR-3), the `EffortLadder` bitset (ADR-C), the
`ReasoningShape` declaration vocabulary (BR-4), the `ResolvedEffort` /
`EffortOmission` return types (ADR-A), the per-kind default tables (ADR-E), and
the `resolve_effort` function every other task consumes (ADR-G).

No I/O, no daemon dependency, no provider dependency. Everything here is a pure
function over plain data, which is what makes AC-3 and AC-12 unit tests rather
than integration tests. This task blocks every other task except TASK-115.

## Files to Create/Modify

- `crates/teton-core/src/effort.rs` — **new module**, everything below
- `crates/teton-core/src/lib.rs` — `pub mod effort;` and re-exports alongside the
  existing `ProviderCapabilities` / `ToolCallTier` exports

## Acceptance Criteria

- [ ] `EffortLevel` is `enum { Low, Medium, High, Xhigh, Max }`, `Copy + Ord`,
      serde `rename_all = "snake_case"`, with `Default = High` (BR-1: the absence
      of a user setting resolves to the declared default, never to an absent
      field). `Ord` follows the ladder order, asserted by a test — the derive is
      correct only because the variants are declared in ladder order, and a
      future reorder must go red.
- [ ] `ReasoningShape` is `enum { EffortOnly, ThinkingFlagOnly, None }`,
      `Copy`, serde `rename_all = "snake_case"` (wire: `effort_only`,
      `thinking_flag_only`, `none`).
- [ ] `EffortLadder` is a `Copy + Eq` newtype over `u8` with `from_levels(&[EffortLevel])`,
      `contains`, `is_empty`, `levels() -> impl Iterator<Item = EffortLevel>`
      (ascending), and `EffortLadder::EMPTY`. **`Copy` is load-bearing** — see
      ADR-C; a `Vec` here breaks `Copy` on `CapabilityProfile` and ripples across
      ~30 `ProviderCapabilities::default()` sites.
- [ ] `EffortLadder` serializes as a `Vec<EffortLevel>` and deserializes from
      one, in **ascending ladder order regardless of input order**, with
      duplicates collapsed. Round-trip tested (`levels() → from_levels()`), and
      pinned against a literal TOML/JSON fixture in both directions — a
      hand-written serde pair is exactly where silent drift lives (ADR-C).
- [ ] `EffortLadder::clamp(EffortLevel) -> Option<EffortLevel>` implements BR-5:
      nearest supported at-or-below, else nearest supported above, `None` only
      for an empty ladder.
- [ ] **AC-3 table**: table-driven test over all five canonical levels × at least
      three ladders, explicitly including the spec's three named cases —
      `xhigh` against `{low, high, max}` → `high`; `medium` against the same →
      `low`; `low` against a ladder whose floor is `high` → `high`.
- [ ] `ResolvedEffort` is `enum { Effort(EffortLevel), ThinkingFlag, Omit(EffortOmission) }`
      and `EffortOmission` is `enum { ShapeNone, EmptyLadder, RefusedThisSession }`.
      **No variant carries two wire fields** (ADR-A) and **neither type implements
      `Default`** (ADR-B).
- [ ] `default_shape_for(ProviderKind)` and `default_ladder_for(ProviderKind)`
      implement ADR-E's table exactly: `Local` → `None` / empty;
      `OpenaiCompatible` and `Custom` → `EffortOnly` / `{low, high}`;
      `Anthropic` → `EffortOnly` / all five. A test asserts each row, and a
      separate test asserts `{low, high}` is a subset of every non-empty default
      ladder — the intersection property ADR-E's choice rests on.
- [ ] `resolve_effort(requested, kind, caps, refused_this_session) -> ResolvedEffort`
      resolves shape and ladder from `caps` where declared, else from the per-kind
      defaults, then:
      - `refused_this_session` → `Omit(RefusedThisSession)` (checked **first**,
        so a session refusal wins over any shape — ADR-F)
      - shape `None` → `Omit(ShapeNone)`
      - shape `ThinkingFlagOnly` → `ThinkingFlag`
      - shape `EffortOnly` → `clamp(requested)` mapped to `Effort(level)`, or
        `Omit(EmptyLadder)` when the ladder is empty
- [ ] A test asserts `resolve_effort` **never returns `Omit(ShapeNone)` for a
      remote kind with no declared shape** — the direct regression for the
      Kimi-defaults-to-`max` defect at the BYOM endpoint (BR-1, BR-4/OQ-2).

## Technical Notes

**Why `resolve_effort` takes `kind` separately from `caps`**: `ProviderCapabilities`
is embedded in `ModelProvider` and does not know its own kind. Passing both keeps
this function pure and avoids a `teton-core::effort` → `ModelProvider` dependency
that would make the module harder to test in isolation.

**`refused_this_session` is a bare `bool`, not an `Option`.** The caller owns the
session memo (TASK-113); this module must not hold state. Keeping it a parameter
is what makes AC-8's shared-resolver assertion possible — the surfaces pass
`false` for a hypothetical view and the router passes the live value, and both
reach the same function.

**Do not add a `minimal` rung.** It exists only on OpenAI, ladder members are
canonical levels by definition (BR-3), and adding it forces a per-provider
spelling table this design does not otherwise need — the canonical spellings are
the wire spellings on Anthropic, DeepSeek and Kimi. See ADR-E.

**LESSON-443 shape to avoid**: do not write the clamp as "if the ladder is the
full canonical set, skip clamping". That is a guard keyed on a condition that
stops holding the moment a provider declares a narrower ladder. Clamp
unconditionally; the full-ladder case is already an identity through the same
code path, and AC-12 requires that an identity clamp be detectable.
