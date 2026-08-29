---
id: LESSON-590
title: "Reordering arguments can extend a lock guard's life across a second acquisition"
component: "daemon/session"
domain: "concurrency"
stack: ["rust"]
concerns: ["reliability", "developer-experience"]
tags: ["deadlock", "mutex", "temporary-lifetime", "refactor", "argument-order", "test-hang"]
req: REQ-598
created: 2026-08-29
updated: 2026-08-29
---

## What Happened

REQ-598 bundled a recurring five-field parameter cluster into `DutyContext`. The
new struct's field order put `config` before `router`; the old `*_route`
signatures had ordered `router` before `config`. One test fixture built its
arguments inline:

```rust
runtime.shell_route(DutyContext::detached(
    &Arc::new(EventBus::new()),
    &SessionId::from("sess"),
    &runtime.config.lock().expect("config mutex").clone(),  // guard still held…
    &router_for(&runtime),                                  // …when this locks it again
    ...
))
```

A `lock()` temporary lives to the end of the **enclosing statement**, not to the
end of the argument. Under the old order `router_for` was evaluated *before* the
lock was taken, so the two never overlapped. Under the new order the guard was
still alive when `router_for` re-locked the same non-reentrant
`std::sync::Mutex`, and the thread deadlocked against itself.

The test suite hung for **3 hours 21 minutes at 0% CPU**. Nothing failed;
nothing timed out. It was diagnosed by sampling the process (`sample <pid>`),
which named the blocked thread and the exact lock, in about a minute.

## Lesson

**An argument reorder is a lifetime change.** When a value passed by argument
owns a lock, a file handle, or any other guard, its release point is the end of
the whole statement — so moving it earlier in the argument list extends its life
across everything evaluated after it.

Two practical rules:

- Bind guards to a `let` before the call. `let cfg = m.lock()…clone();` releases
  at that statement's end, and the call that follows starts clean. Every other
  fixture in the same module already did this; the one that did not was the one
  that hung.
- **A hanging suite is a diagnosis, not a wait.** Sample the process before
  assuming slowness. A 0%-CPU process is not working, and three hours of
  "it's a big suite" was three hours of a deadlock nobody was looking at.

## Why It Matters

This class is invisible to the three things normally relied on. Review sees an
argument list that looks like a faithful reordering. The type checker is
satisfied. A green suite proves nothing, because the deadlock is
scheduling- and order-dependent and the tests that pass simply never take the
second lock in the same statement.

It is also exactly the risk REQ-598's own requirement named — "the risk is that
it silently relocates a call whose *ordering* is load-bearing" — arriving
through a door nobody had listed: not a relocated call, but a relocated
*argument*. Any refactor that bundles parameters into a struct changes evaluation
order at every call site that built its arguments inline.

## Applies When

Introducing or reordering a parameter bundle (context struct, options struct,
builder); changing the field order of a struct whose literals are built inline
at call sites; reviewing a "purely mechanical" signature refactor in a codebase
that uses `std::sync::Mutex`; or triaging a test run that has stopped producing
output.
