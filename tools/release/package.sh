#!/usr/bin/env bash
#
# Build one release target and assemble its distributable tarball.
#
# Usage: tools/release/package.sh <target> <version> [outdir] [phase]
#
#   <target>   Rust target triple, e.g. `aarch64-apple-darwin`.
#   <version>  the release version, with or without a leading `v`. The tarball
#              is named `teton-v<version>-<target>.tar.gz`.
#   [outdir]   where the tarball lands; defaults to `<repo>/dist`.
#   [phase]    `all` (the default), `build` or `pack` — see below.
#
# THE PHASES (ADR-551-1). `all` is everything this script has ever done, and it
# is still what a human and the runbook type. `build` and `pack` are that same
# work cut at one seam, so the release workflow can put a step BETWEEN them:
#
#   build   validate the arguments, `cargo build`, and stage the two binaries
#           plus LICENSE and README.md into `<outdir>/stage-<target>/`. It
#           resolves no signing tool, invokes none, and never reads
#           TETON_SIGN_IDENTITY — that is the whole point of the split. This
#           phase spends ~30 minutes compiling third-party sources (llama.cpp),
#           and REQ-551 exists so that no signing identity is on the machine
#           while it does (BR-1).
#   pack    sign and verify the staged pair, tar it, hash it. It builds
#           nothing, and it packs only what a build phase left behind: a
#           missing, incomplete or ALTERED staging directory is a hard 70, so a
#           `pack` that followed no `build` can never emit a tarball (the
#           cross-boundary half of BR-2).
#   all     `build` then `pack`, in one process — ON THE SUCCESS PATH, byte for
#           byte what this script printed and produced before the phase
#           argument existed. That equivalence is about a run that works: the
#           refusals the split added (a stage that is missing, short a member,
#           built for another version, or altered since it was built) are new
#           exits that the pre-split script had no way to reach, because it
#           never crossed a process boundary to reach them from.
#
# The staging directory is DETERMINISTIC — `<outdir>/stage-<target>/`, derived
# from arguments both invocations already pass — because the two phases are
# separate processes with no other way to agree on a path; the mktemp directory
# it replaces could not survive the one boundary it now has to cross. `build`
# creates it FRESH BEFORE IT COMPILES (see build_phase: the clear is a
# precondition, not tidying-up), and a successful `pack` CONSUMES it, which is
# why `all` still leaves nothing behind. A FAILED pack leaves it in place on
# purpose: the bytes are already compiled, the failure is a signing problem to
# fix and re-`pack`, and there is no tarball or sha256 for a leftover directory
# to be mistaken for. The price of a deterministic name is that two packages of
# the SAME target into the SAME outdir must not run at once — the release runs
# one leg per target per runner, and a human would have to arrange that
# collision on purpose.
#
# What crosses that boundary is a directory on disk, and a directory on disk is
# not self-authenticating: between the two invocations sits a step that imports
# a Developer ID private key, and after it anything that can write to `<outdir>`
# can choose what gets signed. So `build` records what it staged in
# `<stage>/.stage-meta` — the version it was building and a sha256 per member —
# and `pack` re-hashes every member and refuses the lot on any mismatch, BEFORE
# it signs. The bytes that get a release signature are then the bytes the build
# produced, or nothing is signed at all. `.stage-meta` is the handoff's own
# bookkeeping and never ships: the tarball's members are listed explicitly, and
# that list is exactly the four the build staged.
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
# Signing is REQUESTED by the environment, never inferred from it (ADR-550-1).
# With `TETON_SIGN_IDENTITY` set, both staged binaries are codesigned and
# verified before the tarball is written, and any failure ends the leg; without
# it they stay as the linker left them and the script says so. A dev build is
# not a release, and a release that could not sign is not a release either. All
# of that is the PACK phase's, and only the pack phase's.
#
# Exit codes:
#   0   the tarball was built and its sha256 recorded — or, for `build`, the
#       binaries were compiled and staged for a later `pack`.
#   64  EX_USAGE — wrong invocation, or an argument that is not what it claims
#       to be: `version` is pasted into a filesystem path, `target` is handed
#       to `cargo build --target`, and `phase` selects which half of this
#       script runs, so none of the three is free text.
#   70  EX_SOFTWARE — the build reported success but an expected binary is not
#       where it should be, signing was asked for and could not be carried out,
#       or the pack phase found no complete, matching staging directory to pack
#       (absent, short a member, built for a different version, or altered since
#       it was built). All are internal inconsistencies rather than compile
#       errors, and they are worth their own code so none can be read as one.
#   75  EX_TEMPFAIL — a sha256 could not be computed, because the machine has no
#       sha256 tool at all: `build` cannot record what it staged, or `pack`
#       cannot check it, or the finished tarball cannot be hashed. Distinct from
#       70 for the same reason the rest of tools/release/ separates them
#       (LESSON-442): "could not run" is not "ran and found a problem". No
#       shippable-looking artifact survives any of the three — the tarball is
#       REMOVED before this exit, because a tarball with no recorded hash beside
#       it is indistinguishable, to everything downstream, from one nobody
#       hashed on purpose.
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
    echo "usage: package.sh <target> <version> [outdir] [phase]" >&2
    exit "$EXIT_USAGE"
