---
id: REQ-582
title: "Every session-meaningful `teton` command runs from the session — no shell round-trip"
status: complete
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
| `provider add` key entry | echo-off through the session's prompter, into the keychain; never in the transcript, the model's context, or an event (REQ-579). **In a session, a default-no confirmation naming the settled registration comes first** — before the key is read — so a pasted second line answers "no" rather than becoming the key; the session's `--yes` pre-answers it as it does `/model set`'s confirmation; the shell path asks nothing (verify M1) |
| `effort` set (`/effort <level>`, `teton effort <level>` typed) | **recorded exception**: pipe-friendly, on a TTY and on a pipe. REQ-559 BR-9 made `/effort` identical on both, and a typed `teton effort max` runs that row through the same full-argv validation every pre-REQ leaf row gets (verify M2), so it inherits BR-9 rather than this REQ's write gate. It changes one persisted setting the next turn reads; the shell command `teton effort` is equally unattended-friendly |
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
- [ ] BR-4: **Recognition is strict and total.** A prompt line is a CLI
      line iff its first whitespace token is `teton` **and** the following
      tokens name a subcommand path in the CLI parser's own tree (`provider
      list`, `policy set-tier`, `doctor` …). Then: the path names a row → run
      it and print one line `>> teton <sub> → /<row>` first, the row's own
      grammar (clap) validating whatever follows the path — a stray or bad
      argument prints the parser's own error, never a prompt; the path names
      no row (`uninstall`, a bare family such as `teton provider`) or the
      line is `teton` alone / `teton --help` / `teton --version` → one
      refusing line naming why and the shell pointer, never forwarded to the
      model (the BUG-146 shape: the harness was asked and something else
      answered — REQ-555 BR-2). Any `teton…` line whose next token is not a
      subcommand ("teton is slow today") is a plain prompt, byte-identical to
      today. REQ-555 BR-8's totality is amended — the classifier gains the
      recognized and refused CLI-line outcomes beside command, escaped
      prompt and plain prompt — and stays pinned in both directions: a test
      proves every CLI subcommand with a row is reachable from a `teton …`
      line, that every subcommand without a row is refused, and that
      non-subcommand `teton…` lines reach the prompt path unchanged; a
      completeness test walks the parser's tree so every leaf subcommand is
      either a row or an explicit shell-only exception (informed by REQ-555
      BR-8, LESSON-479 via it; amended at architecture, ADR-1/ADR-8). Session-only
      commands have no `teton` form, so `teton provider setup …` typed at
      the prompt is a plain prompt — the model's own hand-off names
      `/provider setup` (REQ-579 ADR-9).
      *(**Amended at TASK-170.** `teton provider setup` is **not** a plain
      prompt. `setup` names no subcommand, so the clap walk stops on the
      **family** `provider` — a recognized path with no row, which is BR-4's
      own second arm — and the line is refused in one sentence that names the
      session's rows under that family, `/provider setup` among them. The
      amendment is an improvement rather than a concession: a user who typed
      `teton provider setup` is asking for a command this session **has**, and
      answering them directly beats spending a model turn on a hand-off that
      says the same thing — the "the harness was asked and something else
      answered" shape BR-4 exists to close (BUG-146, REQ-555 BR-2). REQ-579's
      hand-off is untouched for the case it was built for: a **model reply**
      that recites the shell recipe.)*
      *(**Recorded deviation, TASK-170.** A bare family gets a sentence
      composed from the **table** (`slash::rows_under`), not clap's own error
      for that path as ADR-1 wrote it. `Cli::try_parse_from(["teton",
      "provider"])` does not produce a short "requires a subcommand" error: the
      derive marks the required subcommand `arg_required_else_help`, so clap
      answers with the whole help page for the family — a screen of text whose
      longest line is the global `--yes` description and whose `Usage:` /
      "try `--help`" tail is a shell's instructions. A `Surface` line owns one
      row, and BR-4 and ADR-1 both say one refusing line. Composing from the
      table keeps the anti-drift property BR-3 is about — the table is the list
      that decides what runs here — and is what lets the line name
      `/provider setup`, a session row the CLI has no subcommand for at all.
      Pinned by `slash.rs::a_teton_line_with_no_session_form_is_refused_with_the_reason`.)*
      *(**Verify-pass notes.** (a) Session-only row names typed after `teton`
      are **not** recognized — `teton help`, `teton clear`, `teton web setup`
      stay prompts, or, where the first word is a CLI family, the family's
      refusal. Deliberately: the strict rule "the first token after `teton` is
      a subcommand in the parser's tree" is what keeps `teton help me read this
      backtrace` a prompt, and the model's own hand-off names the `/` spellings
      for the session-only rows (REQ-579 ADR-9, BR-8's generic line for the
      mirrored ones). Widening recognition to table names would trade that
      guarantee for a convenience nobody asked for (Q2). (b) The binary's
      global flags ahead of the subcommand — `teton -y policy set-tier …`,
      `teton --verbose doctor` — are stepped over so the line is still
      recognized, carried to the row, and reported as ignored in one Info line
      (m5/M2). (c) A family followed by `--help`/`-h` (`teton provider --help`)
      renders clap's own page for that family as information — the user asked
      for help — rather than the bare-family refusal (T6). (d) A pre-REQ row
      reached this way (`cost`, `effort`, `model set`, `provider test`) has its
      whole typed argv validated by clap first, and the row is handed what the
      parser derived — `teton model set qwen --yes` reaches `/model set` as
      `qwen`, never as `qwen --yes` (M2). (e) The retired `teton policy set …`
      answers with the retirement sentence the shell prints (m6).)*
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

- [x] AC-1: In a session, `/provider list`, `/boundary list`, `/policy show`,
      `/model list`, `/model status` and `/doctor` each print exactly the
      lines their `teton …` twin prints against the same daemon (byte-diffed
      by a test that drives both), with `/doctor`'s connect arm replaced per
      BR-7.
      *(`cli_e2e.rs::every_read_row_prints_exactly_what_its_shell_twin_prints`
      — twelve client runs against one daemon, line-for-line; the `/doctor`
      carve-out asserted on each side. Unit legs:
      `cli_rows.rs::every_read_row_sends_its_shell_twins_method_on_the_sessions_connection`,
      `cli_rows.rs::doctor_reports_the_connection_the_session_already_has`.)*
- [x] AC-2: `/policy set-tier build <id>`, `/policy set-category edit <id>
      --fallback <id>` and `/boundary add <glob> --mode local-only` change the
      daemon's config exactly as their shell twins do (asserted on
      `config/get` before/after), and the next turn routes accordingly.
      *(`cli_e2e.rs::the_write_rows_change_the_config_their_shell_twins_read_back`
      — before/after through `teton policy show` and `teton boundary list`, so
      the evidence is the daemon's own resolution and not the writing row's
      echo. Argument parity:
      `cli_rows.rs::a_row_parses_its_argument_exactly_as_the_shell_parses_its_argv`.)*
- [x] AC-3: `/provider add <id> --kind openai-compatible --endpoint <url>
      --model <m>` on a TTY reads the key echo-off, stores it in the keychain,
      registers the provider, and the key appears nowhere in the transcript,
      the session's events, or the model's context (egress-capture assertion,
      REQ-579's harness).
      *(**Amended at verify (M1/M4).** The keychain is now a parameter of
      `provider_add_on` — both callers pass `keychain::default_keychain()` —
      and the session confirms, default-no, before it reads. So the composed
      flow is pinned in-process against a double, in `main.rs`:
      `a_confirmed_session_provider_add_stores_the_key_and_registers_by_reference`
      (the key reaches `MockKeychain` under the id; the `config/set` on the
      socket carries `keychain://teton/<id>` and no raw key; no surface line
      and no wire byte carries the key),
      `a_declined_session_provider_add_reads_no_key_and_stores_nothing` ("n",
      an empty answer, and a pasted second command line all decline before
      `ask_secret` is called; the keychain is untouched and only the duplicate
      probe reaches the socket),
      `the_sessions_yes_pre_answers_the_provider_add_confirmation`,
      `a_refused_session_registration_takes_its_stored_key_back_out` and
      `a_refused_session_registration_restores_the_key_it_displaced` (BUG-171's
      `PriorKey` undo through the composed flow), and
      `a_keychain_that_will_not_store_is_a_refusal_and_registers_nothing`. The
      terminal half stays in
      `pty_e2e.rs::a_session_provider_add_asks_for_its_key_echo_off_and_stores_nothing_untyped`
      — confirm, then the hiding prompt, an empty key answer, `config.toml`
      byte-identical — with the echo bit **fail-closed** (under a pty
      `EchoState::NoTerminal` is unreachable and `EchoState::Failed` refuses to
      read) and no credential typed, because the shipped binary still writes
      to the real login keychain. `cli_rows.rs::provider_add_reads_its_key_through_the_hiding_prompt_and_never_as_a_flag`
      pins the question order through the session row and that no `--key`
      flag parses. The bytes-on-a-screen sweep of a real typed credential is
      `pty_e2e.rs::the_key_step_does_not_echo_and_the_key_reaches_nothing`,
      over the same `Prompter::ask_secret` seam this row reads through.)*
