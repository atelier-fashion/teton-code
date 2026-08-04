# Release runbook — cutting `vX.Y.Z`

Pushing a `vX.Y.Z` tag is the entire release procedure. Everything downstream —
three platform builds, the smoke gates, the GitHub Release, the tap formula
bump, and a live `brew install` — is
[`.github/workflows/release.yml`](../.github/workflows/release.yml), in one run,
with no manual step in the middle.

This file is what a human does around that: what has to exist before the first
tag, how to bump and tag, what to check afterwards, and what each failure mode
means.

Two adjacent documents own their own setup; this one points at them rather than
repeating them, because a second copy of a credential procedure is a second copy
to go stale:

- [`docs/homebrew-tap-setup.md`](homebrew-tap-setup.md) — the
  `atelier-fashion/homebrew-tap` repository and the `HOMEBREW_TAP_TOKEN` secret.
- [`docs/site-deploy-runbook.md`](site-deploy-runbook.md) — `tetoncode.ai`,
  which this workflow **dispatches** after the tap bump succeeds. Live since
  v0.1.5 (OQ-5 resolved 2026-08-01); a red `Deploy site` run is now a finding,
  not the expected state.

**The invariant this whole pipeline defends:** a green release run means the tap
points at the release, the release's artifacts hash to the checksums published
beside them, and every one of those artifacts was executed before it shipped. A
release that is published but not in the tap is a release that hands `brew
install` the *previous* version — so the run goes red rather than yellow (BR-4,
LESSON-447).

---

## 1. Before the first tag — one-time

| # | Thing | State | Where | Blocks |
|---|---|---|---|---|
| 1 | `atelier-fashion/homebrew-tap` exists, public, `main` | **done** 2026-07-25 | [homebrew-tap-setup.md §1](homebrew-tap-setup.md) | `bump-formula` push |
| 2 | **`tap-publish` environment** carries `HOMEBREW_TAP_TOKEN` (the environment copy; org-approved if required, repo-level copy retires per §11). Deployment rules: tags `v*.*.*` **+** branch `main` | **done** 2026-07-31 (`main` added 2026-08-01) | [homebrew-tap-setup.md §2](homebrew-tap-setup.md) | `bump-formula` (exits `75` without the token; the job never starts at all on a ref the rules refuse) |
| 3 | **`release-signing` environment** carries secrets `MACOS_CERT_P12`, `MACOS_CERT_PASSWORD` and variable `APPLE_TEAM_ID`. Deployment rules: tags `v*.*.*` **+** branch `main` | **done** 2026-07-31 (`main` added 2026-08-01) | §10 below | the whole `build` matrix — it declares `environment: release-signing`, so nothing builds on a ref the rules refuse |
| 4 | A dry run has gone green end to end | **outstanding** | §2 below | everything — do not skip |
| 5 | GCP secrets for the site | **done** 2026-08-01 (OQ-5 resolved; first green deploy at v0.1.5) | [site-deploy-runbook.md §2](site-deploy-runbook.md) | `tetoncode.ai` only; a release is fine without them |

Rows 2 and 3 are the two things a first-time operator otherwise discovers the
hard way — as a `75` from a job that could not read a secret, or as a job that
never started because the environment refused the ref. Neither piece of state
lives in this repository: environments, their secrets, their variables and their
deployment rules are all GitHub-side settings, which is why they are written
down here and re-recorded per §11 item 5.

The tap is intentionally empty — no `Formula/teton.rb` until the first bump
writes one, and no `Formula/` directory either; `bump-formula` creates it. An
empty tap is the correct pre-first-release state, not a missing step.

Items 2, 3 and 5 were somebody's *access*, not somebody's afternoon: the tap
token may need an organisation owner's approval, the signing secrets need the
Apple Developer account holder (§10), and the site's GCP coordinates needed
somebody with the Atelier GCP org. All three are in place; if any has to be
re-established, start it before you want to release, not during.

---

## 2. The dry run — do this before your first tag

`workflow_dispatch` exists so the pipeline is testable without spending a tag.
Actions → **Release** → *Run workflow*, from `main`, leaving **`dry_run`
checked** (its default).

**Dispatch from `main`, and only from `main`.** `build` declares
`environment: release-signing` and `bump-formula` declares
`environment: tap-publish`, and a job whose environment refuses the ref does not
start — it is blocked before its first step, not failed inside one. Both
environments' deployment rules therefore admit the branch `main` alongside the
tag pattern `v*.*.*`. That is a deliberate BR-4 scoping decision, not a
loosening: the rule as written in the REQ is "`v*.*.*` tags **and/or** `main`",
and a tags-only rule makes the dry run in this section impossible — the only way
to exercise the pipeline would be to spend a tag, which is the exact cost this
section exists to avoid. Any other ref is still refused, and that refusal is
what §11 item 4 records as the AC-4 negative probe.

It runs the version gate, builds and smokes all three targets, assembles
`checksums.txt`, renders the release notes, and renders + `brew style` +
`brew audit` + BR-3-version-checks a real formula against a scratch tap — then
publishes nothing. `verify-install` is skipped, with the reason printed beside
it: there is no published release to install from.

What a dry run proves, precisely:

- The `x86_64-apple-darwin` cross leg compiles and its binaries execute
  (under Rosetta 2 on the arm64 runner, which that leg installs for itself with
  `softwareupdate --install-rosetta` rather than assuming the image has it —
  ADR-548-2). **Until a dry run goes green, that leg is designed but unproven**
  — it is the one most likely to fail first, and finding out from a tag means
  retagging.
