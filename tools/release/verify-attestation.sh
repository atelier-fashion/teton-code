#!/usr/bin/env bash
#
# Verify that a release artifact carries a GitHub build attestation (BR-3) —
# provenance for these exact bytes: which workflow, at which commit, built
# them.
#
# Usage: tools/release/verify-attestation.sh <artifact> <owner/repo>
#
#   <artifact>    the file whose digest must be attested — a tarball, a
#                 checksums file, any uploaded byte sequence. `gh` hashes the
#                 file itself, so the subject is always the bytes on disk and
#                 never a digest handed to it (BR-3).
#   <owner/repo>  the repository whose attestations are trusted, passed
#                 straight to `--repo`. Required, and required to LOOK like a
#                 repo: `gh` will happily search a repo that is not this one.
#
# WHERE THE REAL TOOL RUNS: anywhere `gh` exists — the release job and the
# verify-install job both run it, on every platform. Unlike the signature gate
# this one is not macOS-only, because attestations describe the build rather
# than the platform. It still proves one artifact per invocation: per BR-6 and
# LESSON-433, a verified arm64 tarball says nothing about the x86_64 one.
#
# `TETON_GH` overrides which tool is asked (default: `gh`). It exists so
# selftest.sh can drive every branch below with stand-ins — no network, no
# token, no real attestation — on the Linux `tooling` job. It changes WHO is
# asked, never how the answer is scored.
#
# Exit codes are a taxonomy, matching smoke.sh and verify-version.sh (BR-7,
# LESSON-442):
#
#   0   PASS       gh verified the artifact against <owner/repo>.
#   64  EX_USAGE   wrong invocation (missing argument, malformed repo).
#   65  FAILED     gh RAN and REJECTED these bytes: no attestation covers this
#                  digest, or the one that claims to does not verify. This is
#                  the supply-chain alarm, and it blocks the release.
#   75  UNCHECKED  the check could NOT run or did not reach a verdict: no gh,
#                  artifact missing, or gh failed for a reason that is about
#                  the environment (no token, rate limit, network, API 5xx)
#                  rather than about the bytes.
#
# The classification of a non-zero `gh` IS this script. It is deliberately
# asymmetric: only output that clearly states a verification verdict earns 65,
# and everything else — including a failure whose output this script does not
# recognise — is 75. 65 must be unforgeable as "these bytes are bad"
# (LESSON-442), because an offline runner announcing a supply-chain failure is
# both a lie and, once it happens twice, the reason nobody believes the alarm.
# Widening REJECTION_PATTERN is therefore the dangerous direction: a phrase
# belongs in it only if gh cannot print it while the network is at fault.
#
# `set -e` is deliberately absent for the same reason it is absent from
# verify-signature.sh: under it an unhandled non-zero aborts with the failing
# command's own status, which is exactly the collision LESSON-442 is about.

set -uo pipefail

EXIT_USAGE=64
EXIT_FAILED=65
EXIT_UNCHECKED=75

# Output that means gh reached a verdict and the verdict is NO. Matched
# case-insensitively; kept short, and every alternative is a phrase about the
# ARTIFACT:
#
#   verification failed        the summary line for a rejected subject.
#   failed to verify           the same verdict in error form.
#   no attestations found      these bytes have no provenance at all.
#   no matching attestations   attestations exist; none covers this digest.
#
# Note what is deliberately absent: "failed to fetch attestations", "HTTP 5xx",
# authentication and rate-limit wording. Those are the environment failing, not
# the artifact, and they must land on 75.
REJECTION_PATTERN='verification failed|failed to verify|no attestations found|no matching attestations'

script_dir="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)"

# Checked, because `set -e` is absent here: an unchecked source that failed
# would leave `tool_or_unchecked` undefined, every call to it exiting 127, and
# this script classifying an accident. `source=` lets `shellcheck -x` follow
# this; `disable=SC1091` keeps a bare `shellcheck <file>` — which cannot follow
# it — from failing on the info.
# shellcheck source=tools/release/lib.sh disable=SC1091
if ! . "$script_dir/lib.sh"; then
    echo "verify-attestation: could not source $script_dir/lib.sh — nothing was verified." >&2
    exit "$EXIT_UNCHECKED"
fi