fi

# A fifth argument is REFUSED rather than ignored. Everything this script does
# is decided by the four above, so a fifth is a caller believing in a flag that
# does not exist — a `--sign` or a `--no-verify` that would silently not happen
# and leave them reading the wrong story out of a green step.
if [ "$#" -gt 4 ]; then
    echo "usage: package.sh <target> <version> [outdir] [phase]" >&2
    echo "package: $# arguments were given; this script takes at most four and ignores none." >&2
    exit "$EXIT_USAGE"
fi

# The collision the phase argument created, and the one mistake this script's
# own usage line invites: the phase is the FOURTH argument, so a three-argument
# `package.sh <target> <version> pack` is not a pack — it is an `all` writing a
# release into a directory named `pack`. That run compiles AND signs in one
# step, on a machine that by then has a Developer ID key on it, which is exactly
# the ordering REQ-551 exists to remove; and it would exit 0 while doing it.
# There is no way to tell the two intentions apart from here, so the ambiguous
# one is refused. Anyone who genuinely wants an output directory with one of
# these three names can say `./pack`.
case "${3:-}" in
    all | build | pack)
        echo "package: '$3' is a PHASE name, and the phase is the fourth argument — the third" >&2
        echo "         is [outdir]. Taken literally this would write the release into a directory" >&2
        echo "         named '$3' and then run the default phase 'all', compiling and signing in" >&2
        echo "         one step. Refusing rather than guessing which you meant." >&2
        echo "         did you mean: package.sh <target> <version> <outdir> $3" >&2
        exit "$EXIT_USAGE"
        ;;
esac

target="$1"
version="${2#v}"
phase="${4:-all}"

# Settled before anything else, because the phase decides which of the checks
# below even apply: the seam guard directly under this one belongs to `pack`
# and `all`, and the signing-tool resolution after it belongs to them too.
case "$phase" in
    all | build | pack) ;;
    *)
        echo "package: [phase] must be one of all, build, pack; got '$phase'." >&2
        exit "$EXIT_USAGE"
        ;;
esac

# The seam is a TEST seam, and it is refused where tests do not run — the same
# guard verify-signature.sh and verify-attestation.sh carry, applied here for a
# sharper reason than either. Those two report on bytes somebody else made;
# `TETON_CODESIGN` here names the program that SIGNS the bytes this script is
# about to publish, so an unexpected value of it in CI does not soften a verdict
# — it decides what a release signature is. The release workflow sets neither
# variable, so one appearing on a GitHub Actions run is a mistake or an attack.
# selftest.sh sets TETON_ALLOW_TOOL_SEAM=1 once, at the top, to say it meant it.
#
# Asked of `pack` and `all` only. The build phase resolves no signing tool and
# reads TETON_CODESIGN nowhere, so refusing there would be a guard standing over
# a variable that cannot reach anything — which teaches the next reader the
# wrong thing about what this guard is for. The pack phase, in the same job on
# the same runner, still refuses, and that is where the variable would have
# decided something.
if [ "$phase" != build ] &&
    [ "${GITHUB_ACTIONS:-}" = "true" ] &&
    [ -n "${TETON_CODESIGN:-}" ] &&
    [ "${TETON_ALLOW_TOOL_SEAM:-}" != "1" ]; then
    echo "package: TETON_CODESIGN is set inside GitHub Actions, and TETON_ALLOW_TOOL_SEAM=1 is not" >&2
    echo "         — refusing to build. That variable replaces the program that signs the shipped" >&2
    echo "         binaries, so honouring it here would let an environment variable decide what a" >&2
    echo "         release signature is. No tarball is written." >&2
    exit "$EXIT_USAGE"
