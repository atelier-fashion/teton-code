#!/usr/bin/env bash
#
# Shared helpers for the release scripts in this directory. Sourced, not run:
#
#     script_dir="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)"
#     # shellcheck source=tools/release/lib.sh
#     . "$script_dir/lib.sh"
#
# Both helpers below existed as two copies before this file did, and both pairs
# had already drifted apart:
#
#   * `sha256_of` — package.sh's copy fell through to an unconditional `openssl
#     dgst`, so a machine with none of the three tools aborted the build with a
#     bare "command not found" under `set -e`, a code outside package.sh's own
#     taxonomy. render-formula.sh's copy checked `command -v` and returned 1.
#     The returning shape is the one that survived.
#   * the release-version shape — render-formula.sh and site/render.sh both
#     accepted `-rc.1`/`+build` suffixes that the release workflow's tag
#     preflight refuses, so the two halves of the release disagreed about what
#     a version is.
#
# This file stays deliberately small. Something belongs here when two release
# scripts need it AND they must not be allowed to disagree about it; anything
# used by exactly one script stays in that script, where its reasons are.

# Is this string a release version?
#
# Strictly `X.Y.Z` — no `-rc.1` prerelease, no `+build` metadata, and not the
# empty string. The whole release path assumes one artifact set per version,
# published under URLs built by pasting the version into a path, so a suffixed
# version renders download URLs and formula stanzas for bytes no release ever
# uploaded. Refusing is the honest answer until prereleases are something this
# project actually ships; loosening this regex means teaching the tarball
# naming, the tap push, and the landing page about prereleases first.
#
# THIS IS THE AUTHORITY for that shape. Two other places restate it and must be
# changed with it:
#   * .github/workflows/release.yml — the tag preflight.
#   * site/render.sh — which cannot source this file (the deploy workflow runs
#     it from a checkout that need not contain tools/), so it carries its own
#     copy behind a comment pointing here.
is_release_version() {
    # The `case` runs FIRST because the grep below is line-anchored, not
    # string-anchored: `grep -Eq '^...$'` happily matches a multi-line string
    # whose first line looks like a version, while release.yml's bash `[[ =~ ]]`
    # would reject it. Without this guard the file that calls itself the
    # authority would be the most permissive of the three. (site/render.sh
    # carries the same pair for the same reason.)
    case "${1-}" in
        *[!0-9.]* | '') return 1 ;;
    esac
    printf '%s' "${1-}" | grep -Eq '^[0-9]+\.[0-9]+\.[0-9]+$'
}

# Print a file's sha256, using whatever the platform ships: macOS has `shasum`,
# Linux has `sha256sum`, openssl is the last resort.
#
# Returns 1 when the machine has none of them, rather than running a command
# that is not there and letting `set -e` abort with a generic 1. The caller
# knows which of its own exit codes "this could not be computed" deserves; this
# function does not.
sha256_of() {
    local digest=""
    if command -v shasum >/dev/null 2>&1; then
        digest="$(shasum -a 256 "$1" | awk '{ print $1 }')"
    elif command -v sha256sum >/dev/null 2>&1; then
        digest="$(sha256sum "$1" | awk '{ print $1 }')"
    elif command -v openssl >/dev/null 2>&1; then
        # `$NF`, not `$2`: openssl's prefix has already changed once
        # (`SHA256(f)=` -> `SHA2-256(f)=`), and the digest is the last field
        # under both.
        digest="$(openssl dgst -sha256 "$1" | awk '{ print $NF }')"
    else
        return 1
    fi

    # A tool that EXISTS but fails leaves awk with empty input, and the
    # pipeline's status is awk's — zero. Without this the function returns
    # success with an empty digest, and an empty string is what would then be
    # written into checksums.txt or spliced into the formula's `sha256`. Only a
    # well-formed digest counts as having hashed anything.
    #
    # The hex test covers the WHOLE string, not a prefix. It used to anchor only
    # the first eight characters and then check the length, which accepted a
    # 64-character value whose tail was anything at all — `0f1e2d3c` followed by
    # 56 characters of a tool's error message is not a digest, and the two
    # consumers of this function (checksums.txt, the formula's `sha256`) both
    # take what it prints on trust. Empty is rejected by the length test, since
    # the empty string contains no character outside [0-9a-f].
    case "$digest" in
        *[!0-9a-f]*) return 1 ;;
    esac
    if [ "${#digest}" -eq 64 ]; then
        printf '%s\n' "$digest"
        return 0
    fi
    return 1
}

# Print the external tool a gate script should run — the override if one is set,
# otherwise the default name, resolved on PATH:
#
#     codesign="$(tool_or_unchecked "${TETON_CODESIGN:-}" codesign)" || exit "$EXIT_UNCHECKED"
#
# Returns 1 when neither resolves to something runnable, for the same reason
# `sha256_of` returns rather than aborting: the CALLER owns its exit taxonomy.
# Both gates spell that outcome 75 UNCHECKED today, but a helper that exited on
# their behalf would be a second place where the taxonomy lives — and the one
# thing that must never happen is a gate exiting with a code it did not choose
# (LESSON-442). It also never falls through to running the default name and
# hoping: a command that is not there fails with 127, which is neither a
# rejection nor a verdict.
#
# The override argument is how selftest.sh drives the gates on the Linux
# `tooling` job, where `codesign` does not exist and `gh` has no token
# (ADR-550-4).
#
# BE PRECISE ABOUT WHAT THAT SEAM CANNOT DO, because the earlier wording here
# claimed more than is true. It said "no value of TETON_CODESIGN or TETON_GH can
# turn a rejection into a pass", which is false and dangerously so: a stand-in
# that prints a Developer ID dump for any input, or a `gh` that exits 0, is
# exactly a value that turns a rejection into a pass. That is the entire reason
# selftest.sh can build a KNOWN-GOOD fixture at all.
#
# The property that actually holds is narrower, and it is the one the gates
# rely on: the override changes WHICH tool is asked and nothing else. The
# caller's CLASSIFICATION — which answer earns 0, which earns 65, which earns
# 75 — is not reachable from here, so no override can make the gate score an
# answer differently than it scores the real tool's. Whoever controls the
# override controls the ANSWER, completely; that is why verify-signature.sh and
# verify-attestation.sh refuse to honour these variables inside GitHub Actions
# unless TETON_ALLOW_TOOL_SEAM=1 says a test harness meant it.
tool_or_unchecked() {
    local tool="${1:-}"
    if [ -z "$tool" ]; then
        tool="${2:-}"
    fi
    if [ -z "$tool" ]; then
        return 1
    fi

    # `command -v`, not `[ -x ]`: the default is a bare name to be found on
    # PATH, while an override is usually a path to a stand-in. This resolves
    # both shapes, and rejects a path that exists but cannot be executed. `--`
    # keeps a tool name that begins with `-` from being read as an option.
    command -v -- "$tool" 2>/dev/null || return 1
}
