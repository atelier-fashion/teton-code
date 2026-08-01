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
  which this workflow **dispatches** after the tap bump succeeds, and which is
  blocked until REQ-548's OQ-5 is answered.

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
| 2 | `HOMEBREW_TAP_TOKEN` set on the **`tap-publish` environment**, org-approved if required (repo-level copy retires per §11) | **outstanding** | [homebrew-tap-setup.md §2](homebrew-tap-setup.md) | `bump-formula` (exits `75` without it) |
| 3 | A dry run has gone green end to end | **outstanding** | §2 below | everything — do not skip |
| 4 | GCP secrets for the site | outstanding (OQ-5) | [site-deploy-runbook.md §2](site-deploy-runbook.md) | `tetoncode.ai` only; a release is fine without them |

The tap is intentionally empty — no `Formula/teton.rb` until the first bump
writes one, and no `Formula/` directory either; `bump-formula` creates it. An
empty tap is the correct pre-first-release state, not a missing step.

Items 2 and 4 are somebody's *access*, not somebody's afternoon: the token may
need an organisation owner's approval, and OQ-5 needs somebody with the Atelier
GCP org. Start them before you want to release, not during.

---

## 2. The dry run — do this before your first tag

`workflow_dispatch` exists so the pipeline is testable without spending a tag.
Actions → **Release** → *Run workflow*, from `main`, leaving **`dry_run`
checked** (its default).

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
| `release` | ubuntu | Recomputes `checksums.txt` from the uploaded artifacts (BR-5), renders notes, `gh release create --verify-tag` | fewer than 3 tarballs (`75`); a dry run stops here and prints what it would have published |
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
| `70` | (`package.sh` only) the build reported success but an expected binary is missing |

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
#    the assets match a list published beside them; this proves who made them.
#    Letter for letter the command the `release` job runs as a gate and the
#    README hands users (AC-2).
for t in /tmp/rel/*.tar.gz; do
  gh attestation verify "$t" --repo atelier-fashion/teton-code
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

Until OQ-5 is answered that dispatched run renders the page, uploads it as the
`site-dist` artifact, and then fails its `Deploy result` step on purpose. That
red job does **not** mean the release is bad — see
[site-deploy-runbook.md](site-deploy-runbook.md).

---

## 6. The upgrade path (AC-4)

```sh
brew upgrade teton
brew services restart teton
teton doctor        # confirm the RUNNING daemon reports the new version
```

The restart is the part worth writing down. `brew upgrade` replaces the binaries
on disk; a `tetond` that is already running is still the old binary until
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
4. **The site will not deploy** (OQ-5). Expected; see §5. The dispatch still
   happens — you will see a `Deploy site` run appear and go red at its last
   step. That is the design, not a symptom of the release.
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
Daemon started    :  unrun            (run `tetond` yourself — `brew services`
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

**`Missing release artifacts` (`75`)** — fewer than three tarballs reached the
`release` job. A release that silently omits a platform is worse than no
release. Find the build leg that did not upload.

**`HOMEBREW_TAP_TOKEN is not set` (`75`)** — see §7 item 2 and
[homebrew-tap-setup.md §2](homebrew-tap-setup.md). Add the secret, then
**re-run the failed job** (Actions → the run → *Re-run failed jobs*). Do not
re-dispatch the workflow: `gh release create` would fail on the existing
release. The bump is idempotent — it exits green with *"formula already
current"* if the tap already matches.

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

### Checking a signed release by hand

Continuing from §5 — the tarballs are already in `/tmp/rel`:

```sh
TAG=vX.Y.Z
mkdir -p /tmp/rel-check
tar -xzf "/tmp/rel/teton-$TAG-aarch64-apple-darwin.tar.gz" -C /tmp/rel-check

# 1. The signature is structurally valid and covers the bytes as shipped.
codesign --verify --strict /tmp/rel-check/teton /tmp/rel-check/teton-code

# 2. It names an authority and a team — which (1) does not test. An ad-hoc
#    signature passes (1) while naming nobody at all.
codesign -dvv /tmp/rel-check/teton 2>&1 | grep -E 'Identifier|Authority'
```

That grep prints:

```
Identifier=teton
Authority=Developer ID Application: Atelier Fashion LLC (545BU9G9D6)
Authority=Developer ID Certification Authority
Authority=Apple Root CA
TeamIdentifier=545BU9G9D6
```

`-dvv`, and the second `v` is load-bearing: at verbosity 1 codesign prints
`TeamIdentifier=` but no `Authority=` lines at all, so `-dv` on a perfectly
signed binary shows no authority and reads as a failure. These are the same two
questions `tools/release/verify-signature.sh` asks of every macOS tarball in the
release smoke — this is the laptop-side spelling of the gate, not a second
opinion.

Repeat for `x86_64-apple-darwin`. A green check on the arm64 tarball is not
evidence about the Intel one (LESSON-433), and a green check on `teton` is not
evidence about `teton-code`.

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

Where it surfaces, in the order a run reaches them:

- **the `build` job's "Import the Developer ID signing identity" step** —
  `Signing identity UNAVAILABLE` (the secrets resolved empty — check the
  `release-signing` environment's tag rule against the ref), `Signing
  certificate UNREADABLE` (`MACOS_CERT_P12` is not decodable base64), `Signing
  certificate would not import` (usually a wrong `MACOS_CERT_PASSWORD`, or a
  `.p12` exported without its private key), or `No Developer ID identity for
  team 545BU9G9D6` (it imported, but it is the wrong *kind* of certificate).
  Nothing is built on that leg.
- **`package.sh`, exit `70`** — signing was requested and codesign could not
  carry it out, or it signed and then its own `--verify --strict` rejected the
  result. No tarball is written: a signing-requested build never ships unsigned.
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

### AC-6 — Keychain-grant survival (staged)

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
AC-6 sign-off — Keychain-grant survival, teton vX.Y.Z (upgraded from vA.B.C)
---------------------------------------------------------------------------
Status            :  unrun            ( unrun | pass | fail )
Verified by       :
Date              :
Prior release     :                   (the signed release the grant was given to)
Grant established :  unrun            (a Keychain prompt was raised and accepted
                                       while running the PRIOR release)
brew upgrade + brew services restart : unrun
Re-prompt observed:  unrun            (no prompt == pass; any prompt == fail)
Team id, both releases : unrun         (codesign -dvv on each; must be identical)
Notes / findings  :
```

---

## 11. Secret retirement (REQ-550 AC-4)

`HOMEBREW_TAP_TOKEN` currently exists twice: as a repository secret, which is
how it has always resolved, and on the `tap-publish` environment, which is how
`bump-formula` resolves it now that the job declares `environment: tap-publish`.
AC-4 is met when the repository-level copy is gone *and* a dispatch from a
non-release ref has been shown to be unable to reach the environment one.

The order below is not a preference. Deleting the repository secret before step
2 breaks the next release: until the workflow that declares
`environment: tap-publish` is on the tag's own commit, the bump job running from
that tag is still being served by the repository-level copy, and removing it
takes the token away from the only job that needs it (`75`, "the release is
published but the formula cannot be pushed" — §9).

- [ ] **1. REQ-550 is merged to `main`.** The environment declarations have to
      be in the workflow file before they can be on a tag.
- [ ] **2. One release completes green end to end**, `bump-formula` included,
      from a real `vX.Y.Z` tag cut after that merge. This is the step that
      proves *environment* resolution: a green bump on a tag whose commit
      carries the `environment: tap-publish` declaration is the only evidence
      that the token is reachable from the environment. Nothing before this
      point distinguishes "resolved from the environment" from "resolved from
      the repository".
- [ ] **3. Delete the repository-level `HOMEBREW_TAP_TOKEN`** — Settings →
      Secrets and variables → Actions → the repository secret → *Remove*. The
      `tap-publish` environment secret stays; it is the one the bump now uses.
- [ ] **4. Run the AC-4 negative probe and record the refusal.** Environment
      protection rules are GitHub-side settings, so the only durable evidence
      that the tag rule works is a run that was refused by it. On a throwaway
      branch, add a minimal workflow — `workflow_dispatch` only, one job,
      `environment: tap-publish`, one trivial step — and dispatch it from that
      branch. Record what GitHub does: the job does not start, and the run
      reports the deployment as blocked by the environment's protection rules.
      Screenshot it, put the run URL in REQ-550's AC-4, then delete the branch.
      A probe that *succeeds* is the finding, not a mistake to retry — it means
      the tag rule is not what it is supposed to be, and the retirement is not
      done.
- [ ] **5. Record the environment settings state in REQ-550.** For each of
      `release-signing`, `tap-publish` and `site-deploy`: the deployment branch
      and tag rules, the secret and variable *names* each carries (names only —
      never values), and any required reviewers. None of this configuration
      lives in the repository, so a screenshot plus a written table in the REQ
      is the entire record a future reader gets. Date it; settings drift
      silently and nothing in CI notices.

AC-4 also names `GCP_*`. There is nothing to delete there: per this REQ's
architecture note no `GCP_*` secret was ever set at repository level (the site
deploy is still blocked on OQ-5), `deploy-site.yml` declares
`environment: site-deploy`, and what remains is the GCP-side attribute condition
on the workload identity provider — [site-deploy-runbook.md](site-deploy-runbook.md).
