---
id: REQ-571
title: "Canonical provenance identity for privacy-boundary enforcement"
status: complete
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
is currently bypassable. The harness resolves the file it opens to a canonical
path, but `read` and `edit` tag the result's egress provenance with the **raw,
model-supplied request string** instead. The boundary matcher normalizes only a
single leading `./`, so any other spelling of the same protected file matches no
glob, carries provenance that matches nothing, and is forwarded to a remote
provider intact.

Against a `secrets/**` boundary, seven spellings of the same file were all
accepted by path resolution and **five of them evaded the boundary** — the bare
relative and `./`-prefixed forms were correctly blocked; absolute-inside-root
(`/Users/x/repo/secrets/prod.env`), repeated-`./` (`.//`, `././`), and
`..`-traversing-but-inside-root (`src/../secrets/prod.env`) were not. An in-repo
symlink reaches the same bytes under an innocuous name. The gap is reachable by
prompt injection from any file, dependency README, MCP tool result, or fetched
page — no `shell` call is needed, so the session never gains `Unknown`
provenance.

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
| provenance_rejected | A provenance source fails the fail-closed well-formedness check (absolute, or `.`/`..` segment) | Offending source, originating tool name. **User-visible** — delivered on the client protocol, not daemon stderr only (see Resolved Questions, OQ-2) |

### Permissions

Not applicable — this REQ changes no actor-facing permission. Enforcement is
unconditional and applies to every session regardless of permission level.

## Business Rules

- [ ] BR-1: Egress provenance for a tool result MUST be derived from the resolved filesystem identity of the file(s) the tool actually accessed, never from the request argument. (informed by LESSON-432, LESSON-494)
- [ ] BR-2: Provenance MUST be expressed in exactly one canonical form — repo-root-relative, `/`-separated — and boundary matching MUST operate only on that form. (informed by LESSON-494)
- [ ] BR-3: All spellings of a path that resolve to the same file MUST yield the identical `provenance_id`, and therefore identical boundary verdicts. The enumerated set is: bare relative, `./`-prefixed, repeated `./` (`.//`, `././`), absolute-inside-root, and `..`-traversing-but-inside-root. (Symlinks are governed by BR-5, not by this rule — a symlink is a different path resolving to the same file, not a spelling of one.)
- [ ] BR-4: A provenance source that is absolute, or retains a `.` or `..` segment after derivation, MUST be rejected fail-closed at the egress inspection point, independently of whether any boundary is configured, and MUST emit a `provenance_rejected` event visible to the client. This guard is redundant by design and MUST carry its own test. A malformed source pins the **whole session** to the local tier (it blocks under the boundary cause, which taints), not merely the one turn — confirmed intended (Phase 5, Brett): a source that might name a boundary file is fail-closed at session scope. (informed by LESSON-505, LESSON-508)
- [ ] BR-5: Symlink handling is split by tool class, because explicit single-file access and directory traversal have different risks:
  - `read` / `edit` (explicit, single target): a symlink resolving **inside** the repo root MUST be attributed to its resolved target, not to the link name. A symlink resolving **outside** the root MUST be refused.
  - `grep` / `glob` (traversal): symlink entries MUST be skipped entirely, regardless of where they resolve. Traversal cannot follow a link without risking cycles and duplicate results under two names, and skipping is the same posture as ripgrep's default.
- [ ] BR-6: Path containment for a not-yet-existing target MUST be decided against a resolved existing ancestor, never against a lexical path that traversed unresolved components. (informed by LESSON-494)
- [ ] BR-7: Every tool that surfaces external or file content into model context MUST be enumerated, and each MUST be covered by a boundary test. Carrying a path argument MUST NOT exempt a tool from coverage — that exemption is the specific error this REQ corrects. The enumeration MUST be an artifact that fails when a new content-surfacing tool is added without coverage, not a one-time review. (informed by LESSON-432, LESSON-502)
- [ ] BR-8: Existing boundary-matcher assertions that absolute and `..`-bearing paths match no repo-relative glob remain correct at the matcher layer and MUST be retained, but each MUST be paired with a tool-layer test proving such spellings can never reach the matcher. (informed by LESSON-502)
- [ ] BR-9: The existing session-taint pin and its failover/retry coverage (delivered by BUG-156, already resolved) MUST continue to hold for sessions tainted through a *non-canonical* spelling. This REQ adds no new pinning behavior; it ensures the existing pin is actually reached now that such spellings taint at all. (informed by BUG-156)
- [ ] BR-10: Every `ConfigError` variant that `Config::validate()` can raise MUST have a test asserting it is raised for the input that triggers it. `validate()` is fail-closed and gates daemon startup, so an unasserted variant is an unguarded startup gate. This is stated as a general rule, not a fixed list, so a newly added variant inherits the obligation. (informed by LESSON-508)
- [ ] BR-11: When a resolved path differs from the request string, `read`/`edit` output MUST show both — the request and what it resolved to — so the model is not told it read something other than what it read. Provenance and display remain separate values; only provenance governs enforcement. (see Resolved Questions, OQ-3)

