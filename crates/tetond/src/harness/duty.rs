//! The **shared duty seam**: one resolved route, one trait, one local impl, one
//! remote impl, one egress scoping call, one ceiling (REQ-561 BR-6, ADR-1/ADR-2).
//!
//! ## What a "duty" is
//!
//! A duty is a model call the *harness* makes on its own behalf, as opposed to
//! the turn the user asked for. `digest` is the first one: `summarize_if_large`
//! condenses an oversized tool result before it enters context. It is one
//! instruction in, one bounded string out — never a conversation, never a tool
//! call, never a lifecycle position.
//!
//! ## Why one seam rather than one per category
//!
//! REQ-558 built `digest` as a one-off — a route enum, a trait, a local/remote
//! pair, an egress call, a ceiling constant — roughly 260 lines of machinery for
//! one category. REQ-561 adds four more callers, and copying that shape four
//! times would produce five parallel implementations of the same five concerns.
//! So [`DutyRoute`] is **one non-generic type** holding an [`Arc<dyn Duty>`]
//! (ADR-1): a generic `DutyRoute<T>` monomorphises into five distinct types,
//! which is the same outcome expressed in the type system instead of in copied
//! code.
//!
//! What stays per-category is bounded to three things, and none of them are
//! here: the one-line resolver in [`crate::runtime`] that names the category
//! literally (ADR-3 — the `declared, no call site yet` marker in
//! [`crate::call_sites`] is derived by scanning for exactly that spelling, so
//! collapsing it into a helper taking a category *variable* would make the scan
//! blind), the duty's output-contract constant, and its prompt builder.
//!
//! ## Two implementations, one seam (mirroring `completion.rs`)
//!
//! - [`LocalDuty`] holds an [`Engine`] and **no transport**, so egress is
//!   impossible on that path by construction — exactly
//!   [`LocalEngineSource`](super::completion::LocalEngineSource)'s posture.
//! - [`RemoteDuty`] reaches the network **only** through the provenance-scoped
//!   `&dyn Transport` that [`Egress::scoped`] produces, so a duty over a
//!   `local-only` file is refused before a byte leaves and is billed as one
//!   `CostRecord` against the duty's own category (BR-1/BR-2).
//!
//! ## The provenance is the caller's (ADR-2)
//!
//! [`Duty::perform`] takes the already-merged egress [`Provenance`] of *the
//! content it is about to send* — not a tool-shaped wrapper it would have to
//! know how to interpret. Each call site computes it from the content it is
//! handing over ([`digest`](super::digest::tool_result_provenance) converts a
//! [`ToolProvenance`](super::context::ToolProvenance)), which makes BR-7's
//! "scoped by the content it sends" a property of the signature rather than a
//! convention every duty has to remember.
//!
//! ## `route_decided` fires on **performance**, not on resolution (BR-2)
//!
//! BR-2 exists to make a new *egress path* visible. A path that never fires
//! produced no egress, so announcing a routing decision for a duty that never
//! runs would be observing a resolution, not an egress. And the arithmetic is
//! decisive: `digest_route()` is built unconditionally once per turn attempt
//! whether or not any tool result crosses the threshold, so emitting at
//! resolution would put five spurious `route_decided` events on every turn once
//! all five duties are wired.
//!
//! So [`DutyRoute::announcing`] *attaches* the payload the resolver projected,
//! and [`DutyRoute::perform`] publishes it — once per invocation, because each
//! invocation is a real routed model call, which is exactly what `route_decided`
//! means. The publish sits in the seam rather than at each call site on purpose:
//! the seam is the one place all five duties share, so one emission site here is
//! BR-6 working, not a concession.
//!
//! ## Failure is never silence (LESSON-447)
//!
//! A duty cannot decline. An implementation that cannot serve returns `Err`
//! carrying a broadcast-safe sentence, and an unresolvable binding is a
//! [`DutyRoute::Unresolved`] carrying the resolver's own reason. Both leave the
//! call site holding an explanation it can degrade with, which is the whole
//! reason neither is a bare `None`.
//!
//! # What breaks which test
//!
//! REQ-561 AC-9, across all five duties. Every row was **applied to the source,
//! compiled, and observed** (LESSON-441); none was reasoned about. Test paths
//! are relative to `tetond`'s lib target unless a `tests/` file is named.
//! `compact`'s own four-link chain has its own table at
//! [`super::compact`]'s module head; this one is the cross-cutting set.
//!
//! ## AC-9(a) — the session-taint override, one row per duty
//!
//! Each row deletes that duty's `is_tainted` arm in [`crate::runtime`], so the
//! duty resolves its configured binding on a tainted session.
//!
//! | Mutation | Fails |
//! |---|---|
//! | `digest_route` loses its taint arm | `runtime::tests::dispatch::digest::a_tainted_session_digests_on_the_local_tier` |
//! | `triage_route` loses its taint arm | `runtime::tests::dispatch::triage::a_tainted_session_triages_on_the_local_tier`, `tests/e2e/duty_taint.rs::a_tainted_sessions_duties_never_reach_the_provider_they_are_bound_to` |
//! | `shell_route` loses its taint arm | `runtime::tests::dispatch::shell::a_tainted_session_interprets_on_the_local_tier`, and the same e2e test — the duty is then refused at the choke point instead of served locally, so its interpretation vanishes from the reply |
//! | `title_route` loses its taint arm | `runtime::tests::dispatch::title::a_tainted_session_titles_on_the_local_tier`, and the same e2e test, **by captured bytes** |
//! | `compact_route` loses its taint arm | `runtime::tests::dispatch::compact::a_tainted_session_compacts_on_the_local_tier` |
//!
//! ## AC-9(b) — the failure path stops preserving the call site's invariant
//!
//! `digest` and `compact` must *not* return their input unchanged (their whole
//! job is to shrink it); `triage`, `shell` and `title` must. So the mutation is
//! whichever direction breaks that duty's invariant.
//!
//! | Mutation | Fails |
//! |---|---|
//! | `summarize_if_large`'s mechanical fallback folds the raw text instead of truncating it | `harness::context::tests::engine_failure_falls_back_to_bounded_mechanical_truncation`, `harness::digest::tests::an_unresolved_digest_still_bounds_an_oversized_result`, `…::a_local_only_tool_result_is_never_sent_to_a_remote_digest`, `tests/duty_matrix.rs::every_duty_holds_its_call_sites_invariant_on_every_failure_path` |
//! | `GrepTool::refine`'s failure arm drops the tool's own result instead of returning it | `harness::tools::grep::tests::every_triage_failure_returns_the_tools_own_unranked_result_verbatim`, `tests/duty_matrix.rs::every_duty_holds_…` |
//! | `ShellTool::refine`'s failure arm drops the tool's own result | `harness::tools::shell::tests::every_shell_duty_failure_returns_the_tools_own_capped_result_verbatim`, `tests/duty_matrix.rs::every_duty_holds_…`, `tests/duty_egress.rs::a_shell_duty_sends_only_on_a_machine_with_no_boundary_configured` |
//! | `name_session` answers a failure with a blank title instead of no title | `harness::title::tests::an_empty_answer_is_a_failure_not_a_blank_title`, `…::a_title_over_boundary_content_is_refused_before_a_byte_leaves`, `runtime::tests::dispatch::title::a_failed_title_does_not_retry_on_the_next_turn`, `tests/duty_matrix.rs::every_duty_holds_…` |
//! | the loop's `truncate_to_budget()` becomes conditional on the compaction having **worked** | `harness::turn_loop::tests::a_turn_whose_compact_duty_cannot_serve_still_ends_under_budget` |
//!
//! ## The seam's own guarantees
//!
//! | Mutation | Fails |
//! |---|---|
//! | the ceiling site stops bounding the accumulator (BR-8) | all five `harness::*::tests::a_remote_*_is_bounded_however_much_the_provider_streams` |
//! | [`LocalDuty::perform`] returns the engine's answer unbounded | `harness::title::tests::a_local_title_duty_is_bounded_however_much_the_engine_generates`, `…::a_runaway_local_answer_still_names_the_session_within_the_ceiling`, `harness::shell_duty::tests::a_local_shell_duty_is_bounded_however_much_the_engine_generates`, [`tests::the_duty_path_has_one_egress_scoping_call_and_one_ceiling_site`] |
//! | either leg asks for `GenParams::default().max_tokens` instead of [`DutyKind::max_tokens`] | local: `harness::compact::tests::a_local_compaction_may_write_the_paragraph_its_ceiling_allows`; remote: `harness::title::tests::an_unbounded_machine_sends_the_title_prompt_and_returns_its_answer` |
//! | [`DutyRoute::perform`] loses its [`DUTY_DEADLINE`] | [`tests::a_duty_that_never_answers_fails_at_the_deadline`] — which then **hangs** rather than failing, exactly as the turn it is on would |
//! | `MeteredBody` loses its `Drop` impl, so the deadline buys calls off-ledger | [`tests::a_duty_cut_off_at_the_deadline_is_still_on_the_ledger`], `cost::ledger::tests::a_stream_abandoned_mid_flight_still_bills_what_the_provider_reported` |
//! | [`Egress::scoped`] is handed an empty provenance instead of the content's (BR-7) | six per-duty boundary tests, every test in `tests/duty_egress.rs`, and `tests/duty_matrix.rs::every_duty_holds_…` |
//! | a duty module grows a category from a tool name (`if name == "grep" { … }`) | [`tests::no_duty_category_is_ever_produced_from_text`] |
//! | a per-category route enum grows back (BR-6) | [`tests::one_route_type_one_trait_and_two_implementations_serve_every_duty`] |
//! | a duty module takes an `Egress` of its own (BR-6) | [`tests::no_duty_module_carries_any_of_the_seams_concerns`] |
//! | the choke point stops pinning the session it refused | `runtime::tests::dispatch::a_duty_refused_at_the_choke_point_taints_its_session`, `…::an_unattributable_privacy_block_pins_no_session` |
//!
//! ## REQ-562 — the sixth caller, and the gate it is called from
//!
//! `redact` reaches this seam from the egress choke point rather than from the
//! harness (ADR-1), so its rows are about the *wiring*, not about a call site
//! inside a turn. The rows immediately below are TASK-070's, written from the
//! wiring as it was built; the AC-8 mutations were **applied and observed** by
//! TASK-071 and are recorded in their own table after them, one of them green.
//!
//! | Mutation | Fails |
//! |---|---|
//! | the gate hook moves **above** the provenance early-return in [`Egress::send`](crate::egress::Egress::send) | `egress::tests::a_payload_refused_by_provenance_is_never_scanned` — AC-11's ordering, by scanner call count |
//! | `Egress::send` forwards on a `Block` decision (the permissive-guard mutation, AC-8a) | `egress::tests::an_unavailable_scan_blocks_the_send_and_says_it_could_not_run`, `…::a_high_confidence_finding_blocks_with_its_kind_span_and_locus` |
//! | `block_cause` reports `ScanUnavailable` for a findings verdict, or `Redaction` for an unavailable one | `egress::tests::a_redaction_block_reports_the_first_high_finding_and_never_a_low_one`, and both block tests by `cause` |
//! | the gate is installed unconditionally, ignoring `[privacy] redact` | `runtime::tests::dispatch::redact::off_means_no_gate_and_on_means_a_gate_that_reaches_the_engine` — the off leg, by engine call count |
//! | `redact_route` grows a session-taint arm (the "uniformity fix" ADR-3 forbids) | nothing turns red, and that is the point: the arm cannot change the answer, which is what `runtime::tests::dispatch::redact::a_tainted_session_resolves_redact_exactly_as_a_clean_one_does` pins from the other direction |
//! | `redact_route` falls through to the squatting provider when `local_tier_id` yields nothing | `runtime::tests::dispatch::redact::a_squatted_local_tier_id_leaves_the_scan_unavailable_never_remote` |
//! | `redact_route` treats an unresolved route as clean instead of unresolved | `runtime::tests::dispatch::redact::a_machine_with_no_engine_loaded_blocks_rather_than_passing_the_scan` |
//! | `build_duty_route` stops installing the gate on a remotely bound duty's choke point | [`tests::a_remote_duty_send_is_scanned_by_the_choke_points_gate`] (at the seam) |
//! | `mcp_egress` stops installing the gate on the MCP choke point | `runtime::tests::dispatch::redact::an_mcp_tool_call_crosses_the_gate_when_redact_is_on`. **This was GREEN before that fixture existed** — the attachment lived inside `build_tools`, reachable only through `HttpTransport::new()` and a real socket, so deleting it left the whole suite passing. The transport is a parameter of `mcp_egress` now precisely so the construction is drivable from a test |
//! | the forward path rebuilds the request from the scanned text instead of passing it through | `egress::tests::a_low_only_or_clean_verdict_forwards_the_exact_bytes`, `…::the_gate_reads_the_exact_bytes_that_would_go_on_the_wire` (AC-9) |
//! | the gate arm keeps the report **call** and drops its **effect** — `for _line in redact::forwarded_findings_report(&verdict) {}` | `egress::tests::the_gate_arm_reports_a_forwarded_findings_verdict`. Applied to a freshly built workspace, observed (716 passed / 1 failed), reverted. The needle used to be the call alone, which this mutation satisfies; it now covers the whole statement, `eprintln!` included, over whitespace-normalized source |
//! | the gate hook is placed after `inner.execute`, or metering moves ahead of it | `egress::tests::a_blocked_send_bills_nothing_while_an_allowed_one_still_bills` |
//! | either taint gate pins the session on `ScanUnavailable` (`=> true`) | `runtime::tests::dispatch::{a_scan_unavailable_block_refuses_the_payload_without_pinning_the_session, the_two_taint_gates_agree_cause_for_cause}` and `…::redact::a_scan_unavailable_turn_does_not_pin_the_session` — applied to a freshly built workspace, 694 passed / 3 failed, reverted. **The gates were added by TASK-072**: before them both sites pinned unconditionally, so a 120-second engine stall permanently routed the rest of the session local on the strength of a fact nobody established |
//!
//! ### REQ-562 AC-8 — the mutations, applied and observed (TASK-071, TASK-072)
//!
//! Each was applied to a freshly built workspace (`cargo build --workspace`
//! first — LESSON-489's sibling trap), run, and reverted. The diffs are recorded
//! exactly, because a mutation nobody can reproduce is a claim rather than a
//! check.
//!
//! | # | Mutation (exact diff) | Turns red |
//! |---|---|---|
//! | **(a)** permissive unavailable | `egress/redact.rs`, `decide`: `Outcome::Unavailable => EgressDecision::Block` → `=> EgressDecision::Forward` | **12 lib + 4 integration.** `egress::redact::tests::{confidence_drives_the_egress_decision, an_over_cap_payload_is_unavailable_and_blocks_never_forwards, an_over_cap_payload_carrying_a_credential_still_reports_could_not_scan}`, `egress::tests::an_unavailable_scan_blocks_the_send_and_says_it_could_not_run`, `harness::redact::tests::{an_unresolved_route_is_unavailable_not_clean, an_engine_failure_is_unavailable_not_clean, an_unreadable_answer_blocks_rather_than_passing_as_clean, an_over_cap_payload_is_unavailable_before_any_model_call, a_scan_that_overruns_the_deadline_is_unavailable}`, `runtime::tests::dispatch::redact::{a_machine_with_no_engine_loaded_blocks_rather_than_passing_the_scan, a_squatted_local_tier_id_leaves_the_scan_unavailable_never_remote, a_scan_that_could_not_run_fails_the_turn_saying_so_not_blaming_a_boundary}`, and `tests/redact_egress.rs::{with_no_local_tier_the_payload_is_blocked_unscanned_and_nothing_claims_otherwise, a_payload_past_the_input_cap_blocks_unscanned_and_costs_no_model_call, a_remote_provider_squatting_the_local_id_never_receives_the_scan, an_engine_backed_local_tier_under_another_id_still_serves_the_scan}` |
//! | **(b)** unbounded scan input (BR-7) | `egress/redact.rs`, `pattern_verdict`: delete the `if text.len() > REDACT_INPUT_MAX_BYTES { return RedactionVerdict::unavailable(); }` guard | **4 lib + 1 integration.** `egress::redact::tests::{an_over_cap_payload_is_unavailable_and_blocks_never_forwards, an_over_cap_payload_carrying_a_credential_still_reports_could_not_scan}`, `harness::redact::tests::{an_over_cap_payload_is_unavailable_before_any_model_call, an_over_cap_payload_carrying_a_credential_still_says_the_scan_could_not_run}`, `tests/redact_egress.rs::a_payload_past_the_input_cap_blocks_unscanned_and_costs_no_model_call` |
//! | **(c)** a finding carries its matched text, threaded to the event | `egress/redact.rs`: `Finding` gains `text: String` (both constructors init `String::new()`) plus `fn carrying(self, &str) -> Self` and `fn text(&self) -> &str`; `pattern_pass` maps `Finding::pattern(..).carrying(&text[span])`; `harness/redact.rs`'s `read_findings` maps `Finding::model(..).carrying(&payload[span])`; `egress/mod.rs`'s gate arm builds `path` as `format!("{} ({})", redaction_locus(cause), finding.text())` | **2 lib + 2 integration.** `egress::redact::tests::a_finding_never_carries_the_matched_text` (the derived `Debug` starts rendering the secret), `egress::tests::a_high_confidence_finding_blocks_with_its_kind_span_and_locus`, `tests/redact_egress.rs::{no_emitted_surface_carries_the_sentinel, a_planted_credential_with_clean_provenance_is_blocked_and_the_event_names_redaction}`. **`teton_protocol::events::tests::a_redaction_cause_carries_only_a_kind_and_a_span` stays green** and it is right to: that test guards `BlockCause::Redaction`'s key set, and this variant of the mutation rides the text on `PrivacyBlock.path`, which is a `String` either way. The wire-key-set test covers the *other* variant (a new field on the cause); the sentinel sweep covers this one. Both are needed. |
//! | **(d)** an id-based locality assertion, restored (BR-2) | two placements, see below | **RED at both layers** (one of them only after a fixture was added — see the note) |
//! | **(e)** a prefix shape matches mid-word (the precision mutation) | `egress/redact.rs`, `scan_prefix_shape`: delete the `if !is_left_boundary(bytes, i) { i += 1; continue; }` guard | **3 lib, 0 integration.** `egress::redact::tests::{every_prefix_shape_requires_a_left_word_boundary, only_an_escape_that_decodes_to_a_non_word_character_is_a_boundary, the_pattern_pass_locates_each_shape_and_nothing_else}`. Re-applied to a freshly built workspace after the round-2 boundary rewrite, observed (708 passed / 3 failed), reverted. That **nothing else** turns red is the other half of the check: the boundary requirement loses no true positive, so every existing shape fixture stays green. `…::a_credential_at_the_start_of_a_json_encoded_line_is_still_found` stays green too, and correctly: it is a *recall* fixture, and deleting a precision guard cannot lose recall — its discriminating mutation is the opposite one, restoring the alphabet-relative predicate (below) |
//! | **(e′)** the left boundary goes back to the shape's own body alphabet (`is_word_left: is_sk_body` / `is_upper_alnum`) instead of `is_ascii_alphanumeric`, and the string-escape exception is dropped | `egress::redact::tests::{a_credential_at_the_start_of_a_json_encoded_line_is_still_found, only_an_escape_that_decodes_to_a_non_word_character_is_a_boundary, the_pattern_pass_locates_each_shape_and_nothing_else}`. This is the direction that **shipped broken** in round 1: the scan reads the *serialized* body, where a content newline is backslash + `n`, so every credential at the start of a content line was preceded by a "word byte" and skipped — a JSON body with four line-start credentials detected exactly one |
//!
//! **(d) has two plausible homes, and on the first run only one of them was
//! covered.** Both are red now:
//!
//! - **In `harness::redact::scan`** — `if route.provider() != Some("local") {
//!   return RedactionVerdict::unavailable(); }` after the unresolved check.
//!   **RED**: `tests/redact_egress.rs::an_engine_backed_local_tier_under_another_id_still_serves_the_scan`
//!   and `harness::redact::tests::a_scan_that_overruns_the_deadline_is_unavailable`.
//! - **In `runtime::RedactionGateImpl::redact_route`** — the same comparison
//!   between the resolve and the engine-slot read:
//!   ```text
//!   if provider_id != LOCAL_PROVIDER_ID {
//!       return DutyRoute::unresolved(format!(
//!           "The 'redact' category resolved to '{provider_id}', which is not the local tier."
//!       ));
//!   }
//!   ```
//!   **RED**: `runtime::tests::dispatch::redact::an_engine_backed_local_tier_under_another_id_still_serves_the_scan`
//!   (`left: Unavailable, right: Clean`). **On the first run this was GREEN** —
//!   see immediately below, because the history is the reason the fixture exists.
//!
//! ## The green observation that produced the fixture (kept on the record)
//!
//! On TASK-071's first AC-8 pass the runtime-layer placement of (d) turned
//! **nothing** red: `cargo test -p tetond --lib` reported 684 passed, 0 failed
//! with the guard in place.
//!
//! `local_tier_id` returns the id of any provider declaring `kind = "local"`,
//! which is very often *not* the string `local` — a user who writes
//! `id = "on-device"` has a genuinely engine-backed tier under another name. An
//! id comparison inside `RedactionGateImpl::redact_route` fails that machine's
//! scan closed, and — since the gate sits on the synchronous send path — every
//! one of its remote turns with it. The suite could not see it: every
//! `runtime::tests::dispatch::redact` fixture built its router from a config
//! whose local tier carried the canonical id, so the guard could never fire.
//! `tests/redact_egress.rs::an_engine_backed_local_tier_under_another_id_still_serves_the_scan`
//! covered the same property one layer down — a real `Router` whose
//! `local_provider_id` is `on-device`, driving the real `scan` — but
//! `RedactionGateImpl` is private to this crate, so no integration test can
//! reach the daemon's own copy of the resolver.
//!
//! The gap was reported first and closed second, in that order, by
//! `runtime::tests::dispatch::redact::an_engine_backed_local_tier_under_another_id_still_serves_the_scan`:
//! one config (`[[providers]] id = "on-device", kind = "local"`) and the
//! assertion that the scan resolves, serves, and announces its route under that
//! id. Re-running the identical mutation against a freshly built workspace after
//! the fixture landed turns it red. The green observation is kept here rather
//! than deleted: it is what the fixture is *for*, and a mutation table that only
//! records reds cannot show which of its rows were ever load-bearing.
//!
//! ### REQ-562 round 3 — the model pass is chunked, and the mutations that pin it
//!
//! The scan's cap was one number doing two jobs: the engine's window *and* the
//! bound on what could be scanned. Because the window (27,070 bytes) sits under
//! `HarnessConfig::context_budget_bytes` (32,768), every context-budget-full
//! remote turn assembled a body the scan refused, and blocked. The model pass
//! now cuts the payload into overlapping windows and scans each one
//! ([`crate::harness::redact::chunk_ranges`]); the pattern pass, which has no
//! window, still sweeps the whole payload in one piece.
//!
//! Three properties carry the change, and each has a mutation. All three were
//! applied to a **freshly built workspace** (`cargo build --workspace` first —
//! LESSON-489), run with `--no-fail-fast`, and reverted.
//!
//! | # | Mutation (exact diff) | Turns red |
//! |---|---|---|
//! | **(f)** the overlap is removed — `chunk_ranges`: `start = end - REDACT_CHUNK_OVERLAP_BYTES;` → `start = end;` | **5 lib.** `harness::redact::tests::{a_secret_straddling_a_chunk_boundary_is_still_found_whole_in_the_overlap, a_secret_inside_the_overlap_is_reported_once_not_once_per_chunk, the_chunker_covers_the_payload_with_overlapping_windows_on_char_boundaries, the_chunker_never_cuts_more_chunks_than_the_derived_ceiling, an_over_cap_payload_is_unavailable_before_any_model_call}` (727 passed / 5 failed). The straddling fixture is the discriminator: its secret has **no pattern shape**, so with abutting chunks neither half locates it and the finding vanishes. A `sk-…` fixture here would stay green, because the pattern pass never chunked |
//! | **(g)** a chunk that could not run is skipped instead of failing the scan — `scan`: both `else { return RedactionVerdict::unavailable_with_evidence(established()); }` arms in the chunk loop → `else { continue; }` | **5 lib.** `harness::redact::tests::{a_chunk_that_could_not_run_makes_the_whole_verdict_unavailable, a_credential_the_pattern_pass_established_survives_a_failed_chunk, an_engine_failure_is_unavailable_not_clean, an_unreadable_answer_blocks_rather_than_passing_as_clean, a_scan_that_overruns_the_deadline_is_unavailable}` (728 passed / 5 failed). This is the permissive direction with a new door: the composed verdict reads `Clean` → **Forward** for a payload part of which no model saw — a truncate-and-scan in different clothes (BR-7, LESSON-447). The single-chunk fixtures turn red too, and that is the point: with one chunk, "skip the chunk" *is* "skip the scan". (The second fixture was named `…does_not_outrank_a_chunk_that_failed` when this row was recorded; it was renamed with the round-4 reversal below, and the mutation still reddens it for the same reason — the outcome, not the cause, is what it asserts here) |
//! | **(h)** the chunk-relative span is not translated — `read_findings`: `locate(chunk, quoted).map(\|span\| span.start + offset..span.end + offset)` → `locate(chunk, quoted)` | **3 lib.** `harness::redact::tests::{a_model_finding_from_the_second_chunk_carries_a_payload_absolute_span, a_secret_straddling_a_chunk_boundary_is_still_found_whole_in_the_overlap, a_secret_inside_the_overlap_is_reported_once_not_once_per_chunk}` (730 passed / 3 failed). The span still *looks* well formed — right length, inside the payload — and points thousands of bytes early, at filler the model never mentioned. The assertion that catches it is on the bytes the span selects (`&payload[span] == ADDRESS`), never on the numbers |
//! | **(i)** the render guard is hoisted out of the chunk loop and applied to the whole payload | **10 lib + 1 integration.** Every chunked fixture, plus `tests/redact_egress.rs::a_context_budget_full_payload_is_scanned_across_windows_and_forwards` (723 passed / 10 failed; 9 passed / 1 failed). A payload of several windows renders past a single window by construction, so a hoisted guard refuses every multi-chunk scan — the old collision, restored one layer in. Its subtler sibling (hoist it but measure only the first chunk) is red on `…::a_second_chunk_that_renders_past_the_window_is_refused_before_its_own_call`, whose call count is `1` precisely because neither `0` nor `2` is reachable with a per-chunk guard |
//!
//! **What did *not* change, and is worth saying because a reviewer will look
//! for it.** `route_decided` fires once per [`DutyRoute::perform`], so an
//! N-chunk scan announces N times. That is this seam's stated rule — "two
//! oversized tool results are two routed model calls" — applied to a duty that
//! now makes several calls per send, and it is honest: the events describe
//! model calls, and a chunked scan really is several. Collapsing them would
//! under-report exactly the sends that cost the most.
//! `harness::redact::tests::a_multi_chunk_scan_announces_its_route_once_per_chunk`
//! pins the count in both directions (one chunk → one, two chunks → two, a scan
//! that never reaches a call → none), so a later "deduplicate the announcement"
//! change has to argue with a test rather than slip past one.
//!
//! ### REQ-562 round 4 — a failed chunk no longer erases what the pattern pass found
//!
//! Round 3's composition rule was right about the *outcome* and wrong about the
//! *cause*. The deterministic pass has no window and completes over the whole
//! payload before the chunk loop starts, so a chunk failure would discard a
//! finished High finding and report `ScanUnavailable` — which (correctly) does
//! not pin the session. Net effect: one transient engine stall both mis-told the
//! user "the scan could not run" about a payload where a credential *was* found,
//! and unmade a session pin the deterministic pass had earned.
//!
//! The fix keeps the outcome (`Unavailable`, `scanned: false`, Block) and moves
//! only what the block may say: the pattern pass's High findings ride the verdict
//! as `RedactionVerdict::evidence`, and `egress::block_cause` reads them.
//! Both mutations were applied to a **freshly built workspace** (LESSON-489),
//! run with `--no-fail-fast`, and reverted.
//!
//! | # | Mutation (exact diff) | Turns red |
//! |---|---|---|
//! | **(j)** the cause ignores the evidence — `egress::block_cause`: `verdict.findings().iter().chain(verdict.evidence())` → `verdict.findings().iter()` | **1 lib.** `egress::tests::an_unavailable_verdict_names_a_credential_only_when_the_pattern_pass_found_one` (734 passed / 1 failed). This is the pre-fix behaviour restored exactly: the verdict still carries the finding, and the block still reports "could not run" — and with it goes the `Redaction` cause that is the *only* thing pinning the session, so the mutation is silently permissive one layer downstream |
//! | **(k)** the scan mints no evidence — `scan`: all three `RedactionVerdict::unavailable_with_evidence(established())` in the chunk loop → `RedactionVerdict::unavailable()` | **1 lib.** `harness::redact::tests::a_credential_the_pattern_pass_established_survives_a_failed_chunk` (734 passed / 1 failed). The pair to (j) at the other end of the seam: (j) breaks the reading, (k) breaks the writing, and each is caught by the test on its own side. Its twin `a_clean_payload_whose_chunk_failed_still_reports_only_that_it_could_not_run` stays **green** under both, which is what makes it the discriminator rather than a second copy |
//!
//! ### REQ-562 round 4 — the declared bounds become enforced code
//!
//! Three bounds this REQ had written down but not checked. Each is a line, and
//! each was mutated against a **freshly built workspace** (LESSON-489) with
//! `--no-fail-fast`, then reverted.
//!
//! | # | Mutation (exact diff) | Turns red |
//! |---|---|---|
//! | **(l)** the scan-wide deadline is removed — `scan`: `tokio::time::timeout(DUTY_DEADLINE, async { … }).await` → the same block awaited bare, wrapped in `Ok(…)` so the `let-else` still typechecks | **1 lib.** `harness::redact::tests::a_scan_whose_chunks_each_answer_in_time_still_stops_at_one_scan_deadline` (736 passed / 1 failed). Only that one, and that is the design of the fixture: every other deadline test has a chunk that *overruns*, which the seam's per-call deadline catches on its own. The discriminating shape is a first chunk answering at ⅔ of the budget and a second that stalls — both inside the per-call bound, `1⅔ × DUTY_DEADLINE` of total wait, and only the elapsed assertion sees it |
//! | **(m)** the chunk ceiling never refuses — `past_the_chunk_ceiling`: `ranges.len() > REDACT_MAX_CHUNKS` → `false` | **1 lib.** `harness::redact::tests::the_chunk_ceiling_refuses_a_cut_past_it` (736 passed / 1 failed). Only the unit test, necessarily: the guard is unreachable through `scan` today because the total cap *is* `REDACT_MAX_CHUNKS` windows, which is stated in the fixture rather than left for a reader to wonder about. It is the bound BR-7 asks for, kept for the day one of the four derived constants moves |
//! | **(n)** the stride goes to zero — `REDACT_CHUNK_OVERLAP_BYTES: 256` → `REDACT_CHUNK_MAX_BYTES` | **The build.** `const _: () = assert!(REDACT_CHUNK_STRIDE_BYTES > 0)` fails to evaluate, naming the stride. Recorded honestly: `REDACT_MAX_CHUNKS`'s `div_ceil` *also* fails on this input, as a divide-by-zero two derivations away from the loop that would hang — so today the assert is the better error rather than the only one, and it is what still holds if that derivation changes shape |
//!
//! ## What draining past the ceiling buys, and why it is still done
//!
//! Past the ceiling [`RemoteDuty::perform`] stops *accumulating* but keeps
//! draining the provider's stream. Breaking out instead looks like the obvious
//! saving, and the reason it is not has moved but not gone away.
//!
//! It used to be that an early break **unbilled the call outright**:
//! `MeteredBody` recorded on the terminal `None` of the response body and had no
//! `Drop` fallback. That was a hole rather than a trade — [`DUTY_DEADLINE`] hit
//! it too, since `tokio::time::timeout` drops the future that owns the body — so
//! it is closed at the meter, where cancellation safety belongs, and a dropped
//! body now records what it saw.
//!
//! What survives is an accuracy argument. Both supported provider families
//! report **output** tokens only in their final chunk (Anthropic's terminal
//! `message_delta`, OpenAI's terminal usage chunk), so a stream cut short is
//! billed for its input and for whatever output was reported by then — real, but
//! short. The ceilings are tight enough (`title`'s is 128 bytes) that ordinary
//! chatty answers cross them, which would make "overran its ceiling" and
//! "under-billed" the same set — and a systematic under-count reads as a
//! cheaper harness rather than as a bug. Draining costs bytes off a socket
//! already open; memory is bounded either way, because the accumulator stops
//! growing. The unbounded *wait* was the real harm and the deadline is what
//! bounds it.
//!
//! ## One mutation came back green, and it is recorded rather than fixed
//!
//! Making the loop's gate conditional the **other** way —
//! `if compaction.degraded { ctx.truncate_to_budget(); }` — leaves the whole
//! suite passing. That is not a coverage gap: within the loop it is an
//! *equivalent* mutant, and the three non-degraded outcomes are why.
//!
//! 1. **Applied.** [`ContextManager::compact_if_pressured`](super::context::ContextManager::compact_if_pressured)
//!    commits a candidate only after checking it fits both budgets, so a
//!    successful compaction ends under budget and the gate is a no-op.
//! 2. **Declined for want of pressure.** Under the soft threshold in both
//!    currencies implies under budget in both, so again a no-op.
//! 3. **Declined for want of blocks.** This one *can* be over budget — verified:
//!    a two-block conversation at 6,211 B against a 4,000 B budget declines with
//!    `degraded: false` — but it is unreachable from the loop, which has pushed
//!    a model block and a tool-result block on top of the caller's user turn
//!    before compaction runs, so `blocks.len() >= COMPACT_MIN_BLOCKS` always.
//!
//! Reachable through the public method, though, so the state is pinned at the
//! manager by `harness::context::tests::a_conversation_too_short_to_hold_a_decision_buys_no_compact_call`,
//! which TASK-065 strengthened with the over-budget assertion the gate's
//! necessity rests on. The equivalence is a property of today's single caller,
//! not a guarantee: a second caller that skipped the gate on a non-degraded
//! outcome would be over budget, which is why the call stays unconditional.

