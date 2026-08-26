---
id: TASK-280
title: "The flush seam: end_block(), owned entirely by the event pump"
status: draft
parent: REQ-592
created: 2026-08-26
updated: 2026-08-26
dependencies: [TASK-279]
---

## Description

Add the defaulted `Surface::end_block()` verb and **every** call site. This task owns the rule
that no line falls off the end of a turn. Covers BR-8.

## Files to Create/Modify

- `crates/teton/src/render.rs` — `fn end_block(&mut self) {}` on the trait (defaulted);
  `PlainSurface`'s impl emits any pending line and any open table run.
- `crates/teton/src/client.rs` — the three call sites: end of `Connection::call` on every return
  including error paths; end of `Connection::drain_events`; immediately before
  `resolve_permission` (~line 427).

## Acceptance Criteria

- [ ] AC-10: **no line falls off the end of a turn.** A turn whose final chunk carries no trailing
      newline still has its last line emitted before the session returns to the entry prompt.
      Both legs: on `RecordingSurface` the tail is emitted rather than held; at the pty the row is
      visible above the entry frame **and appears before any `hand_off_after_turn` line**.
- [ ] Mutation-checked: remove the `end_block()` call in `Connection::call` and AC-10's test fails.
- [ ] The **failed-turn path** is covered: a turn ending in `Err(err)` (not `METHOD_NOT_FOUND`)
      still flushes. This is the path `hand_off_after_turn` never reaches.
- [ ] The **idle path** is covered: fragments arriving via `drain_events` with no turn in flight
      (a second client driving the same session) are emitted, not held.
- [ ] A permission question raised mid-turn paints **below** already-streamed assistant text, not
      above it (ADR-4).
- [ ] **`end_block()` clears the fence bit as well as the buffers.** TASK-279 flagged this: an
      unclosed ```` ``` ```` fence leaves `fence == true`, and without clearing it every subsequent
      line of every subsequent turn renders verbatim — no wrap, no styling. Deciding a block has
      ended is exactly this verb's job, which is why TASK-279 correctly refused to self-clear it.
      Assert it: an unterminated fence in one turn must not swallow the next turn's rendering.
- [ ] `end_block()` is defaulted: `RecordingSurface`, the `Bare` impl (render.rs:597), and
      `provider_test_ui`'s harness are **unmodified**, and no file consuming `&mut dyn Surface`
      changes.

## Technical Notes

**This task owns every `end_block()` call site — nothing else may add one.** Not `main.rs`, not
`hand_off_after_turn`, not a self-flush inside `render.rs`. [[LESSON-547]]: a rule that crosses a
seam is owned by exactly one side, and "the turn loop flushes" plus "the surface flushes" are
indistinguishable at review time from two documents that agree.

**Why not `hand_off_after_turn`** — the obvious site, and wrong. At main.rs:1356:

```rust
match conn.call(params, &mut ctx)? {        // `?` — transport failure escapes
    Ok(res)  => { session_ui::hand_off_after_turn(...); ... }
    Err(err) if err.code == METHOD_NOT_FOUND => { ...; break; }
    Err(err) => { render_turn_failure(&err, ctx.surface); }   // ← no hand-off
}
```

Only `Ok` reaches it. Placing the flush inside `Connection::call` instead covers all three arms
*and* the transport-error return, because the pump is what owns the surface on every path an event
can take.

Defaulted, not required: a required method would ripple through ~15 files that consume
`&mut dyn Surface` plus three impls. The existing `repaint_row_above`/`flush` defaults are the
precedent — and their doc comments explain that silence is correct for a surface with no cursor.
