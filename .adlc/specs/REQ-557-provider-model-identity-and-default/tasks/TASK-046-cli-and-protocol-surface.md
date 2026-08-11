---
id: TASK-046
title: "Expose the model on the CLI and in the protocol projection"
status: complete
parent: REQ-557
created: 2026-08-05
updated: 2026-08-11
dependencies: [TASK-043]
---

## Description

Thread the declared model through the two user-facing surfaces: `teton provider
add --model <name>` and the `config/get` projection that `teton provider list`
and any attached client read.

## Files to Create/Modify

- `crates/teton/src/main.rs` — `ProviderAction::Add` (:271) gains a
  `--model` argument; `run_provider_add` (:1039) and
  `build_provider_registration` thread it into the `ProviderConfig` sent as
  `ConfigUpdate::RegisterProvider`; missing `--model` on a remote kind fails
  before any keychain read or RPC
- `crates/teton-protocol/src/methods.rs` — `ProviderConfig` gains
  `model: Option<String>`
- `crates/tetond/src/runtime.rs` — `snapshot_from_config` (:2785) projects
  `model`; the `RegisterProvider` handler (:2820) persists it
- `crates/teton/src/main.rs` — `run_provider_list` renders the model per
  provider

## Acceptance Criteria

- [x] `teton provider add opus --kind anthropic --model claude-opus-5` and
      `teton provider add sonnet --kind anthropic --model claude-sonnet-5` both
      succeed; `teton provider list` shows two providers with distinct models and
      the same kind; a third `add` reusing id `opus` fails (AC-1).
- [x] `teton provider add x --kind anthropic` with no `--model` exits non-zero
      naming `--model`, registers nothing, **and does not prompt for a
      credential** — the argument check precedes `read_secret` (AC-2).
- [x] A local-kind `provider add` still succeeds without `--model` — the local
      model is owned by the REQ-547 consent flow and is read, never set here.
- [x] `config/get`'s `ProviderConfig` carries the model; a round-trip test
      asserts a registered model survives daemon persistence and reappears in the
      projection.
- [x] The existing CLI arg-parsing tests (`main.rs:1437`) are updated for the new
      argument and continue to assert the other fields unchanged.

## Technical Notes

**Fail before the credential prompt.** `run_provider_add` currently calls
`read_secret(id)` (:1051) *before* building the registration. A missing
`--model` must be rejected before that, or the user types a secret into a
command that was always going to fail. This is why AC-2 pins "does not prompt"
rather than just "exits non-zero".

**The daemon owns the write.** The CLI does not persist config — it sends
`ConfigUpdate::RegisterProvider` and the daemon writes (`runtime.rs:2820`). This
task changes what is sent and projected; it does not add a client-side write
path.

**The local kind keeps `model: None` from this surface.** REQ-547's consent flow
owns the local model selection. Whether the projection *mirrors* that selection
into `ModelProvider.model` for display is REQ-557 OQ-4 and is **out of scope
here** — do not mirror it. A mirrored copy is a second source of a fact the
consent flow owns, which is the drift LESSON-456 warns about.

**Protocol field is `Option`,** matching the entity — a client attached to a
daemon mid-migration may legitimately see a provider with no model yet.
