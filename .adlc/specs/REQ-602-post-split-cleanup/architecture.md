# REQ-602 — Architecture

## Approach

Six independent repairs, each with its own guard so the class cannot return.
The only genuine design question is **AC-2's rule**: how a test decides whether
a `pub(crate)` item under `runtime/` is justified. Everything else is
mechanical once that is settled.

## Key Decisions

### ADR-1: The rule is the compiler, not a search

Four methods were applied to the same corpus. They disagree, and the
disagreement is the finding.

| method | items judged to need `pub(crate)` |
|---|---:|
| review sample (unstated rule) | ~24 (of "61 demotable") |
| bare name appears outside `runtime/` | 24–30 |
| qualified `crate::runtime::X` reference | 5 |
| **demote everything, build, read the errors** | **5, exactly** |

**Why the searches failed.** The bare-name grep counted **prose**:
`startup_lifecycle`, `seam_policy`, `cause_taints_the_session`,
`derive_provider_setup` and `LOCAL_ENGINE_N_CTX` appear outside the tree only
in doc comments — `// see \`runtime::startup_lifecycle\``. The qualified-path
rule fixed that and broke differently: it missed `LOCAL_ENGINE_N_CTX` (imported
on its own line, not in a group) while still counting four comment-only hits.

And every estimate that lumped `mod.rs` in with the submodules was measuring a
no-op: **`pub(super)` in `mod.rs` *is* `pub(crate)`**, because `mod.rs` is the
`runtime` module and its parent is the crate root. A third of the "corpus" could
not change.

**So the method is the definition.** Demote every `pub(crate)` under `runtime/`
to `pub(super)`; build; the errors are exactly the set that needs crate reach.
No heuristic can beat it, and three attempts at one produced three wrong
answers — the third by someone who had just corrected the second.

Result, **as planned** — every figure here was superseded by measurement and
is corrected in the block below rather than silently overwritten, because a plan
that quietly becomes a map is the failure this REQ was written to fix:

> ~~Result: **143 `pub(crate)` sites → 8**; `pub(super)` 40 → 175.~~

**Corrected at TASK-307 and again after review (2026-08-31).** Each figure now
carries its rule; the planned ones carried none, which is how "143 → 8" and
"130 → 5" both survived as long as they did.

| rule | before (`8902439`) | after |
|---|---:|---:|
| occurrences of the token `pub(crate)`, seven submodules | 131 | 5 |
| **item declarations** carrying `pub(crate)`, seven submodules | **88** | **4** |
| the same, including `mod.rs` (occurrences) | 147 | 20 |
| `pub(super)` occurrences under `runtime/` | 41 | 168 |

The surface is **four**, not eight and not five. Review asked why
`RenderedProviderSetup` was crate-wide; the answer given was "it must match its
`pub(crate)` accessor", and the accessor —
`DaemonRuntime::derive_provider_setup` in `mod.rs` — had no out-of-tree caller
either. Every hit outside `runtime/` was prose. Making the accessor private
drops the type to `pub(super)` and the build is clean. The demote-all method was
right; it had been applied to the submodules and not to the `mod.rs` item
holding one of them open. The survivors:

| item | reached from |
|---|---|
| `LOCAL_ENGINE_N_CTX` | `egress/redact.rs`, `harness/budget.rs`, `harness/compact.rs` |
| `TAINT_BY_CONTEXT`, `taint_pin_line` | `carry.rs` |
| `endpoint_query_names_a_credential` | `provider_recipes.rs`, `web_setup_catalog.rs` |
| `RenderedProviderSetup` | must match its `pub(crate)` accessor |

The three glob re-exports (`engine`, `taint`, `views`) stay `pub(crate) use`
and are now *correct rather than lazy*: with the items beneath them narrowed,
each glob carries only the crate-wide members. That is why the shape REQ-599
used was not itself the defect — the item visibilities under it were.

### ADR-2: The guard is a ratchet, not a search

The obvious guard is a test that applies ADR-1's rule. It must not be, because
ADR-1's rule is "compile the whole crate", which a unit test cannot do, and any
search-shaped approximation re-encodes the mistake — three searches, three wrong
answers.