use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use futures::StreamExt;

use teton_inference::{Engine, GenParams};
use teton_protocol::events::{Event, RouteDecided};
use teton_protocol::{Category, ProviderId, SessionId};
use teton_providers::{Message, Provider, Role, Transport, TurnEvent, TurnRequest};

use crate::broadcast::EventBus;
use crate::cost::CostAttribution;
use crate::egress::{Egress, EgressContext, Provenance};

use super::context::floor_char_boundary;
use super::render::render_duty;

/// What a duty *is*, independent of where it runs: the two facts every
/// implementation needs and neither of which depends on the tier.
///
/// Carried as one value rather than two parameters because they always travel
/// together and are always stated together — one `const` per category, beside
/// that category's output contract, is the entire per-category surface ADR-3
/// allows. The route builders take it and the [`Duty`] impls project it back
/// out through [`Duty::category`] and [`Duty::ceiling_bytes`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DutyKind {
    category: Category,
    ceiling_bytes: usize,
}

impl DutyKind {
    /// A duty of `category` whose output is bounded to `ceiling_bytes` (BR-8).
    #[must_use]
    pub const fn new(category: Category, ceiling_bytes: usize) -> Self {
        Self {
            category,
            ceiling_bytes,
        }
    }

    /// The category this duty attributes its spend to and routes through.
    #[must_use]
    pub const fn category(self) -> Category {
        self.category
    }

