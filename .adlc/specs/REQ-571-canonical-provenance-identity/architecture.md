# REQ-571 — Architecture

## Approach

The bug is not that two functions forgot to canonicalize. It is that the
provenance channel accepts `String`, so "this is a repo-relative path" is a
doc-comment rather than a fact:

```rust
// harness/tools/mod.rs
pub fn with_paths<I, S>(self, paths: I) -> Self where S: Into<String>

// harness/context.rs
pub enum ToolProvenance { Sources(BTreeSet<String>), Unknown }
//                                 ^ "repo-relative paths" — asserted in prose only
```

`grep`/`glob` happen to pass `strip_prefix(root)` output; `read`/`edit` happen to
pass the request argument. Both type-check identically. The fix therefore is not
to add a third and fourth copy of the `strip_prefix` idiom — that is the
convention that already failed — but to **close the channel** so only a resolved
identity can enter it.

This is a direct application of two patterns the project has already adopted:

- *"A gate decides on the parse the executor will use"* (REQ-563, LESSON-494) —
  stated for URLs, unapplied to filesystem paths. The executor opens
  `resolve()`'s canonical path; the gate must match on that same value's
  re-serialization, not on the original request bytes.
- *"A required field with no `Default` is how 'every call states X' is
  enforced"* (REQ-559 ADR-B, LESSON-443) — a rule of the form "no call path may
  omit X" holds only until the next call path is added. `read`/`edit` **are**
  that next call path.

## Key Decisions

### ADR-A: Provenance is a minted identity, not a string

`teton-core` gains a `ProvenanceId` newtype. It is the only type the provenance
channel accepts — `ToolProvenance::Sources(BTreeSet<ProvenanceId>)`, and
`with_paths` narrows from `S: Into<String>` to `ProvenanceId`.

Crucially, **no `From<String>` / `Into<String>` impl exists**. The type is
constructible only through named constructors, so `.with_paths([raw])` becomes a
compile error rather than a review catch.

Two constructors, both explicit at the call site:

| Constructor | For | Guarantee |
|---|---|---|
| `ProvenanceId::from_resolved(root, resolved)` | Files the daemon actually opened (`read`, `edit`, `grep`, `glob`) | Derived by `strip_prefix`, so it names the file that was read |
| `ProvenanceId::claimed(s)` | A path *asserted* by a third party (MCP tool arguments) | Best-effort. Normalized identically, but the daemon did not open it |

**Why `claimed` must exist.** `mcp.rs` derives provenance from tool arguments
under arbitrary keys — `mcp_egress.rs:428` pins that a boundary path under
`resource` (not `path`) is still caught. The daemon cannot know what a remote
MCP server touched, so that provenance is a claim, not an observation. Giving it
a separate, greppable constructor keeps the distinction visible instead of
letting one permissive `From<String>` reopen the hole for every caller. It is
*not* an escape hatch for `read`/`edit`: a reviewer seeing `claimed()` in a
first-party file tool knows immediately that it is wrong.

**Placement.** The type lives in `teton-core` because that is where boundary
matching consumes it, and construction is pure path arithmetic (`strip_prefix`,
separator normalization) with no filesystem access — so it respects the crate's
"no I/O" rule. Canonicalization stays in `tetond`, which is where the filesystem
is. This follows LESSON-503's shape — mint at the scope that resolves — with the
*validity* of the identity owned by the layer that resolves boundaries.

### ADR-B: The gate decides on the parse the executor used, and never falls back

`ToolContext::resolve` returns both halves of the identity:

```rust
pub struct Resolved { pub path: PathBuf, pub provenance: ProvenanceId }
pub fn resolve(&self, raw: &str) -> Result<Resolved, ToolError>
```

One call yields the path the tool opens *and* the id the boundary matches, so
the two cannot drift. `BoundaryMatcher::normalize`'s single-`./` strip becomes
dead weight rather than load-bearing — every id reaching it is already canonical.

**No fallback to `raw`, ever.** A `strip_prefix` failure means the resolved path
is not under the root, which is precisely the case where substituting the
attacker-supplied string is worst. That path returns `Err`, and the tool refuses.
An exploration pass recommended `.unwrap_or_else(|_| raw.clone())`; it is
rejected here for that reason and recorded so it is not re-proposed.

### ADR-C: Symlink posture splits by tool class

Explicit single-file access and directory traversal have different risks, so
they get different rules (spec BR-5):

- **`read` / `edit`** — resolve the link. Inside the root, attribute to the
  resolved target (the link name is not the identity). Outside, refuse.
- **`grep` / `glob`** — skip symlink entries entirely, wherever they resolve.
  A walker that follows links can cycle, and can surface one file twice under
  two names — two provenance ids for one identity, which ADR-A exists to
  prevent. Skipping is also ripgrep's default, so it matches user expectation.

