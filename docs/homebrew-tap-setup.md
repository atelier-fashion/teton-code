# Homebrew tap — one-time setup

`brew install atelier-fashion/tap/teton` resolves to a formula in a second
repository, `atelier-fashion/homebrew-tap`. This document covers the parts of
that repository a human has to create once, by hand, because they involve
creating a repo and minting a credential — everything after that is done by the
`bump-formula` job in `.github/workflows/release.yml`.

**The tap is a publish target, never a source (ADR-548-1).** `Formula/teton.rb`
there is generated from `packaging/homebrew/teton.rb.tmpl` here. A hand edit in
the tap survives exactly until the next release overwrites it, and leaves no
trace of why it was made. Formula changes belong in the template, where they
ride this repo's PR review.

---

## 1. The tap repository

| | |
|---|---|
| Name | `atelier-fashion/homebrew-tap` (**exactly** — `brew` derives it from the `atelier-fashion/tap` in the install command) |
| Visibility | **public**. `brew` clones it unauthenticated on every user's machine; a private tap is an unusable tap. |
| Default branch | `main` |
| Required layout | a `Formula/` directory at the root |

Nothing else is required. The bump job creates `Formula/teton.rb` on its first
successful run.

**Executed 2026-07-25 (TASK-017)**: the repo exists —
<https://github.com/atelier-fashion/homebrew-tap> — public, `main`, README +
MIT LICENSE only, deliberately no `Formula/teton.rb` yet. `brew tap
atelier-fashion/tap` verified against the empty tap (exit 0) and untapped
again. The token (section 2) followed on 2026-07-31, on the `tap-publish`
environment; the repository-level copy is still there and retires on the
ordered checklist in [release-runbook.md §11](release-runbook.md).

## 2. The token

The release workflow pushes from `teton-code` into `homebrew-tap`. The
automatic `GITHUB_TOKEN` cannot do that: it is scoped to the repository running
the workflow. So the bump needs a credential of its own.

Mint a **fine-grained personal access token**:

| Setting | Value | Why |
|---|---|---|
| Resource owner | `atelier-fashion` | |
| Repository access | **Only select repositories** → `atelier-fashion/homebrew-tap` | The token can push a formula. It must not be able to touch `teton-code`, the site, or anything else. A token that can only damage the tap is a token whose worst case is a bad formula, which the audit gate already catches. |
| Repository permissions | **Contents: Read and write** | The whole job is `git clone` + `git push`. Nothing else is needed — not Actions, not Workflows, not Metadata beyond the default. |
| Expiration | your choice; put the date in the calendar | See "when it expires" below. |

Under an organisation, a fine-grained PAT may need an org owner to approve it
before it works. Approve it before the first release, not during one.

Store it in **this** repository (`atelier-fashion/teton-code`), under
*Settings → Secrets and variables → Actions → New repository secret*:

```
Name:   HOMEBREW_TAP_TOKEN
Value:  github_pat_...
```

The name is load-bearing — `release.yml` reads
`${{ secrets.HOMEBREW_TAP_TOKEN }}` and nothing else.

**As of REQ-550 the token is environment-scoped.** It lives on the
`tap-publish` environment (deployment rules: tags matching `v*.*.*`, **plus**
the branch `main` — the dry run in [release-runbook.md §2](release-runbook.md)
is dispatched from `main` and cannot start a job the environment refuses; BR-4's
own wording is "`v*.*.*` tags and/or `main`"), and
`bump-formula` declares `environment: tap-publish` — so the same
`secrets.HOMEBREW_TAP_TOKEN` reference resolves out of the environment rather
than the repository, and a workflow that does not declare the environment cannot
read the token at all (BR-4). Put it there — Settings → Environments →
`tap-publish` → *Environment secrets* — rather than in the repository-secret box
above. The repository-level copy is retired on the ordered checklist in
[release-runbook.md §11](release-runbook.md), after a release has gone green
from the environment; deleting it earlier breaks the next bump.

### When it expires

The bump job fails, loudly, and the release run goes red (BR-4). That is the
designed behaviour: the alternative is a published release sitting beside a tap
that still points at the previous version, with `brew install` quietly handing
users the old binaries. Mint a new token, update the secret, and re-run the
`bump-formula` job — the release itself does not need to be redone, and the job
is idempotent (it exits green with "formula already current" if the tap is
already correct).

## 3. Rendering the formula by hand

Only needed to seed the tap before a first release, or to reproduce what a
release run did. The script is the same one CI calls.

From a checkout of this repo, with the three release tarballs in a directory:

```sh
tools/release/render-formula.sh \
  --version 0.1.0 \
  --artifacts dist \
  --output /path/to/homebrew-tap/Formula/teton.rb
```

`--artifacts` hashes the tarballs itself. If you only have the hashes, name each
one by its target — they are keyed by triple rather than positional precisely so
a hash cannot be filed under the wrong platform:

```sh
tools/release/render-formula.sh \
  --version 0.1.0 \
  --sha-aarch64-apple-darwin     <sha256> \
  --sha-x86_64-apple-darwin      <sha256> \
  --sha-x86_64-unknown-linux-gnu <sha256> \
  --output /path/to/homebrew-tap/Formula/teton.rb
```

Exit codes: `0` rendered, `64` the inputs were wrong or the template has a
placeholder the script cannot fill, `75` the render could not be attempted
(missing template, missing tarball). A non-zero exit means nothing was written.

Then check it the way CI does, from inside a clone that lives at
`$(brew --repository)/Library/Taps/atelier-fashion/homebrew-tap` — Homebrew
refuses to style or audit a formula that is not in a tap:

```sh
brew style --formula atelier-fashion/tap/teton
brew audit --formula atelier-fashion/tap/teton
```

## 4. Two Homebrew behaviours worth knowing before you debug them

**The formula has no `version` stanza, deliberately.** Homebrew scans the
version out of the release URL, and `brew audit` rejects an explicit `version`
that agrees with the scan as redundant — the audit gate and a declared version
cannot both exist. The version is therefore pinned by the rendered URLs, and the
bump job asserts that the version Homebrew resolves equals the tag before it
pushes anything (BR-3), rather than trusting the scan. If a future Homebrew
changes how it reads `teton-v1.2.3-x86_64-apple-darwin.tar.gz`, that assertion
fails the release instead of publishing a mislabelled formula.

**Homebrew 6 requires third-party taps to be trusted** — `HOMEBREW_REQUIRE_TAP_TRUST`
defaults to on — *but* it treats a fully-qualified name on the command line as
self-authorizing. So:

```sh
brew install atelier-fashion/tap/teton   # works, no trust step (BR-1 holds)
brew services start teton                # may refuse: "not trusted"
brew trust atelier-fashion/tap           # one-time fix for short-name commands
```

Documentation that tells users to run `brew services start teton` should say
this, because the failure message points at `brew trust` without explaining why
the install that just succeeded did not need it.
