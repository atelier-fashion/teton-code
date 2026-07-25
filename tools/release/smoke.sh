#!/usr/bin/env bash
#
# Smoke-test one release tarball — the gate that decides whether these exact
# bytes are fit to be published (BR-7/BR-9).
#
# Usage: tools/release/smoke.sh <tarball> <version>
#
#   <tarball>  a `teton-vX.Y.Z-<target>.tar.gz` produced by package.sh.
#   <version>  the release version, with or without a leading `v`.
#
# It unpacks the archive and exercises the shipped binaries — not the build
# tree, not `cargo run` — because the thing being released is the tarball:
#
#   1. `teton --version` reports the released version.
#   2. `tetond --version` reports the released version.
#   3. `TETON_TEST_SEAMS=1 ./tetond` REFUSES to start (BR-9): non-zero exit AND
#      the refusal text. A release build must never honour the acceptance
#      suite's injection seams, and it must say so rather than starting
#      silently.
#   4. A backgrounded `tetond` and a real `teton doctor` complete a handshake:
#      doctor's OUTPUT names the running daemon and its version.
#
# Two shapes of assertion deserve their reasons recorded, because both are easy
# to "simplify" into something that passes when it should not:
#
#   * The version checks grep for the bare version string. The two binaries
#     print different shapes — clap's `teton X.Y.Z` versus the daemon's
#     hand-rolled `tetond X.Y.Z` — and pinning either exact line here would make
#     this gate a test of the formatting rather than of the version.
#   * The handshake asserts on doctor's TEXT, never its exit code. `teton
#     doctor` deliberately exits 0 when the daemon is unreachable — it is a
#     diagnostic, not a probe — so an exit-code assertion here would pass
#     against a daemon that never started, which is precisely the failure this
#     leg exists to catch.
#
# Exit codes are a taxonomy, matching the version gate's stance (LESSON-442):
#
#   0   PASS       every assertion held against the shipped artifact.
#   64  EX_USAGE   wrong invocation.
#   65  FAILED     the smoke RAN and an assertion FAILED — these bytes are bad.
#   75  UNCHECKED  the smoke could NOT run (no tarball, extraction failed, a
#                  binary is missing). Nothing was learned about the artifact,
#                  so it fails rather than passing an unexercised release.

set -euo pipefail

EXIT_USAGE=64
EXIT_FAILED=65
EXIT_UNCHECKED=75

# How long the seam refusal gets to happen, and how long the daemon gets to come
# up. The refusal is a panic during startup, so it is immediate; the daemon runs
# a hardware probe first, so it gets longer.
SEAM_DEADLINE_SECS=20
HANDSHAKE_DEADLINE_SECS=45

if [ "$#" -lt 2 ]; then
    echo "usage: smoke.sh <tarball> <version>" >&2
    exit "$EXIT_USAGE"
fi

tarball="$1"
version="${2#v}"

if [ ! -f "$tarball" ]; then
    echo "smoke: tarball not found: $tarball — nothing was exercised." >&2
    exit "$EXIT_UNCHECKED"
fi

# A SHORT scratch root, deliberately. The daemon's socket lives under
# `$XDG_RUNTIME_DIR/teton/`, and a Unix-domain socket path is capped near 104
# bytes (SUN_LEN); macOS's default TMPDIR spends about half of that budget
# before we add anything, so a bare `mktemp -d` can push the socket over the
# limit and produce a bind failure that looks nothing like a path-length
# problem. `/tmp` keeps the whole path near 45 characters.
work="$(mktemp -d /tmp/teton-smoke.XXXXXX)"

daemon_pid=""
cleanup() {
    if [ -n "$daemon_pid" ] && kill -0 "$daemon_pid" 2>/dev/null; then
        kill "$daemon_pid" 2>/dev/null || true
        wait "$daemon_pid" 2>/dev/null || true
    fi
    rm -rf "$work"
}
trap cleanup EXIT

failures=0
pass() { echo "smoke: PASS — $*"; }
fail() {
    echo "smoke: FAIL — $*" >&2
    failures=$((failures + 1))
}

echo "smoke: $(basename "$tarball") — expecting version $version"

extract="$work/extract"
mkdir -p "$extract"
if ! tar -xzf "$tarball" -C "$extract"; then
    echo "smoke: could not extract $tarball — nothing was exercised." >&2
    exit "$EXIT_UNCHECKED"
fi

for bin in teton tetond; do
    if [ ! -x "$extract/$bin" ]; then
        echo "smoke: $bin is missing from the tarball (or is not executable) — nothing was exercised." >&2
        exit "$EXIT_UNCHECKED"
    fi
