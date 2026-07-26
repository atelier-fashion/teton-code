#!/usr/bin/env bash
#
# Render the tetoncode.ai landing page from site/index.html.
#
#   usage: bash site/render.sh <version> [install-command]
#
# Stamps {{VERSION}} and {{INSTALL_COMMAND}} into site/dist/index.html. The
# version comes from the release tag (REQ-548 BR-8: the page derives from the
# release source of truth, never hand-edited copy that can drift). A leading
# "v" is stripped, so both `v0.1.0` and `0.1.0` are accepted — the template
# supplies the "v" where it prints one.
#
# Standalone by design: it sources nothing, so the deploy workflow can run it
# from a checkout that carries only the site tree. The cost of that is the
# duplicated version check below; see the comment on it.
#
# Exit codes (ADR-548-4 taxonomy — a distinct code, never a bare 1, so an
# infrastructure failure cannot masquerade as an unfilled render or vice versa):
#
#   0   rendered; no placeholder left behind
#   64  EX_USAGE — the render RAN and the INPUTS are wrong: missing or malformed
#       argument, an unsubstitutable value, or a `{{` placeholder still present
#       in the output.
#   75  EX_TEMPFAIL — the render COULD NOT be attempted: the template is not
#       there. That is an infrastructure failure (a wrong checkout, a partial
#       clone), not bad input, and reporting it as 64 sent a deploy operator
#       looking at the version argument for a problem that is nowhere near it
#       (LESSON-442).
#
# Refusing on a surviving placeholder is the same refuse-on-unfilled shape the
# formula render uses: a page that ships "{{VERSION}}" to users is worse than a
# deploy that fails loudly.

set -euo pipefail

readonly EXIT_OK=0
readonly EXIT_USAGE=64     # EX_USAGE
readonly EXIT_UNCHECKED=75 # EX_TEMPFAIL

readonly DEFAULT_INSTALL_COMMAND='brew install atelier-fashion/tap/teton'

script_dir="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
readonly TEMPLATE="${script_dir}/index.html"
readonly OUT_DIR="${script_dir}/dist"
readonly OUT_FILE="${OUT_DIR}/index.html"

die() {
    printf 'error: %s\n' "$1" >&2
    exit "$EXIT_USAGE"
}

# For the could-not-run paths, which are not the caller's fault and must not be
# reported as though they were.
die_unchecked() {
    printf 'error: %s\n' "$1" >&2
    exit "$EXIT_UNCHECKED"
}

if [ "$#" -lt 1 ]; then
    printf 'usage: %s <version> [install-command]\n' "$0" >&2
    die 'no version given; the version must come from the release tag'
fi

version="${1#v}"
install_command="${2:-$DEFAULT_INSTALL_COMMAND}"

# A version is what the release tag says, not free text: an empty, malformed, or
# multi-line value would otherwise render a page claiming release "v". The case
# guard runs first because it also rejects embedded newlines, which a
# line-oriented grep would happily match around.
#
# Strictly X.Y.Z — a `-rc.1` or `+build` suffix is refused, not rendered. This
# regex is a COPY. tools/release/lib.sh's `is_release_version` is the authority
# and carries the reasoning; this script cannot source it, because the site
# deploy workflow runs from a checkout that need not contain tools/. Change both
# together, or the landing page and the release will disagree about what a
# version is — which is exactly the state this comment was written to end.
case "$version" in
*[!0-9.]* | '')
    die "version '$1' is not a release version (expected X.Y.Z or vX.Y.Z)"
    ;;
esac
if ! printf '%s' "$version" | grep -Eq '^[0-9]+\.[0-9]+\.[0-9]+$'; then
    die "version '$1' is not a release version (expected X.Y.Z or vX.Y.Z)"
fi

# Substituted values land inside an HTML text node unescaped, so allow only
# characters a brew invocation actually needs; that rejects markup and newlines
# in one guard rather than emitting invalid HTML.
case "$install_command" in
*[!0-9A-Za-z./:@_+\ -]* | '')
    die "install command '$install_command' has characters outside the allowed set (letters, digits, space, . / : @ _ + -)"
    ;;
esac

[ -f "$TEMPLATE" ] || die_unchecked "template not found: $TEMPLATE — nothing was rendered"

template="$(cat "$TEMPLATE")"

# Pure-bash substitution: the install command contains '/', which makes sed
# delimiters a footgun, and quoting the pattern keeps it a literal (no globs).
readonly PH_VERSION='{{VERSION}}'
readonly PH_INSTALL='{{INSTALL_COMMAND}}'
rendered="${template//"$PH_VERSION"/$version}"
rendered="${rendered//"$PH_INSTALL"/$install_command}"

mkdir -p "$OUT_DIR"
printf '%s\n' "$rendered" >"$OUT_FILE"

# Refuse on unfilled: any surviving placeholder means the template grew a
# variable this script does not know how to fill.
if grep -Fq '{{' "$OUT_FILE"; then
    leftovers="$(grep -Fno '{{' "$OUT_FILE" | cut -d: -f1 | sort -un | head -5 | paste -sd, -)"
    rm -f "$OUT_FILE"
    die "unfilled placeholder(s) remain after render (template line(s): ${leftovers}); refusing to publish"
fi

printf 'rendered %s (version %s, install: %s)\n' "$OUT_FILE" "$version" "$install_command"
exit "$EXIT_OK"
