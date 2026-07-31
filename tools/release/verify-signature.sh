#!/usr/bin/env bash
#
# Verify that released macOS binaries carry a real Developer ID signature
# (BR-1/BR-2/BR-6).
#
# Usage: tools/release/verify-signature.sh <binary|tarball> <team-id>
#
#   <binary|tarball>  a single binary, or a `teton-vX.Y.Z-<target>.tar.gz`
#                     produced by package.sh — in which case BOTH `teton` and
#                     `teton-code` inside it are checked. A green check on one
#                     binary is not evidence about the other (BR-6).
#   <team-id>         the Apple Developer team id the signature must name
#                     (`APPLE_TEAM_ID`). Public, like the identity string —
#                     this script prints no secret or certificate material.
#
# WHERE THE REAL TOOL RUNS: the macOS release legs only. `codesign` is Apple's
# and exists nowhere else, so the Linux leg does not run this gate at all — it
# ships unsigned and says so (BR-6, ADR-550-4). Per LESSON-433 a verification
# claim is never extrapolated across platforms: each leg proves its own
# artifact, and a leg that cannot check says UNCHECKED rather than inheriting
# another leg's green.
#
# `TETON_CODESIGN` overrides which tool is asked (default: `codesign`). It
# exists so selftest.sh can drive every branch below on the Linux `tooling` job
# with stand-ins. It changes WHO is asked, never how the answer is scored, so
# no value of it can turn a rejection into a pass.
#
# Two questions are asked of every binary, and both must hold:
#
#   1. `codesign --verify --strict` accepts it — the signature is structurally
#      valid and covers the bytes as shipped.
#   2. `codesign -dvv` names BOTH "Developer ID Application" and the team id.
#      This is not redundant with (1): an AD-HOC signature (`codesign -s -`)
#      passes (1) while naming no authority at all, and an ad-hoc binary
#      shipped as a release is precisely what BR-2 exists to catch. (1) alone
#      would call it signed.
#
# `-dvv`, and the second `v` is load-bearing. At verbosity 1 codesign prints
# `TeamIdentifier=` but NOT the `Authority=` chain, so `-dv` on a properly
# Developer ID signed binary contains no "Developer ID Application" anywhere —
# this gate would have rejected every correctly signed release it was given, in
# the one direction a release gate must never be wrong. Verified against Apple's
# codesign: `codesign -dv /bin/ls` yields zero Authority lines, `-dvv` yields
# three.
#
# Exit codes are a taxonomy, matching smoke.sh and verify-version.sh (BR-7,
# LESSON-442):
#
#   0   PASS       every binary checked is Developer ID signed by <team-id>.
#   64  EX_USAGE   wrong invocation (missing argument, empty team id).
#   65  FAILED     the check RAN and REJECTED — these bytes are not signed the
#                  way a release must be. Blocks the release loudly (BR-2).
#   75  UNCHECKED  the check could NOT run: no codesign, artifact missing,
#                  extraction failed, a binary is absent from the tarball, or
#                  `-dv` printed nothing to read. Nothing was learned, so it
#                  still fails the release — but it never claims the bytes are
#                  bad.
#
# 65 must stay unforgeable as "these bytes are bad" (LESSON-442), which is why
# `set -e` is deliberately absent below: under it an unhandled non-zero aborts
# with the FAILING COMMAND'S own status, and a tool that happened to exit 65
# would be indistinguishable from a rejection. Every status here is captured
# explicitly and the only exits are the four constants above.

set -uo pipefail

EXIT_USAGE=64
EXIT_FAILED=65
EXIT_UNCHECKED=75

script_dir="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)"

# Checked, because `set -e` is absent here: an unchecked source that failed
# would leave `tool_or_unchecked` undefined, every call to it exiting 127, and
# this script classifying an accident. `source=` lets `shellcheck -x` follow
# this; `disable=SC1091` keeps a bare `shellcheck <file>` — which cannot follow
# it — from failing on the info.
# shellcheck source=tools/release/lib.sh disable=SC1091
if ! . "$script_dir/lib.sh"; then
    echo "verify-signature: could not source $script_dir/lib.sh — nothing was checked." >&2
    exit "$EXIT_UNCHECKED"
