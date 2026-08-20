//! The session rows that mirror a `teton` subcommand (REQ-582 BR-1/BR-2/BR-3).
//!
//! Ten commands the CLI already had — `provider list`, `provider add`,
//! `boundary list`, `boundary add`, `policy show`, `policy set-tier`,
//! `policy set-category`, `model list`, `model status`, `doctor` — are reachable
//! from inside a session, because a user who asks the session about its own
//! configuration should not have to open a second terminal to be answered.
//!
//! A mirrored row does exactly two things. It rebuilds the argv its shell twin
//! would have received (`["teton", <twin words>…, <argument tokens>…]`) and hands
//! it to the CLI's **own** clap tree ([`crate::Cli::try_parse_from`]); then it
//! runs the parsed [`crate::Command`] through [`crate::run_mirrored_command`],
//! which dispatches onto the same `<sub>_on(conn, ctx, …)` body the subcommand
//! runs after `ensure_connected` (ADR-2/ADR-3). One grammar, one error message,
//! one help text, one renderer, one daemon method per fact — so `/policy show`
//! and `teton policy show` cannot describe one daemon differently, and there is
//! no second hand-written parser of `teton …` arguments anywhere in the client
//! (BR-3; LESSON-529 — two parsers of one string drift into a lie on screen).
//!
//! What the rows do *not* do is open a connection or build a surface: they run on
//! the session's own [`Connection`] and [`UiContext`] (REQ-555 D-4), which is
//! also why `/doctor` reports the daemon it is already attached to rather than
//! dialling the socket again and announcing an attach into the session it is
//! diagnosing (BR-7, BUG-177's shape).
//!
//! Four of the ten write — `provider add`, `boundary add`, `policy set-tier`,
//! `policy set-category` — and those are typed-input-only through [`write_gate`],
//! the generalization of the gate REQ-555 built for `/model set` (ADR-4). Every
//! daemon-side gate is untouched: the rows send the same `config/set` params
//! their shell twins send, so REQ-576's presence attestation and the ancestry
//! gate apply to them exactly as they apply to a shell (BR-6).

use std::sync::OnceLock;

use clap::{CommandFactory, Parser};

use crate::client::{Connection, UiContext};
use crate::render::{LineKind, Surface};
use crate::slash::{test_seams_allowed, CommandOutcome};
use crate::Cli;

/// What a [`crate::slash::CommandSpec`] row knows about the shell command it
/// mirrors (REQ-582 BR-1).
///
/// `shell` is the twin's full spelling — `"teton policy set-tier"` — and it is
/// also the row's argv prefix, its `/help`-independent identity for the hand-off
/// nudge (BR-8), and the name the typed-input refusal points at. It equals
/// `"teton "` + the row's own name by construction; a unit test pins that,
/// because ADR-1's recognition of a typed `teton …` line depends on it (the
/// subcommand path clap walks *is* the row name).
#[derive(Debug, Clone, Copy)]
pub(crate) struct Mirror {
    /// The `teton …` spelling this row mirrors.
    pub(crate) shell: &'static str,
    /// Whether running it changes daemon or machine state, which is what makes
    /// it typed-input-only client-side (ADR-4, BR-6).
    pub(crate) writes: bool,
}

// The ten rows' identities. Each is defined **once** and used twice: the slash
// table carries it as `mirror: Some(…)` and the row's handler hands the same
// value to [`run_mirrored`]. A row declared a write in the table and run as a
// read would be two facts about one command that can disagree — the shape
// LESSON-529 is about, one layer down — and there is nothing here to keep in
// sync because there is nothing here twice.

/// `/provider list` — a read.
pub(crate) const PROVIDER_LIST: Mirror = Mirror {
    shell: "teton provider list",
    writes: false,
};

/// `/provider add` — registers a provider and stores a key.
pub(crate) const PROVIDER_ADD: Mirror = Mirror {
    shell: "teton provider add",
    writes: true,
};

/// `/boundary list` — a read.
pub(crate) const BOUNDARY_LIST: Mirror = Mirror {
    shell: "teton boundary list",
    writes: false,
};

/// `/boundary add` — writes a privacy boundary.
pub(crate) const BOUNDARY_ADD: Mirror = Mirror {
    shell: "teton boundary add",
    writes: true,
};

/// `/policy show` — a read.
pub(crate) const POLICY_SHOW: Mirror = Mirror {
    shell: "teton policy show",
    writes: false,
};

/// `/policy set-tier` — writes a tier binding.
pub(crate) const POLICY_SET_TIER: Mirror = Mirror {
    shell: "teton policy set-tier",
    writes: true,
};

/// `/policy set-category` — writes a category override.
pub(crate) const POLICY_SET_CATEGORY: Mirror = Mirror {
    shell: "teton policy set-category",
    writes: true,
};

/// `/model list` — a read.
pub(crate) const MODEL_LIST: Mirror = Mirror {
    shell: "teton model list",
    writes: false,
};

/// `/model status` — a read.
pub(crate) const MODEL_STATUS: Mirror = Mirror {
    shell: "teton model status",
    writes: false,
};

/// `/doctor` — a read, over this session's own connection (BR-7).
pub(crate) const DOCTOR: Mirror = Mirror {
    shell: "teton doctor",
    writes: false,
};

/// The CLI leaf subcommands that deliberately have **no** session row (ADR-8).
///
/// One entry, and it is a decision rather than an omission: `teton uninstall`
/// stops the daemon this session is attached to and deletes its data, so a
/// session form would pull the floor out from under the session running it. The
/// completeness test walks clap's tree and requires every visible leaf to be
/// either a row name or listed here, so a subcommand added later cannot ship
/// without someone deciding which of the two it is.
///
/// The classifier reads it too: a typed `teton uninstall` is answered with
/// [`refusal_for_path`]'s reason rather than sent to the model (ADR-1), so the
/// same list that exempts a command from having a row is what tells a user why
/// it has none.
pub(crate) const SHELL_ONLY: &[&str] = &["uninstall"];

/// What [`write_gate`] decides.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum WriteGate {
    /// Run the row.
    Run,
    /// Render [`typed_only_line`] and send nothing.
    Refuse,
}

/// Whether a row that **writes** may run, from where the session's input comes
/// (REQ-582 ADR-4).
///
/// REQ-555 built this rule for the one in-session command that changed daemon
/// state; this REQ adds four more, so the rule is one function with five call
/// sites rather than five copies of an `if`. Nothing about it changed on the way
/// out of `slash.rs`: same polarity, same seam, same sentence shape.
///
/// `typed_input` is the honest name for what is actually known — the session's
/// stdin was a terminal when the process started. It bounds the rule without
/// implementing it: a pty opened by `expect(1)`, a tmux `send-keys`, or a paste
/// into a real terminal all satisfy it. What it buys is that the common
/// unattended shapes (a heredoc, a `<<<` string, a piped file, a CI step) cannot
/// change this machine's configuration without naming the auditable surface
/// instead.
///
/// `seams_allowed` is the escape hatch the e2e suite drives the write rows
/// through — [`test_seams_allowed`], never a plain environment variable, so a
/// shipped binary cannot be talked out of the gate.
///
/// Pure, so both answers are unit-tested without a terminal, a pipe, or a
/// daemon: the branch that matters is the one a test process cannot otherwise
/// reach.
#[must_use]
pub(crate) fn write_gate(typed_input: bool, seams_allowed: bool) -> WriteGate {
    if typed_input || seams_allowed {
        WriteGate::Run
    } else {
        WriteGate::Refuse
    }
}

/// The one line a write row gets back on a pipe (ADR-4).
///
/// It reports exactly what was checked — "this session's input is not a
/// terminal" — not a claim to have identified a human, and it names the shell
/// command that does the same thing unattended, because "refused" without a
/// remedy is a dead end. `/model set` keeps its own richer sentence: that one
/// also has to name `--yes`, which no row here consults.
///
/// Built from the row rather than written per row, so a row added later cannot
/// ship with a refusal that names the wrong command.
#[must_use]
pub(crate) fn typed_only_line(row: &str, shell: &str) -> String {
    format!(
        "/{row} is typed-input-only: this session's input is not a terminal, so nothing was \
         changed — run `{shell} …` from a shell instead."
    )
}

/// The lead-in clap puts on an error message.
///
/// Stripped from the first line before it reaches the surface because
/// [`LineKind::Error`] *is* that prefix on a `PlainSurface` — rendering clap's
/// own text verbatim would print `error: error: …`, which is neither what the
/// shell prints (AC-7) nor what any other line in this client does. The
/// convention here is that a caller supplies the sentence and the surface class
/// supplies the prefix; clap is the one message source that ships with its own.
const CLAP_ERROR_LEAD: &str = "error: ";

/// How much of one rendered clap line reaches the surface (verify m2).
///
/// Clap quotes the offending token back in an "unexpected argument" error and
/// in the `tip:` line under it, and the token is whatever the user typed — a
/// pasted paragraph as a stray positional would be replayed in full, into the
/// scrollback and any recording of it. So every line goes through the same
/// bound-and-defuse [`crate::slash::echoed`] applies to a quoted command name.
///
/// The cap is wide because clap does **not** wrap help text in this build (no
/// `wrap_help` feature), so a long doc comment is one long line — the global
/// `--yes` flag's is over six hundred characters — and the binary's own text
/// must reach the surface whole (AC-7's parity with the shell). A unit test
/// walks every leaf's and every family's `--help` and `-h` page and asserts
/// every line clears this bound, so a doc comment that grows past it fails
/// there rather than being silently truncated in a session. Only a user's own
/// long token ever meets the ellipsis.
const CLAP_LINE_MAX_CHARS: usize = 640;

