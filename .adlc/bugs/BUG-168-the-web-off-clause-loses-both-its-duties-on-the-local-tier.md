---
id: BUG-168
title: "The web-off clause loses both its duties on the local tier — the opt-in is never named, and the hunt it forbids is the hunt it causes"
status: resolved
severity: medium
created: 2026-08-13
updated: 2026-08-13
component: "tetond/harness"
domain: "harness"
stack: ["rust", "prompt"]
concerns: ["developer-experience", "cost"]
tags: ["system-prompt", "web-lookup", "opt-in", "local-tier", "prompt-regression", "req-563", "bug-154", "bug-160", "lesson-482"]
---

## Description

REQ-563 BR-6/AC-1 require that with web lookup off, a request that needs the
live web "gets an answer naming the opt-in (no repository hunt)". The clause
that carries this — `WEB_OPT_IN_CLAUSE` in `build_system_prompt` — is present
in every web-off prompt and pinned by tests, but on the shipped local model it
fails **both** of its duties, deterministically:

1. **The opt-in is never named.** Asked to search a service's API docs on the
   web, the model replies "web lookup is disabled on this machine" — an echo of
   the clause's opening — and stops there. `[web] tier` appears in no reply
   (6/6 trials, two question shapes). The user is told the capability does not
   exist, not that it is one config edit away.
2. **The clause causes the hunt it forbids.** Asked "what is the latest
   version of tokio", the model reasons *"Since web lookup is disabled, I'll
   need to look for version information in the repository files"* and spends
   `grep`, `glob`, `glob`, then a `shell: find` that trips a permission prompt
   (3/3 trials) — the exact BUG-154/BUG-160 repository hunt BR-6 exists to
   prevent, launched *because of* the clause.

AC-1's CI test is structural (the prompt contains the clause; zero lookup
egress) and passes throughout; the behavioral half was only checkable live,
and live it fails.

## Reproduction Steps

1. Build with the engine (`cargo build --release --workspace --features
   tetond/llama`) and run an isolated daemon with no `[web]` section in config
   (fresh base dir via `XDG_RUNTIME_DIR`, weights symlinked).
2. Pipe to `teton`: `I'm integrating with the Stripe API. Can you search
   their docs on the web and find the endpoint for creating a refund?`
3. Pipe in a fresh session: `What is the latest version of tokio?`

## Expected Behavior

Both replies name the opt-in — `[web] tier` in Teton's config — per BR-6:
answer what knowledge can answer, say the rest needs the live web, and give
the user the one sentence that turns the refusal into an action. No
repository tool calls for either.

## Actual Behavior

- Shape 1: "I cannot search the web for Stripe's documentation as web lookup
  is disabled on this machine. […] I'd recommend checking Stripe's official
  documentation" — no opt-in named, 3/3.
- Shape 2: "Since web lookup is disabled, I'll need to look for version
  information in the repository files" → grep/glob/glob/find hunt, then gives
  up without naming the opt-in, 3/3.

