#!/usr/bin/env bash
#
# Standing regression suite for the release scripts in this directory, plus
# site/render.sh. CI runs it as, exactly:
#
#     bash tools/release/selftest.sh
#
# It needs NO network, NO cargo, and no release artifacts, and finishes in
# seconds. That is the whole point: these scripts are the last gate before bytes
# reach a user's machine, they only ever run inside a tagged release, and until
# this file existed nothing exercised them at all — every one of them was first
# run in anger on the release that needed it to work.
#
# What it covers, and why each group is here:
#
#   syntax          `bash -n` on every script, including the sourced library.
#   verify-version  match / mismatch / could-not-check, and the TOML shapes a
#                   legal reformat can produce (`version="0.1.0"` with no
#                   spaces, a commented block header) — both of which used to
#                   report "no version found".
#   render-formula  both input modes, and every refusal: unfilled placeholder,
#                   missing sha, non-hex sha, prerelease and +build versions.
#   site/render     the same, against an isolated copy of the tree, so a case
#                   that deliberately renders a broken page cannot touch the
#                   real site/dist.
#   changelog       changelog-section.sh — the topmost changelog section and
#                   ONLY it, the several shapes that must publish nothing
#                   without failing a release, and the release.yml wiring that
#                   turns it into the body's "Upgrade notes" section.
#   smoke           smoke.sh TESTED AS A TESTER. Nothing had ever verified that
#                   its assertions are capable of failing, and one of them was
#                   not: an empty version made `grep -qF -- ""` match any
#                   output, so all four assertions passed against any pair of
#                   binaries. A gate is only a gate if a bad artifact makes it
#                   go red, so this group builds deliberately bad artifacts and
#                   requires exactly that.
#   signature +     the same question asked of the two gates REQ-550 adds,
#   attestation     driven through their `TETON_CODESIGN` / `TETON_GH` seams
#                   with stand-in tools: an ad-hoc signed binary, a
#                   development-signed one, a signature from another team and a
#                   tampered tarball must each make a gate go RED (65); a
#                   missing, unrunnable or mute tool must leave it UNCHECKED
#                   (75); and only the one good shape may pass.
#
# The smoke stand-ins are shell scripts named `teton` and `teton-code`, tarred the
# way package.sh tars the real ones. They are not the product and prove nothing
# about it — they are the KNOWN-BAD and KNOWN-GOOD inputs that let this suite
# ask smoke.sh a question it can get wrong. The `codesign` and `gh` stand-ins
# are the same idea one level out: they are not Apple's or GitHub's tools and
# are no evidence about them, they are the ANSWERS those tools give, in the
# shapes they give them, so that what the gates do with each answer can be
# tested on a machine that has neither.
#
# Exit codes:
#   0   every case passed.
#   1   at least one case FAILED — a release script does not behave as its own
#       documentation says it does.
#   75  the suite could NOT run (a script under test is missing).

set -euo pipefail

EXIT_FAILED=1
EXIT_UNCHECKED=75

# This suite IS the harness the tool seams exist for, and it says so once, here.
#
# package.sh, verify-signature.sh, verify-attestation.sh and smoke.sh all refuse
# to honour their `TETON_CODESIGN` / `TETON_GH` / `TETON_SMOKE_ASSUME_TARGET`
# overrides when GITHUB_ACTIONS=true, because those variables decide which
# program signs the release or answers whether it is signed — a value set by
# anything other than a test harness in CI is a release waved through by an
# environment variable. This export is the one place that says "a harness set
# them", and it is deliberately at the top rather than per-case: every case
# below drives a stand-in, so a per-case opt-in would be forty copies of the
# same sentence and one of them would eventually be forgotten, turning a real
# assertion into a 64 nobody read.
#
# It is safe to export because it grants nothing on its own: without one of the
# override variables also set, it changes no behaviour anywhere.
export TETON_ALLOW_TOOL_SEAM=1

script_dir="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)"
repo_root="$(cd -- "$script_dir/../.." && pwd)"

# `source=` lets `shellcheck -x` follow this; `disable=SC1091` keeps a bare
# `shellcheck <file>` — which cannot follow it — from failing on the info.
# shellcheck source=tools/release/lib.sh disable=SC1091
. "$script_dir/lib.sh"

LIB="$script_dir/lib.sh"
VERIFY="$script_dir/verify-version.sh"
PACKAGE="$script_dir/package.sh"
RENDER_FORMULA="$script_dir/render-formula.sh"
SMOKE="$script_dir/smoke.sh"
VERIFY_SIGNATURE="$script_dir/verify-signature.sh"
VERIFY_ATTESTATION="$script_dir/verify-attestation.sh"
VERIFY_BATCH="$script_dir/verify-attestations-batch.sh"
CHANGELOG_SECTION="$script_dir/changelog-section.sh"
SITE_RENDER="$repo_root/site/render.sh"
SITE_TEMPLATE="$repo_root/site/index.html"
FORMULA_TEMPLATE="$repo_root/packaging/homebrew/teton.rb.tmpl"
RUNBOOK="$repo_root/docs/release-runbook.md"
README="$repo_root/README.md"
RELEASE_WORKFLOW="$repo_root/.github/workflows/release.yml"

# RELEASE_WORKFLOW is in this list and README/RUNBOOK are not, and the
# difference is what happens without it. The ordering group below does not merely
# read the workflow — it hashes it (`sha256_of`) and mutates copies of it under
# `set -e`, so an absent file aborts the whole suite mid-run with a status that
# says nothing about why. The two prose files are only ever grepped, inside a
# loop that already skips a missing one by name.
for required in "$LIB" "$VERIFY" "$PACKAGE" "$RENDER_FORMULA" "$SMOKE" \
    "$VERIFY_SIGNATURE" "$VERIFY_ATTESTATION" "$VERIFY_BATCH" \
    "$CHANGELOG_SECTION" "$SITE_RENDER" "$SITE_TEMPLATE" "$FORMULA_TEMPLATE" \
    "$RELEASE_WORKFLOW"; do
    if [ ! -f "$required" ]; then
        echo "selftest: $required is missing — nothing was tested." >&2
        exit "$EXIT_UNCHECKED"
    fi
done

# Short, like smoke.sh's own scratch root: the stand-in daemon's runtime dir is
# handed to it by smoke.sh, and a long TMPDIR is how you discover SUN_LEN.
work="$(mktemp -d /tmp/teton-selftest.XXXXXX)"
trap 'rm -rf "$work"' EXIT

# --- the runner ------------------------------------------------------------
#
# Every helper returns 0 unconditionally: a failing CASE must not abort the
# suite under `set -e`, because "which of the forty cases fail" is the entire
# output of this script.

passed=0
failed=0
CASE_OUT=""
CASE_OK=0
# Was the last thing that ran an expect_exit at all? CASE_OK alone cannot say:
# it is 0 both when a case misbehaved and when there was no case, and those two
# want opposite treatment from a follow-up assertion. See assert().
CASE_RAN=0

report_pass() {
    passed=$((passed + 1))
    printf '  PASS  %s\n' "$1"
}

report_fail() {
    failed=$((failed + 1))
    printf '  FAIL  %s\n' "$1"
    if [ -n "${2:-}" ]; then
        printf '%s\n' "$2" | sed 's/^/          | /'
    fi
}

group() { printf '\n%s\n' "$1"; }

# expect_exit <expected-code> <label> <command...>
# Leaves the command's combined output in CASE_OUT for a follow-up assertion.
expect_exit() {
    local expected="$1" label="$2"
    shift 2
    local status=0
    CASE_OK=0
    CASE_OUT=""
    CASE_RAN=1
    CASE_OUT="$("$@" 2>&1)" || status=$?
    if [ "$status" -eq "$expected" ]; then
        CASE_OK=1
        report_pass "$label [exit $status]"
    else
        report_fail "$label [expected exit $expected, got $status]" "$CASE_OUT"
    fi
    return 0
}

# expect_output <label> <fixed string the last case must have printed>
expect_output() {
    if [ "$CASE_OK" -ne 1 ]; then
        report_fail "$1 [not checked: the case it reads from did not behave]"
        return 0
    fi
    if printf '%s' "$CASE_OUT" | grep -qF -- "$2"; then
        report_pass "$1"
    else
        report_fail "$1 [output does not contain: $2]" "$CASE_OUT"
    fi
    return 0
}

# CASE_OK / CASE_OUT / CASE_RAN belong to the LAST expect_exit and to nothing
# else.
#
# Every helper that is not expect_exit clears them, so a stray `expect_output`
# — one whose expect_exit was deleted, moved, or separated by an assert during
# an edit — reads an empty CASE_OUT and a CASE_OK of 0, and reports "not
# checked" instead of silently grading itself against whatever the previous case
# left behind. Bookkeeping that carries over is how a suite starts asserting
# against the wrong output and stays green while doing it.
#
# Safe for the `assert "…" [ "$CASE_OUT" = "1.2.3" ]` shape this file uses: the
# caller expands CASE_OUT into argv before assert runs, so the reset here cannot
# reach it.
reset_case() {
    CASE_OK=0
    CASE_OUT=""
    CASE_RAN=0
}

# assert <label> <command...> — passes when the command succeeds.
#
# Consults CASE_OK the way expect_output does, and for the same reason. Four
# assertions in this file are FOLLOW-UPS that read CASE_OUT out of the case
# above them, and they used to be graded unconditionally: when the case they
# read from had misbehaved, CASE_OUT held whatever the broken run printed, and a
# prefix-strip like
#
#     assert "… no empty --signer-workflow was smuggled in" \
#         [ "${CASE_OUT#*--signer-workflow}" = "$CASE_OUT" ]
#
# then reported PASS because a shell's "command not found" contains no such
# needle. The suite would go red on the case and GREEN on the assertion about
# it — an assertion whose evidence never existed, printed as though it had.
#
# The condition is CASE_RAN rather than a bare CASE_OK, and that distinction is
# what keeps the other ~200 standalone asserts working: reset_case clears
# CASE_RAN, and every helper except expect_output calls reset_case, so CASE_RAN
# is 1 only for an assert sitting directly under an expect_exit (optionally with
# expect_output between them). An assert that follows no case at all is graded
# normally. An assert that follows a BROKEN case is not graded at all.
assert() {
    local label="$1"
    shift
    if [ "$CASE_RAN" -eq 1 ] && [ "$CASE_OK" -ne 1 ]; then
        reset_case
        report_fail "$label [not checked: the case it reads from did not behave]"
        return 0
    fi
    reset_case
    if "$@"; then
        report_pass "$label"
    else
        report_fail "$label"
    fi
    return 0
}

# refute <label> <command...> — passes when the command FAILS.
#
# A separate helper rather than `assert <label> ! cmd`, because `!` is a shell
# keyword, not a command: passed through "$@" it is looked up as a program and
# reported as "!: command not found", which `assert` then dutifully counted as a
# failing command — every negative case would have "passed" for the wrong
# reason. (It did, on this suite's first run.)
refute() {
    local label="$1"
    shift
    reset_case
    if "$@"; then
        report_fail "$label [the command succeeded, and should not have]"
    else
        report_pass "$label"
    fi
    return 0
}

# skip <label> — records a case that could NOT be exercised on this machine.
#
# Counted separately from a pass: "this machine has no third hashing tool" is
# not evidence that the third branch works. Never fails the suite — a developer
# laptop missing a tool must not turn CI's signal red — but it prints, so a
# silently-shrinking suite is visible rather than inferred.
skipped=0
skip() {
    skipped=$((skipped + 1))
    reset_case
    printf '  SKIP  %s\n' "$1"
    return 0
}

# --- syntax ----------------------------------------------------------------

group "syntax (bash -n)"
for s in "$LIB" "$VERIFY" "$PACKAGE" "$RENDER_FORMULA" "$SMOKE" \
    "$VERIFY_SIGNATURE" "$VERIFY_ATTESTATION" "$VERIFY_BATCH" \
    "$CHANGELOG_SECTION" "$SITE_RENDER" "${BASH_SOURCE[0]}"; do
    expect_exit 0 "bash -n $(basename "$s")" bash -n "$s"
done

# --- verify-version.sh -----------------------------------------------------

group "verify-version.sh"

vv="$work/vv"
mkdir -p "$vv"

printf '[workspace]\nmembers = ["crates/*"]\n\n[workspace.package]\nversion = "1.2.3"\nedition = "2021"\n' \
    >"$vv/canonical.toml"
# The two shapes that used to be read as "no version found in [workspace.package]"
# and reported as UNCHECKED against a manifest that plainly declares one.
printf '[workspace.package]\nversion="1.2.3"\n' >"$vv/no-spaces.toml"
printf '[workspace.package] # the version every crate inherits\nversion = "1.2.3"\n' \
    >"$vv/header-comment.toml"
printf '[workspace.package]\nversion = "1.2.3"  # bumped by the release PR\n' \
    >"$vv/value-comment.toml"
# The reason the block test exists at all: a `version` in another table must
# stay invisible, and loosening the match on the version LINE must not loosen
# the match on the BLOCK.
printf '[package]\nversion = "9.9.9"\n\n[workspace.package]\nversion = "1.2.3"\n' \
    >"$vv/other-table.toml"
printf '[workspace.package]\nedition = "2021"\n' >"$vv/no-version.toml"

expect_exit 0 "tag matches the manifest" bash "$VERIFY" v1.2.3 "$vv/canonical.toml"
expect_output "  ... and says MATCH" "MATCH"

expect_exit 0 "a tag without its leading v still matches" bash "$VERIFY" 1.2.3 "$vv/canonical.toml"

expect_exit 64 "tag disagrees with the manifest -> 64, not a bare 1" \
    bash "$VERIFY" v9.9.9 "$vv/canonical.toml"
expect_output "  ... and names both versions" "declares version '9.9.9'"

expect_exit 75 "no tag at all -> 75 UNCHECKED" bash "$VERIFY"
expect_exit 75 "an empty tag -> 75 UNCHECKED" bash "$VERIFY" "" "$vv/canonical.toml"
expect_exit 75 "manifest not found -> 75 UNCHECKED" bash "$VERIFY" v1.2.3 "$vv/absent.toml"
expect_exit 75 "no version in [workspace.package] -> 75 UNCHECKED" \
    bash "$VERIFY" v1.2.3 "$vv/no-version.toml"

expect_exit 0 'version="1.2.3" with no spaces is read' bash "$VERIFY" v1.2.3 "$vv/no-spaces.toml"
expect_exit 0 '[workspace.package] # comment header is entered' \
    bash "$VERIFY" v1.2.3 "$vv/header-comment.toml"
expect_exit 0 'a trailing comment on the value is stripped' \
    bash "$VERIFY" v1.2.3 "$vv/value-comment.toml"

expect_exit 0 "--print-version prints the workspace version" \
    bash "$VERIFY" --print-version "$vv/canonical.toml"
assert "  ... and prints exactly 1.2.3" [ "$CASE_OUT" = "1.2.3" ]

expect_exit 0 "a version in another table is not picked up" \
    bash "$VERIFY" --print-version "$vv/other-table.toml"
assert "  ... [package] version 9.9.9 stayed invisible" [ "$CASE_OUT" = "1.2.3" ]

# --- render-formula.sh -----------------------------------------------------

group "render-formula.sh"

SHA_A="1111111111111111111111111111111111111111111111111111111111111111"
SHA_B="2222222222222222222222222222222222222222222222222222222222222222"
SHA_C="3333333333333333333333333333333333333333333333333333333333333333"

rf="$work/rf"
mkdir -p "$rf"

expect_exit 0 "renders the real template from explicit shas" \
    bash "$RENDER_FORMULA" --version 1.2.3 --output "$rf/teton.rb" \
    --sha-aarch64-apple-darwin "$SHA_A" \
    --sha-x86_64-apple-darwin "$SHA_B" \
    --sha-x86_64-unknown-linux-gnu "$SHA_C"
assert "  ... the formula was written" [ -s "$rf/teton.rb" ]
refute "  ... no {{placeholder}} survived" grep -Fq '{{' "$rf/teton.rb"
assert "  ... the arm64 sha landed in the arm64 stanza" \
    grep -Fq "sha256 \"$SHA_A\"" "$rf/teton.rb"
assert "  ... the version reached the download URLs" \
    grep -Fq "/v1.2.3/teton-v1.2.3-aarch64-apple-darwin.tar.gz" "$rf/teton.rb"

# --artifacts is the mode the release workflow uses; it hashes the bytes itself
# (BR-5) via the sha256_of that now lives in lib.sh, so this case is also the
# only coverage that extraction has.
arts="$work/artifacts"
mkdir -p "$arts"
for t in aarch64-apple-darwin x86_64-apple-darwin x86_64-unknown-linux-gnu; do
    printf 'pretend tarball for %s\n' "$t" >"$arts/teton-v1.2.3-$t.tar.gz"
done
expected_arm_sha="$(sha256_of "$arts/teton-v1.2.3-aarch64-apple-darwin.tar.gz")"

expect_exit 0 "--artifacts hashes the tarballs itself" \
    bash "$RENDER_FORMULA" --version 1.2.3 --artifacts "$arts" --output "$rf/from-artifacts.rb"
assert "  ... and the recorded sha is the sha of those bytes" \
    grep -Fq "sha256 \"$expected_arm_sha\"" "$rf/from-artifacts.rb"

expect_exit 64 "--artifacts and --sha-<target> together -> 64" \
    bash "$RENDER_FORMULA" --version 1.2.3 --artifacts "$arts" --sha-x86_64-apple-darwin "$SHA_B"

expect_exit 75 "an artifact missing from --artifacts -> 75 UNCHECKED" \
    bash "$RENDER_FORMULA" --version 1.2.3 --artifacts "$work/rf"

expect_exit 64 "a missing sha -> 64" \
    bash "$RENDER_FORMULA" --version 1.2.3 \
    --sha-aarch64-apple-darwin "$SHA_A" --sha-x86_64-apple-darwin "$SHA_B"
expect_output "  ... and names the target it lacks" "no sha256 for x86_64-unknown-linux-gnu"

expect_exit 64 "a non-hex sha -> 64" \
    bash "$RENDER_FORMULA" --version 1.2.3 \
    --sha-aarch64-apple-darwin "not-a-sha" \
    --sha-x86_64-apple-darwin "$SHA_B" \
    --sha-x86_64-unknown-linux-gnu "$SHA_C"

expect_exit 64 "a prerelease version -> 64, never rendered" \
    bash "$RENDER_FORMULA" --version 1.2.3-rc.1 \
    --sha-aarch64-apple-darwin "$SHA_A" \
    --sha-x86_64-apple-darwin "$SHA_B" \
    --sha-x86_64-unknown-linux-gnu "$SHA_C"

expect_exit 64 "a +build version -> 64, never rendered" \
    bash "$RENDER_FORMULA" --version 1.2.3+build.7 \
    --sha-aarch64-apple-darwin "$SHA_A" \
    --sha-x86_64-apple-darwin "$SHA_B" \
    --sha-x86_64-unknown-linux-gnu "$SHA_C"

expect_exit 64 "an empty version -> 64" \
    bash "$RENDER_FORMULA" --version "" \
    --sha-aarch64-apple-darwin "$SHA_A" \
    --sha-x86_64-apple-darwin "$SHA_B" \
    --sha-x86_64-unknown-linux-gnu "$SHA_C"

expect_exit 64 "a flag with no value -> 64" bash "$RENDER_FORMULA" --version
expect_exit 64 "an unknown argument -> 64" bash "$RENDER_FORMULA" --version 1.2.3 --nope

printf 'class Teton < Formula\n  version "{{VERSION}}"\n  revision "{{MYSTERY}}"\nend\n' \
    >"$rf/drifted.tmpl"
expect_exit 64 "a placeholder the script cannot fill -> 64" \
    bash "$RENDER_FORMULA" --version 1.2.3 --template "$rf/drifted.tmpl" \
    --sha-aarch64-apple-darwin "$SHA_A" \
    --sha-x86_64-apple-darwin "$SHA_B" \
    --sha-x86_64-unknown-linux-gnu "$SHA_C"
expect_output "  ... and names the placeholder" "{{MYSTERY}}"

expect_exit 75 "template not found -> 75 UNCHECKED" \
    bash "$RENDER_FORMULA" --version 1.2.3 --template "$rf/absent.tmpl" \
    --sha-aarch64-apple-darwin "$SHA_A" \
    --sha-x86_64-apple-darwin "$SHA_B" \
    --sha-x86_64-unknown-linux-gnu "$SHA_C"

# --- site/render.sh --------------------------------------------------------
#
# Against COPIES: render.sh resolves its template and its output directory from
# its own location, so running the repo's copy would write (and, in the
# surviving-placeholder case, delete) the real site/dist/index.html.

group "site/render.sh"

site_ok="$work/site-ok"
mkdir -p "$site_ok"
cp "$SITE_RENDER" "$site_ok/render.sh"
cp "$SITE_TEMPLATE" "$site_ok/index.html"

expect_exit 0 "renders the real page template" bash "$site_ok/render.sh" 1.2.3
assert "  ... the page was written" [ -s "$site_ok/dist/index.html" ]
refute "  ... no {{placeholder}} survived" grep -Fq '{{' "$site_ok/dist/index.html"
assert "  ... the version was stamped" grep -Fq 'v1.2.3' "$site_ok/dist/index.html"
assert "  ... the default install command was stamped" \
    grep -Fq 'brew install atelier-fashion/tap/teton' "$site_ok/dist/index.html"

expect_exit 0 "accepts an explicit install command" \
    bash "$site_ok/render.sh" v1.2.3 "brew install teton"
assert "  ... and stamps it" grep -Fq 'brew install teton' "$site_ok/dist/index.html"

# D-1: the landing page must not name a binary the formula does not install.
# tetoncode.ai shipped "teton / tetond" for the whole life of v0.1.1 — the
# daemon had been renamed to teton-code everywhere the rename swept, and the
# page was the one surface nothing checked. The formula's bin.install line is
# the owner (it is what lands in a user's PATH); the page is asserted against
# it rather than against a list restated here.
installed_bins="$(sed -nE 's|^[[:space:]]*bin\.install[[:space:]]+(.+)$|\1|p' \
    "$FORMULA_TEMPLATE" | head -1 | tr -d '"' | tr ',' ' ')"
if [ -z "$installed_bins" ]; then
    report_fail "site/formula binary cross-check could not read bin.install" \
        "from $FORMULA_TEMPLATE"
else
    for bin_name in $installed_bins; do
        assert "the page names the installed binary '$bin_name'" \
            grep -Fq "$bin_name" "$SITE_TEMPLATE"
    done
    # The negative half, and the one that would have caught the live bug: the
    # page must not name a binary that no longer exists.
    if grep -Fq "tetond" "$SITE_TEMPLATE"; then
        report_fail "the page names 'tetond', which bin.install does not install" \
            "$(grep -n 'tetond' "$SITE_TEMPLATE" | head -3)"
    else
        report_pass "the page names no binary outside bin.install"
    fi
fi

# D-2: the published install command and render.sh's default must be the same
# string. deploy-site.yml passes the value explicitly; render.sh owns it. Two
# copies of one fact is exactly the shape that broke v0.1.1's log paths, so the
# pair is pinned here rather than trusted.
wf_cmd="$(sed -nE 's|^[[:space:]]*INSTALL_COMMAND:[[:space:]]*(.+)$|\1|p' \
    "$repo_root/.github/workflows/deploy-site.yml" | head -1)"
default_cmd="$(sed -nE "s|^readonly DEFAULT_INSTALL_COMMAND='(.+)'$|\\1|p" \
    "$SITE_RENDER" | head -1)"
if [ -z "$wf_cmd" ] || [ -z "$default_cmd" ]; then
    report_fail "install command cross-check could not read both copies" \
        "workflow='$wf_cmd' render.sh='$default_cmd'"
else
    assert "the published install command matches render.sh's default" \
        [ "$wf_cmd" = "$default_cmd" ]
fi

expect_exit 64 "no argument -> 64" bash "$site_ok/render.sh"
expect_exit 64 "a malformed version -> 64" bash "$site_ok/render.sh" "not-a-version"
expect_exit 64 "an empty version -> 64" bash "$site_ok/render.sh" ""
expect_exit 64 "a prerelease version -> 64, agreeing with the release scripts" \
    bash "$site_ok/render.sh" 1.2.3-rc.1
expect_exit 64 "a +build version -> 64, agreeing with the release scripts" \
    bash "$site_ok/render.sh" 1.2.3+build.7
expect_exit 64 "an install command carrying markup -> 64" \
    bash "$site_ok/render.sh" 1.2.3 'brew install <script>alert(1)</script>'

site_missing="$work/site-missing"
mkdir -p "$site_missing"
cp "$SITE_RENDER" "$site_missing/render.sh"
expect_exit 75 "template not found -> 75 UNCHECKED, not 64" bash "$site_missing/render.sh" 1.2.3
expect_output "  ... and says nothing was rendered" "nothing was rendered"

