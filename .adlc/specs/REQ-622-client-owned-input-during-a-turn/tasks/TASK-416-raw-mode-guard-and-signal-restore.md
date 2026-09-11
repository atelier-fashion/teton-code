---
id: TASK-416
title: "The RawMode guard, one process-wide restore slot, and a sigaction handler that restores and re-raises"
status: complete
parent: REQ-622
created: 2026-09-10
updated: 2026-09-10
dependencies: []
repo: teton-code
---

## Description

In `crates/teton/src/prompt.rs` (ADR-622-1, ADR-622-3): `RawMode::engage() ->
RawOutcome { Raw(RawMode) | NoTerminal | Failed }` clearing only `ICANON` and `ECHO`
with `VMIN=0`, `VTIME=0`, `TCSAFLUSH` on entry, `TCSANOW` on `Drop`, and
`RawMode::is_engaged()`; a `static` restore slot (armed flag + saved termios) written by
`RawMode::engage` **and** `EchoOff::engage`, cleared by both `Drop`s; a `sigaction`
handler for `SIGINT`/`SIGTERM`/`SIGHUP` installed once lazily that restores from the
slot if armed, resets the disposition to `SIG_DFL`, and `raise`s. `read_available(buf)
-> io::Result<usize>`: `poll` with zero timeout then one `read(2)`.

## Files to Create/Modify