    /// The harness-owned byte ceiling on what this duty may return.
    #[must_use]
    pub const fn ceiling_bytes(self) -> usize {
        self.ceiling_bytes
    }

    /// The generation budget this duty **requests**, sized from its own ceiling.
    ///
    /// The request and the bound are two different things and both have to exist
    /// (LESSON-484). [`Self::ceiling_bytes`] is the bound — enforced by
    /// [`bound_to_ceiling`] on both legs, because a provider or an engine that
    /// runs on is not a hypothetical. This is the *request*, and it exists so a
    /// duty does not pay to generate bytes it can never keep: a shared default
    /// asks for the same number of tokens for a `title` (a handful of words) as
    /// for a `compact` (a conversation), which for the tight ceilings means
    /// generating several times over and throwing the excess away.
    ///
    /// Sized at [`DUTY_REQUEST_BYTES_PER_TOKEN`] bytes per token — deliberately
    /// *below* what real BPE averages for prose, so the request can always cover
    /// the ceiling and the ceiling stays the thing that binds. Capped at
    /// [`DUTY_MAX_TOKENS_REQUEST`] because the loosest ceilings would otherwise
    /// ask for more output than a mainstream provider will accept in one call,
    /// turning a routed duty into a 400.
    #[must_use]
    pub const fn max_tokens(self) -> u32 {
        let requested = self.ceiling_bytes / DUTY_REQUEST_BYTES_PER_TOKEN;
        if requested > DUTY_MAX_TOKENS_REQUEST as usize {
            DUTY_MAX_TOKENS_REQUEST
        } else if requested == 0 {
            // Totality guard: a ceiling under one token's worth of bytes still
            // asks for something, so the duty degrades by truncation rather than
            // by being handed a budget of nothing.
            1
        } else {
            requested as u32
        }
    }
}

