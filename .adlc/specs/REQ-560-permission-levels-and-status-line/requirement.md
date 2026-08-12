---
id: REQ-560
title: "Named permission levels and the interactive session status line"
status: complete
deployable: true
created: 2026-08-05
updated: 2026-08-12
component: "cli"
domain: "harness"
stack: ["rust", "cli", "daemon", "json-rpc"]
concerns: ["security", "privacy", "developer-experience"]
tags: ["permission-levels", "status-line", "prompt-frame", "session-scoped", "tty"]
---

## Description

Teton already has the permission *mechanism* and none of the surface.
`PermissionConfig` (`crates/tetond/src/harness/permissions.rs:36`) holds a
per-tool `Allow`/`Ask`/`Deny` table with two presets — `coding_defaults()`
(reads auto-allow, `edit` and `shell` ask) and `permissive()` — and
`SessionGrants` (`crates/teton/src/session_ui.rs:43`) remembers per-tool
allow-always / reject-always answers for the session. What is missing is a
**name** the user can see and switch: the preset is chosen at construction and
never surfaces.

Two gaps, and they compound:

1. **No named level, no way to change it.** A user who wants "stop asking me
   about every edit, but keep asking before you run a shell command" has no
   expression for that. The table supports it; nothing exposes it.
2. **No persistent indication of session state.** The framed entry prompt
   (`crates/teton/src/prompt.rs:90`) renders a rule, an input row, and a rule.
   Nothing tells the user what permission posture they are in or — once REQ-559
   lands — what reasoning effort they are spending. Both are session-wide
   settings whose current value silently changes what every subsequent turn
   does and costs.

This REQ adds four named levels as presets over the existing table, a status row
below the entry frame showing the permission level and the effort level, and the
commands to read and change them.

**Levels:**

| Level | Behavior |
|---|---|
| `guarded` | Reads auto-allow; `edit` and `shell` ask. Today's `coding_defaults()`. **Default.** |
| `edits` | `edit` auto-allows; `shell` still asks |
| `plan` | Every mutating tool denies — read/grep/glob only; produces a plan, changes nothing |
| `full` | Everything auto-allows. Today's `permissive()` |

**The load-bearing constraint, stated once and enforced twice:** `full` grants
tool *execution*; it does **not** touch egress. The privacy boundary (REQ-544
BR-1) and the session-taint pin that keeps a boundary-exposed session on the
local tier (`crates/tetond/src/runtime.rs:1167`) are unchanged by every level
including `full`. Permission level governs **which tools may run**; the boundary
governs **what leaves the machine**. These are orthogonal, and wiring them
together — even accidentally, even as a convenience — would turn REQ-544's
flagship guarantee into a setting. LESSON-432 is the precedent: the boundary
guarantee is exactly the kind of thing whose hole is invisible because the tests
that would catch it are the ones nobody wrote.

**Scoping asymmetry, deliberate:** the permission level is **session-scoped and
resets to the configured default every session**, while REQ-559's effort level
persists. An effort level that survives a restart costs money predictably. A
`full` that survives a restart removes a guardrail invisibly, in a session the
user does not remember configuring.

**The status line is a test blindfold unless it is designed not to be.** The
frame renders only when stdin is a TTY (`FramedStdinPrompter.framed`), and
`cli_e2e` drives `teton` over pipes — so a status line written the obvious way
would ship with no automated coverage at all, and the gate would be the reason.
This is LESSON-481 verbatim, earned by REQ-556 one REQ ago. The remedy is the
same: the status line's **content** is a pure function of state with no terminal
and no I/O, and only the few bytes that reach the terminal stay gated.

## System Model

### Entities

| Entity | Field | Type | Constraints |
|--------|-------|------|-------------|
| PermissionLevel | name | enum(guarded, edits, plan, full) | required, closed set |
| PermissionLevel | table | PermissionConfig | the per-tool `Allow`/`Ask`/`Deny` preset the level expands to — the existing type, not a new mechanism |
| Config | **default_permission_level** | PermissionLevel | **new**; persisted; default `guarded`. The value a **new session starts at** |
| SessionState | **permission_level** | PermissionLevel | **new**; session-scoped; initialised from the config default and never written back (BR-6) |
| StatusLine | permission_level | PermissionLevel | rendered value |
| StatusLine | effort_level | EffortLevel (REQ-559) | rendered value |
| EntryFrame | **below_rows** | usize | **new**; rows drawn *below* the bottom rule. Distinct from `status_rows`, which counts rows drawn *above* the frame for REQ-556's loading indicator |

