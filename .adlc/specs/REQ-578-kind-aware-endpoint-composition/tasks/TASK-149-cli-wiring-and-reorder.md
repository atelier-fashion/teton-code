---
id: TASK-149
title: "CLI wiring: compose, default, echo, key-prompt reorder"
status: complete
parent: REQ-578
created: 2026-08-15
updated: 2026-08-15
dependencies: ["TASK-148"]
repo: teton-code
---

## Description

Wire composition into `run_provider_add` per ADR-3: compose + Anthropic
default after the `--model` pre-check, echo when changed, duplicate-id check,
then — and only then — the credential prompt. BUG-171's take-back machinery
stays byte-untouched.

## Files to Create/Modify

- `crates/teton/src/main.rs` — `run_provider_add` (l.1295-1397): insert the
  composition step + echo per ADR-3's order; pass the composed endpoint to
  `build_provider_registration` (which stays a verbatim assembler); update
  the `stored_registration` test helper (~l.3004) to read
  `ANTHROPIC_DEFAULT_ENDPOINT`; new unit tests (echo present when changed,
  silent when verbatim, default applied, prompt-order via ScriptedPrompter +
  RecordingSurface asserting the echo precedes the secret prompt).

## Acceptance Criteria

- [ ] BR-5 holds: with a base-URL or defaulted endpoint, the echo line is
  emitted before `read_secret` is reached; a structurally incomplete
  registration (openai-compatible, no endpoint) still refuses before any
  prompt, message naming the flag.
- [ ] BUG-171 tests pass unmodified:
  `a_rejected_registration_takes_back_the_key_it_stored`,
  `a_rejected_registration_restores_the_credential_it_displaced`, and
  `a_provider_key_is_asked_for_through_the_hiding_path`.
- [ ] Existing registration tests (duplicate-id, model pre-check, parsing)
  green; `provider_registration_stores_key_in_keychain_and_keeps_only_a_ref`
  updated only if its fixture endpoint shape requires it.
- [ ] `cargo test -p teton` green; clippy + fmt clean.

## Technical Notes

- Echo rides `surface.line(LineKind::Info, …)` — one line, e.g.
  `endpoint stored as https://api.moonshot.ai/v1/chat/completions`.
- The reorder is minimal: composition slots between the existing model
  pre-check (l.1306) and the duplicate-id probe (l.1326); `read_secret`
  (l.1347) does not move relative to what follows it.

## Implementation Note (2026-08-15) — ADR-3 order amended by one line

ADR-3 sketched `compose → echo → duplicate-id probe → read_secret`. As
shipped, the whole settle step (compose, structural refusal, echo) sits
**after** the duplicate-id probe and before `read_secret`. Two reasons, both
found by running the suite:

1. **BR-7.** `provider_add_refuses_an_id_that_is_already_registered`
   (cli_e2e.rs:1804) registers an existing id *without* `--endpoint` and
   pins the "already registered" refusal. With the endpoint refusal ahead of
   the probe, that shipped message changed — a previously-working command
   behaving differently is exactly what BR-7 forbids. The test passes
   unmodified in the shipped order.
2. An echo emitted before a refusal would say "endpoint stored as …" about a
   registration that is not happening.

ADR-3's *reason* is untouched: everything the user needs in order to decide
whether to type a key is on screen before they are asked for one (BR-4/BR-5),
and `read_secret` still does not move relative to what follows it. Recorded
here for the wrapup's ADR-3 amendment.
