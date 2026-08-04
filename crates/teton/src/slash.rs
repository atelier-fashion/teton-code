//! In-session slash commands: input classification and dispatch.
//!
//! The interactive entry loop hands every non-empty line here before it builds
//! anything (BR-1). A line opening with `/` is a command and never becomes a
//! prompt turn — no model call is made, local or remote, so a command can never
//! appear in the transcript, the context window, or a `CostRecord`. `//` is the
//! escape hatch (BR-1b): the leading pair collapses to one `/` and the rest is
//! sent verbatim, so `//usr/bin/foo` asks the model about `/usr/bin/foo`.
//! Everything else is a plain prompt, byte-identical to what the loop sent
//! before this module existed.
//!
//! [`COMMANDS`] is the single artifact that both dispatches and generates
//! `/help` (BR-7): a command cannot exist without appearing in `/help`, and
//! `/help` cannot list a command that does not dispatch. Handlers render only
//! through the [`Surface`] seam — no direct-to-stdout side channel, so a
//! ratatui front-end inherits the commands by implementing the same seam
//! (BR-9) — and they return a [`CommandOutcome`] rather than exiting, so
//! `/quit` leaves the entry loop through the same post-loop path Ctrl-D takes
//! instead of a parallel shutdown that can drift from it (BR-6).
//!
//! The data-bearing commands add no protocol surface (BR-3): they are new call
//! sites of RPCs the daemon already serves, issued on the session's *own*
//! connection and context (D-4), and they render through the functions the
//! matching subcommands already render through (BR-4) — `/cost` through
//! [`crate::query_and_render_cost`], `/model` through
//! [`model_ui::render_current_model_line`] over the same `model/status`
//! response `teton model status` renders in full, and `/model set` through
//! [`crate::apply_model_set`], the one implementation of the validate → confirm
//! → set flow `teton model set` also runs (BR-4b). Two surfaces describing one
//! piece of daemon state must not be able to disagree, and one consent gate must
//! not have two implementations (LESSON-441).
//!
//! [`classify`] and [`resolve`] are pure and total, and are pinned in both
//! directions (BR-8): every table row is reachable from parsed input, and every
//! non-command line reaches the prompt path unchanged. A one-directional test
//! here would be the BUG-151 shape — a guard that stays green while half the
//! invariant drifts (LESSON-479).
//!
//! One command is deliberately narrower than BR-9's "identical on a TTY and on
//! piped stdin": `/model set` is **typed-input-only** (spec Permissions;
//! security review 2026-08-04). It is the only command that changes daemon
//! state, and the Permissions table says the change must never be inferable
//! from anything but a human typing it — so when the session's stdin is not a
//! terminal it renders one rejection pointing at `teton model set` and sends
//! nothing. Every other command is pipe-friendly exactly as BR-9 says.

use std::io::IsTerminal;

use teton_protocol::methods::{ModelStatusParams, PromptBlock, PromptTurnParams};
use teton_protocol::SessionId;

use crate::client::{Connection, UiContext};
use crate::model_ui;
use crate::render::{LineKind, Surface};

/// The one line `/help` prints about the `//` escape hatch (BR-1b).
const ESCAPE_FOOTER: &str =
    "//text sends text as a prompt with one leading slash — //usr/bin/foo asks about /usr/bin/foo.";

/// The tail every rejected command line carries, so an unknown command and a
/// misused one point at the same place (BR-2).
const HELP_HINT: &str = "type /help for the commands this session knows.";

/// The one line a piped `/model set` gets back (spec Permissions, security
/// review 2026-08-04). It names the shell command that does the same thing,
/// because "refused" without a remedy is a dead end.
const MODEL_SET_TYPED_ONLY: &str =
    "/model set is typed-input-only: this session's input is not a terminal, so nothing was \
     changed — run `teton model set <name>` in a shell instead.";

/// The bucket a non-empty entry line falls into. Every line lands in exactly
/// one (BR-8).
#[derive(Debug, PartialEq, Eq)]
pub enum Input<'a> {
    /// A command line: the table-matched name (without its `/`) and the
    /// trailing argument, which is empty when none was typed.
    Command {
        /// The command name, without the leading `/`.
        name: &'a str,
        /// Everything after the name, trimmed.
        args: &'a str,
    },
    /// A prompt that opened with the `//` escape, with exactly the leading pair
    /// collapsed to one `/` (BR-1b).
    EscapedPrompt(&'a str),
    /// A plain prompt — the input line's own bytes, untouched.
    Prompt(&'a str),
}

/// What the entry loop does once a command has run.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CommandOutcome {
    /// Keep reading input.
    Continue,
    /// End the session: the loop breaks and leaves through the same post-loop
    /// path Ctrl-D takes — session-end cost summary, same exit code (BR-6).
    Quit,
}