fi

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

# Whether this run CAN sign is settled before anything is built, for the reason
# the target allowlist is: a signing-requested `all` on a machine without
# codesign is going to die either way, and dying after a from-source llama.cpp
# compile only makes the same answer slower and dearer to arrive at. Resolved
# here rather than inside pack_phase precisely so that `all` asks it first —
# for a bare `pack` the two positions are the same position, since nothing runs
# before it.
#
# NOT asked when the phase is `build`. A build that resolved the signing tool
# would fail on a machine that has no identity to sign with, which is the
# machine REQ-551 wants the long compile to happen on (BR-1).
#
# The predicate is the REQUEST, never the availability of a certificate. The
# release workflow sets TETON_SIGN_IDENTITY unconditionally on both macOS legs
# (ADR-550-1), so a leg that cannot sign has to die rather than quietly write a
# tarball that looks like every other one (BR-2). A guard that switches itself
# off when its tool is missing is not a guard (LESSON-443) — which is why an
# absent codesign is 70 here and not the 75 the verify gates spell it: those
# gates report on bytes somebody else made, while this script is the one making
# them, and "could not check" and "did not do it" are different failures.
#
# `TETON_CODESIGN` is the same seam verify-signature.sh carries, for the same
# reason: it changes WHICH tool is asked, never how the answer is scored.
codesign_tool=""
if [ "$phase" != build ] && [ -n "${TETON_SIGN_IDENTITY:-}" ]; then
    if ! codesign_tool="$(tool_or_unchecked "${TETON_CODESIGN:-}" codesign)"; then
        echo "package: signing was requested (TETON_SIGN_IDENTITY is set) but '${TETON_CODESIGN:-codesign}'" >&2
        echo "         is not on this machine. Refusing to build a tarball that would be unsigned" >&2
        echo "         and indistinguishable from a signed one." >&2
        exit "$EXIT_INTERNAL"
    fi
fi

outdir="${3:-$repo_root/dist}"
mkdir -p "$outdir"
outdir="$(cd -- "$outdir" && pwd)"

# The one name both phases compute identically, from arguments they both
# already carry. Nothing is passed between two separate invocations except
# these bytes at this path.
stage="$outdir/stage-$target"

# Progress lines that belong to a SPLIT invocation only. `all` must print
# exactly what it printed before the phase argument existed — the runbook
# quotes those lines and the selftest reads them — so the two hand-off
# announcements say nothing there. In a split run they are not decoration:
# `build` and `pack` are separate steps in a workflow log, and each one needs
# to name the directory the other depends on.
phase_note() {
    if [ "$phase" != all ]; then
        printf 'package: %s\n' "$1"
    fi
}

