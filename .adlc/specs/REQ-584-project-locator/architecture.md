# REQ-584 — Architecture: a project locator

Ten ADRs. Six of them close the spec's six Open Questions; the other four are
the decisions the Business Rules imply but do not name.

The through-line: **the registry is a cache of a fact the daemon already
computes.** `session_root_for` already probes every root and classifies it
(REQ-583). Recording the `project`-kind ones is bookkeeping on a value in hand,
not new I/O — which is what makes Leg A cheap and BR-3's "never a walk" the
default rather than a restraint.

---

## ADR-1 — The registry is a `teton-core` value type with a `tetond`-owned store

`teton-core::projects` holds `KnownProject`, `ProjectRegistry`, ranking and
query — all pure, no I/O. `tetond::projects` owns the file: load, prune, write,
and the scan.

**Why the split, and why this way round.** The CLI must render the same facts
(BR-9's one-renderer rule) but must never read the file (System Model
Permissions: "the CLI reads it through the daemon, never the file"). A pure
core type gives both sides one vocabulary without giving the client a path. It
also makes ranking — the part with judgement in it — unit-testable with no
temp dirs, which is the shape `session_root.rs` already uses for `classify`.

`teton` (the CLI) depends on `teton-core` already; it gains no new dependency.

## ADR-2 — The registry file is a field on `DaemonPaths`

`teton_protocol::socket_path::DaemonPaths` gains `projects: base.join("projects.json")`.

**Why not a second path computation.** `resolve_base_dir` is already the one
home for "where this daemon keeps its things", and the e2e harness already
isolates every test by setting `XDG_RUNTIME_DIR`. Deriving the registry path
anywhere else would be a second answer to a question that has one, and would
leak real state into tests — the failure mode `DaemonPaths` exists to prevent.

**Format: JSON, one document, rewritten whole.** The registry is bounded by
ADR-3's cap, so a whole-file rewrite is a few KiB. `serde_json` is already a
dependency. A log or a per-entry file would buy nothing and cost a compaction
rule.

**Permissions: inherited, not set.** The file is created inside a directory the
daemon already owns; BR-5 asks for "the state dir's permissions", which is what
a plain create in that directory gives. No explicit `chmod` — an explicit mode
here would be a second policy that could disagree with the directory's.

**A missing or corrupt file is an empty registry, never an error.** This is
metadata about convenience. A daemon that refused to start because
`projects.json` was truncated would fail closed on something that is not a
safety property. The read logs one line and continues.

## ADR-3 — Cap 128, LRU by `last_seen`, pruned at read and write

`MAX_KNOWN_PROJECTS = 128`. BR-2 requires a cap and calls its exhaustion silent.

128 is chosen against the ranking, not against memory: a machine with more than
128 live project checkouts is one where a *name query* is the only usable
surface anyway, and the entries that fall off are the least recently used — by
construction the ones a query is least likely to want. At ~200 B per entry the
file stays under 30 KiB.

Pruning at **both** read and write (BR-2) is deliberate redundancy: a read-time
prune keeps a deleted checkout out of the very next result even when nothing
has written since, and a write-time prune keeps the file from growing with
corpses when the daemon is long-lived.

## ADR-4 — Recording hooks `store_session_skills`' sibling, not its call sites

BR-1 fires on `session/create` and `session/set_cwd`. Both already funnel
through one place that receives the `ProbedRoot`: `DaemonRuntime`'s root
resolution. The recording call goes **beside** the skill-registry derivation,
for the reason BUG-184 gave for putting the `block_in_place` there — a third
call site cannot forget it.

**The write runs inside the same `block_in_place`.** It is a small file write on
the connection's reader loop otherwise, which is the defect BUG-184 just fixed
one line above; repeating it here would be an odd thing to do the same day.

## ADR-5 — The scan is a separate, smaller walker; it does not reuse `visit`

BR-3 wants depth ≤ 2, the REQ-583 name sets, no symlinks, and a smaller budget.
`walk::visit` is a recursive whole-tree walker whose budget is 100,000 entries.

The scan therefore **reuses `WalkPolicy`'s name sets and the `WalkBudget`
type** but has its own two-level loop. Reusing `visit` and stopping it at depth
2 would mean threading a depth bound through a walker that has no concept of
one, for a caller that wants a fundamentally different shape (a fixed list of
roots, each read one or two levels deep). One skip-set home is what BR-11 of
REQ-583 asked for; one *walker* was never the requirement.

