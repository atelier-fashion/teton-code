---
id: BUG-167
title: "The llama-gated template smoke no longer compiles"
status: resolved
severity: low
created: 2026-08-14
updated: 2026-08-14
component: "daemon"
domain: "harness"
stack: ["rust", "daemon", "ci"]
concerns: ["reliability", "developer-experience"]
tags: ["llama", "feature-gate", "compile-drift", "template-smoke", "req-564", "ci-blind-spot", "bug-164"]
---

## Description

REQ-564 (PR #81) added a `SessionId` parameter to `LocalEngineSource::new` —
the prefix cache's key — and updated every call site the compiler could see.
`crates/tetond/tests/template_smoke.rs` is `#![cfg(feature = "llama")]`, so it
was outside the compiler's sight for that change and for every API pass since
(REQ-571 among them), and kept passing two arguments. The ungated
`conversation_carry.rs` constructs the same source with three arguments; only
the gated target could drift.

Default (no-llama) CI never compiles this target — that is the point of the
gate, since compiling it means building llama.cpp — so the break was invisible
to every automated leg. This is the compile-time member of the BUG-164 family:
llama-gated suites are manual gates, and a manual gate can rot between the
moments someone runs it.

The consequence is not user-facing. It is that REQ-554 AC-6's acceptance
vehicle — the one real-weights check that the ChatML template path emits a
well-formed tool call (mock-only green is not acceptance, LESSON-433) — could
not even be built, and nobody would learn that until the next time real-weights
verification was actually needed.

## Reproduction Steps

1. Check out `origin/main` at `d093ede` (or any commit since PR #81).
2. `cargo build --release -p tetond --features llama --test template_smoke`
3. E0061 at `tests/template_smoke.rs:82`: `LocalEngineSource::new(engine,
   format)` is missing the `SessionId` argument.

## Expected Behavior

Every target in the workspace compiles under every supported feature
combination, whether or not any CI leg happens to exercise it.

## Actual Behavior

The `template_smoke` test target fails to compile with `--features llama`, and
has since REQ-564 merged (PR #81, 2026-08-10) — the breakage shipped in the
0.1.14 tag.

## Environment

- Platform: macOS (Apple Silicon), toolchain per workspace
- Version: broken since PR #81 (post-0.1.13); present in the 0.1.14 tag
- Affected: any `--features llama` build of the `template_smoke` target.
  Default builds and CI are unaffected — which is precisely the defect's cover.

## Root Cause

Two facts compose:

1. **The compiler's "all call sites updated" verdict is scoped to the features
   it compiled with.** `cfg`'d-out code is not checked, so a shared-API change
   can be complete for the always-on surface and silently incomplete for the
   gated one.
2. **No automated leg compiles the gated surface.** CI runs the default
   feature set to avoid the llama.cpp native build, so nothing re-checks
   gated targets after a shared API moves. BUG-164 documented the runtime
   flavor of this blind spot (a gated e2e can exercise a stale daemon); this
   is the compile flavor (a gated target stops building at all).

## Resolution

Pass the session id at the call site —
`LocalEngineSource::new(engine, format, SessionId::from("template-smoke"))` —
matching the idiom of the in-crate unit tests and `conversation_carry.rs`.
Two lines: the import and the argument.

The rest of the gated surface was swept for the same drift. Everything gated
on `feature = "llama"` (`tetond/tests/template_smoke.rs`,
`teton-inference/tests/llama_smoke.rs`, and the gated source in `runtime.rs`,
`lib.rs`, `engine.rs`) type-checks clean; only `template_smoke` had drifted.

This PR fixes the instance. The class — no CI leg compiles gated targets —
remains open here; PR #129, prepared in parallel, proposes an all-features
clippy leg to close it structurally.

## Verification

- The exact repro command,
  `cargo build --release -p tetond --features llama --test template_smoke`,
  exits 0.
- The sweep, `cargo check -p tetond -p teton-inference
  --features tetond/llama,teton-inference/llama --tests`, is clean with no
  warnings.
- The smoke itself remains `#[ignore]`d and was not run — it needs real
  weights via `TETON_TEST_GGUF`, and the defect was compile breakage, not
  smoke behavior. The next real-weights run is unblocked.

## Lessons Captured

- `LESSON-515` — a feature-gated target is invisible to every refactor: the
  compiler's completeness verdict is scoped to the features compiled with, so
  gated targets need an explicit sweep after any shared-API change.

## Files Changed

- `.adlc/knowledge/lessons/LESSON-515-a-feature-gated-target-is-invisible-to-refactors.md` — new.
- `crates/tetond/tests/template_smoke.rs` — import `SessionId`, pass it at the
  `LocalEngineSource::new` call site.
