---
id: TASK-111
title: "Declare reasoning_shape and effort_ladder on ProviderCapabilities; add the persisted Config.effort key"
status: complete
parent: REQ-559
created: 2026-08-11
updated: 2026-08-11
dependencies: [TASK-110]
---

## Description

Wire TASK-110's vocabulary into the two places configuration lives: the
per-provider capability declaration (`ProviderCapabilities`, already carried in
config via `ModelProvider.capabilities` with `#[serde(default)]`), and the global
persisted effort setting (`Config.effort`, BR-8).

Both additions are `serde`-additive: a pre-REQ config file must load unchanged
and behave exactly as ADR-E's per-kind defaults describe.

## Files to Create/Modify

- `crates/teton-core/src/entities.rs` — add `reasoning_shape: Option<ReasoningShape>`
  and `effort_ladder: Option<EffortLadder>` to `ProviderCapabilities` (:59)
- `crates/teton-core/src/config.rs` — add `effort: EffortLevel` to `Config`
- `crates/teton-providers/src/capability.rs` — mirror both fields on
  `CapabilityProfile` and carry them through `from_core` / `to_core`

## Acceptance Criteria

- [ ] `ProviderCapabilities` carries `reasoning_shape: Option<ReasoningShape>`
      and `effort_ladder: Option<EffortLadder>`, both
      `#[serde(default, skip_serializing_if = "Option::is_none")]` — matching the
      existing convention on `ModelProvider::endpoint` / `auth_ref`.
- [ ] **`ProviderCapabilities` and `CapabilityProfile` still derive `Copy`**, and
      a test constructs and copies both. This is the ADR-C guarantee; if it fails,
      the ladder was implemented as a `Vec` and ~30 `ProviderCapabilities::default()`
      sites in `runtime.rs` are about to break.
- [ ] `ProviderCapabilities::default()` yields `None` for both new fields, so
      every existing `::default()` call site keeps its current meaning and the
      per-kind default (ADR-E) is what actually applies.
- [ ] A pre-REQ config TOML — providers with a `[provider.capabilities]` table
      carrying only `tool_call_tier` / `parallel_calls` / `max_context`, and no
      top-level `effort` key — **deserializes successfully** and yields
      `effort = High` and `None` for both capability fields. Asserted against a
      fixture written in the old shape.
- [ ] A config declaring `reasoning_shape = "thinking_flag_only"` and
      `effort_ladder = ["low", "high", "xhigh", "max"]` under
      `[provider.capabilities]` round-trips through load → serialize → load
      unchanged.
- [ ] `Config.effort` is serialized **unconditionally** (no `skip_serializing_if`),
      for the same reason as `Config::judgment_default` and
      `LocalModelConfig::auto_accept`: a declared default that disappears from a
      written-out config whenever it holds its default value is the hidden
      constant that configuration-visibility rules out.
- [ ] `CapabilityProfile::from_core` / `to_core` carry both new fields, and the
      existing `core_roundtrip_is_lossless` test is extended to cover them —
      a lossy projection here would silently drop a user's declared ladder
      between the config and the adapter.
- [ ] `Config::validate()` is **not** extended with any effort rule, and a test
      pins that a config declaring an empty `effort_ladder = []` **loads and
      validates**. See ADR-E: `validate` is fail-closed and gates daemon startup,
      and an effort misconfiguration must not refuse to start the daemon or mark
      the provider unusable. An empty ladder resolves to `Omit(EmptyLadder)` and
      is reported on the surface instead.

## Technical Notes

**Blast radius is deliberately near-zero.** `ProviderCapabilities` derives
`Default` and both new fields are `Option`, so no struct-literal site breaks.
Contrast TASK-112, where breaking the literal sites is the whole point.

**`ProviderKind` is not stored on `ProviderCapabilities`.** The per-kind default
lookup happens at `resolve_effort` call sites, which have the `ModelProvider` in
hand. Do not add a `kind` field here to make the lookup local — it would
duplicate `ModelProvider::kind` and create exactly the two-sources-of-one-fact
drift LESSON-456 is about.

**Serialization order in `Config`.** `config.rs` has an ordering constraint noted
around the `lifetime` field: TOML requires scalar keys before array-of-table
fields. `effort` is a scalar, so it must be declared **before** `providers` in
the struct. Placing it after will produce a config file that cannot be re-read.
