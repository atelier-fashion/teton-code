---
id: LESSON-613
title: "To prove one path ignores a global setting, assert it never reads the setting — do not turn the setting on"
component: "daemon/tools"
domain: "testing"
stack: ["rust", "daemon"]
concerns: ["reliability", "security", "maintainability"]
tags: ["oncelock", "global-state", "test-isolation", "flaky-tests", "structural-check", "req-596", "mutation"]
req: REQ-607
created: 2026-09-01
updated: 2026-09-01
---

## What Happened

REQ-607 added `[shell] allow_ssh_agent`, which admits one variable to the `shell`
tool's child environment. REQ-596 BR-7.1 requires that turning it on must not
change what a spawned MCP server inherits — the two paths share a composer but
pass their own allowlists.

The obvious test installs a policy with `allow_ssh_agent = true`, composes an MCP
environment, and compares it against the flag-off composition. That was written
first and it passed.

It passed for the wrong reason, and it was quietly dangerous. The policy provider
is a process-global `OnceLock`, so installing a permissive one leaked into every
other test in the same binary — including the `shell` advisory tests, which need
the default posture to have anything withheld to talk about. Whichever test won
the `OnceLock` race decided whether the others were testing what they claimed.
Three consecutive local runs were green; the ordering is not guaranteed across
machines or on a loaded CI runner.

It was also weak on its own terms. The MCP path cannot read the flag at all, so
the two compositions are the same pure function called twice — a tautology that
would keep passing if someone later wired the flag in at a point the test's
particular global did not cover.

The reformulation asserts the real guarantee: **the MCP spawn path never names
any of the three symbols the opt-in travels on**, checked over the module's
source with the corpus cut at its first `#[cfg(test)]`, beside a behavioural arm
that a planted `SSH_AUTH_SOCK` is excluded. Mutating the production code to read
the policy in the MCP composer turns the source arm red and leaves the
behavioural arm green — which is the honest report, and the reason the source arm
is the one with teeth.

## Lesson

When the property is *"this code path is unaffected by setting X"*, the strongest
and safest assertion is usually **"this code path never reads X"**, not "X was set
and nothing happened".

- **It is safer.** Setting a global to prove isolation is a contradiction in
  terms: the mechanism you use to demonstrate containment is itself uncontained,
  and it reaches every sibling test sharing the process.
- **It is stronger.** "I set the global and nothing changed" only covers the
  wiring that goes through *that* global. "The module does not mention the
  symbol" fails on the first line of any wiring at all — which is the regression
  actually worth catching.
- **A `OnceLock` (or any set-once singleton) is not a test seam.** It cannot be
  reset, so the first test to install one silently configures the rest of the
  binary. If a test must install one, it must install the *default* posture, not
  a permissive one.

Pair the structural arm with a behavioural one and say plainly in the doc comment
which mutation each catches, so a later reader does not mistake the behavioural
arm for the whole guard or the structural arm for a tautology and delete it.

## Why It Matters

Both failure modes are silent. The leaked global does not fail the test that
caused it — it weakens *other* tests, which keep passing while asserting less than
they say. And a tautological isolation test is exactly the guard people trust when
they later add the wiring it was supposed to forbid: it was green before the
change and it is green after.

## Applies When

Asserting that a feature flag, policy, or capability does not reach some second
consumer; testing anything mediated by `OnceLock`, `lazy_static`, a process-wide
registry, or an environment variable; or reviewing an isolation test whose setup
mutates global state to make its point.