/// What a command handler is. Handlers may call the daemon over the session's
/// already-open connection and render through the session's own context (D-4);
/// the client-local commands ignore the connection but share the signature.
type Handler = fn(&mut Connection, &mut UiContext<'_>, &str) -> anyhow::Result<CommandOutcome>;

/// What a row does with the text after its name.
///
/// Both variants are rejections at [`resolve`] time, before any handler runs:
/// an argument a command has no use for is a typo, and a missing one is a
/// half-typed command. Either way exactly one line renders and no RPC is issued
/// (BR-2).
#[derive(Debug, Clone, Copy)]
enum Args {
    /// No argument is meaningful; one supplied is rejected rather than silently
    /// ignored.
    None,
    /// An argument is required. The string is the usage clause a bare command
    /// line gets back, so the hint says what to type rather than only that
    /// something is missing.
    Required(&'static str),
}

/// One row of the dispatch table.
#[derive(Debug)]
struct CommandSpec {
    /// The name typed after the `/`. A name may contain a space (`model set`):
    /// the longest matching name wins in [`split_name`], so a subcommand row is
    /// added without touching the classifier.
    name: &'static str,
    /// The one line `/help` prints for this command.
    summary: &'static str,
    /// What the row does with a trailing argument.
    args: Args,
    /// The code that runs the command.
    handler: Handler,
}

/// Every slash command, in `/help` order. The dispatcher matches against this
/// array and `/help` renders from it, so the two cannot drift (BR-7).
const COMMANDS: &[CommandSpec] = &[
    CommandSpec {
        name: "help",
        summary: "List the commands this session knows.",
        args: Args::None,
        handler: handle_help,
    },
    CommandSpec {
        name: "cost",
        summary: "Show the daemon's cost report, exactly as `teton cost` does.",
        args: Args::None,
        handler: handle_cost,
    },
    CommandSpec {
        // Argument-less: `model set` is its own row below, and [`split_name`]'s
        // longest match routes `/model set <name>` there without this row ever
        // seeing the argument. Anything else trailing `/model` is a typo and is
        // rejected here rather than being read as a model name.
        name: "model",
        summary: "Show the model the local tier is currently on.",
        args: Args::None,
        handler: handle_model,
    },
    CommandSpec {
        name: "model set",
        summary: "Switch the local tier to a catalog model: /model set <name>.",
        args: Args::Required(
            "a catalog name — `/model set <name>`, and `teton model list` names them",
        ),
        handler: handle_model_set,
    },
    CommandSpec {
        name: "verbose",
        summary: "Toggle the routing and turn-end notices for this session.",
        args: Args::None,
        handler: handle_verbose,
    },
    CommandSpec {
        name: "quit",
        summary: "End the session, exactly as Ctrl-D does.",
        args: Args::None,
        handler: handle_quit,
    },
];

/// Sort one entry line into its bucket (BR-8). Pure and total.
///
/// `input` is already trimmed and non-empty: the entry loop trims the line and
/// skips empty input before classifying, exactly as it did before slash
/// commands existed.
///
/// The name is matched **exactly** after the leading `/` (spec System Model):
/// whitespace between the slash and the first word is not tolerated, so `/ help`
/// is not `/help`. It is still a command line — a leading `/` is what makes a
/// line a command (BR-1) — it is simply one that names no row, so it is rejected
/// with the unknown-command hint and never dispatched and never prompted. Being
/// lenient here would mean two spellings reach one handler, and the one the user
/// did not intend is the one nobody tests.
#[must_use]
pub fn classify(input: &str) -> Input<'_> {
    let Some(rest) = input.strip_prefix('/') else {
        return Input::Prompt(input);
    };
    // Checked after exactly one `/` has been stripped, so `rest` is the input's
    // own bytes minus that one character: the leading pair collapses and every
    // other slash in the line is untouched (BR-1b).
    if rest.starts_with('/') {
        return Input::EscapedPrompt(rest);
    }
    if rest.starts_with(char::is_whitespace) {
        // The whole remainder becomes the "name" so the rejection can quote the
        // line as it was typed: `/ /foo` echoes as `/ /foo`, never as `//foo`,
        // which is the escape hatch's spelling and would point at a different
        // feature entirely.
        return Input::Command {
            name: rest,
            args: "",
        };
    }
    let (name, args) = split_name(rest, COMMANDS);
    Input::Command { name, args }
}

/// Build the `prompt/turn` request a classified prompt line becomes.
///
/// The **one** place the entry loop turns a line into a request, so what reaches
/// the daemon is the classifier's output and nothing else: a plain line arrives
/// byte-identically to what was typed (AC-7) and an escaped line arrives with
/// exactly the leading pair collapsed (AC-7b). `text` is the payload of an
/// [`Input::Prompt`] or [`Input::EscapedPrompt`]; a command never reaches here
/// at all (BR-1).
#[must_use]
pub fn prompt_turn_params(session_id: &SessionId, text: &str) -> PromptTurnParams {
    PromptTurnParams {
        session_id: session_id.clone(),
        prompt: vec![PromptBlock::Text {
            text: text.to_owned(),
        }],
    }
}

/// Run a classified command line, or render the reason it cannot run.
///
/// The table is consulted through [`resolve`]; a line that resolves to no row
/// renders one hint and issues no RPC (BR-2).
///
/// # Errors
///
/// Propagates any transport error a handler's RPC raises.
pub fn dispatch(
    name: &str,
    args: &str,
    conn: &mut Connection,
    ctx: &mut UiContext<'_>,
) -> anyhow::Result<CommandOutcome> {
    match resolve(name, args) {
        Resolution::Run(spec, args) => (spec.handler)(conn, ctx, args),
        Resolution::Rejected(hint) => {
            render_rejection(&hint, ctx.surface);
            Ok(CommandOutcome::Continue)
        }
    }
}

/// Split a command line into its name and trailing argument. The table name
/// matching the most leading *words* wins, so `/model set gemma` resolves to the
/// `model set` row rather than to `model` with a stray argument; a line matching
/// no row keeps its first word as the name so the unknown-command hint can quote
/// what was typed.
///
/// Matching is word-wise rather than literal, so the whitespace *between* a
/// two-word name's words is normalised: `model  set x` and `model<TAB>set x`
/// reach the same row a single space does. A literal `strip_prefix` would send
/// them to the `model` row instead and answer a mistyped-but-unambiguous line
/// with "takes no arguments", which describes neither what was typed nor what to
/// do about it.
///
/// The table is a parameter so the longest-match rule is pinned by a fixture
/// table as well as by the real one.
fn split_name<'a>(line: &'a str, table: &'static [CommandSpec]) -> (&'a str, &'a str) {
    let matched = table
        .iter()
        .filter_map(|spec| match_name_words(line, spec.name).map(|args| (spec, args)))
        .max_by_key(|(spec, _)| spec.name.split_whitespace().count());
    if let Some((spec, args)) = matched {
        return (spec.name, args);
    }
    match line.split_once(char::is_whitespace) {
        Some((name, args)) => (name, args.trim()),
        None => (line, ""),
    }
}

