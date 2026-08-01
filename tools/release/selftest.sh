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
SITE_RENDER="$repo_root/site/render.sh"
SITE_TEMPLATE="$repo_root/site/index.html"
FORMULA_TEMPLATE="$repo_root/packaging/homebrew/teton.rb.tmpl"

for required in "$LIB" "$VERIFY" "$PACKAGE" "$RENDER_FORMULA" "$SMOKE" \
    "$VERIFY_SIGNATURE" "$VERIFY_ATTESTATION" \
    "$SITE_RENDER" "$SITE_TEMPLATE" "$FORMULA_TEMPLATE"; do
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

# assert <label> <command...> — passes when the command succeeds.
assert() {
    local label="$1"
    shift
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
    printf '  SKIP  %s\n' "$1"
    return 0
}

# --- syntax ----------------------------------------------------------------

group "syntax (bash -n)"
for s in "$LIB" "$VERIFY" "$PACKAGE" "$RENDER_FORMULA" "$SMOKE" \
    "$VERIFY_SIGNATURE" "$VERIFY_ATTESTATION" \
    "$SITE_RENDER" "${BASH_SOURCE[0]}"; do
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
# A stand-in `codesign`, answering the two questions verify-signature.sh asks:
# `--verify --strict` (silent, exit 0, when the seal holds) and `-dvv` (the
# identity dump, written to STDERR, which is where the real tool writes it and
# why the gate reads it through 2>&1). The second `v` is why the dumps below
# carry Authority lines at all — at verbosity 1 codesign prints none.
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

case "\${1:-}" in
    --verify)
        if [ "\$mode" = reject ]; then
            echo "\${3:-}: a sealed resource is missing or invalid" >&2
            exit 1
        fi
        exit 0
        ;;
    -dvv)
        if [ "\$mode" = silent ]; then
            exit 0
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
# A stand-in `gh`, answering `gh attestation verify <artifact> --repo <repo>`:
#
#   accept   the subject verifies; gh's summary names the workflow and the tag
#            the bytes were built from, which is the whole point of asking.
#   reject   KNOWN-BAD. gh reached a verdict and the verdict is NO — the shape
#            it takes when a tarball's digest matches no attestation, i.e. the
#            bytes were changed after they were attested.
#   error    NOT a verdict: the API could not be reached. This must land on 75,
#            because an offline runner announcing a supply-chain failure is
#            both a lie and the reason nobody believes the next alarm.
#   crash    NOT a verdict: the tool dies on a signal, having said nothing.
#   forged   NOT a verdict: the tool exits 65 — the gate's own FAILED code —
#            with no verdict in its output. LESSON-442's collision, in the one
#            direction that matters: 65 must mean "these bytes are bad" and
#            must not be inheritable from a subprocess's status.
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
subject="\${3:-artifact}"

case "\$mode" in
    accept)
        echo "Loaded digest \$digest for file://\$subject"
        echo "Loaded 1 attestation from GitHub API"
        echo ""
        echo "Verification succeeded!"
        echo ""
        echo "\$digest was attested by:"
        echo "REPO                        PREDICATE_TYPE                  WORKFLOW"
        echo "atelier-fashion/teton-code  https://slsa.dev/provenance/v1  .github/workflows/release.yml@refs/tags/v1.2.3"
        exit 0
        ;;
    reject)
        echo "Loaded digest \$digest for file://\$subject"
        echo "Verification failed: no matching attestations found for subject \$digest" >&2
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

gh_accept="$(make_gh "$work/gh-accept" accept)"
gh_reject="$(make_gh "$work/gh-reject" reject)"
gh_error="$(make_gh "$work/gh-error" error)"
gh_crash="$(make_gh "$work/gh-crash" crash)"
gh_forged="$(make_gh "$work/gh-forged" forged)"

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
expect_output "  ... and says so as a tooling failure" "could not be run"

expect_exit 75 "no codesign on this machine -> 75 UNCHECKED" \
    env TETON_CODESIGN=/nonexistent/codesign bash "$VERIFY_SIGNATURE" "$sig_bin" "$SIG_TEAM_ID"
expect_output "  ... and says the gate belongs to the macOS legs" "is not on this machine"

expect_exit 64 "too few arguments -> 64" bash "$VERIFY_SIGNATURE" "$sig_bin"
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

# KNOWN-BAD: bytes that no attestation covers, because they are not the bytes
# that were built. This is the supply-chain alarm, and it must be reachable.
expect_exit 65 "a TAMPERED artifact gh rejects -> 65 FAILED" \
    env TETON_GH="$gh_reject" bash "$VERIFY_ATTESTATION" "$att_tampered" "$att_repo"
expect_output "  ... and says the bytes do not verify" "these bytes do not verify against"
expect_output "  ... and quotes gh's verdict" "no matching attestations found"

# Everything that is NOT a verdict. Each of these is a failing gh, and none of
# them may be reported as a supply-chain failure (LESSON-442).
expect_exit 75 "gh failing on the NETWORK -> 75 UNCHECKED, not 65" \
    env TETON_GH="$gh_error" bash "$VERIFY_ATTESTATION" "$tb_linux" "$att_repo"
expect_output "  ... and says why that is deliberate" \
    "must never be announced as a supply-chain alarm"

expect_exit 75 "gh dying on a signal, having said nothing -> 75 UNCHECKED" \
    env TETON_GH="$gh_crash" bash "$VERIFY_ATTESTATION" "$tb_linux" "$att_repo"
expect_output "  ... and says no verdict was reached" "without a verification verdict"

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
expect_exit 64 "a repo that is not owner/repo -> 64" \
    env TETON_GH="$gh_accept" bash "$VERIFY_ATTESTATION" "$tb_linux" "teton-code"

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
# Only the validation that happens BEFORE `cargo build` — this suite does not
# build. That is the whole of package.sh that can be tested without a toolchain,
# and it is the part that was previously absent.

group "package.sh (input validation, no build)"

expect_exit 64 "too few arguments -> 64" bash "$PACKAGE" aarch64-apple-darwin
expect_exit 64 "an unknown target triple -> 64, before cargo is invoked" \
    bash "$PACKAGE" x86_64-pc-windows-msvc 1.2.3 "$work/pkg"
expect_output "  ... and lists the targets it does build" "aarch64-apple-darwin"
expect_exit 64 "an empty version -> 64" bash "$PACKAGE" aarch64-apple-darwin "" "$work/pkg"
expect_exit 64 "a prerelease version -> 64" \
    bash "$PACKAGE" aarch64-apple-darwin 1.2.3-rc.1 "$work/pkg"
expect_exit 64 "a version that is really a path -> 64" \
    bash "$PACKAGE" aarch64-apple-darwin "../../etc" "$work/pkg"

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