/// Bytes per token assumed when sizing a duty's generation *request* from its
/// byte ceiling ([`DutyKind::max_tokens`]).
///
/// Deliberately conservative — real BPE averages nearer four bytes per token on
/// English prose and code, so asking at two keeps the byte ceiling, not the token
/// request, as the thing that actually bounds the answer. Erring the other way
/// would make `max_tokens` the real bound and the declared ceiling decorative,
/// which is the shape LESSON-443 warns about: a guard that can never fire.
///
/// **An estimate, not a bound.** Real BPE on prose and code runs nearer four
/// bytes per token, but dense punctuation, base64 and CJK can all run *under*
/// two — so a byte count divided by this can under-state a real token count.
/// That is safe on the output side, where the byte ceiling binds regardless.
/// It is what makes REQ-562's derived per-chunk cap a *filter* rather than a
/// proof: see
/// [`REDACT_CHUNK_MAX_BYTES`](crate::egress::redact::REDACT_CHUNK_MAX_BYTES),
/// which reads this constant rather than restating it, and the measured render
/// guard in [`crate::harness::redact::scan`] that stands behind it, per chunk.
pub(crate) const DUTY_REQUEST_BYTES_PER_TOKEN: usize = 2;

/// Ceiling on the `max_tokens` any duty asks a provider for.
///
/// Not a safety bound — [`bound_to_ceiling`] is — but a compatibility one: the
/// loosest duty ceiling is 32 KiB, and a request for the ~16k output tokens that
/// implies is above the per-call output cap of several mainstream models, which
/// answer it with a 400 rather than with a shorter completion. 4,096 is at or
/// under every supported provider's cap and still well above what any duty
/// realistically writes.
const DUTY_MAX_TOKENS_REQUEST: u32 = 4_096;

/// How long a single duty call may take before the harness stops waiting.
///
/// A duty is best-effort by construction (BR-3), so the interesting failure is
/// not "it errored" but "it never answered": a provider that streams without
/// ever completing, a connection that stalls after its headers, an engine that
/// will not return. None of those produce an error to degrade on, so without a
/// deadline the call site simply waits — and every one of the five call sites is
/// on the path of a turn a person is watching.
///
/// Generous rather than tight, because the local tier is the common case and a
/// `compact` over a full conversation on a small machine is genuinely slow. The
/// number this replaces is not a smaller number; it is no number at all.
pub const DUTY_DEADLINE: Duration = Duration::from_secs(120);

/// Bound `text` to `ceiling` bytes — **the** ceiling-enforcement site on the duty
/// path (BR-8, LESSON-484).
///
/// One function rather than one per implementation, and called by both, because
/// a ceiling enforced on only one leg is a ceiling on none of some duties'
/// production paths: `title` is `reflex`-tier and has no remote implementation
/// at all, so a remote-only bound would leave the tightest ceiling of the five
/// unenforced everywhere it actually runs.
///
/// The cut is at a character boundary rather than at the byte, so a bound that
/// lands mid-codepoint drops that character instead of producing invalid UTF-8;
/// the result is trimmed because a bound applied to a model's answer routinely
/// lands on trailing whitespace.
fn bound_to_ceiling(text: &str, ceiling: usize) -> String {
    let bounded = if text.len() > ceiling {
        &text[..floor_char_boundary(text, ceiling)]
    } else {
        text
    };
    bounded.trim().to_owned()
}

/// Something that can serve one duty call (ADR-2).
///
/// `Send + Sync` because the turn loop holds the route across awaits and drives
/// it from any task. The trait is deliberately **narrow**: one prompt in, one
/// bounded string or one error message out. Every duty returns text — a
/// structured decision is parsed *at the call site*, never inside the seam,
/// because a parser here would make the seam aware of the category it is meant
/// to be indifferent to.
#[async_trait]
pub trait Duty: Send + Sync {
    /// The duty's own category — used for cost attribution and `route_decided`.
    fn category(&self) -> Category;

    /// The harness-owned output ceiling (BR-8), enforced by the implementation
    /// rather than requested of the provider (LESSON-484).
    fn ceiling_bytes(&self) -> usize;

    /// Perform the duty over `prompt`, whose embedded content came from
    /// `provenance`.
    ///
    /// `provenance` is the egress provenance of the *content being sent*, not of
    /// the conversation. A local implementation ignores it and has no transport
    /// to use it with; a remote one MUST scope its egress by it.
    ///
    /// # Errors
    /// Returns the failure as a broadcast-safe sentence — an engine error, a
    /// provider error, or a refusal at the choke point. Never the model's own
    /// output and never the content.
    async fn perform(&self, prompt: &str, provenance: &Provenance) -> Result<String, String>;
}

/// The `route_decided` a duty publishes when it actually runs (BR-2).
///
/// Holds the payload the resolver already projected off its `Route` rather than
/// the `Route` itself: the seam has no business knowing about routing types, and
/// projecting the event twice is the drift ADR-D exists to prevent.
///
/// Public only because it rides a public enum variant's field. Its own fields
/// are private and it has no constructor, so the only way to attach one is
/// [`DutyRoute::announcing`] and the only thing that can fire one is
/// [`DutyRoute::perform`] — nothing outside this module can announce a duty
/// route that no duty ran.
pub struct DutyAnnouncement {
    bus: Arc<EventBus>,
    session_id: Option<SessionId>,
    decided: RouteDecided,
}

impl DutyAnnouncement {
    fn publish(&self) {
        self.bus.publish(
            self.session_id.clone(),
            Event::RouteDecided(self.decided.clone()),
        );
    }
}

// ── A trap the next four duty tasks will otherwise rediscover ──────────────
//
// `call_sites.rs`'s derived-marker test scans the daemon's production source as
// **text** — doc comments and ordinary comments included, before anything is
// compiled — looking for a router-resolution call with a `Category::` literal
// inside its parentheses. Spelling that call out in prose anywhere in `src/`,
// even as an illustration and even with a placeholder variant name, makes the
// scan read the prose as a real call site, fail to match the placeholder
// against any category, and turn red with "the scan cannot tell which category
// this dispatches on". Describe the resolver in words; never write its
// spelling. (Cost of learning this the hard way: one confusing red test in a
// file that had not yet been compiled into the crate.)

/// One duty category, resolved for this turn.
///
/// Two states, and the second is not an error to be swallowed: an unresolvable
/// binding is a **routing failure with a reason**, and the reason is the
/// resolver's own sentence (BR-6 — this type mints no explanation of its own).
/// The call site reports it and still enforces its invariant by degraded means.
pub enum DutyRoute {
    /// Resolved: `provider_id` serves the duty through `duty`.
    Serves {
        /// The provider the duty's category resolved to. Carried so the failure
        /// log — and a test — can say *where* a duty went, rather than only that
        /// one happened.
        provider_id: String,
        /// What performs the call. One `dyn` object, not a type parameter
        /// (ADR-1): a duty performs a model call, so one vtable indirection is
        /// free next to the inference.
        duty: Arc<dyn Duty>,
        /// What to announce when this duty actually runs, or `None` for a route
        /// no router decided — the transport-free offline entry point builds one
        /// of those, and a path with no routing decision has nothing to report.
        announce: Option<DutyAnnouncement>,
    },
    /// Unresolved: nothing can serve this duty for this turn.
    Unresolved {
        /// The resolver's sentence, verbatim.
        reason: String,
    },
}

impl DutyRoute {
    /// A route served by the local tier's `engine`.
    #[must_use]
    pub fn local(
        kind: DutyKind,
        provider_id: impl Into<String>,
        engine: Arc<Mutex<dyn Engine>>,
    ) -> Self {
        DutyRoute::Serves {
            provider_id: provider_id.into(),
            duty: Arc::new(LocalDuty { kind, engine }),
            announce: None,
        }
    }

    /// A route served by a remote `provider`, reaching the network only through
    /// `egress`.
    #[must_use]
    pub fn remote<T: Transport + 'static>(
        kind: DutyKind,
        provider_id: impl Into<String>,
        provider: Box<dyn Provider>,
        egress: Egress<T>,
        model: impl Into<String>,
        session_id: impl Into<SessionId>,
    ) -> Self {
        let provider_id = provider_id.into();
        DutyRoute::Serves {
            duty: Arc::new(RemoteDuty {
                kind,
                provider,
                egress,
                provider_id: ProviderId::from(provider_id.clone()),
                model: model.into(),
                session_id: session_id.into(),
            }),
            provider_id,
            announce: None,
        }
    }

    /// A route that resolved to nothing, explained by `reason`.
    #[must_use]
    pub fn unresolved(reason: impl Into<String>) -> Self {
        DutyRoute::Unresolved {
            reason: reason.into(),
        }
    }

    /// Attach the `route_decided` this route will announce **if and when** the
    /// duty actually runs (BR-2).
    ///
    /// `decided` is `Option` for the same reason
    /// [`Router::emit_route_decided`](crate::router::Router::emit_route_decided)
    /// self-guards: a resolution that selected no provider projects no event, so
    /// a duty that could not be routed announces nothing rather than announcing
    /// a decision that was never made. A no-op on an [`Unresolved`] route, which
    /// can never reach [`Self::perform`]'s publishing arm anyway.
    ///
    /// [`Unresolved`]: DutyRoute::Unresolved
    #[must_use]
    pub fn announcing(
        self,
        bus: &Arc<EventBus>,
        session_id: Option<SessionId>,
        decided: Option<RouteDecided>,
    ) -> Self {
        match (self, decided) {
            (
                DutyRoute::Serves {
                    provider_id, duty, ..
                },
                Some(decided),
            ) => DutyRoute::Serves {
                provider_id,
                duty,
                announce: Some(DutyAnnouncement {
                    bus: Arc::clone(bus),
                    session_id,
                    decided,
                }),
            },
            (route, _) => route,
        }
    }

    /// Run the duty, announcing its route first (BR-2).
    ///
    /// The announcement is published **before** the call rather than after it,
    /// so the ordering a client observes matches the turn path's — the route,
    /// then whatever that route produced (a `privacy_block`, a `cost` record).
    /// It fires once per invocation and is not deduplicated: two oversized tool
    /// results are two routed model calls, and collapsing them would under-report
    /// egress on exactly the turns that egress most.
    ///
    /// A duty that never answers is a duty that failed (BR-3)
    ///
    /// [`DUTY_DEADLINE`] is applied here rather than at each call site for the
    /// reason the ceiling is enforced here: five call sites each remembering to
    /// bound their own wait is five chances to forget, and the one that forgets
    /// is the one that hangs a turn. Past the deadline the call site is handed a
    /// sentence like any other failure and takes the degraded path it already
    /// has — a mechanical truncation, the tool's own unrefined result, an
    /// unnamed session, a deterministic drop.
    ///
    /// It is a deadline, not a cancellation: a [`LocalDuty`] completion is
    /// already running on the blocking pool and cannot be interrupted, so what
    /// this bounds is how long the *turn* waits, not how long the engine runs.
    /// That is the half that matters — the other end of the wait is a person.
    ///
    /// # Errors
    /// The duty's own failure sentence, the deadline's, or — for a route that
    /// resolved to nothing — the resolver's reason. Callers normally take the
    /// [`Unresolved`](DutyRoute::Unresolved) arm before reaching here, so that
    /// case is a belt rather than a path.
    pub async fn perform(&self, prompt: &str, provenance: &Provenance) -> Result<String, String> {
        match self {
            DutyRoute::Serves { duty, announce, .. } => {
                if let Some(announce) = announce {
                    announce.publish();
                }
                match tokio::time::timeout(DUTY_DEADLINE, duty.perform(prompt, provenance)).await {
                    Ok(result) => result,
                    Err(_) => Err(format!(
                        "the `{}` duty did not answer within {} seconds",
                        duty.category().as_str(),
                        DUTY_DEADLINE.as_secs()
                    )),
                }
            }
            DutyRoute::Unresolved { reason } => Err(reason.clone()),
        }
    }

    /// The provider serving this duty this turn, or `None` when it is
    /// unresolved.
    #[must_use]
    pub fn provider(&self) -> Option<&str> {
        match self {
            DutyRoute::Serves { provider_id, .. } => Some(provider_id),
            DutyRoute::Unresolved { .. } => None,
        }
    }
}