site_drift="$work/site-drift"
mkdir -p "$site_drift"
cp "$SITE_RENDER" "$site_drift/render.sh"
printf '<!DOCTYPE html>\n<html><body><p>v{{VERSION}} {{MYSTERY}}</p></body></html>\n' \
    >"$site_drift/index.html"
expect_exit 64 "a placeholder the script cannot fill -> 64" bash "$site_drift/render.sh" 1.2.3
assert "  ... and the half-rendered page was deleted, not published" \
    [ ! -e "$site_drift/dist/index.html" ]

# --- smoke.sh, tested as a tester ------------------------------------------

group "smoke.sh (as a tester: can its assertions fail?)"

# make_standins <dir> <reported-version> <refuses-seams:yes|no> <handshakes:yes|no>
make_standins() {
    local dir="$1" reported="$2" refuses="$3" handshakes="$4"
    mkdir -p "$dir"

    cat >"$dir/teton" <<EOF
#!/usr/bin/env bash
# selftest stand-in for the released teton CLI. Not the product.
case "\${1:-}" in
    --version) echo "teton $reported"; exit 0 ;;
    doctor)
        if [ -f "\${XDG_RUNTIME_DIR:-/nonexistent}/teton/handshake" ]; then
            echo "daemon: running — teton-code $reported (protocol 1)"
        else
            echo "daemon: not running (no socket)"
        fi
        exit 0
        ;;
esac
exit 0
EOF

    # \`exec sleep\` rather than a wait loop: smoke.sh kills the pid it started,
    # and exec makes that pid the thing that needs killing.
    cat >"$dir/teton-code" <<EOF
#!/usr/bin/env bash
# selftest stand-in for the released teton-code daemon. Not the product.
case "\${1:-}" in --version) echo "teton-code $reported"; exit 0 ;; esac
if [ "\${TETON_TEST_SEAMS:-}" = "1" ] && [ "$refuses" = "yes" ]; then
    echo "teton-code: TETON_TEST_SEAMS=1 is set, but this is a release build, which cannot honour them." >&2
    exit 70
fi
if [ "$handshakes" = "yes" ]; then
    mkdir -p "\$XDG_RUNTIME_DIR/teton"
    : >"\$XDG_RUNTIME_DIR/teton/handshake"
fi
exec sleep 600
EOF

    chmod +x "$dir/teton" "$dir/teton-code"
    printf 'stand-in licence\n' >"$dir/LICENSE"
    printf 'stand-in readme\n' >"$dir/README.md"
}

# make_tarball <name> <reported-version> <refuses> <handshakes> -> path on stdout
make_tarball() {
    local name="$1" dir="$work/standin-$1"
    make_standins "$dir" "$2" "$3" "$4"
    tar -czf "$work/teton-v1.2.3-$name.tar.gz" -C "$dir" teton teton-code LICENSE README.md
    printf '%s\n' "$work/teton-v1.2.3-$name.tar.gz"
}

tb_good="$(make_tarball good 1.2.3 yes yes)"
tb_wrong_version="$(make_tarball wrong-version 0.0.1 yes yes)"
tb_seams_honoured="$(make_tarball seams-honoured 1.2.3 no yes)"
tb_no_handshake="$(make_tarball no-handshake 1.2.3 yes no)"

# A tarball that is missing a binary entirely — the UNCHECKED path.
make_standins "$work/standin-truncated" 1.2.3 yes yes
rm -f "$work/standin-truncated/teton-code"
tar -czf "$work/teton-v1.2.3-truncated.tar.gz" -C "$work/standin-truncated" teton LICENSE README.md

# The stand-in tarballs above are named for the BEHAVIOUR they model — `good`,
# `wrong-version`, `seams-honoured`, `no-handshake`, `truncated` — because what
# each one varies is what its binaries do, not which platform they are for. None
# of those names is a release target, and smoke.sh now refuses an unrecognised
# target outright (75) instead of quietly filing it under "linux, unsigned".
#
# So the fixtures declare their platform out of band. This is the seam
# smoke.sh's header describes, and it is safe by construction rather than by
# discipline: smoke.sh consults it ONLY when the tarball's own name yields no
# recognised triple, so it has no effect at all on `teton-v1.2.3-<triple>.tar.gz`
# — which is every name package.sh writes, and therefore every artifact in a
# real release. The darwin and linux tarballs in the signature group below are
# named for real triples and are classified by those names with this still
# exported.
#
# The alternative considered and rejected: teaching smoke.sh to recognise the
# stand-in names themselves. That would have put selftest fixture names in a
# release gate, where a future fixture named after a target would be a silent
# hole, and where the production path's correctness would depend on a list of
# test data.
export TETON_SMOKE_ASSUME_TARGET=x86_64-unknown-linux-gnu

# Deliberately generous for the good pair: the timing assertion below needs a
# deadline long enough that an orphaned watchdog would be conspicuous.
export TETON_SMOKE_SEAM_DEADLINE_SECS=10
export TETON_SMOKE_HANDSHAKE_DEADLINE_SECS=10

started_at="$(date +%s)"
expect_exit 0 "a GOOD pair passes all four assertions" bash "$SMOKE" "$tb_good" 1.2.3
expect_output "  ... and says so" "all 4 assertions passed"
elapsed=$(($(date +%s) - started_at))
# Reading this script's output must not be what waits for it. Before the
# watchdog was detached, its orphaned `sleep` inherited the caller's stdout and
# held the pipe open for the full seam deadline after every assertion had
# already finished — so this case took 10s to be observed, not 1s.
assert "  ... and finishes when it finishes (${elapsed}s, seam deadline 10s)" \
    [ "$elapsed" -lt 5 ]

# The three bad pairs. Short deadlines: two of them are timeouts, and the only
# thing a longer wait buys is a slower suite.
export TETON_SMOKE_SEAM_DEADLINE_SECS=2
export TETON_SMOKE_HANDSHAKE_DEADLINE_SECS=2

expect_exit 65 "binaries reporting the WRONG version -> 65 FAILED" \
    bash "$SMOKE" "$tb_wrong_version" 1.2.3
expect_output "  ... and says which assertion failed" "--version does not report 1.2.3"

expect_exit 65 "a teton-code that HONOURS TETON_TEST_SEAMS=1 -> 65 FAILED (BR-9)" \
    bash "$SMOKE" "$tb_seams_honoured" 1.2.3
# The seams-honoured stub PRINTS the refusal line and then keeps running, so
# the diagnosis has to name what actually happened — "still running … had to be
# killed" — not the generic did-not-refuse text, which a daemon that exited on
# its own would also earn. Asserting the killed wording is what pins the gate
# hole shut: before smoke.sh recorded the watchdog's kill, the kill itself
# supplied the non-zero exit and this stub scored as a PASS.
expect_output "  ... and says the daemon was killed, not that it refused" "was still running after"
expect_output "  ... and does not credit it with refusing" "it did not refuse, whatever it printed"

expect_exit 65 "a daemon that never handshakes -> 65 FAILED" \
    bash "$SMOKE" "$tb_no_handshake" 1.2.3
expect_output "  ... and says doctor never saw it" "did not report a running daemon"

# The regression that made every assertion above meaningless: `grep -qF -- ""`
# matches anything, so an empty version passed a good tarball, a wrong-version
# tarball, and everything in between.
expect_exit 64 "an EMPTY version -> 64, refused before anything is asserted" \
    bash "$SMOKE" "$tb_good" ""
expect_output "  ... and explains why an empty needle is not a check" "matches everything"

expect_exit 64 "an empty version cannot pass the WRONG-version tarball either" \
    bash "$SMOKE" "$tb_wrong_version" ""

expect_exit 64 "a prerelease version -> 64" bash "$SMOKE" "$tb_good" 1.2.3-rc.1
expect_exit 64 "a malformed version -> 64" bash "$SMOKE" "$tb_good" "latest"
expect_exit 64 "too few arguments -> 64" bash "$SMOKE" "$tb_good"
expect_exit 75 "tarball not found -> 75 UNCHECKED" bash "$SMOKE" "$work/absent.tar.gz" 1.2.3
expect_exit 75 "a tarball missing teton-code -> 75 UNCHECKED" \
    bash "$SMOKE" "$work/teton-v1.2.3-truncated.tar.gz" 1.2.3

expect_exit 64 "a nonsense deadline override -> 64" \
    env TETON_SMOKE_SEAM_DEADLINE_SECS=soon bash "$SMOKE" "$tb_good" 1.2.3

# THE CLOSED FAIL-OPEN (LESSON-443). The same known-good tarball, with the
# platform declaration taken away, so its name — `teton-v1.2.3-good.tar.gz` — is
# all smoke.sh has to go on. It used to reach a catch-all `*` branch that
# announced "artifact is unsigned in v1 (linux — by design)" and ran the four
# behavioural assertions with no signature check whatsoever, which is what a
# fourth release target, a typo in the matrix, or a `${target}` that expanded to
# nothing would have got: a green release, and a log line claiming a decision
# nobody made. It must now be 75 — the gate does not know whether these bytes
# were supposed to be signed, and "I do not know" is not "they are fine".
#
# No TETON_SMOKE_TEAM_ID is set, deliberately: an unrecognised target must stop
# the run before any question about which team signed it can be reached, so
# providing one would only obscure which branch produced the 75.
expect_exit 75 "a tarball whose target is not a release target -> 75 UNCHECKED" \
    env -u TETON_SMOKE_ASSUME_TARGET bash "$SMOKE" "$tb_good" 1.2.3
expect_output "  ... and names the target it did not recognise" \
    "'good' is not one of this release's targets"
# The specific wrong answer that used to be given, asserted as ABSENT: a linux
# claim about an artifact whose platform is unknown is a false statement, and it
# is the one this case exists to keep out of the log.
assert "  ... and does NOT claim it is the unsigned linux leg" \
    [ "${CASE_OUT#*unsigned in v1}" = "$CASE_OUT" ]

# The seam itself has to be a release target, or it is just the hole again with
# an environment variable in front of it.
expect_exit 64 "a TETON_SMOKE_ASSUME_TARGET that is not a release target -> 64" \
    env TETON_SMOKE_ASSUME_TARGET=whatever-i-like bash "$SMOKE" "$tb_good" 1.2.3

# KNOWN-BAD, and the second way to reach the seam branch — the one that looks
# identical to the first and is not. smoke.sh strips `teton-v<version>-` from
# the basename, and that strip SILENTLY does nothing when the version does not
# match, so a v9.9.9 tarball handed to a v1.2.3 run leaves the whole basename
# where a target triple should be. That is a wrong-artifact accident, not an
# unrecognised platform, and the seam must not be able to rename it into
# agreement: without the refusal this exercises the run would go on to smoke a
# tarball from another release as whatever the environment named.
tb_version_mismatch="$work/teton-v9.9.9-aarch64-apple-darwin.tar.gz"
cp "$tb_good" "$tb_version_mismatch"
expect_exit 75 "a tarball whose VERSION does not match the run -> 75, seam refused" \
    env TETON_SMOKE_ASSUME_TARGET=x86_64-unknown-linux-gnu \
    bash "$SMOKE" "$tb_version_mismatch" 1.2.3
expect_output "  ... and names it a version/name mismatch, not an unknown platform" \
    "version/name MISMATCH"

unset TETON_SMOKE_SEAM_DEADLINE_SECS TETON_SMOKE_HANDSHAKE_DEADLINE_SECS

# --- verify-signature.sh + verify-attestation.sh, as gates -----------------
#
# The question this file already asks of smoke.sh, asked of the two gates
# REQ-550 adds: are their assertions capable of FAILING? Every case below feeds
# one of them either an artifact built to be rejected or a tool built to be
# unreadable, and requires the documented verdict — 65 when something was
# learned and it is bad, 75 when nothing was learned, 0 for the one shape that
# deserves it (LESSON-454: build the known-bad input and watch it go red).
#
# WHAT THIS GROUP PROVES, AND WHAT IT DOES NOT (LESSON-433). It drives both
# gates through their `TETON_CODESIGN` / `TETON_GH` seams, so what is under
# test is the CLASSIFICATION: which answer earns which exit code. Apple's
# codesign and GitHub's gh are NOT run here, and nothing below is evidence
# about them, about any real signature, or about any real attestation. Those
# legs run in the release pipeline itself (ADR-550-4) — codesign on the macOS
# release jobs, gh in the release and verify-install jobs — and they stay
# recorded here as unrun rather than extrapolated from these greens. What the
# seams cannot do is soften a verdict: they change WHICH tool is asked and
# never how the answer is scored, and the rejecting cases below are exactly
# that claim, tested rather than asserted.
#
# ASSERTION PROVENANCE. No pass condition below is supplied by the harness.
# Each gate is a plain command whose own exit status IS the assertion; nothing
# times it out, nothing kills it, and no watchdog contributes a status that a
# case then reads as success. The five smoke legs at the end do run under
# smoke.sh's seam watchdog and the shortened deadline overrides, and both are
# one-directional by construction — the watchdog only ever writes the marker
# that turns a pass into a FAIL, and a shorter deadline can only make an
# assertion give up sooner. Neither can manufacture a green.

group "verify-signature.sh + verify-attestation.sh (do the new gates go red?)"

# The team the release must be signed by, and a team that is not it. Every
# fixture below is built from these two, so a case can never pass because the
# needle happened to appear somewhere else in the output.
SIG_TEAM_ID="545BU9G9D6"
SIG_OTHER_TEAM_ID="9Q7XZ4TR2K"

# make_codesign <dir> <mode> -> path to the stand-in on stdout
#
# A stand-in `codesign`, answering the three questions the release scripts ask:
# `--verify --strict` (silent, exit 0, when the seal holds), `-dvv` (the
# identity dump, written to STDERR, which is where the real tool writes it and
# why the gate reads it through 2>&1), and `--force --sign <identity> …`, which
# is package.sh's signing phase. The second `v` is why the dumps below carry
# Authority lines at all — at verbosity 1 codesign prints none.
#
# The mode names the artifact each fixture models:
#
#   accept       a Developer ID Application signature naming SIG_TEAM_ID. The
#                only shape a release may carry.
#   adhoc        KNOWN-BAD. `codesign -s -`, the linker's default signature on
#                Apple silicon: --verify ACCEPTS it — the seal is structurally
#                valid — while the dump names no authority at all. A binary
#                that is signed in form and unsigned in effect, shipped as a
#                release. This is the artifact BR-2 exists to catch, and the
#                reason the gate asks a second question.
#   development  KNOWN-BAD. A local Apple Development build shipped as a
#                release: right team, wrong kind of certificate. It CARRIES the
#                team id, which is what makes it the fixture that pins the
#                authority needle — every other known-bad here is independently
#                condemned by a missing team id, so without this one the
#                authority check could be deleted and the suite stay green.
#   foreign      KNOWN-BAD. A genuine Developer ID Application signature from
#                ANOTHER team — someone else's release, or ours re-signed. It
#                pins the team-id needle for the mirror-image reason.
#   reject       KNOWN-BAD. `--verify --strict` itself rejects the bytes: a
#                tampered or re-signed binary whose seal no longer covers what
#                shipped.
#   silent       NOT bad bytes: `-dvv` prints nothing after --verify accepted
#                them. The identity could not be READ, which is 75.
#   unrunnable   NOT bad bytes: resolvable on PATH, but exec fails (126/127).
#                The tool vanished between resolution and use.
#   crash        NOT bad bytes: the tool dies on a signal (exit 139), having
#                said nothing. A segfault, an OOM kill, a step timeout — every
#                one of them reachable without touching the artifact, so none
#                may be scored as a rejection.
#   dvv-exec     NOT bad bytes, and the nastiest of the lot: --verify accepts,
#                and then the `-dvv` invocation cannot be EXECUTED and the shell
#                says so on stderr. That diagnostic is non-empty output naming
#                neither a Developer ID authority nor a team id, so a gate that
#                scored the OUTPUT alone read it as a perfectly formed rejection
#                and announced an unsigned release on the strength of a lost
#                interpreter — a 65 forged by an exec failure (LESSON-442).
#   mixed        KNOWN-BAD, statefully: keyed on the binary's NAME, it rejects
#                `teton` and goes silent on `teton-code`. One rejection and one
#                unreadable sibling in the same tarball, which must aggregate to
#                65 — a definite rejection outranks an unreadable one, because
#                something WAS learned about these bytes.
#   sign-reject  KNOWN-BAD, on the OTHER side of the fence: `--verify` and
#                `-dvv` behave, but `--force --sign` fails the way a missing or
#                expired certificate fails. package.sh must refuse to write a
#                tarball rather than ship what it could not sign.
make_codesign() {
    local dir="$1" mode="$2" path
    mkdir -p "$dir"
    path="$dir/codesign"

    if [ "$mode" = unrunnable ]; then
        # Executable, so command -v resolves it; the interpreter does not
        # exist, so running it fails with 126 (or 127 — the gate treats both as
        # "could not be run", which is the point).
        printf '#!/nonexistent/teton-selftest-interpreter\nexit 0\n' >"$path"
        chmod +x "$path"
        printf '%s\n' "$path"
        return 0
    fi

    cat >"$path" <<EOF
#!/usr/bin/env bash
# selftest stand-in for Apple's codesign. Not codesign, and no evidence about
# it — it reproduces the shapes of the answers the real tool gives.
mode="$mode"
team="$SIG_TEAM_ID"
other="$SIG_OTHER_TEAM_ID"

# Dying before any argument is looked at: a crash is not a verdict about one
# question, it is the tool not being there for any of them.
if [ "\$mode" = crash ]; then
    kill -SEGV \$\$
fi

case "\${1:-}" in
    --force | --sign)
        # package.sh's signing phase:
        #   codesign --force --sign <identity> --timestamp --options runtime <f>
        if [ "\$mode" = sign-reject ]; then
            echo "Warning: unable to build chain to self-signed root for signer" >&2
            echo "errSecInternalComponent" >&2
            exit 1
        fi
        exit 0
        ;;
    --verify)
        if [ "\$mode" = reject ]; then
            echo "\${3:-}: a sealed resource is missing or invalid" >&2
            exit 1
        fi
        # Stateful, keyed on the binary's name rather than on a counter: a
        # counter would depend on the order the gate happens to walk the pair,
        # which is exactly the thing the gate is free to change.
        if [ "\$mode" = mixed ] && [ "\$(basename "\${3:-}")" = teton ]; then
            echo "\${3:-}: a sealed resource is missing or invalid" >&2
            exit 1
        fi
        exit 0
        ;;
    -dvv)
        if [ "\$mode" = silent ]; then
            exit 0
        fi
        # The sibling of the rejected binary in the "mixed" fixture: --verify
        # accepted it, and its identity cannot be read.
        if [ "\$mode" = mixed ]; then
            exit 0
        fi
        if [ "\$mode" = dvv-exec ]; then
            # What the SHELL prints when it cannot execute a program, on the
            # stream the gate reads, with the status it uses to say so.
            echo "\$0: /nonexistent/teton-selftest-interpreter: bad interpreter: No such file or directory" >&2
            exit 127
        fi
        echo "Executable=\${2:-}" >&2
        echo "Identifier=teton" >&2
        echo "Format=Mach-O thin (arm64)" >&2
        case "\$mode" in
            adhoc)
                echo "CodeDirectory v=20400 size=1337 flags=0x2(adhoc) hashes=40+2 location=embedded" >&2
                echo "Signature=adhoc" >&2
                echo "Info.plist=not bound" >&2
                echo "TeamIdentifier=not set" >&2
                ;;
            development)
                echo "CodeDirectory v=20500 size=1337 flags=0x10000(runtime) hashes=40+2 location=embedded" >&2
                echo "Signature size=8983" >&2
                echo "Authority=Apple Development: releng@atelier.example (\$team)" >&2
                echo "Authority=Apple Worldwide Developer Relations Certification Authority" >&2
                echo "Authority=Apple Root CA" >&2
                echo "TeamIdentifier=\$team" >&2
                ;;
            foreign)
                echo "CodeDirectory v=20500 size=1337 flags=0x10000(runtime) hashes=40+2 location=embedded" >&2
                echo "Signature size=8983" >&2
                echo "Authority=Developer ID Application: Someone Else Ltd (\$other)" >&2
                echo "Authority=Developer ID Certification Authority" >&2
                echo "Authority=Apple Root CA" >&2
                echo "TeamIdentifier=\$other" >&2
                ;;
            *)
                echo "CodeDirectory v=20500 size=1337 flags=0x10000(runtime) hashes=40+2 location=embedded" >&2
                echo "Signature size=8983" >&2
                echo "Authority=Developer ID Application: Atelier Fashion LLC (\$team)" >&2
                echo "Authority=Developer ID Certification Authority" >&2
                echo "Authority=Apple Root CA" >&2
                echo "TeamIdentifier=\$team" >&2
                ;;
        esac
        exit 0
        ;;
esac

echo "codesign stand-in: unexpected invocation: \$*" >&2
exit 2
EOF
    chmod +x "$path"
    printf '%s\n' "$path"
}

# make_gh <dir> <mode> -> path to the stand-in on stdout
#
# A stand-in `gh`, answering
# `gh attestation verify --repo <repo> [--signer-workflow <w>] -- <artifact>`:
#
#   accept        the subject verifies; gh's summary names the workflow and the
#                 tag the bytes were built from, which is the point of asking.
#   silent-accept NOT bad: exit 0 with no output at all. A future gh that stops
#                 printing a summary, or one whose output was swallowed, must
#                 still be a PASS — the verdict is the status, and the summary
#                 is evidence for the log rather than the thing being scored.
#   reject        KNOWN-BAD. gh reached the attestation store and no attestation
#                 there covers this digest. `Error: no attestations found` is
#                 the string gh 2.96.0 actually prints for it, observed against
#                 the live tool; the earlier fixture here invented a
#                 "Verification failed: …" line that no version prints.
#   reject-nomatch  the same verdict in gh's other wording, pinning the second
#                 alternative of REJECTION_PATTERN so it cannot be deleted
#                 without a case going red.
#   notfound      NOT a verdict, and the case that pins the ordering. Observed
#                 against gh 2.96.0: an artifact whose digest is not in a
#                 repository's attestation store — including EVERY artifact in a
#                 repository that has never published one — comes back as an
#                 HTTP 404 on the attestations endpoint. That 404 is
#                 indistinguishable from a repo that does not exist, a repo this
#                 token cannot see, and a degraded API, so it must land on 75.
#   error         NOT a verdict: the API could not be reached. This must land on
#                 75, because an offline runner announcing a supply-chain
#                 failure is both a lie and the reason nobody believes the next
#                 alarm.
#   crash         NOT a verdict: the tool dies on a signal, having said nothing.
#   forged        NOT a verdict: the tool exits 65 — the gate's own FAILED code
#                 — with no verdict in its output. LESSON-442's collision, in
#                 the one direction that matters: 65 must mean "these bytes are
#                 bad" and must not be inheritable from a subprocess's status.
#   echoargs      not a fixture about verdicts at all: it prints its own argv so
#                 a case can assert what the gate ASKED, which is the only way
#                 to test that `--signer-workflow` is forwarded and that `--`
#                 precedes the artifact.
#   one-bad       stateful, for the batch gate: rejects the aarch64 tarball and
#                 accepts the other two.
#   one-error     stateful likewise: the aarch64 tarball comes back as a network
#                 failure, the other two verify.
#   one-bad-one-error  stateful, and the ONLY fixture in which the batch gate's
#                 aggregation ordering is decisive: one tarball is rejected and
#                 a DIFFERENT one transport-errors in the same run. Under
#                 "rejection outranks unchecked" that is 65; under either the
#                 opposite ordering or a last-writer-wins accumulator it is 75,
#                 and every other batch case passes both ways.
#   sums-bad      stateful, keyed on checksums.txt rather than on a triple: the
#                 three tarballs verify and the DIGEST LIST does not. Nothing
#                 else in this suite fails if checksums.txt stops being verified
#                 as a subject of its own.
make_gh() {
    local dir="$1" mode="$2" path
    mkdir -p "$dir"
    path="$dir/gh"

    cat >"$path" <<EOF
#!/usr/bin/env bash
# selftest stand-in for gh. Not gh, and no evidence about it — it reproduces
# the shapes of the answers the real tool gives. It hashes nothing: which
# digest an attestation covers is gh's question, asked for real in the release
# and verify-install jobs.
mode="$mode"
digest="sha256:0f1e2d3c4b5a69788796a5b4c3d2e1f00f1e2d3c4b5a69788796a5b4c3d2e1f0"

# The subject is the LAST argument, not \$3. The gate now invokes
# \`attestation verify --repo R [--signer-workflow W] -- <artifact>\`, so the
# artifact's position depends on whether a signer workflow was passed; only
# "last" is stable across both shapes.
subject="artifact"
for _a in "\$@"; do subject="\$_a"; done

# The stateful fixtures pick their answer from the subject's name. Done before
# the case below so each mode stays a single branch.
case "\$mode" in
    one-bad)
        case "\$subject" in
            *aarch64-apple-darwin*) mode=reject ;;
            *) mode=accept ;;
        esac
        ;;
    one-error)
        case "\$subject" in
            *aarch64-apple-darwin*) mode=error ;;
            *) mode=accept ;;
        esac
        ;;
    one-bad-one-error)
        case "\$subject" in
            *aarch64-apple-darwin*) mode=reject ;;
            *x86_64-apple-darwin*) mode=error ;;
            *) mode=accept ;;
        esac
        ;;
    sums-bad)
        case "\$subject" in
            *checksums.txt) mode=reject ;;
            *) mode=accept ;;
        esac
        ;;
