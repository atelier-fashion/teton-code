---
id: LESSON-545
title: "Splitting one decision into two settable fields silently re-points every caller that set only one — and the fixture keeps passing under its old name"
component: "daemon/harness"
domain: "harness"
stack: ["rust", "daemon"]
concerns: ["reliability", "developer-experience"]
tags: ["testing", "currency", "config-struct", "vacuous-test", "silent-regression", "lesson-446", "req-586"]
req: REQ-586
created: 2026-08-20
updated: 2026-08-20
---

## What Happened

The `digest` duty had one threshold expressed in two currencies: a caller set
`summarize_threshold_tokens`, and the byte twin was *derived* at the call site
as `threshold_tokens × APPROX_BYTES_PER_TOKEN`. REQ-586 needed the two to
scale independently with the route budget, so it made the byte twin its own
`HarnessConfig` field.

Every production path was updated together — `with_route_budget` sets both.
But `conversation_carry`'s compaction fixture set only the token half:

```rust
// A tool result enters context whole: these fixtures fill the budget
// deliberately, and a digest duty condensing them first would be the
// thing under test rather than compaction.
summarize_threshold_tokens: usize::MAX,
..HarnessConfig::default()
```

Before the split, `usize::MAX × 8` saturated and nothing was ever digested —
which is exactly what the comment asks for. After it, `..default()` quietly
supplied a 12,000-byte twin, every tool result was digested, the conversation
never grew, and `a_session_driven_past_its_budget_compacts_and_keeps_answering`
stopped pressing a budget **while still passing**. It was found only because
an implementer noticed the retained-token figures (65, 129, 191, 250 against
4,096) looked wrong in an unrelated failure.

## Lesson

**When you split one decision into two fields, the callers that set one of
them are silently re-pointed at a default for the other.** The compiler cannot
help: `..Default::default()` is exactly the idiom that makes the change
invisible, and it is idiomatic Rust.

Two habits fall out. First, grep for every caller that sets the old field and
decide, per caller, what the new one should be — the ones that used a
saturating sentinel are the dangerous ones, because their *intent* was
"never", and a default is not "never". Second, make the intent sayable in one
call: `HarnessConfig::without_digest()` sets both, so the next fixture that
wants tool results kept whole cannot express half of it.

LESSON-446 says two limits on the same text must share a currency or own an
explicit conversion. This is its sequel: when they stop sharing, the
conversion does not disappear — it moves into every caller, silently.

## Why It Matters

The test kept its name, its comment, and its green tick while measuring
something else entirely. That is worse than a deletion: the suite now asserts
a property nobody holds, and the next person to change compaction will trust
it. A defect like this survives exactly as long as nobody reads the numbers.

## Applies When

Replacing a derived value with a stored one; adding a second currency,
threshold, or unit to a config struct; any change where `..Default::default()`
now fills a field a caller previously controlled indirectly. Also: whenever a
fixture uses a sentinel (`usize::MAX`, `0`, `None`) to mean "disable this" —
that intent is the first casualty of a field split.