So AC-2 ships a **ratchet**: the crate-wide surface under `runtime/` is exactly
the **four** items named in it, and a comment records how to re-derive them.
(This paragraph said "exactly 8, the five items are named" — two figures, both
superseded, neither ever asserted by the shipped test.) The corpus is
enumerated from disk rather than listed, so a module REQ-600 or REQ-603 adds is
scanned rather than silently exempt. Same
shape as `suppression_ratchet.rs` and bounded on both sides — a *drop* is as
suspicious as a climb, because it likelier means the selector stopped matching
than that the code improved.

Its non-vacuity comes from the mutation in AC-3 rather than from a floor: promote
one item and it goes red; delete one of the five and it goes red the other way.

### ADR-3: `projects/scan.rs` is on the recursion list because this REQ line put it there

AC-4's scan list gained `projects/scan.rs` during validation. That scan was
written **one commit earlier**, to repair a guard that had gone silently dead
after a rename — and it used a flat `read_dir`, reproducing the same class in
the fix for it.

The remedy was planned as a single shared helper. **It could not be one, and
saying so is part of the record**: `call_sites::scan::rust_files` is
`#[cfg(test)]`-gated inside the lib, so an integration test — which links the
lib compiled without that cfg — cannot reach it. What shipped is three in-lib
scans sharing `rust_files`, and four integration tests each carrying a
documented local copy, matching a pattern four sibling tests already used.

The property that matters is "every directory scan over `runtime/` recurses",
and that is what AC-4 asserts. One site was missed on the first pass —
`traceability_sweep.rs`'s vacuity-floor read, which AC-4 named explicitly — and
was found by adversarial review, not by the plan. It now shares that file's own
recursive walker, demonstrated by planting `runtime/nested/mod.rs` and watching
the floor's corpus go from 8 files to 9.

### ADR-4: Amend, do not re-tick

AC-6 covers three REQ-599 criteria that were ticked without evidence. The
instinct is to make them true retroactively. Two of them cannot be:

- **AC-11** ("each commit green in CI") is a claim about history. Two
  `macos-latest` runs were cancelled and no present action changes that. It is
  amended to what happened, with the cancellation cause recorded.
- **AC-4**'s module-ownership clause describes a check ADR-5 of REQ-599 argued
  is uncomputable. It is amended to the re-attachment property that shipped.
- **AC-6**'s scenario gap *is* fixable — the fixture can gain skill-expansion
  and consent coverage — so it is either met or narrowed, not amended away.

A criterion moved to match the result stops being a criterion; a criterion
amended with the reason is a record. The difference is whether the amendment
says what actually happened.

### ADR-5: Task order is by blast radius, not by AC number

The visibility change touches **88 declarations** across seven modules (the
"~73" here was a fourth estimate, from the qualified-path rule) and is the one
step that can break the build in a way bisect has to untangle. It goes
**first**, alone, so everything after it lands on a stable base. The doc-only
repairs (AC-6, AC-8) go last, where a mistake costs a re-read rather than a
rebuild.

## Task Graph

| task | subject | depends on |
|---|---|---|
| TASK-301 | every scan over `runtime/` recurses — seven sites, not six (AC-4) | — |
| TASK-302 | narrow the submodule surface to `pub(super)` — 88 declarations, leaving 4 (AC-1) | — |
| TASK-303 | `runtime_visibility.rs` enforcement test + mutation (AC-2, AC-3) | TASK-302 |
| TASK-304 | move the BR-7 tests, or record why they stay (AC-5) | — |
| TASK-305 | stale doc paths + a resolution check (AC-7) | TASK-301 |
| TASK-306 | amend REQ-599's AC-4/AC-6/AC-11; reconcile ADR-4 (AC-6, AC-8) | — |
| TASK-307 | final verification, figures recorded with their rules (AC-9, AC-10) | all |

301, 302, 304 and 306 are independent and can run in parallel. 303 needs the
narrowing to exist; 305 needs the recursive helper it will use; 307 needs
everything.

## Risks

- **TASK-302 is large and mechanical**, which is the shape that hides a
  demotion nobody meant (LESSON-595, twice). The compiler catches an
  over-narrowing; nothing catches an accidental *widening*, so TASK-303's test
  is what makes 302 safe rather than merely green.
- **The session-lifecycle disposition (AC-8)** may turn out to be real work
  rather than a note. If it does, it is deferred with a reason and not folded
  in — this REQ is cleanup, and a new slice is REQ-600's territory.