esac

case "\$mode" in
    accept)
        echo "Loaded digest \$digest for file://\$subject"
        echo "Loaded 1 attestation from GitHub API"
        echo ""
        echo "Verification succeeded!"
        echo ""
        echo "\$digest was attested by:"
        echo "REPO                        PREDICATE_TYPE                  WORKFLOW"
        # The WORKFLOW column carries the SAN's real shape — the whole
        # \`https://github.com/<owner>/<repo>/<path>@<ref>\` URI. The fixture used
        # to print a bare \`.github/workflows/release.yml@refs/tags/v1.2.3\`,
        # which is not a string gh has ever emitted, and a fixture that invents
        # a shorter shape than the tool is how a call site ends up passing a
        # bare path to \`--signer-workflow\` with a green suite behind it.
        echo "atelier-fashion/teton-code  https://slsa.dev/provenance/v1  https://github.com/atelier-fashion/teton-code/.github/workflows/release.yml@refs/tags/v1.2.3"
        exit 0
        ;;
    silent-accept)
        exit 0
        ;;
    reject)
        echo "Error: no attestations found" >&2
        exit 1
        ;;
    reject-nomatch)
        echo "Error: no matching attestations found for subject \$digest" >&2
        exit 1
        ;;
    notfound)
        echo "Error: HTTP 404: Not Found (https://api.github.com/repos/atelier-fashion/teton-code/attestations/\$digest?per_page=30&predicate_type=https%3A%2F%2Fslsa.dev%2Fprovenance%2Fv1)" >&2
        exit 1
        ;;
    error)
        echo "error: failed to fetch attestations from api.github.com: Post \"https://api.github.com/graphql\": dial tcp: lookup api.github.com: i/o timeout" >&2
        exit 1
        ;;
    crash)
        kill -SEGV \$\$
        ;;
    forged)
        exit 65
        ;;
    echoargs)
        printf 'ARGV:'
        printf ' [%s]' "\$@"
        printf '\n'
        exit 0
        ;;
esac

echo "gh stand-in: unexpected invocation: \$*" >&2
exit 2
EOF
    chmod +x "$path"
    printf '%s\n' "$path"
}

cs_accept="$(make_codesign "$work/cs-accept" accept)"
cs_adhoc="$(make_codesign "$work/cs-adhoc" adhoc)"
cs_development="$(make_codesign "$work/cs-development" development)"
cs_foreign="$(make_codesign "$work/cs-foreign" foreign)"
cs_reject="$(make_codesign "$work/cs-reject" reject)"
cs_silent="$(make_codesign "$work/cs-silent" silent)"
cs_unrunnable="$(make_codesign "$work/cs-unrunnable" unrunnable)"
cs_crash="$(make_codesign "$work/cs-crash" crash)"
cs_dvv_exec="$(make_codesign "$work/cs-dvv-exec" dvv-exec)"
cs_mixed="$(make_codesign "$work/cs-mixed" mixed)"
cs_sign_reject="$(make_codesign "$work/cs-sign-reject" sign-reject)"

gh_accept="$(make_gh "$work/gh-accept" accept)"
gh_silent_accept="$(make_gh "$work/gh-silent-accept" silent-accept)"
gh_reject="$(make_gh "$work/gh-reject" reject)"
gh_reject_nomatch="$(make_gh "$work/gh-reject-nomatch" reject-nomatch)"
gh_notfound="$(make_gh "$work/gh-notfound" notfound)"
gh_error="$(make_gh "$work/gh-error" error)"
gh_crash="$(make_gh "$work/gh-crash" crash)"
gh_forged="$(make_gh "$work/gh-forged" forged)"
gh_echoargs="$(make_gh "$work/gh-echoargs" echoargs)"
gh_one_bad="$(make_gh "$work/gh-one-bad" one-bad)"
gh_one_error="$(make_gh "$work/gh-one-error" one-error)"
gh_mixed="$(make_gh "$work/gh-mixed" one-bad-one-error)"
gh_sums_bad="$(make_gh "$work/gh-sums-bad" sums-bad)"

# One extracted binary, and the two tarballs whose NAMES decide which leg
# smoke.sh thinks it is on. Their contents are the same known-good stand-in
# pair the smoke group already uses, so in every case below the only dimension
# that varies is the signature.
sig_bin="$work/sig/teton"
mkdir -p "$work/sig"
printf 'stand-in binary; the signature question is answered by the stand-in codesign\n' \
    >"$sig_bin"

tb_darwin="$(make_tarball aarch64-apple-darwin 1.2.3 yes yes)"
tb_linux="$(make_tarball x86_64-unknown-linux-gnu 1.2.3 yes yes)"

# --- verify-signature.sh ---------------------------------------------------

expect_exit 0 "a Developer ID signature naming the right team -> 0" \
    env TETON_CODESIGN="$cs_accept" bash "$VERIFY_SIGNATURE" "$sig_bin" "$SIG_TEAM_ID"
expect_output "  ... and names the certificate it accepted" \
    "is signed with a Developer ID Application certificate naming team $SIG_TEAM_ID"

# KNOWN-BAD: the linker's default ad-hoc signature, shipped unsigned in effect.
expect_exit 65 "an AD-HOC signed binary -> 65 FAILED (BR-2)" \
    env TETON_CODESIGN="$cs_adhoc" bash "$VERIFY_SIGNATURE" "$sig_bin" "$SIG_TEAM_ID"
expect_output "  ... and refuses it as a release signature" \
    "an ad-hoc or foreign signature is not a release signature (BR-2)"
# The reason the gate asks twice: these bytes PASSED --verify --strict, and a
# gate that stopped there would have called them signed.
expect_output "  ... having been accepted by --verify --strict first" "Signature=adhoc"

# KNOWN-BAD: a local development build shipped as a release. Right team, wrong
# certificate — so this is the case that pins the authority needle.
expect_exit 65 "a binary signed with an Apple DEVELOPMENT certificate -> 65 FAILED" \
    env TETON_CODESIGN="$cs_development" bash "$VERIFY_SIGNATURE" "$sig_bin" "$SIG_TEAM_ID"
expect_output "  ... and blames the authority alone, since the team id is right" \
    "a Developer ID Application authority — an ad-hoc"

# KNOWN-BAD: someone else's Developer ID — the mirror case, pinning the team id.
expect_exit 65 "a Developer ID signature from ANOTHER team -> 65 FAILED" \
    env TETON_CODESIGN="$cs_foreign" bash "$VERIFY_SIGNATURE" "$sig_bin" "$SIG_TEAM_ID"
expect_output "  ... and blames the team id alone, since the authority is right" \
    "does not name team id $SIG_TEAM_ID"

# KNOWN-BAD: a tampered or re-signed binary — the seal no longer covers these
# bytes, and codesign says so itself.
expect_exit 65 "codesign --verify --strict REJECTING the bytes -> 65 FAILED" \
    env TETON_CODESIGN="$cs_reject" bash "$VERIFY_SIGNATURE" "$sig_bin" "$SIG_TEAM_ID"
expect_output "  ... and quotes codesign's own complaint" \
    "a sealed resource is missing or invalid"

# The three ways to learn NOTHING. None of them may be scored as a rejection:
# a broken environment must not be able to announce bad bytes (LESSON-442).
expect_exit 75 "a codesign whose -dvv prints nothing -> 75 UNCHECKED, not 65" \
    env TETON_CODESIGN="$cs_silent" bash "$VERIFY_SIGNATURE" "$sig_bin" "$SIG_TEAM_ID"
expect_output "  ... and says the identity could not be read" "printed nothing"

expect_exit 75 "a codesign that resolves but cannot be RUN -> 75 UNCHECKED, not 65" \
    env TETON_CODESIGN="$cs_unrunnable" bash "$VERIFY_SIGNATURE" "$sig_bin" "$SIG_TEAM_ID"
expect_output "  ... and says so as a tooling failure" "did not run to a verdict"

expect_exit 75 "no codesign on this machine -> 75 UNCHECKED" \
    env TETON_CODESIGN=/nonexistent/codesign bash "$VERIFY_SIGNATURE" "$sig_bin" "$SIG_TEAM_ID"
expect_output "  ... and says the gate belongs to the macOS legs" "is not on this machine"

# A codesign that DIES ON A SIGNAL. The shell reports that as 128+N — 139 for a
# segfault — and a plain `-ne 0` test read it as "codesign rejected these
# bytes", so a crashing tool could announce an unsigned release. Reachable
# without touching the artifact at all: an OOM kill or a cancelled CI step
# produces the same status.
expect_exit 75 "a codesign that dies on a SIGNAL -> 75 UNCHECKED, not a forged 65" \
    env TETON_CODESIGN="$cs_crash" bash "$VERIFY_SIGNATURE" "$sig_bin" "$SIG_TEAM_ID"
expect_output "  ... and says it never reached a verdict" "did not run to a verdict"

# The subtler half of the same hole. --verify accepts, and then the SHELL fails
# to execute the `-dvv` invocation and prints its own diagnostic — non-empty
# output that names no authority and no team id. Scored as a dump, that reads as
# a textbook rejection, and the gate would blame the artifact for a lost
# interpreter.
expect_exit 75 "a codesign whose -dvv cannot be EXECUTED -> 75, never a forged 65" \
    env TETON_CODESIGN="$cs_dvv_exec" bash "$VERIFY_SIGNATURE" "$sig_bin" "$SIG_TEAM_ID"
expect_output "  ... and says the identity was never read, not that it is wrong" \
    "an identity that was never READ is not an identity that is wrong"

# Rejection outranks unreadability, tested rather than asserted: one binary of
# the pair is REJECTED and its sibling's identity cannot be read. 75 would be
# the weaker, less useful claim — something WAS learned about these bytes.
expect_exit 65 "one binary rejected + one unreadable -> 65, the rejection outranks" \
    env TETON_CODESIGN="$cs_mixed" bash "$VERIFY_SIGNATURE" "$tb_darwin" "$SIG_TEAM_ID"
expect_output "  ... and reports the rejection rather than the unreadable sibling" \
    "are not Developer ID signed by team $SIG_TEAM_ID"

expect_exit 75 "artifact not found -> 75 UNCHECKED" \
    env TETON_CODESIGN="$cs_accept" bash "$VERIFY_SIGNATURE" "$work/absent-binary" "$SIG_TEAM_ID"
expect_output "  ... and says nothing was checked" "not found:"

expect_exit 64 "too few arguments -> 64" bash "$VERIFY_SIGNATURE" "$sig_bin"
expect_exit 64 "an EMPTY artifact argument -> 64, not a confusing 75" \
    env TETON_CODESIGN="$cs_accept" bash "$VERIFY_SIGNATURE" "" "$SIG_TEAM_ID"
expect_output "  ... and says no artifact was given" "no artifact given"

# Whitespace in a team id is a call-site mistake — no Apple team id contains
# any — and it must be a usage error rather than a report that the artifact is
# unsigned, which is what a needle of " 545BU9G9D6" would have produced.
expect_exit 64 "a team id containing WHITESPACE -> 64, not a false rejection" \
    env TETON_CODESIGN="$cs_accept" bash "$VERIFY_SIGNATURE" "$sig_bin" " $SIG_TEAM_ID"
expect_output "  ... and says why" "must not contain whitespace"
expect_exit 64 "an EMPTY team id -> 64, refused before anything is checked" \
    env TETON_CODESIGN="$cs_accept" bash "$VERIFY_SIGNATURE" "$sig_bin" ""
# The same class of hole as smoke.sh's empty version: an empty needle is
# contained in every string, so this would have passed the ad-hoc fixture above.
expect_output "  ... and explains that an empty needle matches any signature" \
    "matched as a literal substring"

# A tarball is checked as the PAIR it contains (BR-6, LESSON-455): a green on
# one binary is not evidence about the other, so both names must appear.
expect_exit 0 "a tarball is checked as both binaries, not one -> 0" \
    env TETON_CODESIGN="$cs_accept" bash "$VERIFY_SIGNATURE" "$tb_darwin" "$SIG_TEAM_ID"
expect_output "  ... and names both of them" "every binary checked (teton, teton-code)"

# KNOWN-BAD, and the reason that matters: a tarball shipping only one binary
# must not earn a green on the strength of the one it does ship.
expect_exit 75 "a tarball missing teton-code -> 75 UNCHECKED, never a pass" \
    env TETON_CODESIGN="$cs_accept" bash "$VERIFY_SIGNATURE" \
    "$work/teton-v1.2.3-truncated.tar.gz" "$SIG_TEAM_ID"
expect_output "  ... and names the binary it never saw" "teton-code is missing from"

# --- verify-attestation.sh -------------------------------------------------

att_repo="atelier-fashion/teton-code"

# A genuinely different byte sequence, not a relabelled copy: the fixture this
# case NAMES is a tarball altered after it was attested. The stand-in rejects
# by construction rather than by hashing — comparing a digest against a real
# attestation is gh's job, and it is done for real in the release job.
att_tampered="$work/teton-v1.2.3-x86_64-unknown-linux-gnu-tampered.tar.gz"
cp "$tb_linux" "$att_tampered"
printf 'one flipped byte too many\n' >>"$att_tampered"
refute "the tampered fixture really does differ from the attested tarball" \
    cmp -s "$tb_linux" "$att_tampered"

expect_exit 0 "an artifact gh verifies -> 0" \
    env TETON_GH="$gh_accept" bash "$VERIFY_ATTESTATION" "$tb_linux" "$att_repo"
expect_output "  ... and keeps gh's provenance summary in the release log" \
    "https://slsa.dev/provenance/v1"

# The verdict is the STATUS. A gh that verifies and prints nothing is still a
# pass — the summary is evidence for the log, not the thing being scored — and a
# gate that required output would go red on a future gh that stopped printing
# one, in the direction a release gate must never be wrong.
expect_exit 0 "a gh that verifies SILENTLY (exit 0, no output) -> 0" \
    env TETON_GH="$gh_silent_accept" bash "$VERIFY_ATTESTATION" "$tb_linux" "$att_repo"
expect_output "  ... and still says PASS" "PASS"

# The value every caller in this repository passes, written once here so the
# cases below assert the SHAPE and not merely that some string travelled.
#
# Fully qualified — `<owner>/<repo>/<path>` — because gh matches this against
# the certificate's whole `https://github.com/<owner>/<repo>/<path>@<ref>` SAN.
# The bare `.github/workflows/release.yml` this suite used to pass matches no
# certificate at all: it does not fail as a rejection, it comes back as
# `Error: verifying with issuer "sigstore.dev"` and lands 75, on every asset,
# on every release. A gate that can never pass and a gate that always passes
# are the same kind of bug, and the old assertion — "a string reached gh" —
# could not tell either from a working one.
att_signer="atelier-fashion/teton-code/.github/workflows/release.yml"

# What the gate ASKED, not just how it scored the answer. Forwarding
# --signer-workflow is the difference between "some workflow in this repo
# attested these bytes" and "the release workflow did", and nothing else here
# would notice if the argument were dropped on the floor.
expect_exit 0 "the signer workflow reaches gh as --signer-workflow" \
    env TETON_GH="$gh_echoargs" bash "$VERIFY_ATTESTATION" "$tb_linux" "$att_repo" \
    "$att_signer"
expect_output "  ... as a flag and a value" \
    "[--signer-workflow] [$att_signer]"
# The owner/repo prefix specifically, as its own case. The assertion above is
# satisfied by whatever this file happens to define $att_signer as, so it would
# stay green if that constant were quietly shortened back to a bare path. This
# one names the prefix literally: the value gh receives must begin with the
# repository, which is the whole difference between a SAN that matches and one
# that cannot.
expect_output "  ... FULLY QUALIFIED, carrying the owner/repo prefix" \
    "[--signer-workflow] [atelier-fashion/teton-code/.github/"
# `--` before the subject, and AFTER the flags: verified against gh 2.96.0,
# which reads a `--` placed before `--repo` as making --repo positional and dies
# with "too many arguments".
expect_output "  ... with -- closing flag parsing before the artifact path" \
    "[--] [$tb_linux]"
expect_output "  ... and it is announced in the log" "signed by $att_signer"

expect_exit 0 "no signer workflow given -> the flag is not passed at all" \
    env TETON_GH="$gh_echoargs" bash "$VERIFY_ATTESTATION" "$tb_linux" "$att_repo"
# Not `--signer-workflow ''`: gh enforces that value against the certificate's
# SubjectAlternativeName, and no certificate's SAN is the empty string, so an
# empty flag would turn every verification into a failure. Tested with a prefix
# strip rather than a grep because the needle is a fixed string and this is one
# expansion the caller performs before `assert` ever runs.
assert "  ... and no empty --signer-workflow was smuggled in" \
    [ "${CASE_OUT#*--signer-workflow}" = "$CASE_OUT" ]

# KNOWN-BAD: bytes that no attestation covers, because they are not the bytes
# that were built. This is the supply-chain alarm, and it must be reachable.
# `Error: no attestations found` is what gh 2.96.0 prints, observed against the
# live tool — the fixture used to invent a "Verification failed: …" line.
expect_exit 65 "a TAMPERED artifact gh rejects -> 65 FAILED" \
    env TETON_GH="$gh_reject" bash "$VERIFY_ATTESTATION" "$att_tampered" "$att_repo"
expect_output "  ... and says the bytes do not verify" "these bytes do not verify against"
expect_output "  ... and quotes gh's verdict" "no attestations found"

# gh's other wording for the same verdict. Without this case the second
# alternative of REJECTION_PATTERN could be deleted and the suite stay green.
expect_exit 65 "gh's 'no matching attestations' wording is also a rejection -> 65" \
    env TETON_GH="$gh_reject_nomatch" bash "$VERIFY_ATTESTATION" "$att_tampered" "$att_repo"
expect_output "  ... and quotes it" "no matching attestations found"

# Everything that is NOT a verdict. Each of these is a failing gh, and none of
# them may be reported as a supply-chain failure (LESSON-442).
expect_exit 75 "gh failing on the NETWORK -> 75 UNCHECKED, not 65" \
    env TETON_GH="$gh_error" bash "$VERIFY_ATTESTATION" "$tb_linux" "$att_repo"
expect_output "  ... and says why that is deliberate" \
    "must never be announced as a supply-chain alarm"

# THE 404, and the reason UNCHECKED_PATTERN is tested before REJECTION_PATTERN.
# Observed against gh 2.96.0: a digest that is not in a repository's attestation
# store — which is every artifact, in every repository that has not published an
# attestation yet — comes back as an HTTP 404 on the attestations endpoint, not
# as a verdict. It is indistinguishable from a repo that does not exist, a repo
# the token cannot see, and a degraded API, so 75 is the only honest score. The
# release is blocked either way; only the claim in the log differs.
expect_exit 75 "gh's 404-before-the-first-attestation -> 75 UNCHECKED, not 65" \
    env TETON_GH="$gh_notfound" bash "$VERIFY_ATTESTATION" "$tb_linux" "$att_repo"
expect_output "  ... and blames the environment, not the bytes" "an ENVIRONMENT failure"

expect_exit 75 "gh dying on a signal, having said nothing -> 75 UNCHECKED" \
    env TETON_GH="$gh_crash" bash "$VERIFY_ATTESTATION" "$tb_linux" "$att_repo"
# Decided on the STATUS (>=128), before either pattern is consulted, and the
# needle says so. Reaching the final catch-all instead would be the same verdict
# arrived at by luck — it survives only as long as nothing a shell or a kernel
# prints ever happens to contain a REJECTION_PATTERN phrase.
expect_output "  ... on the status alone, before any pattern is consulted" \
    "did not run to a verdict"

# The collision itself: a tool whose own status is 65. If that were inherited,
# 65 would stop meaning "these bytes are bad".
expect_exit 75 "gh exiting 65 with no verdict -> 75, never a forged FAILED" \
    env TETON_GH="$gh_forged" bash "$VERIFY_ATTESTATION" "$tb_linux" "$att_repo"
expect_output "  ... and does not inherit the tool's status" "without a verification verdict"

expect_exit 75 "no gh on this machine -> 75 UNCHECKED" \
    env TETON_GH=/nonexistent/gh bash "$VERIFY_ATTESTATION" "$tb_linux" "$att_repo"
expect_output "  ... and says a missing tool is not provenance" \
    "not evidence that the artifact has provenance"

expect_exit 75 "artifact not found -> 75 UNCHECKED" \
    env TETON_GH="$gh_accept" bash "$VERIFY_ATTESTATION" "$work/absent.tar.gz" "$att_repo"

expect_exit 64 "too few arguments -> 64" bash "$VERIFY_ATTESTATION" "$tb_linux"
expect_exit 64 "an EMPTY artifact argument -> 64, not a confusing 75" \
    env TETON_GH="$gh_accept" bash "$VERIFY_ATTESTATION" "" "$att_repo"
expect_output "  ... and says no artifact was given" "no artifact given"

# Every malformed <owner/repo> shape the guard names, one case each. `gh` will
# happily search a repository that is not this one, so a typo here is not a
# harmless mistake: it is a gate verifying provenance against the wrong trust
# root, and every one of these must be a usage error rather than the UNCHECKED
# that gh's own argument error would have produced.
for bad_repo in "" "a/b/c" "/lead" "trail/" ".dot" "-dash"; do
    expect_exit 64 "a malformed repo '$bad_repo' -> 64" \
        env TETON_GH="$gh_accept" bash "$VERIFY_ATTESTATION" "$tb_linux" "$bad_repo"
done
expect_exit 64 "a repo that is not owner/repo -> 64" \
    env TETON_GH="$gh_accept" bash "$VERIFY_ATTESTATION" "$tb_linux" "teton-code"

# --- verify-attestations-batch.sh ------------------------------------------
#
# The gate that asks the per-artifact question of a WHOLE release. What it adds
# over a loop is the two assertions a loop cannot make: that there are three
# tarballs, and that checksums.txt describes the bytes actually present. Both
# failures are silent in a loop — it iterates over what is there and passes.

group "verify-attestations-batch.sh (does a partial or drifted release go red?)"

# batch_dir <name> — a release directory holding the three tarballs and a
# checksums.txt computed from those exact bytes. Printed on stdout.
batch_dir() {
    local name="$1" dir="$work/batch-$1" t
    mkdir -p "$dir"
    for t in aarch64-apple-darwin x86_64-apple-darwin x86_64-unknown-linux-gnu; do
        printf 'pretend %s tarball for the batch gate\n' "$t" >"$dir/teton-v1.2.3-$t.tar.gz"
    done
    : >"$dir/checksums.txt"
    for t in aarch64-apple-darwin x86_64-apple-darwin x86_64-unknown-linux-gnu; do
        printf '%s  %s\n' "$(sha256_of "$dir/teton-v1.2.3-$t.tar.gz")" \
            "teton-v1.2.3-$t.tar.gz" >>"$dir/checksums.txt"
    done
    printf '%s\n' "$dir"
}

batch_ok="$(batch_dir ok)"

expect_exit 0 "three tarballs, matching checksums, all attested -> 0" \
    env TETON_GH="$gh_accept" bash "$VERIFY_BATCH" "$batch_ok" "$att_repo"
expect_output "  ... and says all three passed" "all 3 tarballs"
# The count of TARBALLS is 3 and the count of verified SUBJECTS is 4. Asserted
# separately so neither can quietly absorb the other: a gate that stopped
# verifying checksums.txt would still say "all 3 tarballs".
expect_output "  ... and that checksums.txt was a subject too" "all 4 subjects"

expect_exit 0 "the signer workflow is forwarded to each artifact's gate" \
    env TETON_GH="$gh_echoargs" bash "$VERIFY_BATCH" "$batch_ok" "$att_repo" \
    "$att_signer"
expect_output "  ... reaching gh" "[--signer-workflow] [$att_signer]"
# Same reasoning as the per-artifact case: assert the owner/repo prefix
# literally, so shortening $att_signer back to a bare path cannot stay green.
expect_output "  ... FULLY QUALIFIED, carrying the owner/repo prefix" \
    "[--signer-workflow] [atelier-fashion/teton-code/.github/"
