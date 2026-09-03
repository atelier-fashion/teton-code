---
id: LESSON-627
title: "A file-race fix has to land at the seam that opens the file — a flag on the open is inert when the resolver already dereferenced the path"
component: "daemon/harness"
domain: "harness"
stack: ["rust", "daemon"]
concerns: ["security", "privacy"]
tags: ["toctou", "symlink", "o-nofollow", "canonicalize", "inode-identity", "repo-context", "jail", "verify-loop"]
req: REQ-612
created: 2026-09-03
updated: 2026-09-03
---

## What Happened

REQ-612 reads a repository's `TETON.md` into the system prompt. The loader
`lstat`s the spelled entry, refuses a symlink, resolves the path through the
tool jail's `ToolContext::resolve`, then opens and reads. Security review found
the classic race: swap the entry for a symlink after the `lstat` and the open
follows it. The first fix mirrored the transcript writer's precedent and opened
with `O_NOFOLLOW`, with a length comparison as a second check. Its test opened a
symlink through `RealFiles::read` directly and went red on the mutation, so it
looked closed.

The confirmation loop traced the actual call chain: `resolve` calls
`canonicalize`, which dereferences a final-component symlink *before* `read`
ever sees the path. By the time `O_NOFOLLOW` ran there was no link left to
refuse; the flag guarded only the narrower resolve-to-open gap. And length is
not identity — any same-size file substituted in the window passed.

The second fix carried `(dev, ino)` from the entry `lstat` into the read as a
`FileIdentity`, compared it against the opened handle's `fstat`, and refused
`nlink > 1` at the entry rule. The proof test plants the symlink *before* `load`
and drives the whole chain, not the primitive.

## Lesson

**Before fixing a time-of-check/time-of-use race, trace every seam between the
check and the use, and ask what each one does to the path.** A resolver that
canonicalizes is a dereference; a flag added downstream of it protects nothing.
Close the window with an identity the check already holds — device and inode,
which an in-place edit preserves and a substitution cannot forge — and compare
it at the use. A test that exercises the primitive in isolation proves the
primitive works, not that the loader ever hands it the input the attack needs;
prove it through the full entry point.

## Why It Matters

The first fix would have shipped with a green mutation test and a doc comment
saying the window was "unraceable". The reviewer's refutation cost one read of
`resolve`; finding it in production would have meant an arbitrary in-root file
egressing under the notes' identity on every remote turn. LESSON-485's rule
(a fixture that cannot reach the discriminating state is not a test) and
LESSON-552's (test the derivation, not the minter) both apply: the
discriminating state here was "the resolver already ran", and the test never
reached it.

## Applies When

Any check-then-act on a path: symlink refusals, denied-prefix jails, "regular
file only" rules, config or transcript writers. Any time a precedent's flag is
copied into a new call site — the precedent's seam and the new seam may differ
exactly where it matters.