- The rendered formula passes the audit gate that would otherwise fail *after*
  a release is already published.

What it cannot prove: anything about the published URLs, the tap push, or
`brew install` — all of which need real bytes at a real release.

A dispatch from a branch can never publish, whatever `dry_run` says: publishing
requires a tag ref, because `gh release create` would otherwise mint a tag as a
side effect and a release would appear from nowhere.

---

## 3. Cutting the release

```sh
# 1. Bump the single version. Everything else derives from it (BR-3).
$EDITOR Cargo.toml           # [workspace.package] version = "X.Y.Z"

# 2. Cargo.lock carries the workspace crates' versions too — refresh and commit
#    it, or CI fails on a dirty lockfile after the first cargo invocation.
cargo check --workspace

# 3. Confirm the gate the workflow will run, before spending a tag.
bash tools/release/verify-version.sh --print-version    # prints X.Y.Z
bash tools/release/verify-version.sh vX.Y.Z             # exit 0 == MATCH

git switch -c release/vX.Y.Z
git add Cargo.toml Cargo.lock
git commit -m 'chore(release): vX.Y.Z'
```

4. Open the PR, get CI green, merge to `main`.

5. Tag **the merge commit on `main`**, annotated, and push the tag on its own:

```sh
git switch main && git pull
bash tools/release/verify-version.sh vX.Y.Z    # again, on the commit being tagged
git tag -a vX.Y.Z -m "teton X.Y.Z"
git push origin vX.Y.Z
```

6. Watch the run: `gh run watch` (or Actions → Release). Do not walk away from
   the first few releases — see §7.

The tag is what the release names, so tag the commit you want released. If you
tag the wrong commit, delete the tag on the remote *before* the run reaches the
`release` job; after publish, cut a new patch version instead of moving a tag
that users' `brew install` may already have resolved.

---

## 4. What the run does

| Job | Runner | Does | Fails the release when |
|---|---|---|---|
| `preflight` | ubuntu | Compares the tag against `[workspace.package] version` (BR-3) and decides `publish` | `64` the versions disagree; any other non-zero, the check could not run — either way nothing is built |
| `build` (×3) | macos-15 ×2, ubuntu | Builds with `--features tetond/llama`, packages a flat tarball, then smokes the **unpacked tarball**: both binaries report the version, the release build refuses `TETON_TEST_SEAMS=1` (BR-9), `teton doctor` handshakes a live `tetond` | `65` an assertion failed; `75` the smoke could not run. `fail-fast: false` — you learn about all three targets, not the first |
| `release` | ubuntu | Recomputes `checksums.txt` from the uploaded artifacts (BR-5), renders notes, `gh release create --verify-tag` | fewer than 3 tarballs (`65`); a dry run stops here and prints what it would have published |
| `bump-formula` | macos-15 | Re-downloads the published assets and re-checks them against the published `checksums.txt`; renders `Formula/teton.rb`; asserts Homebrew resolves it at the tag; `brew style` + `brew audit`; fetches all three URLs; then — **before the push** — installs the rendered formula, runs `brew test` (AC-5) and `brew services start` → `doctor` → `restart` → `stop` (AC-6); only then commits and pushes the tap | any of the above. Nothing here is `continue-on-error` — BR-4. The install/service gates run pre-push deliberately: a broken `service` block fails with the tap still pointing at the previous good release, and its messages say so ("The tap was NOT updated") |
| `verify-install` | macos-15 | Post-push **reachability** evidence only: `brew install atelier-fashion/tap/teton` on a runner that has never seen the tap, `brew test`, version check — proving the one-command auto-tap path works against the *live* tap (BR-1). The functional service gates already ran in `bump-formula` | any step. Covers **macOS arm64 only** — a green run here is not evidence about Intel or Linux (LESSON-433) |

After `bump-formula` is green, the run **dispatches**
[`deploy-site.yml`](../.github/workflows/deploy-site.yml) to republish
`tetoncode.ai` at the new version. That is a separate workflow run with its own
entry in the Actions list, not a job in this table — see §5 for why it is a
dispatch and why it comes after the bump rather than beside it.

Exit codes across `tools/release/` are a taxonomy, not a boolean, so "it failed"
and "it could not run" can never be confused (LESSON-442, ADR-548-4):

| Code | Meaning |
|---|---|
| `0` | passed |
| `64` | the check RAN and the inputs are wrong (version mismatch, bad invocation, an unfilled `{{PLACEHOLDER}}`) |
| `65` | it RAN and FAILED — these bytes are bad |
| `75` | it could NOT run. Nothing was learned, which is not a pass |
| `70` | (`package.sh` only) the build reported success but an expected binary is missing — or, since REQ-551, a pack-phase contract failure (staging that is absent, short a member, version-skewed, altered since the build, or holding a symlink/directory where a regular file belongs; signing or verify rejection); §9 lists them |

---

## 5. Verify after the release

Everything below is checkable from a laptop. CI already proved the macOS arm64
path — `bump-formula` ran the install, `brew test` and the services handshake
before it pushed the tap, and `verify-install` then installed from the live tap
— so these are the checks that CI structurally cannot make about itself.