# And it reaches the FOURTH subject too. A batch that forwarded the constraint
# for the tarballs and dropped it for checksums.txt would verify the digest list
# against "any workflow in this repository", which is the weaker question this
# argument exists to stop being asked.
expect_output "  ... including for checksums.txt" "[--] [$batch_ok/checksums.txt]"

# KNOWN-BAD: one of three rejected. The other two verify, which is the point —
# a release is only as attested as its least attested artifact (BR-6).
expect_exit 65 "1 of 3 artifacts REJECTED -> 65, however green the other two are" \
    env TETON_GH="$gh_one_bad" bash "$VERIFY_BATCH" "$batch_ok" "$att_repo"
expect_output "  ... and names the artifact gh rejected" \
    "gh rejected teton-v1.2.3-aarch64-apple-darwin.tar.gz"

expect_exit 75 "1 of 3 artifacts UNCHECKED -> 75, never a pass on the other two" \
    env TETON_GH="$gh_one_error" bash "$VERIFY_BATCH" "$batch_ok" "$att_repo"
expect_output "  ... and says nothing was learned" "could not be verified against"

# THE MIXED RUN, and the only case in this group whose verdict the aggregation
# ORDER decides. One tarball is rejected; a different one transport-errors. Both
# counters are non-zero when the run ends, so this is 65 if and only if
# "rejection outranks unchecked" holds. Every other case here sets exactly one
# counter and would stay green under either ordering — which is why the ordering
# survived unasserted, and why breaking it deliberately (swap the two blocks at
# the bottom of verify-attestations-batch.sh) must turn THIS case red and only
# this one.
#
# The ordering is not a preference. A 65 means something was learned about this
# release and it is bad; losing that to a 75 because a sibling artifact could not
# be reached would make the log claim less than the run knows.
expect_exit 65 "1 REJECTED + 1 UNCHECKED in the same run -> 65: rejection outranks" \
    env TETON_GH="$gh_mixed" bash "$VERIFY_BATCH" "$batch_ok" "$att_repo"
expect_output "  ... naming the rejected artifact" \
    "gh rejected teton-v1.2.3-aarch64-apple-darwin.tar.gz"
# Both findings are in the log even though only one decides the exit code: a
# gate that reported the rejection and swallowed the unreachable sibling would
# send whoever reads it looking at two verified artifacts that were nothing of
# the kind.
expect_output "  ... and still reporting the artifact it could not reach" \
    "no provenance verdict for teton-v1.2.3-x86_64-apple-darwin.tar.gz"

# KNOWN-BAD, and the case that exists because checksums.txt is a SUBJECT and not
# a bystander. All three tarballs verify and hash to their recorded digests; the
# digest list itself is not attested. Without the fourth verification this is a
# clean pass — and a clean pass here is a release where an attacker who can
# replace assets publishes their own digests for their own bytes, and every
# reader following the runbook's `shasum -c` verifies against the attacker's
# numbers and sees green.
expect_exit 65 "the tarballs verify and checksums.txt does NOT -> 65" \
    env TETON_GH="$gh_sums_bad" bash "$VERIFY_BATCH" "$batch_ok" "$att_repo"
expect_output "  ... naming checksums.txt as the rejected subject" \
    "gh rejected checksums.txt"

# KNOWN-BAD: one name, two recorded digests. A first-match reader resolves this
# silently — it would accept a file whose FIRST line matches the bytes on disk
# while a consumer's `shasum -c` may just as well act on the second. That is the
# shape an attacker appending to a checksums file produces, so the duplication
# itself is the finding.
batch_dup="$(batch_dir dup)"
printf '%s  %s\n' \
    "2222222222222222222222222222222222222222222222222222222222222222" \
    "teton-v1.2.3-aarch64-apple-darwin.tar.gz" >>"$batch_dup/checksums.txt"
expect_exit 65 "a name listed TWICE in checksums.txt -> 65, even when a line matches" \
    env TETON_GH="$gh_accept" bash "$VERIFY_BATCH" "$batch_dup" "$att_repo"
expect_output "  ... and names the duplication rather than a digest mismatch" \
    "is listed 2 times in checksums.txt"

# KNOWN-BAD: the tarballs are all attested and checksums.txt describes a
# different build. gh alone would pass this — it hashes the file in front of it
# — and the inconsistency would surface as a failed install, weeks later.
batch_drift="$(batch_dir drift)"
printf 'a byte the checksums file has never seen\n' \
    >>"$batch_drift/teton-v1.2.3-x86_64-apple-darwin.tar.gz"
expect_exit 65 "a tarball that does not match its checksums.txt line -> 65" \
    env TETON_GH="$gh_accept" bash "$VERIFY_BATCH" "$batch_drift" "$att_repo"
expect_output "  ... and names the file" \
    "teton-v1.2.3-x86_64-apple-darwin.tar.gz does not match its checksums.txt line"
# Named because a message that printed only the digests would be true and
# useless in a directory of three near-identical names.
expect_output "  ... and shows both digests" "recorded:"

# KNOWN-BAD, and the one a plain loop cannot catch: an upload that never
# arrived. Two of three tarballs, both perfect, and every check that runs
# passes.
batch_short="$(batch_dir short)"
rm -f "$batch_short/teton-v1.2.3-x86_64-unknown-linux-gnu.tar.gz"
expect_exit 65 "only 2 of 3 tarballs present -> 65, not a pass on the two" \
    env TETON_GH="$gh_accept" bash "$VERIFY_BATCH" "$batch_short" "$att_repo"
expect_output "  ... and says a release missing a platform is not a release" \
    "expected 3 tarballs"

batch_extra="$(batch_dir extra)"
printf 'a fourth tarball nobody asked for\n' \
    >"$batch_extra/teton-v1.2.3-riscv64-unknown-linux-gnu.tar.gz"
expect_exit 65 "a FOURTH tarball -> 65 as well: the count is exact, not a minimum" \
    env TETON_GH="$gh_accept" bash "$VERIFY_BATCH" "$batch_extra" "$att_repo"

batch_nosums="$(batch_dir nosums)"
rm -f "$batch_nosums/checksums.txt"
expect_exit 65 "no checksums.txt -> 65" \
    env TETON_GH="$gh_accept" bash "$VERIFY_BATCH" "$batch_nosums" "$att_repo"
expect_output "  ... and says what downstream loses without it" "is missing"

batch_unlisted="$(batch_dir unlisted)"
printf '%s  %s\n' \
    "1111111111111111111111111111111111111111111111111111111111111111" \
    "teton-v1.2.3-some-other-target.tar.gz" >"$batch_unlisted/checksums.txt"
expect_exit 65 "a tarball with no line in checksums.txt -> 65" \
    env TETON_GH="$gh_accept" bash "$VERIFY_BATCH" "$batch_unlisted" "$att_repo"
expect_output "  ... and says the file has no recorded digest" "has no line in checksums.txt"

expect_exit 64 "too few arguments -> 64" bash "$VERIFY_BATCH" "$batch_ok"
expect_exit 64 "no arguments at all -> 64" bash "$VERIFY_BATCH"
expect_exit 64 "an empty directory argument -> 64" \
    env TETON_GH="$gh_accept" bash "$VERIFY_BATCH" "" "$att_repo"
# A path that is not there is a CALL SITE mistake, not a malformed release, and
# it is deliberately not the 65 that an empty-but-present directory earns.
expect_exit 64 "a directory that does not exist -> 64, not 65" \
    env TETON_GH="$gh_accept" bash "$VERIFY_BATCH" "$work/no-such-release-dir" "$att_repo"
expect_output "  ... and says so" "not a directory"

# An empty but PRESENT directory is a release-shape failure: it was read, and
# what was read is not a release. `nullglob` is what makes this 65 rather than a
# confusing report about a file named `teton-*.tar.gz`.
mkdir -p "$work/batch-empty"
expect_exit 65 "an empty release directory -> 65, and no literal-glob nonsense" \
    env TETON_GH="$gh_accept" bash "$VERIFY_BATCH" "$work/batch-empty" "$att_repo"
expect_output "  ... having found zero tarballs" "found 0"

# --- the CI seam refusals --------------------------------------------------
#
# Every case above this line drives a stand-in through TETON_CODESIGN or
# TETON_GH, which is exactly the power these guards exist to deny: whoever sets
# those variables decides what a release signature is, or whether one exists.
# The guards are the reason that power is a TEST seam rather than a way to wave
# an unsigned release through CI.
#
# They have never been exercised. This suite exports TETON_ALLOW_TOOL_SEAM=1 at
# the top and does not set GITHUB_ACTIONS, so the guards' condition has been
# false for the whole run — every one of them could be deleted, or inverted, and
# this file would stay green. The whole of that protection rested on three
# unread `if`s.
#
# `env -u TETON_ALLOW_TOOL_SEAM GITHUB_ACTIONS=true` reconstructs the one
# situation that matters: a CI run where a seam variable is set and no harness
# claimed it. One case per guard, because they are three separate `if`s in three
# files and a shared helper would only prove that one of them works.
#
# 64, not 75: this is a wrong INVOCATION rather than an environment that could
# not answer, and a caller who genuinely meant it has a one-word way to say so —
# which is the positive case at the end.

group "the CI seam refusals (GITHUB_ACTIONS=true, no harness flag)"

expect_exit 64 "verify-signature.sh refuses TETON_CODESIGN in CI -> 64" \
    env -u TETON_ALLOW_TOOL_SEAM GITHUB_ACTIONS=true TETON_CODESIGN="$cs_accept" \
    bash "$VERIFY_SIGNATURE" "$sig_bin" "$SIG_TEAM_ID"
expect_output "  ... and says an unsigned release must not be declared signed" \
    "variable declare an unsigned release signed"

expect_exit 64 "verify-attestation.sh refuses TETON_GH in CI -> 64" \
    env -u TETON_ALLOW_TOOL_SEAM GITHUB_ACTIONS=true TETON_GH="$gh_accept" \
    bash "$VERIFY_ATTESTATION" "$tb_linux" "$att_repo"
expect_output "  ... and says provenance must not be manufactured by a variable" \
    "variable manufacture provenance"

# package.sh's is the sharpest of the three: the other two report on bytes
# somebody else made, and this one names the program that SIGNS what ships.
expect_exit 64 "package.sh refuses TETON_CODESIGN in CI -> 64" \
    env -u TETON_ALLOW_TOOL_SEAM GITHUB_ACTIONS=true TETON_CODESIGN="$cs_accept" \
    bash "$PACKAGE" aarch64-apple-darwin 1.2.3 "$work/pkg-seam-refused"
expect_output "  ... and writes no tarball" "No tarball is written"

# The positive half, and it is not decoration: three cases proving a guard
# refuses would all still pass if the guard refused ALWAYS — which would break
# every other case in this file, but only by accident of ordering. This asserts
# the flag is what makes the difference, with GITHUB_ACTIONS=true held constant.
expect_exit 0 "TETON_ALLOW_TOOL_SEAM=1 admits the same invocation in CI -> 0" \
    env GITHUB_ACTIONS=true TETON_ALLOW_TOOL_SEAM=1 TETON_GH="$gh_accept" \
    bash "$VERIFY_ATTESTATION" "$tb_linux" "$att_repo"
expect_output "  ... and verifies for real" "PASS"

# --- the same gates, through smoke.sh --------------------------------------
#
# smoke.sh decides which leg it is on from the tarball's NAME, so these two
# tarballs differ in nothing but that, and the darwin pair differ in nothing
# but which stand-in codesign answers. The four behavioural assertions run
# against the same known-good stand-ins the smoke group above uses.

export TETON_SMOKE_SEAM_DEADLINE_SECS=10
export TETON_SMOKE_HANDSHAKE_DEADLINE_SECS=10

expect_exit 0 "smoke: a darwin tarball whose binaries are Developer ID signed passes" \
    env TETON_CODESIGN="$cs_accept" TETON_SMOKE_TEAM_ID="$SIG_TEAM_ID" \
    bash "$SMOKE" "$tb_darwin" 1.2.3
# Named explicitly, because a signature gate that checked only the first binary
# would look identical from the exit code (LESSON-455).
expect_output "  ... having checked the SECOND binary too" \
    "teton-code is Developer ID signed by team $SIG_TEAM_ID"
expect_output "  ... and still ran the four behavioural assertions" "all 4 assertions passed"

# KNOWN-BAD, end to end: the ad-hoc fixture inside an otherwise perfect darwin
# tarball. Everything else about this artifact is good, which is the point —
# the signature alone must take the release down.
expect_exit 65 "smoke: AD-HOC signed binaries in a darwin tarball -> 65 FAILED" \
    env TETON_CODESIGN="$cs_adhoc" TETON_SMOKE_TEAM_ID="$SIG_TEAM_ID" \
    bash "$SMOKE" "$tb_darwin" 1.2.3
expect_output "  ... and blames the signature" \
    "is not Developer ID signed by team $SIG_TEAM_ID (BR-2)"
expect_output "  ... while the behaviour it was hiding behind still passed" \
    "doctor handshook a live teton-code"

expect_exit 75 "smoke: a signature that could not be READ -> 75, never a pass" \
    env TETON_CODESIGN="$cs_silent" TETON_SMOKE_TEAM_ID="$SIG_TEAM_ID" \
    bash "$SMOKE" "$tb_darwin" 1.2.3
expect_output "  ... and says nothing was learned" "no signature verdict was reached"

# A missing team id is a usage error, never a skip: reading "then don't check"
# out of an absent argument is how a gate disables itself on the one machine
# that needed it (LESSON-443).
expect_exit 64 "smoke: a darwin tarball with no TETON_SMOKE_TEAM_ID -> 64, not a skip" \
    env -u TETON_SMOKE_TEAM_ID TETON_CODESIGN="$cs_accept" \
    bash "$SMOKE" "$tb_darwin" 1.2.3
expect_output "  ... and names the variable it lacks" "TETON_SMOKE_TEAM_ID, which names the team"

expect_exit 0 "smoke: a linux tarball passes without a signature check" \
    bash "$SMOKE" "$tb_linux" 1.2.3
# Printed, not skipped: a gate that says nothing looks exactly like a gate that
# did not run, and v1 shipping Linux unsigned is a decision, not an omission.
expect_output "  ... and says out loud that it is unsigned in v1" \
    "artifact is unsigned in v1 (linux"

unset TETON_SMOKE_SEAM_DEADLINE_SECS TETON_SMOKE_HANDSHAKE_DEADLINE_SECS

# --- package.sh ------------------------------------------------------------
#
# Two things, and the second used to be described here as impossible.
#
# The input validation that happens before `cargo build` needs nothing at all.
# The SIGNING PHASE happens after it, and the note here used to say that made it
# untestable without a toolchain — "that is the whole of package.sh that can be
# tested without a toolchain". It is not: `cargo` is resolved on PATH and the
# build output is read from `$CARGO_TARGET_DIR`, so a PATH stub that exits 0 and
# a directory containing two files where the binaries would be drive the whole
# of the rest of the script — staging, signing, verifying, tarring, hashing.
# That is where BR-2's real claim lives (a signing-requested build never ships
# unsigned), and it was going untested behind a sentence saying it could not be
# reached.
#
# What this does NOT test, and must not be read as testing: cargo, the compiler,
# the produced binaries, or Apple's codesign. The stub builds nothing and the
# stand-in signs nothing. What is under test is package.sh's own control flow —
# which failures stop it, and which of them ship a tarball anyway.
#
# Every case in THIS group drives the default phase, `all`, and not one of them
# passes a 4th argument. That is deliberate: since ADR-551-1 split the script
# into `build` and `pack`, these cases are the compatibility contract rather
# than a description of it — if the split changed what a plain
# `package.sh <target> <version> <outdir>` prints or produces, they go red
# without having been edited. The phases themselves are driven separately in
# the group after this one.

# A cargo TRIPWIRE, defined HERE rather than beside the phase group that first
# needed it, because "before cargo is invoked" is a claim the group below makes
# too and could not previously enforce. Not the exit-0 stub: this one fails
# loudly and instantly, so a case that says a refusal happens before the build
# can be graded on it rather than trusted. Leaving the real cargo on PATH would
# let a regression "pass" by compiling the workspace — llama.cpp from source,
# minutes of it — which is neither fast nor an assertion. 97 is outside every
# taxonomy in this repo, so if it ever surfaces it can only have come from here.
pkg_nocargo="$work/pkg-nocargo"
mkdir -p "$pkg_nocargo"
cat >"$pkg_nocargo/cargo" <<'EOF'
#!/usr/bin/env bash
echo "selftest: cargo was invoked by a phase that must not build" >&2
exit 97
EOF
chmod +x "$pkg_nocargo/cargo"

group "package.sh (input validation and the signing phase, no toolchain)"

expect_exit 64 "too few arguments -> 64" bash "$PACKAGE" aarch64-apple-darwin
expect_exit 64 "an unknown target triple -> 64, before cargo is invoked" \
    bash "$PACKAGE" x86_64-pc-windows-msvc 1.2.3 "$work/pkg"
expect_output "  ... and lists the targets it does build" "aarch64-apple-darwin"
expect_exit 64 "an empty version -> 64" bash "$PACKAGE" aarch64-apple-darwin "" "$work/pkg"
expect_exit 64 "a prerelease version -> 64" \
    bash "$PACKAGE" aarch64-apple-darwin 1.2.3-rc.1 "$work/pkg"
expect_exit 64 "a version that is really a path -> 64" \
    bash "$PACKAGE" aarch64-apple-darwin "../../etc" "$work/pkg"

# The 4th argument is a selector, not free text: it decides which half of the
# script runs, and a typo'd `packk` that fell through to a default would run the
# other half silently — a "build" step that quietly signed, or a "pack" step
# that quietly rebuilt. Refused with the same code as the other three arguments,
# and before cargo is invoked.
#
# Run with the cargo TRIPWIRE on PATH, so "before cargo is invoked" is enforced
# rather than asserted: a package.sh that validated the phase after building
# would exit 97 here, not 64. The claim was in this label from the start and
# nothing was checking it.
expect_exit 64 "an unknown phase -> 64, before cargo is invoked" \
    env PATH="$pkg_nocargo:$PATH" \
    bash "$PACKAGE" aarch64-apple-darwin 1.2.3 "$work/pkg" packk
expect_output "  ... and lists the phases it knows" "all, build, pack"

# The collision the 4th argument created, and the likeliest way to type it: the
# phase is the FOURTH argument, so `package.sh <target> <version> pack` is an
# `all` writing into a directory named `pack` — a run that compiles AND signs in
# one step, on a machine that by then holds a Developer ID key, and exits 0
# while doing it. That is the pre-REQ-551 ordering reached by a typo, so it is
# refused rather than guessed at.
for pkg_collide in all build pack; do
    expect_exit 64 "'$pkg_collide' as the THIRD argument -> 64, not an outdir named '$pkg_collide'" \
        env PATH="$pkg_nocargo:$PATH" \
        bash "$PACKAGE" aarch64-apple-darwin 1.2.3 "$pkg_collide"
    expect_output "  ... suggesting the invocation that was meant" \
        "did you mean: package.sh <target> <version> <outdir> $pkg_collide"
done

# A fifth argument is a caller believing in a flag this script does not have.
# Ignoring it would let them read the wrong story out of a green step.
expect_exit 64 "a fifth argument -> 64 rather than being ignored" \
    env PATH="$pkg_nocargo:$PATH" \
    bash "$PACKAGE" aarch64-apple-darwin 1.2.3 "$work/pkg" pack --sign
expect_output "  ... saying how many it takes" "takes at most four and ignores none"

# BR-2's known-bad, and the cheapest of the lot: signing is REQUESTED and the
# tool is not there. Settled before `cargo build` on purpose, so the answer
# arrives in seconds rather than after a from-source llama.cpp compile — and 70
# rather than 75, because this script is the one MAKING the bytes: "could not
# check" and "did not do it" are different failures.
expect_exit 70 "signing requested with no codesign -> 70, before anything is built" \
    env TETON_SIGN_IDENTITY="Developer ID Application: Atelier Fashion LLC ($SIG_TEAM_ID)" \
    TETON_CODESIGN=/nonexistent/codesign \
    bash "$PACKAGE" aarch64-apple-darwin 1.2.3 "$work/pkg"
expect_output "  ... and refuses to write an indistinguishable tarball" \
    "Refusing to build a tarball that would be unsigned"

# The PATH stub and the fake build output that make the rest reachable. `cargo`
# is a bare name package.sh resolves on PATH, and the binaries it stages come
# from $CARGO_TARGET_DIR/<triple>/release/ — so the two together stand in for a
# build without being one.
pkg_stub="$work/pkg-stub"
mkdir -p "$pkg_stub"
cat >"$pkg_stub/cargo" <<'EOF'
#!/usr/bin/env bash
# selftest stand-in for cargo. Builds nothing and proves nothing about the
# build; it exists so package.sh's post-build path can be reached at all.
exit 0
EOF
chmod +x "$pkg_stub/cargo"

pkg_target="$work/pkg-target"
mkdir -p "$pkg_target/aarch64-apple-darwin/release"
for pkg_bin in teton teton-code; do
    printf '#!/usr/bin/env bash\nexit 0\n' \
        >"$pkg_target/aarch64-apple-darwin/release/$pkg_bin"
    chmod +x "$pkg_target/aarch64-apple-darwin/release/$pkg_bin"
done

# PATH is PREPENDED, never replaced: package.sh also runs mktemp, cp, tar and a
# hasher, and a PATH of only the stub would fail on the first of them for a
# reason that has nothing to do with what is being tested.
expect_exit 0 "a signing-requested build signs the staged pair and ships -> 0" \
    env PATH="$pkg_stub:$PATH" CARGO_TARGET_DIR="$pkg_target" \
    TETON_SIGN_IDENTITY="Developer ID Application: Atelier Fashion LLC ($SIG_TEAM_ID)" \
    TETON_CODESIGN="$cs_accept" \
    bash "$PACKAGE" aarch64-apple-darwin 1.2.3 "$work/pkg-signed"
# BOTH binaries, named: a signing phase that signed the first and moved on would
# be identical from the exit code (LESSON-455).
expect_output "  ... having signed teton" "package: signed teton"
expect_output "  ... and teton-code too" "package: signed teton-code"
assert "  ... and the tarball was written" \
    [ -s "$work/pkg-signed/teton-v1.2.3-aarch64-apple-darwin.tar.gz" ]
assert "  ... with its sha256 sidecar" \
    [ -s "$work/pkg-signed/teton-v1.2.3-aarch64-apple-darwin.tar.gz.sha256" ]
# The staging directory ADR-551-1 introduced is an implementation detail of the
# phase boundary, and `all` crosses that boundary in one process — so it must
# leave the output directory looking exactly as the old mktemp-and-trap version
# did: one tarball, one sidecar, and no loose signed binaries beside them.
refute "  ... and nothing staged left behind, as the mktemp directory never was" \
    test -e "$work/pkg-signed/stage-aarch64-apple-darwin"

# KNOWN-BAD, and the claim BR-2 actually makes: the certificate is missing or
# expired, `codesign --sign` fails, and NO TARBALL IS WRITTEN. A build that
# shipped here would produce bytes indistinguishable from a signed release,
# which is the failure the whole signing story exists to prevent.
expect_exit 70 "a signing-requested build whose codesign REFUSES -> 70" \
    env PATH="$pkg_stub:$PATH" CARGO_TARGET_DIR="$pkg_target" \
    TETON_SIGN_IDENTITY="Developer ID Application: Atelier Fashion LLC ($SIG_TEAM_ID)" \
    TETON_CODESIGN="$cs_sign_reject" \
    bash "$PACKAGE" aarch64-apple-darwin 1.2.3 "$work/pkg-signreject"
expect_output "  ... and says it will not ship unsigned" \
    "a signing-requested build never ships unsigned"
refute "  ... and no tarball was left behind" \
    test -e "$work/pkg-signreject/teton-v1.2.3-aarch64-apple-darwin.tar.gz"

# KNOWN-BAD, the other half: signing succeeded and the RESULT does not hold.
# `--sign` exiting 0 is not the claim being made; "these bytes carry a signature
# that verifies" is, and only --verify tests it.
expect_exit 70 "signed but --verify --strict rejects the result -> 70" \
    env PATH="$pkg_stub:$PATH" CARGO_TARGET_DIR="$pkg_target" \
    TETON_SIGN_IDENTITY="Developer ID Application: Atelier Fashion LLC ($SIG_TEAM_ID)" \
    TETON_CODESIGN="$cs_reject" \
    bash "$PACKAGE" aarch64-apple-darwin 1.2.3 "$work/pkg-verifyreject"
