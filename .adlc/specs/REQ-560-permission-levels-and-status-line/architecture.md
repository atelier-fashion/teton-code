# REQ-560 — Architecture

Named permission levels over the existing `PermissionConfig`, a session-scoped
level the user can read and change, and a TTY-gated status row below the entry
frame.

The whole design turns on one sentence: **a level is a named preset, and the
preset is produced by exactly one function.** Everything else — the RPC, the
command, the status row, the denial sentence — is a caller of that function or a
renderer of its input.

## Where the code already is

| Concern | Today | File |
|---|---|---|
| Per-tool policy table | `PermissionConfig { default, per_tool }`, presets `coding_defaults()` / `permissive()` | `crates/tetond/src/harness/permissions.rs:102-197` |
| Decision path | `PermissionGate::decide` — **grants first, then policy**, then prompt | `crates/tetond/src/harness/permissions.rs:466-520` |
| In-flight prompts | `PendingPermissions` (daemon-wide, `RequestId` → `Waiter`) | `crates/tetond/src/harness/permissions.rs:230-240` |
| Session gate registry | `session_gates: Mutex<HashMap<SessionId, Arc<PermissionGate>>>`, built once per session by `permission_gate_for` | `crates/tetond/src/runtime.rs:1222, 2635-2665` |
| Denial → model | `"Permission denied: the user declined \`{name}\`. Do not retry…"` | `crates/tetond/src/harness/turn_loop.rs:670-681` |
| Entry frame | `draw` writes 3 rows + `\x1b[2A`; `erase(status_rows)` goes up `1 + status_rows` then `\x1b[J` | `crates/teton/src/prompt.rs:90-148` |
| Above-frame rows | `paint_status` → `[web?][indicator]`, count returned to `erase` | `crates/teton/src/main.rs:405-469` |
| Command table | `COMMANDS: &[CommandSpec]` — dispatch and `/help` from one array | `crates/teton/src/slash.rs:173-260` |
| Session-scoped user-only state change | `web/override` RPC → `WebTaintOverride` | `crates/teton-protocol/src/methods.rs:1162-1188`, `runtime.rs:613-647` |
| Source-scan test helpers | `call_sites::scan::{production_sources, code_only, count}` | `crates/tetond/src/call_sites.rs:101-235` |
| PTY harness | `crates/teton/tests/pty_e2e.rs`, incl. an existing above-frame status-row test | `pty_e2e.rs:315` |

Two facts from that table drive the design:

1. `permissions.rs:174` already anticipates this REQ — *"it is where REQ-560's
   named permission levels will attach when they land: a level names a set of
   tiers, which is the shape this already reads."*
2. `decide` checks **grants before policy**. BR-5 requires the opposite. That
   ordering flip is the single behavioural change in the enforcement path, and it
   is the one thing in this REQ that can regress existing sessions. ADR-C owns it.

## ADR-A: A level is a `default` policy plus a closed read-only allowlist — never a list of mutating tool names

**Decision.** Each level is expressed as a `PermissionConfig` whose `default`
policy carries the level's posture, with explicit rows only for the closed,
first-party **read-only** set (`read`, `glob`, `grep`) and for the web keys where
an existing constructor already pins them.

| Level | `default` | Explicit rows |
|---|---|---|
| `guarded` | `Ask` | `read`/`glob`/`grep` → `Allow`; `edit` → `Ask`; `shell` → `Ask` |
| `edits` | `Ask` | `read`/`glob`/`grep` → `Allow`; **`edit` → `Allow`**; `shell` → `Ask` |
| `plan` | **`Deny`** | `read`/`glob`/`grep` → `Allow` |
| `full` | `Allow` | the three `WEB_PERMISSION_KEYS` → `Ask` |

**This is the answer to OQ-2.** MCP tool names are server-supplied and untrusted
(ADR-003/ADR-009), so no level may enumerate them. It does not have to: every
name a level does *not* mention falls to `default`, and `default` is the level's
classification of *unknown* tools. An MCP tool therefore asks at `guarded` and
`edits`, **denies at `plan`**, and allows at `full` — fail-closed for the level
whose entire point is that nothing changes. The read-only allowlist is safe to
enumerate precisely because it is first-party and closed; the mutating side is
the open set and is never enumerated.

**Rationale.** The spec's Assumptions flagged that "levels likely need a
mutating/read-only classification rather than a name list." A `default` policy is
that classification, already present in the type, and it inverts the risk: adding
a tool to the tree without touching this table gets the *conservative* treatment
at every level rather than being silently unclassified.

**Consequences.** `guarded` and `full` are byte-equal to today's
`coding_defaults()` and `permissive()` by construction (BR-1). A new first-party
read-only tool must be added to the allowlist or it will merely ask — a
degradation, not a hole.

