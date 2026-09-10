---
id: TASK-416
title: "The RawMode guard, one process-wide restore slot, and a sigaction handler that restores and re-raises"
status: draft
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

- [ ] `classify_raw(is_tty, got, set)` is pure: not a tty → `NoTerminal`; a tty that fails → `Failed` (fail-open documented); success → `Raw`
- [ ] The slot is armed after `engage` and cleared after `Drop` for both guards (unit, by reading the slot through a `#[cfg(test)]` accessor)
- [ ] A child-process test: spawn the test binary in a mode that engages the slot with known termios and sends itself `SIGTERM`; the parent observes the child's exit status is the signal's and the child restored termios before dying (the child writes the restored `c_lflag` to a pipe from the handler? no — from a `Drop`-free path: assert via the parent reading the pty's settings). If a pty is not available in unit scope, cover with the pty leg and pin here only the slot bookkeeping and the handler's installation (`sigaction` reports our handler)
- [ ] Every `libc` call sits in a documented `unsafe` block naming what is read and written; the handler body is only `tcsetattr`, `sigaction`, `raise`
- [ ] Mutation "handler skips the restore" observed red on the pty leg (TASK-419) — recorded there; here, mutation "Drop skips the slot clear" reddens the bookkeeping test
- [ ] clippy `-D warnings`, fmt clean, no `#[allow]`

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
