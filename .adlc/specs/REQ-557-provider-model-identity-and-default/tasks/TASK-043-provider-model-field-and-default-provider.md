---
id: TASK-043
title: "Declare ModelProvider.model and Config.default_provider, with validation and one-shot migration"
status: draft
parent: REQ-557
created: 2026-08-05
updated: 2026-08-05
dependencies: []
---

## Description

The foundational schema change. `ModelProvider` gains a declared `model`;
`Config` gains an explicit `default_provider`. Both are enforced in
`Config::validate()` rather than by the deserializer, and a one-shot migration
resolves `model` for pre-REQ configs.

This task is the whole of the `teton-core` change and blocks the other four.
It deliberately does **not** touch the router, the price table, or the CLI —
those consume this shape and are separate tasks.

## Files to Create/Modify

- `crates/teton-core/src/entities.rs` — add `model: Option<String>` to
  `ModelProvider` with `#[serde(default, skip_serializing_if = "Option::is_none")]`,
  matching the existing convention on `endpoint` / `auth_ref`
- `crates/teton-core/src/config.rs` — add `default_provider: Option<String>` to
  `Config` with `#[serde(default)]`; extend `validate()` (the loop at :269) with
  the two new rules; add the migration entry point and the legacy resolver
  parameter

## Acceptance Criteria

- [ ] `ModelProvider` carries `model: Option<String>` and a pre-REQ config TOML
      (no `model` key on any provider) **deserializes successfully** — asserted
      by a test that loads a fixture written in the old shape. This is the ADR-B
      guarantee that makes migration reachable at all.
- [ ] `Config::validate()` **accepts** a remote-kind provider whose `model` is
      `None` — see ADR-E. Making this a validation error refuses daemon startup
      (`runtime.rs:1532`), which both blocks migration on a pre-REQ config and
      contradicts BR-7's "the daemon starts with that provider unusable". A test
      pins that such a config **loads**.
- [ ] A separate, non-fatal **usability** pass reports every remote provider with
      `model: None` by id and marks it unusable. A config with one usable and one
      unusable provider loads, reports the unusable one, and leaves the usable one
      routable.
- [ ] `Config::validate()` rejects a `default_provider` naming an id absent from
      `providers`, with an error naming both the dangling id and the registered
      ids (AC-5).
- [ ] Two providers sharing `kind`, `endpoint`, and `auth_ref` but differing in
      `id` and `model` both validate — the BR-3 case (AC-1).
- [ ] Migration: given a config whose providers lack `model`, a one-shot pass
      resolves each via an injected legacy resolver, writes the value into the
      provider record, and returns the list of providers it could **not**
      resolve. Providers it cannot resolve keep `model: None` and are left for
      `validate()` to reject by id — the migration never guesses (BR-7).
- [ ] Migration is idempotent: running it twice on an already-migrated config is
      a no-op and reports nothing.
- [ ] Table-driven unit tests cover validation across the matrix
      (kind × model-present × default-provider-resolvable) per conventions.md's
      "router policy decisions are pure functions in teton-core" rule.

## Technical Notes

**The serde shape is load-bearing, not a style choice.** A bare
`model: String` makes every pre-REQ config fail to deserialize *before*
migration can run, which makes AC-6 unimplementable. See ADR-B.

**And the validation shape is load-bearing for the same reason, one layer down.**
ADR-B only got the config to *parse*. `Config::load` then calls `validate()`
(`config.rs:251`), and `load_config` turns any error into "Refusing to start"
(`runtime.rs:1532`). Putting the model requirement in `validate()` therefore
re-closes the door ADR-B opened. Read ADR-E before touching `validate()`.

**Blast radius: 9 struct-literal sites.** `ModelProvider` derives no `Default`
(`entities.rs:73`), so adding a field breaks every literal construction —
5 in `runtime.rs`, 3 in `config.rs`, 1 in `entities.rs`. The compiler finds them
all; this note is scoping, not a hazard.

**The legacy resolver is injected, not imported.** `teton-core` must not depend
on `tetond`'s price table. Define migration as taking a
`impl Fn(&str) -> Option<String>` (provider id → model); the daemon supplies the
price-table-backed closure in TASK-044/045's wiring. This keeps `teton-core`
I/O-free per conventions.md.

**LESSON-443 applies directly.** The migration's guard is `model.is_none()` —
a condition keyed on the absence of the very field this task adds. That is safe
here only because absence *is* the migration's subject. Whatever helper performs
the legacy provider-id→model lookup must be deleted in the same change that
stops needing it (TASK-045), or it survives as a live path that can re-derive a
model from the price table after ADR-A forbade exactly that.

**Do not remove `ModelPrice.provider_id` in this task** — the baseline label and
the migration's legacy lookup still use it. TASK-045 owns that decision.