expect_output "  ... and blames the verify, not the sign" \
    "codesign --verify --strict rejected the result"

# The unsigned dev build, for contrast: no identity, no signing, and it says so
# out loud rather than leaving a reader to infer it.
expect_exit 0 "no TETON_SIGN_IDENTITY -> an unsigned dev build that says so" \
    env -u TETON_SIGN_IDENTITY -u TETON_CODESIGN \
    PATH="$pkg_stub:$PATH" CARGO_TARGET_DIR="$pkg_target" \
    bash "$PACKAGE" aarch64-apple-darwin 1.2.3 "$work/pkg-unsigned"
expect_output "  ... and names the variable that would have signed it" \
    "set TETON_SIGN_IDENTITY for release signing"

# KNOWN-BAD: cargo reports success and the binary is not there. An internal
# inconsistency, worth its own code so it cannot be read as a compile error.
pkg_target_short="$work/pkg-target-short"
mkdir -p "$pkg_target_short/aarch64-apple-darwin/release"
printf '#!/usr/bin/env bash\nexit 0\n' \
    >"$pkg_target_short/aarch64-apple-darwin/release/teton"
chmod +x "$pkg_target_short/aarch64-apple-darwin/release/teton"
expect_exit 70 "cargo succeeds but teton-code is missing -> 70" \
    env -u TETON_SIGN_IDENTITY \
    PATH="$pkg_stub:$PATH" CARGO_TARGET_DIR="$pkg_target_short" \
    bash "$PACKAGE" aarch64-apple-darwin 1.2.3 "$work/pkg-short"
expect_output "  ... and names the binary it did not find" \
    "cargo reported success but"

# --- package.sh, phase by phase (ADR-551-1) --------------------------------
#
# The release workflow now runs `build` and `pack` as two steps with the
# Developer ID identity imported BETWEEN them, so that the ~30 minutes of
# compiling third-party sources happens on a runner with no signing identity on
# it (REQ-551 BR-1). Two properties only exist once the phases are separate,
# and neither can be observed from an `all` run:
#
#   * `build` must not touch signing AT ALL — not resolve the tool, not invoke
#     it — even when TETON_SIGN_IDENTITY leaks into its environment. That is
#     what makes the long compile safe to run before an identity exists.
#   * `pack` must work from a staging directory it did not create, and refuse
#     when that directory is missing or short a member: a `pack` whose `build`
#     step never ran must never produce a tarball (BR-2, across the boundary).
#
# The stand-ins are the same ones the group above uses, plus two of this
# group's own.

group "package.sh phases (build / pack / all, no toolchain)"

# `$pkg_nocargo` — the cargo tripwire every case below that drives `pack` runs
# with — is defined above the previous group, which needs it too.

# A codesign stand-in that RECORDS: it answers exactly as cs_accept does and
# appends one line per invocation to a log. "The build phase never invokes the
# signing tool" is not a claim an exit code can make — a build that resolved
# codesign and signed with it exits 0 too — so the invocations have to be
# counted rather than inferred.
pkg_cs_log="$work/pkg-cs-invocations"
pkg_cs_recording="$work/pkg-cs-recording/codesign"
mkdir -p "$(dirname "$pkg_cs_recording")"
cat >"$pkg_cs_recording" <<EOF
#!/usr/bin/env bash
printf '%s\n' "\$*" >>"$pkg_cs_log"
exec "$cs_accept" "\$@"
EOF
chmod +x "$pkg_cs_recording"

# stage_meta_write <stage dir> <version> — the `.stage-meta` manifest a build
# phase writes last: the version it was building, then a sha256 per member.
#
# Spelled out here rather than copied from a build run for the same reason the
# fixture below is: this suite has to be able to write a manifest that is WRONG.
stage_meta_write() {
    local dest="$1" meta_version="$2" member digest
    printf 'version %s\n' "$meta_version" >"$dest/.stage-meta"
    for member in teton teton-code LICENSE README.md; do
        digest="$(sha256_of "$dest/$member")"
        printf '%s  %s\n' "$digest" "$member" >>"$dest/.stage-meta"
    done
}

# stage_for_pack <outdir> [version] — plants the directory a build phase would
# have left, manifest and all. [version] defaults to 1.2.3, the version every
# case in this group packages.
#
# Deliberately NOT produced by running the build phase: the point of `pack` is
# that it works from a staging directory made by a process it never saw, which
# is exactly what the workflow hands it across the identity-import step. A
# fixture built by the code under test could not fail in the way this one can.
stage_for_pack() {
    local dest="$1/stage-aarch64-apple-darwin" member
    mkdir -p "$dest"
    for member in teton teton-code; do
        printf '#!/usr/bin/env bash\nexit 0\n' >"$dest/$member"
        chmod +x "$dest/$member"
    done
    printf 'MIT\n' >"$dest/LICENSE"
    printf '# teton\n' >"$dest/README.md"
    stage_meta_write "$dest" "${2:-1.2.3}"
}

# stage_matches_meta <stage dir> — 0 when every member still hashes to the
# digest `.stage-meta` records for it.
#
# The question "did the pack phase write into the staging directory?" asked
# directly, rather than inferred from an exit code. package.sh's recovery story
# — a failed pack costs seconds to retry, not a 30-minute recompile — is exactly
# the claim that the stage survives UNCHANGED, and nothing else in this suite
# can tell an untouched stage from one whose binaries were rewritten by a
# codesign that then failed.
stage_matches_meta() {
    local dest="$1" member want got
    if [ ! -f "$dest/.stage-meta" ]; then
        return 1
    fi
    for member in teton teton-code LICENSE README.md; do
        if ! got="$(sha256_of "$dest/$member")"; then
            return 1
        fi
        want="$(awk -v m="$member" 'NR > 1 && $2 == m { print $1 }' "$dest/.stage-meta")"
        if [ -z "$want" ] || [ "$want" != "$got" ]; then
            return 1
        fi
    done
    return 0
}

# make_mutating_codesign <path> <exit status its --verify returns>
#
# A codesign stand-in that MODIFIES THE FILE IT SIGNS, which is the one property
# of Apple's tool package.sh's recovery design turns on: `--sign` rewrites the
# binary, so a `--verify` that then rejects it leaves changed bytes behind. Not
# a signature and no evidence about codesign — the side effect, reproduced, so
# that "a failed pack leaves the stage as the build left it" can be tested
# rather than asserted.
#
# The file is the LAST argument, which is how package.sh invokes it:
# `codesign --force --sign <identity> --timestamp --options runtime <file>`.
make_mutating_codesign() {
    local path="$1" verify_status="$2"
    mkdir -p "$(dirname "$path")"
    cat >"$path" <<EOF
#!/usr/bin/env bash
target=""
for a in "\$@"; do target="\$a"; done

case "\${1:-}" in
    --force | --sign)
        printf 'SIGNED-BY-STANDIN\n' >>"\$target"
        exit 0
        ;;
    --verify)
        exit $verify_status
        ;;
esac

echo "mutating codesign stand-in: unexpected invocation: \$*" >&2
exit 2
EOF
    chmod +x "$path"
}

pkg_cs_mutate_reject="$work/pkg-cs-mutate-reject/codesign"
make_mutating_codesign "$pkg_cs_mutate_reject" 1
pkg_cs_mutate_accept="$work/pkg-cs-mutate-accept/codesign"
make_mutating_codesign "$pkg_cs_mutate_accept" 0

# `all`, named out loud — what the Linux leg keeps doing, and what a human who
# reads the usage line might type. Same flow, same lines, same tarball as the
# default cases above: the argument selects, it does not switch modes.
expect_exit 0 "an explicit 'all' behaves exactly as the default does -> 0" \
    env PATH="$pkg_stub:$PATH" CARGO_TARGET_DIR="$pkg_target" \
    TETON_SIGN_IDENTITY="Developer ID Application: Atelier Fashion LLC ($SIG_TEAM_ID)" \
    TETON_CODESIGN="$cs_accept" \
    bash "$PACKAGE" aarch64-apple-darwin 1.2.3 "$work/pkg-all-explicit" all
expect_output "  ... having signed teton" "package: signed teton"
expect_output "  ... and teton-code too" "package: signed teton-code"
assert "  ... and shipped the tarball" \
    [ -s "$work/pkg-all-explicit/teton-v1.2.3-aarch64-apple-darwin.tar.gz" ]
refute "  ... consuming its own staging directory on the way out" \
    test -e "$work/pkg-all-explicit/stage-aarch64-apple-darwin"

# `build` alone: it compiles, it stages, and it stops. The identity is set and
# the signing tool is right there on the seam — and the log below says it was
# called zero times.
: >"$pkg_cs_log"
expect_exit 0 "build alone stages the pair and stops there -> 0" \
    env PATH="$pkg_stub:$PATH" CARGO_TARGET_DIR="$pkg_target" \
    TETON_SIGN_IDENTITY="Developer ID Application: Atelier Fashion LLC ($SIG_TEAM_ID)" \
    TETON_CODESIGN="$pkg_cs_recording" \
    bash "$PACKAGE" aarch64-apple-darwin 1.2.3 "$work/pkg-build" build
assert "  ... staging teton where pack will look for it" \
    [ -x "$work/pkg-build/stage-aarch64-apple-darwin/teton" ]
assert "  ... and teton-code beside it" \
    [ -x "$work/pkg-build/stage-aarch64-apple-darwin/teton-code" ]
assert "  ... with the licence" \
    [ -f "$work/pkg-build/stage-aarch64-apple-darwin/LICENSE" ]
assert "  ... and the readme" \
    [ -f "$work/pkg-build/stage-aarch64-apple-darwin/README.md" ]
# BR-1, and the property the whole reorder exists for.
assert "  ... having invoked the signing tool exactly never (BR-1)" \
    [ ! -s "$pkg_cs_log" ]
refute "  ... and written no tarball, because packing is not its job" \
    test -e "$work/pkg-build/teton-v1.2.3-aarch64-apple-darwin.tar.gz"

# The same claim from the other side, and the sharper of the two: signing is
# requested and TETON_CODESIGN names a path that does not exist. On the `all`
# path that is the 70 above, settled before cargo runs. The build phase must
# not ask the question AT ALL — a build that resolved the tool would exit 70
# here, which would mean the long compile could only run on a machine that
# already had a signing identity, which is the ordering REQ-551 removes.
expect_exit 0 "build with TETON_SIGN_IDENTITY set resolves no signing tool -> 0" \
    env PATH="$pkg_stub:$PATH" CARGO_TARGET_DIR="$pkg_target" \
    TETON_SIGN_IDENTITY="Developer ID Application: Atelier Fashion LLC ($SIG_TEAM_ID)" \
    TETON_CODESIGN=/nonexistent/codesign \
    bash "$PACKAGE" aarch64-apple-darwin 1.2.3 "$work/pkg-build-identity" build
assert "  ... and said nothing about signing, either way" \
    [ "${CASE_OUT#*package: signed}" = "$CASE_OUT" ]

# The seam guard is a `pack`/`all` guard now, and this pins that as a decision
# rather than an oversight: the build phase reads TETON_CODESIGN nowhere, so
# there is nothing here for the variable to decide, and a guard standing over a
# variable that cannot reach anything teaches the next reader the wrong thing
# about what it protects. The pack case directly below is where the refusal has
# to survive, and does.
expect_exit 0 "build in CI ignores TETON_CODESIGN rather than refusing it -> 0" \
    env -u TETON_ALLOW_TOOL_SEAM GITHUB_ACTIONS=true \
    PATH="$pkg_stub:$PATH" CARGO_TARGET_DIR="$pkg_target" \
    TETON_CODESIGN="$cs_accept" \
    bash "$PACKAGE" aarch64-apple-darwin 1.2.3 "$work/pkg-build-ci" build

# The staging directory is CLEARED, not merged into — and the clear is a
# PRECONDITION of the build rather than the end of it. This plants the mess an
# earlier run of a different commit would leave: a member that belongs to no
# build at all, and a `teton` whose bytes are not the ones this build produces.
# A build phase that merged would ship both.
pkg_stale="$work/pkg-stale"
mkdir -p "$pkg_stale/stage-aarch64-apple-darwin"
printf 'this belonged to no build\n' \
    >"$pkg_stale/stage-aarch64-apple-darwin/leftover-from-another-commit"
printf '#!/usr/bin/env bash\necho STALE-BINARY\n' \
    >"$pkg_stale/stage-aarch64-apple-darwin/teton"
chmod +x "$pkg_stale/stage-aarch64-apple-darwin/teton"
expect_exit 0 "build clears a stale staging directory rather than merging into it -> 0" \
    env -u TETON_SIGN_IDENTITY -u TETON_CODESIGN \
    PATH="$pkg_stub:$PATH" CARGO_TARGET_DIR="$pkg_target" \
    bash "$PACKAGE" aarch64-apple-darwin 1.2.3 "$pkg_stale" build
refute "  ... so the file that belonged to no build is gone" \
    test -e "$pkg_stale/stage-aarch64-apple-darwin/leftover-from-another-commit"
refute "  ... and the stale teton was replaced, not left in place" \
    grep -q STALE-BINARY "$pkg_stale/stage-aarch64-apple-darwin/teton"

# The same guarantee at the moment it actually matters, and the one the clear's
# POSITION decides: the compile FAILS. With the clear after `cargo build` the
# script exits here with the previous run's stage untouched — four members, a
# manifest, and nothing for the pack step to object to — so a failed build
# followed by a pack ships the last build's binaries under this build's name.
# Cleared first, there is nothing to pack, and `pack` says so.
pkg_failcargo="$work/pkg-failcargo"
mkdir -p "$pkg_failcargo"
cat >"$pkg_failcargo/cargo" <<'EOF'
#!/usr/bin/env bash
echo "selftest: stand-in cargo failing the way a broken compile does" >&2
exit 101
EOF
chmod +x "$pkg_failcargo/cargo"

stage_for_pack "$work/pkg-buildfail"
expect_exit 101 "a build whose cargo FAILS propagates cargo's own status" \
    env -u TETON_SIGN_IDENTITY -u TETON_CODESIGN \
    PATH="$pkg_failcargo:$PATH" CARGO_TARGET_DIR="$pkg_target" \
    bash "$PACKAGE" aarch64-apple-darwin 1.2.3 "$work/pkg-buildfail" build
refute "  ... having already cleared the previous run's staged teton" \
    test -e "$work/pkg-buildfail/stage-aarch64-apple-darwin/teton"
refute "  ... and its manifest with it" \
    test -e "$work/pkg-buildfail/stage-aarch64-apple-darwin/.stage-meta"
expect_exit 70 "  ... so a pack after a FAILED build has nothing to pack -> 70" \
    env -u TETON_SIGN_IDENTITY -u TETON_CODESIGN PATH="$pkg_nocargo:$PATH" \
    bash "$PACKAGE" aarch64-apple-darwin 1.2.3 "$work/pkg-buildfail" pack
expect_output "  ... naming the contract it is holding to" \
    "The pack phase packs what the build phase staged"
refute "  ... and writing no tarball" \
    test -e "$work/pkg-buildfail/teton-v1.2.3-aarch64-apple-darwin.tar.gz"

stage_for_pack "$work/pkg-pack-seam"
expect_exit 64 "pack still refuses TETON_CODESIGN in CI -> 64" \
    env -u TETON_ALLOW_TOOL_SEAM GITHUB_ACTIONS=true \
    PATH="$pkg_nocargo:$PATH" TETON_CODESIGN="$cs_accept" \
    bash "$PACKAGE" aarch64-apple-darwin 1.2.3 "$work/pkg-pack-seam" pack
expect_output "  ... and writes no tarball" "No tarball is written"

# `pack` alone, from a staging directory it did not create: the whole of the
# signing story above, re-asked at the phase that now owns it.
stage_for_pack "$work/pkg-pack-signed"
expect_exit 0 "pack alone signs the staged pair and ships -> 0" \
    env PATH="$pkg_nocargo:$PATH" \
    TETON_SIGN_IDENTITY="Developer ID Application: Atelier Fashion LLC ($SIG_TEAM_ID)" \
    TETON_CODESIGN="$cs_accept" \
    bash "$PACKAGE" aarch64-apple-darwin 1.2.3 "$work/pkg-pack-signed" pack
expect_output "  ... having signed teton" "package: signed teton"
expect_output "  ... and teton-code too" "package: signed teton-code"
assert "  ... and the tarball was written" \
    [ -s "$work/pkg-pack-signed/teton-v1.2.3-aarch64-apple-darwin.tar.gz" ]
assert "  ... with its sha256 sidecar" \
    [ -s "$work/pkg-pack-signed/teton-v1.2.3-aarch64-apple-darwin.tar.gz.sha256" ]
# The pack phase ships what it was handed and invents nothing: four members,
# flat, in the order package.sh lists them.
pkg_pack_members="$(tar -tzf "$work/pkg-pack-signed/teton-v1.2.3-aarch64-apple-darwin.tar.gz" | tr '\n' ' ')"
assert "  ... and the archive holds exactly the four staged members" \
    [ "$pkg_pack_members" = "teton teton-code LICENSE README.md " ]
# Consumed on success: signed binaries left loose beside the tarball made from
# them are a second copy nobody hashed, and a second `pack` would re-ship a
# build the first one already shipped.
refute "  ... and the staging directory was consumed" \
    test -e "$work/pkg-pack-signed/stage-aarch64-apple-darwin"

# KNOWN-BAD at the pack phase, and the claim BR-2 actually makes: the
# certificate is missing or expired, `codesign --sign` fails, and NO TARBALL IS
# WRITTEN.
stage_for_pack "$work/pkg-pack-signreject"
expect_exit 70 "pack whose codesign REFUSES -> 70" \
    env PATH="$pkg_nocargo:$PATH" \
    TETON_SIGN_IDENTITY="Developer ID Application: Atelier Fashion LLC ($SIG_TEAM_ID)" \
    TETON_CODESIGN="$cs_sign_reject" \
    bash "$PACKAGE" aarch64-apple-darwin 1.2.3 "$work/pkg-pack-signreject" pack
expect_output "  ... and says it will not ship unsigned" \
    "a signing-requested build never ships unsigned"
refute "  ... and no tarball was left behind" \
    test -e "$work/pkg-pack-signreject/teton-v1.2.3-aarch64-apple-darwin.tar.gz"
# The other half of the consume rule, and the reason it is "on success" rather
# than "always": a FAILED pack keeps the staged build, so fixing the identity
# and re-running `pack` costs seconds instead of a second from-source compile.
# Nothing shippable survives — there is no tarball and no sidecar — which is
# what makes keeping it safe rather than tidy.
assert "  ... while the staged build survives for a retry" \
    [ -d "$work/pkg-pack-signreject/stage-aarch64-apple-darwin" ]

# KNOWN-BAD, the other half: signing succeeded and the RESULT does not hold.
stage_for_pack "$work/pkg-pack-verifyreject"
expect_exit 70 "pack that signed but --verify --strict rejects the result -> 70" \
    env PATH="$pkg_nocargo:$PATH" \
    TETON_SIGN_IDENTITY="Developer ID Application: Atelier Fashion LLC ($SIG_TEAM_ID)" \
    TETON_CODESIGN="$cs_reject" \
    bash "$PACKAGE" aarch64-apple-darwin 1.2.3 "$work/pkg-pack-verifyreject" pack
expect_output "  ... and blames the verify, not the sign" \
    "codesign --verify --strict rejected the result"
refute "  ... writing no tarball" \
    test -e "$work/pkg-pack-verifyreject/teton-v1.2.3-aarch64-apple-darwin.tar.gz"
# Kept for a retry here as well, and asserted rather than assumed: "a failed
# pack keeps the stage" is a claim about the pack phase, not about one branch of
# it, and the two failures leave by different exits.
assert "  ... while the staged build survives for a retry" \
    [ -d "$work/pkg-pack-verifyreject/stage-aarch64-apple-darwin" ]

# The signing tool missing, at the phase that now resolves it. The `all` case
# above covers the same refusal reached from a different entry point; both
# matter, because the resolution happens ONCE, at the top of the script, and a
# rearrangement that lost the `$phase != build` condition would break exactly
# one of them.
stage_for_pack "$work/pkg-pack-nocodesign"
expect_exit 70 "pack with signing requested and no codesign -> 70" \
    env PATH="$pkg_nocargo:$PATH" \
    TETON_SIGN_IDENTITY="Developer ID Application: Atelier Fashion LLC ($SIG_TEAM_ID)" \
    TETON_CODESIGN=/nonexistent/codesign \
    bash "$PACKAGE" aarch64-apple-darwin 1.2.3 "$work/pkg-pack-nocodesign" pack
expect_output "  ... and refuses to write an indistinguishable tarball" \
    "Refusing to build a tarball that would be unsigned"
refute "  ... writing no tarball" \
    test -e "$work/pkg-pack-nocodesign/teton-v1.2.3-aarch64-apple-darwin.tar.gz"
assert "  ... while the staged build survives for a retry" \
    [ -d "$work/pkg-pack-nocodesign/stage-aarch64-apple-darwin" ]

# The unsigned dev build, at the phase that decides it: no identity, no
# signing, and it says so out loud rather than leaving a reader to infer it.
stage_for_pack "$work/pkg-pack-unsigned"
expect_exit 0 "pack with no TETON_SIGN_IDENTITY -> an unsigned dev build that says so" \
    env -u TETON_SIGN_IDENTITY -u TETON_CODESIGN PATH="$pkg_nocargo:$PATH" \
    bash "$PACKAGE" aarch64-apple-darwin 1.2.3 "$work/pkg-pack-unsigned" pack
expect_output "  ... and names the variable that would have signed it" \
    "set TETON_SIGN_IDENTITY for release signing"

# BR-2 across the phase boundary, and the case TASK-027's mutation check
# breaks: NO staging directory at all, because the build step never ran — it
# was skipped, cancelled, failed early, or wrote to another outdir. A pack that
# tarred whatever it found would hand the release job an artifact with the
# right NAME and nothing inside it, and the name is most of what the rest of
# the pipeline reads.
expect_exit 70 "pack with no staging directory at all -> 70" \
    env -u TETON_SIGN_IDENTITY -u TETON_CODESIGN PATH="$pkg_nocargo:$PATH" \
    bash "$PACKAGE" aarch64-apple-darwin 1.2.3 "$work/pkg-pack-nostage" pack
expect_output "  ... naming the contract it is holding to" \
    "The pack phase packs what the build phase staged"
# The needle above is deliberately NOT the whole assertion: the per-member
# check below the directory check prints it too, so an absent directory would
# still be refused — with the wrong diagnosis — if the directory check were
# deleted. (It was: removing that branch alone left this suite green until this
# line existed.) The first line of each refusal is what separates "there is
# nothing here" from "what is here is short a member", and an operator reading
# a red step needs to be told which.
expect_output "  ... and saying the directory is not there AT ALL, not merely short" \
    "there is no staged build at"
refute "  ... and writing no tarball" \
    test -e "$work/pkg-pack-nostage/teton-v1.2.3-aarch64-apple-darwin.tar.gz"

# Half a staging directory, which is the likelier accident: a build that died
# between the two copies, or a second binary that never linked. A tarball short
# one binary is worse than no tarball, because it installs.
stage_for_pack "$work/pkg-pack-half"
rm -f "$work/pkg-pack-half/stage-aarch64-apple-darwin/teton-code"
expect_exit 70 "pack with a staging directory short one binary -> 70" \
    env -u TETON_SIGN_IDENTITY -u TETON_CODESIGN PATH="$pkg_nocargo:$PATH" \
    bash "$PACKAGE" aarch64-apple-darwin 1.2.3 "$work/pkg-pack-half" pack
expect_output "  ... naming the binary that is not there" \
    "teton-code is missing or not executable"
expect_output "  ... and the contract, again" \
    "The pack phase packs what the build phase staged"
refute "  ... and writing no tarball" \
    test -e "$work/pkg-pack-half/teton-v1.2.3-aarch64-apple-darwin.tar.gz"

# The ride-alongs are held to the same contract as the binaries. `tar` would
# fail on a missing member by itself — with a status from outside this script's
# taxonomy and a message about a file rather than about a phase.
stage_for_pack "$work/pkg-pack-nolicense"
rm -f "$work/pkg-pack-nolicense/stage-aarch64-apple-darwin/LICENSE"
expect_exit 70 "pack with a staging directory short its LICENSE -> 70, not a tar error" \
    env -u TETON_SIGN_IDENTITY -u TETON_CODESIGN PATH="$pkg_nocargo:$PATH" \
    bash "$PACKAGE" aarch64-apple-darwin 1.2.3 "$work/pkg-pack-nolicense" pack
expect_output "  ... naming the member it did not find" "LICENSE is missing"

