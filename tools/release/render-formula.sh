#!/usr/bin/env bash
#
# Render the Homebrew formula from `packaging/homebrew/teton.rb.tmpl` (ADR-548-1).
#
# Usage:
#   render-formula.sh --version <X.Y.Z> --artifacts <dir> [options]
#   render-formula.sh --version <X.Y.Z> \
#       --sha-aarch64-apple-darwin      <sha256> \
#       --sha-x86_64-apple-darwin       <sha256> \
#       --sha-x86_64-unknown-linux-gnu  <sha256> [options]
#
# Options:
#   --output <path>    where the rendered formula lands; default stdout.
#   --template <path>  default `packaging/homebrew/teton.rb.tmpl` in this repo.
#   --base-url <url>   release download root; default this repo's releases.
#
# Two input modes, one renderer. `--artifacts` points at a directory of release
# tarballs and the script hashes them itself; that is the mode the release
# workflow uses, because a sha it computed from the bytes cannot be a sha that
# belongs to some other bytes (BR-5). The explicit `--sha-<target>` mode exists
# for hand-rendering and for testing this script — the flags are keyed by target
# triple rather than positional so a hash can never be filed under the wrong
# platform, which is the one substitution error `brew install` would surface as
# a checksum failure on a user's machine rather than here.
#
# Every diagnostic goes to stderr, so `--output` may be omitted and the formula
# piped straight into `ruby -c`.
#
# Exit codes are a taxonomy, matching the rest of tools/release/ (LESSON-442):
#
#   0   RENDERED   every placeholder was filled; the formula was written.
#   64  EX_USAGE   the check RAN and the inputs are wrong: bad invocation, a
#                  malformed version or sha256, or — after substitution — a
#                  `{{PLACEHOLDER}}` the caller never supplied a value for.
#                  That last case is template/script drift: the template grew a
#                  slot this script does not know how to fill, and a formula
#                  carrying a literal `{{...}}` must never reach the tap.
#   75  UNCHECKED  the render could NOT be attempted (template missing, an
#                  artifact absent, no sha256 tool). Nothing was rendered, which
#                  is not the same as nothing being wrong with the inputs.

set -euo pipefail

EXIT_USAGE=64
EXIT_UNCHECKED=75

script_dir="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)"
repo_root="$(cd -- "$script_dir/../.." && pwd)"

template="$repo_root/packaging/homebrew/teton.rb.tmpl"
base_url="https://github.com/atelier-fashion/teton-code/releases/download"
version=""
artifacts=""
output=""
sha_arm64_darwin=""
sha_x64_darwin=""
sha_x64_linux=""

usage() {
    cat >&2 <<'EOF'
usage: render-formula.sh --version <X.Y.Z> --artifacts <dir> [options]
       render-formula.sh --version <X.Y.Z> \
           --sha-aarch64-apple-darwin <sha256> \
           --sha-x86_64-apple-darwin <sha256> \
           --sha-x86_64-unknown-linux-gnu <sha256> [options]

options: --output <path>  --template <path>  --base-url <url>
EOF
}

# `shift 2` on a flag whose value is missing would abort under `set -e` with a
# bare exit 1 — the generic code this taxonomy exists to avoid. Check first.
need_value() {
    if [ "$2" -lt 2 ]; then
        echo "render-formula: $1 needs a value." >&2
        usage
        exit "$EXIT_USAGE"
    fi
}

while [ "$#" -gt 0 ]; do
    case "$1" in
        --version)
            need_value "$1" "$#"
            version="${2#v}"
            shift 2
            ;;
        --artifacts)
            need_value "$1" "$#"
            artifacts="$2"
            shift 2
            ;;
        --output)
            need_value "$1" "$#"
            output="$2"
            shift 2
            ;;
        --template)
            need_value "$1" "$#"
            template="$2"
            shift 2
            ;;
        --base-url)
            need_value "$1" "$#"
            base_url="${2%/}"
            shift 2
            ;;
        --sha-aarch64-apple-darwin)
            need_value "$1" "$#"
            sha_arm64_darwin="$2"
            shift 2
            ;;
        --sha-x86_64-apple-darwin)
            need_value "$1" "$#"
            sha_x64_darwin="$2"
            shift 2
            ;;
        --sha-x86_64-unknown-linux-gnu)
            need_value "$1" "$#"
            sha_x64_linux="$2"
            shift 2
            ;;
        -h | --help)
            usage
            exit 0
            ;;
        *)
            echo "render-formula: unknown argument '$1'." >&2
            usage
            exit "$EXIT_USAGE"
            ;;
    esac
done

# The version is interpolated into a Ruby string literal and into three URLs.
# Pinning its shape keeps both honest, and rejects the empty string that a
# `--version ""` or an unset workflow output would otherwise render as
# `version ""` — a formula that parses, installs nothing, and reads as fine.
if ! printf '%s' "$version" | grep -Eq '^[0-9]+\.[0-9]+\.[0-9]+([-+][0-9A-Za-z.-]+)?$'; then
    echo "render-formula: --version must be X.Y.Z (optionally with a -pre/+build suffix); got '$version'." >&2
    exit "$EXIT_USAGE"
