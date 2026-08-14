---
id: TASK-132
title: "CLI: /web setup collection flow, keychain store+delete, status capability field"
status: complete
parent: REQ-572
created: 2026-08-13
updated: 2026-08-13
dependencies: ["TASK-127", "TASK-130"]
---

## Description

The client edge (architecture ADR-1/ADR-3): the `/web setup` slash command,
TTY-gated collection (tier menu, endpoint, echo-off key), preview render +
default-no confirm, keychain store-then-commit with delete-on-failure,
non-TTY degradation to printed instructions, and the status surface's
capability field.

## Files to Create/Modify

- `crates/teton/src/slash.rs` — `/web setup` in the `COMMANDS` table (family: `/web allow`, `/web refresh`); handler delegates to the new module. Typed-input-only gate in the `model_set_gate` pattern.
- `crates/teton/src/web_setup_ui.rs` — new module: drive plan → collect → preview → confirm → store → commit. Tier menu marks search "(unavailable: <reason>)" when plan says so and refuses its selection; endpoint prompt; key prompt echo-off reusing the `teton provider add` secret-prompt mechanism; preview renders the daemon's `toml` + `search_host` verbatim (never re-derived client-side — LESSON-494); confirm is default-**no** (LESSON-470: the write is the costly wrong answer); on confirm → `Keychain::store("web-search", key)` → `web/setup_commit` with the returned ref → on commit error, `Keychain::delete` the entry just created, render the error. Abort (empty/EOF/ctrl-c) at any prompt exits with config untouched and nothing stored.
- `crates/teton/src/keychain.rs` — add `fn delete(&self, account: &str) -> Result<(), KeychainError>` to the `Keychain` trait + macOS impl + fake impl (fake records deletes for tests).
- `crates/teton/src/session_ui.rs` — render `WebSetupCompleted` ("web lookup enabled (<tier>) — the next web-needing question will ask before anything leaves the machine") and `WebSetupRejected` notices; fold the `ConfigSnapshot.web_capability` field into `WebState`/`status_field` so `web: off (available)` renders when off (OQ-2 note: the completion notice IS the post-setup offer — no auto lookup).
- `crates/teton/src/main.rs` — non-TTY path: `/web setup` under piped stdin prints the enablement instructions (plan result + bundled text) and continues the session cleanly (BR-12/AC-10; LESSON-470's is-terminal rule).

## Acceptance Criteria

- [x] `/web setup` on a TTY walks all steps and a scripted daemon answers; on non-TTY it prints instructions, consumes no stdin line meant for the session, and exits the command cleanly — `a_piped_session_is_told_what_to_type_and_asked_nothing` (asked == 0, no frames, keychain empty) and `the_gate_walks_at_a_terminal_and_degrades_on_a_pipe`; the walk itself is `a_full_walk_stores_the_key_and_sends_only_its_reference`. Both branches were also driven manually against a real `teton-code` (plan → menu → endpoint → keyless → preview → confirm → commit wrote the `[web]` table; a second session's plan then read it back). The **pty** leg is TASK-133's
- [x] The key prompt does not echo (pty assertion in TASK-133 — this task provides the hook), the secret appears in no RPC params (fake-client capture: only `search_key_ref` crosses), and abort at every prompt leaves the fake keychain empty — the hook is `Prompter::ask_secret` (no default implementation; `StdinPrompter` clears `ECHO` through a restoring guard, `ScriptedPrompter` records that the hiding path was taken); the capture is `a_full_walk_stores_the_key_and_sends_only_its_reference` (planted key absent from the serialized preview **and** commit frames and from every rendered line); the aborts are `an_abort_at_every_prompt_stores_nothing_and_commits_nothing`, over EOF **and** empty answers at every position. **Not ticked for the echo itself** — that needs a terminal and is TASK-133's assertion
- [x] Commit failure undoes the keychain effect this run caused and renders the daemon's validator sentence — **amended by the verify fix pass** (the original unconditional delete destroyed a rotated-over credential): the undo is now conditional via `PriorKey` — delete when this run *created* the entry (`a_refused_commit_on_a_fresh_account_deletes_and_says_removed`), restore the displaced bytes when it *rotated* one (`a_refused_commit_after_a_rotation_puts_the_previous_key_back`), leave alone when the prior state was unreadable (`a_refused_commit_after_an_unreadable_keychain_leaves_the_entry_alone`) or when the commit's outcome is transport-ambiguous (`a_commit_that_never_answered_leaves_the_keychain_alone_and_says_so`); a cleanup that itself fails reports both failures with the recovery command (`a_refused_commit_whose_own_cleanup_also_fails_reports_both_failures`); `a_refused_preview_asks_for_no_confirmation_and_stores_nothing` still covers the earlier failure point
- [x] Status line shows `web: off (available)` from a snapshot with the new field, and existing `WebState` precedence (overridden > restricted > granted) is unchanged — `the_capability_field_tells_off_from_off_but_available` and `the_configured_capability_never_outranks_what_the_session_did`. Read honestly with note 6 below: the *field* renders it; whether the *row* is drawn is REQ-563's rule, deliberately untouched

## Technical Notes

All rendering through the `Surface`/`Prompter` seams (pure content functions +
gated bytes — REQ-556/REQ-560 BR-8 pattern), so content is unit-testable
without a terminal. The store→commit ordering and the delete-on-failure are
ADR-3's residual-minimizing sequence — keep store immediately before commit,
after the human confirm.

## Implementation notes (as built)

**Surfaces added.** `crates/teton/src/web_setup_ui.rs` (new): `SetupIo` (the
flow's world seam), `DaemonIo` (its production impl over the session's
connection + context), `run` / `drive` / `collect`, `Gate` + `gate`, `Answers`,
and the pure content functions (`plan_lines`, `capability_line`,
`current_table_line`, `tier_menu_lines`, `preview_lines`, `instruction_lines`,
`parse_tier`, `search_refused_line`, `cleanup_line`).
`crates/teton/src/prompt.rs`: `Prompter::ask_secret` + the `EchoOff` guard.
`crates/teton/src/keychain.rs`: `Keychain::delete` (macOS, unsupported and mock
impls; the mock records deletes). `crates/teton/src/session_ui.rs`:
`WebState::capability`, `WebState::configured_field`,
`format_web_setup_completed` / `_rejected` / `format_capability_dead_end`.
`crates/teton/src/slash.rs`: the `web setup` row + `handle_web_setup`;
`echoed` and `test_seams_allowed` widened to `pub(crate)`.
`crates/teton/src/main.rs`: `read_effort_view` → `read_config_view` (one
`config/get`, now filling the capability too).

Deviations from the letter of this file, each with its reason:

1. **The non-TTY branch lives in `web_setup_ui`, not `main.rs`.** The task file
   put it in `main.rs`; handlers reach the world only through `UiContext` and
   the seams (REQ-555 BR-9), and `typed_input` is already on the context — so
   the branch is `gate(ctx.typed_input, test_seams_allowed())` inside the flow,
   where it is unit-testable. `main.rs`'s only change is the capability read.
   The gate reuses `slash::test_seams_allowed` with the **same polarity** the
   seam's invariant requires (it can only make the walk *reachable*), which is
   what lets TASK-133 drive the happy path from `cli_e2e` over pipes.
2. **`Prompter::ask_secret` has no default implementation.** A default would
   have to be `ask`, and an implementor that forgot to override it would echo a
   credential while looking correct. Three implementors, all in `prompt.rs`; a
   fourth must answer the question explicitly. `FramedStdinPrompter` delegates
   to the plain one — a credential is dialogue, and the frame's geometry counts
   rows the terminal echoes, which an echo-off read does not produce.
   **Accepted residual, recorded**: Ctrl-C *during* the key read kills the
   process before the guard's `Drop`, leaving the terminal echo-off until
   `stty sane`. Every `read -s`-shaped prompt without a signal handler has this
   window; the ordinary aborts (EOF, empty) restore normally.
3. **One extra question, and one prompt where empty is an answer.** "Does this
   backend need an API key? [Y/n]" is asked before the key prompt: it is what
   makes a keyless SearxNG reachable (AC-8) while keeping "empty means abort"
   true of every prompt that needs a value. The `search_auth` prompt is the one
   place where empty means "take what the prompt offered" — stated in the
   question itself. **Amended by the verify fix pass**: the offer is the
   daemon's Bearer default only for an unrecognised host; a known backend
   (Brave, Kagi — `KNOWN_BACKEND_AUTH` beside `ENDPOINT_HELP`) is offered its
   own documented template, closing the guided path that used to recreate
   BUG-165's 401. An empty answer to a known-host offer sends that template on
   the wire; an empty answer to the generic offer stays `None`.
   Verify-pass surface additions: `PriorKey`, `Cleanup`, `unchanged_line`,
   `ambiguous_commit_line`, `offered_auth`, `endpoint_host`, `auth_question`,
   `Keychain::read`, `MockKeychain::{fail_delete_with, fail_read_with}`,
   `EchoState`/`classify_echo` (fail-closed secret prompt), and
   `prompt_for_secret` (provider-add parity).
4. **The preview carries the key *reference* before the key is stored.** The
   reference is a name (`keychain://teton/web-search`), not a value, so it is
   knowable up front — and it has to be, or the confirmed bytes would differ
   from the written bytes and the preview would carry a spurious "no
   `search_key_ref`" warning. The **secret** still moves only after the confirm,
   immediately before the commit (ADR-3).
5. **The menu offers the three enabling tiers; `off` is answered, not obeyed.**
   `tier = "off"` is a valid candidate the daemon would happily write, but
   `WebSetupCompleted` documents a tier that is never `Off` and this command's
   completion notice announces an enablement. An answer of `off` points at the
   config key instead and changes nothing.
6. **`WebState::is_engaged` is unchanged — the capability enters what the row
   *says*, not whether it is *drawn*.** This is the one judgement call in the
   task and it is a one-arm change either way. Engaging on `Ready` makes
   REQ-563's pty acceptance test (`the_status_row_shows_the_session_s_web_
   capability`) vacuous: its fixture is `tier = "fetch_any_url"`, it proves
   non-vacuity by first observing **no** row, and `web: fetch` is a substring of
   `web: fetch (configured)`. Engaging on `OffAvailable` alone is worse still —
   the row would *vanish* the moment a setup completed. Engaging on everything
   is a product decision: a permanent capability row above every prompt on every
   machine, and a change to what the row means (from "what this session did" to
   "what this machine can do"). This REQ's discoverability is carried by the
   per-state prompt clause and the refusal text (architecture, Half 1), so the
   layout of a session that has not touched the web is byte-identical. Both
   `is_engaged` and `configured_field` carry that reasoning in their docs, and
   `the_capability_alone_never_makes_the_row_appear` pins it so a later flip is
   a decision somebody takes rather than one that arrives with an edit.
7. **`CapabilityDeadEnd` renders a verbose-only line** (the task left the call
   to this task). The turn that dead-ended already carries its own remedy — an
   ungated line would be that fact twice — so what this adds for someone already
   watching is the capability id, rendered from the string without branching on
   it, as the wire type's doc requires. TASK-133's assertion is about the event
   reaching a subscriber, which is unaffected.
8. **A successful commit renders nothing from the handler.** The daemon's
   `web_setup_completed` event is the completion notice, and `Connection::call`
   has already pumped it by the time the response is read (the daemon fences a
   request's events ahead of its response) — the same arrangement `/clear` uses,
   for the same reason: one change, one line, drawn by the code every attached
   client shares. `applied: false` *is* rendered, because no event announces a
   change that did not happen.
9. **No e2e was added here.** `crates/teton/tests/{cli_e2e,pty_e2e}.rs` are
   TASK-133's files. The real path was nonetheless exercised by hand against a
   spawned `teton-code` (both gate branches, a keyless search commit, the
   written config read back by a second session, and a declined confirm leaving
   the config alone), because the unit suite drives `SetupIo` and cannot prove
   `DaemonIo` is wired to the right methods.

**Suite state at commit.** `cargo test -p teton` green (302 unit + 28 `cli_e2e`
+ 3 `pty_e2e`), `cargo test -p tetond --lib` green (1236),
`cargo check --workspace` clean, `cargo clippy -p teton --all-targets` clean,
`cargo fmt --all -- --check` clean.