fi

usage() {
    echo "usage: verify-signature.sh <binary|tarball> <team-id>" >&2
}

if [ "$#" -lt 2 ]; then
    echo "verify-signature: too few arguments — nothing was checked." >&2
    usage
    exit "$EXIT_USAGE"
fi

artifact="$1"
team_id="$2"

if [ -z "$artifact" ]; then
    echo "verify-signature: no artifact given — nothing was checked." >&2
    usage
    exit "$EXIT_USAGE"
fi

# The team id becomes a literal substring needle below, and EVERY string
# contains the empty one — an empty team id would make the identity assertion
# pass against any signature at all, ad-hoc included. That is worse than no
# gate: it is a gate that reports PASS. (smoke.sh carries the same reasoning for
# its version needle, where it cost exactly that.)
#
# Whitespace is refused for a smaller reason: no Apple team id contains any, so
# one that does is a mistake at the call site, and a mistake deserves a usage
# error rather than a report that the artifact is unsigned.
case "$team_id" in
    '')
        echo "verify-signature: no team id given — nothing was checked." >&2
        echo "                  An empty team id is matched as a literal substring, and every" >&2
        echo "                  string contains the empty one — the identity assertion would" >&2
        echo "                  pass against any signature at all, ad-hoc included." >&2
        usage
        exit "$EXIT_USAGE"
        ;;
    *[[:space:]]*)
        echo "verify-signature: <team-id> must not contain whitespace; got '$team_id'." >&2
        usage
        exit "$EXIT_USAGE"
        ;;
esac

codesign_tool=""
if ! codesign_tool="$(tool_or_unchecked "${TETON_CODESIGN:-}" codesign)"; then
    echo "verify-signature: '${TETON_CODESIGN:-codesign}' is not on this machine — nothing was checked." >&2
    echo "                  This gate belongs to the macOS release legs, where codesign is" >&2
    echo "                  native; elsewhere it must not be run and must not be believed." >&2
    exit "$EXIT_UNCHECKED"
fi

if [ ! -f "$artifact" ]; then
    echo "verify-signature: not found: $artifact — nothing was checked." >&2
    exit "$EXIT_UNCHECKED"
fi

work=""
cleanup() {
    if [ -n "$work" ]; then
        rm -rf "$work"
    fi
}
trap cleanup EXIT

# A tarball is checked as the pair of binaries it contains; anything else is
# taken as the one binary it names. Detection is by NAME, matching what
# package.sh writes — a file called `teton` is a binary even if it happens to
# be a gzip, and guessing from content would only add a second opinion.
targets=()
case "$artifact" in
    *.tar.gz | *.tgz)
        # `%/` because macOS exports a TMPDIR that already ends in a slash, and
        # the doubled separator would otherwise show up in every path this
        # script prints.
        tmp_root="${TMPDIR:-/tmp}"
        if ! work="$(mktemp -d "${tmp_root%/}/teton-verify-signature.XXXXXX")"; then
            work=""
            echo "verify-signature: could not create a scratch directory — nothing was checked." >&2
            exit "$EXIT_UNCHECKED"
        fi
        if ! tar -xzf "$artifact" -C "$work"; then
            echo "verify-signature: could not extract $artifact — nothing was checked." >&2
            exit "$EXIT_UNCHECKED"
        fi
        for bin in teton teton-code; do
            # `-f`, not `-x`: codesign reads the file, and whether the exec bit
            # survived packaging is smoke.sh's assertion, not this one's.
            if [ ! -f "$work/$bin" ]; then
                echo "verify-signature: $bin is missing from $(basename "$artifact") — nothing was checked." >&2
                exit "$EXIT_UNCHECKED"
            fi
            targets+=("$work/$bin")
        done
        ;;
    *)
        targets=("$artifact")
        ;;
esac

failures=0
unchecked=0
checked=""

pass() { echo "verify-signature: PASS — $*"; }
fail() {
    echo "verify-signature: FAIL — $*" >&2
    failures=$((failures + 1))
}
quote() { sed 's/^/       | /' >&2; }

echo "verify-signature: $(basename "$artifact") — expecting Developer ID Application, team $team_id"

