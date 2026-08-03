---
id: LESSON-463
title: "A step boundary is an environment channel — reordering steps for security must scrub what flows forward"
component: "distribution/release"
domain: "security"
stack: ["github-actions", "ci", "bash"]
concerns: ["security", "reliability"]
tags: ["bash-env", "github-env", "github-path", "function-export", "step-reorder", "keychain", "supply-chain"]
req: REQ-551
created: 2026-08-03
updated: 2026-08-03
---

## What Happened

REQ-551 moved the Developer ID keychain import *after* `cargo build` so no
third-party build script could run beside the unlocked keychain. The reorder
itself opened the channel it was closing, in a subtler form: GitHub applies
`$GITHUB_ENV`/`$GITHUB_PATH` writes *between* steps, so a hostile build
script's environment became live inside every later step — including the
import step, where the raw p12 and its password exist. Three concrete,
empirically proven vectors: `BASH_ENV` (sourced by `bash -e {0}` *before*
the step body, so an in-body `unset` runs too late for that step's own
startup), exported shell functions (`export -f codesign` survives an env
scrub and wins `command -v`), and PATH-shadowed binaries receiving the
certificate password in argv. The first fix pass scrubbed only two variable
names in one step and claimed "two lines close that" — the re-verify proved
otherwise.

## Lesson

When you reorder steps so that credential-handling code runs *downstream* of
untrusted execution, treat every forward-flowing channel as hostile at the
point of consumption: `unset BASH_ENV ENV CDPATH` and `unset -f` the tools
you invoke, set a system-only (or system-first) `PATH`, and call
security-critical binaries by absolute path — in *every* step that touches
the credential, not just the obvious one. Have shared helpers reject
non-absolute `command -v` resolutions (a function or alias resolves as a
bare name). And state the residual honestly: an in-body scrub cannot precede
`BASH_ENV` sourcing, and a persistent background process is runner-level
compromise — name the boundary and the control that owns the other axis
(here: job isolation as future work, provenance attestation for byte
integrity).

## Why It Matters

The reorder's whole value was "no untrusted code beside the key"; the
unscrubbed channel silently restored exactly that, in the one step where the
key is exportable — while the diff read as a pure security improvement.
Time-shifted channels are invisible to step-local review because the attack
is planted in one step and detonates in another.

## Applies When

- Reordering CI steps/jobs for isolation (the reorder changes which env
  writes are upstream of what).
- Any step that touches signing keys, tokens, or keychains downstream of
  dependency-executing steps (`build.rs`, npm postinstall, cmake).
- Reviewing "scrub"/"sanitize" fixes: enumerate channels (env vars, PATH,
  BASH_ENV/ENV, exported functions, aliases), not variable names (see
  [[LESSON-460]] — prove against the real mechanism, and [[LESSON-455]] —
  the property spans every step, not the one the finding cited).
