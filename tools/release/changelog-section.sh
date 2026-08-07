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
#   0    PRINTED    a section was found; its body is on stdout.
#   65   UNPUBLISHABLE
#                   this script will not put these bytes on a release page, for
#                   one of two reasons. Either it cannot READ the file — an
#                   unbalanced code fence, or a `## ` heading its fence-aware
#                   scan cannot see, so nobody can say WHICH bytes it would
#                   publish. Or it can read them and they FORGE the release
#                   body's own structure (see the Checksums check below). Stdout
#                   is empty. The release workflow's `*)` arm stops the run.
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
#
# WHY 65 IS ITS OWN CODE, AND NOT FOLDED INTO 75. 75 makes the caller print
# "CHANGELOG.md has no version section to publish" and ship the release without
# an Upgrade notes section. That sentence has to be TRUE. A malformed fence in
# the preamble hides every heading from the scan below, which reaches exactly
# the same 75 — and then the release goes out with the disclosure silently
# absent, under a green build and a notice saying there was nothing to say. A
# silently-omitted upgrade note is the failure this script exists to prevent, so
# "I could not read it" gets a spelling of its own.
#
# The forgery half rides on 65 rather than on a third code because the caller
# does the same thing with both — stop, and say which script said so. What
# distinguishes them is the sentence on stderr, which is what an operator
# actually reads; a fourth number they would have to look up is not.

set -euo pipefail

EXIT_UNPUBLISHABLE=65
EXIT_NO_SECTION=75

script_dir="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)"
repo_root="$(cd -- "$script_dir/../.." && pwd)"

changelog="${1:-$repo_root/CHANGELOG.md}"

if [ ! -f "$changelog" ]; then
    echo "changelog-section: no changelog at $changelog — nothing to print." >&2
    exit "$EXIT_NO_SECTION"
fi

# --- is this file readable AS a changelog? --------------------------------
#
# Both checks below answer one question: can this script say which bytes it
# would publish? Neither asks whether those bytes are any good.

# An unbalanced fence makes every later `## ` invisible to the toggling scan
# below, and the damage runs in BOTH directions depending on where it opens:
#
#   - opened inside the topmost section -> the section never ends, and the run
#     publishes every past release's notes under one heading (over-publishing);
#   - opened in the preamble -> the topmost heading is swallowed, both `heading`
#     and `section` come back empty, and the run publishes NOTHING while the
#     caller reports "no version section" (under-publishing).
#
# The first is loud — the release page is visibly wrong. The second is silent
# and green, which is the one that ships a release with a privacy disclosure
# missing. There is no reading of an unbalanced fence under which this script
# knows what it is doing, so it stops on either. The same ERE as the scans
# below, so what is counted here is exactly what toggles there.
fence_lines="$(grep -c '^[[:space:]]*```' "$changelog" || true)"
if [ $((fence_lines % 2)) -ne 0 ]; then
    echo "changelog-section: $changelog has an unbalanced code fence ($fence_lines fence lines, an odd number) — this script cannot tell which bytes belong to the topmost section, so it is refusing to guess. Close the fence." >&2
    exit "$EXIT_UNPUBLISHABLE"
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

# Balanced fences, and still no heading — but the file plainly has one. That is
# a heading sitting INSIDE a fenced block (a quoted changelog, a diff hunk, a
# snippet), so the scan is right to skip it and wrong to report the file as
# section-less. "There are no version sections" and "there are version sections
# I cannot reach" are the same 75 to a caller that cannot tell them apart, and
# the second one publishes a release with its notes missing.
if [ -z "$heading" ] && grep -q '^## ' "$changelog"; then
    echo "changelog-section: $changelog has a '## ' heading that this script's fence-aware scan cannot see — every one of them is inside a code fence. Refusing to report a file with version sections as having none." >&2
    exit "$EXIT_UNPUBLISHABLE"
fi

# Fence tracking is not fussiness. A ``` block inside a section can contain a
# line starting with `## ` — a shell comment, a diff hunk, a snippet of another
# markdown file — and reading it as a heading would end the section early and
# publish HALF of whatever it says. A privacy disclosure truncated mid-sentence
# is worse than one that never rendered, and it would render green. The fence
# pattern allows leading whitespace because this repo's own changelog indents
# its fences inside list items, and a column-0-only match would not see them.
#
# This is fence TOGGLING, not CommonMark, so it can still be fooled by input a
# real parser would read differently — but not SILENTLY: the two balance checks
# above stop the run on the shapes where toggling and CommonMark disagree about
# where the section is. What is left is the residue a toggling scan gets right,
# and the fix for anything past it is a real markdown parser.
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

# Both of these are 75, and the second one deliberately so. A visible heading
# with an empty body, on a file whose fences balance, is not a parse failure —
# it is `## [Unreleased]` with nobody having written under it yet, which is the
# canonical "do not fail a release over a missing changelog entry" case this
# taxonomy was built for. The unreadable shapes were already turned away above,
# so what reaches here really is an empty section.
if [ -z "$section" ]; then
    if [ -z "$heading" ]; then
        echo "changelog-section: $changelog has no '## ' section — nothing to print." >&2
    else
        echo "changelog-section: the topmost section of $changelog ($heading) has no body — nothing to print." >&2
    fi
    exit "$EXIT_NO_SECTION"
fi

# --- the lifted section may not author the release body's own structure ----
#
# This text is printed VERBATIM into a body whose headings the workflow writes,
# and it lands immediately above `### Checksums (sha256)` and its fenced digest
# list. So a changelog section carrying its own `### Checksums (sha256)` heading
# and a fenced block renders a second, plausible digest list on the release
# page — above the real one, in the position a reader looks first.
#
# It is not a forged PASS: the artifacts are built by the workflow, the real
# list is still on the page below, and anyone who runs `sha256sum` gets the
# truth. It is confusion, and it is bought cheaply — a CHANGELOG diff is
# reviewed as prose, by people asking whether the wording is right, not by
# people asking what it will render as on a page they have not seen.
#
# WHY REFUSE RATHER THAN REORDER. Emitting the checksums first was the other
# option, and it is a weaker fix for a higher price. It would overturn a stated
# decision about where a reader meets upgrade notes (what an upgrade DOES to a
# running machine belongs before the digests, not after), and it would not
# actually remove the second list — it would put it underneath, where it is
# still a plausible digest list. What is wrong here is a lifted section
# authoring structure that belongs to the workflow; that is what this refuses,
# and refusing it costs a changelog nothing it has any reason to want. The real
# digests are generated per-release and cannot be known when the entry is
# written, so there is no honest changelog entry this rejects.
#
# Fence-aware, and only outside fences: a changelog QUOTING a release body
# inside a ``` block renders as code, impersonates no heading, and is fine.
# Case-insensitive and any heading depth, because what matters is what it
# renders as, not how carefully it was spelled.
forged_structure="$(
    printf '%s\n' "$section" | awk '
        /^[[:space:]]*```/ { fence = !fence; next }
        !fence && tolower($0) ~ /^#+[[:space:]]+checksums/ { print; exit }
    '
)"
if [ -n "$forged_structure" ]; then
    echo "changelog-section: the topmost section of $changelog ($heading) contains a heading that forges the release body's own structure: '$forged_structure'. The release page writes its own '### Checksums (sha256)' block from the digests this run computed; a section that renders a second one above it is not something this script will publish. Remove the heading, or put it inside a code fence if you meant to quote a release body." >&2
    exit "$EXIT_UNPUBLISHABLE"
fi

echo "changelog-section: taking the body of $heading from $changelog." >&2
printf '%s\n' "$section"
