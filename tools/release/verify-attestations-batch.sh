#!/usr/bin/env bash
#
# Verify that a WHOLE release carries provenance (BR-3/BR-5/BR-6) — that the
# directory about to be published contains exactly the three tarballs a complete
# release has, that `checksums.txt` describes those exact bytes, and that GitHub
# attests every one of them.
#
# Usage: tools/release/verify-attestations-batch.sh <dir> <owner/repo> [signer-workflow]
#
#   <dir>              a directory holding the release's artifacts: three
#                      `teton-*.tar.gz` and a `checksums.txt`.
#   <owner/repo>       the repository whose attestations are trusted, forwarded
#                      to verify-attestation.sh unchanged.
#   [signer-workflow]  optional, forwarded to verify-attestation.sh as
#                      `--signer-workflow`. It binds the attestation to a
#                      workflow inside that repository rather than to the
#                      repository alone.
#
# WHY A BATCH SCRIPT EXISTS AT ALL. verify-attestation.sh proves ONE artifact
# per invocation, deliberately: per BR-6 and LESSON-433 a verified arm64 tarball
# says nothing about the x86_64 one. The failure that leaves is a loop that
# checks two of three assets and reports success, which is worse than no gate,
# and a loop written inline in a workflow is a loop nothing tests. This script
# is that loop, in a file the selftest can build known-bad inputs for.
#
# THE COUNT IS THE POINT. `for f in dir/*.tar.gz` over a directory that received
# two of its three uploads iterates twice and exits 0, and every artifact it did
# see passes. A release missing a platform is a release the tap will happily
# point users at. So the count is asserted BEFORE anything is verified, against
# the same 3 the release workflow's EXPECTED_TARGETS carries, and a wrong count
# is a rejection rather than an "unchecked": the directory was read, and what
# was read is not a release.
#
# THE CHECKSUMS ARE CHECKED HERE, NOT ONLY BY THE CONSUMER. `gh` hashes each
# file itself, so the attestation check alone already binds provenance to the
# bytes on disk. What it does NOT check is that `checksums.txt` — the file users
# and the Homebrew formula compare against — describes those same bytes. A
# release whose tarballs are all attested and whose checksums file was generated
# from an earlier build is internally inconsistent in the one way nobody notices
# until an install fails.
#
# Exit codes are the same taxonomy the gates it calls use (BR-7, LESSON-442):
#
#   0   PASS       three tarballs, checksums agreeing, every one attested.
#   64  EX_USAGE   wrong invocation: missing argument, or <dir> is not a
#                  directory. A bad call site, not a bad release.
#   65  FAILED     something was READ and it is wrong: the wrong number of
#                  tarballs, no checksums.txt, a checksum that does not match
#                  its file, or gh rejecting an artifact. Blocks the release.
#   75  UNCHECKED  something could not be read at all: no sha256 tool, or a
#                  verify-attestation.sh run that reached no verdict (no gh, no
#                  token, network, API 4xx/5xx). Also blocks the release — the
#                  difference is what the log claims happened.
#
# AGGREGATION, AND WHY REJECTION OUTRANKS. Any 65 makes the whole run 65 even
# when another artifact was merely unchecked: something WAS learned about this
# release and it is bad, and losing that to a 75 because a sibling could not be
# reached would make the log claim less than the run knows. Only when nothing
# was rejected does an unreachable verdict leave the batch unchecked. This is
# the same ordering verify-signature.sh applies across the two binaries in a
# tarball, and smoke.sh across its signature checks.
#
# ANY CHILD STATUS THAT IS NOT 0 AND NOT 65 COUNTS AS UNCHECKED. That is not
# laziness about 64: a usage error out of verify-attestation.sh means this
# script called it wrongly, so nothing was verified, and the one reading it must
# never be able to become a pass.
#
# `set -e` is deliberately absent, exactly as in the two gates: under it an
# unhandled non-zero aborts with the failing command's own status, and a child
# that happened to exit 65 would be indistinguishable from a rejection this
# script decided on. Every status here is captured explicitly.
#
# The seam is inherited rather than re-declared: `TETON_GH` reaches
# verify-attestation.sh through the environment, and so does its refusal to
# honour that variable inside GitHub Actions without TETON_ALLOW_TOOL_SEAM=1.
# This script adds no seam of its own, so there is nothing here to guard.

set -uo pipefail

EXIT_USAGE=64
EXIT_FAILED=65
EXIT_UNCHECKED=75

