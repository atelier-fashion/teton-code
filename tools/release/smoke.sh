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
#   2. `teton-code --version` reports the released version.
#   3. `TETON_TEST_SEAMS=1 ./teton-code` REFUSES to start (BR-9): non-zero exit AND
#      the refusal text. A release build must never honour the acceptance
#      suite's injection seams, and it must say so rather than starting
#      silently.
#   4. A backgrounded `teton-code` and a real `teton doctor` complete a handshake:
#      doctor's OUTPUT names the running daemon and its version.
#
# Before those four, on the macOS targets only, each shipped binary must carry a
# real Developer ID signature (BR-2). The question is put to
# `verify-signature.sh` and to nothing else — a codesign invocation of this
# script's own would be a second opinion about what a release signature is, free
# to drift from the gate the release job runs — and `TETON_SMOKE_TEAM_ID` names
# the team it must find. Which target this is comes off the tarball's own name,
# because a path is all this script is given. The Linux target ships unsigned in
# v1 by design (BR-6, ADR-550-4), and that is PRINTED rather than silently
# skipped: a gate that says nothing looks exactly like a gate that did not run.
#
# THE TARGET IS READ FROM AN ALLOWLIST, not a pattern (LESSON-443). This used to
# be `*apple-darwin*` and a catch-all `*` that printed the Linux line, so any
# name that was not recognisably darwin — a target added later, a tarball
# renamed by hand, a `${target}` that expanded to nothing in a workflow, a typo
# in the release matrix — was announced as "unsigned in v1 (linux)" and shipped
# with no signature check at all. The catch-all was a fail-open: the one input
# that most deserved a hard stop got the softest possible answer, and it got it
# in the release job's log where it reads as normal. The three triples the
# release actually builds are now spelled out, and ANY other name is 75
# UNCHECKED. Not a skip, and never the Linux line.
#
# `TETON_SMOKE_ASSUME_TARGET` is the seam that keeps selftest.sh's stand-in
# tarballs — named `teton-v1.2.3-good.tar.gz` and the like, because what they
# vary is behaviour and not platform — drivable against the allowlist. It is
# safe by construction rather than by discipline: it is consulted ONLY when the
# tarball's own name yields no recognised triple, so it cannot re-classify any
# artifact package.sh produced, which is every artifact in a real release. Its
# value must itself be one of the three triples, and inside GitHub Actions it is
# refused unless `TETON_ALLOW_TOOL_SEAM=1` says a harness meant it.
#
# Two shapes of assertion deserve their reasons recorded, because both are easy
# to "simplify" into something that passes when it should not:
#
#   * The version checks grep for the bare version string. The two binaries
#     print different shapes — clap's `teton X.Y.Z` versus the daemon's
#     hand-rolled `teton-code X.Y.Z` — and pinning either exact line here would make
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
#   64  EX_USAGE   wrong invocation — including a macOS tarball handed over with
#                  no TETON_SMOKE_TEAM_ID to check its signature against.
#   65  FAILED     the smoke RAN and an assertion FAILED — these bytes are bad.
#   75  UNCHECKED  the smoke could NOT run (no tarball, extraction failed, a
#                  binary is missing, the signature could not be read). Nothing
#                  was learned about the artifact, so it fails rather than
#                  passing an unexercised release.

set -euo pipefail

EXIT_USAGE=64
EXIT_FAILED=65
EXIT_UNCHECKED=75

script_dir="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)"

# `source=` lets `shellcheck -x` follow this; `disable=SC1091` keeps a bare
# `shellcheck <file>` — which cannot follow it — from failing on the info.
# shellcheck source=tools/release/lib.sh disable=SC1091
. "$script_dir/lib.sh"

# How long the seam refusal gets to happen, and how long the daemon gets to come
# up. The refusal is a panic during startup, so it is immediate; the daemon runs
# a hardware probe first, so it gets longer.
#
# Both are overridable so selftest.sh can exercise the two timeout paths — a
# daemon that does not refuse the seams, and one that never handshakes — without
# spending a minute per case. The override direction is safe by construction:
# shortening a deadline can only make an assertion give up sooner, i.e. FAIL
# earlier. There is no value of either that turns a failing artifact into a
# passing one, which is why this knob is allowed to exist on a release gate at
# all. Leave them unset in CI and the defaults apply.
SEAM_DEADLINE_SECS="${TETON_SMOKE_SEAM_DEADLINE_SECS:-20}"
HANDSHAKE_DEADLINE_SECS="${TETON_SMOKE_HANDSHAKE_DEADLINE_SECS:-45}"