/// The local tier serving a duty: an [`Engine`] handle and **no transport**.
///
/// The absence is the guarantee, not an omission — this struct has no field a
/// network call could be made through, which is why [`Duty::perform`]'s
/// `provenance` argument is ignorable here without a boundary check. Adding one
/// would be a guard placed where it is convenient rather than where the decision
/// is made (LESSON-484); the decision is "which route", and it was already made.
struct LocalDuty {
    kind: DutyKind,
    engine: Arc<Mutex<dyn Engine>>,
}

#[async_trait]
impl Duty for LocalDuty {
    fn category(&self) -> Category {
        self.kind.category()
    }

    fn ceiling_bytes(&self) -> usize {
        self.kind.ceiling_bytes()
    }

    async fn perform(&self, prompt: &str, _provenance: &Provenance) -> Result<String, String> {
        let engine = Arc::clone(&self.engine);
        let prompt = prompt.to_owned();
        // The completion runs on the blocking pool (E-3): with a real llama.cpp
        // engine a duty takes seconds, and this is awaited from the async turn
        // loop, where running it inline would park the tokio worker and stall
        // every other session's RPCs.
        // The generation budget is this duty's own, not a shared default: a
        // `title` that may keep 128 bytes has no use for a `compact`'s token
        // budget, and asking for one buys latency that is thrown away at the
        // ceiling below.
        let max_tokens = self.kind.max_tokens();
        let result = tokio::task::spawn_blocking(move || {
            let params = GenParams {
                max_tokens,
                ..GenParams::default()
            };
            let guard = engine.lock().expect("engine mutex poisoned");
            // REQ-554 BR-7: a duty prompt gets the same template treatment an
            // agent turn does. The format is read from the guard already held
            // here, inside the blocking task: taking a second lock on the async
            // path to ask the engine its format would park a tokio worker behind
            // whatever completion currently owns the mutex (LESSON-448).
            let format = guard.chat_format();
            let rendered = render_duty(format, &prompt);
            guard
                .complete(&rendered, &params, &mut |_| true)
                .map(|completion| completion.text)
        })
        .await;
        match result {
            // Bounded on the way out, at the shared ceiling site (BR-8). An
            // engine budget is a request like a provider's is: a single
            // unbroken token carries as many bytes as the model feels like
            // emitting, and `title`'s answer lands in a session record that is
            // never revised.
            Ok(Ok(text)) => Ok(bound_to_ceiling(&text, self.ceiling_bytes())),
            Ok(Err(err)) => Err(err.to_string()),
            // Named by the duty that failed, like every other sentence the seam
            // mints. This is the **one** local implementation for all five
            // duties, so a hardcoded noun here reports a `title`, `triage`,
            // `shell` or `compact` join failure as a summarization failure — on
            // `RefinedOutcome::duty_error`, on `CompactionOutcome::reason`, and
            // in the turn loop's log, which are the three places a user or a
            // maintainer goes to find out which duty broke.
            Err(_) => Err(format!(
                "the local `{}` task did not complete",
                self.category().as_str()
            )),
        }
    }
}

/// A remote provider serving a duty, through the single egress choke point.
///
/// The provider is handed only the provenance-scoped `&dyn Transport` that
/// [`Egress::scoped`] produces, so it cannot reach the network any other way: a
/// duty over boundary-protected content is refused before a byte leaves (BR-1),
/// and an allowed one is metered into a `CostRecord` attributed to the duty's
/// own category (BR-2) — so a user who binds a tier remotely can see what their
/// harness duties are costing.
struct RemoteDuty<T: Transport> {
    kind: DutyKind,
    provider: Box<dyn Provider>,
    egress: Egress<T>,
    provider_id: ProviderId,
    model: String,
    session_id: SessionId,
}

#[async_trait]
impl<T: Transport> Duty for RemoteDuty<T> {
    fn category(&self) -> Category {
        self.kind.category()
    }

    fn ceiling_bytes(&self) -> usize {
        self.kind.ceiling_bytes()
    }

    async fn perform(&self, prompt: &str, provenance: &Provenance) -> Result<String, String> {
        let request = TurnRequest {
            model: self.model.clone(),
            // A duty is one instruction, not a conversation: no system prompt to
            // inherit and — crucially — **no tools**. A duty must not be able to
            // emit a tool call that the loop would then have to decide what to
            // do with.
            system: None,
            messages: vec![Message {
                role: Role::User,
                content: prompt.to_owned(),
            }],
            tools: Vec::new(),
            // This duty's own budget, sized from its own ceiling — the same
            // number the local leg asks its engine for, from the same place.
            max_tokens: self.kind.max_tokens(),
        };
        // BR-2: a duty has no lifecycle position, so it attributes no phase — but
        // it does attribute its category, which is the whole point of routing it.
        let attribution = CostAttribution::new(self.model.clone()).with_category(self.category());
        let ctx = EgressContext::new(self.provider_id.clone())
            .with_session(self.session_id.clone())
            .with_cost(attribution);
        // BR-1: the provenance of the *content being sent*, computed by the call
        // site from what it is handing over — narrower than the turn's context,
        // and the reason a `local-only` read is refused while the rest of the
        // conversation still goes. This is the one `Egress::scoped` on the duty
        // path.
        let transport = self.egress.scoped(provenance.clone(), ctx);

        let ceiling = self.ceiling_bytes();
        let mut stream = self
            .provider
            .stream_turn(request, &transport)
            .await
            .map_err(|err| err.to_string())?;
        let mut text = String::new();
        while let Some(event) = stream.next().await {
            match event.map_err(|err| err.to_string())? {
                // Bounded on the way in, at the ONE ceiling-enforcement site on
                // the duty path. `max_tokens` is a request, and a request is not
                // a guarantee: a provider that ignores it — or a stream that
                // simply does not stop — would otherwise grow this buffer
                // without limit, on paths whose entire purpose is to SHRINK
                // their input. Past the ceiling, stop accumulating and let the
                // stream drain.
                TurnEvent::TextDelta(delta) => {
                    if text.len() < ceiling {
                        text.push_str(&delta);
                    }
                }
                // A duty was offered no tools, so a tool call is a provider
                // ignoring the request; drop it rather than fold it into the
                // answer. `Completed` carries usage, which the meter reads off
                // the stream at the choke point.
                TurnEvent::ToolCall(_) | TurnEvent::Completed(_) => {}
            }
        }
        // The last delta may straddle the bound, so trim to it here rather than
        // refusing a partial delta above — dropping a whole delta would cut the
        // answer at an arbitrary earlier point for no gain.
        Ok(bound_to_ceiling(&text, self.ceiling_bytes()))
    }
}

/// Shared fixtures for the five duty modules' unit tests (REQ-561 verify).
///
/// The remote leg of every duty is exercised the same way — a transport that
/// records the bytes it was asked to send, a provider that actually puts its
/// request on that transport before answering, and a route wiring the two
/// through [`Egress`] — and each of `digest`, `triage`, `shell_duty`, `title`
/// and `compact` had grown its own byte-identical copy of all three.
///
/// That is precisely the "five parallel implementations of one concern" shape
/// BR-6 and ADR-1 exist to remove, left standing in the test code that proves
/// they were removed. AC-8's boundary is scoped to production source, so nothing
/// failed; five copies of a leak detector is still five places for one of them
/// to quietly stop detecting leaks.
///
/// It lives here, on the seam, because the thing being faked — a duty reaching a
/// network only through a provenance-scoped transport — is the seam's, not any
/// one category's. What stays per-module is the [`DutyKind`] each passes in and
/// the assertions each makes.
///
/// **Scoped deliberately to the five.** The same trio appears in this crate's
/// `tests/` integration targets and in [`crate::egress`]; those predate REQ-561,
/// are separate compilation units (a `#[cfg(test)]` lib module is not visible to
/// an integration test), and are left alone.
#[cfg(test)]
pub(crate) mod testing {
    use std::sync::{Arc, Mutex};

    use async_trait::async_trait;
    use futures::stream;
    use teton_core::entities::PrivacyBoundary;
    use teton_providers::transport::{
        HttpMethod, TransportError, TransportRequest, TransportResponse,
    };
    use teton_providers::{
        CapabilityProfile, Provider, ProviderError, StopReason, TokenUsage, Transport,
        TurnCompletion, TurnEvent, TurnRequest, TurnStream,
    };

    use super::{DutyKind, DutyRoute};
    use crate::egress::{Egress, NoopSink, RedactionGate};

    /// The request bodies a [`CaptureTransport`] was handed, newest last.
    ///
    /// Read through [`wire`]. This is the instrument every boundary claim in the
    /// five modules is made against: "no boundary content left the machine" is a
    /// statement about bytes on a wire, not about a return value.
    pub(crate) type Sent = Arc<Mutex<Vec<Vec<u8>>>>;

    /// A transport that records every request body it is asked to send.
    #[derive(Default, Clone)]
    struct CaptureTransport {
        sent: Sent,
    }

    #[async_trait]
    impl Transport for CaptureTransport {
        async fn execute(
            &self,
            request: TransportRequest,
        ) -> Result<TransportResponse, TransportError> {
            self.sent
                .lock()
                .expect("capture poisoned")
                .push(request.body);
            Ok(TransportResponse {
                status: 200,
                body: Box::pin(stream::empty()),
            })
        }
    }

    /// A provider that puts its request on the wire before answering, and can be
    /// told to stream its reply `repeat` times — i.e. to ignore `max_tokens`.
    ///
    /// The smallest adapter that can *leak*: it serializes the prompt into the
    /// transport it was handed, so a route that bypassed egress would show the
    /// prompt in the capture. That is what makes the boundary assertions
    /// non-vacuous rather than satisfied by a fixture that could never send.
    struct WireProvider {
        reply: String,
        repeat: usize,
    }

