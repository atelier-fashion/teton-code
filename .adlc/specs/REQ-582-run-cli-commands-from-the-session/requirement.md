---
id: REQ-582
title: "Every session-meaningful `teton` command runs from the session — no shell round-trip"
status: approved
deployable: true
created: 2026-08-18
updated: 2026-08-18
component: "cli"
domain: "clients"
stack: ["rust", "cli", "json-rpc", "daemon"]
concerns: ["developer-experience", "security"]
tags: ["slash-commands", "slash-command", "interactive-session", "repl", "help", "hand-off", "guided-enablement", "cli-parity", "session-commands", "provider-list", "policy-show", "doctor", "self-config-guide"]
---

## Description

Inside a `teton` session, most of the product's own commands are still shell
commands. The session knows `/cost`, `/effort`, `/model`, `/model set`,
`/provider setup`, `/provider test` (plus the session-only `/help`, `/clear`,
`/verbose`, `/permissions`, `/web setup|allow|refresh`, `/quit`); the CLI
also has `provider add`, `provider list`, `boundary add|list`,
`policy show|set-tier|set-category`, `model list|status`, `doctor` and
`uninstall`. So a user who asks the session about its own configuration is
sent to a second terminal — and the shipped guide
(`crates/tetond/src/harness/self_config.md`) tells the model to do exactly
that: "Inspect: `teton policy show`, `teton provider list`, `teton doctor`"
and "You cannot run these commands yourself; hand them to the user."

Observed on the user's dogfood of v0.1.20 (2026-08-18): asked "I want to test
the kimi connection", the model correctly named `/provider test <id>` (the
REQ-581 hand-off stayed quiet, as designed) but sent the user to a shell for
`teton provider list`; the user then typed `teton provider list` at the
session prompt and it went to the model as a chat message, which answered
"That one's for you to run — I can't execute `teton` commands myself. Type it
in your shell." The user's requirement, verbatim: **"The commands need to be
able to run from session, not have to go to Bash for everything."**

Three legs, one thesis — a user in a session should never have to leave it to
operate Teton:

1. **Parity.** Every CLI subcommand that is meaningful inside a session has a
   session row: `/provider list`, `/provider add`, `/boundary list`,
   `/boundary add`, `/policy show`, `/policy set-tier`, `/policy set-category`,
   `/model list`, `/model status`, `/doctor`. Each renders through the *same*
   renderer and calls the *same* daemon method as its shell twin — the way
   `/cost` reuses `teton cost`'s path and `/model set` reuses `teton model
   set`'s flow (REQ-555 BR-4/BR-4b) — so two surfaces describing one daemon
   fact cannot drift.
2. **Recognition.** A line typed at the session prompt that begins with the
   word `teton` and parses — under the CLI's *own* argument parser — as a
   real subcommand runs its session row, with one line naming the session
   spelling (`>> teton provider list → /provider list`), instead of being sent
   to the model as chat. A line that does not parse as a subcommand ("teton is
   slow today") stays a prompt exactly as today. `teton uninstall` typed in a
   session is refused with the reason and the shell pointer — it would kill
   the daemon under the session.
3. **The model's answers.** The guide names the session spellings for the
   commands that now have them, and the REQ-579/REQ-581 hand-off nudge is
   generalized: when a reply names a `teton <sub>` shell command whose session
   row exists, the session prints one line naming the `/` spelling. The
   surface nudge is the guarantee; the guide edit is the improvement
   (LESSON-532 — presence in context is not instruction-following).

REQ-555, which built the slash table, deferred exactly this in its Out of
Scope: "in-session management commands (`/provider`, `/boundary`, `/policy`)
… follow the same shared-flow pattern if promoted later." This is that
promotion, on the same pattern.

This is about **user-typed** commands. It does not make the model run `teton`
through its shell tool — REQ-581 / LESSON-535 established that the product
answers such questions first-class rather than the model improvising shell
probes, and BUG-177 showed what every model-spawned `teton …` costs the
session. The session becomes the place those answers live; the shell stays
the place scripts and CI live.

## System Model

### Entities

| Entity | Field | Type | Constraints |
|--------|-------|------|-------------|
| SlashRow (existing table, `crates/teton/src/slash.rs`) | name | string | unique among rows and aliases; may contain a space (`provider list`) |
| SlashRow | args | None / Required / Optional / **Cli** | new: the row's argument grammar is the CLI subcommand's own clap definition, parsed by the same code the binary uses |
| SlashRow | shell_twin | string | the `teton …` spelling this row mirrors; empty for session-only rows |
| SlashRow | writes | boolean | true for rows that change daemon or machine state (`provider add`, `boundary add`, `policy set-*`, `model set`) |
| CliLine (new input bucket) | tokens | string[] | a prompt line whose first token is `teton`, shell-split; parses strictly under the CLI parser (no trailing junk) |
| CliLine | row | SlashRow? | the session row it maps to; `None` for a subcommand with no row (`uninstall`, bare `teton`, `--version`) |
| HandOff (existing, generalized) | named_commands | string[] | distinct `teton <sub>` spellings a model reply named that have a session row |
| HandOff | line | string | at most one line per turn, naming the `/` spellings |

### Events

None. No daemon change: every row is a new call site of an existing method
(`config/get`, `config/set`, `model/list`, `model/status`, `model/set`,
`cost/query`, `provider/test`), and recognition is client-side classification.

### Permissions

| Action | Roles Allowed |
|--------|---------------|
| read rows (`provider list`, `boundary list`, `policy show`, `model list`, `model status`, `doctor`) | any attached session, TTY or pipe |
| write rows (`provider add`, `boundary add`, `policy set-tier`, `policy set-category`) | typed input only (TTY); on piped stdin the row rejects with the shell pointer — REQ-555 BR-9's rule for the one row that wrote, applied to all rows that write |
| write rows — daemon side | unchanged: the same `config/set` gate the shell twin meets, including presence attestation on a `presence` build (REQ-576 BR-10(b)) and the ancestry gate |
| `provider add` key entry | echo-off through the session's prompter, into the keychain; never in the transcript, the model's context, or an event (REQ-579) |
| `uninstall` | refused in-session, always |

## Business Rules

- [ ] BR-1: **One row per session-meaningful subcommand.** The slash table
      gains `provider list`, `provider add`, `boundary list`, `boundary add`,
      `policy show`, `policy set-tier`, `policy set-category`, `model list`,
      `model status`, `doctor`. `uninstall` and the bare `teton` (open a
      session) get no row. Every new row appears in `/help` because `/help`
      is generated from the table (REQ-555 BR-7); the `/help` listing groups
      the mirrored rows under their family so it stays readable at ~25 rows.
- [ ] BR-2: **One renderer, one method, per fact.** Each mirrored row renders
      through the exact function its shell twin renders through
      (`render_config`, `render_policy`, the boundary/model/doctor renderers)
      and calls the same daemon method with the same params — a shared
      function, not a re-implementation. A `/policy show` and a
      `teton policy show` against one daemon print the same lines, asserted
      by a test that drives both surfaces and diffs the bytes (informed by
      LESSON-456 via REQ-555 BR-4; LESSON-517 — the seam is the only ground
      truth for parity, pin the crossing bytes not a hand-maintained twin).
- [ ] BR-3: **One argument grammar.** A mirrored row's arguments — positionals,
      `--flags`, value enums — are parsed by the CLI's own clap definition of
      that subcommand, so `/policy set-tier build kimi --fallback local` and
      `teton policy set-tier build kimi --fallback local` are one grammar,
      one error message, one help text. There is no second hand-written
      parser of `teton …` lines anywhere in the client (informed by
      LESSON-529 — a display helper is a second parser; two parsers of one
      string drift into a lie on screen).
- [ ] BR-4: **Recognition is strict and total.** A prompt line is a
      `CliLine` iff its first shell-split token is `teton` **and** the rest
      parses under the CLI parser as a complete subcommand with valid
      arguments and nothing trailing. Then: a row exists → run it and print
      one line `>> teton <sub> → /<row>` first; no row (`uninstall`, bare
      `teton`, `--version`, `--help`) → one refusing line naming why and the
      shell pointer, never forwarded to the model (the BUG-146 shape: the
      harness was asked and something else answered — REQ-555 BR-2). Any
      other `teton…` line ("teton is slow today", "teton provider list shows
      nothing, why?") is a plain prompt, byte-identical to today. REQ-555
      BR-8's totality is amended from three buckets to four (command, CLI
      line, escaped prompt, plain prompt) and stays pinned in both
      directions: a test proves every CLI subcommand with a row is reachable
      from a `teton …` line, that every subcommand without a row is refused,
      and that non-parsing `teton…` lines reach the prompt path unchanged
      (informed by REQ-555 BR-8, LESSON-479 via it). Session-only
      commands have no `teton` form, so `teton provider setup …` typed at
      the prompt is a plain prompt — the model's own hand-off names
      `/provider setup` (REQ-579 ADR-9).
- [ ] BR-5: **A `CliLine` runs the row, not a subprocess.** Recognition never
      spawns the `teton` binary and never opens a second connection: it
      dispatches to the same handler `/<row>` dispatches to, over the
      session's already-open connection (REQ-555 D-4). Corollary: no
      recognized line prints `a CLI client attached` into its own session,
      and no `CostRecord`, no model call — local or remote — is ever made for
      one (REQ-555 BR-1).
- [ ] BR-6: **Writes stay gated exactly as before.** The write rows call the
      same daemon methods with the same params as their shell twins; every
      gate is daemon-side and unchanged (config/set presence attestation on a
      `presence` build, REQ-576 BR-10(b); `model/set`'s above-floor second
      confirmation, REQ-547 BR-3; the ancestry gate). Client-side, a write
      row is typed-input-only: under piped stdin it rejects with the shell
      pointer (REQ-555 BR-9), and `provider add`'s key is read echo-off
      through the session prompter into the keychain (REQ-579 BR-2), never
      via a command argument — a `/provider add … --key` is not a thing.
- [ ] BR-7: **`/doctor` in a session reports the same facts through the
      session's connection.** It does not dial the socket afresh (that would
      announce an attach into the very session it is diagnosing — BUG-177's
      shape) — the connect/handshake arm is replaced by "this session's
      connection: <daemon name version, protocol>", and every other line
      (socket/lock paths, config, base-URL advice, the model and provider
      notices) renders through `run_doctor`'s renderer unchanged.
- [ ] BR-8: **The hand-off nudge generalizes, and stays quiet when the model
      already said it.** When a reply names a `teton <sub>` shell command
      that has a session row and does **not** also name the `/` spelling, the
      session prints one line — at most one per turn — naming the `/`
      spellings, deduplicated: `>> in this session: /provider list, /policy
      show`. A reply that already names `/provider list` prints nothing. The
      REQ-579 (`/provider setup`) and REQ-581 (`/provider test`) hand-off
      lines keep their own sentences (each carries a reason the generic line
      does not — "no key in chat", "makes one consented call") and are never
      duplicated by the generic line on the same turn (informed by REQ-579
      ADR-9, REQ-581 ADR-4, LESSON-532; LESSON-535 — a false positive on a
      non-command turn is a finding, so the trigger is the exact `teton <sub>`
      token sequence, not a keyword).
- [ ] BR-9: **The guide says the session spelling first.** `self_config.md`
      is amended: step 3 becomes "Inspect: `/policy show`, `/provider list`,
      `/doctor` in a session (`teton policy show` … from a shell)"; step 2's
      `teton policy set-tier` gains its `/policy set-tier` twin; and the
      sentence "You cannot run these commands yourself; hand them to the user"
      stays — the user runs them, in the session. A test pins that every
      shell command the guide names which has a session row also has its `/`
      spelling in the guide (BR-7-style: the guide cannot name a mirrored
      command by only its shell form).
- [ ] BR-10: **No daemon, protocol, or wire change.** Nothing new on the
      bus, no new method, no new event, no new config key. An older `teton`
      against a newer daemon and vice versa behave exactly as they do today
      (REQ-555 BR-3).
- [ ] BR-11: **Piped stdin.** Read rows and recognized read lines work
      identically on a TTY and on a pipe (the e2e suites drive sessions
      through a pipe); write rows follow BR-6. `//` (REQ-555 BR-1b) still
      escapes a leading slash and is unaffected; a `teton …` line needs no
      escape because only a strict parse intercepts (BR-4).

