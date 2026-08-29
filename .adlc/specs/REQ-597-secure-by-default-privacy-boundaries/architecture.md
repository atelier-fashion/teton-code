# REQ-597 — Architecture

## Approach

The promise is already enforced; only the *set* is empty. So this REQ adds no enforcement
machinery at all — it adds one **composition function** and then makes every existing reader
of the boundary list call it.

`Config.boundaries` keeps its meaning exactly: **the user's rows, as they appear on disk.**
The builtin set is never written into it, never persisted, and never reaches the config
writer. What changes is that the seven production sites which today read `config.boundaries`
for *enforcement or reporting* instead read `config.effective_boundaries()` — user rows
first, builtin rows appended after (BR-2.1). The single write site (`config/set`'s
`SetPrivacyBoundary`) is deliberately **not** converted; it still appends to the user's table
alone, which is what keeps AC-10 true without a special case.

That asymmetry — many readers, one writer, one composer — is the whole design. It is what
makes AC-8's "composed in exactly one place" a structural fact rather than a convention, and
it is why the config document never learns that builtins exist.

## Key decisions

### ADR-1 — One composer in `teton-core`, returning a fresh list, never mutating `Config`

`Config::effective_boundaries(&self) -> Vec<PrivacyBoundary>` is the sole composition site:

```
user rows (self.boundaries, unchanged, in declaration order)
  ++ DEFAULT_BOUNDARIES (origin = Builtin, mode = LocalOnly)     unless disable_default_boundaries
```

**Why a returned `Vec` rather than a field populated at load.** A field would put builtin rows
inside `Config`, and `Config` is the value the config *writer* diffs against. `config_doc.rs`
builds a `canonical_document(config)` and applies the delta to the user's real TOML; any
builtin row living in `Config.boundaries` would be diffed as a row the user is missing and
written to their file on the next unrelated `config/set`. AC-10 would fail through a path
nobody was looking at. Returning a fresh list makes that unrepresentable: the builtin set has
no route to the writer because it never exists inside the value the writer reads.

The cost is 13 small clones per `Egress::new`. The spec's Assumption covers matching cost; see
the Risks section for the compile-per-turn observation, which is pre-existing and unchanged in
kind.

### ADR-2 — Appended, not prepended, because the matcher is earliest-declaration-wins

`BoundaryMatcher::match_path` resolves overlaps with `.matches(path).into_iter().min()` —
the lowest index wins (`crates/teton-core/src/boundary.rs:72-79`, pinned by
`nested_globs_resolve_by_declaration_order`). Appending therefore leaves every user row
strictly ahead of every builtin, so a user row that already matches a builtin path keeps its
own mode and its own identity. That is BR-7, and it is true by construction rather than by a
tie-breaking rule written on top.

This is settled by BR-2.1 and is **not reopened here**. BR-2.2's residual — a user row may
select a *weaker mode* for a builtin path — is accepted and currently inert, because both
`BoundaryMode` arms fail closed at the egress inspector. OQ-4 is where that gets revisited if
a substituting redactor ever lands.

### ADR-3 — `origin` on both the entity and the wire type, defaulting to `User`, and skipped on serialize

BR-6 renders `config/get`'s `PrivacyBoundaryConfig`, and AC-4.1 asserts on the *governing
row's* origin — the value `match_path` returns. Both facts force `origin` onto the core
`PrivacyBoundary`, not onto a wrapper: a wrapper would either not survive the matcher or
would require the matcher to return a pair.

Two serde properties are load-bearing rather than cosmetic:

- `#[serde(default)]` on both copies. On the wire this is AC-9.1's older-daemon case. On disk
  it is every config authored before this REQ.
- `skip_serializing_if` on the `User` value. Without it, `canonical_document` emits
  `origin = "user"` into every `[[boundaries]]` table, and the next `config/set` writes those
  lines into the user's file — a direct AC-10 failure. The default must be `User` for the same
  reason: every row that can reach the writer is, by ADR-1, a user row.

### ADR-4 — Both session-start events are published where the root is already derived

`handle_session_create` (`crates/tetond/src/server.rs:3429`) already computes
`daemon.runtime.session_root_for(...).view` for the create response, and has the daemon's
config in hand. Both events are published there, session-scoped, before the response — the
placement `PhaseTransition` and `rebuild_session_skills` already use, so a client reading the
create result cannot receive the session's first event after it.