/// Render a clap parse failure — or a `--help` request, which clap also reports
/// as an `Err` — onto the session's surface (ADR-2 step 3).
///
/// The kind decides one thing: whether the first line is an error. A help
/// render carries no error line at all, because a user who asked for help got
/// what they asked for (AC-7). `DisplayVersion` is matched beside it for the
/// same reason but is unreachable from every caller today: `--version` is a
/// top-level flag the binary does not propagate to its subcommands, so on a
/// row's argv clap reports it as an unexpected argument (a test pins that,
/// verify T3), and a bare `teton --version` is refused by the classifier before
/// any parse. It stays in the match because if a subcommand ever propagated the
/// version, help-shaped rendering is the right answer and an `error:` line would
/// be the wrong one.
pub(crate) fn render_clap_error(err: &clap::Error, surface: &mut dyn Surface) {
    let asked_for = matches!(
        err.kind(),
        clap::error::ErrorKind::DisplayHelp | clap::error::ErrorKind::DisplayVersion
    );
    render_clap_text(&err.render().to_string(), !asked_for, surface);
}

/// Render clap's own text, line by line, onto the surface.
///
/// **Every** line reaches the surface, in order, once — blank ones included, as
/// [`LineKind::Info`] `""` (verify m1): a `PlainSurface` prints an empty Info
/// line as an empty line, which is exactly the byte the shell prints between
/// clap's error and its `Usage:` clause, and dropping it made the session's
/// rendering differ from the shell's by the blank lines. When `first_is_error`,
/// the first non-blank line renders as [`LineKind::Error`] with
/// [`CLAP_ERROR_LEAD`] removed (see there); everything else — the `Usage:`
/// clause, the argument list, the "For more information" tail, a whole help
/// page — is [`LineKind::Info`], which is what it is and how the shell prints it.
///
/// Each line is bounded and defused ([`CLAP_LINE_MAX_CHARS`], verify m2) on the
/// way out, so a user's own bytes quoted back by clap cannot replay a pasted
/// paragraph or an escape sequence into the terminal.
///
/// `pub(crate)` because two callers render clap text that is not a parse
/// failure of a row's argv: the classifier's family-help outcome
/// ([`crate::slash::Input::CliHelp`], verify T6) and [`crate::slash::run_cli_line`]'s
/// full-argv validation of a pre-REQ row (verify M2). One renderer, so the
/// blank-line and bounding rules cannot be applied on one path and forgotten on
/// another.
pub(crate) fn render_clap_text(text: &str, first_is_error: bool, surface: &mut dyn Surface) {
    let mut lines = text.lines().peekable();
    if first_is_error {
        // Clap opens an error with `error: …` on its first line; the leading
        // blank lines are skipped only in the theoretical case it did not, so
        // the class prefix never lands on an empty row.
        while lines.peek().is_some_and(|line| line.trim().is_empty()) {
            lines.next();
        }
        if let Some(first) = lines.next() {
            let sentence = first.strip_prefix(CLAP_ERROR_LEAD).unwrap_or(first);
            surface.line(
                LineKind::Error,
                &crate::slash::echoed_within(sentence, CLAP_LINE_MAX_CHARS),
            );
        }
    }
    for line in lines {
        surface.line(
            LineKind::Info,
            &crate::slash::echoed_within(line, CLAP_LINE_MAX_CHARS),
        );
    }
}

/// The one line a recognized line prints when the binary's global flags were on
/// it (verify M2).
///
/// `--yes` and `--verbose` are the shell's: the first pre-answers confirmations
/// for an unattended run and the second sets a session's initial verbosity, and
/// neither is a row's to honour from inside a session — the session already has
/// its own `--yes` (the one `run_session` was started with) and `/verbose`. A
/// row that honoured a typed `--yes` would let a line inside a session grant a
/// consent the session's own flag governs. So the flags parse (they are legal
/// argv, and clap says so), are dropped, and this line says they were.
///
/// `/model set` gets the specific sentence because it is the row whose
/// confirmation a user typing `--yes` was trying to skip; every other row gets
/// the general one.
pub(crate) fn shell_flags_line(name: &str, yes: bool) -> String {
    if yes && name == "model set" {
        "`--yes` is a shell flag and was ignored: /model set asks its own confirmation here \
         (this session's own `--yes` answers it)."
            .to_owned()
    } else {
        format!(
            "shell flags (`--yes`, `--verbose`) are ignored in a session — /{name} runs under this \
             session's own `--yes` and `/verbose`."
        )
    }
}

/// The argv a mirrored row hands to the CLI's parser (ADR-2 step 2).
///
/// The twin's words, then the typed argument's words. **Whitespace only** — no
/// quote or backslash processing — which is the same split the slash table
/// already applies to every other row's argument, and is enough for every
/// mirrored subcommand: ids, tiers, categories, model names, URLs and `--mode`
/// enums carry no spaces. A glob with a space in it is legal in principle and
/// cannot be typed here; `/help` says so and OQ-5 records it. Adopting
/// `shell-words` would be a new direct dependency this REQ did not budget, and
/// hand-rolling quote handling would be the second parser BR-3 forbids.
fn mirrored_argv<'a>(twin: &'a str, args: &'a str) -> Vec<&'a str> {
    twin.split_whitespace()
        .chain(args.split_whitespace())
        .collect()
}

/// The row name a twin belongs to — `"teton policy set-tier"` → `"policy
/// set-tier"`.
///
/// Derived rather than passed beside the twin, because two strings that must
/// agree are two strings that can disagree; the invariant they would express is
/// pinned once, in `slash.rs`, over the whole table.
fn row_name(twin: &'static str) -> &'static str {
    twin.strip_prefix("teton ").unwrap_or(twin)
}

/// Run a mirrored row: argv, parse, gate, dispatch (REQ-582 ADR-2; order
/// amended at verify, M3).
///
/// The whole of what the ten handlers below do, so each of them is one line and
/// the order of these steps is stated once:
///
/// 1. [`mirrored_argv`] rebuilds the shell's argv and
///    [`crate::Cli::try_parse_from`] — the binary's own grammar — judges it. A
///    parse that fails is rendered exactly as the shell would render it
///    ([`render_clap_error`]) and that is the end: no gate, no RPC. This is
///    **before** the write gate because `--help` is a parse outcome, and a user
///    on a pipe asking `/policy set-tier --help` asked a question the gate has no
///    business refusing — help changes nothing (M3). A bad argument is likewise
///    a sentence and nothing else, on a pipe or a terminal.
/// 2. a row that **writes** then meets [`write_gate`] — the refusal costs no RPC
///    (ADR-4);
/// 3. the binary's global flags, if the line carried any, are reported as
///    ignored ([`shell_flags_line`], M2);
/// 4. the parsed [`crate::Command`] runs through [`crate::run_mirrored_command`]
///    on the session's connection.
///
/// # Errors
///
/// A **transport** failure from the body — the socket gone, a frame that will
/// not parse — is propagated, and the entry loop ends the session on it exactly
/// as it does when `/cost` meets one (verify m7). Everything else a body can
/// raise is a rendered line and [`CommandOutcome::Continue`]: `provider add`'s
/// refusals and its endpoint-seam and keychain failures travel as
/// [`crate::ProviderAddRefusal`] and never as `Err`, so a mistyped registration
/// cannot end a session while a dead connection cannot be mistaken for one.
fn run_mirrored(
    row: Mirror,
    args: &str,
    conn: &mut Connection,
    ctx: &mut UiContext<'_>,
) -> anyhow::Result<CommandOutcome> {
    run_mirrored_seamed(row, args, conn, ctx, test_seams_allowed())
}

/// [`run_mirrored`] with the seam switch supplied rather than read.
///
/// Split for [`write_gate`]'s reason: the refusal branch is the one a test
/// process cannot reach by having a pipe for stdin, and a test that had to set
/// `TETON_TEST_SEAMS` in its own environment would be a test whose result
/// depended on how `cargo test` was invoked.
fn run_mirrored_seamed(
    row: Mirror,
    args: &str,
    conn: &mut Connection,
    ctx: &mut UiContext<'_>,
    seams_allowed: bool,
) -> anyhow::Result<CommandOutcome> {
    let twin = row.shell;
    let cli = match Cli::try_parse_from(mirrored_argv(twin, args)) {
        Err(err) => {
            render_clap_error(&err, ctx.surface);
            return Ok(CommandOutcome::Continue);
        }
        Ok(cli) => cli,
    };
    if row.writes && write_gate(ctx.typed_input, seams_allowed) == WriteGate::Refuse {
        ctx.surface
            .line(LineKind::Error, &typed_only_line(row_name(twin), twin));
        return Ok(CommandOutcome::Continue);
    }
    // `yes` and `verbose` are global flags of the parsed `Cli` and are dropped
    // — none of the ten rows consults `--yes`, and the session's verbosity is
    // `/verbose`'s to own — and the drop is said out loud (M2).
    if cli.yes || cli.verbose {
        ctx.surface
            .line(LineKind::Info, &shell_flags_line(row_name(twin), cli.yes));
    }
    match cli.command {
        Some(command) => crate::run_mirrored_command(command, conn, ctx)?,
        // Unreachable: every twin names a subcommand, so a successful parse
        // carries one. Reported rather than `unwrap`ed because a panic in a
        // session is worse than a sentence in it.
        None => ctx.surface.line(
            LineKind::Error,
            &format!("`{twin}` did not parse as a subcommand; nothing was run."),
        ),
    }
    Ok(CommandOutcome::Continue)
}

/// The `/provider list` handler (BR-1).
pub(crate) fn handle_provider_list(
    conn: &mut Connection,
    ctx: &mut UiContext<'_>,
    args: &str,
) -> anyhow::Result<CommandOutcome> {
    run_mirrored(PROVIDER_LIST, args, conn, ctx)
}

/// The `/provider add` handler (BR-1, BR-6).
///
/// The key is never an argument of this line: [`crate::provider_add_on`] reads
/// it through `ctx.prompter.ask_secret` — echo-off, into the keychain — or from
/// `TETON_PROVIDER_KEY`, exactly as the shell twin does (REQ-579 BR-2), and in
/// a session only after a default-no confirmation that names the settled
/// registration (verify M1 — a pasted second line answers "no" instead of
/// becoming the key). Its refusals — the three decisions, an endpoint the seam
/// refuses, a keychain that will not store — arrive as a rendered line rather
/// than an `Err`, so a mistyped registration does not end the session (ADR-3,
/// verify m7); only a lost socket does.
pub(crate) fn handle_provider_add(
    conn: &mut Connection,
    ctx: &mut UiContext<'_>,
    args: &str,
) -> anyhow::Result<CommandOutcome> {
    run_mirrored(PROVIDER_ADD, args, conn, ctx)
}

/// The `/boundary list` handler (BR-1).
pub(crate) fn handle_boundary_list(
    conn: &mut Connection,
    ctx: &mut UiContext<'_>,
    args: &str,
) -> anyhow::Result<CommandOutcome> {
    run_mirrored(BOUNDARY_LIST, args, conn, ctx)
}

/// The `/boundary add` handler (BR-1, BR-6).
pub(crate) fn handle_boundary_add(
    conn: &mut Connection,
    ctx: &mut UiContext<'_>,
    args: &str,
) -> anyhow::Result<CommandOutcome> {
    run_mirrored(BOUNDARY_ADD, args, conn, ctx)
}

/// The `/policy show` handler (BR-1).
pub(crate) fn handle_policy_show(
    conn: &mut Connection,
    ctx: &mut UiContext<'_>,
    args: &str,
) -> anyhow::Result<CommandOutcome> {
    run_mirrored(POLICY_SHOW, args, conn, ctx)
}

/// The `/policy set-tier` handler (BR-1, BR-6).
pub(crate) fn handle_policy_set_tier(
    conn: &mut Connection,
    ctx: &mut UiContext<'_>,
    args: &str,
) -> anyhow::Result<CommandOutcome> {
    run_mirrored(POLICY_SET_TIER, args, conn, ctx)
}

/// The `/policy set-category` handler (BR-1, BR-6).
pub(crate) fn handle_policy_set_category(
    conn: &mut Connection,
    ctx: &mut UiContext<'_>,
    args: &str,
) -> anyhow::Result<CommandOutcome> {
    run_mirrored(POLICY_SET_CATEGORY, args, conn, ctx)
}

/// The `/model list` handler (BR-1).
pub(crate) fn handle_model_list(
    conn: &mut Connection,
    ctx: &mut UiContext<'_>,
    args: &str,
) -> anyhow::Result<CommandOutcome> {
    run_mirrored(MODEL_LIST, args, conn, ctx)
}

/// The `/model status` handler (BR-1).
pub(crate) fn handle_model_status(
    conn: &mut Connection,
    ctx: &mut UiContext<'_>,
    args: &str,
) -> anyhow::Result<CommandOutcome> {
    run_mirrored(MODEL_STATUS, args, conn, ctx)
}

/// The `/doctor` handler (BR-1, BR-7).
///
/// The report is the shell twin's, with one line different: the daemon it names
/// is the one this session is already attached to
/// ([`crate::DoctorAttach::Session`]), because a fresh handshake would announce
/// an attach into the very session being diagnosed.
pub(crate) fn handle_doctor(
    conn: &mut Connection,
    ctx: &mut UiContext<'_>,
    args: &str,
) -> anyhow::Result<CommandOutcome> {
    run_mirrored(DOCTOR, args, conn, ctx)
}

/// The CLI's subcommand tree, built once.
///
/// Memoized so [`cli_path`] can hand out `&'static str` names: the classifier
/// runs on every typed line and passes the row name onward as a borrow, and a
/// tree rebuilt per call would force an allocation per token to say the same
/// thing. Building it is also not free, and the answer never changes — the tree
/// is a compile-time fact about this binary.
fn tree() -> &'static clap::Command {
    static TREE: OnceLock<clap::Command> = OnceLock::new();
    TREE.get_or_init(Cli::command)
}