usage() {
    echo "usage: verify-attestation.sh <artifact> <owner/repo>" >&2
}

if [ "$#" -lt 2 ]; then
    echo "verify-attestation: too few arguments — nothing was verified." >&2
    usage
    exit "$EXIT_USAGE"
fi

artifact="$1"
repo="$2"

if [ -z "$artifact" ]; then
    echo "verify-attestation: no artifact given — nothing was verified." >&2
    usage
    exit "$EXIT_USAGE"
fi

# The repo must be given, and must look like one. An empty or malformed value
# would reach `gh --repo` and come back as an argument error — a failure with
# no verdict in it, which this script would honestly report as UNCHECKED, and
# UNCHECKED is a terrible way to learn about a typo.
#
# Shape-checked with `case` rather than a `grep -Eq '^…$'`, for two reasons.
# grep's anchors are LINE anchors, not string anchors, so `^…$` happily accepts
# a multi-line value whose first line looks like a repo — the same hole lib.sh's
# `is_release_version` documents and guards. And a `case` cannot fail for a
# reason of its own: it is the shell, so there is no exit status to misread when
# the environment is missing a tool.
refuse_repo() {
    echo "verify-attestation: <owner/repo> must be a repository like 'atelier-fashion/teton-code'; got '$repo'." >&2
    usage
    exit "$EXIT_USAGE"
}

case "$repo" in
    # Empty; more than one slash; a slash at either end; a component opening
    # with a dot or a dash — none of these is a repository.
    '' | */*/* | /* | */ | .* | -* | */.* | */-*) refuse_repo ;;
    # Any character outside what GitHub allows in an owner or repo name. This
    # is also what rejects whitespace, newlines included.
    *[!A-Za-z0-9._/-]*) refuse_repo ;;
    # Exactly one slash, every character legal: owner/repo.
    */*) : ;;
    *) refuse_repo ;;
esac

gh_tool=""
if ! gh_tool="$(tool_or_unchecked "${TETON_GH:-}" gh)"; then
    echo "verify-attestation: '${TETON_GH:-gh}' is not on this machine — nothing was verified." >&2
    echo "                    A missing tool is not evidence that the artifact has provenance." >&2
    exit "$EXIT_UNCHECKED"
fi

if [ ! -f "$artifact" ]; then
    echo "verify-attestation: not found: $artifact — nothing was verified." >&2
    exit "$EXIT_UNCHECKED"
fi

echo "verify-attestation: $(basename "$artifact") — expecting an attestation from $repo"

gh_status=0
gh_out="$("$gh_tool" attestation verify "$artifact" --repo "$repo" 2>&1)" || gh_status=$?

if [ "$gh_status" -eq 0 ]; then
    if [ -n "$gh_out" ]; then
        # Keep gh's own summary in the release log: it names the workflow and
        # the commit the bytes came from, which is the whole point of asking.
        printf '%s\n' "$gh_out" | sed 's/^/  | /'
    fi
    echo "verify-attestation: PASS — $(basename "$artifact") verifies against $repo."
    exit 0
fi

# Note the direction of this test: 65 requires grep to RUN and MATCH. A grep
# that is missing or broken exits non-zero, which falls through to UNCHECKED
# below — the safe side. Inverting this into `if ! grep …` would hand a missing
# tool the power to announce a supply-chain failure (LESSON-442).
if printf '%s' "$gh_out" | grep -Eqi -- "$REJECTION_PATTERN"; then
    echo "verify-attestation: FAILED — gh rejected $(basename "$artifact") (exit $gh_status): these bytes do not verify against $repo." >&2
    printf '%s\n' "$gh_out" | sed 's/^/       | /' >&2
    exit "$EXIT_FAILED"
fi

echo "verify-attestation: UNCHECKED — gh exited $gh_status without a verification verdict, so nothing was learned about $(basename "$artifact")." >&2
echo "                    Deliberately NOT reported as a verification failure: an auth, rate-limit," >&2
echo "                    or network error must never be announced as a supply-chain alarm." >&2
if [ -n "$gh_out" ]; then
    printf '%s\n' "$gh_out" | sed 's/^/       | /' >&2
else
    echo "                    (gh printed nothing.)" >&2
fi
exit "$EXIT_UNCHECKED"
