---
id: TASK-132
title: "CLI: /web setup collection flow, keychain store+delete, status capability field"
status: draft
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

- [ ] `/web setup` on a TTY walks all steps and a scripted daemon answers; on non-TTY it prints instructions, consumes no stdin line meant for the session, and exits the command cleanly
- [ ] The key prompt does not echo (pty assertion in TASK-133 — this task provides the hook), the secret appears in no RPC params (fake-client capture: only `search_key_ref` crosses), and abort at every prompt leaves the fake keychain empty
- [ ] Commit failure deletes the just-stored fake-keychain entry and renders the daemon's validator sentence
- [ ] Status line shows `web: off (available)` from a snapshot with the new field, and existing `WebState` precedence (overridden > restricted > granted) is unchanged

## Technical Notes

All rendering through the `Surface`/`Prompter` seams (pure content functions +
gated bytes — REQ-556/REQ-560 BR-8 pattern), so content is unit-testable
without a terminal. The store→commit ordering and the delete-on-failure are
ADR-3's residual-minimizing sequence — keep store immediately before commit,
after the human confirm.