/// The argument left over when `line` opens with `name`'s words, or `None` when
/// it does not.
///
/// The first word must sit at the very start of the line — [`classify`] has
/// already stripped the `/` and refuses a space after it, and this keeps that
/// exactness true of the table match itself. Later words may be separated by any
/// run of whitespace, and each word must end on a word boundary so `modelling`
/// never matches `model`.
fn match_name_words<'a>(line: &'a str, name: &'static str) -> Option<&'a str> {
    let mut rest = line;
    for (index, word) in name.split_whitespace().enumerate() {
        if index > 0 {
            rest = rest.trim_start();
        }
        let after = rest.strip_prefix(word)?;
        if !(after.is_empty() || after.starts_with(char::is_whitespace)) {
            return None;
        }
        rest = after;
    }
    Some(rest.trim())
}

/// What a command line resolves to before any handler runs. Separate from
/// [`dispatch`] so the table lookup and the rejection path are exercised
/// without a daemon connection.
#[derive(Debug)]
enum Resolution<'a> {
    /// Run this row with this argument.
    Run(&'static CommandSpec, &'a str),
    /// The line names no row, or names one that takes no argument: one
    /// actionable hint renders instead of any RPC (BR-2). Misdirecting the
    /// input to the model would be the misattribution shape of BUG-146 — the
    /// user asked the harness a question and something else answers it.
    Rejected(String),
}

/// Look a classified command name up in the table. Pure.
fn resolve<'a>(name: &str, args: &'a str) -> Resolution<'a> {
    let Some(spec) = COMMANDS.iter().find(|spec| spec.name == name) else {
        return Resolution::Rejected(format!(
            "unknown command: `{}` — {HELP_HINT}",
            typed_token(name)
        ));
    };
    match spec.args {
        Args::None if !args.is_empty() => Resolution::Rejected(format!(
            "`{}` takes no arguments — {HELP_HINT}",
            typed_token(spec.name)
        )),
        // Rejected here rather than inside the handler, so a half-typed
        // `/model set` renders its usage without opening a `model/list` first.
        Args::Required(usage) if args.is_empty() => Resolution::Rejected(format!(
            "`{}` needs {usage} — {HELP_HINT}",
            typed_token(spec.name)
        )),
        _ => Resolution::Run(spec, args),
    }
}

/// The command as the user typed it, for a rejection that quotes rather than
/// reconstructs.
///
/// [`classify`] strips exactly one `/`, so putting one back reproduces the line.
/// The guard matters for the one shape where it would not: a name that already
/// carries a slash must not gain a second one — `//foo` is the escape hatch's
/// spelling (BR-1b), and echoing it at someone who typed something else would
/// name a feature they never used.
fn typed_token(name: &str) -> String {
    if name.starts_with('/') {
        name.to_owned()
    } else {
        format!("/{name}")
    }
}

/// Render a command line that never reaches a handler: exactly one line, and
/// nothing else (BR-2).
fn render_rejection(hint: &str, surface: &mut dyn Surface) {
    surface.line(LineKind::Error, hint);
}

