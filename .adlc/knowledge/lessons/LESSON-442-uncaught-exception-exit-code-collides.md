---
id: LESSON-442
title: "An uncaught exception's exit code can collide with a meaningful one"
component: "ci/catalog"
domain: "infra"
stack: ["python", "ci", "bash"]
concerns: ["reliability", "observability"]
tags: ["exit-codes", "error-classification", "flake", "transport-error", "ci-signal"]
req: REQ-547
created: 2026-07-24
updated: 2026-07-24
---

## What Happened

`tools/refresh-catalog.py` deliberately distinguishes two outcomes by exit code:
`1` = MISMATCH (a real integrity failure — the catalog disagrees with upstream)
and `75` = UNVERIFIED (could not reach the host; nothing was learned). CI maps
them to different messages precisely so an outage can never be announced as
corruption.

It caught `urllib.error.URLError`, but a connection reset *while reading the
response* raises a bare `ConnectionResetError`, which was uncaught. **Python's
exit code for an uncaught exception is 1** — identical to EXIT_MISMATCH. So a
transient network reset produced a stack trace and CI announced
"a genuine integrity failure, NOT a network problem". It surfaced as a ~1-in-50
test flake; the flake was the symptom, the classifier bug was the disease.

(Root cause underneath: on macOS/BSD a socket returned by `accept(2)` **inherits
`O_NONBLOCK`** from a non-blocking listener, so a mock server dropped connections
under load. The same defect was silently recording empty request bodies
elsewhere, quietly weakening an unrelated egress-capture assertion.)

## Lesson

When exit codes carry meaning, enumerate what the runtime does with codes you did
not choose. `1` is the default for an uncaught Python exception, a failed shell
command, and `SystemExit(str)` — so never assign `1` a specific meaning without a
top-level handler that routes anything unforeseen to the "unknown/unverified"
code. Catch by *behaviour class* (`OSError`, `HTTPException`,
`JSONDecodeError`), not by the one exception type you happened to see.

Corollary: an intermittent test failure in a classification path is worth
root-causing rather than retrying — chase the captured output before proposing a
fix. Here the hypothesis on file was wrong, and only a real captured traceback
revealed the exit-code collision.

## Applies When

Writing any script whose exit code is a signal (CI gates, health checks, hooks);
mapping exit codes in CI; catching network errors around `urlopen`/`requests`;
diagnosing a flaky test that asserts on an error message or exit status; writing
a mock server with a non-blocking listener (set blocking mode + timeouts on the
accepted socket explicitly).