## ADR-B: One classifier, split by what each half needs to know

**Decision.** The level's **identity** lives in `teton-protocol`
(`crates/teton-protocol/src/permissions.rs`); the level's **table** lives in
`tetond`. One enum, two total functions over it, no second table anywhere.

```rust
// teton-protocol — crosses the wire, and the client renders from it
pub enum PermissionLevel { Guarded, Edits, Plan, Full }
impl PermissionLevel {
    pub const ALL: &'static [Self];
    pub fn name(self) -> &'static str;              // "guarded" | "edits" | "plan" | "full"
    pub fn parse(s: &str) -> Option<Self>;          // exact, lowercase, closed set
    pub fn summary(self) -> &'static str;           // one line, used by /permissions
    pub fn denial_sentence(self, tool: &str) -> String;
}

// tetond — needs PermissionConfig, which is daemon enforcement state
pub fn table_for(level: PermissionLevel) -> PermissionConfig;   // THE classifier (ADR-A)
```

`PermissionConfig` deliberately does **not** move to the protocol crate: it is
what the gate enforces, not what the wire carries, and putting an enforcement
table on the wire would invite a client to send one.

`coding_defaults()` and `permissive()` become one-line delegations to
`table_for(Guarded)` / `table_for(Full)`. That keeps ~25 existing call sites
untouched **and** makes drift impossible — there is no second table left to drift
*from*. AC-1's drift check is therefore written as a characterization test that
spells the expected rows out literally in the test body, so it fails if the one
table changes; asserting `Guarded.table() == coding_defaults()` after the
delegation would be a tautology and is explicitly not what AC-1 gets.

**This is BR-15 and AC-17.** The level a session is in, the table it expands to,
and the sentence a denied call returns are `name()`, `table_for()` and
`denial_sentence()` — three total functions over one enum, each an exhaustive
`match`. AC-17 iterates `PermissionLevel::ALL` and asserts every surface answers
for every variant, so a fifth variant is covered by the existing test the moment
it is added, without editing a second table. (Rust cannot literally add a variant
inside a test; iterating `ALL` with exhaustive matches is the enforceable form of
that intent, and the exhaustive `match` is what makes the compiler the enforcer.)

**`denial_sentence` is shared for a reason.** The daemon writes it into the tool
result the model reads (BR-2), and the client renders it to the user (OQ-6). Two
crates, one function — which is what AC-17 means by "asserted by calling that
function rather than by comparing two rendered strings."

## ADR-C: Level before grants, and the web overlay may only relax an `Ask`

**Decision.** `PermissionGate::decide` consults the level's table **first**:

```
policy = table_for(level).policy_for(key)      // level: read under a short lock
match policy {
    Allow => Allowed,                          // grant not consulted
    Deny  => Denied,                           // grant cannot outrank a tightened level
    Ask   => { session grant, if any; else prompt }   // the grant answers the question the level asked
}
```

This is BR-5, and it inverts today's order (`permissions.rs:477-490`). The
in-flight-prompt property (BR-7) falls out **structurally**: the level is read
once at the top of `decide` and never again; the only thing that resolves the
`oneshot` is `PendingPermissions::resolve`, and nothing on the level-change path
touches `pending`. A level change during an open prompt therefore cannot resolve
it in either direction — AC-15's two legs are assertions about a property the
code has by construction, not about a guard someone remembered to write.

