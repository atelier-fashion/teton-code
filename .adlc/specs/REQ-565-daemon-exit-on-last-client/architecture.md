# REQ-565 — Architecture: On-demand daemon lifetime (exit with the last client)

Inverts the daemon's lifetime: the CLI already autostarts it
(`crates/teton/src/client.rs:606` `ensure_connected`), so the daemon counts
its handshaked clients and exits from idle after the last one leaves, and the
shipped packaging stops resurrecting it.

## Approach

A **pure state machine** owns the whole arm/disarm/defer/commit decision and
lives in `teton-core` with no I/O, no tokio, and no socket (BR-9, AC-9). A thin
async **supervisor** in `tetond` wraps it in a mutex, hands out RAII guards for
the two things that pin the daemon (connected clients, in-flight work), emits
the event vocabulary, and signals the accept loop. Everything else — the
handshake refusal, the ordered teardown, the flock hand-off, the packaging —
is a consumer of that one decision point.

The single serialization rule that makes BR-3 true: **admission and commit are
the same mutex**. A client is admitted (count += 1) or refused under the same
lock that flips the state to `Committed`, so there is no interleaving in which
a daemon accepts a session it will not serve.

## Key Decisions

### D-1: The count increments at handshake completion, not at accept (BR-1)

The spec's Events table already says `client_connected` triggers when "handshake
completes", and that is load-bearing rather than cosmetic. A bare
`UnixStream::connect` that never handshakes — the CLI's own socket probe
(`poll_for_daemon`, `client.rs:679`) and the e2e harness's readiness poll
(`tests/e2e/harness.rs:536`) both do exactly this — must not pin the daemon,
and must not arm shutdown when it drops. Counting at `accept` would make every
liveness probe a phantom client whose disconnect kills the daemon.

Consequence: an authenticated-but-silent peer cannot hold the daemon open
either, which is the right posture.

### D-2: Arming happens only on the 1 → 0 transition; startup is a separate state (BR-1)

A daemon begins life with zero clients. If "zero clients" alone armed shutdown,
the daemon would commit to exiting before the CLI that spawned it ever
connected. The machine therefore starts in `AwaitingFirstClient`, which never
arms, and only a *decrement to zero* arms.