# WHAT KIND OF FILE each member is, which every other check in the pack phase
# takes for granted. A member is present, executable and hashes correctly and is
# still not something to ship.
#
# A SYMLINK is the sharp one, because it passes everything: `-f` and `-x` both
# follow it, and it hashes as its TARGET — so the manifest agrees, and the file
# that gets signed and the bytes that were checked stop being the same question.
# The fixture points at the real staged binary, so the digest genuinely matches
# and nothing but the type check can refuse it.
stage_for_pack "$work/pkg-pack-symlink"
mv "$work/pkg-pack-symlink/stage-aarch64-apple-darwin/teton" \
    "$work/pkg-pack-symlink/teton-elsewhere"
ln -s "$work/pkg-pack-symlink/teton-elsewhere" \
    "$work/pkg-pack-symlink/stage-aarch64-apple-darwin/teton"
expect_exit 70 "pack whose staged teton is a SYMLINK -> 70" \
    env -u TETON_SIGN_IDENTITY -u TETON_CODESIGN PATH="$pkg_nocargo:$PATH" \
    bash "$PACKAGE" aarch64-apple-darwin 1.2.3 "$work/pkg-pack-symlink" pack
expect_output "  ... naming it as a symlink rather than as a missing member" \
    "is a symlink, not a regular file"
refute "  ... and writing no tarball" \
    test -e "$work/pkg-pack-symlink/teton-v1.2.3-aarch64-apple-darwin.tar.gz"

# A DIRECTORY where a binary should be, and the case that pins the TAXONOMY
# rather than merely the refusal. A directory is executable, so it sails past
# the completeness check; the next thing to touch it is the manifest's hasher,
# and `sha256_of` failing is spelled 75 — "this machine has no sha256 tool" —
# about a machine that has three. 70, naming the directory, or the operator
# reading the log goes looking for a broken runner.
stage_for_pack "$work/pkg-pack-dirmember"
rm -f "$work/pkg-pack-dirmember/stage-aarch64-apple-darwin/teton"
mkdir -p "$work/pkg-pack-dirmember/stage-aarch64-apple-darwin/teton"
expect_exit 70 "pack whose staged teton is a DIRECTORY -> 70, not the 75 a hasher would report" \
    env -u TETON_SIGN_IDENTITY -u TETON_CODESIGN PATH="$pkg_nocargo:$PATH" \
    bash "$PACKAGE" aarch64-apple-darwin 1.2.3 "$work/pkg-pack-dirmember" pack
expect_output "  ... naming it as a directory" "is a directory, not a regular file"

# The other half of the collision guard, and the half that keeps it a refusal of
# an AMBIGUITY rather than a ban on three directory names: with FOUR arguments
# the phase is spelled where the phase lives, so `<target> <version> pack pack`
# is an ordinary pack into a directory called `pack` and must simply work. The
# `$# -eq 3` gate is the whole of the difference, and nothing else here would
# notice it being widened to `${3:-}`.
mkdir -p "$work/pkg-collide4"
stage_for_pack "$work/pkg-collide4/pack"
# Driven from a `cd` into the scratch directory because the outdir is RELATIVE
# — `pack`, literally — and this suite must not create one in the repository.
# shellcheck disable=SC2016  # deliberate: the body reads argv, not our variables
expect_exit 0 "'pack' as the third argument WITH a fourth -> an ordinary pack into ./pack" \
    env -u TETON_SIGN_IDENTITY -u TETON_CODESIGN PATH="$pkg_nocargo:$PATH" \
    bash -c 'cd "$1" || exit 1; exec bash "$2" aarch64-apple-darwin 1.2.3 pack pack' \
    _ "$work/pkg-collide4" "$PACKAGE"
assert "  ... writing the tarball into the directory really named 'pack'" \
    [ -s "$work/pkg-collide4/pack/teton-v1.2.3-aarch64-apple-darwin.tar.gz" ]

# THE RECOVERY, and the reason the pack phase signs COPIES.
#
# `codesign` rewrites the file it signs. Signing the staged binaries in place
# meant that the sharpest failure — `--sign` succeeded, `--verify --strict`
# rejected the result — left the stage holding bytes the manifest no longer
# described, so the retry the runbook promises ("fix the identity and re-`pack`,
# it costs seconds") ran into the manifest check and was refused as a TAMPERED
# stage. The failure said "these bytes are not the ones the build staged", which
# was true and was the wrong story.
#
# Three claims in one sequence, and only the middle one is new: the failure is
# still 70, the stage is still byte-for-byte what the build wrote, and a second
# pack with a working signer ships. The stand-ins MUTATE, so a pack that signed
# in place could not pass all three.
stage_for_pack "$work/pkg-scratch-sign"
expect_exit 70 "pack whose file-MUTATING codesign fails its own --verify -> 70" \
    env PATH="$pkg_nocargo:$PATH" \
    TETON_SIGN_IDENTITY="Developer ID Application: Atelier Fashion LLC ($SIG_TEAM_ID)" \
    TETON_CODESIGN="$pkg_cs_mutate_reject" \
    bash "$PACKAGE" aarch64-apple-darwin 1.2.3 "$work/pkg-scratch-sign" pack
expect_output "  ... and blames the verify, not the sign" \
    "codesign --verify --strict rejected the result"
assert "  ... while the staged bytes STILL match .stage-meta: pack wrote nothing into the stage" \
    stage_matches_meta "$work/pkg-scratch-sign/stage-aarch64-apple-darwin"
expect_exit 0 "  ... so a re-pack with an accepting signer ships, with no rebuild -> 0" \
    env PATH="$pkg_nocargo:$PATH" \
    TETON_SIGN_IDENTITY="Developer ID Application: Atelier Fashion LLC ($SIG_TEAM_ID)" \
    TETON_CODESIGN="$pkg_cs_mutate_accept" \
    bash "$PACKAGE" aarch64-apple-darwin 1.2.3 "$work/pkg-scratch-sign" pack
mkdir -p "$work/pkg-scratch-extract"
tar -xzf "$work/pkg-scratch-sign/teton-v1.2.3-aarch64-apple-darwin.tar.gz" \
    -C "$work/pkg-scratch-extract" teton 2>/dev/null || true
assert "  ... and the SHIPPED teton carries the signer's bytes, not the stage's unsigned ones" \
    grep -q SIGNED-BY-STANDIN "$work/pkg-scratch-extract/teton"
# Exactly once. Twice would mean the second pack signed a file the FIRST pack
# had already written to — which is the in-place behaviour this design removed,
# reachable again the day someone points codesign back at "$stage/$bin".
assert "  ... exactly once, so the retry signed a fresh copy of the pristine stage" \
    [ "$(grep -c SIGNED-BY-STANDIN "$work/pkg-scratch-extract/teton")" -eq 1 ]

# --- the staged-build manifest ---------------------------------------------
#
# Completeness is one question; PROVENANCE is a different one, and the phase
# split is what made the second one askable. The staging directory is the only
# thing that crosses the boundary, it is an ordinary directory in `dist/`, and
# the step that sits on that boundary imports a Developer ID private key. So
# between `build` and `pack` there is a window in which anything that can write
# to `dist/` chooses what gets a real release signature — and every case above
# would be perfectly happy about it, because a swapped binary is still four
# executable members.
#
# `build` therefore records what it staged (`.stage-meta`: the version, and a
# sha256 per member) and `pack` re-hashes and compares BEFORE it signs. These
# cases are the three ways that can go wrong, and each has to name itself: "no
# manifest", "another version's build" and "these bytes changed" have three
# different causes and three different fixes.

stage_for_pack "$work/pkg-pack-nometa"
rm -f "$work/pkg-pack-nometa/stage-aarch64-apple-darwin/.stage-meta"
expect_exit 70 "pack of a complete stage with NO .stage-meta -> 70" \
    env -u TETON_SIGN_IDENTITY -u TETON_CODESIGN PATH="$pkg_nocargo:$PATH" \
    bash "$PACKAGE" aarch64-apple-darwin 1.2.3 "$work/pkg-pack-nometa" pack
expect_output "  ... naming the manifest it did not find" "carries no .stage-meta"
expect_output "  ... and the contract it is holding to" \
    "The pack phase packs what the build phase staged"
refute "  ... writing no tarball" \
    test -e "$work/pkg-pack-nometa/teton-v1.2.3-aarch64-apple-darwin.tar.gz"

# From a REAL build phase from here on: a manifest the code under test wrote is
# the only evidence that what it writes is what it later accepts. A fixture
# could agree with a mistake in both halves at once.
expect_exit 0 "build records what it staged in .stage-meta -> 0" \
    env -u TETON_SIGN_IDENTITY -u TETON_CODESIGN \
    PATH="$pkg_stub:$PATH" CARGO_TARGET_DIR="$pkg_target" \
    bash "$PACKAGE" aarch64-apple-darwin 1.2.3 "$work/pkg-tamper" build
assert "  ... writing the manifest beside the members" \
    [ -f "$work/pkg-tamper/stage-aarch64-apple-darwin/.stage-meta" ]
assert "  ... naming the version it was building on the first line" \
    [ "$(head -n 1 "$work/pkg-tamper/stage-aarch64-apple-darwin/.stage-meta")" = "version 1.2.3" ]
assert "  ... and one digest line per shipped member" \
    [ "$(grep -c '' "$work/pkg-tamper/stage-aarch64-apple-darwin/.stage-meta")" -eq 5 ]

# THE case this manifest exists for: the staged binary is replaced between the
# two phases — which on the real runner means after the Developer ID identity
# has been imported. A pack that signed it would hand out a genuinely signed,
# genuinely trusted artifact containing bytes no build produced.
#
# Driven through the RECORDING signing stand-in, with the log emptied first,
# because the exit code is only half the claim: a check that ran after the
# signing loop would also exit 70, having by then put a Developer ID signature
# on a substituted binary. Four lines in that log and a 70 is a different — and
# much worse — outcome than no lines and a 70, and only the log can tell them
# apart (LESSON-455).
: >"$pkg_cs_log"
printf '#!/usr/bin/env bash\necho SUBSTITUTED\n' \
    >"$work/pkg-tamper/stage-aarch64-apple-darwin/teton"
expect_exit 70 "pack whose staged teton changed since the build -> 70" \
    env PATH="$pkg_nocargo:$PATH" \
    TETON_SIGN_IDENTITY="Developer ID Application: Atelier Fashion LLC ($SIG_TEAM_ID)" \
    TETON_CODESIGN="$pkg_cs_recording" \
    bash "$PACKAGE" aarch64-apple-darwin 1.2.3 "$work/pkg-tamper" pack
expect_output "  ... naming the member that changed" \
    "the staged teton at"
expect_output "  ... and saying it is not what the build staged" \
    "is not the one the build phase staged"
assert "  ... having invoked the signing tool exactly never: it refuses BEFORE it signs" \
    [ ! -s "$pkg_cs_log" ]
refute "  ... and writing no tarball" \
    test -e "$work/pkg-tamper/teton-v1.2.3-aarch64-apple-darwin.tar.gz"

# The version skew the deterministic stage name makes possible: the directory is
# keyed on the TARGET only, so a stage built for 1.2.3 is sitting exactly where
# a pack for 9.9.9 looks. Packing it would put one release's binaries inside a
# tarball named for another — and downstream, the name is most of what is read.
expect_exit 0 "build for 1.2.3 -> 0" \
    env -u TETON_SIGN_IDENTITY -u TETON_CODESIGN \
    PATH="$pkg_stub:$PATH" CARGO_TARGET_DIR="$pkg_target" \
    bash "$PACKAGE" aarch64-apple-darwin 1.2.3 "$work/pkg-version-skew" build
expect_exit 70 "  ... and a pack of that stage asked for 9.9.9 -> 70" \
    env -u TETON_SIGN_IDENTITY -u TETON_CODESIGN PATH="$pkg_nocargo:$PATH" \
    bash "$PACKAGE" aarch64-apple-darwin 9.9.9 "$work/pkg-version-skew" pack
expect_output "  ... naming the version the stage was built for" \
    "was built for version '1.2.3'"
expect_output "  ... and the one this pack was asked for" \
    "asked for version '9.9.9'"
refute "  ... writing no tarball under either version" \
    test -e "$work/pkg-version-skew/teton-v9.9.9-aarch64-apple-darwin.tar.gz"

# --- when the machine cannot hash at all -----------------------------------
#
# `pack` re-hashes the staged members, so a machine with no sha256 tool cannot
# answer the question this phase now asks before signing. That is 75 and not 70
# for the reason the rest of tools/release/ spells out (LESSON-442): "could not
# check" is not "checked and found a problem". What must hold either way is that
# nothing shippable survives.
#
# The PATH here is MINIMAL rather than prepended — a stub cannot hide a tool
# that is already there — so it is built out of symlinks to exactly the
# externals package.sh reaches for, minus every hasher. `env PATH=… bash` finds
# `bash` through the new PATH too, which is why bash itself is on the list.
pkg_nosha="$work/pkg-nosha"
mkdir -p "$pkg_nosha"
pkg_nosha_ok=1
for pkg_tool in bash dirname mkdir rm cp grep awk tar basename; do
    if pkg_tool_path="$(command -v "$pkg_tool" 2>/dev/null)"; then
        ln -sf "$pkg_tool_path" "$pkg_nosha/$pkg_tool"
    else
        pkg_nosha_ok=0
    fi
done
for pkg_tool in shasum sha256sum openssl; do
    if [ -e "$pkg_nosha/$pkg_tool" ]; then
        pkg_nosha_ok=0
    fi
done

if [ "$pkg_nosha_ok" -ne 1 ]; then
    skip "pack with no sha256 tool at all -> 75 (this machine is missing a tool the stub PATH needs)"
else
    stage_for_pack "$work/pkg-pack-nosha"
    expect_exit 75 "pack on a PATH with no sha256 tool -> 75, and it stops before signing" \
        env -u TETON_SIGN_IDENTITY -u TETON_CODESIGN PATH="$pkg_nosha" \
        bash "$PACKAGE" aarch64-apple-darwin 1.2.3 "$work/pkg-pack-nosha" pack
    expect_output "  ... saying it could not check what it was about to pack" \
        "cannot be checked against the manifest its build wrote"
    refute "  ... and writing no tarball" \
        test -e "$work/pkg-pack-nosha/teton-v1.2.3-aarch64-apple-darwin.tar.gz"
fi

# The other 75, and the one fix-4 is about: the members hashed fine and the
# finished TARBALL cannot be. Reached with a `shasum` stand-in that defers to
# the real one for everything except a `.tar.gz`, because a PATH with no hasher
# at all never gets this far — the manifest check above stops it first. What is
# under test is the cleanup: a tarball with no sidecar beside it is, to the
# upload step and to a glob, indistinguishable from a finished one.
pkg_noshatar="$work/pkg-noshatar"
mkdir -p "$pkg_noshatar"
if pkg_real_shasum="$(command -v shasum 2>/dev/null)"; then
    cat >"$pkg_noshatar/shasum" <<EOF
#!/usr/bin/env bash
# selftest stand-in for shasum: honest about the staged members, mute about the
# tarball, so that exactly one of package.sh's two hashing sites fails.
for arg; do
    case "\$arg" in
        *.tar.gz) exit 3 ;;
    esac
done
exec "$pkg_real_shasum" "\$@"
EOF
    chmod +x "$pkg_noshatar/shasum"
    stage_for_pack "$work/pkg-pack-noshatar"
    # A SIDECAR FROM AN EARLIER RUN, planted before the failing one. Without it
    # "no sidecar was left behind" is a claim about a file that was never
    # written — this refusal happens at the line that would have written it — and
    # the assertion passes on a package.sh that removes nothing. With it the
    # assertion is about a REMOVAL, which is the behaviour being tested: a
    # `.tar.gz.sha256` with no `.tar.gz` beside it is a hash of bytes nobody can
    # produce, sitting exactly where a human re-checking one artifact by hand
    # looks for the right one.
    printf '%s  %s\n' \
        "0000000000000000000000000000000000000000000000000000000000000000" \
        "teton-v1.2.3-aarch64-apple-darwin.tar.gz" \
        >"$work/pkg-pack-noshatar/teton-v1.2.3-aarch64-apple-darwin.tar.gz.sha256"
    expect_exit 75 "pack whose finished tarball cannot be hashed -> 75" \
        env -u TETON_SIGN_IDENTITY -u TETON_CODESIGN \
        PATH="$pkg_noshatar:$pkg_nocargo:$PATH" \
        bash "$PACKAGE" aarch64-apple-darwin 1.2.3 "$work/pkg-pack-noshatar" pack
    expect_output "  ... saying the tarball was removed rather than left" \
        "has been REMOVED"
    refute "  ... and the unhashed tarball really is gone" \
        test -e "$work/pkg-pack-noshatar/teton-v1.2.3-aarch64-apple-darwin.tar.gz"
    refute "  ... and the STALE sidecar from the earlier run went with it" \
        test -e "$work/pkg-pack-noshatar/teton-v1.2.3-aarch64-apple-darwin.tar.gz.sha256"
else
    skip "pack whose finished tarball cannot be hashed -> 75 (no shasum to stand in front of)"
fi

# The handoff, end to end, in the order the workflow will run it: `build`
# stages, and a SEPARATE process signs and packs what it left behind. On the
# real runner the identity is imported between these two invocations, which is
# the whole of ADR-551-1; nothing else here exercises two phases against one
# staging directory.
: >"$pkg_cs_log"
expect_exit 0 "build, then a second process packs what it staged: the build -> 0" \
    env -u TETON_SIGN_IDENTITY -u TETON_CODESIGN \
    PATH="$pkg_stub:$PATH" CARGO_TARGET_DIR="$pkg_target" \
    bash "$PACKAGE" aarch64-apple-darwin 1.2.3 "$work/pkg-handoff" build
expect_exit 0 "  ... and the pack that follows it, with the identity now set -> 0" \
    env PATH="$pkg_nocargo:$PATH" \
    TETON_SIGN_IDENTITY="Developer ID Application: Atelier Fashion LLC ($SIG_TEAM_ID)" \
    TETON_CODESIGN="$pkg_cs_recording" \
    bash "$PACKAGE" aarch64-apple-darwin 1.2.3 "$work/pkg-handoff" pack
assert "  ... producing the tarball the runbook expects" \
    [ -s "$work/pkg-handoff/teton-v1.2.3-aarch64-apple-darwin.tar.gz" ]
# The member list, asked of the ONE tarball in this suite whose staging
# directory a real build phase produced — which is to say the only one that ever
# had a `.stage-meta` in it. `.stage-meta` is the phase boundary's own
# bookkeeping; shipping it would put a checksum file inside the artifact that
# looks, to a user, like something to trust. Four members, flat, in the order
# package.sh lists them — the same list as before the manifest existed.
pkg_handoff_members="$(tar -tzf "$work/pkg-handoff/teton-v1.2.3-aarch64-apple-darwin.tar.gz" | tr '\n' ' ')"
assert "  ... holding exactly the four shipped members, manifest excluded" \
    [ "$pkg_handoff_members" = "teton teton-code LICENSE README.md " ]
# Four invocations, not "some": two binaries, each signed and then verified.
# A count pins the pair AND the verify, which an exit code cannot (LESSON-455),
# and it pins them to the SECOND process — the log was empty when it started.
assert "  ... with all four signing-tool invocations made by the pack process" \
    [ "$(grep -c '' "$pkg_cs_log")" -eq 4 ]

# --- lib.sh ----------------------------------------------------------------

group "lib.sh"

assert "is_release_version accepts 0.1.0" is_release_version 0.1.0
assert "is_release_version accepts 10.20.30" is_release_version 10.20.30
refute "is_release_version rejects the empty string" is_release_version ""
refute "is_release_version rejects a missing argument" is_release_version
refute "is_release_version rejects 1.2" is_release_version 1.2
refute "is_release_version rejects a prerelease" is_release_version 1.2.3-rc.1
refute "is_release_version rejects +build metadata" is_release_version 1.2.3+build.7
refute "is_release_version rejects a leading v" is_release_version v1.2.3

printf 'teton\n' >"$work/hashme"
# The literal digest of "teton\n", not a second call to the same tool the
# function's first branch uses: comparing sha256_of against `shasum` compares
# the function to its own body on any machine that has shasum, which is every
# runner we use — an assertion that cannot fail is not an assertion.
TETON_SHA256='f8d217bfc368404968c10875256c45590bd3fba03f13bd7dac156a548b4857d8'
assert "sha256_of prints the file's sha256" \
    [ "$(sha256_of "$work/hashme")" = "$TETON_SHA256" ]

# Each branch of the fallback chain, forced by hiding the tool ahead of it on
# PATH. Untested fallbacks are where format drift hides: openssl already
# changed its prefix once (`SHA256(f)=` -> `SHA2-256(f)=`), which the
# `awk '{print $NF}'` parse survives only by taking the LAST field.
# Exercise each fallback branch precisely: build a directory containing ONLY
# the tools that branch is allowed to see, and set PATH to exactly it.
#
# Not by planting a stub that exits non-zero — `command -v` finds a failing stub
# and takes that branch, so a stub tests only the first branch's error handling
# (which is how the empty-digest return-0 bug below was found). And not by
# pruning directories either: shasum and openssl both live in /usr/bin, so
# dropping one drops the other.
only_path="$work/only"
mkdir -p "$only_path"
# awk is needed by every branch; link it in unconditionally.
ln -sf "$(command -v awk)" "$only_path/awk" 2>/dev/null || true

for branch in sha256sum openssl; do
    tool_path="$(command -v "$branch" 2>/dev/null || true)"
    if [ -z "$tool_path" ]; then
        skip "sha256_of's $branch branch ($branch is not on this machine)"
        continue
    fi
    rm -f "$only_path/shasum" "$only_path/sha256sum" "$only_path/openssl"
    ln -sf "$tool_path" "$only_path/$branch"
    # PATH is narrowed INSIDE the child: `PATH=... bash -c` cannot find bash
    # itself to run (the same footgun the no-tool case below documents).
    # shellcheck disable=SC2016  # deliberate: argv, not interpolation
    if bash -c 'PATH="$1"; . "$2"; sha256_of "$3"' _ "$only_path" "$LIB" "$work/hashme" >"$work/fb.out" 2>/dev/null; then
        assert "sha256_of's $branch branch produces the right digest" \
            [ "$(cat "$work/fb.out")" = "$TETON_SHA256" ]
    else
        report_fail "sha256_of's $branch branch produced nothing" "$(cat "$work/fb.out" 2>/dev/null)"
    fi
done

# The bug the pruning above uncovered: a hasher that EXISTS but fails must not
# yield exit 0 and an empty digest.
broken="$work/broken-hasher"
mkdir -p "$broken"
printf '#!/bin/sh\nexit 127\n' >"$broken/shasum"
chmod +x "$broken/shasum"
# shellcheck disable=SC2016  # deliberate: argv, not interpolation
expect_exit 1 "sha256_of fails when the hasher it picked is broken (no empty digest)" \
    bash -c 'PATH="$1:$PATH"; . "$2"; sha256_of "$3"' _ "$broken" "$LIB" "$work/hashme"
# The reason lib.sh exists. package.sh's old copy ran `openssl` unconditionally
# here, so a machine with none of the three tools aborted the build with 127
# under `set -e`. `expect_exit 1`, not `refute`, precisely because 127 is also a
# failure and would satisfy a mere "it did not succeed".
#
# PATH is emptied INSIDE the child rather than via `env PATH=... bash`, which
# cannot find bash to run and fails before testing anything.
# shellcheck disable=SC2016  # deliberate: the body takes $1/$2 as argv, not interpolation
expect_exit 1 "sha256_of returns 1, not 127, when no sha256 tool exists" \
    bash -c 'PATH=/nonexistent; . "$1"; sha256_of "$2"' _ "$LIB" "$work/hashme"

# The hex test used to anchor only the first EIGHT characters and then check the
# length, so any 64-character string starting with eight hex digits was accepted
# as a digest — `0f1e2d3c` followed by 56 characters of a tool's error message
# would have been written into checksums.txt and spliced into the formula's
# `sha256`. Both consumers take what this prints on trust, so the whole string
# has to be hex.
hexy="$work/hex-hasher"
mkdir -p "$hexy"
cat >"$hexy/shasum" <<'EOF'
#!/usr/bin/env bash
# A hasher that "succeeds" with 64 characters of which only the first eight are
# hex — the shape the old prefix-anchored test could not tell from a digest.
echo "0f1e2d3c this is not a digest at all, it is 64 characters of prose ok  x"
EOF
chmod +x "$hexy/shasum"
# shellcheck disable=SC2016  # deliberate: argv, not interpolation
expect_exit 1 "sha256_of rejects a 64-char value that is not hex all the way down" \
    bash -c 'PATH="$1:$PATH"; . "$2"; sha256_of "$3"' _ "$hexy" "$LIB" "$work/hashme"

