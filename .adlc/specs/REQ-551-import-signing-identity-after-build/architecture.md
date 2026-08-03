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
BR-2) → tar → sha256, consuming the staging dir; a missing or incomplete
staging dir is a hard 70 — a pack that didn't follow a build can never emit
a tarball (the cross-boundary half of BR-2). "Incomplete" covers the
ride-alongs as well as the binaries: `LICENSE` and `README.md` are staged by
`build` and checked by `pack`, because every member of the tarball comes from
the staging directory and a missing one must fail inside this script's exit
taxonomy rather than as a `tar` error about a file. `all`
is `build` then `pack` in-process. The workflow's darwin legs become:
`package.sh build` → Import identity (step moved verbatim — executable body
byte-identical, `- name:` unchanged; the 16 annotation strings that claimed
"Nothing was built" are retargeted under TASK-029's recorded scope extension,
since the reorder is exactly what made them false) → `package.sh pack` (with
`TETON_SIGN_IDENTITY`) → destroy the keychain → smoke.
Linux stays a single `all` call (no keychain, no reason to split).

**Rationale**: OQ-1 resolved to same-job reorder: split jobs would buy
isolation against a compromised *runner* (out of threat model, per the
spec) at the cost of artifact hand-off and a second macOS spin-up per
leg. The phase argument keeps one implementation of sign→verify→tar
(REQ-551 assumption: interface change, not rewrite), keeps `all` for
humans, and gives selftest direct handles on each phase. The keychain
window shrinks from ~30 min of third-party compilation to seconds of
first-party signing code (BR-1). That last sentence is only literally true
because of a step added in this REQ's verify pass (2026-08-03, from the
reflector's proposal): `Destroy the signing keychain (early)` runs
immediately after `pack` and before the smoke, so the window ends at signing
rather than at the end of the job — without it the smoke and the artifact
upload were still inside it. The `if: always()` cleanup stays as the
failure-path backstop and no-ops once the early destroy has run.

**Consequences**: the stubbed-cargo selftest cases move to driving
`build` and `pack` separately (plus `all` for the compat contract);
existing exit-code taxonomy unchanged (64/70/75 + cargo passthrough).
The import step's env publications and the cleanup step move nothing —
only step order changes.

## ADR-551-2: The ordering is a selftest assertion over release.yml, mutation-proven

**Decision**: a new selftest case asserts LINE ORDER across release.yml —
it does not parse the job's step sequence, and does not try to. Three fixed
anchors are located by `index()`, each of which must appear on exactly ONE
line, with ambiguity treated as absence: the step invoking
`package.sh ... build` precedes "Import the Developer ID signing
identity", which precedes the step invoking `package.sh ... pack`. A missing
or duplicated anchor is a named failure, never a quiet pass — an assertion
satisfiable by deleting what it reads is not an assertion. It
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
partition list, search-list capture/restore), `if: always()` cleanup step,
smoke gate, attestation/publish jobs, environments, seam CI-refusals: all
unchanged as EXECUTABLE code. Comments and annotation strings are not frozen
by this list and did move where the reorder made them false — see the
"moved verbatim" note in ADR-551-1 and the Deviations section of the
requirement.
The accepted-risk comment near the p12 `rm` and runbook §10's residual
note flip to "closed by REQ-551". `.adlc/context/architecture.md` ADR-008's
accepted-risk consequence gets its closing note at wrapup.

Added after this list was written (verify passes, 2026-08-03), so the frozen
surface is not read as "nothing else was added".

In `release.yml`: the `Destroy the signing keychain (early)` step, which grades
its delete on the POST-CONDITION (`[ ! -e ]`) and emits the closing BR-1 notice
only on a graded success, so one step cannot warn that the identity may have
survived and announce that it did not. And an environment preamble on all three
identity-touching steps (import, `Sign and package`, early destroy):
`unset TETON_CODESIGN TETON_ALLOW_TOOL_SEAM BASH_ENV ENV CDPATH`, `unset -f` for
the tools each one runs, a system-directory `PATH`, and `security` invoked as
`/usr/bin/security`. The earlier build step can append to `$GITHUB_ENV`/
`$GITHUB_PATH`, which the runner applies between steps, so the seam guard has to
be un-defeatable from ambient environment (LESSON-460's family) — and the
reorder is what put these steps downstream of that build in the first place.
Stated at its limit: this closes the IN-BAND channels; a persistent background
process started during the build is runner-level compromise, out of scope here,
and OQ-1's split-jobs option is the recorded answer to it.

In `package.sh`/`lib.sh`: the `.stage-meta` handoff manifest (framed as an
accident detector, explicitly not authentication — see the requirement's
Deviations), the scratch-sign design that keeps `pack` from ever writing into
the staging directory, the argument refusals (a fifth argument; a phase name in
the `[outdir]` slot, gated on `$# -eq 3`), per-member file-type checks, and
`tool_or_unchecked`'s absolute-path rule for unoverridden tool names. Each is
enumerated in the requirement's Deviations so the merged scripts need not be
diffed against these ADRs to find them.

Every one of these controls is asserted by `tools/release/selftest.sh` and
mutation-proven: the round-2 pass added them and the round-3 pass found the
suite stayed green with each of them deleted, which is the shape a comment
claiming to be a guard has (LESSON-443/LESSON-454).
