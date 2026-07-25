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
#   64  EX_USAGE — wrong invocation.
#   70  EX_SOFTWARE — the build reported success but an expected binary is not
#       where it should be. That is an internal inconsistency, not a compile
#       error, and it is worth its own code so it cannot be read as one.
#   *   whatever `cargo build` exited with, propagated unchanged.

set -euo pipefail

EXIT_USAGE=64
EXIT_INTERNAL=70

if [ "$#" -lt 2 ]; then
    echo "usage: package.sh <target> <version> [outdir]" >&2
    exit "$EXIT_USAGE"
fi

target="$1"
version="${2#v}"

script_dir="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)"
repo_root="$(cd -- "$script_dir/../.." && pwd)"

outdir="${3:-$repo_root/dist}"
mkdir -p "$outdir"
outdir="$(cd -- "$outdir" && pwd)"

# Hash with whatever the platform ships: macOS has `shasum`, Linux has
# `sha256sum`, and openssl is the last resort. The output format is normalised
# to `<sha256>  <name>`, which both `shasum -a 256 -c` and `sha256sum -c` read.
sha256_of() {
    if command -v shasum >/dev/null 2>&1; then
        shasum -a 256 "$1" | awk '{ print $1 }'
    elif command -v sha256sum >/dev/null 2>&1; then
        sha256sum "$1" | awk '{ print $1 }'
    else
        openssl dgst -sha256 "$1" | awk '{ print $NF }'
    fi
}

echo "package: building $target (release, --features tetond/llama)"
cargo build --release --workspace --target "$target" --features tetond/llama

bin_dir="${CARGO_TARGET_DIR:-$repo_root/target}/$target/release"

stage="$(mktemp -d "${TMPDIR:-/tmp}/teton-package.XXXXXX")"
trap 'rm -rf "$stage"' EXIT

for bin in teton tetond; do
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
# the tarball is flat — `teton`, `tetond`, `LICENSE`, `README.md` at the root.
# COPYFILE_DISABLE stops macOS's bsdtar from smuggling `._*` AppleDouble xattr
# members in beside them; it is meaningless, and harmless, on Linux.
COPYFILE_DISABLE=1 tar -czf "$tarball" -C "$stage" teton tetond LICENSE README.md

# A per-leg sidecar, for this job's log and for anyone re-checking one artifact
# by hand. It is NOT what ends up in the release: `checksums.txt` is recomputed
# in the release job from the files actually uploaded (BR-5), so a hash can
# never describe bytes other than the ones being published.
sha="$(sha256_of "$tarball")"
printf '%s  %s\n' "$sha" "$(basename "$tarball")" >"$tarball.sha256"

echo "package: built $(basename "$tarball")"
echo "package: sha256 $sha"
printf '%s\n' "$tarball"
