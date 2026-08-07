---
id: LESSON-485
title: "A test whose fixture cannot reach the discriminating state is not a test"
component: "testing"
domain: "testing"
stack: ["rust"]
concerns: ["correctness", "test-coverage"]
tags: ["mutation-testing", "fixtures", "false-coverage", "verification"]
req: REQ-558
---

## What Happened

REQ-557 and REQ-558 each passed a full green suite and a validation gate, and each
then had serious defects found by an adversarial review panel. Across the two, the
**same failure appeared five times** — and never as a missing test. Every one was a
test that existed, passed, was correctly named, and could not fail:

1. **`a_tainted_session_cannot_fail_over_to_a_remote_provider`** — written to pin
   BUG-156's fix, stayed green under a faithful restoration of the bug. Its fixture
   bound no tier to `local`, and the defect only fires when the failed provider is
   some row's primary.
2. **`policy show`'s source column** — passed under a mutation replacing the
   `source` field with a recomputation from the provider id. No row in the fixture
   differed between the two.
3. **The AC-8 e2e's "intent-classified" leg** — asserted `category == "edit"`,
   which holds identically under a classifier bypass, because the scripted engine
   answers with `Edit` (also the declared default) *and* the bypass sentence also
   contains `'edit'`.
4. **`unserved_turn_sentence`'s guard** — deleting `if route.selected() { return
   classified; }` left **466 tests across 4 binaries** green. No fixture ever
   constructed a route that *selected* a provider the harness still could not serve.
5. **`route_never_resolves_to_a_remote_provider`** — written to prove BR-5, asserted
   `Some(LOCAL)`. With a remote provider registered under the id `local`, the pin
   resolved to a remote HTTP endpoint and the test stayed green, because it checked
   the **name** rather than what the daemon would **do**.

Two doc comments made the same error in prose: one claimed a mutation had been
verified red when it had not, another claimed a hand-written `Deserialize` covered
the JSON-RPC path when that path uses a different type.

## Lesson

Coverage is not "a test executes this code". Coverage is "a test **fails** when this
code is wrong." The gap between those is a fixture that never reaches the state
where correct and incorrect behaviour differ.

Three habits close it:

**Name the discriminating state before writing the fixture.** For any assertion,
ask: *what would make this line print a different value?* If the answer is "nothing
in this fixture", the test is decoration. In #2 above, no row was simultaneously
inherited-but-sharing-a-provider-with-an-override — so nothing distinguished
`source` from a recomputation.

**Assert on behaviour, not on names.** #5 is the sharpest case: `assert_eq!(provider,
Some("local"))` and "this never leaves the machine" are different claims, and the id
was the wrong one. Prefer the assertion the guarantee is actually about — *would the
daemon dial this?* — over the one that happens to be in a variable.

**A mutation is the only real check, and a green one is a finding.** Every instance
here was found by running a mutation, never by reading. When one comes back green,
the finding is the fixture — do not "fix" it by adding an assertion elsewhere.

## Why It Matters

This class of test is worse than no test. A missing test is visible in a coverage
gap and in review. A non-discriminating test reads as coverage, counts as coverage,
and is cited as coverage in a PR description — so the area it names is the *least*
likely to get a second look.

Both REQs shipped through review with these in place. REQ-558's panel found 18
issues against a suite of 1083 passing tests, including two upgrade-breaking
regressions and a privacy pin that dispatched over HTTP while its own test asserted
it was local.

## Applies When

- Writing a test to pin a fix. Ask whether the fixture reproduces the bug's
  **precondition**, not just its area. Restore the bug and confirm red.
- Any assertion on an identifier, name, or label that stands in for a property
  (locality, usability, freshness, ownership). Assert the property.
- A test whose subject is "X happened rather than Y" where X and Y produce the same
  observable value in the fixture — classifier-vs-default, override-vs-inherited,
  cached-vs-computed.
- Reviewing a green mutation result. It is evidence about the tests, never about the
  code.

Related: [[LESSON-483]] (a mutation check on the outer guard says nothing about the
inner one), [[LESSON-441]] (a deletion is verified only by proving restoration breaks
something), [[LESSON-479]] (a subset invariant only holds where you iterate).