build_phase() {
    local bin bin_dir member digest

    # FIRST, before the compiler runs — the clear is a PRECONDITION of this
    # phase, not tidying-up at the end of it.
    #
    # Cleared, not merged into: a staging directory left by an earlier run of a
    # different commit would otherwise contribute whichever member this build
    # fails to overwrite, and the tarball would carry bytes nobody just built.
    # Doing it after `cargo build` left a window where that was not merely a
    # theory — a compile that FAILS exits this script with the previous run's
    # stage still sitting there, complete and plausible, and the `pack` step
    # that follows finds four members, a manifest, and nothing to object to.
    # The build failing is precisely when there must be no stage to pack.
    rm -rf "$stage"
    mkdir -p "$stage"

    echo "package: building $target (release, --features tetond/llama)"
    cargo build --release --workspace --target "$target" --features tetond/llama

    bin_dir="${CARGO_TARGET_DIR:-$repo_root/target}/$target/release"

    for bin in teton teton-code; do
        if [ ! -x "$bin_dir/$bin" ]; then
            echo "package: cargo reported success but $bin_dir/$bin is missing or not executable." >&2
            exit "$EXIT_INTERNAL"
        fi
        cp "$bin_dir/$bin" "$stage/$bin"
    done

    # The licence and readme ride along so an unpacked tarball is self-describing
    # even for someone who never visits the repo (and so Homebrew's `bin.install`
    # has a licence sitting next to the binaries it installs). Staged here rather
    # than at tar time because the staging directory is the phase boundary: what
    # `pack` ships is what it finds, and it invents no members of its own.
    cp "$repo_root/LICENSE" "$stage/LICENSE"
    cp "$repo_root/README.md" "$stage/README.md"

    # The manifest that makes the handoff checkable. Written LAST, so its
    # presence means every member above it landed, and it describes the bytes as
    # they are at the end of this phase — which is the only moment this process
    # can speak for them.
    #
    # Two claims, and `pack` re-derives both: the VERSION this build was for
    # (a stage is keyed on the target alone, so a re-`pack` at a different
    # version would otherwise silently ship one release's binaries under
    # another's name) and a sha256 per member (what is signed after the
    # identity-import step must be what was compiled before it).
    #
    # No sha256 tool is 75, here, rather than a stage that goes out unlabelled:
    # `pack` refuses a stage with no manifest, so an unwritten one only moves
    # the same failure ~30 minutes later, onto a machine that by then has a
    # signing key on it. The build is the cheap place to find out.
    printf 'version %s\n' "$version" >"$stage/.stage-meta"
    for member in teton teton-code LICENSE README.md; do
        if ! digest="$(sha256_of "$stage/$member")"; then
            echo "package: no sha256 tool (shasum, sha256sum, openssl) on PATH, so the staged" >&2
            echo "         build cannot record what it staged. The pack phase verifies that" >&2
            echo "         manifest before it signs anything, and refuses a stage without one —" >&2
            echo "         failing here rather than handing the next phase an unverifiable stage." >&2
            exit "$EXIT_UNCHECKED"
        fi
        printf '%s  %s\n' "$digest" "$member" >>"$stage/.stage-meta"
    done

    phase_note "staged teton, teton-code, LICENSE and README.md in $stage"
}

