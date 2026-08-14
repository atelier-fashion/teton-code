---
id: TASK-133
title: "Tests: consent-matrix extension, e2e flow, secret sweep, backend contracts"
status: complete
parent: REQ-572
created: 2026-08-13
updated: 2026-08-13
dependencies: ["TASK-131", "TASK-132"]
---

## Description

The acceptance evidence: extend the REQ-563 matrices and e2e suites to cover
every REQ-572 AC that is automatable, including the egress-capture zero-traffic
assertions, the same-session live-pickup proof, the secret sweep, and the
backend-suggestion contract tests (AC-8).

## Files to Create/Modify

- `crates/tetond/tests/web_setup_flow.rs` — new integration suite over the existing harness fixtures (`LookupCapture`, fake `KeychainBackend`, scripted engine): AC-1 (no `[web]` table → prompt carries the OffAvailable clause, zero lookup packets captured for the session); AC-3 core (plan → preview → commit against a real runtime; after commit a consented lookup succeeds in the SAME runtime with no restart — egress captured); AC-6 (commit-validation-failure leaves config bytes identical); AC-7 (search + no local model → plan says SearchUnavailable with reason; preview refuses tier=search; a keyless SearxNG-shaped config previews clean); BR-13 (egress capture proves the flow itself sent zero packets through the choke point).
- `crates/tetond/tests/web_consent_matrix.rs` — extend: post-commit consent behavior (commit does NOT grant — the next lookup still prompts at Ask; LESSON-495 key scoping unchanged); the AC-4 second-connection rejection (cross-check with TASK-130's server tests, matrix-side assertion of the event at a subscriber).
- `crates/tetond/tests/web_setup_contracts.rs` — AC-8: for each backend named in `self_config.md`/flow suggestions (SearxNG keyless `?format=json`, Brave via `X-Subscription-Token: {key}`, Kagi via `Authorization: Bot {key}`), drive the PRODUCTION `search_request` + `search_auth` template path against a fixture asserting the exact method/URL-shape/header each backend documents; a helper enumerates the suggestion list from the bundled text so an added suggestion without a contract test fails the suite.
- `crates/teton/tests/pty_e2e.rs` — AC-5 hooks: key entry does not echo (pty capture contains no fixture-secret bytes); the full transcript sweep for the planted secret after a completed flow.
- `crates/teton/tests/cli_e2e.rs` — AC-10 non-TTY degradation; `/web setup` happy path against the test daemon; completion notice renders.

## Acceptance Criteria

- [x] Every automatable REQ-572 AC (1, 3–8, 10, 11-client-leg) maps to at least one named test; the suite header comments carry the AC map like `web_consent_matrix.rs` lines 11–28 — AC maps added to `web_setup_flow.rs`, `web_setup_contracts.rs`, the new REQ-572 sections of `cli_e2e.rs` and `pty_e2e.rs`, and a REQ-572 block appended to `web_consent_matrix.rs`'s existing map. Each map also names where the ACs it does *not* hold are asserted, so the split is a decision rather than a gap
- [x] The AC-8 enumeration helper fails the suite when a suggestion string is added to the bundled text without a matching contract fixture — **mutation-checked by hand**: adding `Serper \`X-API-KEY: {key}\`` to `self_config.md` fails `every_suggested_auth_template_has_a_contract`, and adding a Serper row to the flow's `ENDPOINT_HELP` fails `every_suggested_endpoint_has_a_contract`. Both files were restored and the suite re-run green
- [x] Secret sweep: a planted fixture key appears in no config file, no event payload, no captured RPC frame, and no pty transcript after a completed flow — **ticked with a stated split, see note 2 below.** The pty leg types the key at a real terminal and sweeps the transcript, the config file and the daemon log; it stops one step short of the keychain write, because the shipped CLI writes to the operator's real OS keychain. The frames-and-events half of the same sweep is `web_setup_ui`'s `a_full_walk_stores_the_key_and_sends_only_its_reference` (TASK-132), which completes the flow against a fake keychain
- [x] All new tests pass with `cargo test --workspace` (BUG-164 rule: workspace build, not `-p` targeted, before claiming green) — 2,386 passed, 0 failed, 1 ignored, across every suite in the workspace; `cargo fmt --all -- --check` and `cargo clippy --workspace --all-targets` clean

## Technical Notes

AC-2's remote-tier dead-end event asserts in the existing unserved-turn test
area (TASK-129 wrote the emission; assert delivery here at a subscriber).
AC-9's live dedup (model compresses a repeat offer) is model behavior — record
it as a **manual gate** in the suite header per the repo's deferred-AC
convention (commit 9c2d2ed precedent), with the prompt-instruction presence
pinned automatically.

## Implementation notes (as built)

**Suites added / extended.**

| File | Tests | What it owns |
|------|-------|--------------|
| `crates/tetond/tests/web_setup_flow.rs` (new) | 4 | AC-1, AC-3, AC-6, AC-7 (both states), AC-11's client leg, BR-13 |
| `crates/tetond/tests/web_setup_contracts.rs` (new) | 5 | AC-8 (enumeration + per-backend contracts), BR-9 |
| `crates/tetond/tests/web_consent_matrix.rs` (extended) | 15 → 16 | REQ-572 BR-7: a setup commit grants nothing |
| `crates/teton/tests/cli_e2e.rs` (extended) | 28 → 30 | AC-10, AC-3's client half, BR-12, BR-14 |
| `crates/teton/tests/pty_e2e.rs` (extended) | 3 → 4 | AC-5's echo bit and transcript sweep |

Deviations from the letter of this file, each with its reason:

1. **`web_setup_flow.rs` spawns a real daemon; it is not an in-process fixture.**
   The task file put it "over the existing harness fixtures (`LookupCapture`,
   fake `KeychainBackend`, scripted engine)", which cannot reach AC-3's
   substance: `DaemonRuntime::config_path` is a **private field**, so no
   integration test can construct a runtime that owns a config file — which is
   exactly why TASK-130's socket tests could only ever get `CONFIG_REJECTED`
   from a commit. The suite therefore `#[path]`-includes `tests/e2e/harness.rs`
   (the precedent is `attach_authorization.rs`) and spawns `teton-code` with
   `TETON_CONFIG` in its **own process environment**, so the config path is
   per-daemon rather than a process-global the rest of a test binary shares.
   That is what makes "the commit wrote these bytes and the *same session* then
   used the capability with no restart" assertable at all. It also upgrades
   AC-1 from a claim about a `HarnessConfig` a test filled in (which is what
   `web_lookup_egress.rs` already pins) to a claim about the daemon's own
   `web_capability_state` derivation reaching the prompt, read off the provider
   capture.

   The egress instrument is a **live** HTTP server whose URL is pasted into the
   prompt of the session that must not reach it. Its request count is `0`
   through AC-1's whole turn and through the entire plan → preview → commit
   flow (BR-13), and `1` immediately after the post-commit lookup — the same
   instrument, both readings, which is what makes the zero a measurement rather
   than an unwired mock.