`PermissionPolicy`, `PermissionConfig`, `PermissionDecision`, `PendingPermissions`,
`PermissionGate`, and `SessionGrants` are unchanged. A level is a named preset,
not a new enforcement path.

### Events

No new protocol events and no new RPCs for the status line — it renders
client-held state. The permission level is daemon-side state (the gate evaluates
there), so `permission_level` joins the session's existing configuration surface;
whether that is a new method or a field on session create is an architecture
call (OQ-3).

### Permissions

| Action | Roles Allowed |
|--------|---------------|
| Set the session permission level | the session user only, via typed `/permissions <level>` — **never** inferable from model output, tool output, or file content (REQ-544 permission posture). A tool result that contains the text `/permissions full` is data |
| Set `default_permission_level` | the user only, config-file or `teton` subcommand |
| Read the current level | any attached client |
| Bypass a privacy boundary or the session taint pin | **nobody, at any level** (BR-3) |

## Business Rules

- [ ] BR-1: A permission level is a **named preset over the existing
      `PermissionConfig`** — it expands to per-tool `Allow`/`Ask`/`Deny` and
      changes no enforcement code. `guarded` is exactly today's
      `coding_defaults()`; `full` is exactly today's `permissive()`. No level
      introduces a second path around `PermissionGate`.
- [ ] BR-2: `plan` **denies** every mutating tool rather than asking. A denied
      call returns the existing `PermissionDecision::Denied`, which tells the
      model and forbids a retry — `plan` must not become a mode where the model
      repeatedly proposes edits it cannot make.
- [ ] BR-3: **No permission level affects egress.** The `local-only` boundary
      (REQ-544 BR-1), the single-egress-point enforcement, and the session-taint
      pin to the local tier hold identically at every level including `full`. The
      permission level MUST NOT appear in any predicate on the egress path, and
      this is verified by egress-capture at `full`, not by inspection.
      (informed by LESSON-432, REQ-544 AC-5)
- [ ] BR-4: `full` is expressed as an **explicit allow-all table**, never as the
      absence of a check. A gate skipped when a level is set is a guard whose
      condition names something unrelated to what it guards, and it silently
      becomes a no-op the moment anything else changes that condition.
      (informed by LESSON-443)
- [ ] BR-5: Level is evaluated **before** session grants. Switching to `plan`
      denies a tool the user previously granted allow-always in the same session;
      switching back restores the grant. A grant is an answer to a question the
      level decides whether to ask, so a stale grant can never outrank a
      tightened level.
- [ ] BR-6: The permission level is **session-scoped**. `/permissions <level>`
      changes it for the current session only and persists nothing; every new
      session starts at `default_permission_level`. This is the deliberate
      asymmetry with REQ-559 BR-8, where effort persists.
- [ ] BR-7: A level change takes effect on the **next** permission evaluation and
      never retroactively resolves a permission prompt already in flight. An
      in-flight prompt is a question the user is answering; changing the level
      under it would answer it for them.
- [ ] BR-8: **The status line's content is a pure function** of
      `(permission_level, effort_level, …)` → `String`, with no terminal, no
      clock, and no I/O — unit-testable with the TTY gate out of the way. Only
      the bytes that reach the terminal are gated. Without this rule the feature
      has no verification path at all: `cli_e2e` drives pipes, and BR-9 makes the
      line invisible there. (informed by LESSON-481, REQ-556 BR-11)
- [ ] BR-9: The status line is **TTY-gated**. With the frame off (stdin not a
      terminal), no status row is emitted and the session's output stays
      byte-identical to the pre-REQ binary. Existing `cli_e2e` whole-output
      assertions pass **unmodified** — a test edited to accommodate status-line
      bytes is a violation, not an accommodation. (informed by REQ-549 BR-4,
      REQ-556 BR-2)
- [ ] BR-10: Because BR-9 hides both settings in piped use, **every value the
      status row shows has a non-visual read path**. This REQ delivers it for
      `/permissions`: bare `/permissions` prints the current level and works on a
      pipe (REQ-555 BR-9). The same guarantee for `/effort` is REQ-559 BR-9's and
      lands with that REQ — this REQ neither implements nor duplicates it. A
      setting whose only surface is a TTY row is unreadable to exactly the users
      who script.