- `crates/teton/src/prompt.rs` — `RawMode`, `RawOutcome`, `classify_raw` (fail-open, documented against `classify_echo`'s fail-closed), the restore slot, `install_restore_handlers`, `read_available`; `EchoOff` registers in the slot; tests

## Acceptance Criteria

- [x] `classify_raw(is_tty, got, set)` is pure: not a tty → `NoTerminal`; a tty that fails → `Failed` (fail-open documented); success → `Raw`
- [x] The slot is armed after `engage` and cleared after `Drop` for both guards (unit, by reading the slot through a `#[cfg(test)]` accessor)
- [x] A child-process test: spawn the test binary in a mode that engages the slot with known termios and sends itself `SIGTERM`; the parent observes the child's exit status is the signal's and the child restored termios before dying (the child writes the restored `c_lflag` to a pipe from the handler? no — from a `Drop`-free path: assert via the parent reading the pty's settings). If a pty is not available in unit scope, cover with the pty leg and pin here only the slot bookkeeping and the handler's installation (`sigaction` reports our handler)
- [x] Every `libc` call sits in a documented `unsafe` block naming what is read and written; the handler body is only `tcsetattr`, `sigaction`, `raise`
- [x] Mutation "handler skips the restore" observed red on the pty leg (TASK-419) — recorded there; here, mutation "Drop skips the slot clear" reddens the bookkeeping test
- [~] clippy `-D warnings`, fmt clean, no `#[allow]` — fmt clean and no
  `#[allow]`/`#[expect]`; clippy reports **no** clippy-proper finding, but the
  non-test `teton` bin target reports 8 rustc `dead_code` notices for the
  pump-facing seam (`RawMode`, `RawOutcome`, `RawVerdict`, `classify_raw`,
  `raw_from`, `engage`/`arm`/`is_engaged`, `SLOT_RAW`, `read_available`). A
  bin crate's private module cannot export, so `pub` does not exempt them and
  the only silencer would be an `#[allow]` this task forbids. Every one is
  retired by TASK-417's first `RawMode::engage()`/`read_available` call in
  `client.rs` and TASK-418's `is_engaged()` in the prompter; `-D warnings` is
  red between this commit and TASK-417's.

## Verification

| rule | kind | artifact | benign_path |
|------|------|----------|-------------|
| BR-1 | test-case | `crates/teton/src/prompt.rs::tests::raw_mode_changes_only_icanon_and_echo` | yes |
| BR-7 | test-case | `crates/teton/src/prompt.rs::tests::both_guards_arm_and_clear_the_restore_slot` | no |
| BR-8 | structural-check | `crates/teton/src/prompt.rs`: the handler resets `SIG_DFL` and raises (asserted by `the_handler_re_raises_after_restoring`) | no |
| BR-11 | test-case | `crates/teton/src/prompt.rs::tests::classify_raw_fails_open` | yes |

## Technical Notes

- The slot: `static RESTORE: RestoreSlot` with `AtomicBool armed` and a `UnsafeCell<libc::termios>`; write the termios, then store `armed = true` with `SeqCst`; the handler loads `armed` first. Document why a `Mutex` is not async-signal-safe.
- `raise` after `SIG_DFL` keeps the kernel's exit status (BR-8). Install handlers with `SA_RESETHAND` unset so a second Ctrl-C at the prompt behaves identically.
- Two guards are never engaged at once (ADR-622-3); assert it in debug builds.

## What landed

`crates/teton/src/prompt.rs` only.

- `RawMode` / `RawOutcome { Raw | NoTerminal | Failed }` / `RawMode::is_engaged`,
  with the flag arithmetic in `raw_from(saved) -> termios` (`ICANON`, `ECHO`,
  `VMIN=0`, `VTIME=0`, `TCSAFLUSH` in; `TCSANOW` on `Drop`) and the verdict in
  the pure `classify_raw`, documented fail-open against `classify_echo`'s
  fail-closed.
- One process-wide `RestoreSlot`: `AtomicU8` owner (`SLOT_EMPTY`/`SLOT_RAW`/
  `SLOT_ECHO_OFF`) plus `UnsafeCell<MaybeUninit<libc::termios>>`. A word rather
  than a bool because the handler asks "is anything owed" and `is_engaged` asks
  "is the terminal raw", and echo-off answers those differently. Armed by both
  guards' `arm`, cleared by both `Drop`s.
- `install_restore_handlers()` (a `Once`, `SA_RESETHAND` unset) and
  `restore_and_reraise`: `tcsetattr` if armed, `SIG_DFL`, `raise`.
- `read_available(buf)`: `stdin_ready(ZERO)` then one `read(2)`; `Ok(0)` for
  nothing waiting, EOF and `EINTR`.
- REQ-572's "accepted residual" paragraph rewritten: the residual is retired,
  and the slot plus the handler are why (AC-6).

Two deliberate departures from the task's Technical Notes, both for the same
reason — the window a signal can land in:

1. **Both guards arm the slot *before* their `tcsetattr`, not after.** A signal
   between the arm and the change writes back settings that are still in
   effect, which is free; arming afterwards leaves a window in which the
   terminal is changed and nothing knows the undo.
2. **Both `Drop`s restore *before* they clear**, the reverse of the note's
   "clear before restoring". Clearing first leaves a window in which the slot
   says "nothing owed" while the terminal is still changed — the one ordering
   that loses the restore BR-7 exists to guarantee. `store` keeps the note's
   order (termios first, owner word second), which is the ordering that
   matters for publication.

**Mutation.** `Drop skips the slot clear` (removing `RESTORE.clear()` from
`RawMode`'s `Drop`) reddens `both_guards_arm_and_clear_the_restore_slot` and
nothing else — 1 of 835. Over a pipe it fails on ```RawMode`'s `Drop` must
clear the slot``; under a pty it fails earlier, on "dropping it must leave the
slot empty", because there the real `engage` arms first. Reverted by targeted
edit; suite green again.

**For TASK-419.** macOS sets `PENDIN` in `c_lflag` on the way out of
non-canonical mode, so a `tcgetattr` (or `stty -a`) taken after a raw/restore
cycle differs from one taken before by a driver status bit no code in this file
wrote. A pty leg that compares full settings either side of a turn must mask it
(or compare only `ICANON`/`ECHO`/`VMIN`/`VTIME`). Measured here at 0x5cb ->
0x200005cb.
