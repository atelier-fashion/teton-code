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
    if command -v shasum >/dev/null 2>&1; then
        shasum -a 256 "$1" | awk '{ print $1 }'
    elif command -v sha256sum >/dev/null 2>&1; then
        sha256sum "$1" | awk '{ print $1 }'
    elif command -v openssl >/dev/null 2>&1; then
        openssl dgst -sha256 "$1" | awk '{ print $NF }'
    else
        return 1
    fi
}