/// The subcommand path `tokens` names, as deep as they name one (REQ-582 ADR-1).
///
/// `tokens` are the whitespace-split words **after** `teton` (and after any
/// leading global flag the classifier stepped over). The walk is clap's own —
/// [`clap::Command::find_subcommand`] — and each level contributes the canonical
/// `get_name()`, so a path is spelled the way the table spells a row. It stops
/// at the first token that names no subcommand, which is what makes `teton is
/// slow today` an empty path and therefore a plain prompt rather than a command.
///
/// No subcommand in this binary declares an alias today, so "canonical" and
/// "typed" are the same words for every path (verify T2). The walk goes through
/// `find_subcommand` all the same, because that is the lookup clap's own parser
/// uses: were an alias ever declared, it would resolve here without a second
/// matcher learning about it, and the classifier counts the words the walk
/// consumed rather than re-matching the row's name for exactly that case.
///
/// This is the recognition half of BR-3's "one grammar": the classifier asks the
/// parser which subcommand a line names instead of matching words itself, so a
/// subcommand renamed in `main.rs` is renamed here too and a hand-written matcher
/// cannot drift out of agreement with the parser (LESSON-529).
#[must_use]
pub(crate) fn cli_path(tokens: &[&str]) -> Vec<&'static str> {
    let mut node = tree();
    let mut path = Vec::new();
    for token in tokens {
        let Some(sub) = node.find_subcommand(token) else {
            break;
        };
        path.push(sub.get_name());
        node = sub;
    }
    path
}

/// The clap node a canonical `path` names, or `None` when it names no
/// subcommand.
fn node_at(path: &[&str]) -> Option<&'static clap::Command> {
    let mut node = tree();
    for word in path {
        node = node.find_subcommand(word)?;
    }
    Some(node)
}

/// Whether `path` names a **leaf** — a subcommand with no subcommands of its own,
/// which is to say one clap can parse to a [`crate::Command`] (verify M2).
///
/// The classifier's pre-REQ rows split on this: `cost`, `effort`, `model set`
/// and `provider test` are leaves, so a typed `teton effort max --yes` can be
/// validated whole by `Cli::try_parse_from` before its row runs; `model` is a
/// family, and `Cli::try_parse_from(["teton", "model"])` yields the family's
/// help page rather than a command — so bare `teton model` dispatches to
/// `/model` directly, as TASK-170 had it.
#[must_use]
pub(crate) fn is_leaf_path(path: &[&str]) -> bool {
    node_at(path).is_some_and(|node| node.get_subcommands().next().is_none())
}

/// Clap's own help page for a **family** path — `teton provider --help`,
/// `teton policy -h` — or `None` when `path` is not a family (verify T6).
///
/// Rendered through the same `try_parse_from` the shell would run for that
/// argv — `help_flag` is the flag as typed, so `-h` gets clap's short page and
/// `--help` its long one, exactly as a shell would — so the text is the
/// binary's own and not a second composition of it. A user who typed `--help`
/// after a family name asked for the family's page, and answering with the
/// bare-family refusal ([`refusal_for_path`]'s session-rows sentence) would
/// swallow the ask. `None` for a leaf — a leaf's `--help` is its row's to
/// render, or its refusal's to explain — and for a path that names nothing.
#[must_use]
pub(crate) fn family_help(path: &[&str], help_flag: &str) -> Option<String> {
    let node = node_at(path)?;
    // A family has at least one subcommand; a leaf's `?` bails here.
    node.get_subcommands().next()?;
    let argv: Vec<&str> = std::iter::once("teton")
        .chain(path.iter().copied())
        .chain(std::iter::once(help_flag))
        .collect();
    match Cli::try_parse_from(argv) {
        Err(err) if err.kind() == clap::error::ErrorKind::DisplayHelp => {
            Some(err.render().to_string())
        }
        // A family always answers `--help` with help; anything else here would
        // mean the tree changed under this walk, and the caller's refusal is
        // the honest fallback.
        _ => None,
    }
}

/// The one line a recognized `teton …` path with **no** session row gets back
/// (REQ-582 ADR-1, BR-4).
///
/// `path` is [`cli_path`]'s output, so it is a real subcommand path in canonical
/// spelling — this function's job is only to say why that path has no session
/// form, in one line, and where the user can go instead. Three shapes:
///
/// * a [`SHELL_ONLY`] command — `teton uninstall`, and the reason is the one
///   recorded there: it stops the daemon this session is attached to;
/// * a **family** — `teton provider`, and also `teton provider setup`, whose
///   second word names no subcommand and leaves the walk on the family. The line
///   names the session's rows *under* that family, from the table
///   ([`crate::slash::rows_under`]), because those are what the user can
///   actually type here: for `provider` that includes `/provider setup`, which
///   the CLI has no subcommand for at all;
/// * the retired `policy set` (REQ-558 AC-9): the one leaf clap hides, and the
///   one path whose reason is a sentence that already exists —
///   [`crate::POLICY_SET_RETIRED`] explains that the routing axis changed, which
///   is what a user with muscle memory needs to hear, and "no session form"
///   would tell them they mistyped (verify m6);
/// * anything else with no rows below it — a leaf with no session row and no
///   listing — says so and points at the shell.
///
/// **One line, and why it is not clap's own message.** ADR-1 wrote the family
/// arm as "clap's own error for that path, rendered", on the reading that
/// `Cli::try_parse_from(["teton", "provider"])` produces the short "requires a
/// subcommand" error that names them. It does not: the derive marks a required
/// subcommand `arg_required_else_help`, so clap answers with the **whole help
/// page** for that family — a screen of text whose longest line is the global
/// `--yes` description, and whose `Usage:` and "try '--help'" tail are a shell's
/// instructions, not a session's. A [`Surface`] line owns exactly one row
/// (newlines in it are flattened on the way out), and BR-4 and ADR-1 both say
/// one refusing line. So the sentence is composed, and from the table rather
/// than from the tree: the anti-drift property BR-3 is about is that no list is
/// maintained twice, and the table is the list that decides what runs here.
/// (A family followed by an explicit `--help` is the one case that *does* get
/// clap's page — the user asked for it — see [`family_help`].)
#[must_use]
pub(crate) fn refusal_for_path(path: &[&str]) -> String {
    let spelling = path.join(" ");
    if SHELL_ONLY.contains(&spelling.as_str()) {
        // The reason is `SHELL_ONLY`'s single entry's reason, and a unit test
        // pins that it is still the only entry: a second shell-only command
        // would need a reason of its own rather than this one.
        return format!(
            "`teton {spelling}` is shell-only: it stops the daemon this session is attached to \
             and removes its data, so nothing was run — run `teton {spelling}` from a shell \
             instead."
        );
    }
    // The hidden leaf. Matched by spelling because it is the only hidden leaf
    // (`every_cli_leaf_is_a_session_row_or_an_explicit_shell_only_exception`
    // pins that), and its sentence is the shell's own for the same argv.
    if spelling == "policy set" {
        return crate::POLICY_SET_RETIRED.to_owned();
    }
    let rows = crate::slash::rows_under(path);
    if rows.is_empty() {
        format!(
            "`teton {spelling}` has no session form — run it from a shell; {}",
            crate::slash::HELP_HINT
        )
    } else {
        format!(
            "`teton {spelling} …` names a family rather than a command — in this session: {}.",
            rows.iter()
                .map(|row| format!("/{row}"))
                .collect::<Vec<_>>()
                .join(", ")
        )
    }
}