for target in "${targets[@]}"; do
    name="$(basename "$target")"
    checked="${checked:+$checked, }$name"

    verify_status=0
    verify_out="$("$codesign_tool" --verify --strict "$target" 2>&1)" || verify_status=$?

    # 126 and 127 are the SHELL's "could not run it", not codesign's verdict:
    # the tool resolved a moment ago and has since vanished or lost its exec
    # bit. Scoring that as a rejection would be a 65 forged by an environment
    # change rather than by the bytes (LESSON-442).
    if [ "$verify_status" -eq 126 ] || [ "$verify_status" -eq 127 ]; then
        echo "verify-signature: '$codesign_tool' could not be run (exit $verify_status) — $name was not checked." >&2
        unchecked=1
        continue
    fi

    if [ "$verify_status" -ne 0 ]; then
        fail "$name: codesign --verify --strict rejected it (exit $verify_status)."
        printf '%s\n' "$verify_out" | quote
        continue
    fi

    # `-dvv` writes its identity dump to stderr, hence the 2>&1. What is scored
    # here is the OUTPUT, not the exit status: everything genuinely unsigned
    # already died at --verify above, so a `-dvv` that returns non-zero while
    # still describing the signature is a verdict worth reading, and one that
    # says nothing at all is a broken tool rather than bad bytes.
    display_status=0
    display_out="$("$codesign_tool" -dvv "$target" 2>&1)" || display_status=$?

    if [ -z "$display_out" ]; then
        echo "verify-signature: '$codesign_tool -dvv $name' printed nothing (exit $display_status)." >&2
        echo "                  --verify had already accepted these bytes, so the identity could" >&2
        echo "                  not be READ — that is a tooling failure, not evidence about the" >&2
        echo "                  artifact, and it is scored UNCHECKED rather than FAILED." >&2
        unchecked=1
        continue
    fi

    # `case`, not `grep -qF`, and the reason is the taxonomy. These two tests
    # are the only ones whose FAILING branch produces 65, so they must not have
    # a way to fail for a reason of their own: a grep that is absent, broken, or
    # killed exits non-zero, `! grep` reads that as "the marker is not there",
    # and the gate announces bad bytes on the strength of a missing tool. A
    # `case` is the shell itself — it has no exit status to misread. (It is also
    # a better -F than -F: the needle is quoted, so a glob character in a team
    # id stays literal, and a multi-line needle is one needle rather than
    # several patterns of which one is empty.)
    missing=""
    case "$display_out" in
        *"Developer ID Application"*) : ;;
        *) missing="a Developer ID Application authority" ;;
    esac
    case "$display_out" in
        *"$team_id"*) : ;;
        *) missing="${missing:+$missing and }team id $team_id" ;;
    esac

    if [ -n "$missing" ]; then
        fail "$name: the signature does not name $missing — an ad-hoc or foreign signature is not a release signature (BR-2)."
        printf '%s\n' "$display_out" | quote
        continue
    fi

    pass "$name is signed with a Developer ID Application certificate naming team $team_id"
done

# A definite rejection outranks an unreadable one. If any binary was REJECTED,
# something was learned about these bytes and 65 is the honest, more useful
# verdict even when a sibling binary could not be read; only when nothing was
# rejected does an unreadable one leave the whole run unchecked. Both codes
# fail the release — the difference is what the release log claims happened.
if [ "$failures" -ne 0 ]; then
    echo "verify-signature: FAILED — $failures of ${#targets[@]} checked ($checked) are not Developer ID signed by team $team_id." >&2
    exit "$EXIT_FAILED"
fi

if [ "$unchecked" -ne 0 ]; then
    echo "verify-signature: $(basename "$artifact") could not be checked — nothing was learned." >&2
    exit "$EXIT_UNCHECKED"
fi

# Falling off the end is the PASS, exactly as in verify-version.sh and
# smoke.sh: the last command is this echo, so the status is 0. A literal
# `exit 0` here would be clearer to a reader but is not free — shellcheck reads
# it as a path that skips the EXIT trap, and then reports `cleanup` as a
# function nobody calls (SC2329) in a repo whose CI treats shellcheck as a gate.
echo "verify-signature: PASS — every binary checked ($checked) is Developer ID signed by team $team_id."