for deadline in "$SEAM_DEADLINE_SECS" "$HANDSHAKE_DEADLINE_SECS"; do
    if ! printf '%s' "$deadline" | grep -Eq '^[1-9][0-9]*$'; then
        echo "smoke: deadline overrides must be a positive whole number of seconds; got '$deadline'." >&2
        exit "$EXIT_USAGE"
    fi
done

if [ "$#" -lt 2 ]; then
    echo "usage: smoke.sh <tarball> <version>" >&2
    exit "$EXIT_USAGE"
fi

tarball="$1"
version="${2#v}"

# Every assertion below is `grep -qF -- "$version"`, and `grep -qF -- ""` matches
# any input at all — so an empty version made all four assertions pass against
# any pair of binaries, including binaries reporting a completely different
# version. That is worse than no gate: it is a gate that reports PASS. The
# version must therefore be a release version before it is allowed to be a
# needle.
if ! is_release_version "$version"; then
    echo "smoke: <version> must be a release version X.Y.Z (a leading 'v' is fine); got '${2}'." >&2
    echo "       An empty or malformed version would be greped for literally, and an empty" >&2
    echo "       needle matches everything — every assertion would pass without checking." >&2
    exit "$EXIT_USAGE"
fi

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

for bin in teton teton-code; do
    if [ ! -x "$extract/$bin" ]; then
        echo "smoke: $bin is missing from the tarball (or is not executable) — nothing was exercised." >&2
        exit "$EXIT_UNCHECKED"
    fi
done

# Architecture evidence in the log. On the cross-compiled leg this is the line
# that shows an x86_64 binary really did run on the arm64 runner (ADR-548-2).
if command -v file >/dev/null 2>&1; then
    echo "smoke: $(file -b "$extract/teton-code")"
fi

# --- 0: the shipped macOS binaries are Developer ID signed (BR-2) ----------
# Numbered 0 because it is a property of the bytes rather than of the program
# they contain, and because it decides whether the four below are worth running
# at all — an unsigned macOS release is unshippable however well it behaves.
#
# The target triple is read off the tarball's name, the shape package.sh writes:
# `teton-v<version>-<target>.tar.gz`, and it must be one of the three the
# release builds. See the header for why this is an allowlist and what the
# catch-all it replaced did.
tarball_target="$(basename "$tarball")"
tarball_target="${tarball_target%.tar.gz}"
name_prefix="teton-v$version-"
tarball_target="${tarball_target#"$name_prefix"}"

# Did the NAME answer? `case`, not `grep -Eq`, for the reason verify-signature.sh
# records at its own needle tests: a grep that is missing, broken or killed exits
# non-zero, and `if ! grep` reads that as "the name is not a release target" — so
# a broken tool would decide the classification. A `case` is the shell itself and
# has no exit status to misread.
name_is_release_target=no
case "$tarball_target" in
    aarch64-apple-darwin | x86_64-apple-darwin | x86_64-unknown-linux-gnu)
        name_is_release_target=yes
        ;;
esac

# The seam, applied ONLY where the name failed to answer. Ordered this way on
# purpose: a tarball called `teton-v1.2.3-aarch64-apple-darwin.tar.gz` is a
# macOS artifact no matter what the environment claims, so no value of this
# variable can move a real release artifact off the signature gate.
if [ "$name_is_release_target" = no ] && [ -n "${TETON_SMOKE_ASSUME_TARGET:-}" ]; then
    if [ "${GITHUB_ACTIONS:-}" = "true" ] && [ "${TETON_ALLOW_TOOL_SEAM:-}" != "1" ]; then
        echo "smoke: TETON_SMOKE_ASSUME_TARGET is set inside GitHub Actions and TETON_ALLOW_TOOL_SEAM=1" >&2
        echo "       is not — refusing to run. Nothing was exercised." >&2
        exit "$EXIT_USAGE"
    fi
    case "${TETON_SMOKE_ASSUME_TARGET}" in
        aarch64-apple-darwin | x86_64-apple-darwin | x86_64-unknown-linux-gnu) ;;
        *)
            echo "smoke: TETON_SMOKE_ASSUME_TARGET must itself be one of the three release targets;" >&2
            echo "       got '${TETON_SMOKE_ASSUME_TARGET}'. Nothing was exercised." >&2
            exit "$EXIT_USAGE"
            ;;
    esac
    echo "smoke: '$tarball_target' is not a release target; TETON_SMOKE_ASSUME_TARGET says to treat"
    echo "       this artifact as ${TETON_SMOKE_ASSUME_TARGET}. This is a TEST seam — a real release"
    echo "       tarball is named for its target and never reaches this branch."
    tarball_target="${TETON_SMOKE_ASSUME_TARGET}"