    #[async_trait]
    impl Provider for WireProvider {
        fn id(&self) -> &str {
            "wire"
        }
        fn capabilities(&self) -> CapabilityProfile {
            CapabilityProfile::default()
        }
        async fn stream_turn(
            &self,
            request: TurnRequest,
            transport: &dyn Transport,
        ) -> Result<TurnStream, ProviderError> {
            let body =
                serde_json::to_vec(&request).map_err(|e| ProviderError::Build(e.to_string()))?;
            transport
                .execute(TransportRequest {
                    method: HttpMethod::Post,
                    url: "https://api.example.com/v1/chat/completions".to_owned(),
                    headers: Vec::new(),
                    body,
                })
                .await
                .map_err(|err| match err {
                    TransportError::PrivacyBlocked(detail) => ProviderError::PrivacyBlocked(detail),
                    _ => ProviderError::Transport,
                })?;
            // `repeat` models a provider that ignores `max_tokens`: the same
            // delta over and over, which is what an unbounded accumulator would
            // happily grow on.
            let mut events: Vec<Result<TurnEvent, ProviderError>> = (0..self.repeat)
                .map(|_| Ok(TurnEvent::TextDelta(self.reply.clone())))
                .collect();
            events.push(Ok(TurnEvent::Completed(TurnCompletion {
                usage: TokenUsage::default(),
                stop_reason: StopReason::EndTurn,
            })));
            Ok(Box::pin(stream::iter(events)))
        }
    }

    /// A remote route for `kind` over a capturing transport, with `boundaries`
    /// enforced at the choke point.
    pub(crate) fn remote_duty_route(
        kind: DutyKind,
        boundaries: Vec<PrivacyBoundary>,
        reply: &str,
        repeat: usize,
    ) -> (DutyRoute, Sent) {
        remote_duty_route_gated(kind, boundaries, reply, repeat, None)
    }

    /// The same route, optionally with a redaction gate at its choke point —
    /// which is how the daemon builds it when `[privacy] redact` is on
    /// (REQ-562 TASK-070).
    ///
    /// `None` is the shape every pre-REQ-562 caller gets, and it is the *same*
    /// construction rather than a parallel one: a duty route built without a
    /// gate has to keep behaving exactly as it did, and that is only checkable
    /// if both come out of one builder.
    pub(crate) fn remote_duty_route_gated(
        kind: DutyKind,
        boundaries: Vec<PrivacyBoundary>,
        reply: &str,
        repeat: usize,
        gate: Option<Arc<dyn RedactionGate>>,
    ) -> (DutyRoute, Sent) {
        let transport = CaptureTransport::default();
        let sent = Arc::clone(&transport.sent);
        let mut egress = Egress::new(transport, boundaries, Arc::new(NoopSink));
        if let Some(gate) = gate {
            egress = egress.with_redaction_gate(gate);
        }
        let route = DutyRoute::remote(
            kind,
            "frontier",
            Box::new(WireProvider {
                reply: reply.to_owned(),
                repeat,
            }),
            egress,
            "claude-opus-4",
            "sess-1",
        );
        (route, sent)
    }

