---
id: TASK-152
title: "Protocol: ProviderSetup{Plan,Preview,Commit} types, ProviderRecipeEntry, two events, error code"
status: complete
parent: REQ-579
created: 2026-08-15
updated: 2026-08-15
dependencies: []
---

## Description

Add the wire types for the `provider/setup_*` trio to `teton-protocol`, mirroring the `WebSetup*` declarations byte-for-byte in style: `RpcMethod` impls binding each params type to its method name, result types, the shared candidate/entry structs, two events, and the error code. Purely additive; no daemon or CLI logic. This is the foundation both the daemon (TASK-153/154) and the CLI (TASK-155) build against in parallel.

**Covers:** AC-5 (types the contract test pins), AC-10/AC-11 (wire codes and events the daemon returns)

## Files to Create/Modify

- `crates/teton-protocol/src/methods.rs` — add `ProviderSetupPlanParams/Result`, `ProviderSetupPreviewParams/Result`, `ProviderSetupCommitParams/Result`, `ProviderSetupCandidate`, `ProviderRecipeEntry`, `ExistingProvider`, `TierSummary`, `TierBinding`; `impl RpcMethod` with `METHOD = "provider/setup_plan" | "provider/setup_preview" | "provider/setup_commit"`; add `PROVIDER_SETUP_INVALID` beside `WEB_SETUP_INVALID` in `error_code`; serde round-trip tests beside the WebSetup ones
- `crates/teton-protocol/src/events.rs` — add `Event::ProviderSetupCompleted { session_id, provider_id, kind, model, bindings }` and `Event::ProviderSetupRejected { session_id, method }` with wire names `provider_setup_completed` / `provider_setup_rejected_nonuser`; add to the wire-name match and any exhaustive-name test
- `crates/teton-protocol/src/lib.rs` — re-export the new types where the WebSetup ones are re-exported

## Acceptance Criteria

- [ ] Every new params type implements `RpcMethod` with the exact method string from architecture.md; a test asserts the three strings
- [ ] `ProviderRecipeEntry` has the seven fields of `tetond::provider_recipes::ProviderRecipe` with identical names and types (`id_suggestion, label, guide_spelling, kind, endpoint, example_model, notes`)
- [ ] `ProviderSetupCandidate.key_ref` is a `String` documented as "a keychain reference, never a key value"; `ProviderSetupCommitParams.expect_digest` is `Option<String>` like `WebSetupCommitParams`
- [ ] Both events serialize with the wire names above; the events wire-name test (if one enumerates names) includes them
- [ ] `cargo test -p teton-protocol` green; `cargo clippy -p teton-protocol --all-targets` clean

## Technical Notes

Copy the `WebSetup*` declarations (search `WebSetupPlanParams` in methods.rs ~L2200) and rename; keep field ordering `session_id` first. `kind` is the existing `ProviderKind`. Do not add `Default` impls that would make an empty candidate constructible in tests without a `key_ref` — a missing reference must be a compile-visible omission. Events: copy `WebSetupCompleted` (events.rs ~L1572) — carry no key, no endpoint userinfo. Look for a test that asserts every `Event` variant has a wire name and add the two rows if it is table-driven.
