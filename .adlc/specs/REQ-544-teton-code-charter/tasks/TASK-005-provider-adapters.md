---
id: TASK-005
title: "teton-providers: adapter trait, Anthropic + OpenAI-compatible"
status: complete
parent: REQ-544
created: 2026-07-17
updated: 2026-07-17
dependencies: [TASK-003]
---

## Description

Provider abstraction (BR-6): a `Provider` trait with streaming chat +
tool-call support, an Anthropic Messages adapter, an OpenAI-compatible adapter
(covers DeepSeek, Kimi, Ollama, vLLM), capability profiles, and
failure-classified fallback (AC-7 groundwork).

## Files to Create/Modify

- `crates/teton-providers/src/lib.rs` — `Provider` trait: stream_turn(request, transport) → stream of deltas/tool-calls; capabilities() → CapabilityProfile
- `crates/teton-providers/src/transport.rs` — `Transport` trait the egress module implements (D-2): adapters receive it, never construct HTTP clients themselves
- `crates/teton-providers/src/anthropic.rs` — Messages API adapter: streaming, tool use, token usage extraction
- `crates/teton-providers/src/openai_compat.rs` — chat/completions adapter: streaming, tool_calls, usage; endpoint + model configurable
- `crates/teton-providers/src/capability.rs` — CapabilityProfile {tool_call_reliability tier, parallel_calls, max_context}; degradation profile mapping (BR-6)
- `crates/teton-providers/src/failure.rs` — failure classification (timeout, 4xx, 5xx, malformed tool call) → retry/fallback/degrade decision; emits data for `provider_degraded`

## Acceptance Criteria

- [x] Both adapters pass a shared conformance test suite against recorded/mock transports (streaming deltas, tool-call assembly, usage extraction)
- [x] Adapters cannot perform I/O without a `Transport` (compile-time: no http client deps in this crate) (informed by D-2)
- [x] Malformed tool-call JSON from a provider is classified and surfaced, never panics
- [x] Fallback decision logic unit-tested per failure class (AC-7 backend)
- [x] Token usage populated on every completed turn (BR-2 dependency)

## Technical Notes

The `Transport` indirection is THE load-bearing decision (D-2) — it is what
makes BR-1/BR-2 enforceable. Resist any convenience shortcut that gives an
adapter a raw reqwest client. Tool-call format normalization to one internal
shape happens here, not in the harness.