## Acceptance Criteria

- [ ] AC-1: In a session, `/provider list`, `/boundary list`, `/policy show`,
      `/model list`, `/model status` and `/doctor` each print exactly the
      lines their `teton …` twin prints against the same daemon (byte-diffed
      by a test that drives both), with `/doctor`'s connect arm replaced per
      BR-7.
- [ ] AC-2: `/policy set-tier build <id>`, `/policy set-category edit <id>
      --fallback <id>` and `/boundary add <glob> --mode local-only` change the
      daemon's config exactly as their shell twins do (asserted on
      `config/get` before/after), and the next turn routes accordingly.
- [ ] AC-3: `/provider add <id> --kind openai-compatible --endpoint <url>
      --model <m>` on a TTY reads the key echo-off, stores it in the keychain,
      registers the provider, and the key appears nowhere in the transcript,
      the session's events, or the model's context (egress-capture assertion,
      REQ-579's harness).
- [ ] AC-4: On piped stdin every write row (`provider add`, `boundary add`,
      `policy set-tier`, `policy set-category`) rejects with one line naming
      the shell command; every read row works.
- [ ] AC-5: Typing `teton provider list` at the session prompt prints
      `>> teton provider list → /provider list` and then the same lines as
      `/provider list`; no model call is made (no `route_decided`, no
      `CostRecord`, no `session_update`) — asserted over the wire.
- [ ] AC-6: Typing `teton uninstall` prints one refusing line naming the
      shell pointer and makes no call; typing `teton is slow today` reaches
      the model as a prompt with the bytes unchanged; typing `teton provider
      list please` reaches the model as a prompt (strict parse).
- [ ] AC-7: `/policy set-tier build` (missing arg) and `/policy set-tier
      summit kimi` (bad enum) print the CLI parser's own error for that
      subcommand — the same text `teton policy set-tier build` prints — and
      issue no RPC.
- [ ] AC-8: `/help` lists every new row, grouped by family; the table-vs-help
      test still holds; `//` footer unchanged.
- [ ] AC-9: A scripted turn whose reply says "run `teton provider list` and
      `teton policy show`" prints exactly one line `>> in this session:
      /provider list, /policy show`; a reply that says "run `/provider list`"
      prints nothing; a reply that names `teton provider add …` prints the
      REQ-579 line and not the generic one; a reply about "the teton binary
      being slow" prints nothing.
- [ ] AC-10: The guide test pins BR-9: every `teton <sub>` the guide names
      that has a session row also appears as `/<row>` in the guide.
- [ ] AC-11: On a `presence`-featured build, `/policy set-tier build kimi`
      raises the same presence prompt `teton policy set-tier` raises and a
      cancel leaves `config.toml` byte-identical (REQ-576 AC-6 pattern, via
      the `TETON_PRESENCE_ACCEPT=fail` seam — informed by LESSON-519,
      LESSON-520: pair the refused test with an accepted one on a payload
      that would persist).
- [ ] AC-12: The workspace suite passes; no new protocol types; `git diff
      -- crates/teton-protocol/src/` is empty.

## External Dependencies

- None. `clap` is already the CLI's parser; the session reuses its
  definitions.

## Assumptions

- The CLI's `Command`/`*Action` clap tree can be parsed from a token vector
  inside the running session process (`try_parse_from`) without touching
  process argv or exiting on error — clap supports this; the architecture
  phase confirms error/help rendering goes to the session `Surface`, not
  stdout directly (REQ-549 BR-4/BR-6).
- Every mirrored subcommand's implementation in `main.rs` already separates
  "connect" from "render", or can be split so without changing its output
  (`run_provider_list`, `run_policy_show`, `run_doctor` … each open their own
  connection today) — the split is the architecture's job; the bytes are
  pinned by AC-1.
- The self-config guide is bundled at build time (`include_str!`), so BR-9 is
  a source edit and ships with the release; no daemon restart semantics
  change.
- id allocated with remote verification (not degraded).

## Open Questions

- [ ] OQ-1: Should `/teton provider list` (a leading slash on the CLI form)
      also be recognized, or stay an unknown command that names `/help`
      (REQ-555 BR-2)? Proposal: recognize it — it costs one line in the
      classifier and matches what a user who has learned "commands start
      with `/`" will type.
- [ ] OQ-2: Should the generic hand-off line (BR-8) subsume the REQ-579 and
      REQ-581 sentences into one renderer with per-command reasons, or keep
      three sentences? Proposal: one renderer, per-row optional reason
      string — the row table already owns the spelling, it can own the
      reason too; decide at architecture.
- [ ] OQ-3: `/model` today is the concise current-model line and `/model
      set` writes; adding `/model list` and `/model status` — should bare
      `/model` stay concise or become `model status`? Proposal: stay concise
      (REQ-555 BR-4 chose it deliberately); `status` is the full form.
- [ ] OQ-4: Should `teton --version` typed in-session print the client and
      daemon versions (cheap and useful after an upgrade) rather than be
      refused? Proposal: yes, as a `/version` row — but only if it costs no
      new RPC (the handshake result already carries the daemon version).

## Out of Scope

- The model running `teton …` through its shell tool, or being given a tool
  that runs session commands — the product answers first-class; the model
  hands off (LESSON-535, REQ-581 ADR-4).
- `teton uninstall` in-session; the bare `teton` (open a session) in-session.
- New daemon methods (e.g. a `provider/list` RPC replacing `config/get`
  reads), new events, protocol bumps.
- Tab completion / history for slash commands; the VS Code extension.
- Changing what any mirrored command *prints* — parity is with today's
  output, byte for byte, except `/doctor`'s connect arm (BR-7).

## Retrieved Context

- LESSON-529 (lesson, score 9): A display helper is a second parser — render the host the request will reach
- LESSON-517 (lesson, score 9): A sanitizing seam owns the styling too — and the seam is the only ground truth for parity
- LESSON-481 (lesson, score 9): A gate that hides a feature from users also hides it from the test suite — split the logic out from under the gate
- LESSON-535 (lesson, score 8): A probe is a billed call and a preview is a surface — four verify-phase catches on REQ-581 and the audit prompts they leave behind
- BUG-173 (bug, score 7): The pty suite's entry-prompt wait absorbs daemon startup, so a slow CI runner reads as a failing test
- LESSON-510 (lesson, score 7): A harness that checked a binary exists has not checked it is the one under test
- LESSON-495 (lesson, score 7): A remembered grant answers every question its key matches — so the key must encode the whole question
- BUG-177 (bug, score 6): Every client attach replays the model lifecycle into every open session
- LESSON-533 (lesson, score 6): The code is the part of the spec you did not write — read it before the task file, and again at review
- LESSON-524 (lesson, score 6): Exposure is not callability — a capability asserted present must be asserted usable at every permission level
- LESSON-518 (lesson, score 6): A blocking gate's reader-loop freedom is not inherited from the await-based reader-loop tests
- LESSON-519 (lesson, score 6): An "assert by inspection, not from the error" AC needs the real artifact — add a refusing test seam to reach it
- LESSON-520 (lesson, score 6): A gate that fires before deserialization makes an invalid-payload test vacuous — use a persistable payload + a refuse/accept pair
- LESSON-512 (lesson, score 6): A spec's named example is a test case, not decoration
- BUG-165 (bug, score 6): The search credential only speaks Bearer, and the spec's own example backends do not

(REQ-555 — the in-session slash-command spec — was read directly for its
BR-1b/BR-2/BR-4/BR-7/BR-8/BR-9 rules; it is `complete` and so outside the
retrieval's status filter.)
