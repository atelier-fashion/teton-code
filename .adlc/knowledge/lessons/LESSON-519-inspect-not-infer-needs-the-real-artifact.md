---
id: LESSON-519
title: "An 'assert by inspection, not from the error' AC needs the real artifact — add a refusing test seam to reach it"
component: "daemon/session"
domain: "harness"
stack: ["rust", "daemon", "keychain"]
concerns: ["security", "reliability", "developer-experience"]
tags: ["acceptance-criteria", "test-seam", "presence", "inspect-dont-infer", "config", "req-575", "req-570"]
req: REQ-575
created: 2026-08-14
updated: 2026-08-14
---

## What Happened

REQ-575's AC-1 demanded a refused presence-gated commit be proven to write
nothing "by inspecting [config on disk and in-memory state], **not inferred from
the error**." The delivered unit test used a bare `Daemon::new()` — which has no
config path at all — and reasoned "no `CONFIG_REJECTED`, so nothing was
written." Three reviewers flagged it: that is exactly the inference the AC rules
out (a guard against a future refactor that writes *before* returning an error,
per LESSON-445). The obstruction was real: the only present-but-refusing
verifier (`AlwaysFailsVerifier`) is injectable only in-process, while a real
config file exists only in the spawned-binary harness — and the spawned harness's
presence seam offered only `accept` and `unavailable`, no "refuse".

## Lesson

An AC worded "assert by inspecting X, not from the error" is a *testability
obligation*, not prose — satisfy it against the real artifact. When the refusing
path and the real artifact live in different harnesses, add the missing **test
seam** rather than settling for inference: here, a `TETON_PRESENCE_ACCEPT=fail`
arm installing `AlwaysFailsVerifier`, letting a spawned daemon with a real config
be driven into the refusal so the test reads the config bytes (before == after)
and the live state (still `off_available`) back from the world. Pair it with the
happy-path test on the same fixture so a regression that *did* write flips both.

## Why It Matters

"Inspect, don't infer" exists because an error code is a claim, not evidence:
the write may land and *then* the error return. A test that only reads the code
passes even when the config was already rewritten. Adding the seam is cheap and
stays safe — it rides the `TETON_TEST_SEAMS` master switch, so a release build
refuses it — and it can only ever *deny*, never grant.

## Applies When

Any AC that says "inspect, don't infer"; any presence/consent refusal you want to
prove is inert; any time the fixture that can *refuse* and the fixture that owns
the *artifact* are different harnesses. REQ-576 will reuse the `fail` seam.
See [[lesson-508-a-redundant-guard-needs-its-own-test]].
