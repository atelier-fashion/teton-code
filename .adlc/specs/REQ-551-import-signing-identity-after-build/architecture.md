# REQ-551 Architecture — Import the signing identity after untrusted compilation

Grounding (post-#13/#14 main, traced at line level): the `build` job runs
Import (release.yml ~319-620, publishes `TETON_SIGNING_KEYCHAIN`/`_P12`/
`TETON_PRIOR_KEYCHAINS` to `$GITHUB_ENV` before creating anything) → Build
and package (~640-648, one `package.sh` call that does cargo build at
line 144 AND sign→verify→tar at 168-228) → smoke → upload, with an
`always()` cleanup (~722-773) that defensively tolerates unset vars. The
unlocked keychain joins the search list at ~553 and stays through the
job. Selftest drives package.sh's signing phase through a PATH-stubbed
cargo (~1696-1712) and has a cross-file consistency case that greps
release.yml (~1876-1895) — the pattern the new ordering assertion mirrors.
Cleanup, dry-run, and GITHUB_ENV lifetimes impose no ordering constraint
(integration-explorer verdict: window-closing only).

## ADR-551-1: Same-job reorder via a package.sh phase argument — `build`, `pack`, `all`

**Decision**: `package.sh` gains an optional 4th argument `phase` ∈
`{all, build, pack}`, default `all` (byte-compatible with today's single
invocation — runbook and any local use unchanged). `build`: arg validation →
cargo build → stage binaries + LICENSE/README into a deterministic
`<outdir>/stage-<target>/` — no signing, no signing-tool resolution, no
`TETON_SIGN_IDENTITY` required. `pack`: seam guard → signing-tool
resolution → sign→verify (keyed on `TETON_SIGN_IDENTITY` exactly as today,
BR-2) → tar → sha256, consuming the staging dir; a missing/empty staging
dir (either binary absent) is a hard 70 — a pack that didn't follow a
build can never emit a tarball (the cross-boundary half of BR-2). `all`
is `build` then `pack` in-process. The workflow's darwin legs become:
`package.sh build` → Import identity (step moved verbatim, content
unchanged) → `package.sh pack` (with `TETON_SIGN_IDENTITY`) → smoke.
Linux stays a single `all` call (no keychain, no reason to split).

**Rationale**: OQ-1 resolved to same-job reorder: split jobs would buy
isolation against a compromised *runner* (out of threat model, per the
spec) at the cost of artifact hand-off and a second macOS spin-up per
leg. The phase argument keeps one implementation of sign→verify→tar
(REQ-551 assumption: interface change, not rewrite), keeps `all` for
humans, and gives selftest direct handles on each phase. The keychain
window shrinks from ~30 min of third-party compilation to seconds of
first-party signing code (BR-1).

**Consequences**: the stubbed-cargo selftest cases move to driving
`build` and `pack` separately (plus `all` for the compat contract);
existing exit-code taxonomy unchanged (64/70/75 + cargo passthrough).
The import step's env publications and the cleanup step move nothing —
only step order changes.

## ADR-551-2: The ordering is a selftest assertion over release.yml, mutation-proven

**Decision**: a new selftest case parses the `build` job's step sequence
out of release.yml (step-name grep, same file-reading pattern as the
team-id consistency case) and asserts: the step invoking
`package.sh ... build` precedes "Import the Developer ID signing
identity", which precedes the step invoking `package.sh ... pack`. It
runs in ci.yml's tooling job on every PR. AC-4's known-bad proof: move
the import step above the build step in a scratch copy, watch the case
fail, restore (recorded in the case comment).

**Rationale**: BR-6 — REQ-550's verify pass showed a comment claiming a
guard is not a guard (LESSON-443's shape, LESSON-454's remedy). A
structural assertion in the suite that already greps this file is the
cheapest mechanical enforcement; actionlint cannot express step-order
policy.

## Non-changes (frozen surface)

Import-step content (creation order, masking, p12 `rm` after import,
partition list, search-list capture/restore), cleanup step, smoke gate,
attestation/publish jobs, environments, seam CI-refusals: all unchanged.
The accepted-risk comment near the p12 `rm` and runbook §10's residual
note flip to "closed by REQ-551". `.adlc/context/architecture.md` ADR-008's
accepted-risk consequence gets its closing note at wrapup.
