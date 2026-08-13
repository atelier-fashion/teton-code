---
id: REQ-571
title: "Canonical provenance identity for privacy-boundary enforcement"
status: draft
deployable: true
created: 2026-08-13
updated: 2026-08-13
component: "daemon/egress"
domain: "privacy"
stack: ["rust", "daemon"]
concerns: ["privacy", "security"]
tags: ["br-1", "boundary-enforcement", "provenance", "path-canonicalization", "symlink", "prompt-injection", "fail-closed", "config-validation"]
---

## Description

The BR-1 privacy guarantee — "paths marked `local-only` never leave the machine" —
is currently bypassable. Every harness tool resolves the file it opens to a
canonical path, but `read` and `edit` tag the result's egress provenance with the
**raw, model-supplied request string** instead. The boundary matcher normalizes
only a single leading `./`, so any other spelling of the same protected file
matches no glob, carries provenance that matches nothing, and is forwarded to a
remote provider intact.

Seven spellings of the same file evade a `secrets/**` boundary, including
absolute-inside-root (`/Users/x/repo/secrets/prod.env`), `..`-traversing
(`src/../secrets/prod.env`), and repeated-`./` forms. An in-repo symlink reaches
the same bytes under an innocuous name. The gap is reachable by prompt injection
from any file, dependency README, MCP tool result, or fetched page — no `shell`
call is needed, so the session never gains `Unknown` provenance.

Every backstop shares the single normalization gap rather than compensating for
it: session taint calls the same matcher on the same raw sources, so the session
never pins local; the web-lookup taint gate keys on that taint, leaving a clean
second-hop exfiltration channel via a model-composed `web_fetch`; and redaction
defaults off and matches secret *shapes*, not `local-only` *sources*.

This is a partial application of a lesson this project already recorded. REQ-544
fixed provenance for the tools that carry **no** path argument — `shell`, `grep`,
`glob` — and left `read`/`edit` untouched because they "happened to be enforced."
But LESSON-432's stated principle is that a tool's provenance is *the set of
files it read*, and `read`/`edit` still report the **argument**, not the file. The
two tools the lesson exempted are now the only ones still violating it.

Separately, the directory walkers have their own escape: `DirEntry::file_type()`
does not traverse symlinks, so a symlink is not a directory and falls into the
file branch, where the read *does* follow it — surfacing content from outside the
repo root under an in-jail relative name.

## System Model

### Entities

| Entity | Field | Type | Constraints |
|--------|-------|------|-------------|
| ResolvedPath | canonical | path | Absolute, symlink-resolved, verified inside repo root |
| ResolvedPath | provenance_id | string | Repo-root-relative, `/`-separated; the ONLY form used for boundary matching; never absolute, never contains `.` or `..` segments |
| ToolOutcome | paths | list of provenance_id | Derived from files actually accessed; empty only when no file was accessed |
| ToolOutcome | provenance_kind | enum | `Known(paths)` \| `Unknown` (opaque operations) |

### Events

| Event | Trigger | Payload |
|-------|---------|---------|
| privacy_block | Egress inspection finds a boundary-matching or `Unknown` provenance source on a remote-bound turn | Matched `provenance_id`, boundary glob, session id |
| provenance_rejected | A provenance source fails the fail-closed well-formedness check (absolute, or `.`/`..` segment) | Offending source, originating tool name |

### Permissions

Not applicable — this REQ changes no actor-facing permission. Enforcement is
unconditional and applies to every session regardless of permission level.

## Business Rules