pack_phase() {
    local bin staged sha tarball meta meta_version member want got

    # The cross-boundary half of BR-2. `pack` runs in a process that did not
    # build anything, so "the binaries are there" is an assumption it has to
    # test rather than inherit: without this, a pack whose build step never ran
    # (skipped, failed early, cancelled) would tar whatever it found and hand
    # the release job a tarball with the right name and the wrong — or no —
    # contents. Every way of being incomplete is 70, and none of them writes a
    # tarball.
    if [ ! -d "$stage" ]; then
        echo "package: there is no staged build at $stage." >&2
        echo "         The pack phase packs what the build phase staged: run this script with" >&2
        echo "         phase 'build' (or 'all') first. A pack that followed no build writes no" >&2
        echo "         tarball — a release that packages nothing must not look like one." >&2
        exit "$EXIT_INTERNAL"
    fi

    # By -x rather than -e: a staged file that cannot be executed is not a
    # binary anybody can run out of the tarball.
    for bin in teton teton-code; do
        if [ ! -x "$stage/$bin" ]; then
            echo "package: the staged build at $stage is incomplete — $bin is missing or not executable." >&2
            echo "         The pack phase packs what the build phase staged, and it packs the pair or" >&2
            echo "         nothing: a tarball short one binary is worse than no tarball, because it" >&2
            echo "         installs." >&2
            exit "$EXIT_INTERNAL"
        fi
    done

    # The ride-alongs, held to the same contract. `tar` would fail on a missing
    # member by itself, but with a status from outside this script's taxonomy
    # and a message about a file rather than about a phase.
    for staged in LICENSE README.md; do
        if [ ! -f "$stage/$staged" ]; then
            echo "package: the staged build at $stage is incomplete — $staged is missing." >&2
            echo "         The pack phase packs what the build phase staged, and every member of" >&2
            echo "         the tarball comes from there." >&2
            exit "$EXIT_INTERNAL"
        fi
    done

    # The staging directory is COMPLETE, by the two guards above. This is the
    # separate question of whether it is the one this build phase left — asked
    # BEFORE a single byte is signed, because a signature is a claim about
    # provenance and the whole of what stands between the two phases is a
    # directory anything on the runner can write to. Between `build` and here
    # sits the step that imports a Developer ID private key; a stage swapped
    # after it would be signed with a real identity and be indistinguishable
    # from a release for the rest of its life.
    #
    # Three ways it can fail, each named separately, because they are three
    # different accidents with three different fixes.
    meta="$stage/.stage-meta"
    if [ ! -f "$meta" ]; then
        echo "package: the staged build at $stage carries no .stage-meta." >&2
        echo "         The pack phase packs what the build phase staged, and it checks that it is" >&2
        echo "         packing those exact bytes — a stage with no manifest is one this script's" >&2
        echo "         build phase did not write, or one an older copy of it did. Either way there" >&2
        echo "         is nothing here to verify against, and unverified bytes do not get signed." >&2
        exit "$EXIT_INTERNAL"
    fi

    # `$1 == "version"` on line 1 only: the manifest's first line is its header,
    # and a digest line's first field is a digest.
    meta_version="$(awk 'NR == 1 && $1 == "version" { print $2 }' "$meta")"
    if [ "$meta_version" != "$version" ]; then
        echo "package: the staged build at $stage was built for version '$meta_version', and this" >&2
        echo "         pack was asked for version '$version'. The staging directory is keyed on the" >&2
        echo "         target alone, so packing it anyway would put one release's binaries inside a" >&2
        echo "         tarball named for another — and the name is most of what the rest of the" >&2
        echo "         pipeline reads." >&2
        exit "$EXIT_INTERNAL"
    fi

    # The members this script SHIPS, not the members the manifest happens to
    # list: reading the list out of the file being checked would let a tampered
    # manifest excuse a tampered member by dropping its line. A member with no
    # digest recorded for it is a mismatch like any other.
    for member in teton teton-code LICENSE README.md; do
        if ! got="$(sha256_of "$stage/$member")"; then
            echo "package: no sha256 tool (shasum, sha256sum, openssl) on PATH, so the staged" >&2
            echo "         build cannot be checked against the manifest its build wrote. Nothing" >&2
            echo "         is signed and no tarball is written: 'could not check' is not 'checked'." >&2
            exit "$EXIT_UNCHECKED"
        fi
        want="$(awk -v m="$member" 'NR > 1 && $2 == m { print $1 }' "$meta")"
        if [ -z "$want" ] || [ "$want" != "$got" ]; then
            echo "package: the staged $member at $stage is not the one the build phase staged." >&2
            echo "         .stage-meta records ${want:-no digest at all} for it; it hashes to $got." >&2
            echo "         The bytes that get signed have to be the bytes that were built — that is" >&2
            echo "         the entire reason the identity is imported between the two phases." >&2
            exit "$EXIT_INTERNAL"
        fi
    done

    phase_note "packing the staged build in $stage"

    # The signing phase (ADR-550-1), on the STAGED copies rather than the
    # originals under target/: the tarball below is assembled from this
    # directory, so signing here is what makes the shipped bytes the verified
    # bytes. Signing the build output instead would leave a gap between what was
    # checked and what was tarred — and, since REQ-551 split the phases, the
    # build output need not even be on this machine's disk any more.
    #
    # Keyed on the identity again, not on `$codesign_tool` being non-empty. The
    # two are equivalent by construction — the check above exits when they are
    # not — but "sign if a tool happens to be around" is the self-disabling
    # shape (LESSON-443) and should not be what this reads like to whoever edits
    # it next.
    if [ -n "${TETON_SIGN_IDENTITY:-}" ]; then
        for bin in teton teton-code; do
            # `--options runtime` (the hardened runtime) is what notarization
            # will require of these binaries (spec OQ-3) and costs nothing
            # before then. `--force` is there because Apple's linker already
            # ad-hoc signs arm64 Mach-Os at link time and codesign refuses to
            # replace an existing signature without it; it decides only WHICH
            # signature survives, never whether one is checked — the --verify
            # below, and verify-signature.sh in the smoke, still have to accept
            # whatever it produced.
            if ! "$codesign_tool" --force --sign "$TETON_SIGN_IDENTITY" \
                --timestamp --options runtime "$stage/$bin"; then
                echo "package: codesign could not sign $bin with the requested identity." >&2
                echo "         No tarball is written: a signing-requested build never ships unsigned." >&2
                exit "$EXIT_INTERNAL"
            fi

            # Asked immediately, of the same file, because `--sign` exiting 0 is
            # not the claim being made — "these bytes carry a signature that
            # holds" is, and only --verify tests it. Whether the signature names
            # the right AUTHORITY is verify-signature.sh's question, asked of the
            # tarball in the smoke; asking it a second way here would be a second
            # opinion free to drift from the gate that actually blocks the
            # release.
            if ! "$codesign_tool" --verify --strict "$stage/$bin"; then
                echo "package: $bin was signed but codesign --verify --strict rejected the result." >&2
                exit "$EXIT_INTERNAL"
            fi

            echo "package: signed $bin"
        done
    else
        echo "package: binaries are unsigned (dev build — set TETON_SIGN_IDENTITY for release signing)"
    fi

    tarball="$outdir/teton-v$version-$target.tar.gz"

    # Members are listed explicitly rather than archiving `.`: it fixes the entry
    # order and keeps the `./` prefix (and any stray dotfile) out of the archive, so
    # the tarball is flat — `teton`, `teton-code`, `LICENSE`, `README.md` at the root.
    # `.stage-meta` is one of the dotfiles that explicit listing keeps out, and that
    # is deliberate rather than incidental: it is this script's bookkeeping about a
    # handoff between two of its own phases, it has already done its job by the time
    # this line runs, and a manifest riding along inside the artifact would read to a
    # user like a checksum they should trust. `checksums.txt` in the release is the
    # thing that speaks to them. THIS LIST IS THE TARBALL'S CONTRACT — adding a name
    # here changes what every installed copy contains.
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
        echo "         $(basename "$tarball") was built and has been REMOVED: every other refusal" >&2
        echo "         in this script leaves no artifact behind, and an unhashed tarball sitting" >&2
        echo "         in the output directory looks exactly like a finished one to the upload" >&2
        echo "         step, to a human re-running a leg, and to a glob." >&2
        rm -f "$tarball"
        exit "$EXIT_UNCHECKED"
    fi
    printf '%s  %s\n' "$sha" "$(basename "$tarball")" >"$tarball.sha256"

    echo "package: built $(basename "$tarball")"
    echo "package: sha256 $sha"
    printf '%s\n' "$tarball"

    # Consumed, AFTER the lines above. The staging directory exists to carry one
    # build across one phase boundary, and it has: keeping it would leave signed
    # binaries loose in the output directory beside the tarball made from them,
    # and would let a second `pack` re-ship a build the first one already
    # shipped.
    #
    # Last, and non-fatal, because by this point the packaging has SUCCEEDED —
    # the tarball is written, hashed and announced. A failing `rm` here (a
    # read-only mount, a file another process is holding open) is housekeeping
    # that did not happen, and exiting non-zero for it would fail a release leg
    # over a directory nobody needs, after telling the log the leg worked.
    rm -rf "$stage" || {
        echo "package: warning — could not remove the staged build at $stage. The tarball above" >&2
        echo "         is complete, hashed and announced; this is leftover scratch, not a" >&2
        echo "         packaging failure. Remove it before packing this target again, or the" >&2
        echo "         next build phase will (it clears the directory before it compiles)." >&2
    }
}

case "$phase" in
    all)
        build_phase
        pack_phase
        ;;
    build)
        build_phase
        ;;
    pack)
        pack_phase
        ;;
esac
