---
id: BUG-169
title: "A model-loaded llama daemon aborts (SIGABRT, exit 134) on every process exit"
status: resolved
severity: high
created: 2026-08-14
updated: 2026-08-14
component: "daemon/lifecycle"
domain: "harness"
stack: ["rust", "daemon", "llama.cpp", "metal", "macos"]
concerns: ["reliability", "user-experience"]
tags: ["req-565", "shutdown", "sigterm", "exit-code", "ggml", "static-destructor", "cxa-finalize", "crash-report"]
---

## Description

> **ID note:** filed and merged as BUG-166 (PR #136) in a parallel session,
> colliding with BUG-166-a-rejection-notice-can-be-spent-on-nobody (PR #130,
> which landed first and keeps the number). Renumbered to BUG-169 — commits
> referencing `BUG-166` for the daemon-abort fix mean this record.

A `--features tetond/llama` daemon that has loaded the local model cannot exit
cleanly. Every exit path that leaves `main` normally — SIGTERM/SIGINT (the
`brew services stop` path, BR-5) and the REQ-565 exit-with-last-client policy —
completes the ordered teardown (BR-8) and then dies with SIGABRT (exit 134)
inside libc `exit()`. A daemon that never loaded the model exits 0 on the same
path.

REQ-565 made process exit a *routine* lifecycle event: the shipped Homebrew
daemon (a llama build) exits every time its last client leaves. Each such exit
writes a macOS crash report to `~/Library/Logs/DiagnosticReports/`, so normal
idle behavior litters the system with crash reports (18 accumulated on the
dogfooding machine at filing time, several per hour while sessions cycle), and
launchd/`brew services` sees a crashing service rather than a clean stop.

## Reproduction Steps

1. `cargo build --release --workspace --features tetond/llama`.
2. Isolate a daemon: `XDG_RUNTIME_DIR=/tmp/tetcrash`, symlink the verified
   weights to `$XDG_RUNTIME_DIR/teton/models`, copy `model-selection.toml`
   across, start `teton-code --shutdown-policy never`.
3. Run one piped CLI session so the daemon loads the model (RSS climbs to
   ~18 GiB).
4. `kill -TERM <pid>`.
5. The daemon logs its ordered `daemon_shutdown (reason="signal", …)` line —
   the Rust teardown is *complete* — then exits 134 and a
   `teton-code-*.ips` crash report appears.

Without step 3 (no model ever loaded), the same SIGTERM exits 0.

## Expected Behavior

Exit status 0 on every ordered shutdown, no crash report, regardless of
whether the model is resident.

## Actual Behavior

```
teton-code: daemon_shutdown (reason="signal", uptime_seconds=394, sessions_closed=2)
…/llama.cpp/ggml/src/ggml-metal/ggml-metal-device.m:622: GGML_ASSERT([rsets->data count] == 0) failed
5   libsystem_c.dylib   __cxa_finalize_ranges + 416
6   libsystem_c.dylib   exit + 44
```

Crash report: `EXC_CRASH (SIGABRT)`, faulting thread in
`abort ← ggml_metal_rsets_free ← ggml_metal_device_free ← __cxa_finalize_ranges ← exit`.

## Environment

- Platform: macOS (Apple Silicon, M-series), Darwin 25.6
- Version: workspace 0.1.14, `llama-cpp-2` =0.1.151
- Affected: every llama-build daemon exit after a model load, local and shipped

## Root Cause

Three facts compose:

1. **ggml's Metal device registry is a C++ function-local static.**
   `ggml_metal_device_get` holds `static std::vector<ggml_metal_device_ptr>`,
   whose destructor is registered via `__cxa_atexit` and runs inside libc
   `exit()`. The deleter calls `ggml_metal_device_free` →
   `ggml_metal_rsets_free`, which hard-asserts every Metal residency set was
   already released: `GGML_ASSERT([rsets->data count] == 0)` — annotated
   upstream "if you hit this assert, most likely you haven't deallocated all
   Metal resources before exiting".
2. **The model's Metal buffers are still resident when `main` returns.** The
   engine lives in `Arc<Mutex<dyn Engine>>` handles cloned into spawned tasks;
   the shared `LlamaBackend` is deliberately a leaked `static OnceLock`
   (`shared_backend`, engine.rs). Whatever the exact holder, the weights'
   residency sets are non-empty at `exit()` — and any single surviving `Arc`
   is sufficient, so "drop everything first" is not an invariant the daemon
   can promise across future changes.
3. **Returning from `main` runs `exit()`.** Rust's runtime calls libc
   `exit(code)` after `main` returns, which runs `__cxa_finalize_ranges`, which
   runs destructor (1) against state (2) → `abort()`. `std::process::exit`
   makes no difference: it calls the same libc `exit()` and runs the same
   handlers. Only `_exit(2)` skips them.

## Resolution

`main` now ends in `libc::_exit(code)` after the ordered teardown, so C++
static destructors never run mid-teardown (`crates/tetond/src/main.rs`,
BUG-169 comment block). Nothing the skip discards is load-bearing:

- the cost ledger is SQLite in autocommit — durable per `record` (BR-8);
- `shutdown()` already unlinked the socket, in-order, before this point;
- the daemon reports on stderr, which Rust does not buffer;
- `_instance`'s `flock` releases at process death — the same point, relative
  to the socket unlink, that BR-3 got from the guard's drop at the end of
  `main`, so the successor-handoff ordering is unchanged.

The error arm prints the same `Error: {err:?}` stderr tail a `?`-return
produced, because the CLI surfaces that tail when an autostart fails (E-4);
early paths (`--version`, policy refusal, already-running) keep their normal
returns — they never construct the runtime, so no ggml static can exist there.

Fixing it by "drop the engine before exit" was rejected: it would work today
and silently regress the first time any task, cache, or future feature keeps
an engine `Arc` alive at teardown — reintroducing a crash-on-every-idle-exit
that nothing in CI can see (the abort needs a real model load). `_exit` makes
clean exit a property of the exit path itself rather than of every reference
ever taken to the engine.

## Verification

Same isolated-daemon procedure as Reproduction, A/B on this change, model
resident (~18 GiB RSS) in both:

| trigger | pre-fix | post-fix |
|---|---|---|
| SIGTERM (`never` policy) | exit 134 + `.ips` crash report | **exit 0**, socket unlinked, no new crash report |
| last client leaves (default policy) | exit 134 (observed in the wild every idle cycle) | **exit 0**, `reason="last_client"` |

- `cargo test --workspace --no-fail-fast`: all green (see PR).
- New e2e: `a_sigterm_runs_the_ordered_teardown_and_exits_zero`
  (`crates/tetond/tests/e2e/daemon_lifetime.rs`) — pins signal → ordered
  teardown → status 0 → socket unlinked. The mock build cannot reproduce the
  ggml abort itself (that needs a real model load); the test pins the contract
  the fix preserves, and the reproduction above is the manual proof for the
  llama build.
- `cargo clippy --workspace --all-targets`: clean.

## Files Changed

- `crates/tetond/src/main.rs` — `main` ends in `libc::_exit`; the why is the
  BUG-169 comment block; `shutdown()`'s lock-ordering doc updated.
- `crates/tetond/tests/e2e/daemon_lifetime.rs` — new SIGTERM teardown e2e.
- `crates/tetond/Cargo.toml` — `libc` added to dev-dependencies (the test
  delivers a real `kill(2)`).
