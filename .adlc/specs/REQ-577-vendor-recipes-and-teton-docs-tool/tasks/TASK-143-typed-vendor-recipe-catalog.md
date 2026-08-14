---
id: TASK-143
title: "Typed vendor recipe catalog with golden pins"
status: complete
parent: REQ-577
created: 2026-08-14
updated: 2026-08-14
dependencies: []
repo: teton-code
---

## Description

Create the single typed source for vendor provider recipes (spec BR-2):
`provider_recipes.rs`, a pure daemon-owned factory modeled on
`web_setup_catalog.rs` (REQ-573), carrying the OQ-1 roster — Anthropic,
OpenAI, Moonshot (Kimi), DeepSeek, Ollama, Grok (xAI) — with each entry's
facts verified against the vendor's current public docs (BR-3, LESSON-512)
and pinned by golden verbatim tests.

## Files to Create/Modify

- `crates/tetond/src/provider_recipes.rs` — new module: `ProviderRecipe`
  struct (`id_suggestion: String`, `label: String`, `kind: ProviderKind`
  reused from `teton_core::entities`, `endpoint: Option<String>`,
  `example_model: String`, `notes: Option<String>`) and pure `#[must_use]`
  `recipe_catalog() -> Vec<ProviderRecipe>`; module-level golden tests.
- `crates/tetond/src/lib.rs` — declare the new module (match how
  `web_setup_catalog` is declared).

## Acceptance Criteria

- [ ] `recipe_catalog()` takes nothing and reads nothing (no config, env,
  TTY, or daemon state — the web_setup_catalog purity rule).
- [ ] All six OQ-1 vendors present; `anthropic`-kind entries carry
  `endpoint: None`; every `openai-compatible` entry carries a real endpoint
  URL; Ollama is marked keyless/local in `notes`.
- [ ] Every endpoint, kind, and example model verified against the vendor's
  current public documentation at implementation time (WebFetch/WebSearch),
  with a one-line doc comment per entry citing what was checked (BR-3).
- [ ] Golden verbatim test in the module pins every entry byte-for-byte
  (hand-written second spelling, `the_catalog_ships_the_three_backends_verbatim`
  posture) with an update-don't-delete failure message.
- [ ] No credential and no field that could hold one appears in the module
  (the web_setup_catalog BR-6 rule).
- [ ] `cargo test -p tetond provider_recipes` green; `cargo clippy` clean.

## Technical Notes

- Follow `crates/tetond/src/web_setup_catalog.rs` structure and doc-comment
  voice closely — it is the reviewed precedent for exactly this shape.
- Reusing `ProviderKind` (teton-core/src/entities.rs:19) makes "a recipe
  names a kind `provider add` accepts" a compile-time fact (ADR-1).
- Known-good starting facts to verify, not assume (BR-3): Moonshot
  `https://api.moonshot.ai/v1`, DeepSeek `https://api.deepseek.com`, Grok
  `https://api.x.ai/v1`, Ollama `http://localhost:11434/v1` (keyless),
  OpenAI/Anthropic native kinds. Example models are examples — label them so
  in `notes` or the doc comment, not as "current best".

## Verification Record (BR-3, 2026-08-14)

Every value below was read off the vendor's own current public documentation at
implementation time. Full citations live in the per-entry comments in
`crates/tetond/src/provider_recipes.rs`.

| Vendor | kind | endpoint | example_model | source consulted |
|--------|------|----------|---------------|------------------|
| Anthropic | `anthropic` | *(none — built in)* | `claude-opus-5` | platform.claude.com models overview (Claude API IDs table) |
| OpenAI | `openai-compatible` | `https://api.openai.com/v1` | `gpt-5.6` | developers.openai.com chat/create curl example; models/gpt-5.6 (alias → `gpt-5.6-sol`) |
| Moonshot (Kimi) | `openai-compatible` | `https://api.moonshot.ai/v1` | `kimi-k3` | platform.kimi.ai API overview (`base_url`) + models overview |
| DeepSeek | `openai-compatible` | `https://api.deepseek.com` | `deepseek-v4-pro` | api-docs.deepseek.com first-call example + models & pricing |
| Ollama | `openai-compatible` | `http://localhost:11434/v1` | `llama3.2` | docs.ollama.com OpenAI-compatibility ("required but ignored" key) |
| Grok (xAI) | `openai-compatible` | `https://api.x.ai/v1` | `grok-4.6` | docs.x.ai overview quickstart + models list |

Two of this task's drafted starting facts had already gone stale and were
**corrected against the docs, not carried forward** — the case for BR-3:

- DeepSeek: `deepseek-chat` / `deepseek-reasoner` no longer appear on the
  models page; current ids are `deepseek-v4-pro` / `deepseek-v4-flash`.
- Moonshot: the `kimi-k2` family has been superseded by `kimi-k3` as the
  documented flagship. **The README's `kimi-k2` (README.md ~l.252) is now
  provably stale** — the ADR-2 prose gate task must update it, not just pin it.

Endpoints all matched the drafted values. `platform.moonshot.ai` now 301s to
`platform.kimi.ai`, but the *API* host `api.moonshot.ai` is unchanged.
