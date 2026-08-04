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

use teton_protocol::jsonrpc::error_code;
use teton_protocol::methods::ModelStatusParams;

use crate::client::{Connection, UiContext};
use crate::model_ui;
use crate::render::{LineKind, Surface};

/// The one line `/help` prints about the `//` escape hatch (BR-1b).
const ESCAPE_FOOTER: &str =
    "//text sends text as a prompt with one leading slash — //usr/bin/foo asks about /usr/bin/foo.";

/// The tail every rejected command line carries, so an unknown command and a
/// misused one point at the same place (BR-2).
const HELP_HINT: &str = "type /help for the commands this session knows.";

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
    let (name, args) = split_name(rest.trim(), COMMANDS);
    Input::Command { name, args }
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

/// Split a command line into its name and trailing argument. The longest table
/// name the line starts with on a word boundary wins, so `/model set gemma`
/// resolves to the `model set` row rather than to `model` with a stray
/// argument; a line matching no row keeps its first word as the name so the
/// unknown-command hint can quote what was typed.
///
/// The table is a parameter so the longest-match rule is pinned by a fixture
/// table now, rather than waiting for TASK-036 to add the real two-word row.
fn split_name<'a>(line: &'a str, table: &'static [CommandSpec]) -> (&'a str, &'a str) {
    let matched = table
        .iter()
        .filter(|spec| {
            line.strip_prefix(spec.name)
                .is_some_and(|rest| rest.is_empty() || rest.starts_with(char::is_whitespace))
        })
        .max_by_key(|spec| spec.name.len());
    if let Some(spec) = matched {
        return (spec.name, line[spec.name.len()..].trim());
    }
    match line.split_once(char::is_whitespace) {
        Some((name, args)) => (name, args.trim()),
        None => (line, ""),
    }
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
        return Resolution::Rejected(format!("unknown command: /{name} — {HELP_HINT}"));
    };
    match spec.args {
        Args::None if !args.is_empty() => {
            Resolution::Rejected(format!("/{} takes no arguments — {HELP_HINT}", spec.name))
        }
        // Rejected here rather than inside the handler, so a half-typed
        // `/model set` renders its usage without opening a `model/list` first.
        Args::Required(usage) if args.is_empty() => {
            Resolution::Rejected(format!("/{} needs {usage} — {HELP_HINT}", spec.name))
        }
        _ => Resolution::Run(spec, args),
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
    match conn.call(ModelStatusParams::default(), ctx)? {
        Ok(status) => model_ui::render_current_model_line(&status, ctx.surface),
        Err(err) if err.code == error_code::METHOD_NOT_FOUND => ctx.surface.line(
            LineKind::Notice,
            "this daemon build does not expose model/status yet.",
        ),
        Err(err) => ctx.surface.line(
            LineKind::Error,
            &format!("could not read the model status: {}", err.message),
        ),
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
fn handle_model_set(
    conn: &mut Connection,
    ctx: &mut UiContext<'_>,
    args: &str,
) -> anyhow::Result<CommandOutcome> {
    // A bare `/model set` never reaches here: `Args::Required` rejects it at
    // resolve time with the usage line.
    let assume_yes = ctx.auto_accept_model;
    crate::apply_model_set(args, assume_yes, conn, ctx)?;
    Ok(CommandOutcome::Continue)
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
        for expected in ["help", "cost", "model", "model set", "verbose", "quit"] {
            assert!(
                names.contains(&expected),
                "/{expected} is missing from the dispatch table: {names:?}"
            );
        }
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