```sh
TAG=vX.Y.Z

# 1. The Release carries three tarballs and checksums.txt.
gh release view "$TAG" --json assets --jq '.assets[].name'

# 2. The published bytes hash to the published checksums — from a fresh fetch.
mkdir -p /tmp/rel && gh release download "$TAG" --dir /tmp/rel --clobber
(cd /tmp/rel && shasum -a 256 -c checksums.txt)

# 3. The published bytes carry GitHub build provenance: this workflow, at this
#    commit, in this repository, built exactly them. The checksums above prove
#    the assets match a list published beside them; this proves who made them —
#    including the list itself, which is attested too, so step 2 stops being a
#    page that only agrees with itself. Every published asset, not just the
#    tarballs (AC-2).
for a in /tmp/rel/*.tar.gz /tmp/rel/checksums.txt; do
  gh attestation verify "$a" --repo atelier-fashion/teton-code
done

# 4. The tap resolves the formula AT THE TAG. The formula carries no `version`
#    stanza on purpose (brew audit rejects one that agrees with the URL scan),
#    so this is Homebrew's own reading of the rendered URLs — the same assertion
#    the bump job makes before it pushes (BR-3).
brew info --json=v2 --formula atelier-fashion/tap/teton \
  | jq -r '.formulae[0].versions.stable'        # == X.Y.Z

# 5. The tap's history shows the bump, and only the bump.
gh api repos/atelier-fashion/homebrew-tap/commits --jq '.[0].commit.message'
```

Step 3 is the command the README hands users, run over every asset at once
instead of one named file. The CI gate makes the same call against the same set
of assets, and additionally pins

```sh
--signer-workflow atelier-fashion/teton-code/.github/workflows/release.yml
```

— a constraint no user can be expected to type, and the one that stops some
*other* workflow in this repository from being an acceptable signer. It also
cross-checks each asset's sha256 against the `checksums.txt` that same run
computed, so "attested" and "hashes to the published list" are asserted about
one set of bytes rather than two.

**That value is fully qualified, and it has to be.** `gh` matches it against
the certificate's SAN, which is the whole
`https://github.com/<owner>/<repo>/<path>@<ref>` URI — the bare
`.github/workflows/release.yml` matches no certificate at all. It does not fail
as a rejection; it comes back as `Error: verifying with issuer "sigstore.dev"`,
which the gate correctly scores `75` UNCHECKED, on every asset, on every
release. If the gate ever starts reporting UNCHECKED for everything with that
error text, suspect this argument before suspecting GitHub.

**Before this repository's first attested release, step 3 exits non-zero with an
HTTP 404** — GitHub has no attestations to serve. That is the correct answer to
"what attests these bytes?" when the answer is "nothing yet", and it is why the
gate classifies it as `75` UNCHECKED rather than `65` (§9). Expect it until the
first signed release ships; after that, a 404 is a finding.

Then, on a real machine (not CI):

```sh
brew install atelier-fashion/tap/teton
teton --version                 # names the tag
brew services start teton
teton doctor                    # daemon: running, and the version matches
```

`brew services start teton` names the formula the short way, which Homebrew 6
refuses for an untrusted tap; `brew trust atelier-fashion/tap` once if it does.
The fully-qualified install above never needs it — see
[homebrew-tap-setup.md §4](homebrew-tap-setup.md).

The macOS signature is the other half of step 3's question — provenance says who
built these bytes, the signature says who vouches for them on the machine that
runs them. It has a section of its own (§10) because certificate renewal has a
procedure of its own. AC-6, the Keychain-grant survival that signature buys, is
**staged**: it needs two consecutively signed releases before it can be run at
all, and is recorded as unrun until the second one (§10).

Finally, the site. Once `bump-formula` has pushed the tap, this workflow
**dispatches** [`deploy-site.yml`](../.github/workflows/deploy-site.yml) — an
explicit `workflow_dispatch`, not a `release: published` subscription. It has to
be: `gh release create` runs under the default `GITHUB_TOKEN`, and GitHub does
not raise workflow-triggering events for that token's actions, so a subscribed
workflow would sit silent after every release while looking perfectly healthy.

The ordering is deliberate and is an invariant (ADR-548-3a): the page advertises
a version *and* the `brew install` that fetches it, and both are only true once
the tap points at the new release. A site deploy that ran beside the bump would
publish a page naming vX.Y.Z next to an install command still handing out
vX.Y.Z-1. So if `bump-formula` fails, the site is not deployed — correctly: the
page already up matches the version the tap still serves.

That dispatched run renders the page, uploads it as the `site-dist` artifact,
and publishes it. It has deployed for real since **v0.1.5** (OQ-5 resolved
2026-08-01) — so a red `Deploy site` run is now a finding to diagnose, not the
documented expectation it was through v0.1.4. Check the page yourself:

```sh
curl -s https://tetoncode.ai | grep -o 'v[0-9]\+\.[0-9]\+\.[0-9]\+' | head -1
```

The `Deploy result` step still fails on purpose when the GCP configuration is
missing, rather than skipping or going green having published nothing — see
[site-deploy-runbook.md](site-deploy-runbook.md). That guard is what makes the
red-is-a-finding reading true.

---

## 6. The upgrade path (AC-4)

```sh
brew upgrade teton
brew services restart teton
teton doctor        # confirm the RUNNING daemon reports the new version
```

The restart is the part worth writing down. `brew upgrade` replaces the binaries
on disk; a `teton-code` that is already running is still the old binary until
something restarts it, and every symptom of that is confusing (a CLI on the new
version talking to a daemon on the old one). Do not rely on the upgrade to
restart the service for you — run the restart, and confirm with `doctor`, which
prints the version of the daemon that actually answered.

AC-4 is **staged at v0.1.0**: with no release N-1 there is nothing to upgrade
from, so the path is documented but unexercised. The first `v0.1.x` release is
where it gets exercised for real — run the three commands above from the v0.1.0
install and record the result in that release's sign-off.

---

## 7. First release (`v0.1.0`) — the extras