/// `/help`: the command list, generated from [`COMMANDS`] (BR-7), plus the one
/// footer line documenting the `//` escape (BR-1b).
fn render_help(surface: &mut dyn Surface) {
    // Names pad to the widest row, so a later two-word row (`model set`)
    // re-aligns the whole list instead of breaking out of it.
    let width = COMMANDS
        .iter()
        .map(|spec| spec.name.len())
        .max()
        .unwrap_or(0);
    for spec in COMMANDS {
        surface.line(
            LineKind::Info,
            &format!("/{:<width$}  {}", spec.name, spec.summary),
        );
    }
    surface.line(LineKind::Info, ESCAPE_FOOTER);
}

/// `/verbose`: flip the session's notice visibility and echo the new state
/// (BR-5). Session-scoped — it persists nothing, and the next session starts
/// from the `--verbose` flag's default again.
fn toggle_verbose(ctx: &mut UiContext<'_>) {
    ctx.state.verbose = !ctx.state.verbose;
    let echo = if ctx.state.verbose {
        "verbose on"
    } else {
        "verbose off"
    };
    ctx.surface.line(LineKind::Info, echo);
}

/// The `/help` handler.
fn handle_help(
    _conn: &mut Connection,
    ctx: &mut UiContext<'_>,
    _args: &str,
) -> anyhow::Result<CommandOutcome> {
    render_help(ctx.surface);
    Ok(CommandOutcome::Continue)
}

/// The `/verbose` handler.
fn handle_verbose(
    _conn: &mut Connection,
    ctx: &mut UiContext<'_>,
    _args: &str,
) -> anyhow::Result<CommandOutcome> {
    toggle_verbose(ctx);
    Ok(CommandOutcome::Continue)
}

/// The `/cost` handler: the daemon's authoritative cost report, rendered by the
/// same function `teton cost` calls (BR-4 / AC-2).
///
/// There is nothing else here on purpose. Every figure, the daemon-too-old
/// notice, and the RPC-error line all come from
/// [`crate::query_and_render_cost`], so the in-session meter cannot drift from
/// the subcommand's — a second rendering would be two answers to one question
/// about the user's money.
fn handle_cost(
    conn: &mut Connection,
    ctx: &mut UiContext<'_>,
    _args: &str,
) -> anyhow::Result<CommandOutcome> {
    // The session's own connection and context (D-4): no second connection, and
    // an event arriving while this RPC pumps behaves exactly as it would
    // between turns.
    crate::query_and_render_cost(conn, ctx)?;
    Ok(CommandOutcome::Continue)
}

/// The `/model` handler: one line naming the model the local tier is on (AC-3),
/// derived from the same `model/status` response `teton model status` renders in
/// full (BR-4, D-6).
///
/// The daemon-too-old and RPC-error arms mirror `run_model_status`'s wording and
/// return [`CommandOutcome::Continue`] either way: a failed status query ends a
/// command, never the session.
fn handle_model(
    conn: &mut Connection,
    ctx: &mut UiContext<'_>,
    _args: &str,
) -> anyhow::Result<CommandOutcome> {
    let answered = conn.call(ModelStatusParams::default(), ctx)?;
    // The failure arms are `run_model_status`'s own (one function, one set of
    // strings), so `teton model status` and `/model` cannot report the same
    // unreachable method differently. Only the success rendering differs, which
    // is the whole of what D-6 says is different between the two surfaces.
    if let Some(status) = crate::model_status_or_report(answered, ctx.surface) {
        model_ui::render_current_model_line(&status, ctx.surface);
    }
    Ok(CommandOutcome::Continue)
}

/// The `/model set <name>` handler: the write path, and the only command here
/// that changes daemon state.
///
/// It contains no validation and no confirmation of its own — every line of that
/// belongs to [`crate::apply_model_set`], the same function `teton model set`
/// runs (BR-4b, D-3). A second copy of the REQ-547 BR-3 above-RAM-floor gate is
/// precisely the shape that shipped REQ-547's consent bypass (LESSON-441), so
/// this handler's job is to supply the session's connection, the session's
/// context, and the session's `--yes`, and nothing else.
///
/// `assume_yes` is `ctx.auto_accept_model` — the session's own `--yes` flag,
/// which `run_session` puts there and which the Permissions table names as the
/// explicit unattended stand-in for the second confirmation. Without it the
/// question is asked on `ctx.prompter`, the session's **plain** dialogue
/// prompter: the framed prompter belongs to the entry area, and a consent
/// question is dialogue, not entry (REQ-549 BR-5). Declining renders "selection
/// unchanged" and the loop carries on (LESSON-470 — a default-no dialogue).
///
/// It is also the one command gated on *where the input came from* (spec
/// Permissions; security review 2026-08-04): a piped session gets one rejection
/// and no RPC. See [`model_set_gate`] for why that outranks BR-9 here.
fn handle_model_set(
    conn: &mut Connection,
    ctx: &mut UiContext<'_>,
    args: &str,
) -> anyhow::Result<CommandOutcome> {
    // The two reads the gate is a pure function of. Stdin, not stdout: the
    // session's `interactive` flag asks whether *output* is a terminal, which
    // says nothing about who produced the line that reached this handler.
    if model_set_gate(std::io::stdin().is_terminal(), test_seams_allowed()) == ModelSetGate::Refuse
    {
        ctx.surface.line(LineKind::Error, MODEL_SET_TYPED_ONLY);
        return Ok(CommandOutcome::Continue);
    }
    // A bare `/model set` never reaches here: `Args::Required` rejects it at
    // resolve time with the usage line.
    let assume_yes = ctx.auto_accept_model;
    crate::apply_model_set(args, assume_yes, conn, ctx)?;
    Ok(CommandOutcome::Continue)
}