## Acceptance Criteria

- [ ] AC-1: With boundary `secrets/**`, an egress-capture test drives `read` against all five spellings in BR-3 and asserts (a) every one produces `privacy_block`, and (b) every one yields the byte-identical `provenance_id` — which is what pins BR-2. A positive control asserts non-boundary content IS present in the captured payload, so the zero-leak claim cannot be vacuous. (informed by LESSON-479, LESSON-502)
- [ ] AC-2: The same five-spelling matrix is applied to `edit`, with the same two assertions.
- [ ] AC-3: A test creates an in-repo symlink pointing at a boundary-protected file. `read` and `edit` attribute it to the resolved target and the turn is blocked; no captured payload contains the protected bytes.
- [ ] AC-4: A test creates two symlinks — one resolving inside the repo root, one outside. `grep` and `glob` skip both: neither file's content is surfaced, and neither is reported under an in-jail relative path. A third case asserts a symlink *cycle* terminates the walk rather than hanging.
- [ ] AC-5: A unit test asserts the BR-4 fail-closed rejection fires for an absolute source and for a `..`-bearing source, with no boundary configured.
- [ ] AC-6: A test asserts a not-yet-existing path routed through a symlinked directory (`link/new` where `link` resolves outside the root) is refused.
- [ ] AC-7: Mutation check — reverting provenance derivation to the raw argument in `read`, and separately in `edit`, each causes at least one test to fail. Neither tool's coverage may ride on the other's. (informed by LESSON-502)
- [ ] AC-8: A test asserts that a session tainted via a non-canonical spelling reaches the existing local-tier pin, and that a subsequent model-composed `web_fetch` is refused. (informed by BUG-156)
- [ ] AC-9: The full pre-existing egress-capture, `web_lookup_egress`, and `mcp_egress` suites pass unchanged — this REQ adds coverage and must not weaken any existing assertion.
- [ ] AC-10: Tests assert the four currently-unasserted `ConfigError` variants — `UnknownDefaultProvider`, `UnknownCategoryProvider`, `UnknownTierFallback`, `WebPermissionAllowNamesOff` — are each raised for their triggering input. Verified absent at spec time: each has zero references past the `#[cfg(test)]` boundary in `crates/teton-core/src/config.rs`, while every named sibling variant has at least one.
- [ ] AC-11: A check enumerates every `ConfigError` variant constructed in `Config::validate()` and fails if any lacks an asserting test, so BR-10 holds for variants added later rather than only for today's four.
- [ ] AC-12: A test enumerates every tool that can surface external or file content into model context — at minimum `read`, `edit`, `grep`, `glob`, `shell`, `web_fetch`, and MCP tool results — and asserts each has at least one boundary test. Adding a content-surfacing tool without coverage MUST fail this test. This is the BR-7 artifact.
- [ ] AC-13: For each retained boundary-matcher assertion covered by BR-8, a corresponding tool-layer test exists proving that spelling cannot reach the matcher. The pairing is explicit — a comment or shared fixture name links the two — so neither can be deleted alone.
- [ ] AC-14: A test asserts `provenance_rejected` is delivered to a connected client, not only written to daemon logs. (informed by LESSON-505)
- [ ] AC-15: A test asserts that when a request string and its resolved path differ, `read` output contains both, and that when they match, output is unchanged from today.

## External Dependencies

- None. No new crates, services, or APIs are required.

## Assumptions

- The repo root canonicalizes successfully at tool-context construction; a repo root that cannot be resolved already fails closed today and that behavior is retained.
- Boundary globs are authored as repo-root-relative patterns. This REQ does not introduce absolute-path globs.
- Nothing currently depends on `read`/`edit` echoing the literal request string. Verified at spec time: no test asserts on the echoed path, and `with_paths` has seven call sites, all within the four tools and two egress tests.

## Resolved Questions

Recorded so the reasoning survives the decision.

- **OQ-1 — in-repo symlinks: attribute, refuse, or split?** *Split by tool class* (BR-5). Attribution falls out of BR-1/BR-2 almost for free, since provenance is already the canonical path relative to the root, whereas blanket refusal needs extra symlink-detection logic and breaks legitimate in-repo links. But traversal is different from explicit access: a walker that follows links risks cycles and reports one file under two names, so `grep`/`glob` skip them — matching ripgrep's default.
- **OQ-2 — should `provenance_rejected` be user-visible?** *Yes, on the client protocol* (BR-4, AC-14). LESSON-505: an audit signal that reaches only daemon stderr is a weak control, because the same-uid attacker it guards against can suppress or truncate it. LESSON-508: a redundant guard nobody can observe is the one that gets deleted as noise.
- **OQ-3 — should `read` echo the literal request spelling?** *Show both when they differ* (BR-11, AC-15). A model that reads through a symlink or an absolute path would otherwise be told it read something other than what it read. Display and provenance stay separate values; only provenance governs enforcement.

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