    /// Everything the capturing transport was asked to send, as one string.
    pub(crate) fn wire(sent: &Sent) -> String {
        sent.lock()
            .expect("capture poisoned")
            .iter()
            .map(|body| String::from_utf8_lossy(body).into_owned())
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeSet;
    use std::future::pending;
    use std::sync::atomic::{AtomicUsize, Ordering as AtomicOrdering};

    use super::{
        Duty, DutyKind, DutyRoute, Provenance, DUTY_DEADLINE, DUTY_MAX_TOKENS_REQUEST,
        DUTY_REQUEST_BYTES_PER_TOKEN,
    };
    use crate::call_sites::scan::{code_only, count, production_sources};
    use async_trait::async_trait;
    use std::sync::{Arc, Mutex};
    use teton_inference::{Completion, Engine, EngineError, GenParams};
    use teton_protocol::Category;

    /// The duty modules — where ADR-3 allows per-category source to live, and
    /// the only place it may.
    ///
    /// A census, not a bound: REQ-561 shipped five and REQ-562 adds `redact` as
    /// the sixth caller of this seam, so a task that wires a new duty adds its
    /// module here and the scans below start covering it. What the list is *for*
    /// is the rule beneath it — one `DutyKind` per duty module and no others,
    /// and no duty module carrying any of the seam's concerns — which is
    /// unchanged by how many entries it has. (`redact`'s call site is not in the
    /// harness at all: it is the egress choke point, REQ-562 ADR-1.)
    const DUTY_MODULES: [&str; 6] = [
        "harness/digest.rs",
        "harness/triage.rs",
        "harness/shell_duty.rs",
        "harness/title.rs",
        "harness/compact.rs",
        "harness/redact.rs",
    ];

    /// The one place in the daemon that maps text to a routing-category *name*,
    /// named here so the exception below is a stated decision rather than a hole
    /// in the scan (the shape `REPORTING_ONLY` established in
    /// [`crate::call_sites`]).
    ///
    /// `cost::ledger::category_from_wire` reads back a column the ledger wrote
    /// itself, and the value it produces is `teton_protocol::Category` — the
    /// wire twin, which has no conversion into the routing type. It is not on
    /// the duty path and cannot reach one. [`the_only_text_to_category_map_is_the_ledgers_own_round_trip`]
    /// pins both halves of that.
    const WIRE_ROUND_TRIP: &str = "cost/ledger.rs";

    /// Every production source, comments stripped, as `(path, code)`.
    fn code() -> Vec<(String, String)> {
        production_sources()
            .into_iter()
            .map(|(rel, src)| (rel, code_only(&src)))
            .collect()
    }

    /// The paths whose code contains `needle`.
    fn files_with(needle: &str) -> Vec<String> {
        code()
            .into_iter()
            .filter(|(_, src)| src.contains(needle))
            .map(|(rel, _)| rel)
            .collect()
    }

    /// The code of one production file.
    fn source_of(rel: &str) -> String {
        code()
            .into_iter()
            .find(|(path, _)| path == rel)
            .unwrap_or_else(|| panic!("no production source at {rel}"))
            .1
    }

    // =======================================================================
    // AC-8 — one seam serves all five duties.
    //
    // The architecture states the boundary in five points; each is one
    // assertion below, in order.
    // =======================================================================

    /// **AC-8, points 1 and 2.** Exactly one route type, exactly one `Duty`
    /// trait, and exactly two implementations of it — the local one and the
    /// remote one, generic over the prompt they carry and **not** over the
    /// category.
    ///
    /// This is the assertion ADR-1 was written to make writable. A
    /// `DutyRoute<T: Duty>` would monomorphise into five distinct types, which
    /// is the five-parallel-implementations outcome BR-6 forbids expressed in
    /// the type system instead of in copied code — and it would make this test
    /// unwritable, because five would then be the expected number.
    #[test]
    fn one_route_type_one_trait_and_two_implementations_serve_every_duty() {
        let mut route_enums: Vec<String> = Vec::new();
        let mut trait_defs: Vec<String> = Vec::new();
        let mut duty_impls: Vec<String> = Vec::new();
        for (rel, src) in code() {
            for (at, _) in src.match_indices("enum ") {
                let name: String = src[at + "enum ".len()..]
                    .chars()
                    .take_while(|c| c.is_ascii_alphanumeric() || *c == '_')
                    .collect();
                if name.ends_with("Route") {
                    route_enums.push(format!("{rel}: {name}"));
                }
            }
            for (at, _) in src.match_indices("trait Duty") {
                let _ = at;
                trait_defs.push(rel.clone());
            }
            duty_impls.extend(
                src.match_indices("Duty for")
                    .map(|_| rel.clone())
                    .collect::<Vec<_>>(),
            );
        }

        assert_eq!(
            route_enums,
            vec!["harness/duty.rs: DutyRoute"],
            "BR-6: exactly one route enum serves every duty. A second one is the \
             per-category plumbing this REQ removed, growing back."
        );
        assert_eq!(
            trait_defs,
            vec!["harness/duty.rs"],
            "and exactly one `Duty` trait defines what a duty is"
        );
        assert_eq!(
            duty_impls,
            vec!["harness/duty.rs", "harness/duty.rs"],
            "with exactly two implementations, both here: the local tier and the \
             remote one. A third is a duty that grew its own way of running."
        );

        // And the removed shape does not survive anywhere, under any name.
        for gone in ["DigestRoute", "Digester", "TriageRoute", "CompactRoute"] {
            assert!(
                files_with(gone).is_empty(),
                "the pre-REQ-561 per-category plumbing `{gone}` is back in {:?}",
                files_with(gone)
            );
        }
    }

    /// **AC-8, points 3 and 4.** One `Egress::scoped(` call on the duty path,
    /// and one ceiling-enforcement site.
    ///
    /// `.scoped(` occurs twice in the daemon and the two are named here rather
    /// than counted: the turn path's, in `harness/completion.rs`, and the duty
    /// path's, here. A *third* is a second way to reach the network, which is
    /// the thing the single choke point exists to make impossible.
    ///
    /// The ceiling is read in exactly one file, enforced by exactly one
    /// function, and **every** implementation of [`Duty`](super::Duty) runs its
    /// answer through it (LESSON-484 — enforced where the decision is made, not
    /// at each of five call sites).
    ///
    /// The per-leg half is the one this test learned the hard way. Counting the
    /// enforcement *site* is satisfied by a ceiling that exists on the remote
    /// leg and nowhere else, which is what shipped: `LocalDuty::perform` read a
    /// shared `GenParams::default()` and returned whatever the engine produced.
    /// For `title` that is not a partial gap but a total one — it is
    /// `reflex`-tier with no remote implementation at all, so its ceiling was
    /// enforced on **zero** of the paths it actually runs on, and its answer
    /// lands in a session record that is never revised.
    #[test]
    fn the_duty_path_has_one_egress_scoping_call_and_one_ceiling_site() {
        assert_eq!(
            files_with(".scoped("),
            vec!["harness/completion.rs", "harness/duty.rs"],
            "the turn path and the duty path, and nothing else, reach a transport"
        );
        assert_eq!(
            count(&source_of("harness/duty.rs"), ".scoped("),
            1,
            "and the duty path scopes its egress exactly once — every duty's \
             content goes through the same call"
        );

        assert_eq!(
            files_with("ceiling_bytes()"),
            vec!["harness/duty.rs"],
            "BR-8's bound is read only inside the seam; the duty modules declare \
             their ceiling and nothing else"
        );
        let seam = source_of("harness/duty.rs");
        assert_eq!(
            count(&seam, "fn bound_to_ceiling("),
            1,
            "and there is exactly one function that enforces it (LESSON-484)"
        );

        // Split at each `impl … Duty for …` header: every implementation of the
        // trait must bound its own answer. `no_duty_module_carries_any_of_the_seams_concerns`
        // already pins that the only two live here.
        let legs: Vec<&str> = seam.split("Duty for").skip(1).collect();
        assert_eq!(
            legs.len(),
            2,
            "the local leg and the remote leg, and this assertion knows about both"
        );
        for leg in legs {
            let bounded = leg
                .split("\nfn bound_to_ceiling(")
                .next()
                .expect("split always yields a first part");
            assert!(
                bounded.contains("bound_to_ceiling(&text, self.ceiling_bytes())"),
                "BR-8 VIOLATION: an implementation of `Duty` returns its answer \
                 without bounding it. A ceiling on one of the two legs is no \
                 ceiling at all for a duty that only has the other one:\n{}",
                &leg[..leg.len().min(400)]
            );
        }
    }

    /// **BR-8's request half.** Every duty asks for a generation budget sized
    /// from its **own** ceiling, not from one number shared by all five.
    ///
    /// A shared `GenParams::default()` asked for the same 256 tokens for a
    /// `title` (128 bytes, a handful of words) as for a `compact` (32 KiB, a
    /// conversation) — so the tight duties paid to generate several times what
    /// they could keep, and the loose ones were bounded by a number that has
    /// nothing to do with their declared contract. The ordering assertion is
    /// what makes this fail if the derivation is replaced by a constant again.
    #[test]
    fn every_duty_asks_for_a_generation_budget_sized_from_its_own_ceiling() {
        use crate::harness::{COMPACT_DUTY, DIGEST_DUTY, SHELL_DUTY, TITLE_DUTY, TRIAGE_DUTY};

        assert!(
            TITLE_DUTY.max_tokens() < TRIAGE_DUTY.max_tokens(),
            "a title asks for less than a ranking: {} vs {}",
            TITLE_DUTY.max_tokens(),
            TRIAGE_DUTY.max_tokens()
        );
        assert!(TRIAGE_DUTY.max_tokens() < DIGEST_DUTY.max_tokens());
        assert_eq!(SHELL_DUTY.max_tokens(), TRIAGE_DUTY.max_tokens());

        // The request must be able to cover the ceiling, or `max_tokens` is the
        // real bound and the declared ceiling is decorative (LESSON-443).
        for kind in [TITLE_DUTY, TRIAGE_DUTY, SHELL_DUTY] {
            assert!(
                kind.max_tokens() as usize * DUTY_REQUEST_BYTES_PER_TOKEN >= kind.ceiling_bytes(),
                "`{}` asks for {} tokens, too few to reach its own {}-byte ceiling",
                kind.category().as_str(),
                kind.max_tokens(),
                kind.ceiling_bytes()
            );
        }
        // And the two loosest are capped for provider compatibility rather than
        // by their ceilings — stated, so the exception is visible.
        assert_eq!(DIGEST_DUTY.max_tokens(), DUTY_MAX_TOKENS_REQUEST);
        assert_eq!(COMPACT_DUTY.max_tokens(), DUTY_MAX_TOKENS_REQUEST);

        // Totality: a ceiling smaller than one token's worth of bytes still asks
        // for something rather than for nothing.
        assert_eq!(DutyKind::new(Category::Title, 1).max_tokens(), 1);
    }

    // =======================================================================
    // A duty that never answers is a duty that failed (BR-3).
    // =======================================================================

    /// A duty that is asked, counts it, and then never returns — the failure a
    /// provider streaming without completing, or a stalled connection, actually
    /// produces. Neither is an `Err` to degrade on.
    struct NeverAnswers {
        calls: Arc<AtomicUsize>,
    }

    #[async_trait]
    impl Duty for NeverAnswers {
        fn category(&self) -> Category {
            Category::Title
        }
        fn ceiling_bytes(&self) -> usize {
            128
        }
        async fn perform(&self, _prompt: &str, _provenance: &Provenance) -> Result<String, String> {
            self.calls.fetch_add(1, AtomicOrdering::SeqCst);
            pending().await
        }
    }

    /// **The deadline.** A duty that never answers becomes a failure sentence
    /// the call site can degrade with, rather than a turn that waits forever.
    ///
    /// Run on a paused clock, so the assertion is about the deadline existing
    /// rather than about two minutes elapsing. Without the timeout this test
    /// does not fail — it hangs, which is precisely the production symptom.
    #[tokio::test(start_paused = true)]
    async fn a_duty_that_never_answers_fails_at_the_deadline() {
        let calls = Arc::new(AtomicUsize::new(0));
        let route = DutyRoute::Serves {
            provider_id: "stub".to_owned(),
            duty: Arc::new(NeverAnswers {
                calls: Arc::clone(&calls),
            }),
            announce: None,
        };

        let started = tokio::time::Instant::now();
        let err = route
            .perform("name this", &Provenance::empty())
            .await
            .expect_err("a duty that never answers cannot have succeeded");

        assert_eq!(
            calls.load(AtomicOrdering::SeqCst),
            1,
            "non-vacuity: the duty really was asked, so this is the deadline \
             firing rather than the route declining"
        );
        assert!(
            err.contains("did not answer within"),
            "the call site is handed an explanation, not a silence: {err}"
        );
        assert!(
            err.contains("title"),
            "and it names the duty that hung: {err}"
        );
        assert!(
            started.elapsed() >= DUTY_DEADLINE,
            "the wait really was the deadline's, not something shorter"
        );
    }

    /// **The deadline must not buy calls off the ledger** (REQ-561 verify).
    ///
    /// `tokio::time::timeout` does not cancel the work, it **drops the future**
    /// — and for a [`RemoteDuty`] that future owns the `TurnStream`, and through
    /// it the `MeteredBody` the choke point wrapped the response in. So the
    /// exact thing the deadline is for — a provider slow enough, or adversarial
    /// enough, to sit past two minutes — was also the thing that made its calls
    /// invisible: the request left the machine, the provider reported the input
    /// tokens it was charging for, and no `CostRecord` was ever written. Serving
    /// duties off-ledger indefinitely needed nothing more than stalling.
    ///
    /// This is the whole path, not the unit: a real [`Egress`] with a real
    /// [`CostLedger`] behind it, a provider that puts its prompt on the wire and
    /// streams the response body back, and [`DutyRoute::perform`]'s own
    /// deadline. Deleting `MeteredBody`'s `Drop` impl leaves the duty failing
    /// exactly as it does here and the ledger empty.
    #[tokio::test(start_paused = true)]
    async fn a_duty_cut_off_at_the_deadline_is_still_on_the_ledger() {
        use futures::StreamExt;
        use teton_providers::transport::{
            ByteStream, HttpMethod, TransportError, TransportRequest, TransportResponse,
        };
        use teton_providers::{
            CapabilityProfile, Provider, ProviderError, TurnEvent, TurnRequest, TurnStream,
        };

        use crate::cost::{CostLedger, CostMeter, NoopCostSink, PriceTable};
        use crate::egress::{Egress, NoopSink};

        /// A transport whose response starts and then stops: the usage the
        /// provider has already reported, and then nothing, forever.
        struct StallingTransport;

        #[async_trait]
        impl teton_providers::Transport for StallingTransport {
            async fn execute(
                &self,
                _request: TransportRequest,
            ) -> Result<TransportResponse, TransportError> {
                let first: Result<Vec<u8>, TransportError> = Ok(b"event: message_start\ndata: {\"message\":{\"usage\":{\"input_tokens\":900,\"output_tokens\":1}}}\n\n".to_vec());
                let body: ByteStream =
                    Box::pin(futures::stream::iter(vec![first]).chain(futures::stream::pending()));
                Ok(TransportResponse { status: 200, body })
            }
        }

        /// A provider that behaves like the real adapters: it sends through the
        /// transport it was handed and builds its `TurnStream` **from the
        /// response body**, so the body's lifetime is the duty future's. A
        /// fixture that dropped the response and answered from a canned list
        /// would pass this test with the fix reverted.
        struct StreamsTheBody;

        #[async_trait]
        impl Provider for StreamsTheBody {
            fn id(&self) -> &str {
                "stalling"
            }
            fn capabilities(&self) -> CapabilityProfile {
                CapabilityProfile::default()
            }
            async fn stream_turn(
                &self,
                request: TurnRequest,
                transport: &dyn teton_providers::Transport,
            ) -> Result<TurnStream, ProviderError> {
                let body = serde_json::to_vec(&request)
                    .map_err(|e| ProviderError::Build(e.to_string()))?;
                let response = transport
                    .execute(TransportRequest {
                        method: HttpMethod::Post,
                        url: "https://api.example.com/v1/messages".to_owned(),
                        headers: Vec::new(),
                        body,
                    })
                    .await
                    .map_err(|_| ProviderError::Transport)?;
                // The body is folded into the returned stream, so it lives
                // exactly as long as whoever holds that stream — which is the
                // duty future the deadline drops.
                Ok(Box::pin(futures::stream::unfold(
                    response.body,
                    |mut body| async move {
                        let chunk = body.next().await?;
                        let event = match chunk {
                            Ok(bytes) => {
                                Ok(TurnEvent::TextDelta(String::from_utf8_lossy(&bytes).into()))
                            }
                            Err(_) => Err(ProviderError::Transport),
                        };
                        Some((event, body))
                    },
                )))
            }
        }

        let ledger = Arc::new(
            CostLedger::open_in_memory(PriceTable::bundled(), Arc::new(NoopCostSink))
                .expect("in-memory ledger"),
        );
        let egress = Egress::new(StallingTransport, Vec::new(), Arc::new(NoopSink))
            .with_cost_meter(Arc::clone(&ledger) as Arc<dyn CostMeter>);
        let route = DutyRoute::remote(
            super::super::title::TITLE_DUTY,
            "frontier",
            Box::new(StreamsTheBody),
            egress,
            "claude-opus-4",
            "sess-stalled",
        );

        let err = route
            .perform("name this session", &Provenance::empty())
            .await
            .expect_err("a provider that never finishes cannot have answered");
        assert!(
            err.contains("did not answer within"),
            "non-vacuity: this is the deadline firing, not a transport error: {err}"
        );

        let rows = ledger.all_records().expect("read the ledger");
        assert_eq!(
            rows.len(),
            1,
            "a duty call that went out and burned tokens must appear on the ledger \
             even when the wait was cut short"
        );
        assert_eq!(rows[0].session_id, "sess-stalled");
        assert_eq!(
            rows[0].category,
            Some(Category::Title),
            "attributed to the duty that made it (BR-2)"
        );
        assert_eq!(
            rows[0].input_tokens, 900,
            "at the tokens the provider had already reported, not at zero"
        );
    }

    /// An engine whose completion panics on the blocking pool — the one way a
    /// `spawn_blocking` handle comes back as a `JoinError` rather than as a
    /// result the seam can read.
    struct PanickingEngine;

    impl Engine for PanickingEngine {
        fn model_id(&self) -> &str {
            "panicking"
        }
        fn complete(
            &self,
            _prompt: &str,
            _params: &GenParams,
            _on_token: &mut dyn FnMut(&str) -> bool,
        ) -> Result<Completion, EngineError> {
            panic!("the blocking pool lost this task");
        }
    }

    /// **The join-failure sentence names the duty that failed.**
    ///
    /// [`LocalDuty`] is the single local implementation behind all five duties,
    /// so this arm is reached by `title`, `triage`, `shell` and `compact` as
    /// readily as by `digest` — and its sentence surfaces to the user through
    /// `RefinedOutcome::duty_error`, `CompactionOutcome::reason`, and the turn
    /// loop's log. A hardcoded noun there tells a person that summarization
    /// failed on a turn that never summarized anything.
    ///
    /// Driven through `TITLE_DUTY` precisely because `title` is not the duty the
    /// old sentence named: the assertion is that the sentence *changed with the
    /// category*, which is exactly what a fixed string cannot do.
    #[tokio::test]
    async fn a_local_duty_that_never_joins_names_itself_rather_than_summarization() {
        let engine: Arc<Mutex<dyn Engine>> = Arc::new(Mutex::new(PanickingEngine));
        let route = DutyRoute::local(crate::harness::title::TITLE_DUTY, "local", engine);

        let err = route
            .perform("name this session", &Provenance::empty())
            .await
            .expect_err("a duty whose task panicked cannot have succeeded");

        assert!(
            err.contains("title"),
            "the call site must be told which duty failed: {err}"
        );
        assert!(
            !err.contains("summarization"),
            "one shared local impl must not report every duty as a summarization: {err}"
        );
    }

    /// **AC-8, point 5.** Per-category source is bounded to what ADR-3 allows: a
    /// resolver line naming the category, an output-contract constant, and a
    /// prompt builder. None of the plumbing.
    ///
    /// The list below is every concern the seam owns. A duty module that grows
    /// one of them has started reimplementing the seam, which is exactly the
    /// regression AC-8 exists to catch — and it is the shape REQ-558 shipped for
    /// `digest` and this REQ removed.
    #[test]
    fn no_duty_module_carries_any_of_the_seams_concerns() {
        /// `(needle, what having it would mean)`.
        const PLUMBING: [(&str, &str); 8] = [
            ("trait ", "a second definition of what a duty is"),
            ("Duty for", "a per-category duty implementation"),
            ("Egress", "its own path to a transport"),
            (".scoped(", "its own egress scoping"),
            ("spawn_blocking", "its own local-execution strategy"),
            ("EgressContext", "its own egress metadata"),
            ("CostAttribution", "its own cost attribution"),
            ("TurnRequest", "its own provider request"),
        ];
        for module in DUTY_MODULES {
            let src = source_of(module);
            for (needle, meaning) in PLUMBING {
                assert!(
                    !src.contains(needle),
                    "BR-6 VIOLATION: `{module}` contains `{needle}` — {meaning}. The seam \
                     owns that concern once, for every duty (ADR-3)."
                );
            }
            assert!(
                src.contains("DutyKind::new("),
                "`{module}` must declare its category and its ceiling in one const"
            );
        }

        // And one `DutyKind` per duty module, each constructed exactly once. One
        // built anywhere else is a duty nobody declared.
        let built: Vec<String> = code()
            .into_iter()
            .flat_map(|(rel, src)| std::iter::repeat_n(rel, count(&src, "DutyKind::new(")))
            .collect();
        assert_eq!(
            built.len(),
            DUTY_MODULES.len(),
            "one `DutyKind` per duty module and no others, but found: {built:?}"
        );
    }

    // =======================================================================
    // AC-10 — no duty's category comes from text.
    // =======================================================================

    /// **AC-10.** Every place the daemon names one of the five duty categories,
    /// it names it as a literal enum variant — never derived from prompt text, a
    /// tool name, or any string comparison.
    ///
    /// The type system already forbids the *judgment* path from naming these
    /// five (`JudgmentCategory` has no variant for them). Nothing forbids the
    /// duty path from reintroducing it, and the tempting shape is exactly the
    /// one ADR-10 rejected: `if name == "grep" { Category::Triage }` in the turn
    /// loop. This scan is what makes that unwritable.
    ///
    /// The rule is mechanical: on every line that names a duty category, there
    /// may be no string literal and no comparison. A category is *named*, never
    /// *computed*.
    #[test]
    fn no_duty_category_is_ever_produced_from_text() {
        /// Ways a line could be deciding a category by looking at data.
        const FROM_DATA: [&str; 8] = [
            "\"",
            "==",
            "!=",
            ".contains(",
            ".starts_with(",
            ".ends_with(",
            ".eq_ignore_ascii_case(",
            "name()",
        ];
        /// The three shapes ADR-3 allows a duty category to be named in.
        const ALLOWED: [&str; 3] = ["DutyKind::new(", "router.resolve(", "=>"];

        let mut checked = 0usize;
        for (rel, src) in code() {
            if rel == WIRE_ROUND_TRIP {
                continue;
            }
            for line in src.lines() {
                if !names_a_duty_category(line) {
                    continue;
                }
                checked += 1;
                for smell in FROM_DATA {
                    assert!(
                        !line.contains(smell),
                        "AC-10 VIOLATION: {rel} decides a duty category from data \
                         (`{smell}` on the same line). A duty's category is tagged at \
                         its call site and never derived (BR-1):\n  {}",
                        line.trim()
                    );
                }
                assert!(
                    ALLOWED.iter().any(|shape| line.contains(shape)),
                    "{rel} names a duty category in a shape ADR-3 does not allow \
                     (expected one of {ALLOWED:?}):\n  {}",
                    line.trim()
                );
            }
        }
        assert!(
            checked >= DUTY_MODULES.len(),
            "the scan found only {checked} duty-category mentions, which is fewer than \
             the `DutyKind` constants that must exist — it is reading the wrong thing"
        );

        // ADR-10's own claim, and the sharpest one: the tool layer — where a
        // name→category map would be most natural to write — never names a
        // category at all. `ToolRegistry::refine` looks a tool up by the same
        // name `dispatch` already used and asks the *instance*; the category
        // rides in on an already-resolved route.
        for (rel, src) in code() {
            if !rel.starts_with("harness/tools/") {
                continue;
            }
            assert!(
                !src.contains("Category::"),
                "ADR-10 VIOLATION: {rel} names a routing category. The tool layer \
                 dispatches on the tool, never on a name→category map (BR-1)."
            );
        }
    }

    /// Whether `line` names one of the duty categories **on the routing type**.
    ///
    /// The qualifier matters and is read rather than assumed. Three other enums
    /// in this workspace carry the same variant names and none of them routes
    /// anything: `ConfigurableCategory` is the *config* surface (which is why
    /// `migrated.categories.contains(&ConfigurableCategory::Digest)` is a
    /// migration decision and not a routing one), `ProtoCategory` is the wire
    /// twin, and `ProtoConfigurableCategory` is the wire twin of the config
    /// surface. Only `teton_core`'s `Category` — imported here and, in
    /// `router.rs`, aliased `CoreCategory` — can reach a router.
    fn names_a_duty_category(line: &str) -> bool {
        const DUTIES: [&str; 6] = ["Digest", "Triage", "Shell", "Title", "Compact", "Redact"];
        const ROUTING_TYPE: [&str; 2] = ["Category", "CoreCategory"];
        line.match_indices("Category::").any(|(at, _)| {
            let qualifier: String = line[..at + "Category".len()]
                .chars()
                .rev()
                .take_while(|c| c.is_ascii_alphanumeric() || *c == '_')
                .collect::<Vec<_>>()
                .into_iter()
                .rev()
                .collect();
            if !ROUTING_TYPE.contains(&qualifier.as_str()) {
                return false;
            }
            let variant: String = line[at + "Category::".len()..]
                .chars()
                .take_while(char::is_ascii_alphanumeric)
                .collect();
            DUTIES.contains(&variant.as_str())
        })
    }

    /// The stated exception to the scan above, pinned so it stays an exception.
    ///
    /// The ledger reads back a category column **it wrote itself**, which is a
    /// round trip rather than a decision — and the value it produces is the
    /// protocol's `Category`, the wire twin, which has no conversion into
    /// `teton_core::Category` and therefore cannot reach a router. Both halves
    /// are asserted, because the exception is only safe while both hold.
    #[test]
    fn the_only_text_to_category_map_is_the_ledgers_own_round_trip() {
        let ledger = source_of(WIRE_ROUND_TRIP);
        assert!(
            ledger.contains("use teton_protocol::{Category,"),
            "the round trip must produce the wire twin, not the routing type"
        );
        assert!(
            !ledger.contains("teton_core"),
            "the file holding the only text→category map must not be able to name \
             the routing type at all"
        );

        // And nothing on the duty path has grown one.
        for (rel, src) in code() {
            if !(rel.starts_with("harness/") || rel == "runtime.rs" || rel == "router.rs") {
                continue;
            }
            for line in src.lines() {
                if !names_a_duty_category(line) {
                    continue;
                }
                assert!(
                    !line.contains('"'),
                    "a text→category map appeared on the duty path, in {rel}:\n  {}",
                    line.trim()
                );
            }
        }
    }

    // =======================================================================
    // AC-11 — every duty declares a ceiling, and the five are ordered.
    // =======================================================================

    /// **AC-11, the cross-cutting half.** Each duty enforces its own bound
    /// against its own declared constant — asserted per duty, beside each
    /// constant, in `harness::{digest,triage,shell_duty,title,compact}::tests`.
    /// What none of those can say is that **all five** declare one, and that the
    /// five stand in a stated order rather than being five numbers that happen
    /// to have been written down.
    ///
    /// The order is the claim BR-8 makes in prose: "a `title` is a handful of
    /// words, a `compact` is a conversation". Asserted at compile time, because
    /// the relationship between two constants is a compile-time fact — widening
    /// one past its neighbour stops the build rather than waiting for a run.
    #[test]
    fn every_duty_declares_a_ceiling_and_the_five_are_ordered() {
        use crate::harness::{COMPACT_DUTY, DIGEST_DUTY, SHELL_DUTY, TITLE_DUTY, TRIAGE_DUTY};

        let declared: BTreeSet<&str> = [
            DIGEST_DUTY,
            TRIAGE_DUTY,
            SHELL_DUTY,
            TITLE_DUTY,
            COMPACT_DUTY,
        ]
        .into_iter()
        .map(|kind| kind.category().as_str())
        .collect();
        assert_eq!(
            declared.into_iter().collect::<Vec<_>>(),
            vec!["compact", "digest", "shell", "title", "triage"],
            "the five duties on the seam, each with its own category"
        );

        for kind in [
            DIGEST_DUTY,
            TRIAGE_DUTY,
            SHELL_DUTY,
            TITLE_DUTY,
            COMPACT_DUTY,
        ] {
            assert!(
                kind.ceiling_bytes() > 0,
                "`{}` declares no bound, so AC-11 has nothing to assert against \
                 (BR-8: a bound that lives only in a reviewer's head is not a bound)",
                kind.category().as_str()
            );
        }

        const {
            assert!(TITLE_DUTY.ceiling_bytes() < TRIAGE_DUTY.ceiling_bytes());
            assert!(TITLE_DUTY.ceiling_bytes() < SHELL_DUTY.ceiling_bytes());
            assert!(TRIAGE_DUTY.ceiling_bytes() < DIGEST_DUTY.ceiling_bytes());
            assert!(SHELL_DUTY.ceiling_bytes() < DIGEST_DUTY.ceiling_bytes());
            assert!(DIGEST_DUTY.ceiling_bytes() < COMPACT_DUTY.ceiling_bytes());
        }
    }

    // =======================================================================
    // REQ-562 TASK-070 — every remote path crosses the redaction gate.
    // =======================================================================

    /// A gate that records every payload it is shown and answers `verdict`.
    struct RecordingGate {
        verdict: crate::egress::RedactionVerdict,
        seen: Mutex<Vec<String>>,
    }

    impl RecordingGate {
        fn new(verdict: crate::egress::RedactionVerdict) -> Arc<Self> {
            Arc::new(Self {
                verdict,
                seen: Mutex::new(Vec::new()),
            })
        }

        fn seen(&self) -> Vec<String> {
            self.seen.lock().expect("gate poisoned").clone()
        }
    }

    #[async_trait]
    impl crate::egress::RedactionGate for RecordingGate {
        async fn scan(&self, payload: &str) -> crate::egress::RedactionVerdict {
            self.seen
                .lock()
                .expect("gate poisoned")
                .push(payload.to_owned());
            self.verdict.clone()
        }
    }

    /// **REQ-562 ADR-1.** A remotely bound duty reaches the network through the
    /// same [`Egress::send`] a turn does, so with the gate installed its prompt
    /// is scanned too — and a blocking verdict stops it before a byte leaves.
    ///
    /// `boundaries` is deliberately **empty**: the provenance inspection must
    /// not be what refuses this, or the test would pass against a choke point
    /// with no gate at all. The two legs share everything but the verdict, so
    /// what discriminates them is the scan and nothing else.
    #[tokio::test]
    async fn a_remote_duty_send_is_scanned_by_the_choke_points_gate() {
        use crate::egress::redact::{Finding, FindingKind, RedactionVerdict};
        use crate::harness::duty::testing;
        use crate::harness::DIGEST_DUTY;

        const PROMPT: &str = "summarize this tool result for the turn";

        // Blocked: the duty fails and the wire is empty.
        let blocking = RecordingGate::new(RedactionVerdict::from_findings(vec![Finding::pattern(
            FindingKind::Credential,
            0..4,
        )]));
        let (route, sent) = testing::remote_duty_route_gated(
            DIGEST_DUTY,
            Vec::new(),
            "a summary",
            1,
            Some(blocking.clone()),
        );
        let err = route
            .perform(PROMPT, &Provenance::empty())
            .await
            .expect_err("a blocking verdict must refuse the duty's send");
        assert!(
            testing::wire(&sent).is_empty(),
            "not a byte of a blocked duty prompt may leave: {}",
            testing::wire(&sent)
        );
        assert!(!err.is_empty(), "the call site is handed a sentence");
        let scanned = blocking.seen();
        assert_eq!(scanned.len(), 1, "the duty's send was scanned exactly once");
        assert!(
            scanned[0].contains(PROMPT),
            "and what it scanned is the duty's own outbound body: {}",
            scanned[0]
        );

        // Allowed: the same route, the same prompt, a clean verdict — and now
        // the prompt really is on the wire, which is what makes the leg above
        // an assertion about the gate rather than about a route that cannot
        // send at all.
        let clean = RecordingGate::new(RedactionVerdict::clean());
        let (route, sent) = testing::remote_duty_route_gated(
            DIGEST_DUTY,
            Vec::new(),
            "a summary",
            1,
            Some(clean.clone()),
        );
        let answer = route
            .perform(PROMPT, &Provenance::empty())
            .await
            .expect("a clean verdict forwards");
        assert_eq!(answer, "a summary");
        assert!(testing::wire(&sent).contains(PROMPT));
        assert_eq!(clean.seen().len(), 1);
    }
}
