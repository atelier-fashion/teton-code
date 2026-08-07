#!/usr/bin/env bash
#
# Print the BODY of the topmost version section of a Keep a Changelog file —
# the hand-written half of a release body.
#
# Usage:
#   tools/release/changelog-section.sh [changelog]
#
#   [changelog]  the changelog to read; defaults to this repo's CHANGELOG.md.
#
# "Topmost section" means: everything after the FIRST `## ` heading, up to the
# next one. The heading line itself is not printed. That is deliberate — the
# top section is `## [Unreleased]` right up until a release PR renames it, so
# printing the heading would put the word "Unreleased" on the release page of a
# tagged version roughly half the time. The caller supplies its own heading and
# this prints what is under it.
#
# The body is printed VERBATIM. No heading levels are shifted, no lines are
# rewritten. What a reader sees in CHANGELOG.md is byte-for-byte what the
# release page shows, so the pipeline has no way to alter a disclosure on its
# way out. The cost is that the section's own `### Changed` / `### Added`
# headings land at the same level as the caller's heading rather than nested
# under it; that is a cosmetic flattening, and it is the cheaper half of the
# trade.
#
# Exit codes are a taxonomy, not a boolean — the same stance verify-version.sh
# takes (LESSON-442, ADR-548-4). "There is nothing to print" and "this script
# is broken" must not share a spelling, because the caller wants to shrug at
# the first and stop at the second:
#
#   0   PRINTED     a section was found; its body is on stdout.
#   75  NO SECTION  there is nothing to print — no changelog at that path, or a
#                   changelog with no `## ` heading, or a topmost section whose
#                   body is empty. Stdout is empty and the reason is on stderr.
#                   This is NOT an error: a release must never fail because
#                   somebody did not write a changelog entry, and the caller in
#                   .github/workflows/release.yml treats it as "omit the
#                   section and carry on".
#
# Anything else is this script failing, not the changelog being absent, and the
# release workflow stops on it.

set -euo pipefail

EXIT_NO_SECTION=75

script_dir="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)"
repo_root="$(cd -- "$script_dir/../.." && pwd)"

changelog="${1:-$repo_root/CHANGELOG.md}"

if [ ! -f "$changelog" ]; then
    echo "changelog-section: no changelog at $changelog — nothing to print." >&2
    exit "$EXIT_NO_SECTION"
fi

# The heading this run took its body from, for the release log. An operator
# reading a release that shipped the wrong notes wants to see which section was
# picked without re-deriving it from the file.
heading="$(
    awk '
        /^[[:space:]]*```/ { fence = !fence; next }
        !fence && /^## / { print; exit }
    ' "$changelog"
)"

# Fence tracking is not fussiness. A ``` block inside a section can contain a
# line starting with `## ` — a shell comment, a diff hunk, a snippet of another
# markdown file — and reading it as a heading would end the section early and
# publish HALF of whatever it says. A privacy disclosure truncated mid-sentence
# is worse than one that never rendered, and it would render green. The fence
# pattern allows leading whitespace because this repo's own changelog indents
# its fences inside list items, and a column-0-only match would not see them.
#
# This is fence TOGGLING, not CommonMark: an UNCLOSED fence leaves every later
# `## ` invisible and publishes the rest of the file — every past release's
# notes under one heading. Known and left, because that failure is loud (the
# release page is visibly wrong) and its opposite is not (a section silently
# cut in half), and because the fix is a real markdown parser. If this ever
# happens, close the fence.
#
# Leading blank lines are dropped (they are layout under the heading, not
# content); trailing ones are dropped by the command substitution below.
section="$(
    awk '
        /^[[:space:]]*```/ { fence = !fence }
        !fence && /^## / {
            if (started) exit
            started = 1
            next
        }
        !started { next }
        !body && /^[[:space:]]*$/ { next }
        { body = 1; print }
    ' "$changelog"
)"

if [ -z "$section" ]; then
    if [ -z "$heading" ]; then
        echo "changelog-section: $changelog has no '## ' section — nothing to print." >&2
    else
        echo "changelog-section: the topmost section of $changelog ($heading) has no body — nothing to print." >&2
    fi
    exit "$EXIT_NO_SECTION"
fi

echo "changelog-section: taking the body of $heading from $changelog." >&2
printf '%s\n' "$section"