- [ ] BR-1: Egress provenance for a tool result MUST be derived from the resolved filesystem identity of the file(s) the tool actually accessed, never from the request argument. (informed by LESSON-432, LESSON-494)
- [ ] BR-2: Provenance MUST be expressed in exactly one canonical form — repo-root-relative, `/`-separated — and boundary matching MUST operate only on that form. (informed by LESSON-494)
- [ ] BR-3: All spellings of a path that resolve to the same file MUST yield identical provenance, and therefore identical boundary verdicts. The enumerated set is: bare relative, `./`-prefixed, repeated `./`, absolute-inside-root, `..`-traversing-but-inside-root, and in-repo symlink to the target.
- [ ] BR-4: A provenance source that is absolute, or retains a `.` or `..` segment after derivation, MUST be rejected fail-closed at the egress inspection point, independently of whether any boundary is configured. This guard is redundant by design and MUST carry its own test. (informed by LESSON-508)
- [ ] BR-5: A directory-walking tool MUST NOT surface content from a file whose resolved identity lies outside the repo root. In-repo symlinks that resolve inside the root MUST be attributed to their resolved target, not to the link name.
- [ ] BR-6: Path containment for a not-yet-existing target MUST be decided against a resolved existing ancestor, never against a lexical path that traversed unresolved components. (informed by LESSON-494)
- [ ] BR-7: Every tool that surfaces file content into model context MUST be enumerated and covered by boundary tests. Carrying a path argument MUST NOT exempt a tool from coverage — that exemption is the specific error this REQ corrects. (informed by LESSON-432, LESSON-502)
- [ ] BR-8: Existing boundary-matcher assertions that absolute and `..`-bearing paths match no repo-relative glob remain correct at the matcher layer and MUST be retained, but each MUST be paired with a tool-layer test proving such spellings can never reach the matcher. (informed by LESSON-502)
- [ ] BR-9: A session whose context includes any boundary-matching source MUST pin to the local tier, and that pin MUST hold on recovery paths (failover, retry), not only the primary path. (informed by BUG-156)
- [ ] BR-10: Every `ConfigError` variant that `Config::validate()` can raise MUST have a test asserting it is raised for the input that triggers it. `validate()` is fail-closed and gates daemon startup, so an unasserted variant is an unguarded startup gate. This is stated as a general rule, not a fixed list, so a newly added variant inherits the obligation. (informed by LESSON-508)

## Acceptance Criteria

- [ ] AC-1: With boundary `secrets/**`, an egress-capture test drives `read` against all six spellings in BR-3 and asserts every one produces `privacy_block`, with a positive control asserting non-boundary content IS present in the captured payload (so the zero-leak claim cannot be vacuous). (informed by LESSON-479, LESSON-502)
- [ ] AC-2: The same six-spelling matrix is applied to `edit`, and each spelling is blocked.
- [ ] AC-3: A test creates an in-repo symlink pointing at a boundary-protected file; `read`, `edit`, `grep`, and `glob` each either refuse it or attribute it to the resolved target, and no captured payload contains the protected bytes.
- [ ] AC-4: A test creates an in-repo symlink pointing OUTSIDE the repo root; `grep` and `glob` do not surface its content and do not report it under an in-jail relative path.
- [ ] AC-5: A unit test asserts the BR-4 fail-closed rejection fires for an absolute source and for a `..`-bearing source, with no boundary configured.
- [ ] AC-6: A test asserts a not-yet-existing path routed through a symlinked directory (`link/new` where `link` resolves outside the root) is refused.
- [ ] AC-7: Mutation check — reverting provenance derivation to the raw argument in `read`, and separately in `edit`, each causes at least one test to fail. Neither tool's coverage may ride on the other's. (informed by LESSON-502)
- [ ] AC-8: A test asserts that a session tainted via a non-canonical spelling pins to the local tier, and that a subsequent model-composed `web_fetch` is refused. (informed by BUG-156)
- [ ] AC-9: The full pre-existing egress-capture, `web_lookup_egress`, and `mcp_egress` suites pass unchanged — this REQ adds coverage and must not weaken any existing assertion.
- [ ] AC-10: Tests assert the four currently-unasserted `ConfigError` variants — `UnknownDefaultProvider`, `UnknownCategoryProvider`, `UnknownTierFallback`, `WebPermissionAllowNamesOff` — are each raised for their triggering input. Verified absent at spec time: each has zero references past the `#[cfg(test)]` boundary in `crates/teton-core/src/config.rs`, while every named sibling variant has at least one.
- [ ] AC-11: A check enumerates every `ConfigError` variant constructed in `Config::validate()` and fails if any lacks an asserting test, so BR-10 holds for variants added later rather than only for today's four.