1. **Dry run first** (§2). The `x86_64-apple-darwin` cross leg has never run.
2. **The tap must exist and the token must be set** before the tag, not after.
   The failure ordering is unkind if they are not: `release` publishes, then
   `bump-formula` exits `75` with *"the release is published but the formula
   cannot be pushed"*. The release is real and the tap is empty until you add
   the secret and re-run the job — which is safe and idempotent, and is the
   recovery, not a redo of the release.
3. **`verify-install` is the first time anything installs from the live tap.**
   It runs `brew install atelier-fashion/tap/teton` as its very first step,
   before it teaches Homebrew anything about the tap — that ordering *is* the
   BR-1 evidence. (`bump-formula` also installs, but from its own local clone,
   which cannot exercise the auto-tap fetch — that is exactly why both exist.)
4. ~~**The site will not deploy** (OQ-5). Expected; see §5.~~ **No longer
   true** — OQ-5 was resolved 2026-08-01 and the site has deployed for real
   since v0.1.5. Kept struck through rather than deleted because this section
   is the record of what v0.1.0 actually looked like: at that release the
   dispatched `Deploy site` run did go red at its last step, by design. If you
   see that today, diagnose it (§5) instead of expecting it.
5. **AC-4 is staged**, not met (§6).
6. **AC-1/AC-2 human sign-off** (§8) — CI covers macOS arm64. The other two
   platforms are unrun until a human runs them, and are recorded as unrun.
7. **The release tooling is already under standing CI.** `ci.yml`'s `tooling`
   job runs on every pull request: `actionlint` (shellcheck-backed) over the
   workflows, `shellcheck` over `tools/release/*.sh` and `site/render.sh`, and
   `tools/release/selftest.sh`, which exercises each release script's success
   **and** failure paths with no network and no cargo. So a syntax error or a
   broken exit code in this pipeline is caught on the PR that introduces it,
   not by a tag. What that job cannot cover is everything that needs real
   published bytes — the tarball URLs, the tap push, `brew install` — which is
   exactly what the dry run in §2 and this section exist for. Green `tooling`
   is not a substitute for item 1.

---

## 8. AC-1 / AC-2 sign-off

> AC-1: on a clean machine with only Homebrew installed,
> `brew install atelier-fashion/tap/teton` → `brew services start teton` →
> `teton` reaches the first-run model proposal (names the pick, download size,
> RAM floor).
>
> AC-2: the same sequence on macOS x86_64 and Linux x86_64, **each verified on
> its own platform** — a green arm64 run is not evidence for the others
> (LESSON-433). Unrun legs are recorded as unrun, not assumed.

Copy all three blocks into the release's notes or a PR comment **with the
defaults as written**, and edit only the lines you actually ran. `unrun` is the
correct, publishable answer for a platform nobody had hardware for; a blank line
is not, and a copied `pass` from another platform is a false claim.

"Clean machine" means one with no recorded model decision and no downloaded
weights — otherwise the first-run proposal, which is the entire point of AC-1,
is not raised and the leg proves nothing. The daemon's state directory is not a
single fixed path (`XDG_RUNTIME_DIR`, then macOS Application Support, then the
temp dir); [manual-verification.md §0](manual-verification.md) resolves it and
names exactly what to clear.

```
AC-1 / AC-2 sign-off — teton vX.Y.Z
-----------------------------------
Platform          :  macOS 15+, Apple Silicon (aarch64-apple-darwin)
Status            :  unrun            ( unrun | pass | fail )
Verified by       :
Date              :
Machine           :                   (chip, RAM, OS build; "clean" = no prior teton state)
brew install      :  unrun            (one command, no tap step, no toolchain)
brew services start: unrun            (launchd took the service)
teton doctor      :  unrun            (daemon: running; version == vX.Y.Z)
First-run proposal:  unrun            (names model, download size, RAM floor)
Model accepted + loaded : unrun       (optional beyond AC-1; note if run)
Notes / findings  :
```

```
AC-1 / AC-2 sign-off — teton vX.Y.Z
-----------------------------------
Platform          :  macOS 13+, Intel (x86_64-apple-darwin)
Status            :  unrun            ( unrun | pass | fail )
Verified by       :
Date              :
Machine           :
brew install      :  unrun
brew services start: unrun
teton doctor      :  unrun
First-run proposal:  unrun
Notes / findings  :  CI evidence for this target is Rosetta-only (ADR-548-2) —
                     the binary loads and runs, which is not native-hardware
                     evidence. This block is the native evidence.
```

```
AC-1 / AC-2 sign-off — teton vX.Y.Z
-----------------------------------
Platform          :  Linux x86_64, glibc (x86_64-unknown-linux-gnu)
Status            :  unrun            ( unrun | pass | fail )
Verified by       :
Date              :
Machine           :                   (distro, glibc version, RAM)
brew install      :  unrun            (Homebrew on Linux)
Daemon started    :  unrun            (run `teton-code` yourself — `brew services`
                                       is NOT a v1 claim on Linux, OQ-2)
teton doctor      :  unrun
First-run proposal:  unrun            (CPU-only inference on this platform)
Notes / findings  :
```

The same posture as [docs/manual-verification.md](manual-verification.md), for
the same reason: sign-offs are per platform and per release, and the value of
the record is that its gaps are visible.

---

## 9. When it fails

**`Version MISMATCH (BR-3)` (`preflight`, `64`)** — the tag and
`[workspace.package] version` name different versions. Nothing was built. Delete
the tag on the remote, fix the manifest (§3 step 1–3), retag. Do not "fix" it by
editing the formula or the release title.