**The consequence worth stating plainly:** an `Allow` from the level now
supersedes a `RejectAlways` grant. That is deliberate. `/permissions full` is a
typed act by the same user, later in time than the grant, and a level change that
silently did not do what it said is the BR-4 shape — a guard whose condition names
something unrelated to what it guards. The web keys are unaffected: they stay
`Ask` at every level (`full` inherits `permissive()`'s explicit rows), so
REQ-563's capability-refusal semantics and the `remembered()` cache-path read are
untouched.

**The overlay rule.** `apply_web_permission` is narrowed to upgrade a key only
when its current policy is `Ask`; it never turns a `Deny` into an `Allow`.
Without this, a machine with `[web] permission_allow` in config would punch
straight through `plan` — a config file silently overruling the level whose
entire promise is that nothing leaves. Today's behaviour is unchanged in every
pre-existing case, because every level except `plan` leaves the web keys at
`Ask`.

**Gate shape.** `PermissionGate.config: PermissionConfig` becomes
`level: Mutex<PermissionLevel>` + `web_allow: Vec<WebTier>`, and `decide` builds
the table per decision. A `PermissionConfig` is a five-entry `HashMap` and a
permission decision happens once per tool call, so rebuilding it is free relative
to the tool call it gates — and it removes the only place a stale cached table
could survive a level change.

## ADR-D: `session/permissions` — one RPC that both reads and sets (OQ-3)

**Decision.** A new method on the session's existing configuration surface,
modelled on `web/override` (the established shape for a user-only, session-scoped
state change issued by a client):

```rust
pub struct SessionPermissionsParams { session_id: SessionId, level: Option<PermissionLevel> }
pub struct SessionPermissionsResult { level: PermissionLevel, changed: bool }
const METHOD: &str = "session/permissions";
```

`level: None` reads; `Some(l)` sets. The result always carries the current level,
so the client has one call site for both `/permissions` and `/permissions <l>`,
and a set can never leave the client's rendered value out of step with the
daemon's.

**Why a method and not a field on session create:** the level is mutable
mid-session, and it is daemon-side state, so a second attached client observes a
change made in the first — which is the surface-parity rule (REQ-544 BR-4)
working as intended. A create-time field could not express a change at all.

**Why this does not violate "no new RPCs":** the spec forbids new RPCs *for the
status line*, which renders client-held state and adds none. The level explicitly
"joins the session's existing configuration surface," with the method-vs-field
choice left to architecture as OQ-3.

**Structural containment.** Like `web/override`, this is a client RPC and not a
harness tool, and that placement *is* the enforcement: tool dispatch and the RPC
surface are distinct channels, so a model emitting a tool call named
`session/permissions` — or tool output containing the text `/permissions full` —
reaches nothing. That is the spec's Permissions-table row ("a tool result that
contains the text `/permissions full` is data") satisfied by construction rather
than by filtering.

## ADR-E: The status row is content-first; only four bytes of geometry are TTY-gated

**Decision.** Split into a pure function and a placement:

```rust
// crates/teton/src/status.rs — no terminal, no clock, no I/O
pub fn status_line(level: PermissionLevel, effort: Option<&str>, width: usize) -> Option<String>;
```

`None` means "draw no row" — the single degradation path for BR-13/OQ-5. It is
returned when the rendered content does not fit `width`. **Truncation is never
an option:** a clipped security label (`permissions: fu`) is worse than no label,
and BR-10 guarantees the value is still readable via bare `/permissions`.

`effort: None` renders the permission field alone. **REQ-559 has not landed on
this REQ's base** (no `EffortLevel` exists in the tree), so the row ships
permission-only and gains its second field when REQ-559 does — which the spec
explicitly permits. **This REQ adds no `/effort` row, alias, or handler**
(BR-14); the parameter is the seam REQ-559 fills.

**Placement (BR-11).** The row is drawn *below* the bottom rule by
`FramedStdinPrompter`, making the frame four rows:

```
[web?] [indicator]        ← above-frame rows, counted by `status_rows` (REQ-556)
────────────────────      ← top rule
> _                       ← input row, cursor
────────────────────      ← bottom rule
permissions: guarded      ← NEW, counted by `below_rows`
```

The two counts are **independent**, which is exactly BR-11's requirement, and the
arithmetic that follows is small:

| Operation | Today | With a below-row |
|---|---|---|
| `draw` cursor-up | `\x1b[2A` | `\x1b[{2 + below_rows}A` |
| `erase` cursor-up | `1 + status_rows` | **unchanged** |
| `read_line`, Enter | 1 newline | `1 + below_rows` newlines |
| `read_line`, EOF | 2 newlines | `2 + below_rows` newlines |

`erase` needs no change because `\x1b[J` erases from the cursor *to the end of
the screen*, which already includes everything below the frame. This is the
spec's Assumption, and it is confirmed by reading the escape's semantics — AC-10
confirms it empirically at a real terminal, which is where an assumption about
terminals belongs.

`read_line` **does** need the change, and it is the stranding hazard the spec
names: after Enter the cursor lands on the bottom rule, so today's single newline
would put the next output on top of the status row, leaving a partially
overwritten row behind. `below_rows` is stored on the prompter by `draw` and read
by `read_line`, so the pair cannot drift — the count is written once, by the code
that drew the rows, and every consumer reads that field.

**BR-9/AC-8 by construction:** every one of those writes is inside
`if self.framed`. With stdin not a terminal, `draw` returns at its first line and
no status byte is produced, so `cli_e2e`'s whole-output assertions pass
unmodified. A test edited to accommodate status-line bytes would be a violation,
and none is.

**BR-12/AC-16:** the row is written by `FramedStdinPrompter::draw` — the
`Prompter` seam — and by nothing else. AC-16 asserts this by scanning the
client's own production source for `print!`/`println!`/`stdout()` in
status-rendering code, mirroring `call_sites::scan`'s shape.

## ADR-F: `full` needs no confirmation (OQ-1)

**Decision.** `/permissions full` takes effect on the word alone.

**Rationale.** REQ-547 requires a confirmation for an above-RAM-floor model pick
because that decision is *irreversible in practice* (a multi-GB download) and
*invisible after the fact*. A permission level is the opposite on both axes: it
is session-scoped (BR-6), reversed by typing four more characters, and — this is
the load-bearing part — **continuously visible in the status row this same REQ
adds**. The status row is the compensating control that makes a confirmation
redundant; shipping the row and the prompt would be paying twice.

**Residual, recorded rather than dismissed:** on a pipe, BR-9 hides the row, so a
scripted `/permissions full` has no persistent indicator. BR-10's bare
`/permissions` is the read path there, and BR-6 bounds the blast radius to one
session. `full` still does not touch egress (BR-3), which is the guarantee that
makes a mistyped level a productivity event rather than a privacy one.

## ADR-G: `plan` denies visibly (OQ-6)

**Decision.** A `plan` denial renders the level's `denial_sentence` to the user,
not just to the model.

A denied tool already renders `{title} [failed]` through
`SessionUpdatePayload::ToolCallUpdate` (`session_ui.rs:829-842`) — visible, but
with no reason, which is the BUG-154 shape: the model looks busy doing nothing
and the user cannot see why. Because the client knows the session level (it
renders it) and `denial_sentence` lives in `teton-protocol`, the client appends
the reason from **the same function** the daemon wrote into the tool result. No
new event, no second sentence, no drift.

## Orthogonality to egress — the part that is verified, not asserted

BR-3 is the requirement this REQ could most easily break silently. Three
independent things hold it:

1. **Structural.** The permission level lives on `PermissionGate` and reaches
   only `decide`. The egress choke point takes a `TaintView` and does not depend
   on the daemon runtime at all (`runtime.rs:649-653`), so there is no handle
   through which a level *could* reach an egress predicate.
2. **AC-5, source-level.** A scan over the daemon's production sources asserts no
   file under `src/egress/` mentions `PermissionLevel` or `permission_level`,
   reusing `call_sites::scan::{production_sources, code_only}` so the
   "production source only" rule has one spelling (the reason that helper is
   shared).
3. **AC-4, behavioural.** Egress-capture at `full`: zero remote calls containing
   boundary content, `privacy_block` still emitted, and a session tainted by
   unknown-provenance results still pinned to the local tier. This is the one
   that cannot be satisfied by inspection, and LESSON-432 is why it is required.

`full` is an explicit allow-all **table** (`default: Allow`), never a skipped
gate — BR-4. Every call still runs `decide`; at `full` it returns `Allowed` from
the policy arm. There is no `if level == Full { skip }` anywhere, which is what
keeps the gate from silently becoming a no-op when something else changes that
condition (LESSON-443).

## Task graph

Sequential by data dependency — each task compiles and tests green on its own.

```
001 (protocol enum) → 002 (classifier) → 003 (gate ordering) → 004 (config + RPC)
  → 005 (/permissions command) → 006 (status row) → 007 (source assertions)
  → 008 (integration/e2e/mutation) → 009 (docs + CI)
```

| Task | Delivers | ACs |
|---|---|---|
| TASK-001 | `PermissionLevel` in `teton-protocol` | AC-17 (part) |
| TASK-002 | `table_for`, delegating presets, overlay rule | AC-1, AC-17 |
| TASK-003 | level-before-grants, `denial_sentence` wiring | AC-2 (unit), AC-3, BR-7 |
| TASK-004 | `default_permission_level` config + `session/permissions` RPC | AC-6 |
| TASK-005 | `/permissions` COMMANDS row | AC-9, AC-11 |
| TASK-006 | status-line content fn + below-frame row | AC-7, AC-12 |
| TASK-007 | AC-5 + AC-16 source assertions | AC-5, AC-16 |
| TASK-008 | piped / egress-capture / pty / mutation | AC-2, AC-3, AC-4, AC-6, AC-8, AC-10, AC-14, AC-15 |
| TASK-009 | docs, CI two-platform confirmation | AC-13 |

## Risks

- **The ordering flip (ADR-C) is the regression surface.** An `Allow` policy now
  beats a `RejectAlways` grant. Mitigated by: web keys stay `Ask` at every level,
  and the existing `web_consent_matrix` / `web_lookup_egress` suites exercise
  precisely that interaction. If any of them goes red, the flip is wrong and not
  the test.
- **BUG-159** — `call_sites.rs` and `harness/duty.rs` read production source with
  `.expect("readable source file")`. TASK-007 adds two more scanners of that
  shape. Do not run the suite concurrently with an edit to `src/`; that panic is
  BUG-159, not a failure of this REQ.
- **AC-10 needs a real terminal.** `pty_e2e.rs` exists and already has an
  above-frame status-row test to model on. If the empirical check contradicts the
  `\x1b[J` assumption, `erase` gains a below-count too — the two counts are
  already independent, so that is a local change.