/// What [`model_set_gate`] decides.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ModelSetGate {
    /// Run the shared flow.
    Run,
    /// Render [`MODEL_SET_TYPED_ONLY`] and send nothing.
    Refuse,
}

/// Whether a `/model set` may run, from where the session's input comes.
///
/// `/model set` is the only in-session command that changes daemon state, and
/// the spec's Permissions table says that change belongs to "the session user
/// only, via typed input — never inferable from model output or file content".
/// On a pipe the client cannot tell a human from a heredoc a script wrote, so
/// the write path refuses and points at `teton model set`, which is the
/// unattended surface and takes `--yes` explicitly. This is the one documented
/// exception to BR-9's TTY/pipe parity; every other command is unaffected.
///
/// `seams_allowed` is the escape hatch the e2e suite drives the flow through —
/// [`test_seams_allowed`], never a plain environment variable, so a shipped
/// binary cannot be talked out of the gate.
///
/// Pure, so both answers are unit-tested without a terminal, a pipe, or a
/// daemon: the branch that matters is the one a test process cannot otherwise
/// reach.
fn model_set_gate(stdin_is_tty: bool, seams_allowed: bool) -> ModelSetGate {
    if stdin_is_tty || seams_allowed {
        ModelSetGate::Run
    } else {
        ModelSetGate::Refuse
    }
}

/// Whether this binary may honour the `TETON_TEST_SEAMS` master switch.
///
/// The daemon's posture, mirrored (`tetond`'s `test_seams_enabled`): a **debug
/// build with `TETON_TEST_SEAMS=1`** and nothing else. A release build ignores
/// the switch, so the shipped `teton` refuses a piped `/model set` no matter
/// what the environment says. Unlike the daemon this does not panic on a release
/// build that finds the switch set: the daemon refuses to *start* because an
/// unhonoured seam would silently change what it does, whereas ignoring it here
/// only keeps the stricter of the two behaviours.
fn test_seams_allowed() -> bool {
    cfg!(debug_assertions) && std::env::var("TETON_TEST_SEAMS").ok().as_deref() == Some("1")
}

