#!/usr/bin/env bash
#
# Verify that a release artifact carries a GitHub build attestation (BR-3) —
# provenance for these exact bytes: which workflow, at which commit, built
# them.
#
# Usage: tools/release/verify-attestation.sh <artifact> <owner/repo> [signer-workflow]
#
#   <artifact>         the file whose digest must be attested — a tarball, a
#                      checksums file, any uploaded byte sequence. `gh` hashes
#                      the file itself, so the subject is always the bytes on
#                      disk and never a digest handed to it (BR-3).
#   <owner/repo>       the repository whose attestations are trusted, passed
#                      straight to `--repo`. Required, and required to LOOK like
#                      a repo: `gh` will happily search a repo that is not this
#                      one.
#   [signer-workflow]  optional, passed on as `--signer-workflow`. It binds the
#                      attestation to a WORKFLOW inside that repository rather
#                      than to the repository alone. Without it, any workflow in
#                      <owner/repo> that can mint an attestation — including one
#                      added by a pull request, or a reusable workflow nobody
#                      reviewed — produces provenance this gate accepts. gh's
#                      own help says as much: "Ideally, the path of the signer
#                      workflow is also validated". Optional rather than
#                      required only because the callers that have a value for
#                      it pass one and the ones that do not must still be able
#                      to ask the weaker question.
#
# WHERE THE REAL TOOL RUNS: anywhere `gh` exists — the release job and the
# verify-install job both run it, on every platform. Unlike the signature gate
# this one is not macOS-only, because attestations describe the build rather
# than the platform. It still proves one artifact per invocation: per BR-6 and
# LESSON-433, a verified arm64 tarball says nothing about the x86_64 one.
# verify-attestations-batch.sh is the wrapper that asks it of a whole release.
#
# `TETON_GH` overrides which tool is asked (default: `gh`). It exists so
# selftest.sh can drive every branch below with stand-ins — no network, no
# token, no real attestation — on the Linux `tooling` job. It changes WHO is
# asked, never how the answer is scored — but whoever sets it controls the
# ANSWER completely, so it is a test seam and not a configuration knob. Inside
# GitHub Actions this gate REFUSES to honour it (exit 64) unless
# `TETON_ALLOW_TOOL_SEAM=1` is also set, which is selftest.sh saying "I am the
# harness". A release verified against a stand-in gh is not verified.
#
# Exit codes are a taxonomy, matching smoke.sh and verify-version.sh (BR-7,
# LESSON-442):
#
#   0   PASS       gh verified the artifact against <owner/repo>.
#   64  EX_USAGE   wrong invocation (missing argument, malformed repo, or the
#                  TETON_GH seam offered to a CI run that is not a harness).
#   65  FAILED     gh RAN and REJECTED these bytes: it reached the attestation
#                  store and no attestation there covers this digest. This is
#                  the supply-chain alarm, and it blocks the release.
#   75  UNCHECKED  the check could NOT run or did not reach a verdict: no gh,
#                  artifact missing, or gh failed for a reason that is about
#                  the environment (no token, rate limit, network, API 5xx,
#                  API 404) rather than about the bytes.
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
# case-insensitively. Both alternatives are phrases gh prints ONLY after
# reaching the attestation store and finding nothing there for this digest:
#
#   no attestations found      observed verbatim against gh 2.96.0 as
#                              `Error: no attestations found` (exit 1), for a
#                              repository that has attestations but none for
#                              these bytes.
#   no matching attestations   gh's other wording for the same verdict, kept
#                              because it is the phrase that appears once
#                              attestations were fetched and then filtered
#                              (predicate type, signer workflow) to nothing.
#
# WHAT WAS REMOVED, AND WHY. This pattern used to also carry "verification
# failed" and "failed to verify". Both are substrings of environment failures
# gh prints without ever reaching a verdict — "failed to verify" appears in
# transport and trusted-root errors — so either one let a broken network
# announce a supply-chain alarm. Narrowing the pattern can only make 65 harder
# to reach, which is the direction this gate is allowed to be wrong in: 75 also
# blocks the release, and it does not lie about why.
REJECTION_PATTERN='no attestations found|no matching attestations'

# Output that means gh did not reach the store at all, tested BEFORE the
# rejection pattern and winning over it. Environment beats verdict: if a single
# run of gh manages to print both, the honest reading is that something in the
# transport is broken and whatever verdict-shaped text came with it cannot be
# trusted.
#
# THE 404 IS THE INTERESTING ONE, and it is why this pattern exists at all.
# Observed against gh 2.96.0: an artifact whose digest is not in a repository's
# attestation store — including every artifact in a repository that has never
# published an attestation, i.e. the state this project is in before its first
# signed release — comes back as
#
#   Error: HTTP 404: Not Found (https://api.github.com/repos/<owner>/<repo>/
#   attestations/sha256:<digest>?per_page=30&predicate_type=...)
#
# and NOT as a verdict. That 404 is indistinguishable from a repository that
# does not exist, a repository this token cannot see, and an API returning 404
# while degraded. It is therefore scored 75 UNCHECKED, deliberately, even
# though one of the things it can mean is genuinely "these bytes are not
# attested". The release is blocked either way; the difference is that the log
# says "no verdict was reached" instead of raising a supply-chain alarm that
# would be a lie in three of its four possible causes.
#
# Every alternative is a whole phrase, never a bare number: a digest is hex, so
# a pattern of `404` alone would match `…c4b5a404e…` inside gh's own summary of
# a genuine rejection and quietly demote it. `http 4[0-9][0-9]` covers the whole
# 4xx family on purpose — 401, 403 and 404 from the attestations endpoint are
# all "this token could not see the store", none of them a statement about the
# bytes.
UNCHECKED_PATTERN='failed to fetch|dial |i/o timeout|timed? ?out|deadline exceeded|no such host|connection refused|rate limit|http [45][0-9][0-9]'

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
    echo "usage: verify-attestation.sh <artifact> <owner/repo> [signer-workflow]" >&2
}