## External Dependencies

- None. No new crates, services, or APIs are required.

## Assumptions

- The repo root canonicalizes successfully at tool-context construction; a repo root that cannot be resolved already fails closed today and that behavior is retained.
- Boundary globs are authored as repo-root-relative patterns. This REQ does not introduce absolute-path globs.
- REQ id allocated with remote high-water verification (not degraded).

## Open Questions

- [ ] Should an in-repo symlink whose target is also in-repo be permitted and attributed to its target, or refused outright? Attribution is more permissive and preserves legitimate workflows; refusal is simpler to reason about. BR-5 currently specifies attribution.
- [ ] Should `provenance_rejected` (BR-4) surface as a user-visible event, or only as a daemon-internal fail-closed refusal? LESSON-505 argues audit signals that only reach daemon stderr are weak controls.
- [ ] Does any legitimate workflow depend on `read` reporting the literal argument spelling back to the model in its output text? Changing provenance does not require changing the displayed string, but the two are currently the same value.

## Out of Scope

- **Case-insensitive filesystem matching.** On APFS, `Secrets/prod.env` opens the file `secrets/**` protects. This is a real but independent defect: canonicalization preserves the on-disk spelling, so fixing this REQ does not fix it, and fixing it does not require this REQ. Track separately.
- **Changing the redaction default.** Redaction defaulting off is a deliberate product decision and is not a substitute for boundary enforcement; it detects secret shapes, not `local-only` sources.
- **`TETON_LOCAL_SCRIPT` seam gating**, socket-squatting hardening, weights-client SSRF, and unpinned CI actions — all surfaced by the same audit but independent of provenance identity.
- **Adding a `write`/`create` tool.** BR-6 closes the TOCTOU gap prospectively; introducing a mutating tool is separate work.

## Scope Note

BR-10 / AC-10 / AC-11 cover `Config::validate()` error-path coverage, which is a
different subsystem from provenance identity. It is folded in deliberately rather
than filed separately: both are fail-closed gates whose correctness was invisible
to a green test suite, the fix is four assert blocks plus an enumeration check,
and a standalone REQ would cost more ceremony than the work. It is called out
here so the coupling is a recorded decision rather than silent scope creep.

## Retrieved Context

- LESSON-432 (lesson, score 13): Provenance must derive from what a tool touches, not from an argument name
- LESSON-494 (lesson, score 11): A security gate and the client that executes the request must share one parser
- LESSON-490 (lesson, score 9): A guard that runs on an encoded form is tested against the encoder's output
- LESSON-492 (lesson, score 9): A composite guard's failure path must not discard evidence a completed pass established
- LESSON-497 (lesson, score 8): A test fixture that looks like a real credential blocks the push
- BUG-156 (bug, score 7): A session pinned local by the privacy taint backstop can fail over to a remote provider
- LESSON-501 (lesson, score 7): State carried past its creator's lifetime sheds invariants silently
- LESSON-502 (lesson, score 6): An invariant enforced at several seams needs an adversarial test at each seam
- LESSON-503 (lesson, score 6): An id must be minted at the scope that resolves it
- LESSON-504 (lesson, score 6): A gate's precondition is part of its security claim
- LESSON-505 (lesson, score 6): An audit control is judged in the adversarial case, not the honest one
- LESSON-508 (lesson, score 5): A redundant guard needs its own test precisely because it is redundant
- BUG-161 (bug, score 4): Permission request_ids collide across concurrent sessions
- BUG-162 (bug, score 4): model/confirm can be answered by any connection
- LESSON-495 (lesson, score 4): A remembered grant answers every question its key matches