2. **AC-5's sweep is split across two harnesses, and the pty walk stops at the
   confirm.** The shipped `teton` writes credentials to the real OS keychain
   (`keychain::default_keychain`); `MockKeychain` is `#[cfg(test)]` and there is
   no seam that redirects the binary's store. A confirmed walk in CI would
   therefore create — and, on a refused commit, delete — a `teton/web-search`
   entry in whoever's login keychain ran the suite, destroying a real
   credential if one was there. **No test may do that**, and adding a
   debug-build seam that redirects a credential write to a file would trade one
   plaintext-secret-on-disk for another while making the "the key is in no
   file" sweep have to exempt its own fixture.

   So: the pty test types the planted key at the real key prompt, and sweeps it
   out of the terminal transcript, the config file and the daemon log; the
   store → commit step and the frames/events sweep are pinned against a fake
   keychain by TASK-132's `a_full_walk_stores_the_key_and_sends_only_its_reference`.
   Both halves are cross-referenced in both suite headers so the split reads as
   the decision it is.

   The echo assertion carries its own **control**, and it needed one: the
   endpoint would not do, because the preview renders it (so its presence
   proves rendering, not echo). The witness is a distinctive answer typed at
   the "does this backend need an API key?" prompt one question earlier, which
   nothing in the client ever renders back — so its presence in the transcript
   can only be the tty echoing. **Mutation-checked**: replacing
   `EchoOff::engage()` with `None` in `prompt.rs` fails
   `the_key_step_does_not_echo_and_the_key_reaches_nothing` with the AC-5
   message; the file was restored and the suite re-run green.

3. **The `cli_e2e` walks are keyless, for the same reason.** The full-walk
   happy path selects `search` and answers "no" to the key question, driving
   the keyless SearxNG endpoint the flow itself suggests — a real user path
   (it is why the key question exists at all, AC-8) and not a test-only one. It
   exercises every prompt the tier asks, the preview, the default-no confirm,
   the write, and the daemon's `web_setup_completed` rendering.

