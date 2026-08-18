---
id: LESSON-535
title: "A probe is a billed call and a preview is a surface — four verify-phase catches on REQ-581 and the audit prompts they leave behind"
component: "cli/providers"
domain: "providers"
stack: ["rust", "cli", "daemon", "json-rpc", "tokio", "llm-providers"]
concerns: ["security", "developer-experience", "routing", "lifetime"]
tags: ["consent-preview", "credential-redaction", "displayed-endpoint", "typed-outcomes", "teardown", "activity-guard", "drain", "zero-token", "redirect", "verify-phase"]
req: REQ-581
created: 2026-08-17
updated: 2026-08-17
---

## What Happened

REQ-581 (a first-class provider connection test) implemented cleanly across
eight tasks — 2812 tests green, every task self-reported mutation checks —
and the Phase-5 verify loop still found four things the implementers had not:

1. **Critical — the consent preview printed a stored endpoint verbatim.** An
   endpoint can carry `user:password@` (the product permits it), and every
   other CLI line renders endpoints through `displayed_endpoint`, which masks
   the userinfo. The new preview did not, so the one line whose job is to say
   "this is what will be sent — proceed?" would have put the credential in
   the transcript. The CLI's own "never the key" test could not see it: the
   planted key was never fed into anything the preview rendered.
2. **Major — a drained probe with no lifetime claim.** The probe's task was
   moved off the abort-at-teardown list onto the drained one so a Ctrl-C
   mid-request keeps the ledger row for money already spent — REQ-565's rule.
   But it took no `BlockingActivity` claim, so under `on-last-disconnect` the
   supervisor committed the moment the last client left and `_exit` beat the
   drain. The drain was theatre. The in-process test could not see it either:
   it drove `handle_client` directly, with no supervisor to commit and no
   `main` to exit.
3. **Major — a redirect read as `reached`.** Both adapters only error on
   status ≥ 400 and synthesize a terminal `Completed` even with no `data:`
   lines, so a 301 (redirects are not followed), a 204, or a non-SSE 200
   drained to `Completed { 0/0 }` → `reached` → health `healthy`. The exact
   misconfiguration the command exists to catch would have been called fine.
4. **Minor — three facts in one variant.** "Nothing answered", "answered but
   not with a completion", and "no answer in time" all landed in
   `unreachable`, told apart by prose — inside a REQ whose thesis is typed
   outcomes.

All four were fixed with a test that fails under the mutation it guards; the
Critical's fixture now plants `sk-planted-provider-test-key` in the endpoint's
userinfo, and the lifetime test asserts `client_disconnected <
shutdown_deferred < provider_tested` on the bus.

## Lesson

- **A new surface that prints an endpoint imports the masking renderer or it
  prints the userinfo.** Grep for the existing renderer before writing a
  format string; and plant the secret where a credential can actually live on
  *that* surface, or the "never printed" assertion is decorative.
- **Moving a task to a drained list is half a fix.** Draining protects work
  only if something keeps the process alive to finish it — grep for the
  `activity(` claim beside the spawn; if a turn takes one, so does anything
  billed. Test it with the supervisor in the loop, not around it.
- **Zero tokens and no text is not a success** — it is a proxy, a redirect,
  or the wrong endpoint. Give it its own outcome; touch no health.
- **When a design says "typed", prose inside one variant is the smell.**
  Split at the enum, exhaustively, before a renderer learns to parse the
  sentence.

## Why It Matters

Each of the four had a green test suite behind it. The Critical is a
credential in a transcript; the drain gap silently loses billed spend in the
single most common shape (one client, Ctrl-C); the redirect declares broken
providers healthy and clears their downgrade; the prose variant taxes every
future consumer. Verify caught them because it read the code against the
neighbouring precedents (REQ-565's teardown comments, `displayed_endpoint`'s
own doc, the adapters' `finalize`), not the commit messages.

## Applies When

- Adding any surface that renders an endpoint, URL or authority string.
- Spawning a task that spends money or writes a durable record, and deciding
  what disconnect owes it.
- Classifying an HTTP answer as success by "the stream ended".
- Any REQ whose contract is "typed outcomes" — audit the enum before the
  renderer.