`DirEntry::file_type()` does not traverse links, which is why a link currently
fails `is_dir()` and lands in the file branch where the read *does* follow it.
The walkers must test `is_symlink()` explicitly rather than inferring from
`is_dir()`.

### ADR-D: The malformed-provenance guard is fail-closed and reports to the client

At the egress inspection point, before boundary matching: any `ProvenanceId`
that is absolute or retains a `.`/`..` segment is refused, whether or not a
boundary is configured, and emits `provenance_rejected` on the `EventBus`.

By construction ADR-A should make this unreachable. That is the point —
LESSON-508: a redundant guard needs its own test *because* it is redundant, or
it gets deleted as noise. And LESSON-505: an audit signal that reaches only
daemon stderr is a weak control against a same-uid adversary, so it goes on the
protocol where the user can see it.

The guard is what makes `claimed()` safe: a third-party MCP server supplying an
absolute path is caught here rather than silently matching no glob.

### ADR-E: Coverage drift is a build failure, not a review item

BR-7's tool enumeration follows the precedent ADR-009 rule 3 already set for
frame markers — *"a test asserting every output marker is claimed by exactly one
input layer, so drift is a build failure rather than a silently reopened hole."*

The same shape applies here: a test enumerates every tool that can surface
external or file content and asserts each has boundary coverage. Adding a
content-surfacing tool without coverage fails the suite. This is the mechanism
that makes BR-7 more than a promise — and it is the direct answer to LESSON-432,
where the uncovered tools were exactly the vulnerable ones.

## Protocol Change

`Event::ProvenanceRejected(ProvenanceRejected)` is added to
`teton-protocol/src/events.rs` (27 variants today), with
`{ source: String, tool: String, reason: ProvenanceRejection }`.

**This is a compile-breaking change for the CLI.** The match at
`crates/teton/src/session_ui.rs:273-494` is exhaustive — verified, no wildcard
arm. The protocol variant and its `session_ui` arm therefore land in a single
task, so no intermediate commit leaves the workspace un-buildable.

Wire compatibility is unaffected: the enum is internally tagged and the addition
is purely additive, so `PROTOCOL_VERSION` (currently 2) does not move. An older
client never receives a variant it cannot parse, because an older client is
talking to an older daemon.

## Layering

No new crate dependencies. `tetond → teton-core` and `tetond → teton-protocol`
already exist; the CLI gains no dependency it lacks (project BR-4 preserved).

The single-egress-choke-point pattern is preserved and strengthened:
`ToolContext::resolve` becomes the sole mint point for file-backed identity, and
`egress::inspector` remains the sole enforcement point, now with a fail-closed
pre-check ahead of boundary matching.

## Consequences

- `with_paths` narrowing touches every tool. That is the point: the compiler
  enumerates the call sites rather than a reviewer.
- `ToolProvenance::Sources` changes its element type, so the
  `tool_result_provenance` bridge (`harness/digest.rs:95-106`) and the egress
  `Provenance` type move together.
- The four unasserted `ConfigError` variants (BR-10) are independent of all of
  the above and can be done in parallel from the start.
- `BoundaryMatcher::normalize` keeps its single-`./` strip for defence in depth,
  and its existing assertions that absolute/`..` paths match no glob are
  retained (BR-8) — now paired with tool-layer tests proving those spellings
  cannot reach it.

## Proposed addition to `.adlc/context/architecture.md`

Add to Key Patterns:

> **An identity that enforcement depends on is minted, never described** — when
> a security decision keys on a value ("this is the repo-relative path of what
> we read"), that value is a type constructible only by the code that can
> establish the claim, not a `String` whose meaning lives in a doc comment. A
> permissive conversion (`Into<String>`) means every present and future call
> site is a place the invariant can be quietly dropped, and the call site that
> drops it type-checks exactly like the one that honours it. Where a claim
> genuinely cannot be observed — a remote MCP server's asserted path — it gets
> its own named constructor so the weaker guarantee is visible at the call site
> rather than merged into the strong one (REQ-571 ADR-A, LESSON-432,
> LESSON-443).

## Amendment (Phase 4, as-landed)

The ADR-A blast-radius estimate ("seven `with_paths` call sites") was low: the
compiler surfaced `crates/tetond/src/mcp/client.rs` (`collect_paths` /
`call_provenance`) — a **production** site building egress provenance directly
from raw MCP argument strings, exactly the class of hole ADR-A closes. It now
mints via `ProvenanceId::claimed`. Recorded here per the architect-phase rule
that call sites beyond the plan feed back into the spec rather than being
quietly absorbed. Two consequences worth naming: an un-mintable claimed path
(absolute or `..`-bearing MCP argument) now taints the call `Unknown` —
fail-closed where the old code failed open, observable only when a boundary is
configured; and the spelling matrix is six-wide, not five (`.//x` and `././x`
are distinct model-emittable spellings).