**`Version gate UNCHECKED`** — the gate could not read a version at all. This is
not evidence that the versions agree, which is why it stops the release.

**`Smoke FAILED (<target>)` (`65`)** — a shipped-artifact assertion failed for
that platform's tarball. The bytes are bad; the log names which assertion. Do
not retry hoping for green. `fail-fast: false` means the other two legs still
report, so read all three before diagnosing.

**`Rosetta 2 unavailable` (`75`)** — the arm64 macOS runner could not execute
the x86_64 binaries, so the Intel artifact was not exercised. Failing beats
shipping an unsmoked binary.

Note what this does **not** mean any more. The x86_64 leg now runs
`softwareupdate --install-rosetta --agree-to-license` before its smoke, so an
image that merely ships without Rosetta preinstalled is handled, not fatal — the
workflow installs it. Reaching this failure therefore means the *install* did
not take: GitHub's image refused it, the runner lost network mid-install, or
Apple stopped offering Rosetta for that macOS version. The first two are
re-runnable infrastructure faults. Only the third puts ADR-548-2's whole
cross-compile-and-Rosetta-smoke approach in question, and it would be visible in
the `softwareupdate` output rather than inferred from this exit code. Read that
step's log before concluding anything about the ADR.

**`Missing release artifacts` (`65`)** — fewer than three tarballs reached the
`release` job. A release that silently omits a platform is worse than no
release. Find the build leg that did not upload.

**`Attestation FAILED` (`65`) or `Provenance UNCHECKED` (`75`) — after the
release is published (`release` job)** — this gate runs *after*
`gh release create`, deliberately: it verifies the assets the release actually
serves, not the local copies. So when it goes red the release exists, and
`bump-formula` never ran because it `needs: release` — the tap still points at
the previous version and `brew install` keeps handing out the previous release.
Nothing unverified reached a user; the state is *published but not advertised*,
which is the invariant working, not breaking.

**Residual, stated plainly:** this gate proves the assets were good *at the
moment it ran*, and `bump-formula` downloads them again afterwards to compute
the formula's hashes — so an asset replaced in the window between the two is
not caught here at all, and is caught only by `verify-install`, which runs
after the tap has already moved.

The two exit codes are different findings and take different actions:

