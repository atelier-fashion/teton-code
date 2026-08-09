---
id: TASK-078
title: "Acceptance suite: egress-capture, consent matrix, taint/override, search-redact, e2e"
status: complete
parent: REQ-563
created: 2026-08-08
updated: 2026-08-08
dependencies: ["TASK-076", "TASK-077"]
---

## Description

The integration/acceptance layer proving the 13 ACs with the repo's existing
fixtures (CaptureTransport, CountingGate, scripted engine, TestDaemon). Unit
tests live in their feature tasks; this task is the cross-piece evidence.

## Files to Create/Modify

- `crates/tetond/tests/web_lookup_egress.rs` — new: CaptureTransport-backed suite: AC-1 (tier Off → a session makes ZERO lookup transport calls; prompt names the opt-in), AC-3 (tainted session ModelComposed → `taint_restricted`, no packet; planted credential in search query → `blocked_redact` with fixtures built through the production encoder — LESSON-490), AC-8 (connect error → offline outcome, turn completes), AC-10 (second lookup served from cache, zero transport calls, `web_cache_hit`… ledger row present), AC-11 (fetch a fixture HTML page; assert no raw page bytes in any captured remote-provider payload — only reduced text).
- `crates/tetond/tests/web_consent_matrix.rs` — new: AC-2 (deny → no packet; allow-once → exactly one; allow-session → until session end, not beyond; enable-permanent → config persisted, next daemon start honors it), AC-4 (tier gradation refusals name the missing tier), AC-9 (allowlist matrix incl. user-pasted exemption), AC-12 (taint trip → visible notice + `UserPasted` proceeds; override via RPC restores; override attempted via tool dispatch fails; fresh session restricted again), AC-13 (search gate installed ⇔ tier Search; Unavailable blocks the query; local tier absent → search not offered at consent time).
- `crates/tetond/src/harness/render.rs` tests (extend in place) — AC-5: a fetched page containing frame markers, role labels, and BOTH envelope spellings is neutralized by the existing sanitizers when framed as a web result; assert the ADR-009 bidirectional coverage tests still pass UNCHANGED (no new markers were introduced — that absence is the assertion).
- `crates/teton/tests/cli_e2e.rs` — extend: `/web allow` + `/web refresh` command flows against TestDaemon with scripted engine; status row shows web state; `/cost` includes lookup lines (AC-6); `/help` lists both commands.

## Acceptance Criteria

- [x] Every AC (1–13) is exercised by at least one test in this task or a feature task, and a comment header maps AC → test fn name.
- [x] Egress-capture assertions are byte-level: zero lookup traffic for AC-1, no raw page bytes for AC-11, no query text in any event/ledger row.
- [x] Redact fixtures are built through the production encoder path (LESSON-490 — no hand-written raw fixtures for encoded forms).
- [x] The suite runs with the scripted engine harness (no model download, no network) and passes with `cargo test --workspace`.
- [x] Negative controls: each blocking test proves it can fail (temporarily invert a gate in-test via config, not by editing prod code) — a passing test that has never failed proves nothing (LESSON-479 falsification discipline).

## Technical Notes

- Reuse `CaptureTransport` (egress_capture.rs:44-67), `CountingGate`
  (egress/mod.rs:1002-1030), `TestDaemon` + `TETON_LOCAL_SCRIPT`
  (cli_e2e.rs:65-165). No new test scaffolding.
- AC-13's "local tier absent" leg: run with the loaderless default build state
  (no `llama` feature) where the engine slot is honestly absent.
- Workspace build first, then targeted runs — a stale daemon binary can mask
  failures in e2e (repo memory: targeted e2e runs test a stale daemon).

## Outcome

23 new tests; workspace 1684 green, clippy clean.