/// Every leaf subcommand path in the CLI's tree, with whether clap hides it.
///
/// The other half of ADR-8's completeness property, and it lives here rather
/// than in the test that uses it because the tree is this module's subject: the
/// test belongs beside the table it checks (`slash.rs`), the walk belongs beside
/// [`cli_path`], which walks the same tree for the classifier.
///
/// A *leaf* is a subcommand with no subcommands of its own — `provider add`, not
/// `provider` — because a leaf is what a user can actually run.
#[cfg(test)]
pub(crate) fn leaf_command_paths() -> Vec<(String, bool)> {
    fn walk(node: &clap::Command, prefix: &[&str], out: &mut Vec<(String, bool)>) {
        for sub in node.get_subcommands() {
            let mut path = prefix.to_vec();
            path.push(sub.get_name());
            if sub.get_subcommands().next().is_none() {
                out.push((path.join(" "), sub.is_hide_set()));
            } else {
                walk(sub, &path, out);
            }
        }
    }
    let mut out = Vec::new();
    walk(tree(), &[], &mut out);
    out
}

/// Every **family** subcommand path in the CLI's tree — a node with subcommands
/// of its own (`provider`, `policy`, `model`, `boundary`), the root excluded.
///
/// The complement of [`leaf_command_paths`], for the tests that walk what a
/// family answers with: `--help` on a family is [`family_help`]'s page (verify
/// T6), and it is composed by the same parser under the same line bound as a
/// leaf's, so the bound test walks both.
#[cfg(test)]
pub(crate) fn family_command_paths() -> Vec<String> {
    fn walk(node: &clap::Command, prefix: &[&str], out: &mut Vec<String>) {
        for sub in node.get_subcommands() {
            if sub.get_subcommands().next().is_none() {
                continue;
            }
            let mut path = prefix.to_vec();
            path.push(sub.get_name());
            out.push(path.join(" "));
            walk(sub, &path, out);
        }
    }
    let mut out = Vec::new();
    walk(tree(), &[], &mut out);
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::prompt::ScriptedPrompter;
    use crate::render::RecordingSurface;
    use crate::session_ui::SessionState;
    use std::os::unix::net::UnixStream;

    /// A session context for a mirrored row. `typed_input` defaults to a
    /// terminal — the write rows' refusal is reached by flipping it, never by
    /// what this process's stdin happens to be.
    fn session_ctx<'a>(
        surface: &'a mut RecordingSurface,
        state: &'a mut SessionState,
        prompter: &'a mut ScriptedPrompter,
        typed_input: bool,
    ) -> UiContext<'a> {
        UiContext {
            surface,
            state,
            prompter,
            answer_permissions: true,
            answer_model_proposals: true,
            auto_accept_model: false,
            typed_input,
            session_id: None,
        }
    }

    /// Every JSON-RPC method name written to the connection's socket, in order
    /// — the shared fixture reader ([`crate::client::methods_written`]), named
    /// for what these tests ask of it.
    ///
    /// Non-blocking, so "nothing was sent" is an assertion a test can make
    /// without waiting for a daemon that does not exist — which is what AC-4 and
    /// AC-7 are actually about.
    fn methods_sent(peer: &UnixStream) -> Vec<String> {
        crate::client::methods_written(peer)
    }

    /// Whether `TETON_PROVIDER_KEY` is exported in this process — the same
    /// guard `main.rs`'s `provider_add_on` tests take, for a sharper reason
    /// here: the `/provider add` tests below run through the shipped
    /// dispatcher, whose keychain is the **real** one, and `read_secret` takes
    /// the environment's key ahead of the prompter. A test that confirms "y"
    /// on that path with the variable exported would store a developer's real
    /// key into their login keychain, so those tests skip — out loud — rather
    /// than run.
    fn provider_key_exported() -> bool {
        let exported = std::env::var("TETON_PROVIDER_KEY").is_ok_and(|key| !key.trim().is_empty());
        if exported {
            eprintln!("skipped: TETON_PROVIDER_KEY is exported");
        }
        exported
    }

    /// A `config/get` answer with nothing configured — enough for the read rows
    /// to render, and the shape their shared bodies deserialize.
    fn empty_config() -> serde_json::Value {
        serde_json::to_value(teton_protocol::methods::ConfigGetResult::default())
            .expect("a config snapshot serializes")
    }

    /// A `model/list` answer with an empty catalog. Built typed rather than as
    /// hand-written JSON so a field added to the wire type fails to compile here
    /// instead of deserializing into a surprise at runtime.
    fn empty_catalog() -> serde_json::Value {
        use teton_protocol::events::{ChosenBand, GpuClass, ProbeReportView};
        serde_json::to_value(teton_protocol::methods::ModelListResult {
            probe: ProbeReportView {
                total_ram_bytes: 16 * 1024 * 1024 * 1024,
                free_disk_bytes: 64 * 1024 * 1024 * 1024,
                gpu_class: GpuClass::AppleSilicon,
                chosen_band: ChosenBand::Small,
                reason: "under test".to_owned(),
            },
            models: Vec::new(),
            selection: None,
        })
        .expect("a model list serializes")
    }

    /// ADR-1's walk, on the four shapes the classifier will meet.
    #[test]
    fn cli_path_walks_the_parsers_own_tree_as_deep_as_the_tokens_go() {
        assert_eq!(cli_path(&["provider", "list"]), vec!["provider", "list"]);
        assert_eq!(cli_path(&["doctor"]), vec!["doctor"]);
        // The path stops where the subcommands stop: everything after it is the
        // row's argument, and clap's grammar — not this walk — validates it.
        assert_eq!(
            cli_path(&["policy", "set-tier", "build", "kimi"]),
            vec!["policy", "set-tier"]
        );
        // A subcommand with no session row still resolves: it is the classifier
        // that decides what to do about one, and it cannot decide without being
        // told the line names a real command.
        assert_eq!(cli_path(&["uninstall"]), vec!["uninstall"]);
        // A family typed bare is a one-element path, not a row.
        assert_eq!(cli_path(&["provider"]), vec!["provider"]);
        // And a line that names no subcommand at all is empty — the shape that
        // keeps "teton is slow today" a prompt (BR-4).
        assert!(cli_path(&["is", "slow"]).is_empty());
        assert!(cli_path(&[]).is_empty());
    }

    /// BR-3 / AC-2's parse leg: the argv a row builds parses to the **same**
    /// `Command` the shell's argv parses to.
    ///
    /// Compared over `Debug`, which renders the whole parsed tree — every
    /// positional, every flag, every value enum — rather than a field this test
    /// happened to think of. The four lines are the spec's own examples
    /// (LESSON-512: a named example is a test case).
    #[test]
    fn a_row_parses_its_argument_exactly_as_the_shell_parses_its_argv() {
        for (twin, args, argv) in [
            (
                "teton policy set-tier",
                "build kimi --fallback local",
                vec![
                    "teton",
                    "policy",
                    "set-tier",
                    "build",
                    "kimi",
                    "--fallback",
                    "local",
                ],
            ),
            (
                "teton policy set-category",
                "edit kimi",
                vec!["teton", "policy", "set-category", "edit", "kimi"],
            ),
            (
                "teton boundary add",
                "src/** --mode local-only",
                vec!["teton", "boundary", "add", "src/**", "--mode", "local-only"],
            ),
            (
                "teton provider add",
                "kimi --kind openai-compatible --endpoint https://x/v1/chat/completions \
                 --model kimi-k3",
                vec![
                    "teton",
                    "provider",
                    "add",
                    "kimi",
                    "--kind",
                    "openai-compatible",
                    "--endpoint",
                    "https://x/v1/chat/completions",
                    "--model",
                    "kimi-k3",
                ],
            ),
            // REQ-586 BR-3: the window flags are part of the same grammar, so
            // the session spelling has to reach the same parse as the shell's.
            // A flag added to the clap tree but not to the mirrored row would
            // be a command a user can run in one place and not the other.
            (
                "teton provider add",
                "kimi --kind openai-compatible --endpoint https://x/v1/chat/completions \
                 --model kimi-k3 --max-context 128000 --context-budget-cap 40000",
                vec![
                    "teton",
                    "provider",
                    "add",
                    "kimi",
                    "--kind",
                    "openai-compatible",
                    "--endpoint",
                    "https://x/v1/chat/completions",
                    "--model",
                    "kimi-k3",
                    "--max-context",
                    "128000",
                    "--context-budget-cap",
                    "40000",
                ],
            ),
        ] {
            let from_session = Cli::try_parse_from(mirrored_argv(twin, args))
                .unwrap_or_else(|err| panic!("`/{twin}` did not parse: {err}"));
            let from_shell = Cli::try_parse_from(&argv).expect("the shell's own argv");
            assert_eq!(
                format!("{:?}", from_session.command),
                format!("{:?}", from_shell.command),
                "`{twin} {args}` parsed differently from `{argv:?}`"
            );
        }
    }

    /// ADR-2's tokenization, stated as the rule it is: whitespace, and nothing
    /// else. Repeated spaces collapse; quotes are ordinary characters.
    #[test]
    fn the_argv_is_split_on_whitespace_and_quotes_are_not_interpreted() {
        assert_eq!(
            mirrored_argv("teton policy set-tier", "  build   kimi  "),
            vec!["teton", "policy", "set-tier", "build", "kimi"]
        );
        assert_eq!(
            mirrored_argv("teton boundary add", "\"src/a b\""),
            vec!["teton", "boundary", "add", "\"src/a", "b\""]
        );
        assert_eq!(
            mirrored_argv("teton provider list", ""),
            vec!["teton", "provider", "list"]
        );
    }

    /// AC-7: a bad line prints the CLI parser's **own** error and issues no RPC.
    ///
    /// **Every** line of clap's message reaches the surface, in order, once —
    /// the first as the error (minus the `error: ` the surface class itself
    /// supplies), the rest as the neutral context lines the shell prints under
    /// it, and the blank lines between them as empty Info lines (verify m1):
    /// the shell prints those blanks, and byte parity is the claim. Both spec
    /// examples are here: a missing positional and a value outside the enum.
    #[test]
    fn a_bad_argument_renders_claps_own_error_and_sends_nothing() {
        for (args, argv) in [
            ("build", vec!["teton", "policy", "set-tier", "build"]),
            (
                "summit kimi",
                vec!["teton", "policy", "set-tier", "summit", "kimi"],
            ),
        ] {
            let (mut conn, peer) = Connection::scripted(&[]);
            let mut surface = RecordingSurface::new();
            let mut state = SessionState::new();
            let mut prompter = ScriptedPrompter::new(&[]);
            {
                let mut ctx = session_ctx(&mut surface, &mut state, &mut prompter, true);
                let outcome =
                    run_mirrored_seamed(POLICY_SET_TIER, args, &mut conn, &mut ctx, false)
                        .expect("a parse error never fails the command");
                assert_eq!(outcome, CommandOutcome::Continue);
            }

            let rendered = Cli::try_parse_from(&argv)
                .expect_err("the shell rejects it too")
                .render()
                .to_string();
            let mut expected = rendered.lines();
            let first = expected.next().expect("clap says something");
            assert_eq!(
                surface.lines_of(LineKind::Error),
                vec![first
                    .strip_prefix("error: ")
                    .expect("clap leads with its own error: prefix")],
                "`/policy set-tier {args}` did not print the parser's own first line"
            );
            let context: Vec<&str> = expected.collect();
            assert!(
                context.iter().any(|line| line.trim().is_empty()),
                "clap's own message carries a blank line, or this test proves nothing \
                 about m1: {rendered:?}"
            );
            assert_eq!(
                surface.lines_of(LineKind::Info),
                context,
                "`/policy set-tier {args}` dropped or reworded the parser's context lines"
            );
            assert!(
                methods_sent(&peer).is_empty(),
                "`/policy set-tier {args}` reached the daemon"
            );
        }
    }

    /// `--help` on a row is clap's help for that subcommand, and it is not an
    /// error: a user who asked for help got what they asked for (AC-7). Blank
    /// lines included, as the shell prints them (m1).
    #[test]
    fn a_help_request_renders_claps_help_for_that_subcommand() {
        let (mut conn, peer) = Connection::scripted(&[]);
        let mut surface = RecordingSurface::new();
        let mut state = SessionState::new();
        let mut prompter = ScriptedPrompter::new(&[]);
        {
            let mut ctx = session_ctx(&mut surface, &mut state, &mut prompter, true);
            run_mirrored_seamed(POLICY_SET_TIER, "--help", &mut conn, &mut ctx, false)
                .expect("a help request never fails the command");
        }

        let rendered = Cli::try_parse_from(["teton", "policy", "set-tier", "--help"])
            .expect_err("clap reports help as an Err")
            .render()
            .to_string();
        assert_eq!(
            surface.lines_of(LineKind::Info),
            rendered.lines().collect::<Vec<_>>()
        );
        assert!(
            surface.lines_of(LineKind::Error).is_empty(),
            "asking for help is not an error: {:?}",
            surface.calls
        );
        assert!(methods_sent(&peer).is_empty());
    }

    /// **M3.** The parse comes before the write gate, so `--help` on a write
    /// row answers on a pipe: help changes nothing and the gate has no business
    /// refusing it. The same pipe still refuses the row's real arguments.
    #[test]
    fn help_on_a_piped_write_row_renders_and_the_write_is_still_refused() {
        let (mut conn, peer) = Connection::scripted(&[]);
        let mut surface = RecordingSurface::new();
        let mut state = SessionState::new();
        let mut prompter = ScriptedPrompter::new(&[]);
        {
            let mut ctx = session_ctx(&mut surface, &mut state, &mut prompter, false);
            run_mirrored_seamed(POLICY_SET_TIER, "--help", &mut conn, &mut ctx, false)
                .expect("a help request never fails the command");
        }
        let expected = Cli::try_parse_from(["teton", "policy", "set-tier", "--help"])
            .expect_err("clap reports help as an Err")
            .render()
            .to_string();
        assert_eq!(
            surface.lines_of(LineKind::Info),
            expected.lines().collect::<Vec<_>>(),
            "help on a pipe must be the page, not the refusal: {:?}",
            surface.calls
        );
        assert!(
            surface.lines_of(LineKind::Error).is_empty(),
            "the gate refused a help request: {:?}",
            surface.calls
        );
        assert!(methods_sent(&peer).is_empty());

        // The gate is intact for a real write on the same pipe.
        let mut surface = RecordingSurface::new();
        {
            let mut ctx = session_ctx(&mut surface, &mut state, &mut prompter, false);
            run_mirrored_seamed(POLICY_SET_TIER, "build kimi", &mut conn, &mut ctx, false)
                .expect("a refusal never fails the command");
        }
        let errors = surface.lines_of(LineKind::Error);
        assert_eq!(errors.len(), 1, "{:?}", surface.calls);
        assert!(errors[0].contains("typed-input-only"), "{}", errors[0]);
        assert!(methods_sent(&peer).is_empty());
    }

    /// **T3.** `--version` on a row is an unexpected argument, not a version
    /// render: the binary's version flag is top-level and is not propagated to
    /// its subcommands, so `render_clap_error`'s `DisplayVersion` arm is
    /// unreachable from a row (see its doc for why it stays).
    #[test]
    fn version_on_a_row_is_an_unexpected_argument() {
        let (mut conn, peer) = Connection::scripted(&[]);
        let mut surface = RecordingSurface::new();
        let mut state = SessionState::new();
        let mut prompter = ScriptedPrompter::new(&[]);
        {
            let mut ctx = session_ctx(&mut surface, &mut state, &mut prompter, true);
            run_mirrored_seamed(PROVIDER_LIST, "--version", &mut conn, &mut ctx, false)
                .expect("a parse error never fails the command");
        }
        let errors = surface.lines_of(LineKind::Error);
        assert_eq!(errors.len(), 1, "{:?}", surface.calls);
        assert!(
            errors[0].contains("unexpected argument '--version'"),
            "{}",
            errors[0]
        );
        // And clap agrees it is not a version request.
        let kind = Cli::try_parse_from(["teton", "provider", "list", "--version"])
            .expect_err("rejected")
            .kind();
        assert_ne!(kind, clap::error::ErrorKind::DisplayVersion);
        assert!(methods_sent(&peer).is_empty());
    }

    /// **m2's precondition.** The bound clears every line the binary's own
    /// parser composes — every leaf's long and short help, and every family's
    /// (the page [`family_help`] renders for `teton provider --help`, verify
    /// T6) — so nothing but a user's own token ever meets the ellipsis. A doc
    /// comment that grows past the bound fails here, by name, rather than
    /// being truncated in a session.
    #[test]
    fn every_help_line_the_binary_composes_clears_the_line_bound() {
        let mut checked = 0;
        let leaves = leaf_command_paths().into_iter().map(|(path, _)| path);
        let families = family_command_paths();
        assert!(
            families.iter().any(|path| path == "provider") && families.len() >= 4,
            "the family walk found too little: {families:?}"
        );
        for path in leaves.chain(families) {
            for flag in ["--help", "-h"] {
                let argv: Vec<&str> = std::iter::once("teton")
                    .chain(path.split_whitespace())
                    .chain(std::iter::once(flag))
                    .collect();
                let text = Cli::try_parse_from(&argv)
                    .expect_err("help is reported as an Err")
                    .render()
                    .to_string();
                for line in text.lines() {
                    checked += 1;
                    assert!(
                        line.chars().count() <= CLAP_LINE_MAX_CHARS,
                        "`teton {path} {flag}` composes a line of {} chars, over the \
                         {CLAP_LINE_MAX_CHARS} bound the session renders clap text under; \
                         shorten the doc comment or raise the bound:\n{line}",
                        line.chars().count()
                    );
                }
            }
        }
        assert!(checked > 100, "the walk found almost nothing: {checked}");
    }

    /// **m2.** A user's own token quoted back by clap is bounded and defused:
    /// a pasted paragraph as a stray positional does not come back in full.
    #[test]
    fn a_long_stray_token_is_bounded_when_clap_quotes_it_back() {
        let pasted = format!("sk-{}", "A".repeat(2 * CLAP_LINE_MAX_CHARS));
        let (mut conn, peer) = Connection::scripted(&[]);
        let mut surface = RecordingSurface::new();
        let mut state = SessionState::new();
        let mut prompter = ScriptedPrompter::new(&[]);
        {
            let mut ctx = session_ctx(&mut surface, &mut state, &mut prompter, true);
            run_mirrored_seamed(PROVIDER_LIST, &pasted, &mut conn, &mut ctx, false)
                .expect("a parse error never fails the command");
        }
        let errors = surface.lines_of(LineKind::Error);
        assert_eq!(errors.len(), 1, "{:?}", surface.calls);
        assert!(
            !errors[0].contains(&pasted),
            "the whole token was echoed back: {}",
            errors[0]
        );
        assert!(
            errors[0].chars().count() <= CLAP_LINE_MAX_CHARS + 1,
            "the line is over the bound: {}",
            errors[0]
        );
        assert!(errors[0].ends_with('…'), "{}", errors[0]);
        // Control characters in a quoted token are defused too, on every line.
        let mut surface = RecordingSurface::new();
        {
            let mut ctx = session_ctx(&mut surface, &mut state, &mut prompter, true);
            run_mirrored_seamed(PROVIDER_LIST, "x\x1b[2Jy", &mut conn, &mut ctx, false)
                .expect("a parse error never fails the command");
        }
        for call in &surface.calls {
            let crate::render::Rendered::Line(_, text) = call else {
                continue;
            };
            assert!(
                !text.chars().any(char::is_control),
                "a control character reached the surface: {text:?}"
            );
        }
        assert!(methods_sent(&peer).is_empty());
    }

    /// AC-4: on a pipe, every write row refuses with one line naming its shell
    /// twin, and sends nothing — no parse, no RPC.
    #[test]
    fn a_write_row_on_a_pipe_names_its_shell_twin_and_sends_nothing() {
        for (row, args) in [
            (POLICY_SET_TIER, "build kimi"),
            (POLICY_SET_CATEGORY, "edit kimi"),
            (BOUNDARY_ADD, "src/** --mode local-only"),
            (
                PROVIDER_ADD,
                "kimi --kind openai-compatible --endpoint https://x/v1 --model k3",
            ),
        ] {
            let twin = row.shell;
            let (mut conn, peer) = Connection::scripted(&[]);
            let mut surface = RecordingSurface::new();
            let mut state = SessionState::new();
            // One answer that must never be read, and it is a decline rather
            // than a key: `provider add` runs through the shipped dispatcher,
            // whose keychain is the real one, so no test on this path may
            // script a key-shaped value (see
            // `a_duplicate_id_is_refused_through_the_session_row_before_any_question`).
            let mut prompter = ScriptedPrompter::new(&["n"]);
            {
                let mut ctx = session_ctx(&mut surface, &mut state, &mut prompter, false);
                let outcome = run_mirrored_seamed(row, args, &mut conn, &mut ctx, false)
                    .expect("a refusal never fails the command");
                assert_eq!(outcome, CommandOutcome::Continue);
            }

            let lines = surface.lines_of(LineKind::Error);
            assert_eq!(
                lines.len(),
                1,
                "one line, and only one: {:?}",
                surface.calls
            );
            assert!(lines[0].contains(twin), "{}", lines[0]);
            assert!(lines[0].contains("not a terminal"), "{}", lines[0]);
            assert_eq!(surface.calls.len(), 1, "{:?}", surface.calls);
            assert!(
                methods_sent(&peer).is_empty(),
                "`{twin}` reached the daemon"
            );
            assert!(
                prompter.secrets.is_empty() && prompter.asked == 0,
                "`{twin}` asked the user something after refusing"
            );
        }
    }

    /// The six read rows, each with the one method its shared body sends and a
    /// scripted answer that body can render.
    fn read_rows() -> Vec<(Mirror, &'static str, serde_json::Value)> {
        vec![
            (PROVIDER_LIST, "config/get", empty_config()),
            (BOUNDARY_LIST, "config/get", empty_config()),
            (POLICY_SHOW, "config/get", empty_config()),
            (DOCTOR, "config/get", empty_config()),
            (MODEL_LIST, "model/list", empty_catalog()),
            (
                MODEL_STATUS,
                "model/status",
                serde_json::to_value(teton_protocol::methods::ModelStatusResult::default())
                    .expect("a model status serializes"),
            ),
        ]
    }

    /// BR-11: a read row never consults the gate. The same piped session that
    /// refuses a write answers a read, which is what makes the e2e suites — all
    /// of which drive a pipe — able to assert AC-1 at all. Over all six read
    /// rows (verify T11), because a `writes: true` slipped onto one of them
    /// would refuse exactly that row and no other.
    #[test]
    fn a_read_row_ignores_the_write_gate() {
        for (row, method, scripted) in read_rows() {
            let twin = row.shell;
            let (mut conn, peer) = Connection::scripted(&[scripted]);
            let mut surface = RecordingSurface::new();
            let mut state = SessionState::new();
            let mut prompter = ScriptedPrompter::new(&[]);
            {
                let mut ctx = session_ctx(&mut surface, &mut state, &mut prompter, false);
                run_mirrored_seamed(row, "", &mut conn, &mut ctx, false)
                    .unwrap_or_else(|err| panic!("`{twin}` did not run on a pipe: {err:#}"));
            }
            assert_eq!(methods_sent(&peer), vec![method], "`{twin}`");
            assert!(
                !surface
                    .lines_of(LineKind::Error)
                    .iter()
                    .any(|line| line.contains("typed-input-only")),
                "`{twin}` met the write gate on a pipe: {:?}",
                surface.calls
            );
            conn.assert_all_consumed();
        }
    }

    /// AC-1's unit leg: each read row reaches the shared body its shell twin
    /// runs, over the session's own connection, and sends that body's one method.
    ///
    /// The method name is the assertion because it is the thing BR-2 is about —
    /// "the same daemon method with the same params". What the bodies *render*
    /// is pinned where those bodies live and, byte for byte against the shell,
    /// by AC-1's e2e diff.
    #[test]
    fn every_read_row_sends_its_shell_twins_method_on_the_sessions_connection() {
        for (row, method, scripted) in read_rows() {
            let twin = row.shell;
            let (mut conn, peer) = Connection::scripted(&[scripted]);
            let mut surface = RecordingSurface::new();
            let mut state = SessionState::new();
            let mut prompter = ScriptedPrompter::new(&[]);
            {
                let mut ctx = session_ctx(&mut surface, &mut state, &mut prompter, true);
                let outcome = run_mirrored_seamed(row, "", &mut conn, &mut ctx, false)
                    .expect("a read row answers or reports");
                assert_eq!(outcome, CommandOutcome::Continue);
            }
            assert_eq!(methods_sent(&peer), vec![method], "`{twin}`");
            assert!(
                !surface.calls.is_empty(),
                "`{twin}` rendered nothing at all"
            );
            conn.assert_all_consumed();
        }
    }

    /// **m7 / T9.** A transport failure is the one thing a mirrored row does
    /// *not* turn into a sentence: it propagates, and the entry loop ends the
    /// session on it exactly as it does when `/cost` meets one. A connection
    /// scripted with no answers is a daemon that hung up mid-call, and every
    /// read row must return `Err` on it rather than render "connection closed"
    /// and carry on as though the next command might fare better.
    #[test]
    fn a_lost_connection_propagates_out_of_every_read_row() {
        for (row, _, _) in read_rows() {
            let twin = row.shell;
            let (mut conn, _peer) = Connection::scripted(&[]);
            let mut surface = RecordingSurface::new();
            let mut state = SessionState::new();
            let mut prompter = ScriptedPrompter::new(&[]);
            let result = {
                let mut ctx = session_ctx(&mut surface, &mut state, &mut prompter, true);
                run_mirrored_seamed(row, "", &mut conn, &mut ctx, false)
            };
            let err = result.expect_err(&format!(
                "`{twin}` swallowed a lost connection: {:?}",
                surface.calls
            ));
            assert!(
                format!("{err:#}").contains("connection to the daemon closed"),
                "`{twin}` failed for another reason: {err:#}"
            );
        }
    }

    /// BR-7: the session's `/doctor` names the daemon **this** connection
    /// handshook with rather than dialling the socket again — the one line that
    /// differs from the shell twin's report (ADR-5).
    #[test]
    fn doctor_reports_the_connection_the_session_already_has() {
        let (mut conn, _peer) = Connection::scripted(&[empty_config()]);
        let mut surface = RecordingSurface::new();
        let mut state = SessionState::new();
        let mut prompter = ScriptedPrompter::new(&[]);
        {
            let mut ctx = session_ctx(&mut surface, &mut state, &mut prompter, true);
            run_mirrored_seamed(DOCTOR, "", &mut conn, &mut ctx, false)
                .expect("the report renders");
        }
        let report = surface.lines_of(LineKind::Info).join("\n");
        assert!(
            report.contains("this session's connection"),
            "the daemon line must name the session's own attach: {report}"
        );
        assert!(
            !report.contains("protocol "),
            "a session performs no handshake here, so there is no version to \
             quote: {report}"
        );
    }

    /// ADR-3: a `provider add` refusal is a rendered line and the session
    /// continues. An `Err` out of the handler would end it — the entry loop
    /// propagates one — which is a session lost to a mistyped registration.
    ///
    /// The remote-without-`--model` refusal is the one that lands before any RPC
    /// and before the key is read, which is also what keeps a credential from
    /// being typed into a command that was always going to fail.
    #[test]
    fn a_provider_add_refusal_renders_a_line_and_keeps_the_session() {
        let (mut conn, peer) = Connection::scripted(&[]);
        let mut surface = RecordingSurface::new();
        let mut state = SessionState::new();
        // A decline, not a key: the real keychain sits behind this dispatcher
        // path (see the duplicate-id test below), so the one answer scripted
        // for a question that must never be asked is one that stores nothing.
        let mut prompter = ScriptedPrompter::new(&["n"]);
        {
            let mut ctx = session_ctx(&mut surface, &mut state, &mut prompter, true);
            let outcome = run_mirrored_seamed(
                PROVIDER_ADD,
                "kimi --kind openai-compatible --endpoint https://x/v1/chat/completions",
                &mut conn,
                &mut ctx,
                false,
            )
            .expect("a refusal is not a failure of the command");
            assert_eq!(outcome, CommandOutcome::Continue);
        }
        let lines = surface.lines_of(LineKind::Error);
        assert_eq!(
            lines.len(),
            1,
            "one line, and only one: {:?}",
            surface.calls
        );
        assert!(lines[0].contains("--model"), "{}", lines[0]);
        assert!(methods_sent(&peer).is_empty(), "a refusal must cost no RPC");
        assert!(
            prompter.secrets.is_empty(),
            "a command that was always going to fail must not read a key first"
        );
    }

    /// AC-3's seam: `/provider add` reads its key through the **hiding** prompt
    /// and never off the command line — and, since the verify pass (M1), only
    /// after the session's own default-no confirmation.
    ///
    /// Four claims, one run:
    ///
    /// * the row's **first** question is the plain-prompt confirmation, and it
    ///   names the settled registration — the id, the kind, the model and the
    ///   endpoint — before any key is asked for;
    /// * the credential question went through [`Prompter::ask_secret`]
    ///   (`prompter.secrets`) and not [`Prompter::ask`] — the choice REQ-572
    ///   made and the one a later edit could silently swap back;
    /// * those two are the row's **only** questions, in that order, so nothing
    ///   else in this flow reads from the user at a point a key could be
    ///   mistaken for an answer;
    /// * an empty key answer refuses, and the only method that reached the
    ///   daemon is the duplicate probe — no `config/set`, so no registration and
    ///   no reference to a key that does not exist.
    ///
    /// The row is driven through the shipped dispatcher, which hands the body
    /// the **real** keychain — so the key answer here is empty by necessity: a
    /// value would go to whoever's login keychain ran `cargo test`. The
    /// completed store, the wire bytes and the undo are asserted where the
    /// keychain *is* a double: `main.rs`'s `provider_add_on` tests over
    /// `MockKeychain` (M4). The terminal's echo bit is `pty_e2e.rs`'s.
    #[test]
    fn provider_add_reads_its_key_through_the_hiding_prompt_and_never_as_a_flag() {
        if provider_key_exported() {
            return;
        }
        // A `--key` flag does not exist and must not: the credential would be in
        // the session's scrollback, in any recording of it, and — for a typed
        // `teton provider add … --key …` — in a shell history file.
        let with_flag = Cli::try_parse_from(mirrored_argv(
            PROVIDER_ADD.shell,
            "kimi2 --kind openai-compatible --endpoint https://x/v1/chat/completions \
             --model kimi-k3 --key sk-not-a-thing",
        ))
        .expect_err("`--key` must not parse");
        assert!(
            with_flag.render().to_string().contains("--key"),
            "the parser must reject `--key` by name: {with_flag}"
        );

        let (mut conn, peer) = Connection::scripted(&[empty_config()]);
        let mut surface = RecordingSurface::new();
        let mut state = SessionState::new();
        // "y" to the confirmation, then empty for the key: "nothing was
        // typed", the one error `read_secret` has.
        let mut prompter = ScriptedPrompter::new(&["y", ""]);
        {
            let mut ctx = session_ctx(&mut surface, &mut state, &mut prompter, true);
            let outcome = run_mirrored_seamed(
                PROVIDER_ADD,
                "kimi2 --kind openai-compatible --endpoint https://x/v1/chat/completions \
                 --model kimi-k3",
                &mut conn,
                &mut ctx,
                false,
            )
            .expect("a missing key is a refusal, not a failure");
            assert_eq!(outcome, CommandOutcome::Continue);
        }

        assert_eq!(
            prompter.asked, 2,
            "the confirmation and the credential are the row's only questions: {:?}",
            prompter.questions
        );
        let confirm = &prompter.questions[0];
        assert!(confirm.contains("[y/N]"), "default-no: {confirm}");
        for named in [
            "kimi2",
            "openai-compatible",
            "kimi-k3",
            "https://x/v1/chat/completions",
        ] {
            assert!(
                confirm.contains(named),
                "the confirmation must name `{named}`: {confirm}"
            );
        }
        assert!(
            confirm.contains("the key is read next"),
            "the confirmation must say what a `y` consents to: {confirm}"
        );
        assert_eq!(
            prompter.secrets.len(),
            1,
            "the key must be read through `ask_secret`, once: {:?}",
            prompter.secrets
        );
        assert_eq!(
            prompter.secrets[0], prompter.questions[1],
            "the credential is the second question, through the hiding path"
        );
        assert!(
            prompter.secrets[0].contains("API key for `kimi2`")
                && prompter.secrets[0].contains("not shown"),
            "the question must name the provider and promise not to show the \
             value: {:?}",
            prompter.secrets[0]
        );
        let lines = surface.lines_of(LineKind::Error);
        assert_eq!(
            lines.len(),
            1,
            "one line, and only one: {:?}",
            surface.calls
        );
        assert!(lines[0].contains("no API key provided"), "{}", lines[0]);
        assert_eq!(
            methods_sent(&peer),
            vec!["config/get"],
            "a registration that got no key must send nothing but the duplicate \
             probe"
        );
        conn.assert_all_consumed();
    }

    /// **T5.** The duplicate-id refusal through the session row: one line, the
    /// probe is the only RPC, and no question is asked — the key is not read
    /// for a registration that was never going to happen (BUG-155).
    #[test]
    fn a_duplicate_id_is_refused_through_the_session_row_before_any_question() {
        if provider_key_exported() {
            return;
        }
        let existing = teton_protocol::methods::ConfigGetResult {
            snapshot: teton_protocol::methods::ConfigSnapshot {
                providers: vec![teton_protocol::methods::ProviderConfig {
                    id: teton_protocol::ProviderId::from("kimi"),
                    kind: teton_protocol::ProviderKind::OpenaiCompatible,
                    endpoint: Some("https://api.moonshot.ai/v1/chat/completions".to_owned()),
                    model: Some("kimi-k3".to_owned()),
                    auth_ref: Some("keychain://teton/kimi".to_owned()),
                    max_context: None,
                    context_budget_cap: None,
                    floored_budget: None,
                }],
                ..Default::default()
            },
        };
        let (mut conn, peer) = Connection::scripted(&[serde_json::to_value(existing).unwrap()]);
        let mut surface = RecordingSurface::new();
        let mut state = SessionState::new();
        // "y" for a confirmation that must never be asked, and **no key answer**
        // (see the test above): this row runs through the shipped dispatcher,
        // which hands `provider_add_on` the REAL keychain, so under the very
        // mutation this test guards — the probe not refusing — a scripted key
        // would be stored into whoever ran `cargo test`'s login keychain as
        // `teton/kimi`, and the `config/set` that follows would meet an
        // exhausted scripted connection: a transport failure, which leaves the
        // stored key in place (BUG-171's undo is for a daemon that answered
        // no). An unexpected key read therefore gets an empty answer, which
        // refuses before any store. No test on this dispatcher path may script
        // a non-empty key.
        let mut prompter = ScriptedPrompter::new(&["y"]);
        {
            let mut ctx = session_ctx(&mut surface, &mut state, &mut prompter, true);
            let outcome = run_mirrored_seamed(
                PROVIDER_ADD,
                "kimi --kind openai-compatible --endpoint https://x/v1/chat/completions \
                 --model kimi-k3",
                &mut conn,
                &mut ctx,
                false,
            )
            .expect("a refusal is not a failure of the command");
            assert_eq!(outcome, CommandOutcome::Continue);
        }
        let lines = surface.lines_of(LineKind::Error);
        assert_eq!(lines.len(), 1, "{:?}", surface.calls);
        assert!(
            lines[0].contains("`kimi` is already registered"),
            "{}",
            lines[0]
        );
        assert_eq!(methods_sent(&peer), vec!["config/get"]);
        assert_eq!(
            prompter.asked, 0,
            "a duplicate id asks nothing: {:?}",
            prompter.questions
        );
        conn.assert_all_consumed();
    }

    /// **m7.** An endpoint the registration seam refuses (REQ-578 BR-5) is a
    /// rendered line and the session continues — the same sentence the shell
    /// exits with, and no key read for a registration that was never going to
    /// be stored.
    #[test]
    fn a_refused_endpoint_renders_the_seams_sentence_and_keeps_the_session() {
        if provider_key_exported() {
            return;
        }
        let (mut conn, peer) = Connection::scripted(&[empty_config()]);
        let mut surface = RecordingSurface::new();
        let mut state = SessionState::new();
        // "y" only, no key answer — the real keychain is behind this dispatcher
        // path (see the duplicate-id test above for the full reason): if the
        // seam ever stopped refusing, the key read gets an empty answer and
        // refuses before any store.
        let mut prompter = ScriptedPrompter::new(&["y"]);
        {
            let mut ctx = session_ctx(&mut surface, &mut state, &mut prompter, true);
            let outcome = run_mirrored_seamed(
                PROVIDER_ADD,
                // A relative endpoint: not an absolute http(s) URL, which the
                // seam refuses before any credential is asked for.
                "kimi2 --kind openai-compatible --endpoint api.example/v1 --model kimi-k3",
                &mut conn,
                &mut ctx,
                false,
            )
            .expect("a seam refusal is not a failure of the command");
            assert_eq!(outcome, CommandOutcome::Continue);
        }
        let lines = surface.lines_of(LineKind::Error);
        assert_eq!(lines.len(), 1, "{:?}", surface.calls);
        let expected = match crate::settle_endpoint_text(
            "kimi2",
            teton_protocol::ProviderKind::OpenaiCompatible,
            Some("api.example/v1"),
        ) {
            Err(sentence) => sentence,
            Ok(_) => panic!("the seam refuses a relative endpoint"),
        };
        assert_eq!(lines[0], expected, "not the seam's own sentence");
        assert_eq!(methods_sent(&peer), vec!["config/get"]);
        assert_eq!(
            prompter.asked, 0,
            "a refused endpoint asks nothing: {:?}",
            prompter.questions
        );
        conn.assert_all_consumed();
    }

    /// A typed `--yes` on `/provider add` does **not** pre-answer the
    /// register-this? confirmation: the flag is the shell's, the row drops it
    /// and says so ([`shell_flags_line`]), and the consent the confirmation
    /// stands for stays the session's own `--yes` (`ctx.auto_accept_model`) —
    /// a line typed inside a session cannot grant itself the consent the
    /// session's flag governs.
    ///
    /// Scripted "n" **by necessity**: the real keychain is behind this
    /// dispatcher path, so the confirmation must decline — a "y" here would
    /// be a test that reads a key toward whoever ran `cargo test`'s keychain.
    /// The declined flow is what proves the question was asked at all.
    #[test]
    fn a_typed_yes_on_provider_add_is_ignored_and_the_confirmation_is_still_asked() {
        let (mut conn, peer) = Connection::scripted(&[empty_config()]);
        let mut surface = RecordingSurface::new();
        let mut state = SessionState::new();
        let mut prompter = ScriptedPrompter::new(&["n"]);
        {
            let mut ctx = session_ctx(&mut surface, &mut state, &mut prompter, true);
            assert!(
                !ctx.auto_accept_model,
                "the session's own --yes must be off, or the confirm is pre-answered \
                 for the right reason and this test proves nothing"
            );
            let outcome = run_mirrored_seamed(
                PROVIDER_ADD,
                "kimi2 --kind openai-compatible --endpoint https://x/v1/chat/completions \
                 --model kimi-k3 --yes",
                &mut conn,
                &mut ctx,
                false,
            )
            .expect("a declined confirmation is not a failure of the command");
            assert_eq!(outcome, CommandOutcome::Continue);
        }
        assert_eq!(
            prompter.asked, 1,
            "the confirmation must still be asked under a typed --yes: {:?}",
            prompter.questions
        );
        assert!(
            prompter.questions[0].contains("[y/N]"),
            "the one question is the default-no confirmation: {:?}",
            prompter.questions
        );
        assert!(
            prompter.secrets.is_empty(),
            "a declined confirmation reads no key: {:?}",
            prompter.secrets
        );
        assert_eq!(
            methods_sent(&peer),
            vec!["config/get"],
            "the duplicate probe is the only RPC; no registration was sent"
        );
        let infos = surface.lines_of(LineKind::Info);
        assert!(
            infos.contains(&shell_flags_line("provider add", true).as_str()),
            "the dropped flag must be reported: {:?}",
            surface.calls
        );
        assert!(
            infos.contains(&crate::provider_add_declined_line("kimi2").as_str()),
            "the decline must be said: {:?}",
            surface.calls
        );
        assert!(
            surface.lines_of(LineKind::Error).is_empty(),
            "a decline is not an error: {:?}",
            surface.calls
        );
        conn.assert_all_consumed();
    }

    /// ADR-4's polarity, at this module's own gate. The seam only ever
    /// **loosens** — which is what makes ignoring it on a release build
    /// fail-closed (see [`test_seams_allowed`]).
    #[test]
    fn the_write_gate_refuses_only_an_unseamed_pipe() {
        assert_eq!(write_gate(true, false), WriteGate::Run);
        assert_eq!(write_gate(true, true), WriteGate::Run);
        assert_eq!(write_gate(false, true), WriteGate::Run);
        assert_eq!(write_gate(false, false), WriteGate::Refuse);
    }

    /// The refusal is one line, names the row and its shell twin, and claims
    /// exactly what was checked — a terminal, not a human. Over-claiming here is
    /// how a control becomes load-bearing for something it never enforced.
    #[test]
    fn the_typed_only_line_names_the_row_the_twin_and_what_was_checked() {
        let line = typed_only_line("policy set-tier", "teton policy set-tier");
        assert_eq!(line.lines().count(), 1);
        assert!(line.starts_with("/policy set-tier"), "{line}");
        assert!(line.contains("teton policy set-tier"), "{line}");
        assert!(line.contains("not a terminal"), "{line}");
        assert!(line.contains("nothing was changed"), "{line}");
    }

    /// The twin → row derivation the handlers rely on, both ways round.
    #[test]
    fn a_row_name_is_its_twin_without_the_binary() {
        assert_eq!(row_name("teton policy set-tier"), "policy set-tier");
        assert_eq!(row_name("teton doctor"), "doctor");
    }
}