- [x] AC-4: On piped stdin every write row (`provider add`, `boundary add`,
      `policy set-tier`, `policy set-category`) rejects with one line naming
      the shell command; every read row works.
      *(`cli_e2e.rs::on_a_pipe_every_write_row_names_its_shell_twin_and_changes_nothing`
      — one refusal line each, the three reads answering on the same pipe, and
      the twins' readings unchanged afterwards. Unit legs:
      `cli_rows.rs::a_write_row_on_a_pipe_names_its_shell_twin_and_sends_nothing`,
      `cli_rows.rs::a_read_row_ignores_the_write_gate`.)*
- [x] AC-5: Typing `teton provider list` at the session prompt prints
      `>> teton provider list → /provider list` and then the same lines as
      `/provider list`; no model call is made (no `route_decided`, no
      `CostRecord`, no `session_update`) — asserted over the wire.
      *(`cli_e2e.rs::a_typed_teton_line_runs_the_row_it_names_and_costs_no_turn`
      — the two spellings' whole session bodies diffed, and the scripted reply
      queue pinned untouched. Unit:
      `slash.rs::every_row_that_names_a_subcommand_is_reachable_from_a_typed_teton_line`.)*
- [x] AC-6: Typing `teton uninstall` prints one refusing line naming the
      shell pointer and makes no call; typing `teton is slow today` reaches
      the model as a prompt with the bytes unchanged; typing `teton provider
      list please` is recognized (its subcommand path is a row) and prints
      the CLI parser's own `unexpected argument 'please'` — never a prompt.
      *(Amended at architecture, ADR-1: a recognisable command with a stray
      word sent to the model reproduces the failure this REQ removes; the
      subcommand path is decided by clap's tree, the arguments by clap's
      grammar.)*
      *(`cli_e2e.rs::a_teton_line_with_no_session_form_is_refused_and_a_question_still_reaches_the_model`
      — four lines, one session, the reply queue as the arithmetic. Units:
      `slash.rs::a_teton_line_with_no_session_form_is_refused_with_the_reason`,
      `slash.rs::a_teton_line_that_names_no_subcommand_is_a_byte_identical_prompt`,
      `slash.rs::a_recognized_line_with_a_stray_word_prints_the_parsers_own_error`,
      `slash.rs::the_double_slash_escape_still_outranks_recognition`.)*