# The number of tarballs a complete release has: one per leg of the release
# workflow's `build` matrix (ADR-548-2). The workflow spells the same 3 as
# EXPECTED_TARGETS; a fourth target lands in both, and in package.sh's and
# smoke.sh's target allowlists, or the gate keeps passing on a release missing a
# platform.
EXPECTED_TARBALLS=3

script_dir="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)"

# Checked, because `set -e` is absent here: an unchecked source that failed
# would leave `sha256_of` undefined, every call to it exiting 127, and this
# script classifying an accident. `source=` lets `shellcheck -x` follow this;
# `disable=SC1091` keeps a bare `shellcheck <file>` — which cannot follow it —
# from failing on the info.
# shellcheck source=tools/release/lib.sh disable=SC1091
if ! . "$script_dir/lib.sh"; then
    echo "verify-attestations-batch: could not source $script_dir/lib.sh — nothing was verified." >&2
    exit "$EXIT_UNCHECKED"
fi

VERIFY_ATTESTATION="$script_dir/verify-attestation.sh"

usage() {
    echo "usage: verify-attestations-batch.sh <dir> <owner/repo> [signer-workflow]" >&2
}

if [ "$#" -lt 2 ]; then
    echo "verify-attestations-batch: too few arguments — nothing was verified." >&2
    usage
    exit "$EXIT_USAGE"
fi

dir="$1"
repo="$2"
signer_workflow="${3:-}"

if [ -z "$dir" ]; then
    echo "verify-attestations-batch: no directory given — nothing was verified." >&2
    usage
    exit "$EXIT_USAGE"
fi

# A directory that is not there is a CALL SITE mistake — the wrong path, an
# unset variable that expanded to nothing — and 64 says so. It is deliberately
# not 65: 65 is reserved for a directory that was read and found to be the wrong
# shape, which is a statement about the release rather than about the command.
if [ ! -d "$dir" ]; then
    echo "verify-attestations-batch: not a directory: $dir — nothing was verified." >&2
    usage
    exit "$EXIT_USAGE"
fi

if [ -z "$repo" ]; then
    echo "verify-attestations-batch: no <owner/repo> given — nothing was verified." >&2
    usage
    exit "$EXIT_USAGE"
fi

# The repo shape is verify-attestation.sh's to enforce, and it is left there on
# purpose: two copies of that `case` would be two places to drift. A malformed
# repo therefore reaches this script's children and comes back as 64 per
# artifact, which the aggregator scores UNCHECKED — honest, since nothing was
# verified — and the children's own message names the problem.

if [ ! -f "$script_dir/verify-attestation.sh" ]; then
    echo "verify-attestations-batch: $VERIFY_ATTESTATION is missing — nothing was verified." >&2
    exit "$EXIT_UNCHECKED"
fi

# `nullglob` so a directory with no tarballs yields an EMPTY array rather than
# the one-element array holding the literal pattern. Without it the count below
# would read 1, and the loop would then hand `dir/teton-*.tar.gz` to a gate that
# would truthfully report it as a missing file — a correct statement about a
# path that does not exist, in place of "this release has no artifacts".
shopt -s nullglob
tarballs=("$dir"/teton-*.tar.gz)
shopt -u nullglob

echo "verify-attestations-batch: $dir — expecting $EXPECTED_TARBALLS attested tarballs from $repo"

if [ "${#tarballs[@]}" -ne "$EXPECTED_TARBALLS" ]; then
    echo "verify-attestations-batch: FAILED — expected $EXPECTED_TARBALLS tarballs matching" >&2
    echo "                           teton-*.tar.gz in $dir, found ${#tarballs[@]}." >&2
    echo "                           A release missing a platform is not a release, and a gate that" >&2
    echo "                           verifies the artifacts that happen to be present would pass it." >&2
    # Guarded, because the interesting case here is the EMPTY one and bash 3.2 —
    # which is the /bin/bash every macOS runner and every developer Mac still
    # ships — treats `"${array[@]}"` on an empty array as an unbound variable
    # under `set -u`. Unguarded, this listing aborted the script with a bare
    # exit 1 immediately after printing the correct diagnosis, so a release with
    # no artifacts at all produced a status outside this script's own taxonomy.
    if [ "${#tarballs[@]}" -gt 0 ]; then
        for t in "${tarballs[@]}"; do
            echo "                           | $(basename "$t")" >&2
        done
    fi
    exit "$EXIT_FAILED"
fi

checksums="$dir/checksums.txt"
if [ ! -f "$checksums" ]; then
    echo "verify-attestations-batch: FAILED — $checksums is missing." >&2
    echo "                           checksums.txt is what users and the Homebrew formula compare" >&2
    echo "                           downloads against (BR-5); a release published without it has" >&2
    echo "                           no way for anyone downstream to notice a substituted byte." >&2
    exit "$EXIT_FAILED"