- **`75` — nothing was learned.** No `gh`, no `attestations: read`, an API rate
  limit, a network fault, or simply **no attestations exist yet** (HTTP 404
  before this repository's first attested release — §5). Diagnose which, then
  re-run the failed jobs.
- **`65` — `gh` reached a verdict and rejected the bytes.** The artifact served
  for this tag is not the artifact this run attested. Treat it as a
  supply-chain event: do **not** re-run past it, do not bump the tap by hand,
  and do not delete-and-retag until you know why the served bytes differ from
  the attested ones. A re-run that goes green after an unexplained `65` has told
  you nothing except that it is intermittent, which is the worst possible thing
  for it to be.

Re-running is safe: the publish is idempotent — `gh release create` is guarded
on the release already existing, and the asset uploads use `--clobber` — so a
re-run after a diagnosed `75` re-uploads the same bytes and re-verifies rather
than dying on "release already exists".

**`HOMEBREW_TAP_TOKEN is not set` (`75`)** — see §7 item 2 and
[homebrew-tap-setup.md §2](homebrew-tap-setup.md). Add the secret to the
`tap-publish` environment, then **re-run the failed job** (Actions → the run →
*Re-run failed jobs*) rather than re-dispatching: a fresh dispatch rebuilds all
three targets to arrive at the same place. The bump is idempotent — it exits
green with *"formula already current"* if the tap already matches.

**`bump-formula` (or `build`) never started, no logs** — not a failure inside a
job; the environment's deployment rules refused the ref. `build` declares
`environment: release-signing` and `bump-formula` declares
`environment: tap-publish`, and both admit only `v*.*.*` tags and `main` (§1
rows 2–3, §2). The run page reports the deployment as blocked by protection
rules. Dispatch from `main` or push a tag — and if a ref that *should* be
admitted is refused, the rules have drifted; re-check and re-record them per
§11 item 5.

**`Published assets do not match published checksums (BR-5)` (`65`)** — the
tarballs downloaded from the release hash differently than the `checksums.txt`
published beside them. No formula was written. This should be impossible; treat
it as a supply-chain event, not a flake, and do not push a formula by hand.

**`Formula version does not equal the tag (BR-3)` (`65`)** — Homebrew's URL
version scan changed shape. The template needs an explicit `version` stanza
again *and* `brew audit`'s redundancy check needs an exception; both live in
`packaging/homebrew/teton.rb.tmpl`, never in the tap.

**`Rendered formula failed brew style/audit` (`65`)** — fix
`packaging/homebrew/teton.rb.tmpl` in this repo. The tap copy is generated;
editing it there is undone by the next release and leaves no record of why
(ADR-548-1).

**`Tap push FAILED (BR-4)` (`65`)** — the release is published and the tap still
points at the previous version, so `brew install` hands users the old release.
Check the token's `contents: write` on `homebrew-tap`, then re-run the job.
Highest-urgency failure in this list: it is the one that is wrong on users'
machines rather than in a log.

**`brew services start FAILED` / daemon never answered (`bump-formula`,
`65`)** — the formula's `service` block or the daemon's startup is broken on a
clean machine. The release is published at this point but **the tap was not
updated**, so no user can install the broken formula: `brew install` still
serves the previous release. Fix forward with a patch release; the failure
log's `brew services list` + daemon logs are the
starting point.

---

## 10. Signing identity (BR-1)

Both macOS binaries in both macOS tarballs are signed with an Apple **Developer
ID Application** certificate belonging to team `545BU9G9D6`. Linux binaries are
unsigned in v1, and the release notes say so rather than leaving it to be
inferred (BR-6). Nothing here is notarized: that is a separate Apple service and
a separate claim, and this pipeline does not make it.

**The contract, stated the way a user experiences it:** a Keychain grant given
to release N survives the upgrade to release N+1 with no fresh consent prompt.
That is why the identity has to be *stable*, not merely *present* — an unsigned
release, or one signed by a different team, is a new identity to macOS and every
grant is asked for again.

**The keychain window — the signing step, and nothing else.** The certificate is
imported into a throwaway keychain inside the `build` job, and that keychain is
destroyed in the step immediately after signing, so no key material survives the
run. REQ-551 moved the import to *after* every line of third-party code has
finished compiling; its verify pass then added the early destroy. Together they
close the accepted risk REQ-550 recorded, and the resulting claim is literal
rather than approximate:

- the identity exists from the import step's `list-keychains` call through the
  `Sign and package` step — seconds of first-party code, all of it in this
  repository — and is gone at `Destroy the signing keychain (early)`;
- the ~30 minutes of `cargo build` and llama.cpp cmake run **before** the
  import, with no signing credential anywhere on the machine;
- the tarball **smoke and the artifact upload run after the destroy**, with no
  keychain present. They used to sit inside the window; they no longer do.

The `if: always()` cleanup at the end of the job stays, and is now a
failure-path backstop: it covers an import that died halfway, a cancelled job,
or a signing step that failed before the early destroy could run. On a green leg
it finds nothing and says so.

Moving the import *below* the build also moved it **downstream of untrusted
code**, so the three steps that touch the identity (import, sign, early destroy)
each open with the same preamble: `BASH_ENV`/`ENV`/`CDPATH` unset, the shell
functions that could shadow their tools removed, `PATH` set to the system
directories, and every `security` call spelled `/usr/bin/security`. That closes
the **in-band** channels — a build script can append to `$GITHUB_ENV` and
`$GITHUB_PATH`, and the runner applies both *between* steps. It does not close a
**persistent background process** started during the build and still running
while the key is on the machine: that is runner-level compromise, it is outside
this REQ's threat model, and the recorded fix is OQ-1 — splitting build and sign
into separate *jobs* — kept as future hardening rather than pretended away.

What REQ-551 does *not* claim: the build is still untrusted, and it still
determines the bytes the identity vouches for. Narrowing the key's reach is a
different axis from trusting the compile — the control on that axis is the
build-provenance attestation (§5), not this window. `package.sh`'s
`.stage-meta` manifest is on the same footing: it detects a stale, partial,
version-skewed or accidentally altered staging directory, which is what actually
goes wrong, but it is **unauthenticated** — anything that can swap a staged
binary can rewrite the digest line beside it — so it is an accident detector,
not a second attestation.

### Checking a signed release by hand

Continuing from §5 — the tarballs are already in `/tmp/rel`:

```sh
TAG=vX.Y.Z
mkdir -p /tmp/rel-check
tar -xzf "/tmp/rel/teton-$TAG-aarch64-apple-darwin.tar.gz" -C /tmp/rel-check

# Both binaries, both questions. (1) the signature is structurally valid and
# covers the bytes as shipped; (2) it names the authority and the team it
# should — which (1) does not test, because an ad-hoc signature passes (1)
# while naming nobody at all.
for b in teton teton-code; do
  codesign --verify --strict "/tmp/rel-check/$b" &&
    codesign -dvv "/tmp/rel-check/$b" 2>&1 |
      grep -E 'Developer ID Application|TeamIdentifier'
done
```

Per binary, that prints:

```
Authority=Developer ID Application: Atelier Fashion LLC (545BU9G9D6)
TeamIdentifier=545BU9G9D6
```

This is the block the README hands users, spelled against the extracted paths.
Drop the `grep` to read the whole record — `-dvv` also prints
`Identifier=teton`, the rest of the chain (`Developer ID Certification
Authority`, `Apple Root CA`) and the signing time; the two lines above are the
claim, the rest is context.

`-dvv`, and the second `v` is load-bearing: at verbosity 1 codesign prints
`TeamIdentifier=` but no `Authority=` lines at all, so `-dv` on a perfectly
signed binary shows no authority and reads as a failure. These are the same two
questions `tools/release/verify-signature.sh` asks of every macOS tarball in the
release smoke — this is the laptop-side spelling of the gate, not a second
opinion.

Repeat for `x86_64-apple-darwin`. A green check on the arm64 tarball is not
evidence about the Intel one (LESSON-433) — and a green check on `teton` is not
evidence about `teton-code`, which is why the loop above checks both rather than
sampling one.

### What stays constant — and why renewal is not a user-visible event

Two things, and the certificate is neither of them:

- the team id, `545BU9G9D6`;
- the binary identifiers, `teton` and `teton-code` — codesign derives them from
  the file names, so renaming a shipped binary is a signing change even when
  nothing about the signing changed (LESSON-457).

The designated requirement anchors on exactly those, plus Apple's own anchor.
Read it off a shipped binary rather than taking it on trust:

```sh
codesign -d --requirements - /tmp/rel-check/teton
```

It names the identifier and the team OU. There is no serial number in it and no
expiry date. So **renewing the Developer ID Application certificate does not
re-prompt users**: the new leaf signs the same identifier for the same team, and
macOS matches the requirement, not the particular certificate that satisfied it
last time. Changing the team, renaming a binary, or shipping unsigned is what
breaks a grant — and none of those is something a renewal does.

### When the certificate is expired or absent (BR-2)

The release fails, loudly, at the first step that needs the identity. That is
the design, not an accident: there is no branch anywhere in this pipeline that
notices a missing certificate and carries on unsigned. The "should we sign?"
predicate is the target triple — macOS release leg ⇒ sign or die — never the
certificate's availability, because a guard that switches itself off when its
input is missing is not a guard (LESSON-443).

Where it surfaces, in the order a run reaches them. Note that since REQ-551 the
compile happens first, so a `package.sh` exit `70` from the **build** phase —
cargo reported success but a binary is missing from `target/<triple>/release/` —
is reached *before* the import step and has nothing to do with signing at all.
The list below starts where the identity does:

- **the `build` job's "Import the Developer ID signing identity" step** —
  `Signing identity UNAVAILABLE` (the secrets resolved empty — check the
  `release-signing` environment's deployment rules against the ref; they admit
  `v*.*.*` tags and `main`, §1 row 3), `Signing
  certificate UNREADABLE` (`MACOS_CERT_P12` is not decodable base64), `Signing
  certificate would not import` (usually a wrong `MACOS_CERT_PASSWORD`, or a
  `.p12` exported without its private key), or `No Developer ID identity for
  team 545BU9G9D6` (it imported, but it is the wrong *kind* of certificate).
  Since REQ-551 this step runs *after* the compile, and its annotations say so:
  they read *"The build already finished — this fails the leg before anything is
  signed, and the binaries it produced are discarded with the job."* The leg did
  build; what did not happen is any signing, so no tarball is written and
  nothing from that leg can ship.
- **`package.sh`, exit `70`** — signing was requested and codesign could not
  carry it out, or it signed and then its own `--verify --strict` rejected the
  result. No tarball is written: a signing-requested build never ships unsigned.
  Recovery does not need a rebuild, and that is now literally true rather than
  nearly true: `pack` signs **copies** of the staged binaries in a scratch
  directory and never writes into `dist/stage-<target>/` at all, so *any* pack
  failure leaves the stage exactly as the build left it — four members whose
  digests still match `.stage-meta`. `tools/release/package.sh <target>
  <version> dist pack` then re-runs *only* the signing phase (the phase
  arguments exist since REQ-551). Before the scratch copy, the sharpest case did
  not recover: `codesign --sign` rewrites the file it signs, so a `--sign` that
  succeeded followed by a `--verify` that rejected left modified bytes in the
  stage, and the retry was refused by the manifest check as a *tampered* stage —
  true about the bytes, wrong about what happened. In CI, *Re-run failed jobs*
  recompiles, which is the honest cost of a fresh runner; on a machine where you
  are reproducing the failure by hand, re-`pack` and skip the 30-minute build.
- **the macOS smoke's signature gate** — `65`, these bytes are not signed the
  way a release must be; `75`, the gate could not run and nothing was learned.
  Both stop the release.

The fix:

1. Renew the **Developer ID Application** certificate for team `545BU9G9D6` in
   the Apple developer portal. A Mac Development or Apple Distribution
   certificate is not what a released binary is signed with, and the import step
   checks which one it got.
2. Export it from Keychain Access as a `.p12` **with its private key**, then
   base64 it: `base64 -i cert.p12 | pbcopy`.
3. Update `MACOS_CERT_P12` (the base64) and `MACOS_CERT_PASSWORD` on the
   **`release-signing` environment** — Settings → Environments → `release-signing`
   → *Environment secrets*. Not repository secrets: a signing credential any
   workflow on any branch can read is exactly what the environment exists to
   prevent (BR-4, and §11).
4. Re-run the failed jobs.

Never bypass the gate to unblock a release: not by unsetting
`TETON_SIGN_IDENTITY`, not by dropping the `environment:` declaration, not by
publishing a run that skipped signing. A release published unsigned after a
signed one re-prompts every user who has granted the daemon Keychain access, and
the next release cannot undo that — the prompt already happened.

### AC-6 — Keychain-grant survival (PASSED 2026-08-03)

AC-6 is the human check for the contract at the top of this section, and it
cannot be run until two consecutively signed releases exist: with one, there is
nothing to upgrade *from*. It is therefore **defined now and recorded as
unrun** — the same posture as AC-4 in §6 and the unrun platform legs in §8, for
the same reason: a criterion nobody could have exercised is recorded as
unexercised, never assumed and never quietly dropped.

First exercisable at the **second** signed release, from a machine still running
the first one that has already granted the daemon Keychain access. Copy this
block into that release's sign-off with the defaults as written, and edit only
the lines you actually ran.

```
AC-6 sign-off — Keychain-grant survival, teton v0.1.3 (upgraded from v0.1.2)
---------------------------------------------------------------------------
Status            :  pass
Verified by       :  Brett (user-confirmed in session)
Date              :  2026-08-03
Prior release     :  v0.1.2            (first Developer ID signed release)
Grant established :  yes               (Keychain prompt raised and accepted
                                       while running v0.1.2)
brew upgrade + brew services restart : run
Re-prompt observed:  NO — pass         (the grant survived the upgrade)
Team id, both releases : 545BU9G9D6    (identical; asserted per-release by the
                                       smoke's signature gate)
Notes / findings  :  The user-visible contract of REQ-550 BR-1 holds: the
                     v0.1.2 prompt was the LAST one. This closes the arc that
                     began with REQ-549 ("why is teton code coming across as
                     'tetond' when asking for permissions?").
```

---

## 11. Secret retirement (REQ-550 AC-4)

`HOMEBREW_TAP_TOKEN` currently exists twice: as a repository secret, which is
how it has always resolved, and on the `tap-publish` environment, which is how
`bump-formula` resolves it now that the job declares `environment: tap-publish`.
AC-4 is met when the repository-level copy is gone, a dispatch from a ref the
rules refuse has been shown to be unable to reach the environment one (step 4),
*and* a release has gone green through `bump-formula` with only the environment
copy left standing (step 6).

The order below is not a preference. Deleting the repository secret before step
2 breaks the next release: until the workflow that declares
`environment: tap-publish` is on the tag's own commit, the bump job running from
that tag is still being served by the repository-level copy, and removing it
takes the token away from the only job that needs it (`75`, "the release is
published but the formula cannot be pushed" — §9).

- [x] *(2026-08-01, PR #13)* **1. REQ-550 is merged to `main`.** The environment declarations have to
      be in the workflow file before they can be on a tag.
- [x] *(2026-08-03, v0.1.2 — all jobs green incl. bump-formula and verify-install)* **2. One release completes green end to end**, `bump-formula` included,
      from a real `vX.Y.Z` tag cut after that merge. What this proves is that
      the workflow still works with the environment declared — and nothing
      more. It does **not** prove which copy of the token served it. Both
      copies exist at this point; GitHub prefers the environment's when a job
      declares one, but a green bump is equally consistent with either having
      been used, and no log line says which. The positive proof of environment
      resolution comes at step 6, once the repository copy is gone. Step 2 is
      the safety check before the deletion, not the evidence for it.
- [x] *(2026-08-03, deleted via API; post-deletion listing then showed the intro's premise was wrong — see step 6)* **3. Delete the repository-level `HOMEBREW_TAP_TOKEN`** — Settings →
      Secrets and variables → Actions → the repository secret → *Remove*. The
      `tap-publish` environment secret stays; it is the one the bump now uses.
- [x] *(2026-08-03, run 30832422680 on probe/ac4-tap-publish-refusal: conclusion failure, zero steps executed — refused before the job started; push-triggered rather than workflow_dispatch because GitHub does not index dispatch-only workflows living solely on a branch; API job record stands in for the screenshot; branch deleted)* **4. Run the AC-4 negative probe and record the refusal.** Environment
      protection rules are GitHub-side settings, so the only durable evidence
      that they work is a run that was refused by them. On a throwaway branch —
      not `main`, which the rules deliberately admit (§2), and not a tag — add a
      minimal workflow — `workflow_dispatch` only, one job,
      `environment: tap-publish`, one trivial step — and dispatch it from that
      branch. Record what GitHub does: the job does not start, and the run
      reports the deployment as blocked by the environment's protection rules.
      Screenshot it, put the run URL in REQ-550's AC-4, then delete the branch.
      A probe that *succeeds* is the finding, not a mistake to retry — it means
      the rules are not what they are supposed to be, and the retirement is not
      done.
- [x] *(2026-08-03, recorded in REQ-550 via API listing — names only)* **5. Record the environment settings state in REQ-550.** For each of
      `release-signing`, `tap-publish` and `site-deploy`: the deployment branch
      and tag rules, the secret and variable *names* each carries (names only —
      never values), and any required reviewers. None of this configuration
      lives in the repository, so a screenshot plus a written table in the REQ
      is the entire record a future reader gets. Date it; settings drift
      silently and nothing in CI notices.
- [x] *(2026-08-03: blocker hit exactly as this step's last sentence anticipated — post-deletion verification found tap-publish empty, the token was re-added as an environment secret (fresh fine-grained PAT, old one retired), and v0.1.2's bump-formula job RERAN GREEN (run 30831179493, job 91746712506) with the environment as the only copy. Environment resolution proven; AC-4 complete.)* **RESOLVED — post-deletion verification found `tap-publish` holds NO token copy — the environment paste never landed, so v0.1.2's green bump was served by the (now deleted) repository copy. Remediation per this step: add `HOMEBREW_TAP_TOKEN` to the `tap-publish` environment, then re-run v0.1.2's `bump-formula` job — a green rerun with the environment as the only copy is the positive proof.** **6. The first release AFTER the deletion goes green through
      `bump-formula`.** This is the run that proves environment resolution —
      the positive half of AC-4, and the claim step 2 could not make. With no
      repository-level copy left, a green bump can only mean the token resolved
      out of `tap-publish`. Step 4 is the negative half (a ref the rules refuse
      gets nothing); this is the positive one (the ref they admit gets the
      token). Record the run URL in REQ-550's AC-4 beside the probe's. If this
      bump instead exits `75` with the token unset, the deletion took away the
      only copy that was ever working — restore it as an *environment* secret,
      not a repository one, and re-run the job.

AC-4 also names `GCP_*`. There is nothing to delete there, and that is now
checkable rather than asserted: `deploy-site.yml` declares
`environment: site-deploy`, and `GCP_PROJECT`, `GCP_SERVICE_ACCOUNT` and
`GCP_WIF_PROVIDER` live on that environment with **no repository-level copy of
any of them** — verified 2026-08-04, when the repository secret list came back
empty. So the site deploy went from blocked to live (OQ-5, 2026-08-01) without
ever putting a `GCP_*` secret at repository level, which is the posture AC-4
asks for. What remains is the GCP-side attribute condition on the workload
identity provider — [site-deploy-runbook.md](site-deploy-runbook.md).