- [x] AC-7: `/policy set-tier build` (missing arg) and `/policy set-tier
      summit kimi` (bad enum) print the CLI parser's own error for that
      subcommand — the same text `teton policy set-tier build` prints — and
      issue no RPC.
      *(`cli_rows.rs::a_bad_argument_renders_claps_own_error_and_sends_nothing`
      — both spec examples, compared against `Cli::try_parse_from`'s own
      rendering for the same argv, with the socket asserted silent; and
      `cli_rows.rs::a_help_request_renders_claps_help_for_that_subcommand`.)*
- [x] AC-8: `/help` lists every new row, grouped by family; the table-vs-help
      test still holds; `//` footer unchanged.
      *(`cli_e2e.rs::slash_help_lists_every_mirrored_row_grouped_with_both_footers`
      — the shipped binary's listing: the ten rows with their summaries, the
      fourteen that were already there, contiguous families, both footers.
      Unit: `slash.rs::help_lists_every_mirrored_row_grouped_by_family`,
      `slash.rs::help_lists_every_alias_that_dispatches`.)*
- [x] AC-9: A scripted turn whose reply says "run `teton provider list` and
      `teton policy show`" prints exactly one line `>> in this session:
      /provider list, /policy show`; a reply that says "run `/provider list`"
      prints nothing; a reply that names `teton provider add …` prints the
      REQ-579 line and not the generic one; a reply about "the teton binary
      being slow" prints nothing.
      *(`session_ui.rs::a_reply_that_recites_shell_twins_names_their_session_spellings`,
      `a_reply_that_already_names_the_session_spelling_earns_nothing`,
      `the_setup_hand_off_wins_over_the_generic_line`,
      `the_connection_hand_off_wins_over_the_generic_line`,
      `a_reply_that_names_no_mirrored_command_earns_nothing`,
      `a_capitalised_mention_of_a_command_is_not_one`,
      `the_generic_line_is_tty_only_and_prints_once_per_turn`,
      `every_mirrored_row_is_a_candidate_of_the_generic_line`. The piped
      negative at the binary:
      `cli_e2e.rs::a_piped_session_whose_reply_recites_the_cli_gets_no_hand_off_line`.)*
- [x] AC-10: The guide test pins BR-9: every `teton <sub>` the guide names
      that has a session row also appears as `/<row>` in the guide.
      *(`cli_rows.rs::guide_tests::the_guide_names_every_mirrored_command_in_its_session_spelling`
      — over the guide's own bytes via `include_str!`, with the one recorded
      equivalence (`provider add` → `/provider setup`) and a non-vacuity check
      that the guide names at least one mirrored command in `teton …` form.)*
- [x] AC-11: On a `presence`-featured build, `/policy set-tier build kimi`
      raises the same presence prompt `teton policy set-tier` raises and a
      cancel leaves `config.toml` byte-identical (REQ-576 AC-6 pattern, via
      the `TETON_PRESENCE_ACCEPT=fail` seam — informed by LESSON-519,
      LESSON-520: pair the refused test with an accepted one on a payload
      that would persist).
      *(`cli_e2e.rs::a_presence_refused_session_set_tier_leaves_the_config_untouched`
      + `cli_e2e.rs::an_attested_session_set_tier_writes` — the refuse/accept
      pair on one line, with the file read back on both. **Amendment**: the
      test is not feature-gated and does not need to be. `TETON_PRESENCE_ACCEPT`
      installs a verifier in place of whatever the build has
      (`tetond::attest::seam_verifier`), so a default build driven through it
      takes the same `config/set` path a `--features presence` build takes with
      a real mechanism — which is how `tetond/tests/config_set_attestation.rs`
      already drives this gate. The seam rides `TETON_TEST_SEAMS`, and a
      release build refuses to start when that is set, so none of it exists in
      a shipped binary. The refusal is also asserted identical to the shell
      twin's, which is BR-6's claim.)*
- [x] AC-12: The workspace suite passes; no new protocol types; `git diff
      -- crates/teton-protocol/src/` is empty.
      *(`git diff origin/main...HEAD -- crates/teton-protocol/src/` printed
      zero lines at TASK-173; the workspace suite is green under
      `cargo test --workspace --no-fail-fast` with no `FAILED`.)*

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
- **(verify D2)** A near-miss `teton …` line — one whose words after `teton`
  do not name a subcommand path — reaches the model as a prompt, including
  one that happens to carry a pasted key (`teton api key is sk-…`). That is
  BR-4's own rule ("teton is slow today" is a question), and the same is true
  of any prompt line at all: the session's key-in-chat guard is the guide's
  prohibition plus the REQ-579 hand-off, not the classifier. Recorded rather
  than fixed; see Out of Scope.

## Open Questions

- [x] OQ-1: **RESOLVED (TASK-170) — recognized.** `/teton provider list` runs
      the same row `teton provider list` does. It cost the one line the
      proposal predicted: recognition is a single function, offered both the
      plain line and the post-`/` remainder, so the two spellings cannot
      diverge. BR-1 is untouched — a `/teton …` line that names no subcommand
      path is still an unknown command rejected with the `/help` hint, and
      never a prompt.
- [x] OQ-2: ~~Should the generic hand-off line (BR-8) subsume the REQ-579 and
      REQ-581 sentences into one renderer?~~ **RESOLVED at wrapup (2026-08-18):
      three sentences kept** — the two older ones carry reasons the generic
      line does not ("no key in chat", "makes one consented call"); the generic
      line is table-driven and third in precedence (ADR-6, TASK-171).
- [x] OQ-3: ~~Should bare `/model` stay concise once `/model list` and
      `/model status` exist?~~ **RESOLVED at wrapup (2026-08-18): stays
      concise** (REQ-555 BR-4); `/model status` is the full form (TASK-169).
- [ ] OQ-5: Quoted arguments. Session-side tokenization is whitespace only
      (ADR-2) — no quote or backslash processing — because no mirrored
      subcommand takes a whitespace-bearing value and a shell-words
      dependency was not budgeted. Revisit if a `boundary add` glob with a
      space is ever asked for.
- [ ] OQ-4: Should `teton --version` typed in-session print the client and
      daemon versions (cheap and useful after an upgrade) rather than be
      refused? Proposal: yes, as a `/version` row — but only if it costs no
      new RPC (the handshake result already carries the daemon version).
- [ ] OQ-6 **(verify D1, recorded)**: There is no verbatim escape for a
      `teton …` line the way `//` escapes a leading slash — a user who wants
      to *ask the model about* `teton provider list` types exactly the line
      that runs it. Rationale for shipping without one: recognition intercepts
      only lines whose words after `teton` are an exact subcommand path in the
      parser's own tree, so the interception surface is small and enumerable
      (the ten mirrored rows, the four pre-REQ leaves, `model`, `uninstall`,
      the families, and the binary's own flags); every write it can reach is
      typed-input-gated and, for `provider add`, confirmed — the one exception
      being `effort` set, recorded in Permissions; and a question phrased as a
      sentence ("why does teton provider list show two kimis?") is not
      intercepted at all, because `teton` is not its first word. If a user
      genuinely needs the model to read a bare command line, `//teton …`
      already works (BR-11: the escape outranks recognition and the model sees
      `/teton …`), which is a workable spelling for a rare need. Revisit if
      the interception surface grows.

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
- **(verify D2)** Intercepting near-miss `teton …` lines that carry a pasted
  key before they reach the model. The classifier is a command recognizer,
  not a secret scanner; the guard against a credential in chat is the
  prompt's prohibition and the REQ-579 hand-off, and a scanner here would be
  a second, weaker copy of the redactor's job.

## Deferred

- OQ-6 — a verbatim escape for a `teton …` line (recognition intercepts exact
  subcommand paths only; writes are gated; `effort` set is the recorded
  REQ-559 BR-9 exception). Revisit if a user hits it.
- Quoted/whitespace-bearing arguments in session rows (OQ-5, ADR-2).
- Extracting the clap tree from `main.rs` into its own module (verify D3).
- `docs/manual-verification.md` REQ-582 runbook — OUTSTANDING until the
  shipped binary is dogfooded (needs a release + `brew upgrade`).

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
