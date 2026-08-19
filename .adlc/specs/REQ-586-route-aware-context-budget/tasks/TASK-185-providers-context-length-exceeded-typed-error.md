---
id: TASK-185
title: "teton-providers: ProviderError::ContextLengthExceeded — class-less, narrowly sniffed from the 400 body, conformance-tested"
status: draft
parent: REQ-586
created: 2026-08-19
updated: 2026-08-19
dependencies: []
repo: teton-code
---

## Description

A provider answering "context length exceeded" is a typed outcome, not a
generic client error (BR-2, ADR-8): no retry, no failover, no health change.
Mirror `EffortRefused` exactly (lib.rs:372-445).

## Files to Create/Modify

- `crates/teton-providers/src/lib.rs` — `ProviderError::ContextLengthExceeded { provider_id: ProviderId }` (doc: "no `FailureClass`: the request is too big for the window, which retrying or failing over cannot fix; the harness reports it"); `failure_class()` (L424-445) returns `None` for it; `classify_client_error` (L291-308): read the head for **every** 400 — including the `ResolvedEffort::Omit` retry path, not only inside the `if let ResolvedEffort::Effort` arm — and after the effort sniff run `body_names_context_length(head)` — **narrow**, exact vendor spellings only: OpenAI-compatible `"code":"context_length_exceeded"` or `maximum context length`; Anthropic `prompt is too long`; nothing else (the `body_names_the_effort_field` posture, L250-259); tests beside L548-564: each spelling → the variant; a 400 with neither spelling → today's path; `failure_class()` is `None`.
- `crates/teton-providers/src/anthropic.rs`, `openai_compat.rs` — confirm their 4xx paths go through `classify_client_error` and add the new arm's test per adapter; comment `DEFAULT_MAX_CONTEXT` (anthropic.rs:25) "overridden by the config record's capabilities at construction (runtime.rs `build_provider`); 0 there means unknown".
- `crates/teton-providers/tests/conformance.rs` — both adapters map the spelling to `ContextLengthExceeded` and map an unrelated 400 unchanged.
- `crates/teton-providers/src/failure.rs` — doc on `classify` (L71-89): `ContextLengthExceeded` never reaches it (no class).

## Acceptance Criteria

- [ ] `cargo test -p teton-providers` green; a 400 body with each vendor spelling yields `ContextLengthExceeded` with `failure_class() == None`; an unrelated 400 still classifies `Fallback`.
- [ ] The sniff reads only `read_error_head` bytes (no full-body parse); the message carries no body text (conventions: no provider prose in errors).

## Technical Notes

- `EffortRefused` precedent: L372-400 variant, L291-308 classify, L441-443 `None`. Keep the spellings as `const`s with a "verified against vendor docs <date>" comment (REQ-577 both-halves rule).
- Commit as `feat(providers): typed ContextLengthExceeded, class-less [TASK-185]`.