- [ ] BR-11: The status row is drawn **below the bottom rule**, making the frame
      four rows, and a redraw strands **no** row in either direction — neither
      the status row below the frame nor REQ-556's loading indicator above it.
      Rows above and below the frame are counted independently; one count
      serving both directions strands one of them. The redraw arithmetic that
      delivers this is an architecture decision, not a requirement (see
      Assumptions).
- [ ] BR-12: All status-line output goes through the existing `Surface`/`Prompter`
      seams — no direct-to-stdout side channel — so the anticipated ratatui
      front-end inherits it by implementing the same seams. (informed by
      REQ-549 BR-6, REQ-555 BR-9, REQ-556 BR-3)
- [ ] BR-13: A status-line rendering failure is never fatal and never silent:
      a terminal too narrow for the row, or a write error, degrades to no status
      row with the session fully usable and the values still readable via BR-10.
      (informed by LESSON-447, REQ-556 BR-9)
- [ ] BR-14: `/permissions` is a row in the **existing** `COMMANDS` table, so
      `/help` lists it from the same table the dispatcher matches against and it
      cannot exist without appearing in `/help` (REQ-555 BR-7). A second name for
      it is an alias on the same row, never a second row. **`/effort`'s row
      belongs to REQ-559 (BR-9)** — this REQ renders the effort value in the
      status line and must not add, alias, or duplicate the command. (informed by
      BUG-153)
- [ ] BR-15: The permission level and its effect are described by **one
      classifier**: the level a session is in, the table it expands to, and the
      sentence a denied call returns all derive from one function. Two surfaces
      describing one permission state must not drift. (informed by LESSON-456)

## Acceptance Criteria

- [x] AC-1 *(unit)*: Each of the four levels expands to its documented table.
      `guarded`'s table is byte-equal to today's `coding_defaults()` and `full`'s
      to today's `permissive()`, asserted against the existing constructors so a
      drift in either is caught.
- [x] AC-2 *(piped)*: In a scripted session at `guarded`, an `edit` prompts; after
      `/permissions edits`, the next `edit` runs without prompting and a `shell`
      still prompts; after `/permissions plan`, both are denied and the denial
      reaches the model as `Denied`. All three legs in one test.
- [x] AC-3 *(piped)*: Grant precedence — allow-always a tool at `guarded`, switch
      to `plan`, the tool is denied; switch back to `guarded`, the grant applies
      again without re-prompting. (BR-5)
- [x] AC-4 *(egress-capture)*: At `full`, a session touching a `local-only`
      boundary produces **zero** remote calls containing boundary content and
      still emits `privacy_block`; a session tainted by unknown-provenance
      results stays pinned to the local tier at `full`. This is the criterion
      that keeps the levels orthogonal to the boundary, and it is not satisfiable
      by code inspection. (BR-3)
- [x] AC-5 *(unit)*: A source-level assertion that no egress-path predicate
      references the permission level — the same shape as REQ-544's
      enumerate-every-tool posture, so a future call site cannot quietly couple
      them. (BR-3, BR-4)
- [x] AC-6 *(piped)*: `/permissions full`, then a full daemon restart and a fresh
      session, starts at `guarded` — the level did **not** persist. Paired with
      REQ-559 AC-7's contrast case, which asserts effort *did*. (BR-6)
- [x] AC-7 *(unit)*: The status-line content function returns the expected string
      for each (level × effort) pair with no terminal involved, including the
      REQ-559 BR-6 "not applicable" effort rendering for a local-only session.
      (BR-8)
- [x] AC-8 *(piped)*: Non-TTY invocation is byte-identical to the pre-REQ binary;
      the existing `cli_e2e` whole-output tests and the
      `/quit`-equals-Ctrl-D equivalence tests pass **unmodified**. (BR-9)
- [x] AC-9 *(piped)*: Bare `/permissions` prints the current level on a pipe.
      When REQ-559 has landed, the same test covers bare `/effort`; until then
      its absence is not a gap in this REQ. (BR-10)
