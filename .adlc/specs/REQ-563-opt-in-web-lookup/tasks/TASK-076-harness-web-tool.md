---
id: TASK-076
title: "Harness web tool: conditional registration, tier/allowlist/cache logic, prompt text"
status: draft
parent: REQ-563
created: 2026-08-08
updated: 2026-08-08
dependencies: ["TASK-074", "TASK-075"]
---

## Description

The model-facing half (architecture D-1/D-3/D-5): the `web` tool, registered
only when a tier is enabled, with tier-ceiling and allowlist checks, cache
consult, permission-gate authorization showing the verbatim query, reduction +
untrusted framing of results, and the BR-6 system-prompt text for the absent
case.

## Files to Create/Modify

- `crates/tetond/src/harness/tools/web.rs` — new: `Tool` impl with `run()` orchestrating: normalize input → tier-ceiling check (refusal names the missing tier — AC-4) → authorship classification via `UserUrls` → allowlist check for ModelComposed fetches + search-result hops (refusal names the allowlist — AC-9) → cache consult (hit → framed result, no egress, no Ask — BR-12) → `PermissionGate::authorize` with verbatim query/URL + host in the description → `Egress::lookup` → `reduce()` → `ToolOutcome` with EMPTY provenance (`Sources(∅)` — architecture D-3, LESSON-432) framed via the existing untrusted builtin envelope. Terse `description()` (<100 chars); `input_schema()` carries the detail.
- `crates/tetond/src/harness/tools/mod.rs` — conditional registration AFTER `shell` (cut first under `max_tools` — charter BR-6); registration happens only when config tier > Off.
- `crates/tetond/src/harness/turn_loop.rs` — `build_system_prompt`: when the web tool is absent because tier = Off, extend the no-tool-ending clause to name the opt-in ("web lookup is available as an opt-in — `[web] tier` in config") so the model has a legal ending (BUG-154/LESSON-482); when present, no extra text (the tool docs suffice). Keep the pinned-prompt regression test in sync (turn_loop.rs:1297-1330 pattern: pin the exact clause, fail with update instructions).
- `crates/tetond/src/harness/context.rs` — confirm web tool results flow through `summarize_if_large` unchanged (local-pinned condensation — BR-10); adjust the reduce cap constant if the budget math needs it (LESSON-446 shared currency).

## Acceptance Criteria

- [ ] tier = Off → tool NOT registered: `ToolRegistry::exposed_names()` excludes it, system prompt names the opt-in, and dispatch of "web" errors as unknown tool (AC-1 structural half).
- [ ] Tier ceiling: with FetchUserUrl only, a ModelComposed fetch is refused naming the missing tier and makes zero egress calls; with FetchAnyUrl, search refused likewise (AC-4).
- [ ] Allowlist: configured allowlist refuses out-of-list ModelComposed fetches (naming the allowlist), exempts UserPasted URLs (AC-9); no allowlist → tier grant alone governs.
- [ ] Cache: fresh entry → result served with zero egress AND no permission prompt; `PermissionGate` not consulted (BR-12); stale → normal flow.
- [ ] Ask flow: the permission description contains the verbatim query/URL and destination host (AC-2's visibility half); results are framed `<tool-result trust="untrusted">` via the existing builtin framing — NO new envelope spelling, ADR-009 marker tests untouched (AC-5 posture).
- [ ] Provenance: `ToolOutcome` carries empty Sources — a session that only did web lookups does not fail-close provider egress (test: lookup then provider send with a boundary configured succeeds absent other taint).
- [ ] Prompt budget: the new clause + tool docs clear the existing budget-headroom test with margin (LESSON-493/BUG-160 pattern — assert against the real rendered prompt).

## Technical Notes

- Weak-model exposure: registration after shell means default `max_tools:
  Some(5)` hides web for degraded profiles automatically — do not special-case.
- The tool NEVER calls the override RPC or mutates taint/grants; it only reads
  session state handles it is constructed with.
- Search results that include URLs: fetching a result URL is a NEW lookup and
  re-enters the full gate chain (tier/allowlist/permission) — no implicit hops.