That leaves an orphan case the spec does not name: a CLI that spawns the daemon
and then dies during its 5 s autostart poll (`POLL_ATTEMPTS` 50 ×
`POLL_INTERVAL` 100 ms) would strand a daemon with zero clients forever. So
`AwaitingFirstClient` carries a bounded **startup grace** (default 60 s, an
order of magnitude above the client's own poll budget) after which an
un-contacted daemon exits with reason `startup_unclaimed`. Under the `never`
policy the startup grace does not apply.

### D-3: The blocking-work list is a guard-counted multiset, not a boolean (BR-2)

`BlockingActivity::{Turn, ModelDownload, ModelLoad, LedgerFlush}` are counted,
not flagged, because two turns can overlap on one connection and the daemon must
stay alive until the *last* completes. Each is acquired as an RAII
`ActivityGuard`, so a panicking or cancelled future releases its claim rather
than wedging the daemon alive forever (the same failure mode
`InFlightGuard` in `model_consent.rs:1616` already guards against, and for the
same reason).

`ModelDownload` / `ModelLoad` reuse the existing claim: `ModelConsentGate`
already holds one in-flight claim across download → verify → load
(ADR-006), so the supervisor queries it rather than inventing a second source of
truth that could disagree.

`LedgerFlush` is declared for vocabulary completeness but is **structurally
empty**: the cost ledger is SQLite in autocommit
(`cost/ledger.rs:811` `insert_and_emit` executes a single `INSERT` per row), so
a recorded row is already durable when `record` returns. There is no buffer to
flush. What actually threatens ledger integrity is D-4.

### D-4: Client teardown must stop aborting in-flight prompt turns (BR-2, AC-3)

Today `handle_client` ends with `for task in prompt_tasks { task.abort(); }`
(`server.rs:297`). A client that disconnects mid-turn *kills* the turn at
whatever await point it had reached — so the turn never reaches its
`record_call`, and AC-3's "the ledger row for that turn is intact" is false
today.

The fix is the deferral itself: the turn holds an `ActivityGuard(Turn)` for its
whole execution, and teardown **awaits** the outstanding prompt tasks instead of
aborting them. Ordering inside teardown is deliberate:

1. drop the `ClientGuard` → count 1 → 0 → `daemon_shutdown_armed`
2. evaluate → the turn's guard is live → `daemon_shutdown_deferred{turn}`
3. await the prompt tasks → the turn completes and records its row
4. the last `ActivityGuard` drops → re-evaluate → commit → exit

The turn's streamed output goes nowhere (the writer half is gone); that is
fine and intended. The *ledger row* is the durable artifact AC-3 asserts on.

### D-5: Admission and commit share one mutex; the refusal is a typed wire error (BR-3)

`LifetimeSupervisor::admit() -> Option<ClientGuard>` is called from
`do_handshake` after version negotiation succeeds. Under the state mutex:

- phase is not `Committed` → count += 1, disarm any pending arm/linger, return a
  guard. A client arriving into an *armed* (not yet committed) daemon therefore
  **cancels** the shutdown — the first arm of BR-3.
- phase is `Committed` → return `None`; the handshake answers
  `error_code::DAEMON_SHUTTING_DOWN` — the second arm of BR-3.

Because the flip to `Committed` happens under the same lock, the two arms are
exhaustive and there is no third outcome.

### D-6: The exiting daemon holds the flock past the unlink, and a successor waits for it (BR-3, BR-8)

`SingleInstance` (`single_instance.rs`) is held by `main` for the process
lifetime and released when the guard drops at process exit. The ordered teardown
runs *inside* that lifetime, so the socket is unlinked while the lock is still
held — a racing autostart can never find a stale socket left by a daemon that
has already released the lock.

That ordering creates the mirror-image hazard, and it is the one gap the spec's
assumption does not cover: a successor spawned during the predecessor's teardown
hits `flock` `EWOULDBLOCK`, prints "already running", and exits 0 — after which
the CLI's `poll_for_daemon` polls a socket that will never appear, and the user
gets "could not reach the daemon after autostart". So `SingleInstance::acquire`
gains a **bounded retry** (default 5 s at 25 ms, matching the existing test
helper's `acquire_within` shape) before it concludes another daemon is genuinely
live. A predecessor mid-teardown is transient by construction; a real live
daemon still yields "already running", just 5 s later — and in that case the
CLI's *first* connect attempt would have succeeded anyway, so the slow path is
unreachable in the common case.

### D-7: Policy is one value resolved flag > env > config > default (BR-7, OQ-2)

```
[lifetime]
shutdown = "on-last-disconnect" | "linger" | "never"   # default on-last-disconnect
linger_seconds = 0                                      # only meaningful in linger
```

with `--shutdown-policy <mode>` / `--linger-seconds <n>` flags on `teton-code`
and `TETON_SHUTDOWN_POLICY` / `TETON_LINGER_SECONDS` env overrides.
`PolicySource::{Default, Config, Env, Flag}` is reported for diagnostics.

**`Flag` extends the spec's `source` enum** (`default | config | env`)
deliberately: OQ-2's resolution requires the formula to pass the policy
"explicitly (flag/env)", and a flag is the better of the two — it is visible in
the launchd plist and in `ps`, it cannot leak into unrelated child processes the
way an exported env var can, and it cannot be silently dropped by a launchd
environment that does not propagate it.

These are **operator** settings, not test seams: they are deliberately *not*
gated behind `TETON_TEST_SEAMS` (`runtime.rs:4358`), because a release build must
honour them — the shipped formula depends on it. This follows the existing
`TETON_CONFIG` precedent, which is likewise honoured in release builds and is
absent from the seam list that `test_seams_enabled` refuses.

### D-8: Build-version skew is a new, distinct check from protocol skew (BR-6, AC-7)

`teton-protocol` already has `VersionSkew` / `VersionMismatch`, but they classify
**protocol range disjointness** and produce a hard *rejection*. The harm REQ-565
cites — a v0.1.12 daemon still serving four hours after v0.1.13 was installed —
is precisely the case where protocol ranges still overlap, negotiation succeeds,
and nothing is said. README lines 68–71 currently claim "nothing is silently
wrong" about exactly this case; that claim is false for same-protocol builds and
is corrected as part of AC-6.

So: a pure `build_skew(client_version, daemon_version) -> Option<BuildSkew>` in
`teton-protocol` (transport-free, knows nothing about brew), with the sentence
rendered at the CLI surface — mirroring the existing "the sentences themselves
deliberately live at the surfaces" split in `handshake.rs`. `HandshakeResult`
already carries `daemon_version`, so no protocol change is needed. Unit-testable
with injected versions, satisfying AC-7's "does not appear when they match".

### D-9: Ordered teardown, with the flock released last (BR-8)

After the accept loop breaks on the shutdown signal:

1. stop accepting (loop already exited)
2. await outstanding client tasks, bounded — their in-flight turns are what
   deferred the commit, so this is normally already drained
3. close open sessions; count them for the event payload
4. drop the ledger connection (rows already committed — D-3)
5. `unlink` the socket path
6. emit `daemon_shutdown{reason, uptime_seconds, sessions_closed}` and flush stderr
7. return from `block_on`; `_instance` drops → flock released **last**

## Data Model Changes

| Where | Change |
|---|---|
| `crates/teton-core/src/lifetime.rs` | **new** — `ShutdownPolicy`, `PolicySource`, `BlockingActivity`, `LifetimePhase`, `LifetimeState`, `LifetimeAction`, `Admission` |
| `crates/teton-core/src/config.rs` | **new** `LifetimeConfig` (`[lifetime]`), wired into `Config` following the `PrivacyConfig` placement pattern |
| `crates/teton-protocol/src/events.rs` | `DaemonLifetime` event family (connected / disconnected / armed / deferred / shutdown) |
| `crates/teton-protocol/src/jsonrpc.rs` | `error_code::DAEMON_SHUTTING_DOWN` |
| `crates/teton-protocol/src/handshake.rs` | `BuildSkew` + `build_skew()` |
| `crates/tetond/src/lifetime.rs` | **new** — `LifetimeSupervisor`, `ClientGuard`, `ActivityGuard` |

The spec's Events table names five events; they are realized as one
`DaemonLifetime` variant family carrying a `stage`, following the D-8 fold
REQ-563 established for the web-lookup vocabulary, so the enum does not grow
five near-identical variants.

## AC → Decision Map

| AC | Decisions | Verified by |
|---|---|---|
| AC-1 autostart → exit on CLI exit | D-1, D-2, D-9 | e2e |
| AC-2 two clients, second exit stops it | D-1, D-3 | e2e |
| AC-3 in-flight turn defers, ledger intact | D-3, D-4 | e2e |
| AC-4 connect-vs-shutdown race | D-5, D-6 | e2e + race test |
| AC-5 packaging: no keep_alive, smoke re-pointed | D-7 | release workflow |
| AC-6 caveats + docs for the upgrade path | D-8 | docs review |
| AC-7 build-version skew warning | D-8 | unit (injected versions) |
| AC-8 `never` + `linger` modes | D-7 | unit + e2e |
| AC-9 pure state machine coverage | D-2, D-3, D-5 | unit, no socket |

## Open Question Resolved

**OQ-3 — answered: no.** No background duty needs the daemon alive with zero
clients. An enumeration of every `tokio::spawn` / `spawn_blocking` in
`crates/tetond/src` finds no periodic timer, no `interval()`, and no compaction
or catalog-refresh task; every spawn is request-scoped (per-client reader,
writer, event forwarder, prompt turn) or belongs to the startup consent flow
(`main.rs:91`), which is exactly what D-3's `ModelDownload`/`ModelLoad`
deferral covers. Nothing needs to outlive the last client.

## Risks / Notes for Implementation

- **Every daemon start re-pays the deep model verify (out of scope here, but
  amplified by this REQ).** `ModelConsentGate::resolve` re-digests the installed
  weights on every start — its own comment says "`resolve` runs once at startup,
  so this pays the hash at most once per boot" (`model_consent.rs:1017`). Under
  on-demand lifetime, *per boot* becomes *per CLI invocation*: a multi-GB
  SHA-256 read on `teton doctor`, and — because D-3 makes `ModelLoad` a
  deferring activity per BR-2 — the daemon cannot exit until it finishes. The
  spec's accepted tradeoff ("the next CLI start pays the model load") covers a
  session start; it did not consider short non-session commands. This is
  implemented **as BR-2 specifies** rather than silently redesigned, because the
  deep verify is a security control (ADR-006 / M-10: the tier must not open on a
  forgeable receipt) and relaxing it belongs in its own REQ with its own review.
  **Recommended follow-up: lazy local-tier initialization** — defer the startup
  verify/load until a turn actually needs the local tier.
- The e2e harness needs two additions: `DaemonOptions::arg()` (it can only set
  env today) and `Daemon::wait_for_exit(timeout)`.
- `serve()` becomes `select!` over `accept()` and the shutdown signal; every
  existing caller of `server::serve` must pass the supervisor.
- Do not let the startup grace fire during a slow first-run consent flow: the
  socket is bound *before* the consent task is spawned (`main.rs:82` then `:91`),
  so the grace is measured from bind and the CLI connects within its 5 s poll —
  but an operator starting `teton-code` by hand and walking away will see the
  60 s expiry, which is intended.

## Task Graph (TASK-085..091)

```
085 (core state machine + config) ─┬─→ 087 (supervisor + server) ─┬─→ 088 (main teardown + flock + policy) ─→ 090 (packaging + docs)
086 (protocol vocabulary) ─────────┘                              │                                            
086 ────────────────────────────────→ 089 (CLI refusal + skew)    │
087, 088, 089 ────────────────────────────────────────────────────┴─→ 091 (acceptance suite)
```

TASK ids start at 085 rather than 079 deliberately: REQ-564 is in flight
concurrently from the same base commit and would otherwise claim the same block.
079–084 are left to it.