- [x] AC-10 *(pty)*: At a real terminal the status row renders below the bottom
      rule, a typed line is accepted intact with the frame uncorrupted, and a
      REQ-556 loading indicator drawn above the frame at the same time leaves
      neither row stranded after a redraw. This is the criterion BR-11 exists
      for and it cannot be reached on a pipe. (BR-11)
- [x] AC-11 *(piped)*: `/help` lists `/permissions` from the dispatch table; the
      BR-8 bidirectional table test from REQ-555 still passes with the new row.
      `/effort`'s row is REQ-559's and is covered there. (BR-14)
- [x] AC-12 *(unit)*: A simulated narrow terminal / write failure produces no
      status row, no panic, and a usable session. (BR-13)
- [x] AC-13: Verified on **both** macOS and Linux in CI — TTY detection and
      terminal-width handling are platform-specific and a green macOS run is not
      evidence about Linux. (informed by LESSON-433, REQ-556 AC-7)
- [x] AC-14: Mutation check — freezing the status-line content function to a
      constant, removing the level-before-grants ordering (BR-5), or making
      `full` skip the gate rather than allow-all (BR-4), each makes at least one
      test red. A suite that stays green with the feature disabled has not tested
      it. (informed by LESSON-441, LESSON-481)
- [x] AC-15 *(piped)*: An in-flight permission prompt is **not** resolved by a
      level change. With a prompt pending on `shell`, a `/permissions full`
      arriving before the answer leaves the prompt pending and still awaiting the
      user; the user's own answer decides that call, and the *next* `shell`
      evaluates at `full`. The inverse leg — pending prompt, then `/permissions
      plan` — likewise does not auto-deny the in-flight call. (BR-7)
- [x] AC-16 *(unit)*: A source-level assertion that no status-line write reaches
      stdout outside the `Surface`/`Prompter` seams — the same shape as AC-5's
      egress-predicate assertion, so a future direct-to-stdout call site cannot
      quietly bypass the seam the ratatui front-end will implement. (BR-12)
- [x] AC-17 *(unit)*: One classifier — the level a session is in, the
      `PermissionConfig` it expands to, and the sentence a denied call returns
      all derive from a single function, asserted by calling that function rather
      than by comparing two rendered strings. Adding a fifth level in the test
      exercises every surface without touching a second table. (BR-15)

## External Dependencies

- **REQ-559** for the effort value the status line renders **and for the
  `/effort` command itself** (REQ-559 BR-9 owns the `COMMANDS` row; this REQ owns
  `/permissions`). The permission half of this REQ is independently shippable;
  the status row can land showing only the permission level and gain the effort
  field when REQ-559 does. Named here so the sequencing is a decision rather than
  a discovery.
- **A PTY test harness** — already a dev-dependency from REQ-556, reused by
  AC-10. No new crate expected.
- No runtime dependencies. `PermissionConfig`, `PermissionGate`, `SessionGrants`,
  the `Surface`/`Prompter` seams, the `COMMANDS` table, and TTY/width detection
  all exist.

## Assumptions

- The four levels cover the real postures. `edits` is the one most users will
  live in; `plan` is the one that justifies its own row rather than being
  "`guarded` and decline everything", because a denial the model is told about
  produces a plan while a prompt storm produces frustration.
- `PermissionConfig`'s per-tool granularity is sufficient to express all four
  levels without a new policy variant. **To be verified at architecture time**
  against the full tool set including MCP tools (ADR-003), whose names are
  server-supplied — a level that enumerates tool names by hand cannot cover them,
  so levels likely need a mutating/read-only classification rather than a name
  list.
- REQ-556's `status_rows` mechanism counts only rows above the frame, so BR-11's
  separate below-frame count is an addition rather than a change in meaning.
  Verified against `erase`'s doc comment, which states the above-frame intent
  explicitly.
- The likely shape of BR-11's arithmetic — recorded here as a starting point for
  `/architect`, not as a requirement: `draw`'s cursor-up count and `erase`'s
  upward count are a **matched pair** and the existing code says so, so a
  below-frame row has to move both together. Any design satisfying BR-11's
  no-stranded-row property is acceptable; this one is merely the one the current
  code suggests.
- `erase`'s `\x1b[J` (erase to end of screen) already clears anything drawn below
  the cursor, so the status row is removed by today's erase without a new escape
  — only the cursor arithmetic in `draw` changes. To be confirmed empirically at
  a real terminal, not assumed from the escape's documentation.