# --- cross-file consistency ------------------------------------------------
#
# The team id is a LITERAL in five places — this suite, the README, the runbook,
# the Homebrew formula template and the release workflow — and nothing in the
# build reads it from a single source. That is the drift surface: a team id
# changes (a new Apple account, a transferred membership), four of the five are
# updated, and the fifth tells a user or a gate the wrong thing indefinitely.
# Pinned here rather than refactored because four of the five are prose and a
# YAML string; the cheap fix is to make drift go red.

group "cross-file consistency (the literals nothing else pins)"

for drift_file in "$README" "$RUNBOOK" "$FORMULA_TEMPLATE" "$RELEASE_WORKFLOW"; do
    if [ ! -f "$drift_file" ]; then
        skip "team id in $(basename "$drift_file") (file is not in this checkout)"
        continue
    fi
    assert "the team id $SIG_TEAM_ID appears in $(basename "$drift_file")" \
        grep -Fq -- "$SIG_TEAM_ID" "$drift_file"
done

# --- the import-after-build ordering in release.yml ------------------------
#
# BR-6 / ADR-551-2, and the only structural claim this suite makes about a
# workflow file. The whole of REQ-551 is a STEP ORDER: on a macOS leg the
# Developer ID identity is imported AFTER the ~30 minutes of third-party
# compilation and BEFORE the seconds of signing, so that no crate's build
# script ever runs beside an unlocked keychain holding a release key (BR-1).
# YAML cannot express that and actionlint has no opinion about it — release.yml
# is exactly as valid with the import step first, and until this group existed
# the only thing holding the order in place was a comment saying it mattered.
# REQ-550's verify pass is precisely where a comment claiming to be a guard was
# found not to be one (LESSON-443). This group is the guard; that comment now
# points at it by name.
#
# Three anchors, and they are LOAD-BEARING STRINGS rather than descriptions of
# strings — a rename is a change to this assertion and belongs in the same
# commit:
#
#   the `dist build` package.sh invocation   the unsigned compile
#   the import step's `- name:` line         kept verbatim by TASK-028
#   the `dist pack` package.sh invocation    sign, verify, tar
#
# So a missing anchor is a NAMED failure here (order-status 2), never a quiet
# pass: an assertion that can be satisfied by deleting what it reads is not an
# assertion, and this file has met that shape before.
#
# Which is why AC-4's mutation lives in the suite rather than in a sentence
# about a mutation somebody performed by hand once and undid. Four known-bad
# copies of the REAL release.yml are built under $work and graded on every run
# — the import step moved above the build (the regression itself), the step
# renamed, and each phase argument dropped — so both directions are proven
# here, permanently, instead of asserted about a file nobody can re-check. The
# workflow itself is only ever read; the last case in the group proves that.

group "the import-after-build ordering in release.yml (BR-6 / ADR-551-2)"

# Single-quoted: `$TARGET` and `$VERSION` are release.yml's text, not this
# script's variables.
# shellcheck disable=SC2016  # deliberate: these are the workflow's bytes
ORDER_ANCHOR_BUILD='bash tools/release/package.sh "$TARGET" "$VERSION" dist build'
ORDER_ANCHOR_IMPORT='- name: Import the Developer ID signing identity'
# shellcheck disable=SC2016  # deliberate: these are the workflow's bytes
ORDER_ANCHOR_PACK='bash tools/release/package.sh "$TARGET" "$VERSION" dist pack'
# " (early)" is LOAD-BEARING in this needle. The plain
# "- name: Destroy the signing keychain" matches TWO lines — the early destroy
# and the `if: always()` backstop — and a two-hit needle is refused as ambiguous
# by workflow_line_of, so an anchor without the suffix would report the step as
# duplicated on a perfectly correct file.
ORDER_ANCHOR_DESTROY='- name: Destroy the signing keychain (early)'
ORDER_ANCHOR_SMOKE='- name: Smoke the tarball (BR-7/BR-9)'

# workflow_line_of <file> <fixed needle> — the line number of the ONE line
# containing the needle. Prints NOTHING when it appears zero times, and
# `ambiguous:<n>` when it appears more than once.
#
# awk's index(), not grep: every anchor here carries `$` and `(`, and a regex
# reading of them is a different question than the one being asked.
#
# Ambiguity is still REFUSED — with two matching lines "the first one" is a
# guess about which step is meant, and guessing is the failure mode this group
# exists to remove — but it is no longer refused by printing nothing. The two
# outcomes had one spelling and therefore one diagnosis, and the diagnosis was
# the wrong one: a workflow that grew a SECOND `dist pack` invocation (a retry
# step, a second matrix leg, a copied step somebody forgot to edit) was reported
# as "the signing anchor vanished", which is the opposite of what happened and
# sends whoever reads it looking for a deletion. Callers must therefore treat
# any non-numeric value as "no single line", which every caller here does by
# case rather than by arithmetic.
workflow_line_of() {
    awk -v needle="$2" '
        index($0, needle) { hits++; if (hits == 1) line = NR }
        END {
            if (hits == 1) print line
            else if (hits > 1) print "ambiguous:" hits
        }
    ' "$1"
}

# anchor_unique <workflow> <anchor> <workflow_line_of result> <what a missing
# one means> — 0 when the result is a line number, otherwise 1 with
# IMPORT_ORDER_VERDICT set to the diagnosis.
#
# One function for both non-numeric outcomes, so the three anchors cannot drift
# into three different accounts of the same two problems.
anchor_unique() {
    local wf="$1" anchor="$2" got="$3" missing_means="$4"
    case "$got" in
        ambiguous:*)
            IMPORT_ORDER_VERDICT="'$anchor' appears ${got#ambiguous:} times in $wf — no longer unique, so which line it names would be a guess. Nothing about the step order was checked."
            return 1
            ;;
        '')
            IMPORT_ORDER_VERDICT="no line of $wf reads '$anchor' — $missing_means Nothing about the step order was checked."
            return 1
            ;;
    esac
    return 0
}

# workflow_contains <file> <fixed needle> — 0 when at least one line contains
# it. `index()` rather than grep, for the reason every other reader in this
# group gives: the needles here carry `$`, `(` and `/`, and a regex reading of
# them asks a different question.
workflow_contains() {
    awk -v needle="$2" '
        index($0, needle) { found = 1; exit }
        END { exit(found ? 0 : 1) }
    ' "$1"
}

# workflow_run_start <file> <a step's `- name:` line number> — the line number
# of that step's own `run:` block header, or nothing when the step has none
# (a `uses:` step) — the scan stops at the NEXT step's `- name:` line, so it can
# never report a later step's body as this one's.
workflow_run_start() {
    awk -v start="$2" '
        NR <= start { next }
        /^      - name: / { exit }
        /^[[:space:]]*run:[[:space:]]*[|>]/ { print NR; exit }
    ' "$1"
}

# workflow_run_before <file> <line number> — the LAST `run:` block header
# strictly above <line number>, which is how a command line is traced back to
# the body it lives in without parsing YAML.
workflow_run_before() {
    awk -v stop="$2" '
        NR >= stop { exit }
        /^[[:space:]]*run:[[:space:]]*[|>]/ { last = NR }
        END { if (last) print last }
    ' "$1"
}

# workflow_has_scrub <file> <after> <before> <extra token, or ""> <max
# non-comment lines to read, 0 = all>
#
# 0 when the region strictly between <after> and <before> holds BOTH:
#
#   * an `unset` line naming BASH_ENV (and <extra token>, when one is given);
#   * an `export PATH=` line whose value STARTS with /usr/bin — prepending is
#     the whole point, and a PATH that merely mentions /usr/bin somewhere is the
#     shape the scrub exists to reject.
#
# Comments and blank lines are skipped, so the check is about what the step
# RUNS. <max> exists for the import step, where the requirement is that the
# preamble comes FIRST: with a limit of 3 a scrub moved below the first
# `security` call no longer satisfies this, which is the property that matters —
# BASH_ENV planted by the build has to be neutralised before anything sensitive
# runs, not eventually.
workflow_has_scrub() {
    awk -v from="$2" -v to="$3" -v extra="$4" -v maxn="$5" '
        NR <= from { next }
        NR >= to { exit }
        { s = $0; sub(/^[ \t]+/, "", s) }
        s == "" { next }
        substr(s, 1, 1) == "#" { next }
        {
            seen++
            if (maxn > 0 && seen > maxn) exit
            if (substr(s, 1, 6) == "unset " && index(s, "BASH_ENV") &&
                (extra == "" || index(s, extra))) unset_ok = 1
            if (substr(s, 1, 12) == "export PATH=") {
                v = substr(s, 13)
                sub(/^"/, "", v)
                if (substr(v, 1, 8) == "/usr/bin") path_ok = 1
            }
        }
        END { exit((unset_ok && path_ok) ? 0 : 1) }
    ' "$1"
}

# workflow_security_before_build <file> <the unsigned build's line number> —
# the line number of the FIRST non-comment line inside ANY `run:` block above
# it that runs a `security ` command; nothing when there is none.
#
# THE EMBARGO, and it is deliberately not anchored on a step name or on a
# particular subcommand. The four `expect_keychain_op_after_build` cases below
# pin four exact invocations, which leaves the whole space of other spellings:
# a `security import` with different arguments, a `security create-keychain`
# with a differently-named password variable, an importer written as a helper
# script — every one of them puts a private key on the machine before the
# compile and passes those four cases. So the token is `security ` with nothing
# else demanded of it, in the region where NO reason to run it exists.
#
# `run:` block membership is tracked by INDENTATION: a block opens at
# `run: |` / `run: >` and holds every line indented further than that key, which
# is what YAML's block scalar means and is enough here without a parser.
# Comment lines are excluded, because this group's whole subject is prose that
# discusses `security` sitting next to code that runs it.
workflow_security_before_build() {
    awk -v stop="$2" '
        NR >= stop { exit }
        {
            line = $0
            match(line, /^ */)
            ind = RLENGTH
            if (in_run) {
                if (line ~ /^[[:space:]]*$/) next
                if (ind <= run_ind) {
                    in_run = 0
                } else {
                    s = line
                    sub(/^[ \t]+/, "", s)
                    if (substr(s, 1, 1) == "#") next
                    if (index(line, "security ")) { print NR; exit }
                    next
                }
            }
            if (line ~ /^[[:space:]]*run:[[:space:]]*[|>]/) {
                in_run = 1
                run_ind = ind
            }
        }
    ' "$1"
}

# import_order_check <workflow path> — the assertion itself, as a function so
# that the same code grades the real workflow and every known-bad copy below.
# That is the whole reason it is not written inline: a mutation proof that runs
# different code from the real check proves nothing about the real check.
#
# Sets IMPORT_ORDER_VERDICT rather than printing, so a caller can grade a
# failure without the verdict landing in the suite's output. Returns:
#
#   0  every property below holds — the only shape REQ-551 accepts
#   1  every anchor is present and a PROPERTY does not hold (an order, a
#      position, a missing scrub, a `security` call before the build)
#   2  an anchor is missing or ambiguous, or the file could not be read; the
#      verdict names WHICH, because "renamed", "duplicated" and "reordered" are
#      three regressions with three different fixes
#
# The properties, and THE ORDER THEY ARE ASKED IN IS LOAD-BEARING. The
# import-above-build regression trips the embargo too — moving the import step
# above the build is precisely a `security` call before the build — so the
# ordering question has to be settled first or the flagship mutation would be
# reported as an embargo violation and every reader would be sent to the wrong
# fix.
#
#   1. build < import < pack           the reorder itself (BR-1/ADR-551-2)
#   2. pack < early destroy < smoke    the window's far edge: the identity is
#                                      gone before anything runs the freshly
#                                      built binaries
#   3. the signing step's env scrub     TETON_CODESIGN/BASH_ENV unset, system
#                                      directories first on PATH
#   4. the import step's preamble       the same, FIRST in its body, and its
#                                      keychain calls spelled absolutely
#   5. no `security` before the build   the catch-all the four exact-invocation
#                                      cases below cannot express
IMPORT_ORDER_VERDICT=""
import_order_check() {
    local wf="$1"
    local build_ln import_ln pack_ln destroy_ln smoke_ln
    local sign_run_ln import_run_ln embargo_ln
    IMPORT_ORDER_VERDICT=""

    if [ ! -f "$wf" ]; then
        IMPORT_ORDER_VERDICT="$wf is not a file, so no step order was read."
        return 2
    fi

    build_ln="$(workflow_line_of "$wf" "$ORDER_ANCHOR_BUILD")"
    import_ln="$(workflow_line_of "$wf" "$ORDER_ANCHOR_IMPORT")"
    pack_ln="$(workflow_line_of "$wf" "$ORDER_ANCHOR_PACK")"
    destroy_ln="$(workflow_line_of "$wf" "$ORDER_ANCHOR_DESTROY")"
    smoke_ln="$(workflow_line_of "$wf" "$ORDER_ANCHOR_SMOKE")"

    anchor_unique "$wf" "$ORDER_ANCHOR_BUILD" "$build_ln" \
        "the unsigned-build anchor vanished (phase argument dropped, or the invocation rewritten)." ||
        return 2
    anchor_unique "$wf" "$ORDER_ANCHOR_IMPORT" "$import_ln" \
        "the import step was renamed or removed." ||
        return 2
    anchor_unique "$wf" "$ORDER_ANCHOR_PACK" "$pack_ln" \
        "the signing anchor vanished (phase argument dropped, or the invocation rewritten)." ||
        return 2
    anchor_unique "$wf" "$ORDER_ANCHOR_DESTROY" "$destroy_ln" \
        "the EARLY keychain destroy was renamed or removed — it is the step that closes BR-1's window, and the 'if: always()' backstop at the bottom of the job does not close it before the smoke." ||
        return 2
    anchor_unique "$wf" "$ORDER_ANCHOR_SMOKE" "$smoke_ln" \
        "the smoke step was renamed or removed, so 'the identity is gone before anything runs the built binaries' has no step to be measured against." ||
        return 2

    if ! { [ "$build_ln" -lt "$import_ln" ] && [ "$import_ln" -lt "$pack_ln" ]; }; then
        IMPORT_ORDER_VERDICT="the identity import is not strictly between the two phases: build l.$build_ln, import l.$import_ln, pack l.$pack_ln. A Developer ID key must not be on the machine while cargo builds (BR-1/ADR-551-2)."
        return 1
    fi

    if ! { [ "$pack_ln" -lt "$destroy_ln" ] && [ "$destroy_ln" -lt "$smoke_ln" ]; }; then
        IMPORT_ORDER_VERDICT="the early keychain destroy is not strictly between the pack and the smoke: pack l.$pack_ln, destroy l.$destroy_ln, smoke l.$smoke_ln. The smoke EXECUTES the binaries this job built, so an identity that is still on the machine when it runs puts third-party code back inside the window REQ-551 narrowed (BR-1)."
        return 1
    fi

    sign_run_ln="$(workflow_run_before "$wf" "$pack_ln")"
    if [ -z "$sign_run_ln" ]; then
        IMPORT_ORDER_VERDICT="no 'run:' block header precedes the signing invocation at l.$pack_ln, so the signing step's body could not be located and its environment scrub was not checked."
        return 1
    fi
    if ! workflow_has_scrub "$wf" "$sign_run_ln" "$pack_ln" TETON_CODESIGN 0; then
        IMPORT_ORDER_VERDICT="the signing step's environment scrub is gone: between its 'run:' at l.$sign_run_ln and the pack invocation at l.$pack_ln there is no 'unset' naming both TETON_CODESIGN and BASH_ENV, or no 'export PATH=' starting at /usr/bin. The build step can append to \$GITHUB_ENV/\$GITHUB_PATH and the runner applies both BETWEEN steps, so without those lines package.sh's seam refusal is defeatable from the step it guards against."
        return 1
    fi

    import_run_ln="$(workflow_run_start "$wf" "$import_ln")"
    if [ -z "$import_run_ln" ]; then
        IMPORT_ORDER_VERDICT="the import step at l.$import_ln has no 'run:' block, so its preamble could not be checked."
        return 1
    fi
    # Bounded by the pack line only as a backstop; the real bound is the
    # three-non-comment-line limit, which is what makes this "FIRST in the body"
    # rather than "somewhere in the step".
    if ! workflow_has_scrub "$wf" "$import_run_ln" "$pack_ln" "" 3; then
        IMPORT_ORDER_VERDICT="the import step's preamble is gone or is no longer first: the first three non-comment lines after its 'run:' at l.$import_run_ln do not include an 'unset' naming BASH_ENV and an 'export PATH=' starting at /usr/bin. This is the step that holds the raw certificate and its password, and it runs downstream of every third-party build script in the tree."
        return 1
    fi
    if ! workflow_contains "$wf" '/usr/bin/security import'; then
        IMPORT_ORDER_VERDICT="the import step no longer spells its keychain calls absolutely — no line reads '/usr/bin/security import'. An unqualified 'security' can be shadowed by an exported shell function or a PATH entry an earlier step wrote, which is exactly what the preamble exists to make impossible."
        return 1
    fi

    embargo_ln="$(workflow_security_before_build "$wf" "$build_ln")"
    if [ -n "$embargo_ln" ]; then
        IMPORT_ORDER_VERDICT="line $embargo_ln runs a 'security' command inside a run: block BEFORE the unsigned build at l.$build_ln. Whatever it is spelled, a keychain operation there puts credentials on the machine while cargo compiles third-party sources (BR-1) — the four exact-invocation cases below cannot see a differently-spelled importer, and this is what catches it."
        return 1
    fi

    IMPORT_ORDER_VERDICT="build l.$build_ln < import l.$import_ln < pack l.$pack_ln < early destroy l.$destroy_ln < smoke l.$smoke_ln; both scrubs present; nothing runs 'security' before the build"
    return 0
}

# expect_order <expected status> <label> <workflow path>
expect_order() {
    local expected="$1" label="$2" wf="$3"
    local status=0
    reset_case
    import_order_check "$wf" || status=$?
    if [ "$status" -eq "$expected" ]; then
        report_pass "$label"
    else
        report_fail "$label [expected order-status $expected, got $status]" \
            "$IMPORT_ORDER_VERDICT"
    fi
    return 0
}

# verdict_names <fixed string> — reads the verdict the last expect_order left.
# `case`, not `grep -qF`, for the reason release.yml's own identity assertion
# gives at length: a grep that is missing or killed exits non-zero, which would
# read here as "the verdict does not say that" and turn a tool failure into a
# report about the workflow (LESSON-442).
verdict_names() {
    case "$IMPORT_ORDER_VERDICT" in
        *"$1"*) return 0 ;;
        *) return 1 ;;
    esac
}

# workflow_mutate <src> <dst> <needle> <replacement> — <src> copied to <dst>
# with the FIRST occurrence of the fixed string <needle> replaced. Exits
# non-zero when the needle was not there at all, so a mutation that silently
# changed nothing cannot be graded as a known-bad case: that would be a
# known-bad copy of the GOOD file, reported green.
workflow_mutate() {
    awk -v needle="$3" -v repl="$4" '
        !mutated {
            p = index($0, needle)
            if (p) {
                $0 = substr($0, 1, p - 1) repl substr($0, p + length(needle))
                mutated = 1
            }
        }
        { print }
        END { exit(mutated ? 0 : 3) }
    ' "$1" >"$2"
}

# expect_order_mutant <name> <needle> <replacement> <expected status>
#                     <verdict needle> <label>
#
# SIX parameters, and the sixth is the one the label is built from — this
# comment listed five for as long as the function had six, which is how a reader
# ends up passing the label into <verdict needle> and grading a case against its
# own name.
#
# Builds a known-bad copy under the scratch directory, grades it, and asserts
# the verdict names what changed. A mutation that would not apply is itself a
# failure, named as one.
expect_order_mutant() {
    local name="$1" needle="$2" repl="$3" expected="$4" verdict_needle="$5"
    local label="$6"
    local dst="$order_scratch/$name.yml"
    if ! workflow_mutate "$RELEASE_WORKFLOW" "$dst" "$needle" "$repl"; then
        report_fail "$label [the known-bad copy could not be built: release.yml no longer contains '$needle']"
        return 0
    fi
    expect_order "$expected" "$label" "$dst"
    assert "  ... and the verdict names it: '$verdict_needle'" \
        verdict_names "$verdict_needle"
    return 0
}

# workflow_step_relocate <src> <dst> <step anchor> <destination anchor>
#
# <src> copied to <dst> with the whole step whose `- name:` line contains
# <step anchor> lifted out — from that line to the line before the next step's
# — and reinserted immediately before the `- name:` line containing
# <destination anchor>. An EMPTY destination deletes the step instead.
#
# The sibling of workflow_mutate, for the mutations a single-line replacement
# cannot express: "this step is gone" and "this step is in the wrong place" are
# both whole-step edits, and both are regressions a reviewer would plausibly
# wave through. Exits 3 when an anchor is not there or the destination is inside
# the step being moved, so a mutation that changed nothing — or changed
# something incoherent — cannot be graded as a known-bad case.
workflow_step_relocate() {
    awk -v step_anchor="$3" -v dest_anchor="$4" '
        { lines[NR] = $0 }
        /^      - name: / {
            if (!s_start && index($0, step_anchor)) s_start = NR
            else if (s_start && !s_end) s_end = NR - 1
            if (dest_anchor != "" && !d_line && index($0, dest_anchor)) d_line = NR
        }
        END {
            if (!s_start) exit 3
            if (!s_end) s_end = NR
            if (dest_anchor != "") {
                if (!d_line) exit 3
                if (d_line >= s_start && d_line <= s_end) exit 3
            }
            for (i = 1; i <= NR; i++) {
                if (dest_anchor != "" && i == d_line)
                    for (j = s_start; j <= s_end; j++) print lines[j]
                if (i >= s_start && i <= s_end) continue
                print lines[i]
            }
        }
    ' "$1" >"$2"
}

# expect_order_relocation <name> <step anchor> <destination anchor>
#                         <expected status> <verdict needle> <label>
#
# workflow_step_relocate's grader, shaped exactly like expect_order_mutant so
# the two kinds of known-bad copy read the same in the log.
expect_order_relocation() {
    local name="$1" step_anchor="$2" dest_anchor="$3" expected="$4"
    local verdict_needle="$5" label="$6"
    local dst="$order_scratch/$name.yml"
    if ! workflow_step_relocate "$RELEASE_WORKFLOW" "$dst" "$step_anchor" "$dest_anchor"; then
        report_fail "$label [the known-bad copy could not be built from release.yml: '$step_anchor' or '$dest_anchor' is not a step name in it]"
        return 0
    fi
    expect_order "$expected" "$label" "$dst"
    assert "  ... and the verdict names it: '$verdict_needle'" \
        verdict_names "$verdict_needle"
    return 0
}

order_scratch="$work/release-yml-mutants"
mkdir -p "$order_scratch"
order_workflow_before="$(sha256_of "$RELEASE_WORKFLOW")"

# The real file, and the case that goes red the day someone moves the step.
expect_order 0 "release.yml imports the identity AFTER the build and BEFORE the pack" \
    "$RELEASE_WORKFLOW"

# Each anchor as its own case, so a rename or a dropped phase argument names
# ITSELF in the log rather than arriving as one opaque ordering failure.
assert "the unsigned build is still invoked with the 'build' phase argument" \
    grep -Fq -- "$ORDER_ANCHOR_BUILD" "$RELEASE_WORKFLOW"
assert "the import step is still named 'Import the Developer ID signing identity'" \
    grep -Fq -- "$ORDER_ANCHOR_IMPORT" "$RELEASE_WORKFLOW"
assert "the signing pass is still invoked with the 'pack' phase argument" \
    grep -Fq -- "$ORDER_ANCHOR_PACK" "$RELEASE_WORKFLOW"
assert "the early keychain destroy is still a step, named '(early)'" \
    grep -Fq -- "$ORDER_ANCHOR_DESTROY" "$RELEASE_WORKFLOW"
assert "the smoke step is still named 'Smoke the tarball (BR-7/BR-9)'" \
    grep -Fq -- "$ORDER_ANCHOR_SMOKE" "$RELEASE_WORKFLOW"
# The absolute spelling, as its own case, so a `security` that quietly loses its
# /usr/bin names ITSELF here rather than arriving as one opaque order failure.
assert "the import step calls security by absolute path" \
    grep -Fq -- "/usr/bin/security import" "$RELEASE_WORKFLOW"

