#!/usr/bin/env bash
#
# Build one release target and assemble its distributable tarball.
#
# Usage: tools/release/package.sh <target> <version> [outdir]
#
#   <target>   Rust target triple, e.g. `aarch64-apple-darwin`.
#   <version>  the release version, with or without a leading `v`. The tarball
#              is named `teton-v<version>-<target>.tar.gz`.
#   [outdir]   where the tarball lands; defaults to `<repo>/dist`.
#
# The build always carries `--features tetond/llama`. The local inference engine
# is the product this release exists to install: a tarball built without it
# would ship a daemon that cannot serve the model the installer just told the
# user about, and that failure would surface on the user's machine rather than
# here. llama.cpp is compiled from source, so cmake must be on PATH (it is, on
# GitHub hosted runners).
#
# `--target` is passed on every leg, native ones included, so the build output
# always lands at `target/<triple>/release/` and packaging has no per-leg
# special case to get wrong.
#
# Cross-compiling `x86_64-apple-darwin` on an arm64 macOS runner (ADR-548-2)
# additionally needs `CMAKE_OSX_ARCHITECTURES=x86_64` exported by the caller so
# llama.cpp's own cmake build produces objects for the same architecture Rust is
# targeting; a mismatch fails at link time, in this job, which is the loud place
# for it to fail.
#
# Exit codes:
#   0   the tarball was built and its sha256 recorded.
#   64  EX_USAGE — wrong invocation, or an argument that is not what it claims
#       to be: `version` is pasted into a filesystem path and `target` is handed
#       to `cargo build --target`, so neither is free text.
#   70  EX_SOFTWARE — the build reported success but an expected binary is not
#       where it should be. That is an internal inconsistency, not a compile
#       error, and it is worth its own code so it cannot be read as one.
#   75  EX_TEMPFAIL — the tarball was built but could not be hashed, because the
#       machine has no sha256 tool at all. Distinct from 70 for the same reason
#       the rest of tools/release/ separates them (LESSON-442): "could not run"
#       is not "ran and found a problem".
#   *   whatever `cargo build` exited with, propagated unchanged.

set -euo pipefail

EXIT_USAGE=64
EXIT_INTERNAL=70
EXIT_UNCHECKED=75

script_dir="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)"
repo_root="$(cd -- "$script_dir/../.." && pwd)"

# `source=` lets `shellcheck -x` follow this; `disable=SC1091` keeps a bare
# `shellcheck <file>` — which cannot follow it — from failing on the info.
# shellcheck source=tools/release/lib.sh disable=SC1091
. "$script_dir/lib.sh"

if [ "$#" -lt 2 ]; then
    echo "usage: package.sh <target> <version> [outdir]" >&2
    exit "$EXIT_USAGE"
fi

target="$1"
version="${2#v}"

# Both arguments are validated before anything is built, because both leave this
# script: `version` becomes part of a path this script creates and writes, and
# `target` is passed to `cargo build --target`. This was the only script in
# tools/release/ that took its inputs on trust.
if ! is_release_version "$version"; then
    echo "package: <version> must be a release version X.Y.Z (a leading 'v' is fine); got '$2'." >&2
    exit "$EXIT_USAGE"
fi

# An allowlist, not a pattern: these are exactly the three targets the release
# builds (ADR-548-2), and a typo'd fourth should stop here rather than after a
# long cargo failure — or, worse, produce a tarball named for a platform the
# formula will never point at.
case "$target" in
    aarch64-apple-darwin | x86_64-apple-darwin | x86_64-unknown-linux-gnu) ;;
    *)
        echo "package: <target> must be one of aarch64-apple-darwin, x86_64-apple-darwin," >&2
        echo "         x86_64-unknown-linux-gnu; got '$target'." >&2
        exit "$EXIT_USAGE"
        ;;
esac

outdir="${3:-$repo_root/dist}"
mkdir -p "$outdir"
outdir="$(cd -- "$outdir" && pwd)"

echo "package: building $target (release, --features tetond/llama)"
cargo build --release --workspace --target "$target" --features tetond/llama

bin_dir="${CARGO_TARGET_DIR:-$repo_root/target}/$target/release"

stage="$(mktemp -d "${TMPDIR:-/tmp}/teton-package.XXXXXX")"
trap 'rm -rf "$stage"' EXIT

for bin in teton teton-code; do
    if [ ! -x "$bin_dir/$bin" ]; then
        echo "package: cargo reported success but $bin_dir/$bin is missing or not executable." >&2
        exit "$EXIT_INTERNAL"
    fi
    cp "$bin_dir/$bin" "$stage/$bin"
done

# The licence and readme ride along so an unpacked tarball is self-describing
# even for someone who never visits the repo (and so Homebrew's `bin.install`
# has a licence sitting next to the binaries it installs).
cp "$repo_root/LICENSE" "$stage/LICENSE"
cp "$repo_root/README.md" "$stage/README.md"

tarball="$outdir/teton-v$version-$target.tar.gz"

# Members are listed explicitly rather than archiving `.`: it fixes the entry
# order and keeps the `./` prefix (and any stray dotfile) out of the archive, so
# the tarball is flat — `teton`, `teton-code`, `LICENSE`, `README.md` at the root.
# COPYFILE_DISABLE stops macOS's bsdtar from smuggling `._*` AppleDouble xattr
# members in beside them; it is meaningless, and harmless, on Linux.
COPYFILE_DISABLE=1 tar -czf "$tarball" -C "$stage" teton teton-code LICENSE README.md

# A per-leg sidecar, for this job's log and for anyone re-checking one artifact
# by hand. It is NOT what ends up in the release: `checksums.txt` is recomputed
# in the release job from the files actually uploaded (BR-5), so a hash can
# never describe bytes other than the ones being published.
#
# The sidecar's format is `<sha256>  <name>`, which both `shasum -a 256 -c` and
# `sha256sum -c` read.
if ! sha="$(sha256_of "$tarball")"; then
    echo "package: no sha256 tool (shasum, sha256sum, openssl) on PATH." >&2
    echo "         $(basename "$tarball") was built but has no recorded hash." >&2
    exit "$EXIT_UNCHECKED"
fi
printf '%s  %s\n' "$sha" "$(basename "$tarball")" >"$tarball.sha256"

echo "package: built $(basename "$tarball")"
echo "package: sha256 $sha"
printf '%s\n' "$tarball"
