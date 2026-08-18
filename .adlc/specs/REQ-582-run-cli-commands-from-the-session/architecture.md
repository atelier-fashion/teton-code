# REQ-582 — Architecture: every session-meaningful `teton` command runs from the session

Status: approved for implementation · 2026-08-18

## Approach

Three seams, all client-side, no daemon change:

1. **Ten mirrored rows** join the slash table (`crates/teton/src/slash.rs`
   `COMMANDS`): `provider list`, `provider add`, `boundary list`, `boundary
   add`, `policy show`, `policy set-tier`, `policy set-category`, `model list`,
   `model status`, `doctor`. A mirrored row's handler does exactly two things:
   builds the argv its shell twin would have received (`["teton", <twin
   words>…, <args tokens>…]`) and hands it to the CLI's **own** clap tree
   (`Cli::try_parse_from`); the parsed `Command` then runs through a shared
   `*_on(conn, ctx, …)` function that the shell subcommand also calls after
   `ensure_connected` + `passive_ctx`. One grammar, one renderer, one daemon
   method per fact (BR-2/BR-3).
2. **Recognition** of a `teton …` line typed at the prompt is a classifier
   change: `slash::classify` walks clap's own subcommand tree over the
   whitespace-split tokens after `teton` to find the subcommand path; that
   path *is* the row name (rows are named after their subcommands, BR-1), so
   a recognized line becomes `dispatch(<row>, <rest>)` after one Notice
   line `teton <row> → /<row>`. Two new `Input` variants carry it (BR-4/5).
3. **The model's answers**: `hand_off_after_turn` gains a third, table-driven
   line — after REQ-579's and REQ-581's, which keep precedence — naming the
   `/` spellings of every mirrored shell command the reply recited and did
   not also name in `/` form (BR-8); the bundled guide is rewritten to say
   `/` spellings first, under its **2-byte** growth budget (BR-9, ADR-7).

The daemon, the protocol, and the wire are untouched (BR-10, AC-12).

## Data model changes

None on the wire. Client-side only:

| Type | Change |
|---|---|
| `slash::CommandSpec` | gains `mirror: Option<Mirror>`; `Mirror { shell: &'static str /* e.g. "teton policy set-tier" */, writes: bool }` — `None` for the session-only rows |
| `slash::Args` | gains `Cli` — "the shell twin's grammar; the handler parses with clap and rejects nothing at resolve time" (behaves like `Optional` in `resolve`) |
| `slash::Input` | gains `CliLine { name: &str, args: &str, shell_flags: &str }` (`shell_flags` — the global flags typed ahead of the path, `teton -y policy …`, verify m5), `CliRefused(String)` (the composed one-line refusal), and `CliHelp(String)` (clap's own page for `teton <family> --help`, verify T6) |
| `SessionState` | unchanged — the generic hand-off reads the same `turn_reply` the two existing lines read |

## API changes

None. Every mirrored row is a new call site of `config/get`, `config/set`,
`model/list`, `model/status` — the same params its shell twin sends. `/doctor`
adds no RPC (it renders the handshake facts the session's `Connection` already
holds, then the same `config/get` `run_doctor` makes).

## Service layer (client modules)

| Module | Role after REQ-582 |
|---|---|
| `crates/teton/src/main.rs` | still owns the clap tree (`Cli`, `Command`, `*Action`) — made `pub(crate)`; each `run_<sub>(paths, …)` becomes `ensure_connected` + `passive_ctx` + `<sub>_on(conn, ctx, …)`; new `run_mirrored_command(cmd: Command, conn, ctx)` matches the ten mirrored variants onto the `*_on` functions |
| `crates/teton/src/cli_rows.rs` (new) | `Mirror`, the ten mirrored handlers, `run_mirrored(twin, args, conn, ctx)` in the shipped order (verify M3): tokens → `Cli::try_parse_from` → a parse failure **or a `--help`/`--version` outcome** rendered by `render_clap_error`/`render_clap_text` (bounded, blank lines kept) → typed-input `write_gate` for a write row → the ignored-shell-flags note (`shell_flags_line`) → `run_mirrored_command`; `cli_path(tokens) -> Vec<&str>` (clap-tree walk used by the classifier), `is_leaf_path`, `family_help` (a family's own page), `refusal_for_path` (the composed refusal), `SHELL_ONLY` (`uninstall`) |
| `crates/teton/src/slash.rs` | table rows + `Args::Cli` + `Input::{CliLine, CliRefused, CliHelp}` + `classify` recognition arm (`cli_line`, `LEADING_GLOBAL_FLAGS` — pinned to the tree's `global` args by a unit test) + `run_cli_line` (the entry loop's `CliLine` arm: mirrored rows dispatch with the flags spliced on; a pre-REQ leaf's whole argv is judged by the tree first, with an exhaustive no-wildcard derive-match; the pre-REQ family `model` answers `--help` with its page and drops reported flags) + `/help` grouping |
| `crates/teton/src/session_ui.rs` | generic hand-off (BR-8), fed by the mirrored rows' `shell` twins |
| `crates/tetond/src/harness/self_config.md` | `/` spellings first (BR-9) |

## Key decisions

### ADR-1: Recognition walks clap's tree to the subcommand path, then dispatches through the slash table — and a stray argument is a parser error, not a prompt

**Decision.** A prompt line whose first whitespace token is exactly `teton`
is classified by walking `Cli::command()` (clap's `CommandFactory`) over the
following tokens with `find_subcommand` (which honours clap aliases) as deep
as the tokens name nested subcommands. Outcomes:

| Walk result | Bucket | Then |
|---|---|---|
| a path that names a row (`provider list`, `policy set-tier`, `model set`, `provider test`, `cost`, `effort`, `doctor`, …) | `Input::CliLine { name, args }` (`args` = the text after the path words, via the existing `match_name_words`) | one Notice `teton <name> → /<name>`, then `dispatch(name, args)` — the row's own argument grammar (clap for mirrored rows) validates `args` |
| a path with no row: `uninstall`, or a family typed bare (`teton provider`, `teton policy`) | `Input::CliRefused(text)` — `uninstall` gets its own sentence (shell-only; it stops the daemon under this session); a bare family gets clap's own error for that path (`Cli::try_parse_from` rendered) which lists the subcommands | one Error line, no RPC, never the model |
| an empty path and the next token is `--help`/`-h`/`--version`/`-V` | `Input::CliRefused` naming `/help` and the shell | same |
| an empty path otherwise (`teton is slow today`, `teton`) | bare `teton` → `CliRefused` ("you are already in a session"); anything else → `Input::Prompt` byte-identical | as today |

**Amendment to the spec (AC-6, BR-4).** The spec said `teton provider list
please` "reaches the model (strict parse)". Under this ADR it is *recognized*
(the path `provider list` is a row) and the row's clap parse prints
`error: unexpected argument 'please'` — never a prompt. Rationale: sending a
recognisable command with a stray word to the model reproduces the exact
failure this REQ exists to remove (the model replies "that one's for you to
run"); a shell would print the parser error, and one grammar means one
behaviour (BR-3). "Strict" now means: **the subcommand path is decided by
clap's tree, the arguments by clap's grammar**; a line whose first token
after `teton` is not a subcommand stays a prompt. The requirement's AC-6 and
BR-4 wording are updated in this PR to say so.

**Why through the table.** Dispatching a recognized line as `/<row>` (rather
than calling the parsed `Command` directly) keeps one place for the row's
typed-input gate, `/help` parity, "every row reachable" tests, and the
`--yes`/session-context conventions. It also means recognition cannot run
anything the table does not list (BR-5's "no subprocess, no second
connection" holds by construction — recognition never touches `main()`).

**Rejected.** A hand-written token matcher (`tokens[1] == "provider" && …`) —
LESSON-529: a second parser of one string is a display helper that lies.
Spawning `teton …` as a subprocess — BUG-177's shape and BR-5. Sending
unparsable-but-`teton`-prefixed lines to `CliRefused` instead of the model —
"teton is slow today" is a legitimate question about the product.

### ADR-2: Mirrored rows parse arguments by rebuilding argv for `Cli::try_parse_from` — whitespace tokens, no quote processing

**Decision.** `run_mirrored(twin, args, conn, ctx)`:
1. typed-input gate for `writes` rows (ADR-4);
2. `tokens = twin.split(' ') ++ args.split_whitespace()` (twin is `"teton
   policy set-tier"` etc.);
3. `Cli::try_parse_from(tokens)`: `Err(e)` → render `e.render().to_string()`
   line by line as `LineKind::Error` (clap's own text — the same the shell
   prints, AC-7 — including `--help` output for `/policy set-tier --help`);
   `Ok(cli)` → `crate::run_mirrored_command(cli.command, conn, ctx)`.

**Tokenization.** `split_whitespace`, exactly as the slash table already
splits, and no interpretation of quotes. None of the mirrored subcommands
takes a whitespace-bearing value (ids, tiers, categories, model names, URLs,
`--mode` enums; a glob with a space is legal in principle and unsupported
here). `shell-words`/`shlex` are in `Cargo.lock` only transitively (via
`portable-pty`, a dev-dependency); adopting one would be a new direct
dependency the spec did not budget. Recorded as a limitation in the row's
`/help` footer sentence and OQ-5.

**Rejected.** A per-row hand-parser of `--flags` (LESSON-529 again).
Changing `Handler`'s signature to receive the `CommandSpec` (touches every
existing handler for a benefit ten one-line handlers deliver).

### ADR-3: Each shell subcommand's body is extracted to `<sub>_on(conn, ctx, …)`; refusals become rendered outcomes, not `bail!`

**Decision.** `run_provider_list/add`, `run_boundary_list/add`,
`run_policy_show/bind`, `run_model_list/status`, `run_doctor` split into (a)
the shell wrapper — `stdout_surface`, `ensure_connected`, `passive_ctx` — and
(b) `<sub>_on(conn, ctx, …)` holding the RPC + renderer. The session handlers
call (b) with the session's own `Connection` and `UiContext` (REQ-555 D-4).
`run_provider_add`'s three `anyhow::bail!` refusals (remote without
`--model`, duplicate id, no key) become an enum the shell wrapper maps back to
the same `bail!` messages and the session handler renders as one
`LineKind::Error` line and `Continue` — a handler `Err` would end the session
(`dispatch(...)?` in the entry loop). Message text is identical; only the
channel differs, which is what AC-1's byte-parity (reads) and AC-2/AC-3
(effects) each assert.

`read_secret(id)` becomes `read_secret(id, prompter)`: the env shortcut
(`TETON_PROVIDER_KEY`) is unchanged and the fallback is the *caller's*
prompter — `StdinPrompter` from the shell, `ctx.prompter.ask_secret` in the
session (echo-off, dialogue prompter, REQ-549 BR-5).

**Why.** REQ-555 BR-4/BR-4b's rule ("one shared function, not a
re-implementation") is what made `/cost` and `/model set` safe; a second copy
of the registration flow is how REQ-547's consent bypass was born
(LESSON-441). Byte parity is then a property of the code shape and is
*pinned* by AC-1's diff test (LESSON-517 — the seam is the ground truth).

### ADR-4: Write rows are typed-input-only through one generalized gate whose refusal names the shell twin; daemon gates untouched

**Decision.** `model_set_gate` generalizes to `write_gate(typed_input,
seams_allowed)` (same polarity, same `test_seams_allowed` seam — the
invariant its doc states is preserved: the seam only *loosens*). Mirrored
rows with `writes: true` (`provider add`, `boundary add`, `policy set-tier`,
`policy set-category`) refuse on a pipe with one line built from the row:
"`/policy set-tier` is typed-input-only: this session's input is not a
terminal, so nothing was changed — run `teton policy set-tier …` from a
shell instead." `/model set` keeps its own richer sentence (it names
`--yes`). Read rows never consult the gate (BR-11).

Nothing changes daemon-side: the rows send the same `config/set` params, so
REQ-576's presence attestation on a `presence` build, REQ-547 BR-3's
above-floor confirmation, and the ancestry gate apply exactly as to the shell
twin (AC-11 reuses the `TETON_PRESENCE_ACCEPT=fail` seam — LESSON-519/520:
a persisting payload, refused-vs-accepted pair, config bytes read back).

### ADR-5: `/doctor` renders the same report over the session's connection

**Decision.** `run_doctor` splits into `doctor_report_on(paths, conn, ctx,
attach: DoctorAttach)` where `DoctorAttach::{Fresh(handshake), Session}`
decides only the one line — `daemon: running — teton-code X (protocol N)`
from a fresh handshake, or `daemon: running — teton-code X (this session's
connection)` from `conn.daemon_version()` (the handshake result the
`Connection` already keeps for build-skew reporting). Every other line
(socket/lock paths, `render_config`, `advise_on_base_url_endpoints`, the
model and providers notices) is the same code. `DaemonPaths` is
`socket_path::daemon_paths()`, a pure env read `run_session` already made —
the handler calls it again rather than threading it through `UiContext`.

**Why.** A fresh `Connection::connect` from inside a session announces `a
CLI client attached` into the very session running the diagnosis (BUG-177's
shape, and a lie about "another" client).

### ADR-6: The generic hand-off is table-driven and third in precedence

**Decision.** `hand_off_after_turn` keeps its shape (consume the turn's
record; TTY only; at most one line) and gains, after the REQ-579 and REQ-581
arms return without printing, a generic arm:

- candidates = every `COMMANDS` row with `mirror: Some(m)`, in table order;
- a candidate is *recited* when `contains_word(plain, m.shell)` (case-sensitive,
  the reply-side rule REQ-581 chose: a command is lowercase; a capitalised
  prose mention is not a command);
- a recited candidate is *dropped* when `plain` also contains `/<row name>` —
  the model already said it (REQ-579 ADR-9's dormancy, per command);
- if any remain: one Notice `in this session: /<a>, /<b>` in table order.

The REQ-579 line keeps `teton provider add` and `teton policy set-tier` (its
`PROVIDER_CLI_RECIPES` are unchanged) and the REQ-581 line keeps its
predicate; the generic arm is reached only when neither printed, so a
setup-shaped reply is still corrected by the sentence that says "no key in
chat" (BR-8). The line's spelling list is derived from the table, so a row
added later is nudged for without a second list to maintain (BR-7's rule
extended to the hand-off).

### ADR-7: The guide says `/` first, under a two-byte growth budget; the guide/table cross-check lives in `teton`

**Decision.** `self_config.md` today: `the_total_cap_clears_the_harness_
context_budget_with_margin` reports **50 bytes of headroom against a floor
of 48** — the guide may grow by at most 2 bytes. So the edit is a
*rewrite*, not an addition: every mirrored command the guide names moves to
its `/` spelling (`/policy set-tier`, `/policy set-category`, `/policy show`,
`/provider list`, `/doctor` — each saves the 5 bytes of `teton ` → `/`), and
one short sentence teaches the mapping for shell users ("From a shell the
same commands are `teton …`: `/policy show` is `teton policy show`."). Step 1
keeps `/provider setup` first and `teton provider add` marked "shell only"
(REQ-579's test); step 3 keeps `` `/provider test <id>` `` (REQ-581's test);
the prohibition line and its "ask" uniqueness are untouched; the sentence
"You cannot run these commands yourself; hand them to the user." stays. The
implementer trims elsewhere in the guide until the margin test is green.

**Cross-check test (BR-9/AC-10).** Lives in `crates/teton` (which owns the
row table) and reads the guide with
`include_str!("../../tetond/src/harness/self_config.md")` — compile-time, no
crate dependency, no source scanning (BUG-159). For every `teton <sub>` the
guide names whose `<sub>` is a mirrored row, the guide must also contain
`/<sub>` — with one explicit equivalence, `provider add → /provider setup`
(the guided flow is the session answer REQ-579 chose; naming `/provider add`
beside it would re-open the ambiguity the live A/B closed).

### ADR-8: Totality and completeness are pinned in both directions, against clap's tree

**Decision.** `Input` has six variants (`Command`, `CliLine`, `CliRefused`,
`CliHelp`, `EscapedPrompt`, `Prompt` — five as designed, the sixth added at
the verify pass; see the Deviations section below, "BR-4's four buckets");
the REQ-555 BR-8 tests are extended: every
mirrored row is reachable from `teton <row>` (→ `CliLine`), `teton uninstall`
/ bare `teton` / `--version` → `CliRefused`, `teton is slow today` and
`tetonx provider list` → `Prompt` unchanged, and `//` unaffected. A
completeness test walks `Cli::command()` recursively: **every leaf
subcommand path is either a row name or listed in `SHELL_ONLY`** — so a
future `teton foo bar` cannot ship without a decision about its session
form (the compile-time analogue is `run_mirrored_command`'s exhaustive
`match`).

## Proposed additions to `.adlc/context/architecture.md`

Key Pattern (to add at wrapup): **"A second surface for a command is a
second call site of one grammar and one renderer"** — a session command that
mirrors a shell command rebuilds the shell's argv and parses it with the
shell's own parser, then runs the same `*_on(conn, ctx)` body; recognition of
a typed shell line walks the parser's own tree; and the nudge that points a
model's shell recipe at the session spelling is derived from the same table
that dispatches it (REQ-555 BR-4 → REQ-582 ADR-1/2/6, LESSON-529/517).

## Deviations from the requirement (recorded here, mirrored into the spec)

- **AC-6 / BR-4** — `teton provider list please` is recognized and answered
  with clap's "unexpected argument" rather than sent to the model (ADR-1).
- **BR-4's "four buckets"** — modelled as five `Input` variants (recognized
  vs refused CLI lines are distinct outcomes); the totality property is the
  same (ADR-8). **Six since the verify pass**: `Input::CliHelp(String)`
  carries clap's own page for a family followed by `--help`/`-h` (`teton
  provider --help`), rendered as Info lines — a user who asked for help asked
  for more than one line, which is why it cannot ride `CliRefused` (T6).
- **Tokenization** — whitespace only, no quotes (ADR-2); OQ-5 added.

### Verify-pass deviations (2026-08-18)

- **ADR-1 recognition, pre-REQ rows (M2).** A recognized line whose row has
  `mirror: None` and whose name is a *leaf* in clap's tree (`cost`, `effort`,
  `model set`, `provider test`) is not dispatched straight to the row: the
  entry loop calls `slash::run_cli_line`, which validates the FULL typed argv
  with `Cli::try_parse_from(["teton", …tokens])` first — `Err` renders the
  parser's message and dispatches nothing; `Ok` derives the row's argument
  from the parsed `Command` (`Cost` → `""`, `Effort{level}` → the level or
  `""`, `Model{Set{name}}` → the name, `Provider{Test{id}}` → the id) and
  dispatches that. Global flags (`--yes`, `--verbose`) parse, are dropped,
  and are reported in one Info line (`cli_rows::shell_flags_line`). Bare
  `teton model` — a family the table answers — keeps TASK-170's direct
  dispatch, since clap cannot parse it to a command. Mirrored rows are
  unchanged: their handlers parse for themselves. Not a second parser (BR-3):
  the judge is the binary's tree and the row receives what it parsed.
  `effort` set stays pipe-friendly (REQ-559 BR-9) — recorded in the spec's
  Permissions table as the one write recognition can reach without the gate.
- **ADR-1 recognition, leading global flags (m5).** `teton -y policy set-tier
  …` / `teton --verbose doctor`: the classifier steps over
  `-y/--yes/-v/--verbose` before the walk and carries them on
  `Input::CliLine.shell_flags`, so the row's own clap parse still meets them
  (spliced onto the argument for a mirrored row; part of the full argv for a
  pre-REQ leaf) and says they were ignored. Without this the walk began at
  `-y`, found no subcommand, and sent the line to the model.
- **ADR-2 step order (M3).** `run_mirrored_seamed` parses **first**, then
  applies the write gate, then runs: `--help` is a parse outcome, and a piped
  `/policy set-tier --help` is a question the gate has no business refusing.
  A parse error on a piped write row likewise renders (no RPC, nothing
  changed). `render_clap_error`'s `DisplayVersion` arm is unreachable —
  `--version` is not propagated to subcommands and clap reports it as an
  unexpected argument (T3) — and stays for the reason its doc gives.
- **ADR-2 step 3 rendering (m1/m2).** Every line of clap's text reaches the
  surface, blank lines as `Info ""` (byte parity with the shell), and each line
  is bounded and defused through `slash::echoed_within` at
  `CLAP_LINE_MAX_CHARS` (640 — clap does not wrap help in this build, and the
  global `--yes` doc is one long line — 612 characters as rendered, after the
  closing round named the `/provider add` confirmation in it; a tree-walk test
  over every leaf *and* family asserts the binary's own text clears the bound). One renderer, `render_clap_text`, for
  a row's parse failure, a pre-REQ row's full-argv failure, and the family
  help page.
- **ADR-3 error taxonomy (m7).** A **transport** failure propagates out of a
  mirrored row (the session ends as it does when `/cost` meets one; a body
  that swallowed it would report a dead socket one command later). Every
  non-transport failure of `provider add` — the endpoint the registration
  seam refuses (REQ-578 BR-5), a keychain that will not store — travels as a
  `ProviderAddRefusal` variant (`Endpoint`, `KeychainStore`) beside the three
  decisions, rendered as one line with the session continuing; the shell
  `bail!`s with the same sentence.
- **ADR-3 / BR-6, session-only confirm (M1).** `provider_add_on` takes an
  `AddConsent`: `Shell` asks nothing (the command line was the consent; bytes
  unchanged), `Session{assume_yes}` asks a default-no confirmation — after the
  duplicate probe and endpoint settlement, before the key is read, only when a
  key is about to be read — naming id, kind, model and the settled endpoint
  through the masking renderer (LESSON-529/535). Anything but `is_yes`
  declines with one Info line and no key read; the session's `--yes`
  pre-answers it as it does `/model set`'s confirmation. Motivation: a
  multi-line paste's second line was the key.
- **ADR-3, keychain as a parameter (M4).** `provider_add_on(…, keychain:
  &dyn Keychain)`; both callers pass `keychain::default_keychain()`. The
  composed read → store → `config/set` path, the wire bytes, and BUG-171's
  undo are now unit-tested against `MockKeychain`.
- **ADR-2 step 3, the dispatcher's unreachable arms (m3).**
  `run_mirrored_command`'s six non-mirrored arms render one shared sentence
  (`not_a_mirrored_row`) — unreachable by construction, tested by direct
  call; the per-arm `session_spelling_line`/`UNINSTALL_IS_SHELL_ONLY` texts
  are gone. `teton policy set …` typed at the prompt answers with
  `POLICY_SET_RETIRED` from the classifier (m6); the family refusal reads
  "`teton provider …` names a family rather than a command — …" (m4).
- **ADR-6 dormancy (m8).** The "already named the `/` spelling" check is a
  whole-word match, so `/doctor` inside `crates/teton/src/doctor.rs` does not
  silence the nudge.
- **ADR-7 guide (m9).** Step 1's "Shell only:" became "Shell:" — REQ-582 gave
  the by-hand registration a session row, so "only" was false. The REQ-579
  step-1 pin reads `shell:` with the reason recorded; margin after the edit:
  **56 bytes** against the 48 floor (−5 bytes on the guide).
- **Test-strategy row AC-9 (T14).** The proof of the generic line's TTY-only
  gate is the unit test `the_generic_line_is_tty_only_and_prints_once_per_turn`;
  the piped e2e `a_piped_session_whose_reply_recites_the_cli_gets_no_hand_off_line`
  is the binary-level negative and now also asserts the generic prefix is
  absent (its reply recites two mirrored rows).
- **Recorded, not fixed (D1–D3).** D1: no verbatim escape for a `teton …`
  line — OQ-6 in the spec, with the rationale (small enumerable interception
  surface; writes gated except `effort`; `//teton …` already reaches the
  model). D2: near-miss lines carrying a pasted key reach the model as any
  prompt does — spec Assumptions/Out of Scope. D3: `main.rs` size / extracting
  the clap tree to a `cli.rs` — a follow-up, not this REQ's change.

### Re-verify residue (2026-08-18, closing round)

- **`run_cli_line`, the pre-REQ family (`model`).** `teton model --help` /
  `-h` renders the family's own page (`family_help` → `render_clap_text`)
  instead of `/model`'s "takes no arguments"; `teton -y model` judges the
  flags alone with `Cli::try_parse_from(["teton", "-y"])` (legal argv — a
  session with `--yes`), prints `shell_flags_line`, and dispatches `/model`
  on its argument alone. The derive-match over `cli.command` is exhaustive
  with no wildcard (mirroring `run_mirrored_command`).
- **`LEADING_GLOBAL_FLAGS` / `CLI_FLAGS`** are pinned both ways to the clap
  tree's `global` arguments and the built root's `Help`/`Version` actions.
- **`--yes`'s doc** names the in-session `/provider add` confirmation it
  pre-answers; a *typed* `--yes` on that row does not (unit-tested — the
  consent is `ctx.auto_accept_model`'s, never the line's).
- **Test safety.** No test on the shipped `provider add` dispatcher path
  (real keychain) scripts a non-empty key; the line-bound test also walks
  family nodes; the piped hand-off negative recites a non-recipe row on a
  second turn; the parity test's multiset assertion is stated as the
  construction it was; `teton provider --help` has an e2e leg.

## Test strategy summary

| AC | Where | Harness |
|---|---|---|
| AC-1 parity (reads) | `crates/teton/tests/cli_e2e.rs` | drive `teton <sub>` and a piped session `/<sub>` against one scripted daemon; diff the lines after the session-ready line (`/doctor`: diff all but the connect arm) |
| AC-2 writes | `cli_e2e.rs` under `TETON_TEST_SEAMS=1` (`run_cli_seamed`) | `/policy set-tier`, `/policy set-category`, `/boundary add` then `teton policy show` / `teton boundary list` |
| AC-3 `/provider add` | `crates/teton/tests/pty_e2e.rs` (confirm, then the echo-off key prompt; no credential typed) + `main.rs` `provider_add_on` tests over `MockKeychain` (M4: the composed store, the wire carrying `keychain://` and never the key, the refused-registration undo, the declined confirm) | key never in transcript or on the wire; config carries `keychain://` |
| AC-4 piped write refusal | `cli_e2e.rs` | each write row prints the shell pointer; reads work |
| AC-5 recognized line | `cli_e2e.rs` | note line + `/provider list` output; the scripted engine's reply queue is untouched (no turn consumed) |
| AC-6 refusals/prompts | `slash.rs` unit + `cli_e2e.rs` | `teton uninstall` refused; `teton is slow today` reaches the model (scripted reply consumed) |
| AC-7 clap errors | `slash.rs`/`cli_rows.rs` unit (`RecordingSurface`) | rendered text equals `Cli::try_parse_from` error for the same argv |
| AC-8 `/help` | `slash.rs` unit + `cli_e2e.rs` | every row listed; grouping; footer |
| AC-9 hand-off | `session_ui.rs` unit (`hand_off_turn` helper) | four cases from the AC; the TTY-only gate is `the_generic_line_is_tty_only_and_prints_once_per_turn` (unit — the proof); `cli_e2e.rs::a_piped_session_whose_reply_recites_the_cli_gets_no_hand_off_line` is the binary-level negative for both the REQ-579 line and the generic prefix (T14) |
| AC-10 guide | `crates/teton` unit (include_str) | cross-check with the equivalence |
| AC-11 presence | `cli_e2e.rs`/`pty_e2e.rs` with `TETON_PRESENCE_ACCEPT=fail` on a `presence` build (feature-gated test) | refused line, config bytes identical; paired with `accept` |
| AC-12 | workspace suite; `git diff -- crates/teton-protocol/src/` empty | CI |