First observed against the installed 0.1.13 daemon (which already carried the
clause, PR #74); reproduced identically against 0.1.14 + PR #123 HEAD.

## Environment

- Platform: macOS 26 / Apple Silicon, 48 GiB
- Version: teton-code 0.1.14 (also 0.1.13), local tier qwen3-coder-30b-a3b,
  temp-0.2 profile — replies were byte-identical across trials

## Root Cause

Prompt wording, not a code path: `build_system_prompt` has a single call site
and the clause provably reaches the model (its opening is echoed back). Two
LESSON-482 patterns in the old sentence —

- *"say so and name the opt-in — `[web] tier` in Teton's config —"* puts the
  payload in an em-dash aside behind a meta-instruction. The local model
  executes the "say so" half and drops the aside: it reproduces text it is
  given far more reliably than it executes instructions *about* text.
- *"If a question needs the live web … instead of searching the repository"*
  sits beside the frame's "Use tools to find out what only the files can tell
  you" with the connecting premise unstated. The model resolves the pair as
  "web off → the files are the remaining source" — the clause's mention of
  web-off becomes the *reason* to hunt. LESSON-482's corollary: at the local
  tier's temperature, a contradiction between a soft clause and a strong one
  is resolved by ignoring the soft one.

A secondary LESSON-493 gap compounded it: the clause invites "how do I enable
web lookup?", and the bundled self-config guide (BUG-160) said nothing about
`[web]` — the follow-up's knowledge source did not exist.

## Resolution

REQ-572 (#124) landed while this fix was in flight and rebuilt the same
surface: the static clause became the capability-keyed
`WEB_OFF_AVAILABLE_CLAUSE` (naming both enablement paths, `/web setup` and
`[web] tier`), and `self_config.md` gained a fuller `[web]` recipe than this
fix's draft — so the guide half of the original resolution was superseded and
dropped. REQ-572's new clause, however, kept the two shapes this bug proved
fatal (the enablement paths in an em-dash aside behind "say so and name how
to turn it on"; no stated premise about outside-world facts), so the fix
lands as a rewording of that clause, preserving all five facts its test pins:

- **The premise is stated outright**: "facts about the world outside this
  repository … are never in the project files, so do not search the
  repository for them or for the web setting."
- **The ending is dictated, not described**: "end with exactly this
  sentence: 'Web lookup is available but switched off; turn it on with
  `/web setup`, or set `[web] tier` in Teton's config.'"
- **The first part targets the underlying question** ("name the endpoint,
  command, version, or fact they are after"): that phrasing turned the
  question-shaped first part from a generic refusal into an actual answer.
  The clause's doc comment warns that rewordings are unverified until A/B'd
  against the real local tier — near-identical variants behaved differently
  live.
- Regression pins extended in `the_off_clause_names_the_capability_its_off_state_and_both_enablement_paths`
  (the premise; the dictation mechanism) and the AC-1 egress test's prompt
  pin updated to the new anti-hunt spelling. Both prompt-size margins still
  clear.
- Verified live against an isolated post-REQ-572 daemon (release build with
  `tetond/llama`, qwen3-coder-30b-a3b), A/B against the pre-fix baseline
  and across seven wording/context variants: in **every** post-fix
  configuration and every trial, the reply reproduces the opt-in sentence
  verbatim, makes zero tool calls, and never hunts the repository. The
  question shape answers its stale best ("the latest version of tokio is
  1.32.0") ahead of the sentence. Controls: a Cargo.toml question still
  calls `read` and answers from the file with no clause bleed; BUG-160's
  provider answer is unchanged.

**Accepted residuals**, both documented in the clause's comment:

- The staleness qualifier ("marked as possibly out of date") is dropped by
  the model — it sits in the aside position this model reliably drops. The
  closing sentence itself signals why the answer may be stale.
- Whether an **action-shaped** request ("can you search the web for X")
  also volunteers the from-knowledge half proved chaotic: byte-level prompt
  changes far from the clause flipped it in both directions across builds,
  so it is not a property the fix promises or pins. The sentence-only reply
  it settles on is a direct, honest answer to that request, and every
  tested state is strictly better than the pre-fix repo hunt.

## Deployment

n/a — OSS repo, no staging/production pipeline; the fix ships in the next
tagged release (post-0.1.14).

## Files Changed

- `crates/tetond/src/harness/turn_loop.rs` — `WEB_OFF_AVAILABLE_CLAUSE`
  rewording with the BUG-168 rationale, extended pins
- `crates/tetond/src/harness/self_config.md` — two phrases shortened (same
  content) to pay for the longer clause under `MIN_PROMPT_HEADROOM_BYTES`
- `crates/tetond/tests/web_lookup_egress.rs` — AC-1 prompt pin updated to the
  new anti-hunt spelling
- `.adlc/bugs/BUG-168-the-web-off-clause-loses-both-its-duties-on-the-local-tier.md`
  — this file