fi

if [ ! -f "$template" ]; then
    echo "render-formula: template not found: $template — nothing was rendered." >&2
    exit "$EXIT_UNCHECKED"
fi

# Hash with whatever the platform ships — macOS has `shasum`, Linux has
# `sha256sum` — the same fallback chain package.sh uses to record the per-leg
# sidecar hashes.
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

tarball_name() {
    printf 'teton-v%s-%s.tar.gz' "$version" "$1"
}

if [ -n "$artifacts" ]; then
    if [ -n "$sha_arm64_darwin$sha_x64_darwin$sha_x64_linux" ]; then
        echo "render-formula: --artifacts and --sha-<target> are two ways to answer the same question; pass one." >&2
        exit "$EXIT_USAGE"
    fi

    if [ ! -d "$artifacts" ]; then
        echo "render-formula: artifact directory not found: $artifacts — nothing was hashed." >&2
        exit "$EXIT_UNCHECKED"
    fi

    for target in aarch64-apple-darwin x86_64-apple-darwin x86_64-unknown-linux-gnu; do
        tarball="$artifacts/$(tarball_name "$target")"
        if [ ! -f "$tarball" ]; then
            echo "render-formula: $tarball is missing, so its sha256 could not be computed." >&2
            echo "                A formula is rendered from all three targets or from none." >&2
            exit "$EXIT_UNCHECKED"
        fi

        if ! sha="$(sha256_of "$tarball")"; then
            echo "render-formula: no sha256 tool (shasum, sha256sum, openssl) on PATH — nothing was hashed." >&2
            exit "$EXIT_UNCHECKED"
        fi

        case "$target" in
            aarch64-apple-darwin) sha_arm64_darwin="$sha" ;;
            x86_64-apple-darwin) sha_x64_darwin="$sha" ;;
            x86_64-unknown-linux-gnu) sha_x64_linux="$sha" ;;
        esac
    done
fi

# Checked for every mode, including the one that just computed them: a hash
# tool that printed a warning instead of a digest should stop here, not sail
# into the template.
check_sha() {
    if [ -z "$2" ]; then
        echo "render-formula: no sha256 for $1 — pass --sha-$1 or --artifacts <dir>." >&2
        exit "$EXIT_USAGE"
    fi
    if ! printf '%s' "$2" | grep -Eq '^[0-9a-f]{64}$'; then
        echo "render-formula: sha256 for $1 is not 64 lowercase hex characters; got '$2'." >&2
        exit "$EXIT_USAGE"
    fi
}

check_sha aarch64-apple-darwin "$sha_arm64_darwin"
check_sha x86_64-apple-darwin "$sha_x64_darwin"
check_sha x86_64-unknown-linux-gnu "$sha_x64_linux"

url_arm64_darwin="$base_url/v$version/$(tarball_name aarch64-apple-darwin)"
url_x64_darwin="$base_url/v$version/$(tarball_name x86_64-apple-darwin)"
url_x64_linux="$base_url/v$version/$(tarball_name x86_64-unknown-linux-gnu)"

# Bash parameter substitution rather than sed: the values carry `/`, `:` and `.`,
# and picking a sed delimiter that none of them can ever contain is a bet this
# does not need to make.
rendered="$(cat "$template")"
rendered="${rendered//\{\{VERSION\}\}/$version}"
rendered="${rendered//\{\{URL_ARM64_DARWIN\}\}/$url_arm64_darwin}"
rendered="${rendered//\{\{SHA_ARM64_DARWIN\}\}/$sha_arm64_darwin}"
rendered="${rendered//\{\{URL_X64_DARWIN\}\}/$url_x64_darwin}"
rendered="${rendered//\{\{SHA_X64_DARWIN\}\}/$sha_x64_darwin}"
rendered="${rendered//\{\{URL_X64_LINUX\}\}/$url_x64_linux}"
rendered="${rendered//\{\{SHA_X64_LINUX\}\}/$sha_x64_linux}"

leftover="$(printf '%s\n' "$rendered" | grep -o '{{[^}]*}}' | sort -u | tr '\n' ' ' || true)"
if [ -n "${leftover// /}" ]; then
    echo "render-formula: unfilled placeholder(s) after substitution: ${leftover% }" >&2
    echo "                $template declares a slot this script does not fill. Teach" >&2
    echo "                render-formula.sh about it — a formula carrying a literal" >&2
    echo "                {{...}} must never reach the tap." >&2
    exit "$EXIT_USAGE"
fi

if [ -n "$output" ]; then
    mkdir -p "$(dirname -- "$output")"
    printf '%s\n' "$rendered" >"$output"
else
    printf '%s\n' "$rendered"
fi

{
    echo "render-formula: rendered teton $version${output:+ -> $output}"
    echo "  aarch64-apple-darwin      $sha_arm64_darwin  $url_arm64_darwin"
    echo "  x86_64-apple-darwin       $sha_x64_darwin  $url_x64_darwin"
    echo "  x86_64-unknown-linux-gnu  $sha_x64_linux  $url_x64_linux"
} >&2