`unbounded_root_warning` fires when `root.kind` is `Home`/`FilesystemRoot` **and**
`effective_boundaries()` is empty. Note what the second half means after this REQ: the empty
set is reachable only through `disable_default_boundaries` (BR-3), so the warning is precisely
"you turned the defaults off, in a place where that matters" rather than a startup nag.
AC-4's paired case — same opt-out plus one unrelated user row, no warning — is what pins that
the condition is the empty set and not the flag.

The CLI renders it as a `LineKind::Notice`, **never verbose-gated**. BR-5 says user-visible,
and REQ-571 BR-4 is the reason: a signal that reaches only the log can be suppressed by the
party it indicts.

### ADR-5 — `boundary list` is the reporting surface; there is no `config show`

BR-6's surfaces are `teton boundary list` and the in-session `/boundary list`, which share one
body (`boundary_list_on`, `crates/teton/src/main.rs:2745`) over a single `config/get`. One
assertion covers both, and AC-9's shared-body test is what keeps that true.

The empty-set branch of that body ("no privacy boundaries configured") becomes reachable only
under the opt-out, so its sentence changes to say so — otherwise the one case it now describes
is the one case where the user most needs to be told *why* the list is empty.

## Component map

| Layer | File | Change |
|---|---|---|
| Entity | `crates/teton-core/src/entities.rs` | `BoundaryOrigin`; `origin` on `PrivacyBoundary` |
| Config | `crates/teton-core/src/config.rs` | `DEFAULT_BOUNDARIES`; `effective_boundaries()`; `disable_default_boundaries` |
| Wire | `crates/teton-protocol/src/methods.rs` | `origin` on `PrivacyBoundaryConfig` |
| Wire | `crates/teton-protocol/src/events.rs` | `UnboundedRootWarning`, `BoundaryDefaultsApplied` |
| Daemon | `crates/tetond/src/runtime.rs` | 7 read sites → `effective_boundaries()`; `config/get` carries origin |
| Daemon | `crates/tetond/src/server.rs` | publish both session-start events |
| CLI | `crates/teton/src/main.rs` | `boundary_list_on` renders origin |
| CLI | `crates/teton/src/session_ui.rs` | render both events |

The seven daemon read sites (all in `runtime.rs`): 5467 (`CarriedTurn::begin` — session taint),
6728 (MCP egress), 6868 (remote provider egress), 7342, 9067 (provider test), 9599, and 13772
(`config/get` snapshot). Site 14005/14011 (`SetPrivacyBoundary`) stays on `config.boundaries`
by design.

## Risks and accepted consequences

**The blast radius is the point, and it is large.** Every session on every machine gains 13
globs. `**/*.key` and `**/*.pem` in particular match ordinary test fixtures and generated
files, so a repo that reads one will now taint its session to the local tier and block the
content at egress. The spec's Assumption accepts exactly this trade ("a false positive with a
clear message and an opt-out, over a silent credential leak") and it is the central judgment
of the REQ, not an oversight. The remedy is named in the same breath as the block:
`disable_default_boundaries`, or a narrower user row.

**Existing tests will move.** This changes a default, and a default is what ~3,600 tests were
written against. The expected fallout is bounded to two shapes: tests that assert an empty
effective boundary set (e.g. `runtime.rs:21350`, whose premise is "no boundaries, so
`context_is_sensitive` cannot be what pins"), and tests whose fixture paths happen to match a
builtin glob. Fixing a moved test means re-establishing its *premise* — usually by setting
`disable_default_boundaries` on the fixture config — never by weakening the assertion.

**Glob semantics were verified before the list was fixed, not after.** `**/` matches zero
leading directories under `globset` with `literal_separator(true)`, so every BR-1 glob matches
at the repo root as written (`.ssh/id_rsa`, `.env`, `.aws/credentials`, …) and none matches
`src/main.rs`, `README.md`, `env`, or `notes/.envrc`. This was run against the real crate
before any code was written; it is the assumption the whole REQ rests on.

**One pre-existing cost is now paid on every turn rather than rarely.** `Egress::new` compiles
its `GlobSet` per construction, and `egress/mod.rs:793` guards on
`!self.boundaries.is_empty()` — a guard that was almost always false and is now almost always
true. Compiling 13 globs per turn is small, and this REQ does not change the *shape* of that
cost, so it is recorded rather than fixed. A shared lazily-compiled matcher is the follow-up
if measurement ever asks for one.
