---
id: TASK-177
title: "The environment block in every prompt, paid for under the resident ceiling"
status: draft
parent: REQ-583
created: 2026-08-18
updated: 2026-08-18
dependencies: ["TASK-175"]
---

## Description

Leg A's prompt half (BR-1, BR-3's block wording, AC-1..AC-4) per
`architecture.md` ADR-2: render `HarnessConfig.session_root` (field added by
TASK-175) as one bounded line after the opener; add the 200-char-root row to
both ceiling sweeps; buy the bytes by moving the guide's web-paragraph
**reference data** into the existing `web` topic; record the new headroom.
**File ownership (parallel tier):** `harness/turn_loop.rs`,
`harness/self_config.md`, `egress/redact.rs` (test + note only),
`harness/tools/web.rs` (its ceiling test only), and the guide-contract tests
that must keep passing. Do not touch `tools/{glob,grep,shell,read,edit,walk}.rs`
(TASK-176), `server.rs`/`runtime.rs`/`sessions.rs` (TASK-178), or `crates/teton`
(TASK-179).

## Files to Create/Modify

- `crates/tetond/src/harness/turn_loop.rs` — `pub(crate) fn environment_block(root: &SessionRoot) -> String` (pure): `Session root: {display} ({kind phrase}). Platform: {os}.\n` where kind phrase = `project {name}, branch {branch}` / `project {name}` (no branch) / `your home folder` / `the filesystem root` / `not a project`; `{os}` = `macOS` / `Linux` / `Windows` / `unknown` from `cfg!(target_os)`; values are already bounded (teton-core `bounded_field`) but re-bound here defensively (`DISPLAY_MAX_CHARS`, `NAME_MAX_CHARS`) — mid-line only, never at column 0. `build_system_prompt`: `if let Some(root) = &config.session_root { s.push_str(&environment_block(root)); }` immediately after the opener paragraph and before the verification clause. Tests: AC-1 (git-project root → block contains display, "project", name, branch, platform — asserted by content), AC-2 (home/root/plain phrases; no "branch" word), AC-3 (project with no branch), a "one opener" guard (`matches("You are Teton Code").count() == 1`), and `a_harness_authored_system_prompt_is_byte_identical` (`render.rs:795`) still passes (default config has no block).
- `crates/tetond/src/harness/self_config.md` — trim the web paragraph to: `Web lookup is a separate opt-in, off by default; `/web setup` writes `[web]` (`tier`: `off`, `fetch_user_url`, `fetch_any_url`, `search`, cumulative; keys by keychain reference, never raw; the rest: `teton_docs web`). `search_auth` is the header the key rides, `{key}` marking it: default `Authorization: Bearer {key}`, Brave `X-Subscription-Token: {key}`, Kagi `Authorization: Bot {key}`; keyless SearxNG needs none (endpoint ends `/search?format=json`).` — keep every pinned string (`suggested_auth_templates` parse: backticked `Header: … {key}` spans; ` needs none`; `/search?format=json`; recipes; inspect spellings; step-1 hand-off). If more bytes are needed, shorten step 2's `--fallback`/`set-category` clause (the `policy` topic carries both) — never a recipe, never a template.
- `crates/tetond/src/egress/redact.rs` — `the_total_cap_clears_the_harness_context_budget_with_margin`: sweep also over `session_root: Some(worst)` where `worst` = a `SessionRoot` whose display is `bounded_field(<200-char path>)`, name/branch at their caps, kind Project (and the same for each web state); same two assertions, no constant change; add a **REQ-583 paragraph** to the headroom note (L1965-2010 style) recording the worst prompt, `spent`, margin, what was moved and where; assert margin ≥ 80 in this task's local run (the constant stays 48 — the extra is TASK-176's doc-rewording allowance; TASK-180 records the final number).
- `crates/tetond/src/harness/tools/web.rs` — the twin sweep (`the_web_tool_docs_clear_the_outbound_body_overhead`, L2248-2335) gains the same row.
- Verify unchanged (run, do not edit unless a pinned string moved): `crates/tetond/tests/web_setup_contracts.rs`, `crates/tetond/src/provider_recipes.rs` guide tests, `crates/teton/src/cli_rows.rs` guide_tests, `turn_loop.rs:2408` (`never inside the repository` — the guide's line 1 stays), `:2641`, `:2673`.

## Acceptance Criteria

- [ ] AC-1, AC-2, AC-3 named tests green; the block appears exactly once, after the opener, only when `session_root` is `Some`.
- [ ] AC-4: both ceiling sweeps include the 200-char-root row and pass with `REDACT_BODY_OVERHEAD_BYTES`/`MIN_PROMPT_HEADROOM_BYTES` unchanged; recorded margin in the note ≥ 80 bytes at this task's tip.
- [ ] `cargo test -p tetond` for `web_setup_contracts`, `provider_recipes`, `turn_loop` and `cargo test -p teton cli_rows::guide_tests` green with no assertion weakened.
- [ ] `render.rs::a_harness_authored_system_prompt_is_byte_identical` green.
- [ ] No "repository"/"repo" in the block's own text.

## Technical Notes

- Bytes: measure, don't estimate (LESSON-491) — run the sweep, read the failure message's numbers, trim, re-run.
- The web capability clause and the guide's line 1 keep the word "repository"; AC-6 scopes only tool descriptions and refusals (BR-3).
- The `web` topic already carries every fact removed; do not add to it unless a removed sentence is genuinely absent there (then add it — a topic is not resident).
- Commit as `feat(prompt): the environment block — session root, kind, project, platform — resident and paid for [TASK-177]`.