fi

# The recorded digest for one file, read out of checksums.txt.
#
# Parsed in the shell rather than with `grep "$name"`: a filename is a regex to
# grep and a literal here, and `grep -F` would still match a line whose NAME
# merely contains this one. The format is `<sha256>  <name>`, which both
# `shasum -a 256 -c` and `sha256sum -c` read, and whose binary-mode variant
# marks the name with a leading `*`.
recorded_digest() {
    local want="$1" line sha name
    while IFS= read -r line || [ -n "$line" ]; do
        case "$line" in
            '' | '#'*) continue ;;
        esac
        sha="${line%%[[:space:]]*}"
        name="${line##*[[:space:]]}"
        name="${name#\*}"
        if [ "$name" = "$want" ]; then
            printf '%s\n' "$sha"
            return 0
        fi
    done <"$checksums"
    return 1
}

failed=0
unchecked=0

for tarball in "${tarballs[@]}"; do
    name="$(basename "$tarball")"

    # --- the checksum this release publishes describes these bytes ---------
    actual=""
    if ! actual="$(sha256_of "$tarball")"; then
        # No sha256 tool, or one that exists and failed. Nothing was learned
        # about this file, and a hash that could not be computed is not a hash
        # that matched (LESSON-442).
        echo "verify-attestations-batch: UNCHECKED — could not compute a sha256 for $name." >&2
        echo "                           No shasum, sha256sum or openssl on this machine, so the" >&2
        echo "                           recorded checksum could not be tested against the bytes." >&2
        unchecked=1
        continue
    fi

    expected=""
    if ! expected="$(recorded_digest "$name")"; then
        echo "verify-attestations-batch: FAILED — $name has no line in checksums.txt." >&2
        echo "                           The file is being published with no recorded digest, so" >&2
        echo "                           nothing downstream can tell these bytes from other bytes." >&2
        failed=1
        continue
    fi

    # Both sides come from the same family of tools (shasum / sha256sum /
    # openssl), all of which print lowercase hex, and lib.sh's sha256_of refuses
    # anything that is not 64 hex characters — so a literal comparison is the
    # whole test and a case fold would only hide a writer that had drifted.
    if [ "$actual" != "$expected" ]; then
        echo "verify-attestations-batch: FAILED — $name does not match its checksums.txt line." >&2
        echo "                           recorded: $expected" >&2
        echo "                           actual:   $actual" >&2
        echo "                           The bytes in this directory are not the bytes this release" >&2
        echo "                           says it is publishing." >&2
        failed=1
        continue
    fi

    echo "verify-attestations-batch: $name — sha256 matches checksums.txt"

    # --- GitHub attests these bytes ----------------------------------------
    # Invoked through `bash` like every other call to these scripts in CI, so a
    # lost exec bit cannot turn a gate into a 126. The signer workflow is
    # forwarded only when there is one: passing an empty third argument would
    # make verify-attestation.sh hand gh `--signer-workflow ''`.
    status=0
    if [ -n "$signer_workflow" ]; then
        bash "$VERIFY_ATTESTATION" "$tarball" "$repo" "$signer_workflow" || status=$?
    else
        bash "$VERIFY_ATTESTATION" "$tarball" "$repo" || status=$?
    fi

    case "$status" in
        0) : ;;
        "$EXIT_FAILED")
            echo "verify-attestations-batch: FAILED — gh rejected $name." >&2
            failed=1
            ;;
        *)
            # 75, and everything else it could exit with, 64 included. The
            # verdict was not reached, and "could not check" is not a pass.
            echo "verify-attestations-batch: UNCHECKED — no provenance verdict for $name (verify-attestation.sh exit $status)." >&2
            unchecked=1
            ;;
    esac
done

if [ "$failed" -ne 0 ]; then
    echo "verify-attestations-batch: FAILED — $dir does not verify against $repo. The release is blocked." >&2
    exit "$EXIT_FAILED"
fi

if [ "$unchecked" -ne 0 ]; then
    echo "verify-attestations-batch: UNCHECKED — $dir could not be verified against $repo, so nothing was learned." >&2
    echo "                           Deliberately not a rejection, and deliberately not a pass." >&2
    exit "$EXIT_UNCHECKED"
fi

echo "verify-attestations-batch: PASS — all $EXPECTED_TARBALLS tarballs in $dir match checksums.txt and are attested by $repo."
