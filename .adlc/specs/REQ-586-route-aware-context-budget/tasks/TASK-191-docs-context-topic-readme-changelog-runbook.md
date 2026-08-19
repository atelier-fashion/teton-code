---
id: TASK-191
title: "Docs: teton_docs `context` topic, providers/doctor topic lines, README, CHANGELOG, manual-verification runbook, architecture.md pattern + ADR-006 note"
status: draft
parent: REQ-586
created: 2026-08-19
updated: 2026-08-19
dependencies: ["TASK-193", "TASK-190"]
repo: teton-code
---

## Description

Ship the vocabulary (ADR-11, BR-9): a fifth bundled `teton_docs` topic
`context` (budget, window, bound, `context_pressure`, `capabilities.max_context`,
`context_budget_cap`, worst-case per-prompt input — in **both** currencies,
words and bytes, saying the byte guard binds for prose/code on remote routes,
with the measured bytes/token per corpus class from TASK-183), a ≤ 40 B
pointer in `providers.md` (it sits at 4,050 of 4,096 B — if it does not fit,
nothing) and a line in `doctor.md`, README rows, CHANGELOG, the REQ-586 runbook
section, and the Key Pattern / ADR-006 note in `.adlc/context/architecture.md`.

## Files to Create/Modify

- `crates/tetond/src/harness/docs/context.md` (new, ≤ 4,096 B, ≥ 500 B) — what the budget is, how it derives per route, the five bounds, the `context_pressure` line, how to declare a window (`teton provider add --max-context`, `/provider setup`, `config/set`), the cap, the redact bound, the worst case (budget × loop iterations per prompt), "unknown window = default budget".
- `crates/tetond/src/harness/tools/docs.rs` — `TOPICS` (L68-73) + `("context", include_str!("../docs/context.md"))`; `TOPIC_INDEX` (L82) and `DESCRIPTION` (L127-130) gain `, context`; `every_bundled_topic_is_under_the_ceiling` (L474) covers it; `the_description_indexes_every_bundled_topic` updated; the resident-prompt margin tests must stay green (the description grows by ~9 bytes — assert, do not assume).
- `crates/tetond/src/harness/docs/doctor.md` — the `window:` column and the unknown-window advisory (≤ 200 B).
- `README.md` — "Hooking up an external model" (L298-328): `provider add` lines gain `--max-context <n>` (gated by `the_readme_recipes_and_the_catalog_agree`); a short "Context budget" paragraph.
- `CHANGELOG.md` — Unreleased: new wire fields/event, `--max-context`/`--context-budget-cap`, `window:` column, `context_pressure`, typed context-length outcome, older-peer degradation notes.
- `docs/manual-verification.md` — `## REQ-586` section = AC-14 runbook (Kimi `max_context = 128000` via `/provider add --max-context` or `config/set`; a 6,000-word prompt reaches Kimi whole — `/verbose` + cost row; `/doctor` window; `redact = true` → `bound: redact_scan`; worst-case per-prompt input; chunk-count note; "once REQ-585 lands, `/proceed REQ-xxx` expands").
- `.adlc/context/architecture.md` — Key Pattern "A per-route fact is derived once, where the route is decided, and every surface reads that value — the budget joins effort" (after the REQ-583 entries); ADR-006 consequence note (~L491) on per-route currency compatibility.

## Acceptance Criteria

- [ ] `cargo test -p tetond harness::tools::docs` green incl. ceiling/floor for `context`; both prompt-margin tests green without moving the ceiling.
- [ ] `web_setup_contracts.rs` README/guide/topic gates green; `every_bundled_topic_is_under_the_ceiling` green.
- [ ] The runbook section names every manual check of AC-14 with a checkbox, and records the resident-prompt headroom after this REQ (the `+9 B` description growth against the ≈887 B BUG-181 left — measure, as REQ-577/579/581/583 did; REQ-587 will read it).

## Technical Notes

- **From TASK-188 (verified 2026-08-19)**: the shipped recipe windows are Anthropic claude-opus-5 **1,000,000**, OpenAI gpt-5.6 **1,050,000**, Kimi-k3 **1,000,000**, DeepSeek **1,000,000**, Grok-4.6 **500,000**, Ollama llama3.2 **4,096** (served default). The runbook's worst case must be computed at the 1M class (≈665k words × up to 25 iterations per prompt — say it plainly and point at `context_budget_cap`), AC-14's Kimi step now gets 1M via `/provider setup` (or any figure by hand), and a declared 4k window yields a budget *smaller* than the local default (correct; worth one sentence in context.md).

- Do **not** touch `crates/tetond/src/harness/self_config.md` (BUG-181's headroom; REQ-585 BR-9 owns its next amendment).
- Commit as `docs(REQ-586): context topic, window docs, runbook, architecture pattern [TASK-191]`.