- The permission level is daemon-side state because the gate evaluates there; the
  client renders it. This keeps the surface-parity rule (REQ-544 BR-4) — a client
  crash loses nothing the daemon holds.
- id allocated with remote verification (no degradation warning from the
  allocator).

## Open Questions

- [ ] OQ-1: Does `full` require a confirmation to enter, the way REQ-547 BR-3
      requires one for an above-RAM-floor model pick? It is the level that stops
      asking about `shell`. A confirmation is friction on a mode power users
      will want; no confirmation makes a single typo consequential.
- [ ] OQ-2: How do MCP tools (ADR-003) classify into levels? Their names and
      descriptions are server-supplied and untrusted (ADR-009's residual), so a
      level cannot enumerate them by name. Options: default every MCP tool to the
      `shell` treatment at each level, or require per-server declaration. This is
      the question the Assumptions entry above turns into work.
- [ ] OQ-3: Is the permission level carried on session create, a new
      `session/configure` method, or a field on an existing method? Affects
      whether a second attached client sees a level change made in the first.
- [ ] OQ-4: Should the status line show anything else — the active model, the
      session's running cost, the boundary count? Each is a real candidate and
      each widens this REQ. Leaning: exactly two values in v1, because the row
      has to stay readable at 80 columns.
- [ ] OQ-5: What does the status row do at a terminal narrower than the rendered
      content — truncate, drop the effort half, or drop the row (BR-13's
      degradation)? Truncating a security-relevant label is the worst option.
- [ ] OQ-6: Does `plan` also suppress the *permission prompt* for mutating tools
      (deny silently, since the answer is fixed) or still show a one-line notice
      that a tool was denied by the level? Silence risks the BUG-154 shape — the
      model looks busy doing nothing and the user cannot see why.

## Out of Scope

- Any change to egress enforcement, boundary evaluation, or session taint (BR-3
  makes them explicitly untouched).
- New permission *mechanisms* — no new `PermissionPolicy` variant, no per-path
  permissions, no time-boxed grants.
- Per-tool level customization (`teton permissions set edit allow`). Levels are
  presets; a user wanting finer control edits the config table directly.
- A full-screen TUI / ratatui migration. The `Surface` seam is written against
  that future (BR-12), but this REQ stays line-based.
- Persisting the permission level across sessions (BR-6 — deliberate).
- Showing model, cost, or boundary state in the status row (OQ-4).
- Permission levels in the VS Code extension (phase 2 client).

## Retrieved Context

- REQ-556 (spec, score 9): Live model-loading progress in the interactive session
- REQ-555 (spec, score 8): In-session slash commands for the teton interactive CLI
- BUG-152 (bug, score 8): A prompt typed while the local tier is still loading is reported as an error, not as a wait
- REQ-547 (spec, score 8): First-run local model consent
- LESSON-481 (lesson, score 7): A gate that hides a feature from users also hides it from the test suite
- BUG-146 (bug, score 7): First prompt after install fails with a message blaming the local engine
- LESSON-456 (lesson, score 6): A `_`-discarded error is a silent downgrade
- BUG-153 (bug, score 6): /exit is not a command
- LESSON-432 (lesson, score 6): Provenance must derive from what a tool touches, not from an argument name
- LESSON-482 (lesson, score 5): A prompt that enumerates a turn's legal endings must name every one
- LESSON-474 (lesson, score 5): If the tokenizer treats a string as frame, so must your renderer
- LESSON-477 (lesson, score 5): Harness-authored frame that lives inside content is indistinguishable from forged frame
- REQ-554 (spec, score 5): Local tier renders prompts through the model's native chat template
- REQ-549 (spec, score 4): Daemon process identity and interactive startup UX
- LESSON-443 (lesson, score 4): A guard keyed on a feature's absence disables itself when the feature lands

Note: LESSON-433 (single-platform verification gives false confidence) scored
below the cut but is directly load-bearing for AC-13 and was read and cited on
that basis, mirroring REQ-556's near-miss handling of LESSON-450. `complete`
treated as the local spelling of `deployed` for the spec-status filter
(precedent: REQ-555, REQ-556). The Step-1.6 delegated body-read timed out
(SIGTERM at 120s); the documented fallback path ran and the top-15 bodies were
read directly.