/// **The bundled guide and this table name the same commands** (REQ-582 BR-9,
/// AC-10).
///
/// The daemon's setup guide is the copy of Teton's own instructions that is
/// *resident in every turn*: it is what a model answers a setup question from
/// with no tool call at all. Until this REQ it could only name the shell
/// spellings, because that was the only place those commands existed — so the
/// resident prompt's answer to a user who was already in a session was "open a
/// terminal", which is the failure this REQ exists to remove. Now that ten rows
/// answer from the session, a guide that still names one of them by its
/// `teton …` form alone would reintroduce that failure in the one surface no
/// test of the client can see.
///
/// So the check lives here, in the crate that owns the table, and reads the
/// guide's own bytes across the crate boundary with `include_str!` — the
/// posture `tetond/tests/web_setup_contracts.rs` and `boundary_coverage.rs`
/// already use: compile time, no crate dependency, and no scanning of source
/// text at runtime (BUG-159). The cost is that this crate rebuilds when the
/// guide changes, which is exactly the coupling being asserted.
///
/// What it cannot catch is the guide naming a `/command` in the wrong *place* —
/// order and step membership are pinned where the prompt is built
/// (`turn_loop.rs`'s prohibition test, `provider_recipes.rs`'s inspect-step
/// test). This one answers a narrower question: is there any mirrored command
/// the guide knows only by its shell name.
#[cfg(test)]
mod guide_tests {
    use crate::slash::mirrored_rows;