# Quoting a tool's output back into the log, at ONE indent width. Everything
# this script quotes is quoted through here.
quote() { sed 's/^/       | /' >&2; }

# The PASS path is the single exception, and only in WHICH STREAM it writes to.
# gh's summary on a green run is evidence — it names the workflow and the commit
# the bytes were built from, which is the whole reason for asking — so it
# belongs on stdout beside the passing line rather than on stderr beside a
# complaint. The prefix is deliberately identical, so a release log reads the
# same shape whether the news is good or bad.
quote_evidence() { sed 's/^/       | /'; }

# The seam is a TEST seam, and it is refused where tests do not run. See the
# matching guard in verify-signature.sh: `TETON_GH` decides which program
# answers "does GitHub attest these bytes", so whoever sets it decides the
# verdict outright. selftest.sh needs that offline; a release job has no reason
# to set it, and a release verified against a stand-in gh is not verified.
if [ "${GITHUB_ACTIONS:-}" = "true" ] &&
    [ -n "${TETON_GH:-}" ] &&
    [ "${TETON_ALLOW_TOOL_SEAM:-}" != "1" ]; then
    echo "verify-attestation: TETON_GH is set inside GitHub Actions, and TETON_ALLOW_TOOL_SEAM=1 is" >&2
    echo "                    not — refusing to run. That variable replaces the tool whose answer IS" >&2
    echo "                    this gate's verdict, so honouring it in CI would let an environment" >&2
    echo "                    variable manufacture provenance. Nothing was verified." >&2
    usage
    exit "$EXIT_USAGE"
fi

if [ "$#" -lt 2 ]; then
    echo "verify-attestation: too few arguments — nothing was verified." >&2
    usage
    exit "$EXIT_USAGE"
fi

artifact="$1"
repo="$2"
signer_workflow="${3:-}"

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

if [ -n "$signer_workflow" ]; then
    echo "verify-attestation: $(basename "$artifact") — expecting an attestation from $repo, signed by $signer_workflow"
else
    echo "verify-attestation: $(basename "$artifact") — expecting an attestation from $repo"
fi

# Built as an array so the optional flag is a real pair of argv entries rather
# than a string that word-splits, and so an empty signer workflow adds nothing
# instead of adding `--signer-workflow ''` — which gh would take as an identity
# to enforce, and no certificate's SAN is the empty string.
#
# `--` closes flag parsing before the artifact path, so a tarball whose name
# begins with `-` is a subject and not an unknown option. Verified against gh
# 2.96.0: `gh attestation verify --repo O/R -- <path>` works, and the `--` must
# come AFTER the flags — `gh attestation verify -- <path> --repo O/R` makes
# `--repo` positional and dies with "too many arguments".
gh_args=(attestation verify --repo "$repo")
if [ -n "$signer_workflow" ]; then
    gh_args+=(--signer-workflow "$signer_workflow")
fi
gh_args+=(-- "$artifact")

gh_status=0
gh_out="$("$gh_tool" "${gh_args[@]}" 2>&1)" || gh_status=$?

if [ "$gh_status" -eq 0 ]; then
    if [ -n "$gh_out" ]; then
        # Keep gh's own summary in the release log: it names the workflow and
        # the commit the bytes came from, which is the whole point of asking.
        printf '%s\n' "$gh_out" | quote_evidence
    fi
    echo "verify-attestation: PASS — $(basename "$artifact") verifies against $repo."
    exit 0
fi

# Environment first, and it OUTRANKS the rejection pattern below. A run that
# somehow prints both is a run whose transport is broken, and a verdict carried
# out of a broken transport is not a verdict. See UNCHECKED_PATTERN for the 404
# that makes this ordering matter in practice rather than in theory.
if printf '%s' "$gh_out" | grep -Eqi -- "$UNCHECKED_PATTERN"; then
    echo "verify-attestation: UNCHECKED — gh exited $gh_status reporting an ENVIRONMENT failure (network," >&2
    echo "                    auth, rate limit, or an API 4xx/5xx), so nothing was learned about" >&2
    echo "                    $(basename "$artifact")." >&2
    echo "                    Deliberately NOT reported as a verification failure: an auth, rate-limit," >&2
    echo "                    or network error must never be announced as a supply-chain alarm." >&2
    printf '%s\n' "$gh_out" | quote
    exit "$EXIT_UNCHECKED"
fi

# Note the direction of this test: 65 requires grep to RUN and MATCH. A grep
# that is missing or broken exits non-zero, which falls through to UNCHECKED
# below — the safe side. Inverting this into `if ! grep …` would hand a missing
# tool the power to announce a supply-chain failure (LESSON-442).
if printf '%s' "$gh_out" | grep -Eqi -- "$REJECTION_PATTERN"; then
    echo "verify-attestation: FAILED — gh rejected $(basename "$artifact") (exit $gh_status): these bytes do not verify against $repo." >&2
    printf '%s\n' "$gh_out" | quote
    exit "$EXIT_FAILED"
fi

echo "verify-attestation: UNCHECKED — gh exited $gh_status without a verification verdict, so nothing was learned about $(basename "$artifact")." >&2
echo "                    Deliberately NOT reported as a verification failure: an auth, rate-limit," >&2
echo "                    or network error must never be announced as a supply-chain alarm." >&2
if [ -n "$gh_out" ]; then
    printf '%s\n' "$gh_out" | quote
else
    echo "                    (gh printed nothing.)" >&2
fi
exit "$EXIT_UNCHECKED"
