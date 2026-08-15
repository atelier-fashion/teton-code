---
id: BUG-173
title: "The pty suite's entry-prompt wait absorbs daemon startup, so a slow CI runner reads as a failing test"
status: resolved
severity: low
created: 2026-08-14
updated: 2026-08-14
component: "cli"
domain: "harness"
stack: ["rust", "cli", "pty", "ci"]
concerns: ["test-determinism", "developer-experience"]
tags: ["pty-e2e", "flaky-test", "entry-prompt", "readiness-barrier", "autostart-race", "linux", "REQ-556", "BUG-164"]
found_by: "CI on PR #151 (a docs-only change), 2026-08-15"
introduced_by: REQ-556
---

## Description

`crates/teton/tests/pty_e2e.rs::the_status_row_shows_the_session_s_web_capability`
failed once on the **ubuntu-latest** leg with "the session never reached the
entry prompt" and a transcript holding only the CLI's banner art, then passed on
re-run of the same SHA. The PR under test changed two markdown files under
`.adlc/` and cannot have touched session code.

The suite's `wait_for` polls the pty transcript every 25ms until a fixed
`WINDOW` of 20s. The structure is right — it asserts on state reached, never on
a fixed sleep (LESSON-450) — but the ceiling was doing double duty. Unlike
`cli_e2e.rs`, whose `TestDaemon` has always called `wait_for_socket()` before
any client runs, the pty suite's `TestDaemon` (an explicit copy of "`cli_e2e`'s
fixture shape") spawned the daemon and returned immediately. The entry-prompt
window therefore had to absorb, in sequence: daemon process spawn, the daemon's
full runtime assembly, the pty client's process spawn, banner, socket
handshake, the outstanding-model-proposal RPC, and `session/create` — all on
whatever scheduling a shared CI runner felt like providing, with the suite's
other three tests (each carrying its own daemon, client, and scripted turns)
running concurrently on the same box.

All four tests in the file share the same `wait_for`/`WINDOW` mechanism, so all
four carried the same exposure; this one is simply where it first landed.

## Reproduction Steps

Not reproduced deliberately — observed once and confirmed intermittent by
re-run:

1. Push any commit and let CI run (the observed failure was on PR #151, a
   two-file `.adlc` markdown change).
2. Observe the ubuntu-latest leg fail in `pty_e2e` with "the session never
   reached the entry prompt" after ~20s, the transcript showing only banner art.
3. Re-run the failed job on the identical SHA — it passes.

Locally the suite finishes in well under a second per test; the failure needs a
degraded runner, which is exactly why the fixed 20s ceiling and not the code
under test is the variable.

## Expected Behavior

A slow machine costs latency, never a verdict (LESSON-450). The wait should
bound only a genuinely hung session, and the harness should not start the
clock on a client whose daemon may still be assembling.

## Actual Behavior

```
the session never reached the entry prompt; transcript:
<ASCII banner art only>
```

after ~20 seconds, on a leg whose change could not affect the behavior under
test. `fmt · clippy · test (macos-latest)` passed on the same commit.

## Environment

- Platform: ubuntu-latest (GitHub Actions), run 31853759487, 2026-08-15. Not
  observed on macos-latest.
- Version: workspace 0.1.15.
- The transcript's shape (banner, then nothing) says the client got as far as
  printing the banner and never received `session/create`'s answer — or never
  got scheduled long enough to send it — inside what remained of the window.

## Root Cause

Two composing facts, one observed and one latent:

1. **The 20s `WINDOW` was a ceiling real startup could reach.** The
   entry-prompt wait is the first `wait_for` of every test, so it pays for the
   entire cold path: two process spawns of large debug binaries, daemon runtime
   assembly, and three client RPC round-trips, concurrently with three sibling
   tests doing the same. On a degraded runner that sum crossed 20s with every
   process behaving correctly — the re-run passing on the same SHA is the
   proof. A deadline that a healthy-but-slow run can cross converts machine
   weather into a red X, which teaches everyone to re-run the suite — the harm
   BUG-163 names for the attach suite, here in its milder, explicable form.