    /// The same bytes `build_system_prompt` embeds as `SELF_CONFIG_GUIDE`, not
    /// a copy of them — a copy is a file that agrees with the prompt until
    /// somebody edits one of the two.
    const GUIDE: &str = include_str!("../../tetond/src/harness/self_config.md");

    /// The mirrored rows whose session answer is **not** `/<row name>`.
    ///
    /// One entry, and it is a product decision rather than a spelling
    /// convenience: `/provider setup` is the guided flow REQ-579's live A/B
    /// chose over a bare registration, and it is what step 1 hands a user for a
    /// key question. Naming `/provider add` beside it in the guide would re-open
    /// the ambiguity that A/B closed, so the guide is allowed — required, in
    /// fact — to answer for `teton provider add` with the guided command.
    const SESSION_ANSWERS: &[(&str, &str)] = &[("provider add", "/provider setup")];

    /// The three BR-9 names outright, which the guide must carry whether or not
    /// it happens to spell their shell twins anywhere.
    ///
    /// The sweep below is a conditional — it only fires on a command the guide
    /// mentions in `teton …` form — so on its own it would pass a guide that
    /// dropped the inspect step altogether. These are the commands a model
    /// reaches for when asked what is configured, and that question is the one
    /// most likely to be asked from inside a session.
    const INSPECT: &[&str] = &["/policy show", "/provider list", "/doctor"];

