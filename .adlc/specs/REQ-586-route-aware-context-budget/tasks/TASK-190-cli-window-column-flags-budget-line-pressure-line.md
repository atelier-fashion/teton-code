---
id: TASK-190
title: "CLI: --max-context/--context-budget-cap, window column + doctor advisory, verbose budget line on route_decided, context_pressure line, setup candidate window; cli_e2e"
status: draft
parent: REQ-586
created: 2026-08-19
updated: 2026-08-19
dependencies: ["TASK-181", "TASK-188"]
repo: teton-code
---

## Description

The user-facing half (ADR-9; BR-3, BR-7, BR-9): declare a window from the
CLI, see it in `/doctor` and `/provider list`, see the budget per turn under
`/verbose`, and see every pressure event as one line.

## Files to Create/Modify

- `crates/teton/src/main.rs` — `ProviderAction::Add` (L198-217): `--max-context <TOKENS>` and `--context-budget-cap <TOKENS>` (u32, optional); `build_provider_registration` (L3405-3424) sets the wire fields; `render_config` (L3624-3665): each row appends `window: 128k` (`Some(n)`, n>0; `Nk` for n ≥ 1000), `window: unknown — context budget defaulted (set capabilities.max_context)` (`Some(0)`), `window: not reported` (`None` — the daemon predates the field; `effort_ui.rs:210` precedent), and **`(local engine)` for a `ProviderKind::Local` row — never the unknown text**; `doctor_report_on` (L1823-1846) adds one advisory line per unknown-window **remote** provider (local rows excluded) and one per inert cap (cap ≥ window), in the `advise_on_base_url_endpoints` (L3395-3401) shape; `render_config` goldens (L4010, L6210) updated + the three window states.
- `crates/teton/src/session_ui.rs` — `render_event`: `Event::ContextPressure(p)` renders one line **never** verbose-gated (`context: 3 older blocks dropped to fit the 4,096-word budget (bound: local engine)` / `context: newest message middle-elided by 12 KB to fit …` / `context: re-fitted to the local engine's 4,096-word budget after a reroute — 7 older blocks dropped`) via a pure `format_context_pressure(&ContextPressure) -> String` (the `format_context_cleared` precedent, L1276); `format_route` (L2264-2284) appends ` · budget {n} words / {k} KB (bound: {bound})` when the fields are `Some` (older daemon → unchanged line); the four `format_route` goldens (L2553-2633, L2833) + new; a never-silent test for `context_pressure` (the inverse of `a_prefix_cache_event_is_silent_unless_verbose` L3034).
- `crates/teton/src/provider_setup_ui.rs` — `Answers::candidate()` (L267-290) carries `entry.max_context` into `ProviderSetupCandidate.max_context` silently (OQ-1 lean); catalog literals in tests (L1684-1727).
- `crates/teton/src/cli_rows.rs` — `/provider add` row help names `--max-context`; grammar tests (L905-917, L1212, L1466-1485) cover the flag.
- `crates/teton/tests/cli_e2e.rs` — AC-4: `provider_list_renders_the_declared_model` (L1920) + a fixture provider with `max_context = 128000` → `window: 128k`; the default fixture (L187-199, no capabilities) → `window: unknown — …` on both `/provider list` and `teton provider list` (`every_read_row_prints_exactly_what_its_shell_twin_prints` L4180 stays green — same renderer); `doctor_flags_a_hand_edited_base_url_endpoint_and_stays_green` (L2253) + the unknown-window advisory; `slash_verbose_toggles_the_route_notice_around_real_turns` (L1114): the `route [` line ends with `· budget N words (bound: local engine)` under verbose and quiet segments contain no `route [` and no `context:` line for a short turn; AC-5 (CLI half): `provider_add_*` (L1847-2200) with `--max-context 128000` → `/provider list` shows `window: 128k`; AC-8: a fixture with `context_budget_cap = 40000` on a 200k provider (redact off, as the fixture is) → verbose line says `bound: user cap`; a local-tier row renders `(local engine)` and no advisory; AC-10 (CLI half): a scripted turn that forces a drop renders exactly one `context:` line.

## Acceptance Criteria

- [ ] `cargo test -p teton` and `--test cli_e2e` green; AC-4, AC-5 (CLI), AC-8, AC-10 (CLI) pinned; an older-daemon snapshot (`max_context: None`) renders `window: not reported` and a `route_decided` without the fields renders the pre-REQ line byte-for-byte.
- [ ] `/provider list` and `teton provider list` print identical rows (L4180 golden).

## Technical Notes

- One renderer rule (REQ-582); the `Surface` sanitizer for provider-supplied text (LESSON-517).
- Commit as `feat(cli): window column and flags, budget line, context pressure line [TASK-190]`.