/// The `/quit` handler: it renders nothing and ends no session itself — the
/// entry loop breaks on [`CommandOutcome::Quit`] and the existing post-loop
/// path prints the session-end cost summary (BR-6).
fn handle_quit(
    _conn: &mut Connection,
    _ctx: &mut UiContext<'_>,
    _args: &str,
) -> anyhow::Result<CommandOutcome> {
    Ok(CommandOutcome::Quit)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::prompt::ScriptedPrompter;
    use crate::render::RecordingSurface;
    use crate::session_ui::SessionState;

    /// The session's own context (D-4). No answers are scripted: none of the
    /// client-local commands asks a question.
    fn session_ctx<'a>(
        surface: &'a mut RecordingSurface,
        state: &'a mut SessionState,
        prompter: &'a mut ScriptedPrompter,
    ) -> UiContext<'a> {
        UiContext {
            surface,
            state,
            prompter,
            answer_permissions: true,
            answer_model_proposals: true,
            auto_accept_model: false,
        }
    }

    // BR-8, forward direction (LESSON-479): iterate the dispatch table and prove
    // every entry is reachable from parsed input. This direction alone would
    // stay green while the passthrough drifted, so the two tests below cover the
    // reverse.
    #[test]
    fn every_table_row_is_reachable_from_a_typed_command_line() {
        for spec in COMMANDS {
            // A row that requires an argument is only reachable *with* one, so
            // the loop types what the row asks for rather than skipping it.
            let expected_args = match spec.args {
                Args::None => "",
                Args::Required(_) => "qwen2.5-coder-3b",
            };
            let typed = format!("/{} {expected_args}", spec.name);
            let typed = typed.trim_end();
            let Input::Command { name, args } = classify(typed) else {
                panic!("`{typed}` did not classify as a command");
            };
            assert_eq!(name, spec.name);
            assert_eq!(args, expected_args);
            let Resolution::Run(resolved, run_args) = resolve(name, args) else {
                panic!("`{typed}` classified as a command but did not dispatch");
            };
            assert_eq!(resolved.name, spec.name);
            assert_eq!(run_args, expected_args);
            assert!(
                !resolved.summary.is_empty(),
                "/{} would appear in /help with no summary",
                spec.name
            );
        }
    }

    // The loop above proves every row in the table is reachable — and stays
    // green if a row is *deleted*. This is the other half of that invariant
    // (LESSON-479): the commands this REQ promises are in the table at all.
    #[test]
    fn the_table_carries_every_command_this_req_promises() {
        let names: Vec<&str> = COMMANDS.iter().map(|spec| spec.name).collect();
        let promised = ["help", "cost", "model", "model set", "verbose", "quit"];
        for expected in promised {
            assert!(
                names.contains(&expected),
                "/{expected} is missing from the dispatch table: {names:?}"
            );
        }
        // The count closes the third direction: the loop above proves the six
        // are present and the reachability loop proves each row dispatches, but
        // neither notices a *seventh* row. The REQ scopes the surface at six
        // deliberately ("the command set is deliberately small"), so a new row
        // is a spec decision and lands here first.
        assert_eq!(
            COMMANDS.len(),
            promised.len(),
            "the table grew past the six commands this REQ scopes: {names:?}"
        );
    }

    // Decision 3 (verify pass, 2026-08-04): the name is matched EXACTLY after the
    // leading `/`. A space between them is not leniently absorbed — `/ help` is
    // an unknown command, not `/help` — so one spelling reaches one handler and
    // the other is told plainly that it named nothing.
    #[test]
    fn whitespace_after_the_slash_is_never_a_command() {
        for typed in ["/ help", "/  model set qwen2.5-coder-3b", "/\tcost"] {
            let Input::Command { name, args } = classify(typed) else {
                panic!("`{typed}` must stay a command line — never a prompt");
            };
            let Resolution::Rejected(hint) = resolve(name, args) else {
                panic!("`{typed}` reached a handler through the space after the slash");
            };
            assert!(hint.contains("unknown command"), "{hint}");
            assert!(hint.contains("/help"), "{hint}");
        }
    }

    // The rejection quotes what was typed rather than rebuilding it. `/ /foo`
    // must not echo as `//foo`: that is the escape hatch's spelling (BR-1b), so
    // the hint would name a feature the user never used.
    #[test]
    fn a_rejection_never_echoes_a_doubled_slash() {
        let Input::Command { name, args } = classify("/ /foo") else {
            panic!("`/ /foo` did not classify as a command");
        };
        let Resolution::Rejected(hint) = resolve(name, args) else {
            panic!("`/ /foo` reached a handler");
        };
        assert!(!hint.contains("//"), "the hint doubled the slash: {hint}");
        assert!(
            hint.contains("/ /foo"),
            "the hint dropped the typed line: {hint}"
        );
        // And the same guard holds if a name ever reaches `typed_token` with its
        // slash already on it.
        assert_eq!(typed_token("/foo"), "/foo");
        assert_eq!(typed_token("foo"), "/foo");
    }

    // Internal whitespace inside a two-word name is normalised: the words are
    // what identify the row, not the single space between them. Without this,
    // `/model  set x` routes to the `model` row and is answered with "takes no
    // arguments", which describes neither what was typed nor what to do.
    #[test]
    fn extra_whitespace_between_a_two_word_names_words_still_routes_to_its_row() {
        for typed in [
            "/model  set qwen2.5-coder-3b",
            "/model\tset qwen2.5-coder-3b",
            "/model \t set qwen2.5-coder-3b",
        ] {
            let Input::Command { name, args } = classify(typed) else {
                panic!("`{typed}` did not classify as a command");
            };
            assert_eq!((name, args), ("model set", "qwen2.5-coder-3b"), "{typed}");
            let Resolution::Run(spec, run_args) = resolve(name, args) else {
                panic!("`{typed}` did not reach the model-set row");
            };
            assert_eq!(spec.name, "model set");
            assert_eq!(run_args, "qwen2.5-coder-3b");
        }
    }

    // AC-7 / AC-7b at the level the request is actually built: the classifier's
    // output goes straight into `PromptTurnParams`, so a plain line arrives
    // byte-identically and an escaped line arrives with exactly the leading pair
    // collapsed. The e2e leg can see that these lines reach the model; only this
    // can see the bytes.
    #[test]
    fn a_prompt_line_becomes_a_turn_carrying_exactly_its_own_bytes() {
        let session = SessionId::from("sess-7");
        let text_of = |params: &PromptTurnParams| match params.prompt.as_slice() {
            [PromptBlock::Text { text }] => text.clone(),
            other => panic!("a prompt turn carries exactly one text block: {other:?}"),
        };

        for line in [
            "explain this stack trace",
            "what does src/main.rs do?",
            "a/b/c",
            "-- /help",
        ] {
            let Input::Prompt(text) = classify(line) else {
                panic!("`{line}` did not classify as a plain prompt");
            };
            let params = prompt_turn_params(&session, text);
            assert_eq!(text_of(&params), line, "the bytes changed on the way out");
            assert_eq!(params.session_id, session);
        }

        for (typed, sent) in [
            (
                "//usr/local/bin/deploy.sh fails — why?",
                "/usr/local/bin/deploy.sh fails — why?",
            ),
            ("//help", "/help"),
            ("///etc", "//etc"),
        ] {
            let Input::EscapedPrompt(text) = classify(typed) else {
                panic!("`{typed}` did not classify as an escaped prompt");
            };
            assert_eq!(text_of(&prompt_turn_params(&session, text)), sent);
        }
    }

    // Decision 1 (security review, 2026-08-04): `/model set` is typed-input-only.
    // The gate is a pure function of the two facts the handler reads, so the
    // refusal — the branch a test process with a piped stdin cannot otherwise
    // reach on purpose — is pinned here rather than inferred from an e2e run.
    #[test]
    fn model_set_runs_only_from_a_terminal_or_under_the_test_seam() {
        assert_eq!(model_set_gate(true, false), ModelSetGate::Run);
        assert_eq!(model_set_gate(true, true), ModelSetGate::Run);
        // The e2e suite's allowance, and nothing else in the wild: a release
        // build's `test_seams_allowed` is false whatever the environment says.
        assert_eq!(model_set_gate(false, true), ModelSetGate::Run);
        // The shape that matters: piped input, no seam, no write.
        assert_eq!(model_set_gate(false, false), ModelSetGate::Refuse);
        // The refusal names the surface that does the same thing unattended.
        assert!(MODEL_SET_TYPED_ONLY.contains("teton model set"));
        assert_eq!(MODEL_SET_TYPED_ONLY.lines().count(), 1);
    }

    // BR-8, reverse direction (LESSON-479): input that is not a command reaches
    // the prompt path byte-identically to today (AC-7).
    #[test]
    fn a_line_not_opening_with_a_slash_is_a_byte_identical_prompt() {
        for line in [
            "explain this stack trace",
            "what does src/main.rs do?",
            "a/b/c",
            "-- /help",
            "help",
        ] {
            assert_eq!(classify(line), Input::Prompt(line));
        }
    }

    // BR-8, reverse direction, escape-hatch leg (BR-1b, AC-7b): `//` collapses
    // EXACTLY the leading pair; slashes anywhere else are untouched.
    #[test]
    fn the_double_slash_escape_collapses_only_the_leading_pair() {
        assert_eq!(
            classify("//usr/local/bin/x — why?"),
            Input::EscapedPrompt("/usr/local/bin/x — why?")
        );
        assert_eq!(classify("//"), Input::EscapedPrompt("/"));
        assert_eq!(classify("///etc"), Input::EscapedPrompt("//etc"));
        assert_eq!(classify("//help"), Input::EscapedPrompt("/help"));
    }

    // The longest matching name wins on a word boundary, which is what lets the
    // `model set` row sit beside `model` without touching the classifier.
    #[test]
    fn a_two_word_row_wins_over_its_one_word_prefix() {
        const FIXTURE: &[CommandSpec] = &[
            CommandSpec {
                name: "model",
                summary: "show the current model",
                args: Args::None,
                handler: handle_help,
            },
            CommandSpec {
                name: "model set",
                summary: "change the current model",
                args: Args::Required("a catalog name"),
                handler: handle_help,
            },
        ];

        assert_eq!(
            split_name("model set gemma-3", FIXTURE),
            ("model set", "gemma-3")
        );
        assert_eq!(split_name("model", FIXTURE), ("model", ""));
        // A row name is only a match on a word boundary.
        assert_eq!(split_name("modelling", FIXTURE), ("modelling", ""));
        assert_eq!(split_name("frobnicate now", FIXTURE), ("frobnicate", "now"));
    }

    // AC-1 unit leg: /help is generated from the table (BR-7), so every row
    // appears with its summary, and the escape hatch gets its footer (BR-1b).
    #[test]
    fn help_renders_every_table_row_and_the_escape_footer() {
        let mut surface = RecordingSurface::new();
        render_help(&mut surface);

        let lines = surface.lines_of(LineKind::Info);
        assert_eq!(
            lines.len(),
            COMMANDS.len() + 1,
            "one line per command plus the escape footer"
        );
        for (spec, line) in COMMANDS.iter().zip(&lines) {
            assert!(line.starts_with(&format!("/{}", spec.name)), "{line}");
            assert!(line.ends_with(spec.summary), "{line}");
        }
        assert_eq!(lines.last(), Some(&ESCAPE_FOOTER));
        assert!(ESCAPE_FOOTER.contains("//"));
    }

    // BR-5: the toggle owns one flag and echoes what it just set.
    #[test]
    fn verbose_toggles_the_session_flag_and_echoes_the_new_state() {
        let mut surface = RecordingSurface::new();
        let mut state = SessionState::new();
        let mut prompter = ScriptedPrompter::new(&[]);
        {
            let mut ctx = session_ctx(&mut surface, &mut state, &mut prompter);
            // A session starts quiet unless `--verbose` initialised it.
            assert!(!ctx.state.verbose);
            toggle_verbose(&mut ctx);
            assert!(ctx.state.verbose);
            toggle_verbose(&mut ctx);
            assert!(!ctx.state.verbose);
            toggle_verbose(&mut ctx);
            assert!(ctx.state.verbose);
        }
        assert_eq!(
            surface.lines_of(LineKind::Info),
            vec!["verbose on", "verbose off", "verbose on"]
        );
        assert_eq!(prompter.asked, 0, "the toggle asks nothing");
    }

    // AC-6 unit leg: an unknown command resolves to no handler at all — there is
    // no path from here to an RPC — and renders exactly one actionable line.
    #[test]
    fn an_unknown_command_is_one_error_line_naming_help() {
        let Input::Command { name, args } = classify("/frobnicate") else {
            panic!("`/frobnicate` did not classify as a command");
        };
        let Resolution::Rejected(hint) = resolve(name, args) else {
            panic!("`/frobnicate` resolved to a handler");
        };

        let mut surface = RecordingSurface::new();
        render_rejection(&hint, &mut surface);
        assert_eq!(surface.calls.len(), 1, "the hint is the only output");
        assert_eq!(surface.lines_of(LineKind::Error), vec![hint.as_str()]);
        assert!(hint.contains("/frobnicate"), "{hint}");
        assert!(hint.contains("/help"), "{hint}");

        // A bare `/` is the same shape: a command line with nothing to
        // dispatch, never a prompt.
        let Input::Command { name, args } = classify("/") else {
            panic!("a bare `/` did not classify as a command");
        };
        assert!(matches!(resolve(name, args), Resolution::Rejected(_)));
    }

    // A command that takes no argument says so rather than ignoring it.
    #[test]
    fn a_trailing_argument_to_an_arg_less_command_is_rejected() {
        let Input::Command { name, args } = classify("/help extra") else {
            panic!("`/help extra` did not classify as a command");
        };
        assert_eq!((name, args), ("help", "extra"));

        let Resolution::Rejected(hint) = resolve(name, args) else {
            panic!("`/help extra` ran the handler");
        };
        assert!(hint.contains("takes no arguments"), "{hint}");
        assert!(hint.contains("/help"), "{hint}");
    }

    // The data-bearing commands follow the same convention: a trailing word is
    // rejected with the hint rather than quietly dropped, so a typo never runs
    // an RPC the user did not ask for (BR-2). `/cost` is unparameterized by
    // OQ-3's resolution, and `/model` takes no argument of its own.
    #[test]
    fn a_trailing_argument_to_cost_or_model_is_rejected() {
        for typed in ["/cost extra-arg", "/model extra"] {
            let Input::Command { name, args } = classify(typed) else {
                panic!("`{typed}` did not classify as a command");
            };
            assert!(!args.is_empty(), "`{typed}` parsed no argument");
            let Resolution::Rejected(hint) = resolve(name, args) else {
                panic!("`{typed}` ran the handler with a stray argument");
            };
            assert!(hint.contains("takes no arguments"), "{hint}");
            assert!(hint.contains(&format!("/{name}")), "{hint}");
            assert!(hint.contains("/help"), "{hint}");
        }
    }

    // The `/model set` seam, post-TASK-036. `set <name>` is no longer a stray
    // argument to `/model`: [`split_name`]'s longest match routes the line to
    // the two-word row, carrying the catalog name as the argument. The two
    // neighbouring lines stay rejections — a bare `/model set` is a half-typed
    // command, and `/model <anything-else>` is a typo that must never be read as
    // a model name.
    #[test]
    fn model_set_routes_to_its_own_row_and_a_bare_one_asks_for_a_name() {
        let Input::Command { name, args } = classify("/model set qwen2.5-coder-3b") else {
            panic!("`/model set …` did not classify as a command");
        };
        assert_eq!((name, args), ("model set", "qwen2.5-coder-3b"));
        let Resolution::Run(spec, run_args) = resolve(name, args) else {
            panic!("`/model set qwen2.5-coder-3b` did not reach the model-set row");
        };
        assert_eq!(spec.name, "model set");
        // The catalog name reaches the handler verbatim — the flow validates it
        // against `model/list`, so nothing here second-guesses it.
        assert_eq!(run_args, "qwen2.5-coder-3b");

        // No name: the usage line, not a `model/list` with an empty name.
        let Input::Command { name, args } = classify("/model set") else {
            panic!("`/model set` did not classify as a command");
        };
        assert_eq!((name, args), ("model set", ""));
        let Resolution::Rejected(hint) = resolve(name, args) else {
            panic!("a bare `/model set` reached the handler");
        };
        assert!(hint.contains("/model set"), "{hint}");
        assert!(hint.contains("<name>"), "{hint}");
        assert!(hint.contains("/help"), "{hint}");

        // And the one-word row still refuses a stray word rather than treating
        // it as a model name.
        let Input::Command { name, args } = classify("/model extra") else {
            panic!("`/model extra` did not classify as a command");
        };
        assert_eq!((name, args), ("model", "extra"));
        assert!(matches!(resolve(name, args), Resolution::Rejected(_)));
    }
}