    /// AC-10: no mirrored command is named by its shell form alone.
    #[test]
    fn the_guide_names_every_mirrored_command_in_its_session_spelling() {
        // BR-9's three, unconditionally.
        for spelling in INSPECT {
            assert!(
                GUIDE.contains(spelling),
                "the bundled guide (crates/tetond/src/harness/self_config.md) no longer \
                 names `{spelling}`, so the resident prompt answers an inspect question \
                 with a command the user has to leave the session to run (REQ-582 BR-9). \
                 Put it back in the inspect step — the `teton ` → `/` spelling is five \
                 bytes cheaper, which is what pays for it under the two prompt-margin \
                 tests."
            );
            // And they are rows, not prose: a spelling pinned into the guide
            // from here that the table does not carry would send a session user
            // to a command that dispatches to nothing.
            let name = spelling.trim_start_matches('/');
            assert!(
                mirrored_rows().any(|(row, _)| row == name),
                "`{spelling}` is pinned into the guide by this test and no mirrored row is \
                 named `{name}`. Either the row was renamed — fix this list and the guide \
                 together — or the guide is now advertising a command the session does not \
                 answer."
            );
        }

        // Table → equivalences: a mapping for a command the table no longer
        // mirrors is a rule about nothing, and it would silently excuse the row
        // it names if that row ever came back.
        for (row, answer) in SESSION_ANSWERS {
            assert!(
                mirrored_rows().any(|(name, _)| name == *row),
                "`SESSION_ANSWERS` maps `{row}` to `{answer}` and no mirrored row is named \
                 `{row}`. Remove the entry; do not re-add the row to satisfy it."
            );
        }

        // Non-vacuity for the sweep: it is a conditional, so a guide that named
        // no shell command anywhere would satisfy it by having nothing to check.
        let named: Vec<&str> = mirrored_rows()
            .filter(|(_, shell)| GUIDE.contains(shell))
            .map(|(_, shell)| shell)
            .collect();
        assert!(
            !named.is_empty(),
            "the guide names no mirrored command in its `teton …` form at all, so the \
             sweep below holds over nothing. It is expected to name at least `teton \
             provider add` (step 1's shell-only path) and the example the inspect step's \
             shell mapping is taught from. If both legitimately went away, re-anchor this \
             check rather than leaving the guide ungated."
        );

        for (name, shell) in mirrored_rows() {
            if !GUIDE.contains(shell) {
                continue;
            }
            let mapped = SESSION_ANSWERS.iter().find(|(row, _)| *row == name);
            let session = match mapped {
                Some((_, answer)) => (*answer).to_owned(),
                None => format!("/{name}"),
            };
            assert!(
                GUIDE.contains(&session),
                "the bundled guide names `{shell}` and never names `{session}`, so a model \
                 reading it hands a user who is already in a session a command they have \
                 to open a terminal for — the REQ-582 BR-9 defect. Add `{session}` to the \
                 guide and say it first (the `teton …` form is the shell footnote, not the \
                 lead), or add a row to `SESSION_ANSWERS` if this command's session answer \
                 is deliberately spelled differently."
            );
            // A mapped equivalence is a claim that the guide teaches `session`
            // *as the answer for* `shell`, and that is only true when the two
            // sit on the same line (verify T7): `/provider setup` named in step
            // 1 and `teton provider add` recited three steps later would let
            // this sweep pass while the guide's actual instruction for a
            // by-hand registration was the shell one. A row's own `/` spelling
            // needs no such proximity — it is the same command under the name
            // the session gives it.
            if mapped.is_some() {
                assert!(
                    GUIDE
                        .lines()
                        .any(|line| line.contains(shell) && line.contains(&session)),
                    "the bundled guide names `{shell}` and `{session}` on different lines, \
                     so the mapping in `SESSION_ANSWERS` describes a pairing the guide does \
                     not actually make. Put the session answer beside the shell form (as \
                     step 1 does), or drop the equivalence and add the row's own spelling."
                );
            }
        }
    }
}