# KNOWN-BAD, and the reason this group exists: AC-4's mutation, run on every
# suite run instead of once by hand. The whole import step — from its `- name:`
# line to the line before the next step's — is lifted out of a copy and
# reinserted immediately above the step that invokes the `build` phase, which
# is the pre-REQ-551 workflow and the exact regression a reviewer would wave
# through as "just moving a step back". The build step's own `- name:` line is
# DERIVED from the build anchor (the last `- name:` at or above it) rather than
# spelled out, so this mutation carries no fourth string to keep in sync.
order_moved="$order_scratch/import-above-build.yml"
order_move_status=0
awk -v import_anchor="$ORDER_ANCHOR_IMPORT" -v build_anchor="$ORDER_ANCHOR_BUILD" '
    { lines[NR] = $0 }
    /^      - name: / { last_name = NR }
    !build_step && index($0, build_anchor) { build_step = last_name }
    !imp_start && index($0, import_anchor) { imp_start = NR }
    imp_start && !imp_end && NR > imp_start && /^      - name: / { imp_end = NR - 1 }
    END {
        if (!imp_start || !build_step || build_step >= imp_start) exit 3
        if (!imp_end) imp_end = NR
        for (i = 1; i <= NR; i++) {
            if (i == build_step)
                for (j = imp_start; j <= imp_end; j++) print lines[j]
            if (i >= imp_start && i <= imp_end) continue
            print lines[i]
        }
    }
' "$RELEASE_WORKFLOW" >"$order_moved" || order_move_status=$?

if [ "$order_move_status" -ne 0 ]; then
    report_fail "a copy with the import step moved ABOVE the build (AC-4) [the mutation could not be constructed from release.yml, awk exited $order_move_status]"
else
    expect_order 1 "a copy with the import step moved ABOVE the build -> RED (AC-4)" \
        "$order_moved"
    assert "  ... naming the inversion rather than a missing anchor" \
        verdict_names "not strictly between"
fi

# KNOWN-BAD: the step renamed. The anchor is a step NAME, so this is the
# realistic drift — and the one that must not read as "the order is fine".
expect_order_mutant renamed-import \
    "$ORDER_ANCHOR_IMPORT" "- name: Set up code signing" \
    2 "$ORDER_ANCHOR_IMPORT" \
    "a copy whose import step is RENAMED -> RED, not a vacuous pass"

# KNOWN-BAD: the phase arguments dropped, which is how the split gets undone
# one invocation at a time. `dist` alone is package.sh's `all` default — a
# single step that compiles AND signs, with the identity already imported.
# shellcheck disable=SC2016  # deliberate: the workflow's bytes, not ours
expect_order_mutant no-build-phase \
    "$ORDER_ANCHOR_BUILD" 'bash tools/release/package.sh "$TARGET" "$VERSION" dist' \
    2 "$ORDER_ANCHOR_BUILD" \
    "a copy with the 'build' phase argument dropped -> RED"
# shellcheck disable=SC2016  # deliberate: the workflow's bytes, not ours
expect_order_mutant no-pack-phase \
    "$ORDER_ANCHOR_PACK" 'bash tools/release/package.sh "$TARGET" "$VERSION" dist' \
    2 "$ORDER_ANCHOR_PACK" \
    "a copy with the 'pack' phase argument dropped -> RED"

# KNOWN-BAD: an anchor that appears TWICE. The realistic shape is a step copied
# rather than moved — a retry, a second matrix leg, a paste somebody meant to
# edit — and it is the case a line-number check is quietest about, because "the
# first match" is still a number and still compares. Before the ambiguity
# sentinel existed this file reported it as the anchor having VANISHED, which is
# the opposite diagnosis and sends the reader hunting for a deletion.
#
# The replacement carries a `\n`, which `awk -v` turns into a real newline: the
# one matching line becomes two identical ones.
# shellcheck disable=SC2016  # deliberate: the workflow's bytes, not ours
expect_order_mutant duplicated-pack \
    "$ORDER_ANCHOR_PACK" "$ORDER_ANCHOR_PACK\\n          $ORDER_ANCHOR_PACK" \
    2 "appears 2 times" \
    "a copy with the pack anchor DUPLICATED -> RED as ambiguous, not as missing"

# KNOWN-BAD: the EARLY DESTROY deleted outright. Every control this group grew
# in REQ-551's third pass was, until these four cases existed, entirely
# unguarded — the round-2 review added the steps and the round-3 review proved
# by mutation that deleting them left the suite green. A control nothing can
# fail is a comment.
#
# The whole step is lifted out rather than renamed, because "somebody removed
# the cleanup" is the regression, and it must not be reachable by a mutation
# this suite cannot construct. What is left behind is the `if: always()`
# backstop, which runs after the smoke and the upload — a workflow that still
# destroys the keychain, just not before the binaries are executed.
expect_order_relocation no-early-destroy \
    "$ORDER_ANCHOR_DESTROY" "" \
    2 "$ORDER_ANCHOR_DESTROY" \
    "a copy with the early keychain destroy step DELETED -> RED"

# KNOWN-BAD: the same step kept, moved after the smoke. This is the quieter
# regression of the two — every anchor is present, the step still exists, the
# job still cleans up — and it puts the smoke, which EXECUTES the freshly built
# binaries, back inside the window. A name-only check cannot see it, which is
# why the position is graded rather than the presence.
expect_order_relocation destroy-after-smoke \
    "$ORDER_ANCHOR_DESTROY" "- name: Upload target artifact" \
    1 "not strictly between the pack and the smoke" \
    "a copy with the early destroy MOVED after the smoke -> RED"

# KNOWN-BAD: the signing step's environment scrub deleted. The line is replaced
# with a shell no-op rather than removed, so the mutation is a one-line edit of
# exactly the kind a reviewer waves through as tidying.
expect_order_mutant no-sign-scrub \
    "unset TETON_CODESIGN TETON_ALLOW_TOOL_SEAM BASH_ENV ENV CDPATH" ":" \
    1 "environment scrub is gone" \
    "a copy with the signing step's env scrub deleted -> RED"

# KNOWN-BAD: an importer BEFORE the build, spelled differently from the four
# invocations `expect_keychain_op_after_build` pins. This is the hole the
# embargo closes: those four cases match exact argument lists, so a keychain
# operation written any other way — a different subcommand, different variable
# names, a helper script — sails past all of them while putting credentials on
# the machine for the whole compile.
#
# `\n` in the replacement is turned into real newlines by `awk -v`, so one line
# becomes a whole inserted step.
expect_order_mutant security-before-build \
    "      - name: Build (unsigned)" \
    "      - name: Warm up the keychain (known-bad)\\n        run: |\\n          security list-keychains -d user\\n      - name: Build (unsigned)" \
    1 "BEFORE the unsigned build" \
    "a copy with a differently-spelled security call BEFORE the build -> RED"

# Nine mutations, none of them to the tracked file. A suite that edits the
# workflow it asserts about — even to put it back — is one interrupted run away
# from a working tree nobody trusts.
assert "release.yml itself was never written to: every mutation was a copy" \
    [ "$(sha256_of "$RELEASE_WORKFLOW")" = "$order_workflow_before" ]

# --- what the ordering is FOR ----------------------------------------------
#
# The three anchors above pin an order between two package.sh invocations and a
# step NAME. A step name is a label, and a label is not a behaviour: the import
# step could keep its name, keep its position, and have its keychain work moved
# into a differently-named step above the build — and every case above would
# still be green while a Developer ID private key sat on the machine for the
# whole of the compile, which is the one thing BR-1 forbids.
#
# So these cases anchor on the KEYCHAIN OPERATIONS themselves, wherever they
# live: each must appear exactly once in the workflow, and after the unsigned
# build. Each needle is the invocation, spelled precisely enough to exclude the
# prose that discusses it — `security import` alone appears on several lines of
# release.yml, most of them comments and error text, and an assertion that
# counted those would be counting sentences.
#
# THE IMPORT NEEDLE IS ABSOLUTE — `/usr/bin/security import "$p12"`, not
# `security import "$p12"` — because that is now the spelling, and a needle that
# accepted either would go on passing after the /usr/bin were dropped. An
# unqualified `security` is resolvable by an exported shell function or a PATH
# entry that the untrusted build step wrote to `$GITHUB_PATH`, so the absolute
# path is a control and not a style choice. (The other three needles below
# match either spelling as substrings; this one pins the property.)
#
# `delete-keychain` and `list-keychains` are deliberately NOT pinned this way:
# destroying the keychain is something the workflow may reasonably do in more
# than one place (early, and again in an always() cleanup), so "exactly once" is
# the wrong shape for them. Creating, unlocking, importing into and unlocking
# the KEY are the operations that put a usable private key on the machine, and
# those are once-only by construction.

group "the keychain operations themselves are after the build (BR-1)"

# expect_keychain_op_after_build <label> <needle>
expect_keychain_op_after_build() {
    local label="$1" needle="$2"
    local build_ln op_ln
    reset_case
    build_ln="$(workflow_line_of "$RELEASE_WORKFLOW" "$ORDER_ANCHOR_BUILD")"
    op_ln="$(workflow_line_of "$RELEASE_WORKFLOW" "$needle")"
    case "$build_ln" in
        '' | ambiguous:*)
            report_fail "$label [no single line of release.yml invokes the unsigned build, so 'after the build' has no line to be after: $ORDER_ANCHOR_BUILD]"
            return 0
            ;;
    esac
    case "$op_ln" in
        '')
            report_fail "$label [no line of release.yml runs: $needle]"
            return 0
            ;;
        ambiguous:*)
            report_fail "$label [release.yml runs it on ${op_ln#ambiguous:} lines — it must happen once, in one place: $needle]"
            return 0
            ;;
    esac
    if [ "$op_ln" -gt "$build_ln" ]; then
        report_pass "$label"
    else
        report_fail "$label [line $op_ln runs it, and the unsigned build is line $build_ln — the key is on the machine while cargo runs (BR-1)]"
    fi
    return 0
}

# shellcheck disable=SC2016  # deliberate: the workflow's bytes, not ours
expect_keychain_op_after_build \
    "the signing keychain is CREATED after the unsigned build, once" \
    'security create-keychain -p "$keychain_pw"'
# shellcheck disable=SC2016  # deliberate: the workflow's bytes, not ours
expect_keychain_op_after_build \
    "the signing keychain is UNLOCKED after the unsigned build, once" \
    'security unlock-keychain -p "$keychain_pw"'
# shellcheck disable=SC2016  # deliberate: the workflow's bytes, not ours
expect_keychain_op_after_build \
    "the Developer ID .p12 is IMPORTED after the unsigned build, once, by absolute path" \
    '/usr/bin/security import "$p12"'
expect_keychain_op_after_build \
    "the private key is made usable without a prompt after the unsigned build, once" \
    'security set-key-partition-list -S apple-tool:,apple:'

# --- the `if:` predicates that decide whether any of it runs ----------------
#
# Every ordering assertion in this file is about line NUMBERS, and a step that
# is skipped has line numbers too. `if: contains(matrix.target, 'apple-darwin')`
# on the import step is what makes the order above a statement about the macOS
# legs rather than about a file; flip it to `false`, or to a target expression
# that matches nothing, and the identity is never imported, the pack step signs
# nothing, and the leg ships unsigned binaries with every case above still
# green. (`package.sh` refuses to ship unsigned when TETON_SIGN_IDENTITY is set,
# which is the backstop — but a workflow whose steps silently stop running is
# worth catching where it is written.)
#
# The predicate is pinned as the line IMMEDIATELY AFTER each step's `- name:`,
# which is both where GitHub Actions convention puts it and what makes "this
# predicate belongs to that step" checkable without a YAML parser. An exact
# string, indentation included: a `!` or a different matrix key is the whole
# bug, and a fuzzy match is how it survives.

group "the darwin/linux step predicates in release.yml"

IF_DARWIN="        if: contains(matrix.target, 'apple-darwin')"
IF_NOT_DARWIN="        if: \${{ !contains(matrix.target, 'apple-darwin') }}"

# expect_if_predicate <label> <step-name anchor> <expected `if:` line>
expect_if_predicate() {
    local label="$1" anchor="$2" want="$3"
    local ln got
    reset_case
    ln="$(workflow_line_of "$RELEASE_WORKFLOW" "$anchor")"
    case "$ln" in
        '')
            report_fail "$label [no line of release.yml reads: $anchor]"
            return 0
            ;;
        ambiguous:*)
            report_fail "$label [${ln#ambiguous:} lines of release.yml read it, so which step carries the predicate is a guess: $anchor]"
            return 0
            ;;
    esac
    got="$(awk -v n="$((ln + 1))" 'NR == n' "$RELEASE_WORKFLOW")"
    if [ "$got" = "$want" ]; then
        report_pass "$label"
    else
        report_fail "$label [line $((ln + 1)), the line under that step's name, is not the expected predicate]" \
            "expected: $want
got:      $got"
    fi
    return 0
}

expect_if_predicate "the unsigned build runs on the darwin legs and only those" \
    "- name: Build (unsigned)" "$IF_DARWIN"
expect_if_predicate "the identity import runs on the darwin legs and only those" \
    "$ORDER_ANCHOR_IMPORT" "$IF_DARWIN"
expect_if_predicate "the sign-and-pack step runs on the darwin legs and only those" \
    "- name: Sign and package" "$IF_DARWIN"
# The early destroy is NOT `always()`, deliberately — it is the happy-path
# narrowing and the `if: always()` step at the bottom of the job is the
# failure-path backstop. Pinned for the reason every predicate here is: a
# `false`, a negation or a drifted matrix key leaves the step in the file, in
# the right place, and never run, with every position case above still green.
expect_if_predicate "the early keychain destroy runs on the darwin legs and only those" \
    "$ORDER_ANCHOR_DESTROY" "$IF_DARWIN"
# The negated form, which is the half that keeps the Linux leg from being run
# TWICE — once by its own step and once by a darwin step whose predicate drifted.
expect_if_predicate "the Linux one-shot step runs on everything that is NOT darwin" \
    "- name: Build and package (Linux, unsigned)" "$IF_NOT_DARWIN"

# --- changelog-section.sh, and the Upgrade notes it feeds ------------------
#
# The release body used to be generated in full and read no file, so the
# CHANGELOG was a document with no reader: a disclosure could be written,
# reviewed and merged, and never reach the page where an upgrading user would
# look for it. release.yml now lifts the topmost changelog section into the
# body, which makes this script part of the release path and gives it the same
# obligation as everything else in this directory — to be exercised somewhere
# other than the release that needs it.
#
# Two things are graded here, and they are different questions:
#
#   the SCRIPT   given a changelog, does it print the newest section and only
#                that section — and given the several shapes of "there is
#                nothing to publish", does it print nothing and say so without
#                failing? The 75 cases are the load-bearing ones. A release
#                that dies because nobody wrote a changelog entry is a worse
#                outcome than a release with no notes, and the only thing
#                standing between the two is that this script distinguishes
#                "no section" from "I broke".
#
#   the WIRING   does release.yml actually call it, does the section land
#                between the platforms and the checksums, and is the extracted
#                text passed as a printf ARGUMENT rather than spliced into a
#                format string? A changelog full of backticks, `%s` and `\n`
#                rendered through a format string is a mangled disclosure at
#                best and a broken body at worst.
#
# Both halves matter on their own: a perfect extractor nothing calls publishes
# nothing, and a call to a broken extractor publishes garbage.

group "changelog-section.sh (the hand-written half of a release body)"

cs="$work/changelog"
mkdir -p "$cs"

# Stdout and stderr are kept APART. "It printed nothing" is a claim about
# stdout alone, and expect_exit folds stderr into it — so a case asserting
# silence would be satisfied by the very message the script prints to explain
# the silence. Both streams land in files the follow-up assertions read.
run_changelog_section() {
    bash "$CHANGELOG_SECTION" "$@" >"$cs/out" 2>"$cs/err"
}

cat >"$cs/normal.md" <<'CHANGELOG_EOF'
# Changelog

Preamble prose that belongs to no section.

## [Unreleased]

### Changed

- The newest thing.

## [0.1.9] - 2026-08-01

### Fixed

- The older thing.
CHANGELOG_EOF

expect_exit 0 "a changelog with a topmost section -> 0" \
    run_changelog_section "$cs/normal.md"
assert "  ... and prints that section's body" \
    grep -Fq -- "- The newest thing." "$cs/out"
refute "  ... and not the section under it" \
    grep -Fq -- "- The older thing." "$cs/out"
refute "  ... and not the preamble over it" \
    grep -Fq -- "Preamble prose" "$cs/out"
# The heading is deliberately NOT printed: the top section is `## [Unreleased]`
# until a release PR renames it, and a release page reading "Unreleased" under
# a tagged version is the failure this omission prevents.
refute "  ... and not the version heading, which would say 'Unreleased' on a tagged release" \
    grep -Fq -- "[Unreleased]" "$cs/out"
assert "  ... and starts at content, not at the blank line under the heading" \
    [ -n "$(head -n 1 "$cs/out")" ]

# --- the shapes that must publish NOTHING and still not fail a release -----

expect_exit 75 "no changelog at that path -> 75, which is not a failed release" \
    run_changelog_section "$cs/does-not-exist.md"
assert "  ... and stdout is empty, so the body gains no section" [ ! -s "$cs/out" ]
assert "  ... and the reason is on stderr" grep -Fq -- "nothing to print" "$cs/err"

: >"$cs/empty.md"
expect_exit 75 "an empty changelog -> 75" run_changelog_section "$cs/empty.md"
assert "  ... and stdout is empty" [ ! -s "$cs/out" ]

printf '# Changelog\n\nNo version sections in here yet.\n' >"$cs/no-section.md"
expect_exit 75 "a changelog with no '## ' section at all -> 75" \
    run_changelog_section "$cs/no-section.md"
assert "  ... and stdout is empty" [ ! -s "$cs/out" ]
# The two 75s have separate sentences on purpose: "there is no file" and "the
# file says nothing" send whoever reads the release log to different places.
assert "  ... and names the absent section, not an absent file" \
    grep -Fq -- "has no '## ' section" "$cs/err"

printf '# Changelog\n\n## [Unreleased]\n\n## [0.1.9]\n\n### Fixed\n\n- The older thing.\n' \
    >"$cs/empty-section.md"
expect_exit 75 "a topmost section with no body -> 75" \
    run_changelog_section "$cs/empty-section.md"
assert "  ... and stdout is empty" [ ! -s "$cs/out" ]
# The one that would be a real incident: an empty newest section must not fall
# through to the previous release's notes. Publishing v0.1.9's changes as
# v0.1.10's upgrade notes is not a missing disclosure, it is a false one.
refute "  ... and the PREVIOUS release's section was not promoted into its place" \
    grep -Fq -- "- The older thing." "$cs/out"

# --- code fences are not section boundaries --------------------------------
#
# A ``` block can contain a line starting with `## `. Read as a heading, it
# ends the section early and publishes HALF of it — and a privacy disclosure
# truncated mid-sentence renders green and reads as complete.

cat >"$cs/fenced.md" <<'CHANGELOG_EOF'
# Changelog

## [Unreleased]

Before the fence.

```md
## This line is inside a fence and is not a section boundary.
```

After the fence.

## [0.1.9]

- The older thing.
CHANGELOG_EOF

expect_exit 0 "a '## ' inside a code fence does not end the section" \
    run_changelog_section "$cs/fenced.md"
assert "  ... so the prose after the fence is still published" \
    grep -Fq -- "After the fence." "$cs/out"
refute "  ... and the genuine next section is still excluded" \
    grep -Fq -- "- The older thing." "$cs/out"

cat >"$cs/indented-fence.md" <<'CHANGELOG_EOF'
# Changelog

## [Unreleased]

- A bullet whose fence is indented, the way this repo's changelog writes them:

  ```sh
  ## still not a heading
  ```

- The last bullet.

## [0.1.9]

- The older thing.
CHANGELOG_EOF

expect_exit 0 "an INDENTED fence is a fence too (this repo's changelog indents them)" \
    run_changelog_section "$cs/indented-fence.md"
assert "  ... so the bullet after it survives" \
    grep -Fq -- "- The last bullet." "$cs/out"
refute "  ... and the genuine next section is still excluded" \
    grep -Fq -- "- The older thing." "$cs/out"

# The real file, because every case above is a fixture this suite wrote to be
# parseable. Whether the changelog THIS repo actually ships parses is a
# different claim, and it is the one that decides what the next release page
# says. Not in the `required` list at the top: a checkout without a changelog
# is a legal state that publishes no notes, and this suite says so by skipping
# rather than by aborting.
CHANGELOG_FILE="$repo_root/CHANGELOG.md"
if [ -f "$CHANGELOG_FILE" ]; then
    expect_exit 0 "this repo's own CHANGELOG.md yields a section" \
        run_changelog_section "$CHANGELOG_FILE"
    assert "  ... with a body the release body can carry" [ -s "$cs/out" ]
else
    skip "this repo's own CHANGELOG.md (the file is not in this checkout)"
fi

# --- the wiring in release.yml ---------------------------------------------

# Single-quoted: `$(...)` and `$upgrade_notes` are release.yml's text, not this
# script's variables.
# shellcheck disable=SC2016  # deliberate: these are the workflow's bytes
CS_ANCHOR_CALL='upgrade_notes="$(bash tools/release/changelog-section.sh)"'
CS_ANCHOR_UPGRADE="printf '### Upgrade notes"
CS_ANCHOR_PLATFORMS="printf '### Platforms"
CS_ANCHOR_CHECKSUMS="printf '### Checksums (sha256)"
# `%s` with the section as an ARGUMENT. The changelog is prose full of
# backticks, percent signs and backslashes; the day it reaches a printf FORMAT
# string is the day a release body renders as garbage — or silently drops the
# half of a disclosure that followed a stray `%`.
CS_ANCHOR_PRINT="printf '%s\\n\\n' \"\$upgrade_notes\""
# The 75 branch, pinned by its user-visible title. What this really guards is
# the shape it replaced: a `|| true`, which would turn a BROKEN extractor into
# a silently note-less release.
CS_ANCHOR_NOTICE="title=No upgrade notes"

assert "release.yml calls changelog-section.sh for the release body" \
    grep -Fq -- "$CS_ANCHOR_CALL" "$RELEASE_WORKFLOW"
assert "release.yml renders an 'Upgrade notes' heading" \
    grep -Fq -- "$CS_ANCHOR_UPGRADE" "$RELEASE_WORKFLOW"
assert "the section is passed as a printf ARGUMENT, never as part of a format string" \
    grep -Fq -- "$CS_ANCHOR_PRINT" "$RELEASE_WORKFLOW"
assert "a changelog with no section is a notice, not a failed release" \
    grep -Fq -- "$CS_ANCHOR_NOTICE" "$RELEASE_WORKFLOW"
# The dry-run branch renders the same `$notes` file the publish uploads, so the
# Upgrade notes section is visible from a workflow_dispatch without burning a
# tag — the property the rest of this workflow is built to preserve.
# shellcheck disable=SC2016  # deliberate: the workflow's bytes
assert "the dry-run branch prints the rendered notes, Upgrade notes included" \
    grep -Fq -- 'cat "$notes"' "$RELEASE_WORKFLOW"

cs_line_platforms="$(workflow_line_of "$RELEASE_WORKFLOW" "$CS_ANCHOR_PLATFORMS")"
cs_line_upgrade="$(workflow_line_of "$RELEASE_WORKFLOW" "$CS_ANCHOR_UPGRADE")"
cs_line_checksums="$(workflow_line_of "$RELEASE_WORKFLOW" "$CS_ANCHOR_CHECKSUMS")"
cs_anchors_ok=1
for cs_got in "$cs_line_platforms" "$cs_line_upgrade" "$cs_line_checksums"; do
    case "$cs_got" in
        '' | ambiguous:*) cs_anchors_ok=0 ;;
    esac
done
if [ "$cs_anchors_ok" -eq 1 ]; then
    # Upgrade notes belong where a reader meets them BEFORE deciding to take
    # the release. The checksums are what they need afterwards.
    assert "the Upgrade notes section renders after the Platforms block" \
        [ "$cs_line_platforms" -lt "$cs_line_upgrade" ]
    assert "  ... and before the checksums" \
        [ "$cs_line_upgrade" -lt "$cs_line_checksums" ]
else
    # Named, never a quiet pass — the same rule the ordering group above
    # follows. An assertion satisfied by deleting what it reads is not one.
    report_fail "the Upgrade notes section's position in release.yml" \
        "one of the three anchors is missing or ambiguous:
  platforms=${cs_line_platforms:-<none>}
  upgrade=${cs_line_upgrade:-<none>}
  checksums=${cs_line_checksums:-<none>}"
fi

# --- summary ---------------------------------------------------------------

total=$((passed + failed))
summary="selftest: $passed/$total passed, $failed failed"
# Skips are reported, never folded into the pass count: a case that could not
# run on this machine is not evidence that the thing it covers works.
if [ "$skipped" -ne 0 ]; then
    summary="$summary, $skipped skipped (not exercised on this machine)"
fi
printf '\n%s\n' "$summary."

if [ "$failed" -ne 0 ]; then
    exit "$EXIT_FAILED"
fi
