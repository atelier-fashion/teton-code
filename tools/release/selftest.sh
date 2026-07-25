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
#
# The smoke stand-ins are shell scripts named `teton` and `tetond`, tarred the
# way package.sh tars the real ones. They are not the product and prove nothing
# about it — they are the KNOWN-BAD and KNOWN-GOOD inputs that let this suite
# ask smoke.sh a question it can get wrong.
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
SITE_RENDER="$repo_root/site/render.sh"
SITE_TEMPLATE="$repo_root/site/index.html"
FORMULA_TEMPLATE="$repo_root/packaging/homebrew/teton.rb.tmpl"

for required in "$LIB" "$VERIFY" "$PACKAGE" "$RENDER_FORMULA" "$SMOKE" \
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

# --- syntax ----------------------------------------------------------------

group "syntax (bash -n)"
for s in "$LIB" "$VERIFY" "$PACKAGE" "$RENDER_FORMULA" "$SMOKE" \
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
            echo "daemon: running — tetond $reported (protocol 1)"
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
    cat >"$dir/tetond" <<EOF
#!/usr/bin/env bash
# selftest stand-in for the released tetond daemon. Not the product.
case "\${1:-}" in --version) echo "tetond $reported"; exit 0 ;; esac
if [ "\${TETON_TEST_SEAMS:-}" = "1" ] && [ "$refuses" = "yes" ]; then
    echo "tetond: TETON_TEST_SEAMS=1 is set, but this is a release build, which cannot honour them." >&2
    exit 70
fi
if [ "$handshakes" = "yes" ]; then
    mkdir -p "\$XDG_RUNTIME_DIR/teton"
    : >"\$XDG_RUNTIME_DIR/teton/handshake"
fi
exec sleep 600
EOF

    chmod +x "$dir/teton" "$dir/tetond"
    printf 'stand-in licence\n' >"$dir/LICENSE"
    printf 'stand-in readme\n' >"$dir/README.md"
}

# make_tarball <name> <reported-version> <refuses> <handshakes> -> path on stdout
make_tarball() {
    local name="$1" dir="$work/standin-$1"
    make_standins "$dir" "$2" "$3" "$4"
    tar -czf "$work/teton-v1.2.3-$name.tar.gz" -C "$dir" teton tetond LICENSE README.md
    printf '%s\n' "$work/teton-v1.2.3-$name.tar.gz"
}

tb_good="$(make_tarball good 1.2.3 yes yes)"
tb_wrong_version="$(make_tarball wrong-version 0.0.1 yes yes)"
tb_seams_honoured="$(make_tarball seams-honoured 1.2.3 no yes)"
tb_no_handshake="$(make_tarball no-handshake 1.2.3 yes no)"

# A tarball that is missing a binary entirely — the UNCHECKED path.
make_standins "$work/standin-truncated" 1.2.3 yes yes
rm -f "$work/standin-truncated/tetond"
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

expect_exit 65 "a tetond that HONOURS TETON_TEST_SEAMS=1 -> 65 FAILED (BR-9)" \
    bash "$SMOKE" "$tb_seams_honoured" 1.2.3
expect_output "  ... and says the daemon did not refuse" "did not refuse as a release build should"

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
expect_exit 75 "a tarball missing tetond -> 75 UNCHECKED" \
    bash "$SMOKE" "$work/teton-v1.2.3-truncated.tar.gz" 1.2.3

expect_exit 64 "a nonsense deadline override -> 64" \
    env TETON_SMOKE_SEAM_DEADLINE_SECS=soon bash "$SMOKE" "$tb_good" 1.2.3

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
# Independently known: sha256("teton\n").
assert "sha256_of prints the file's sha256" \
    [ "$(sha256_of "$work/hashme")" = "$(shasum -a 256 "$work/hashme" | awk '{print $1}')" ]
# The reason lib.sh exists. package.sh's old copy ran `openssl` unconditionally
# here, so a machine with none of the three tools aborted the build with 127
# under `set -e`. `expect_exit 1`, not `refute`, precisely because 127 is also a
# failure and would satisfy a mere "it did not succeed".
#
# PATH is emptied INSIDE the child rather than via `env PATH=... bash`, which
# cannot find bash to run and fails before testing anything.
expect_exit 1 "sha256_of returns 1, not 127, when no sha256 tool exists" \
    bash -c "PATH=/nonexistent; . '$LIB'; sha256_of '$work/hashme'"

# --- summary ---------------------------------------------------------------

total=$((passed + failed))
printf '\n%s\n' "selftest: $passed/$total passed, $failed failed."

if [ "$failed" -ne 0 ]; then
    exit "$EXIT_FAILED"
fi
