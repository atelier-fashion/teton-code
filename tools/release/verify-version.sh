#!/usr/bin/env bash
#
# Verify that a release tag and the workspace's declared version agree (BR-3).
#
# Usage:
#   tools/release/verify-version.sh <tag> [manifest]
#   tools/release/verify-version.sh --print-version [manifest]
#
#   <tag>       the release tag, with or without its leading `v` (`v0.1.0`).
#   [manifest]  the workspace root Cargo.toml; defaults to this repo's.
#
# `--print-version` prints the workspace version and exits 0 without comparing
# anything. It exists so every caller shares ONE parser for
# `[workspace.package] version` — the release workflow's `workflow_dispatch`
# path has no tag to gate against, and a second copy of this parse expressed in
# YAML would be a second thing to get wrong.
#
# The version is read from the root manifest's `[workspace.package]` block and
# nowhere else: every crate manifest says `version.workspace = true`, so
# grepping the crates would find the inheritance marker, not a version.
#
# Exit codes are a taxonomy, not a boolean — the same stance the catalog gate in
# ci.yml takes (LESSON-442, ADR-548-4). A release must never be able to confuse
# "the versions disagree" with "the check could not run":
#
#   0   MATCH      the tag names exactly the version the workspace declares.
#   64  MISMATCH   EX_USAGE. The check RAN and FAILED: the tag and
#                  `[workspace.package] version` name different versions, both
#                  of which are printed. Deliberately not a bare `1` — `1` is
#                  also what a missing file, a typo'd command, and a broken pipe
#                  exit with, and a version gate that is indistinguishable from
#                  a typo is not a gate.
#   75  UNCHECKED  the check could NOT be performed (no tag given, manifest
#                  missing, or no version in `[workspace.package]`). This is not
#                  evidence that the versions agree, so it still fails.

set -euo pipefail

EXIT_MISMATCH=64
EXIT_UNCHECKED=75

script_dir="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)"
repo_root="$(cd -- "$script_dir/../.." && pwd)"

usage() {
    echo "usage: verify-version.sh <tag> [manifest]" >&2
    echo "       verify-version.sh --print-version [manifest]" >&2
}

if [ "$#" -lt 1 ] || [ -z "${1:-}" ]; then
    echo "verify-version: no tag given, so nothing was checked." >&2
    usage
    exit "$EXIT_UNCHECKED"
fi

tag="$1"
manifest="${2:-$repo_root/Cargo.toml}"

if [ ! -f "$manifest" ]; then
    echo "verify-version: manifest not found: $manifest — nothing was checked." >&2
    exit "$EXIT_UNCHECKED"
fi

# Read `version` from the `[workspace.package]` table only. Every `[` line
# re-evaluates whether we are inside that table, so a `version` under
# `[package]`, `[workspace.dependencies]`, or any other table can never be
# picked up by accident.
#
# Both patterns match on the whole line rather than on awk's default
# whitespace-split fields, because TOML does not require the whitespace those
# fields assume. `version="0.1.0"` is one field, not three, and a `$1 ==
# "version"` test missed it; `[workspace.package] # comment` is legal TOML that
# an anchored `]$` header test refused to enter. Both forms exited 75 —
# "the check could not run" — against a manifest that was perfectly well-formed
# and did declare a version. A release gate that a legal reformat can silence
# is not a gate.
manifest_version="$(
    awk '
        /^[[:space:]]*\[/ {
            in_block = ($0 ~ /^[[:space:]]*\[workspace\.package\][[:space:]]*(#.*)?$/)
            next
        }
        in_block && $0 ~ /^[[:space:]]*version[[:space:]]*=/ {
            value = $0
            sub(/^[^=]*=[[:space:]]*/, "", value)
            sub(/[[:space:]]*#.*$/, "", value)
            gsub(/["'"'"']/, "", value)
            sub(/[[:space:]]+$/, "", value)
            print value
            exit
        }
    ' "$manifest"
)"

if [ -z "$manifest_version" ]; then
    echo "verify-version: no version found in [workspace.package] of $manifest — nothing was checked." >&2
    exit "$EXIT_UNCHECKED"
fi

if [ "$tag" = "--print-version" ]; then
    printf '%s\n' "$manifest_version"
    exit 0
fi

# A tag is `vX.Y.Z`; the manifest carries the bare `X.Y.Z`.
tag_version="${tag#v}"

if [ "$tag_version" != "$manifest_version" ]; then
    echo "verify-version: MISMATCH — tag '$tag' declares version '$tag_version', but" >&2
    echo "                $manifest [workspace.package] version is '$manifest_version'." >&2
    echo "                Bump the workspace version to '$tag_version', or retag as 'v$manifest_version'." >&2
    exit "$EXIT_MISMATCH"
fi

echo "verify-version: MATCH — tag '$tag' and [workspace.package] both declare $manifest_version."