2. **No readiness barrier, so the fixture also carried an autostart race.**
   `TestDaemon::spawn_with` returned the moment the daemon process existed.
   A pty client that reached the socket before the daemon bound it walked
   `teton`'s autostart path (`client.rs::ensure_connected`): it spawned a
   *second* daemon from beside its own binary, inheriting none of the fixture's
   seams (`TETON_LOCAL_SCRIPT`, `TETON_TEST_SEAMS`, the probe pins), and raced
   the fixture for the single-instance flock. On a win, the session would be
   served by a daemon probing the real machine with no scripted tier — the
   exact signature BUG-164's resolution records from its rejected
   build-from-the-harness approach ("autostarted its own, hitting the
   model-consent prompt and timing out at the 20s window"). The observed
   failure did not take this path (its transcript lacks the "no daemon
   reachable — starting teton-code…" line), but the same missing barrier
   allows it.

`cli_e2e.rs` has neither problem: its fixture blocks in `wait_for_socket()`
until the daemon accepts a connection, before any client is spawned. The pty
fixture is documented as a copy of that shape; the barrier is the piece the
copy dropped.

This is distinct from BUG-163, which tracks an attach-response delivery stall
in `tetond/tests/attach_authorization.rs` whose cause is still unnamed and
where the report explicitly warns against raising the deadline. That warning
does not transfer here: BUG-163's deadline makes a *withheld frame* loud, and
its capture proves the daemon side completed. This wait is a *readiness*
bound over real, measurable startup work, and the observed failure is fully
explained by that work exceeding the ceiling.

## Resolution

Two changes in `crates/teton/tests/pty_e2e.rs`, both to the harness, neither
to what any test asserts:

1. **The fixture now gates on its daemon accepting a connection.**
   `TestDaemon::spawn_with` ends in `wait_for_socket()`, mirroring `cli_e2e`'s:
   poll `UnixStream::connect` against the fixture's socket every 25ms, panic
   with the daemon's log quoted if the window empties. The daemon binds its
   socket only after `DaemonRuntime::from_env` completes (`tetond/src/main.rs`
   H-1: assembly strictly precedes bind) and serves the accept loop
   immediately after, so a successful connect means startup is over.
   Consequences: daemon startup time is excluded from every test's first
   `wait_for`, and the client autostart path is structurally unreachable — a
   client cannot lose a race the harness no longer runs.

2. **`WINDOW` raised from 20s to 60s.** Every assertion in the file triggers
   on state reached, so a passing run returns the moment its marker lands and
   never pays the ceiling; the extra 40s is spent only on a run that is
   already failing. What the larger number buys is that the bound is one
   machine slowness cannot reach, which is what LESSON-450 requires of it.

Deliberately not done: no retry loops, no assertion weakened, no change to the
20s deadlines in other suites (`attach_authorization.rs`'s `READ_DEADLINE` is
BUG-163's instrument and its report forbids touching it; `cli_e2e`'s
turn-completion waits sit behind a readiness barrier already and have not
flaked).

## Verification

- `cargo build --workspace` first (BUG-164: a targeted `-p teton` run does not
  rebuild the daemon).
- `cargo test -p teton --test pty_e2e --no-fail-fast`: 4/4, run **six times
  consecutively** — all green, total wall time per run 0.6–1.5s (the barrier
  adds no measurable cost on a healthy machine).
- `cargo test -p teton --no-fail-fast`: 336 unit + 30 `cli_e2e` + 4 `pty_e2e`,
  all passing.
- `cargo clippy -p teton --all-targets` clean; `cargo fmt --all --check` clean.
- The flake itself is a CI-weather event and cannot be reproduced on demand;
  the structural claim (daemon startup excluded from the client's window, no
  autostart possible) holds by construction and the barrier's failure mode is
  a loud panic quoting the daemon's own log.

## Deployment

Test-harness only — no shipped binary changes. Lands with the next merge to
`main`; no release, migration, or operator action.

## Files Changed

- `crates/teton/tests/pty_e2e.rs` — `WINDOW` 20s → 60s with the rationale in
  its doc comment; `TestDaemon::wait_for_socket()` readiness barrier added and
  called from `spawn_with`, doc comment carrying the race it closes and the
  BUG-164 precedent for its failure signature.
- `.adlc/bugs/BUG-173-the-pty-entry-prompt-wait-absorbs-daemon-startup.md` —
  this record.
