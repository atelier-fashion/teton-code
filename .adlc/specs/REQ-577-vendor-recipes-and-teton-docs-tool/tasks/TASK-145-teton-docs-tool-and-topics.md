---
id: TASK-145
title: "teton_docs tool, bundled topics, cap-exempt registration"
status: draft
parent: REQ-577
created: 2026-08-14
updated: 2026-08-14
dependencies: ["TASK-143"]
repo: teton-code
---

## Description

Implement the read-only `teton_docs` builtin (spec BR-6..BR-10; ADR-3): four
bundled markdown topics served from memory, registered cap-exempt inside
`ToolRegistry::with_builtins()`, didactic unknown-topic error, per-topic byte
ceiling, tool_call title support.

## Files to Create/Modify

- `crates/tetond/src/harness/tools/docs.rs` — new `DocsTool` implementing
  `Tool` (tools/mod.rs:518): `name() = "teton_docs"`; `description()` ≤ ~120
  chars ending with the topic index (`topics: providers, policy, web,
  doctor`); one-string `topic` schema; `run()` matches the topic to an
  `include_str!` body; unknown topic → `ToolOutcome::error("unknown topic
  \`{t}\`; valid topics: …")`; `gates_itself()` default false; provenance
  identical to a tool that touched no paths.
- `crates/tetond/src/harness/docs/providers.md` — full recipes (all six
  vendors: exact `teton provider add` + `teton policy set-tier` commands,
  endpoint, example model, keyless note for Ollama) plus the BUG-165
  troubleshooting note (401 that looks like a bad key).
- `crates/tetond/src/harness/docs/policy.md` — tiers (reflex/scan/build/
  think), `set-tier`/`set-category`/`--fallback` semantics, `policy show`.
- `crates/tetond/src/harness/docs/web.md` — `[web]` setup depth beyond the
  guide: tiers, `search_auth` shapes (Brave/Kagi/SearxNG from the web
  catalog), keychain refs, restart requirement.
- `crates/tetond/src/harness/docs/doctor.md` — interpreting `teton doctor`:
  CLI/daemon version skew, weights/engine state, keychain and provider
  status, where config lives.
- `crates/tetond/src/harness/tools/mod.rs` — register `DocsTool` via
  `register_cap_exempt` inside `with_builtins()`; rewrite the
  `register_cap_exempt` + `with_builtins` doc comments ("exactly one tool
  registers this way") to enumerate both exempt tools with their distinct
  rationales (ADR-3).
- `crates/tetond/src/harness/turn_loop.rs` — `describe_call` arm rendering
  `teton_docs <topic>`.
- `crates/tetond/tests/web_setup_contracts.rs` — gate
  `the_providers_topic_and_the_recipe_catalog_agree` (topic ↔ catalog, both
  directions) and a web.md ↔ `suggestion_catalog()` auth-shape agreement
  check.

## Acceptance Criteria

- [ ] `teton_docs("providers")` returns the bundled body containing every
  catalog recipe; unknown topic returns the didactic error naming all four
  topics (AC-3) — unit tests in docs.rs.
- [ ] Per-topic ceiling test sweeps every bundled topic against 4,096 bytes
  with a trim-or-split failure message (BR-9, AC-8).
- [ ] `exposed_names(Some(DEGRADED_MAX_TOOLS))` contains `teton_docs`
  alongside the 5 builtins; updated invariants in
  `docs_are_capped_by_max_tools_for_degraded_providers` (tools/mod.rs:997)
  and `a_cap_exempt_tool_is_never_displaced_by_the_max_tools_cut`
  (tools/mod.rs:1034) assert the mechanism, not registration-order luck
  (BR-7, AC-5).
- [ ] Both margin tests still clear their 48-byte floor with the new tool
  docs included (BR-4).
- [ ] `cargo test -p tetond` green; clippy + fmt clean.

## Technical Notes

- Registration lives in the constructor, unlike web's call-site registration
  — web is config-gated opt-in (its doc comment explains why it must not
  live in `with_builtins`); `teton_docs` is unconditional, so the
  constructor makes "present in every session" true by construction, and
  every existing fixture inherits it without edits.
- Topics are plain markdown: no new envelopes/delimiters, so no ADR-009
  marker changes; results ride `frame_untrusted_builtin` as-is.
- Keep `description()` tight — tool docs bytes land in the resident prompt.