`ProjectScanBudget::default()` is **2,000 entries / 2 s** — an order of
magnitude under a tool walk, as BR-3 requires, and ample for eleven dev folders
two levels deep.

## ADR-6 — `projects` is cap-exempt, with its own distinct reason

BR-6 requires either an exemption with a stated rationale or mandatory-set
membership. The tool joins `CAP_EXEMPT_TOOLS` with:

> "the machine's own project list, which the cap's profiles need most: a weak
>  local model is exactly the one that answers 'where is my X repo' with a
>  disk walk, and this tool is the alternative to that walk (REQ-584 BR-6)"

That reason is **distinct** from `teton_docs`' ("self-serving product
knowledge") in the way the table's membership rule demands: docs is about
Teton, this is about the machine.

## ADR-7 — OQ-1: rank by match class, then source, then recency

Adopted as recommended. `exact > prefix > substring > path-segment`, then
`launched` before `scanned`, then `last_seen` descending, then `uses`
descending, then path ascending.

The final path tiebreak is not cosmetic: without a total order, two entries
equal on every other key would rank by hash order and the AC-6 assertion would
be platform-flaky — LESSON-540's shape, which is why REQ-585's discovery sorts
before it caps.

## ADR-8 — OQ-2: N = 5 for the notice; the environment line takes what fits

Adopted as recommended.

**The line's budget rule, stated exactly** (BR-7, AC-8). REQ-583's worst-case
project row is the ceiling both resident sweeps measure. The clause is built
by adding names one at a time, in rank order, while the *rendered whole line*
stays ≤ that row's byte length. Then:

1. names that fit → `Known projects: a, b, c (more: the projects tool; /cd <name> moves there).`
2. no name fits but the fixed pointer does → the clause without names.
3. not even that → no clause at all.

This is A-3's "shrink order", made concrete. It is expressed as a loop against
the *measured* line rather than an arithmetic budget, so the constants cannot
drift out of agreement with the sweep — the sweep measures the same function.

## ADR-9 — OQ-3, OQ-4, OQ-5, OQ-6: all adopted as recommended

- **OQ-3** — `--cwd` keeps path semantics only. A flag with shell completion
  behind it does not need the registry, and widening it would give the two
  entry points different grammars for one argument.
- **OQ-4** — no `forget`/edit path. BR-2's pruning and ADR-3's cap already
  bound the registry; a hide feature with no request behind it is speculative.
  Recorded as a follow-up trigger, not a task.
- **OQ-5** — a `local-only` boundary does not hide project names. Boundaries
  are file globs over content; a project *name* is not file content, and
  making it one is REQ-583 OQ-1's territory (boundary re-anchoring), which this
  REQ's Out of Scope already excludes. **A-1 is the honest statement of the
  residual** and stays visible in the spec.
- **OQ-6** — one platform-agnostic `$HOME`-relative table. Linux adds nothing
  the common names miss; Windows is out of scope.

## ADR-10 — `/cd` resolution is a two-reading function in `teton-core`

BR-8's rule — path reading first, registry second — lives in
`teton_core::session_root`, beside `resolve_cwd_argument`, as a widening of
that function rather than a wrapper around it.

**Why not a wrapper in the CLI.** The refusal has to name *both* readings ("no
directory `x` under the session root, and no known project named `x`"), and a
wrapper would compose that sentence from two places — the drift LESSON-529
names. Widening the existing function keeps one composer, and keeps REQ-583's
grammar table (`CwdGrammarRow`) as the single fixture both readings are proved
against: AC-9 re-runs it unchanged, which is only meaningful if it still runs
through the same entry point.

The registry reaches it as a **borrowed slice of candidates**, not as a store —
the function stays pure and the daemon stays the only thing that reads a file.

---

## Task graph

```
TASK-A (core types + ranking)  ─┬─► TASK-C (store: load/prune/write)  ─┬─► TASK-D (recording hook)
                                │                                      ├─► TASK-E (scan)
TASK-B (DaemonPaths.projects) ──┘                                      │
                                                                       ├─► TASK-F (projects tool)
                                                                       ├─► TASK-G (environment line)
                                                                       └─► TASK-H (/cd by name)
                                                                              │
                                            TASK-I (/projects + notice + hand-off line) ◄┘
                                                                              │
                                                        TASK-J (docs runbook, AC-14) ◄┘
```

Tier 1: A, B (independent).
Tier 2: C.
Tier 3: D, E, F, G, H (independent of each other; all need C).
Tier 4: I. Tier 5: J.