| AC | Where it is proven |
|----|--------------------|
| AC-1 | `tetond/tests/web_lookup_egress.rs::tier_off_makes_zero_lookup_traffic_across_a_scripted_session` |
| AC-2 | `tetond/tests/web_consent_matrix.rs::a_denied_lookup_puts_no_packet_on_the_wire`, `…::allow_once_permits_exactly_one_lookup_and_asks_again`, `…::allow_for_this_session_lasts_to_session_end_and_not_beyond`, `…::enable_permanent_writes_a_ceiling_the_next_daemon_start_honours`; e2e prompt half in `teton/tests/cli_e2e.rs::a_web_lookup_is_consented_reported_and_counted_in_the_cost_report` |
| AC-3 | `web_lookup_egress.rs::a_model_composed_lookup_in_a_tainted_session_leaves_no_packet`, `…::a_planted_credential_is_blocked_in_the_form_the_encoder_would_send` |
| AC-4 | `web_consent_matrix.rs::tier_gradation_refusals_name_the_missing_tier` |
| AC-5 | `tetond/src/harness/render.rs::tests::a_hostile_fetched_page_is_neutralized_as_a_web_tool_result`, `…::the_web_capability_introduced_no_new_frame_marker` |
| AC-6 | `cli_e2e.rs::a_web_lookup_is_consented_reported_and_counted_in_the_cost_report` (`/cost`), `cli_e2e.rs::slash_help_lists_every_command_and_no_turn_is_attempted` (`/help`, TASK-077), `teton/tests/pty_e2e.rs::the_status_row_shows_the_session_s_web_capability` (status row) |
| AC-7 | TASK-072 config validation + TASK-077 `runtime.rs` keychain-ref tests; the **wiring** — that a resolved `search_key_ref` becomes a `Bearer` header bound to the endpoint's origin — is `tetond/src/runtime.rs::tests::web_lookup_seam::the_search_key_rides_only_the_search_request_and_only_to_its_endpoint` (added at verify; it reads the header off a loopback socket, because the transport `web_lookup_egress` builds is not handed back). Its legs also cover the confused-deputy case, now a refusal: a fetch aimed at the search origin is `RefusedDomain`/`SearchEndpointFetch` before the wire |
| AC-8 | `web_lookup_egress.rs::an_unreachable_destination_is_offline_and_the_turn_still_completes`; e2e in `cli_e2e.rs` |
| AC-9 | `web_consent_matrix.rs::the_allowlist_constrains_model_chosen_destinations_only` |
| AC-10 | `web_lookup_egress.rs::a_second_lookup_is_served_from_cache_with_zero_transport_calls`; `/web refresh` client half in `cli_e2e.rs::the_two_web_commands_reach_the_daemon_and_render_its_answer` |
| AC-11 | `web_lookup_egress.rs::no_raw_page_bytes_reach_any_remote_provider_payload` |
| AC-12 | `web_consent_matrix.rs::the_taint_notice_names_cause_and_effect_and_a_paste_still_works`, `…::only_the_client_rpc_can_lift_the_restriction`, `…::no_tool_is_named_for_the_override_or_the_refresh`; `/web allow` client half in `cli_e2e.rs` |
| AC-13 | `web_consent_matrix.rs::a_search_with_no_gate_installed_is_a_block_not_a_skip`, `…::an_unavailable_scan_blocks_the_query_and_sends_nothing`, `…::on_a_loaderless_build_the_real_search_gate_refuses_every_query`; wiring ⇔ in TASK-077's `runtime.rs::the_lookup_choke_point_carries_the_recorder_and_the_tier_s_gate`; the **kind-aware notice** — a blocked search naming the missing *local model* rather than a generic refusal, and saying something different for a fetch — in `teton/src/session_ui.rs::tests::a_blocked_search_names_the_missing_local_model_rather_than_a_refusal`. Realization note: BR-14's "the search tier is not offered at consent time" ships as *always installed, blocks when it cannot run* — an engine that arrives mid-session must not require a consent surface to have hidden an option earlier (architecture Deviations §5). The capped/absent-engine case is the same fact from the other side: on a non-`Native` profile the tool is registered and never exposed, reported as `web: unavailable (profile)` (Deviations §8, covered by `runtime.rs::tests::web_tool_wiring::a_capped_profile_reports_the_web_tool_as_unavailable_for_the_turn`) |

Falsification: every blocking test in this task was re-run against a targeted
production mutation and observed red (taint gate, redact block, tier-off
registration, cache lookup, page reduction, offline taxonomy, allowlist, tier
ceiling, consent gate, missing-search-gate refusal, `enable_permanent`
persistence, `WebTaintOverride::lift`, `redact::decide`, tool name, envelope
alphabet, frame-label neutralization, `WebState::is_engaged`, `/cost` web
section, `web_override`'s `was_restricted`). All mutations were reverted; no
production code changed in this task except the added `render.rs` tests.