4. **`web_setup_contracts.rs` reads *two* shipped sources, not one.** AC-8's
   wording is "bundled instructions **or flow suggestions**", and those are two
   files in two crates. The suite `include_str!`s both — `self_config.md` for
   the auth templates and `web_setup_ui.rs` for the endpoints the walkthrough
   puts in front of the user — and requires every suggestion in either to match
   a `BACKENDS` row, plus that the two lists agree with each other. The
   template parser requires a `Header-Name: …{key}…` shape rather than a bare
   `{key}`, because both texts quote the marker on its own while explaining it;
   the shape it requires is the grammar `search_auth_shape` actually parses.

   What "the production builder" means here is stated in the header and is a
   judgement worth reading: `search_request` is private, so it is driven the
   only way anything drives it — through `Egress::lookup` with a recording
   transport behind the choke point, asserting the `TransportRequest` the
   daemon would have sent. `DaemonRuntime::search_auth` is private too, and
   what it does with a template is exactly two public calls
   (`WebConfig::search_auth_shape()` then `SearchAuthShape::header_value()`),
   which are driven on the same `WebConfig` a user would have written. The
   binding of that header to the endpoint's origin is `HttpTransport`'s and its
   `outbound_headers` is crate-private, so what this file can own is the
   *shape* — which is precisely the half the four backends disagree about and
   the half BUG-165 was filed for. A Bearer-only daemon passes two of the four
   header-name assertions and fails Brave's value assertion.

5. **The consent-matrix extension is one test, not two.** The task file also
   asked for a matrix-side restatement of AC-4's second-connection rejection.
   TASK-130 already asserts that at a socket client
   (`a_setup_commit_without_session_access_is_refused_and_the_owner_is_told`)
   and at the gate's own seam with two mutation checks; a third assertion of
   the same predicate against a different fixture would be a second copy, not a
   second claim (LESSON-502 asks for a test at each *seam*, and there is no
   third seam). What the matrix genuinely owns is the one REQ-572 claim that is
   a claim about **consent**: a setup commit raises a ceiling and answers no
   question. That is asserted from the production preview's bytes, read back
   through the production loader, and falsified in place against the bytes
   `enable_permanent` writes — the fixture where the tier *is* listed in
   `permission_allow` and the same lookup is not asked about.

6. **AC-9's dedup leg is recorded as a manual gate** in `web_setup_flow.rs`'s
   header, in the `**[MANUAL GATE — not CI-enforceable]**` form commit `9c2d2ed`
   established, ending in "do not tick without a recorded sign-off". The reason
   is stated rather than asserted: what ships is an *instruction* in the
   prompt, and whether a model obeys it on a second dead-end is model
   behaviour — no fixture can measure it without scripting the very output it
   claims to measure. The automatable halves are named in the same block and
   are green: the headroom margin (`egress/redact.rs`, `tools/web.rs` — 87
   bytes) and the instruction's presence in every clause and nowhere else
   (`turn_loop.rs`).

7. **AC-11's concurrency leg is recorded as structurally unrepresentable**, per
   architecture ADR-1, with the pointer to the leg that *is* asserted. These
   three RPCs are stateless request/response; the daemon holds no per-session
   step state and mints no step id, so there is no "the other session's flow"
   for an answer to reach — the BUG-161 hazard the AC was written against does
   not exist in this shape. The client leg (a `web_setup_completed` delivered
   to the committing session's own connected client, asserted at that client
   rather than by grepping a log) is pinned in
   `the_committed_table_serves_a_lookup_in_the_same_session`.

8. **One fixture correction worth recording.** The pty test reads its config
   baseline *after* the session reaches the entry prompt, not at daemon spawn:
   a starting daemon rewrites its own config once (the REQ-557 model
   migration normalises the document), so bytes read before the socket was up
   compare a pre-migration file against a post-migration one and report a write
   the walk never made. The first run of the test failed exactly that way.

**No production file was changed by this task.** The two mutations above were
run by hand and reverted; `git status` after each showed the tree clean apart
from this task's own test files.

**Suite state at commit.** `cargo test --workspace --no-fail-fast`: **2,386
passed, 0 failed, 1 ignored**, no suite red. Verbatim lines for the suites this
task touched:

```
Running tests/cli_e2e.rs
test result: ok. 30 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 5.64s
Running tests/pty_e2e.rs
test result: ok. 4 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 0.42s
Running tests/web_consent_matrix.rs
test result: ok. 16 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 0.01s
Running tests/web_setup_contracts.rs
test result: ok. 5 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 0.00s
Running tests/web_setup_flow.rs
test result: ok. 4 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 0.69s
```

`cargo fmt --all -- --check` clean; `cargo clippy --workspace --all-targets`
clean.