fi

case "$tarball_target" in
    aarch64-apple-darwin | x86_64-apple-darwin)
        # A missing team id is a USAGE error, never a skip. Reading "then don't
        # check" out of an absent argument is how a gate disables itself on
        # exactly the machine that most needed it (LESSON-443), and an empty
        # team id is worse still — verify-signature.sh matches it as a literal
        # substring, and every string contains the empty one.
        if [ -z "${TETON_SMOKE_TEAM_ID:-}" ]; then
            echo "smoke: $tarball_target is a macOS target, so both binaries must be checked for a" >&2
            echo "       Developer ID signature — and TETON_SMOKE_TEAM_ID, which names the team" >&2
            echo "       they must be signed by, is unset or empty. Nothing was exercised." >&2
            exit "$EXIT_USAGE"
        fi

        # Per binary, and on the extracted copies: these are the bytes the four
        # assertions below run, so they are the bytes whose signature is worth
        # a verdict (LESSON-455 — both binaries, no per-file drift). Invoked
        # through `bash` like every other call to these scripts in CI, so a lost
        # exec bit cannot turn a gate into a 126.
        sig_unchecked=0
        for bin in teton teton-code; do
            sig_status=0
            bash "$script_dir/verify-signature.sh" "$extract/$bin" "$TETON_SMOKE_TEAM_ID" ||
                sig_status=$?

            case "$sig_status" in
                0) pass "$bin is Developer ID signed by team $TETON_SMOKE_TEAM_ID" ;;
                "$EXIT_FAILED")
                    fail "$bin in this tarball is not Developer ID signed by team $TETON_SMOKE_TEAM_ID (BR-2)"
                    ;;
                *)
                    # 75 and everything else it could exit with: no codesign, an
                    # identity that could not be read, a usage error of our own
                    # making. The signature was not READ, and "could not check"
                    # is not a pass.
                    echo "smoke: $bin's signature could not be checked (verify-signature.sh exit $sig_status)." >&2
                    sig_unchecked=1
                    ;;
            esac
        done

        # A definite rejection outranks an unreadable one, the same ordering
        # verify-signature.sh applies internally: if one binary was REJECTED,
        # something was learned about these bytes, and losing that to a 75
        # because its sibling was unreadable would make the log claim less than
        # the run knows. Either way the release is blocked.
        if [ "$sig_unchecked" -ne 0 ] && [ "$failures" -eq 0 ]; then
            echo "smoke: no signature verdict was reached for $(basename "$tarball") — nothing was learned." >&2
            exit "$EXIT_UNCHECKED"
        fi
        ;;
    x86_64-unknown-linux-gnu)
        # Printed, not skipped: a gate that says nothing looks exactly like a
        # gate that did not run, and shipping Linux unsigned in v1 is a decision
        # rather than an omission. Reached only by the ONE name that earns it.
        echo "smoke: artifact is unsigned in v1 (linux — by design, REQ-550 BR-6)"
        ;;
    *)
        # The closed fail-open. An unrecognised target means this script does
        # not know whether these bytes were supposed to be signed, and "I do not
        # know" is 75 — never the Linux line, which would be a claim, and never
        # a silent skip, which would be the same claim with the evidence
        # removed. A fourth release target arriving here is a real possibility;
        # it should stop the release and be added deliberately, in this case and
        # in package.sh's matching allowlist, rather than inherit Linux's
        # unsigned exemption by accident (LESSON-443).
        echo "smoke: '$tarball_target' is not one of this release's targets (aarch64-apple-darwin," >&2
        echo "       x86_64-apple-darwin, x86_64-unknown-linux-gnu), so whether these bytes must" >&2
        echo "       carry a Developer ID signature is unknown — and an unknown signature" >&2
        echo "       requirement is not a satisfied one. Nothing was exercised." >&2
        exit "$EXIT_UNCHECKED"
        ;;