done

# Architecture evidence in the log. On the cross-compiled leg this is the line
# that shows an x86_64 binary really did run on the arm64 runner (ADR-548-2).
if command -v file >/dev/null 2>&1; then
    echo "smoke: $(file -b "$extract/tetond")"
fi

# --- 1 + 2: both binaries report the released version ----------------------
for bin in teton tetond; do
    out="$("$extract/$bin" --version 2>&1 || true)"
    if printf '%s' "$out" | grep -qF -- "$version"; then
        pass "$bin --version reports $version ($out)"
    else
        fail "$bin --version does not report $version — got: $out"
    fi
done

# --- 3: the shipped daemon refuses the test seams (BR-9) -------------------
# The refusal is a panic inside `DaemonRuntime::from_env`, so it fires during
# startup and needs no daemon lifecycle to observe. Both halves are asserted: a
# non-zero exit ALONE would also be satisfied by a daemon that died for an
# unrelated reason (a busy socket, a bad config), which would leave this gate
# green while proving nothing about the seams.
seam_runtime="$work/seam"
mkdir -p "$seam_runtime"
seam_out="$work/seam.out"

TETON_TEST_SEAMS=1 \
    XDG_RUNTIME_DIR="$seam_runtime" \
    HOME="$work" \
    TETON_REPO_ROOT="$work" \
    "$extract/tetond" >"$seam_out" 2>&1 &
seam_pid=$!

# A watchdog, because the failure mode being guarded against is a daemon that
# does NOT refuse: that daemon runs in the foreground forever, and without this
# the smoke would hang instead of reporting the failure it just found.
(
    sleep "$SEAM_DEADLINE_SECS"
    kill -9 "$seam_pid" 2>/dev/null || true
) &
seam_watchdog=$!

seam_status=0
wait "$seam_pid" || seam_status=$?
kill "$seam_watchdog" 2>/dev/null || true
wait "$seam_watchdog" 2>/dev/null || true

if [ "$seam_status" -ne 0 ] && grep -qF "TETON_TEST_SEAMS=1 is set" "$seam_out"; then
    pass "TETON_TEST_SEAMS=1 tetond refused to start (exit $seam_status) with the release-build refusal"
else
    fail "TETON_TEST_SEAMS=1 tetond did not refuse as a release build should (exit $seam_status). Output:"
    sed 's/^/       | /' "$seam_out" >&2 || true
fi

# --- 4: a real handshake, asserted on doctor's text ------------------------
run_dir="$work/run"
mkdir -p "$run_dir"
daemon_log="$work/tetond.log"

XDG_RUNTIME_DIR="$run_dir" \
    HOME="$work" \
    TETON_REPO_ROOT="$work" \
    "$extract/tetond" >"$daemon_log" 2>&1 &
daemon_pid=$!

# Poll `doctor` itself rather than waiting for the socket file to appear: bind()
# creates that file before listen() accepts anything, so an existence check has
# a window in which it is true and a connect still fails.
doctor_out="$work/doctor.out"
: >"$doctor_out"
deadline=$(($(date +%s) + HANDSHAKE_DEADLINE_SECS))
while :; do
    XDG_RUNTIME_DIR="$run_dir" HOME="$work" \
        "$extract/teton" doctor </dev/null >"$doctor_out" 2>&1 || true
    if grep -qF "daemon: running" "$doctor_out"; then
        break
    fi
    # If the daemon is already gone there is nothing left to wait for; stop and
    # let the assertion below report it with the daemon's own log.
    kill -0 "$daemon_pid" 2>/dev/null || break
    [ "$(date +%s)" -lt "$deadline" ] || break
    sleep 0.5
done

daemon_line="$(grep -F "daemon: running" "$doctor_out" || true)"
if [ -n "$daemon_line" ] && printf '%s' "$daemon_line" | grep -qF -- "$version"; then
    pass "teton doctor handshook a live tetond — $daemon_line"
else
    fail "teton doctor did not report a running daemon at version $version. doctor said:"
    sed 's/^/       | /' "$doctor_out" >&2 || true
    echo "       daemon log:" >&2
    sed 's/^/       | /' "$daemon_log" >&2 || true
fi

if [ "$failures" -ne 0 ]; then
    echo "smoke: $failures assertion(s) FAILED for $(basename "$tarball")." >&2
    exit "$EXIT_FAILED"
fi

echo "smoke: all 4 assertions passed for $(basename "$tarball")."