esac

# --- 1 + 2: both binaries report the released version ----------------------
for bin in teton teton-code; do
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
# Marker file: written by the watchdog iff it had to kill the daemon.
seam_killed="$work/seam.killed"

TETON_TEST_SEAMS=1 \
    XDG_RUNTIME_DIR="$seam_runtime" \
    HOME="$work" \
    TETON_REPO_ROOT="$work" \
    "$extract/teton-code" >"$seam_out" 2>&1 &
seam_pid=$!

# A watchdog, because the failure mode being guarded against is a daemon that
# does NOT refuse: that daemon runs in the foreground forever, and without this
# the smoke would hang instead of reporting the failure it just found.
#
# Its output is redirected to /dev/null, and that redirect is load-bearing. The
# `kill` below signals this subshell, not the `sleep` it is blocked on: with job
# control off, a background job shares the script's process group, so the signal
# reaches the subshell alone and the `sleep` is orphaned and runs out its full
# deadline. An orphan is harmless — except that it inherited the caller's stdout,
# and a pipe stays open while any writer holds it. Undetached, this made a smoke
# whose four assertions all finished instantly take SEAM_DEADLINE_SECS to be
# observed as finished, once per build leg, by anything reading its output —
# every CI step, every `$(...)`.
(
    sleep "$SEAM_DEADLINE_SECS"
    # Only claim a kill if there is something to kill. The marker's meaning is
    # "this daemon was STILL RUNNING at the deadline", and the watchdog is not
    # always cancelled before it wakes: a daemon that refused instantly leaves
    # the main script racing to `wait`, report, and kill the watchdog, and on a
    # loaded runner the watchdog can wake first. Writing the marker
    # unconditionally made that race a FAIL against a daemon that had already
    # done exactly what BR-9 asks of it — a flaky red on a good release.
    #
    # The residual window (alive at `kill -0`, gone a microsecond later) is
    # one-directional and it is the safe direction: it can only turn a pass into
    # a failure, never a failure into a pass.
    if kill -0 "$seam_pid" 2>/dev/null; then
        # Marker BEFORE the kill: the kill is what makes $seam_status non-zero,
        # so without it a daemon that printed the refusal and then kept running
        # with the seams honoured — a BR-9 violation, the exact thing this
        # assertion exists to catch — would be scored as a refusal.
        : >"$seam_killed"
        kill -9 "$seam_pid" 2>/dev/null || true
    fi
) >/dev/null 2>&1 &
seam_watchdog=$!

seam_status=0
wait "$seam_pid" || seam_status=$?
kill "$seam_watchdog" 2>/dev/null || true
wait "$seam_watchdog" 2>/dev/null || true

if [ -e "$seam_killed" ]; then
    fail "TETON_TEST_SEAMS=1 teton-code was still running after ${SEAM_DEADLINE_SECS}s and had to be killed — it did not refuse, whatever it printed. Output:"
    sed 's/^/  | /' "$seam_out" >&2 || true
elif [ "$seam_status" -ne 0 ] && grep -qF "TETON_TEST_SEAMS=1 is set" "$seam_out"; then
    pass "TETON_TEST_SEAMS=1 teton-code refused to start (exit $seam_status) with the release-build refusal"
else
    fail "TETON_TEST_SEAMS=1 teton-code did not refuse as a release build should (exit $seam_status). Output:"
    sed 's/^/       | /' "$seam_out" >&2 || true
fi

# --- 4: a real handshake, asserted on doctor's text ------------------------
run_dir="$work/run"
mkdir -p "$run_dir"
daemon_log="$work/teton-code.log"

XDG_RUNTIME_DIR="$run_dir" \
    HOME="$work" \
    TETON_REPO_ROOT="$work" \
    "$extract/teton-code" >"$daemon_log" 2>&1 &
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
    pass "teton doctor handshook a live teton-code — $daemon_line"
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
