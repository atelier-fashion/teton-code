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
//! `/help` cannot list a command that does not dispatch. Aliases are part of
//! that same artifact — [`split_name`] canonicalises `/exit` to the `quit` row
//! before anything looks it up, so a second spelling adds no second handler and
//! no second `/help` entry. Handlers render only
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
//! piped stdin": `/model set` is **refused when the session's stdin is not a
//! terminal** (spec Permissions; security review 2026-08-04). It is the only
//! command that changes daemon state, so on a pipe it renders one rejection
//! pointing at `teton model set` and sends nothing. That is what is enforced,
//! and it is narrower than what it is enforced *for*: the check separates a pipe
//! from a pty, not a machine from a human — `expect(1)`, a tmux `send-keys` and
//! a pasted line all present a terminal and pass. The spec records that residual
//! and names `teton model set` as the auditable surface for the unattended case.
//! Every other command is pipe-friendly exactly as BR-9 says.

use teton_protocol::effort::EffortLevel;
use teton_protocol::jsonrpc::{error_code, RpcError};
use teton_protocol::methods::{
    ConfigGetParams, ConfigSetParams, ConfigUpdate, ModelStatusParams, PromptBlock,
    PromptTurnParams, SessionClearParams, SessionPermissionsParams, SessionPermissionsResult,
    WebOverrideParams, WebOverrideResult, WebRefreshOutcome, WebRefreshParams, WebRefreshResult,
};
use teton_protocol::permissions::PermissionLevel;
use teton_protocol::SessionId;

use crate::client::{Connection, UiContext};
use crate::model_ui;
use crate::render::{LineKind, Surface};
use crate::session_ui::web_tier_name;

/// The one line `/help` prints about the `//` escape hatch (BR-1b).
const ESCAPE_FOOTER: &str =
    "//text sends text as a prompt with one leading slash — //usr/bin/foo asks about /usr/bin/foo.";

/// The tail every rejected command line carries, so an unknown command and a
/// misused one point at the same place (BR-2).
const HELP_HINT: &str = "type /help for the commands this session knows.";

/// The one line a piped `/model set` gets back (spec Permissions, security
/// review 2026-08-04).
///
/// What it reports is exactly what was checked — "this session's input is not a
/// terminal" — not a claim to have identified a human: a pty is a pty however it
/// was opened. It names the shell command that does the same thing because
/// "refused" without a remedy is a dead end, and it names `--yes` with it: a
/// script that reaches for `teton model set` without the flag meets the
/// above-RAM-floor confirmation on a stdin nobody is typing into, and an EOF
/// declines it silently — a second dead end one step further along.
const MODEL_SET_TYPED_ONLY: &str =
    "/model set is typed-input-only: this session's input is not a terminal, so nothing was \
     changed — run `teton model set <name>` from a shell instead (add --yes for a pick above \
     this machine's RAM floor).";

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
    /// An argument is meaningful but optional: the bare form does something
    /// useful in its own right.
    ///
    /// Added for REQ-559's `/effort`, whose bare form is a **read** (BR-9) —
    /// modelling it as `Required` would reject the very line the spec asks to
    /// work, and modelling it as `None` would reject the set form. The handler
    /// validates the argument when one is present, because only it knows the
    /// vocabulary.
    ///
    /// REQ-560's `/permissions` is the second row to need it, for the same
    /// reason on both sides: its bare form is the read BR-10 requires to work on
    /// a pipe, and its argument form is the only way to change the level. Two
    /// commands, one variant — a second spelling of "optional" would be two
    /// rules that can drift.
    ///
    /// Unlike the two variants above, this is never a rejection at [`resolve`]
    /// time: the handler is entered either way and decides what an empty
    /// argument means.
    Optional,
}

/// One row of the dispatch table.
#[derive(Debug)]
struct CommandSpec {
    /// The name typed after the `/`. A name may contain a space (`model set`):
    /// the longest matching name wins in [`split_name`], so a subcommand row is
    /// added without touching the classifier.
    name: &'static str,
    /// Other spellings that reach this row. [`split_name`] canonicalises them to
    /// [`Self::name`], so an alias adds a way to *type* a command and never a
    /// second row, a second handler, or a second `/help` entry (BR-7).
    ///
    /// Aliases exist for the one command a user reaches for under a habit
    /// formed elsewhere: `/exit` is what other REPLs call `/quit`, and a shell
    /// that answers it with "unknown command" is answering a question the user
    /// asked correctly. They are not a general synonym mechanism — a command
    /// worth a second name is worth a table row.
    aliases: &'static [&'static str],
    /// The one line `/help` prints for this command.
    summary: &'static str,
    /// What the row does with a trailing argument.
    args: Args,
    /// The code that runs the command.
    handler: Handler,
}

impl CommandSpec {
    /// Every spelling that reaches this row, canonical name first.
    fn spellings(&self) -> impl Iterator<Item = &'static str> {
        std::iter::once(self.name).chain(self.aliases.iter().copied())
    }
}

/// Every slash command, in `/help` order. The dispatcher matches against this
/// array and `/help` renders from it, so the two cannot drift (BR-7).
const COMMANDS: &[CommandSpec] = &[
    CommandSpec {
        name: "help",
        aliases: &[],
        summary: "List the commands this session knows.",
        args: Args::None,
        handler: handle_help,
    },
    CommandSpec {
        name: "cost",
        aliases: &[],
        summary: "Show the daemon's cost report, exactly as `teton cost` does.",
        args: Args::None,
        handler: handle_cost,
    },
    // REQ-559 BR-9: this REQ owns the `/effort` row, its bare-argument read
    // path, and therefore its `/help` entry — `/help` is generated from this
    // array (BR-7), so a command cannot exist without appearing there.
    // REQ-560 renders the effort *value* in its status line and adds
    // `/permissions`; it does not add, alias, or duplicate this row.
    CommandSpec {
        name: "effort",
        aliases: &[],
        summary: "Show or set the global reasoning effort: /effort [low|medium|high|xhigh|max].",
        // Deliberately NOT `Args::Required`: the bare form is a *read*, which
        // BR-9 requires, so an argument-less line must dispatch rather than be
        // rejected as half-typed.
        args: Args::Optional,
        handler: handle_effort,
    },
    CommandSpec {
        // Argument-less: `model set` is its own row below, and [`split_name`]'s
        // longest match routes `/model set <name>` there without this row ever
        // seeing the argument. Anything else trailing `/model` is a typo and is
        // rejected here rather than being read as a model name.
        name: "model",
        aliases: &[],
        summary: "Show the model the local tier is currently on.",
        args: Args::None,
        handler: handle_model,
    },
    CommandSpec {
        name: "model set",
        aliases: &[],
        summary: "Switch the local tier to a catalog model: /model set <name>.",
        args: Args::Required(
            "a catalog name — `/model set <name>`, and `teton model list` names them",
        ),
        handler: handle_model_set,
    },
    // REQ-567's user-only clear. Placed beside `/verbose` because both are
    // commands about *this session* rather than about the machine's
    // configuration, and listed here — and therefore in `/help` — because a
    // conversation the user cannot drop is a conversation they cannot get out
    // of (BUG-153).
    CommandSpec {
        name: "clear",
        aliases: &[],
        summary: "Drop this session's retained conversation; the next prompt starts fresh.",
        args: Args::None,
        handler: handle_clear,
    },
    CommandSpec {
        name: "verbose",
        aliases: &[],
        summary: "Toggle the routing and turn-end notices for this session.",
        args: Args::None,
        handler: handle_verbose,
    },
    // REQ-560's permission level. Placed beside `/clear` and `/verbose` because
    // all three are about *this session* rather than about the machine's
    // configuration — the level resets every session (BR-6), so it belongs with
    // the session-scoped commands and not with `/model set`, which writes.
    //
    // Listed here — and therefore in `/help` — because a command a user cannot
    // discover is a command they do not have (BUG-153), and this one is how they
    // find out what the session is allowed to do at all.
    //
    // `/effort` is deliberately **not** here: REQ-559 BR-9 owns that row. This
    // REQ renders the effort value in the status line and adds no second way to
    // type it (BR-14).
    CommandSpec {
        name: "permissions",
        aliases: &[],
        summary: "Show or set this session's permission level: /permissions [level].",
        args: Args::Optional,
        handler: handle_permissions,
    },
    // REQ-563's two user-only web actions. Both are client commands rather than
    // harness tools, and that placement is the enforcement: tool dispatch and
    // this table are structurally distinct channels, so a model emitting a tool
    // call named `web allow` reaches nothing (AC-12). Listed here — and
    // therefore in `/help` — because a command a user cannot discover is a
    // command they do not have (BUG-153).
    // REQ-572 OQ-3: the enablement walkthrough joins the `/web` family rather
    // than opening a capability-generic `/setup` namespace — a namespace the
    // provider flow can still introduce later without breaking this spelling.
    // It leads the family because it is the command a user with no `[web]`
    // table needs, and the other two are about a capability that is already on.
    CommandSpec {
        name: "web setup",
        aliases: &[],
        summary: "Set up web lookup: pick a tier, name a backend, confirm before anything is \
                  written.",
        args: Args::None,
        handler: handle_web_setup,
    },
    CommandSpec {
        name: "web allow",
        aliases: &[],
        summary: "Lift this session's web taint restriction (grants no new tier).",
        args: Args::None,
        handler: handle_web_allow,
    },
    CommandSpec {
        name: "web refresh",
        aliases: &[],
        summary: "Drop a URL's cached copy so the next lookup re-fetches: /web refresh <url>.",
        args: Args::Required("a URL — `/web refresh <url>`"),
        handler: handle_web_refresh,
    },
    // REQ-579: the second instance of the guided-enablement pattern `/web setup`
    // opened, and it sits beside it for that reason. `Args::Optional` because
    // every form is a real command: the bare line lists the vendors the daemon
    // knows (AC-3), one argument names the vendor, and two carry the tier the
    // model's hand-off was asked for (BR-7). The handler splits them; the table
    // is not the place to police an argument whose vocabulary is the daemon's.
    CommandSpec {
        name: "provider setup",
        aliases: &[],
        summary: "Register a provider and route a tier to it: /provider setup [vendor] [tier] — \
                  confirm before anything is written.",
        args: Args::Optional,
        handler: handle_provider_setup,
    },
    // REQ-581 BR-7: the second `/provider` row, beside the one that registers.
    // `Args::Required` because there is nothing useful a bare form could do —
    // testing *every* provider would be N previews and N outbound calls from one
    // line, which is OQ-2's deferred `teton doctor --probe` and not this row.
    CommandSpec {
        name: "provider test",
        aliases: &[],
        summary: "Test a registered provider with one consented call: /provider test <id>",
        args: Args::Required(
            "a provider id — `/provider test <id>`, and `/provider setup` \
                             registers one",
        ),
        handler: handle_provider_test,
    },
    CommandSpec {
        name: "quit",
        // BUG-153: `/exit` is the same command under the name most other REPLs
        // give it. A user who types it has said exactly what they want;
        // answering with "unknown command" — or, on a build without this table,
        // sending it to the model, which replies conversationally and does not
        // exit — is the BUG-146 shape: the harness was asked and something else
        // answered.
        aliases: &["exit"],
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
/// An [alias](CommandSpec::aliases) is matched here alongside the canonical
/// name and returns [`CommandSpec::name`], so everything downstream — [`resolve`],
/// the handler, and the rejection text — sees the one spelling the table knows.
/// `/exit` therefore cannot dispatch differently from `/quit`: it *is* `/quit`
/// by the time either is looked up.
///
/// The table is a parameter so the longest-match rule is pinned by a fixture
/// table as well as by the real one.
fn split_name<'a>(line: &'a str, table: &'static [CommandSpec]) -> (&'a str, &'a str) {
    let matched = table
        .iter()
        .filter_map(|spec| {
            spec.spellings()
                .find_map(|spelling| Some((spelling, match_name_words(line, spelling)?)))
                .map(|(spelling, args)| (spec, spelling, args))
        })
        // Keyed on the spelling that actually matched, not on the row's
        // canonical name: a one-word alias on a two-word row must not win a
        // longest-match contest it did not enter.
        .max_by_key(|(_, spelling, _)| spelling.split_whitespace().count());
    if let Some((spec, _, args)) = matched {
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
        // `Args::Optional` never rejects here — both forms are real commands and
        // the handler tells them apart.
        _ => Resolution::Run(spec, args),
    }
}

/// How much of a name a rejection echoes back. Long enough that every real
/// command name and every plausible typo survives intact; short enough that the
/// echo is a quotation and not a replay of the line.
const ECHO_MAX_CHARS: usize = 40;

/// What a control character in an echoed name is replaced with.
const ECHO_REPLACEMENT: char = '?';

/// The command name a rejection quotes, with its `/` restored — either as the
/// user typed it, or as the table spells it.
///
/// Two of the three call sites pass the *canonical* [`CommandSpec::name`] (the
/// "takes no arguments" and "needs an argument" arms, which have already matched
/// a row, so the row's spelling is the right one to show — it is what `/help`
/// lists). Only the unknown-command arm passes the user's own bytes, and that is
/// the arm the two guards below exist for.
///
/// [`classify`] strips exactly one `/`, so putting one back reproduces the line.
/// The slash guard matters for the one shape where it would not: a name that
/// already carries a slash must not gain a second one — `//foo` is the escape
/// hatch's spelling (BR-1b), and echoing it at someone who typed something else
/// would name a feature they never used.
///
/// Bounded and sanitised because the unknown-command arm's input is arbitrary.
/// [`classify`]'s whitespace-after-slash branch deliberately keeps the *whole*
/// line remainder as the name so the rejection can quote `/ /foo` faithfully, and
/// a `Surface` writes what it is given: an unbounded echo would replay a pasted
/// paragraph back at the user, and an escape sequence in it would reach the
/// terminal as an escape sequence. Control characters (`\x1b` among them) become
/// [`ECHO_REPLACEMENT`], so what renders is visible, inert, and one line.
fn typed_token(name: &str) -> String {
    let mut token = String::with_capacity(name.len() + 1);
    if !name.starts_with('/') {
        token.push('/');
    }
    token.push_str(&echoed(name));
    token
}

/// Bound and sanitise arbitrary user bytes for quoting back in one line.
///
/// The shared half of [`typed_token`], split out so a rejection that quotes an
/// *argument* rather than a command name — `/permissions bogus` — passes through
/// the same guards instead of growing a second, subtly different copy of them.
/// One place to check, on the way in.
///
/// `pub(crate)` since REQ-572: the `/web setup` flow quotes a mistyped tier
/// answer back, which is the same arbitrary-user-bytes problem, and a second
/// copy of the bounding and defusing is a second place for one of them to be
/// forgotten.
pub(crate) fn echoed(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut chars = text.chars();
    for ch in chars.by_ref().take(ECHO_MAX_CHARS) {
        out.push(if ch.is_control() {
            ECHO_REPLACEMENT
        } else {
            ch
        });
    }
    if chars.next().is_some() {
        out.push('…');
    }
    out
}

/// Render a command line that never reaches a handler: exactly one line, and
/// nothing else (BR-2).
///
/// The hint is a `format!` of static text around [`typed_token`], so the only
/// untrusted bytes in it have already been bounded and stripped of control
/// characters there rather than here — one place to check, on the way in.
fn render_rejection(hint: &str, surface: &mut dyn Surface) {
    surface.line(LineKind::Error, hint);
}

/// `/help`: the command list, generated from [`COMMANDS`] (BR-7), plus the one
/// footer line documenting the `//` escape (BR-1b).
///
/// Aliases are rendered from the same rows, so BR-7 covers them too: a spelling
/// that dispatches cannot be absent from `/help`, and `/help` cannot promise one
/// that does not dispatch.
fn render_help(surface: &mut dyn Surface) {
    // Names pad to the widest row, so a later two-word row (`model set`)
    // re-aligns the whole list instead of breaking out of it.
    let width = COMMANDS
        .iter()
        .map(|spec| spec.name.len())
        .max()
        .unwrap_or(0);
    for spec in COMMANDS {
        // The alias is a tail clause rather than a second column: it belongs to
        // one row, and widening the name column for it would push every other
        // summary right for a fact about `/quit`.
        let also = match spec.aliases {
            [] => String::new(),
            aliases => format!(
                " (also {})",
                aliases
                    .iter()
                    .map(|alias| format!("/{alias}"))
                    .collect::<Vec<_>>()
                    .join(", ")
            ),
        };
        surface.line(
            LineKind::Info,
            &format!("/{:<width$}  {}{also}", spec.name, spec.summary),
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

/// The `/effort` handler (REQ-559 BR-9).
///
/// Bare: read the current level and each provider's clamped level. With an
/// argument: set it — persisted (BR-8) — then read back, so the user sees the
/// clamp their new level lands on rather than only the number they typed.
///
/// Renders through [`crate::effort_ui::render`], the **same** function
/// `teton effort` calls, over the same `config/get` snapshot. Two surfaces
/// describing one setting must not be able to disagree (BR-9, LESSON-456), and
/// there is exactly one renderer so they cannot.
fn handle_effort(
    conn: &mut Connection,
    ctx: &mut UiContext<'_>,
    args: &str,
) -> anyhow::Result<CommandOutcome> {
    let args = args.trim();
    if !args.is_empty() {
        let level: EffortLevel = match args.parse() {
            Ok(level) => level,
            Err(err) => {
                // One line, no RPC — the same shape every other rejected
                // command line takes (BR-2). The error names every accepted
                // spelling, from the list that also defines the enum.
                ctx.surface
                    .line(LineKind::Error, &format!("{err} — {HELP_HINT}"));
                return Ok(CommandOutcome::Continue);
            }
        };
        if let Err(err) = conn.call(
            ConfigSetParams {
                update: ConfigUpdate::SetEffort(level),
            },
            ctx,
        )? {
            ctx.surface.line(
                LineKind::Error,
                &format!("could not set the effort level: {}", err.message),
            );
            return Ok(CommandOutcome::Continue);
        }
    }
    match conn.call(ConfigGetParams::default(), ctx)? {
        Ok(cfg) => {
            // REQ-560: the status row renders from session state, so it has to
            // learn what the daemon just reported — otherwise a `/effort high`
            // would leave the row showing the previous level for the rest of the
            // session. Cached from the daemon's answer rather than from the
            // request, so what the row shows is what actually took effect
            // (a clamped or refused set is reported, not assumed).
            ctx.state.effort = cfg.snapshot.effort.clone();
            crate::effort_ui::render(ctx.surface, cfg.snapshot.effort.as_ref());
        }
        Err(err) => ctx.surface.line(
            LineKind::Error,
            &format!("could not read the effort setting: {}", err.message),
        ),
    }
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
    // The two facts the gate is a pure function of, and neither is read here:
    // `typed_input` is the session's own, taken once at the edge and carried on
    // the context like every other world-fact a handler needs (BR-9 — handlers
    // reach the world through the seams or not at all), and the seam switch is a
    // build-time posture, not a runtime interrogation of the terminal.
    if model_set_gate(ctx.typed_input, test_seams_allowed()) == ModelSetGate::Refuse {
        ctx.surface.line(LineKind::Error, MODEL_SET_TYPED_ONLY);
        return Ok(CommandOutcome::Continue);
    }
    // A bare `/model set` never reaches here: `Args::Required` rejects it at
    // resolve time with the usage line.
    let assume_yes = ctx.auto_accept_model;
    crate::apply_model_set(args, assume_yes, conn, ctx)?;
    Ok(CommandOutcome::Continue)
}

/// The `/web allow` handler: lift this session's taint restriction (REQ-563
/// BR-13 / AC-12).
///
/// User-only **by construction**, not by check. The restriction is lifted by a
/// client RPC, and tool dispatch has no path to one — a model that emits a tool
/// call named `web allow` reaches the tool registry and finds no such tool. There
/// is no "reject if the model asked" branch here because there is no way for the
/// model to have asked.
///
/// It grants nothing. The tiers it names are the ones the machine's `[web] tier`
/// already allows, consent still runs per lookup, and nothing is written to
/// config — a fresh session starts restricted-on-taint again.
///
/// The daemon's answer, not the event, is what this renders: `was_restricted`
/// distinguishes "the restriction is gone" from "there was none", and a client
/// that could not tell those apart would confirm a lift that never happened.
fn handle_web_allow(
    conn: &mut Connection,
    ctx: &mut UiContext<'_>,
    _args: &str,
) -> anyhow::Result<CommandOutcome> {
    let Some(params) = web_override_params(ctx.session_id.clone()) else {
        ctx.surface.line(LineKind::Error, WEB_NEEDS_A_SESSION);
        return Ok(CommandOutcome::Continue);
    };
    let answered = conn.call(params, ctx)?;
    if let Some(result) = web_override_or_report(answered, ctx.surface) {
        ctx.surface
            .line(LineKind::Notice, &render_web_override(&result));
    }
    Ok(CommandOutcome::Continue)
}

/// The `/web setup` handler: the guided enablement walkthrough (REQ-572 BR-2,
/// ADR-1/ADR-3).
///
/// User-only by construction, exactly as `/web allow` is: it is a slash command,
/// and tool dispatch has no path to this table — a model that emits a tool call
/// named `web setup` reaches the tool registry and finds no such tool. The
/// daemon does not take that on trust and gates all three RPCs on session
/// attachment anyway (TASK-130), announcing a refusal it did not admit.
///
/// Everything it does lives in [`crate::web_setup_ui`]: the collection, the
/// preview render, the default-no confirm, the keychain write and its undo. The
/// handler's whole job is to hand that flow the session's connection, the
/// session's context and the platform keychain — the same division `/model set`
/// has with [`crate::apply_model_set`], and for the same reason (LESSON-441: one
/// consent gate, one implementation).
fn handle_web_setup(
    conn: &mut Connection,
    ctx: &mut UiContext<'_>,
    _args: &str,
) -> anyhow::Result<CommandOutcome> {
    // Constructed here rather than inside the flow so the flow takes a
    // `&dyn Keychain` and its tests can hand it one that touches no OS store.
    let keychain = crate::keychain::default_keychain();
    crate::web_setup_ui::run(conn, ctx, keychain.as_ref())?;
    Ok(CommandOutcome::Continue)
}

/// What `/provider setup` says to a line carrying more than a vendor and a tier
/// (REQ-579).
///
/// A third word is a typo, and reading it as anything would mean guessing which
/// of two arguments the user meant — the same reason `Args::None` rejects a
/// stray argument rather than ignoring it (BR-2).
const PROVIDER_SETUP_USAGE: &str =
    "`/provider setup` takes at most a vendor and a tier — `/provider setup kimi think`. Nothing \
     was changed.";

/// The `/provider setup [vendor] [tier]` handler: the guided registration
/// walkthrough (REQ-579 BR-3, ADR-1).
///
/// User-only by construction, exactly as `/web setup` is: it is a slash command,
/// and tool dispatch has no path to this table — a model that emits a tool call
/// named `provider setup` reaches the tool registry and finds no such tool. The
/// daemon does not take that on trust and gates all three RPCs anyway (BR-12).
///
/// Everything it does lives in [`crate::provider_setup_ui`]: the vendor
/// resolution, the collection, the preview render, the default-no confirm, the
/// keychain write and its undo. The handler's whole job is to split the argument
/// and hand that flow the session's connection, the session's context and the
/// platform keychain — the same division `/web setup` has, and for the same
/// reason (LESSON-441: one consent gate, one implementation).
///
/// It is **not** refused on a pipe the way `/model set` is. The flow's own gate
/// degrades a non-typed session to the CLI recipe and reads no stdin (BR-11),
/// which is a better answer than a refusal for a command whose whole purpose is
/// to tell a user how to get a provider registered.
fn handle_provider_setup(
    conn: &mut Connection,
    ctx: &mut UiContext<'_>,
    args: &str,
) -> anyhow::Result<CommandOutcome> {
    let mut words = args.split_whitespace();
    let vendor = words.next();
    let tier = words.next();
    if words.next().is_some() {
        ctx.surface.line(LineKind::Error, PROVIDER_SETUP_USAGE);
        return Ok(CommandOutcome::Continue);
    }
    // Constructed here rather than inside the flow so the flow takes a
    // `&dyn Keychain` and its tests can hand it one that touches no OS store.
    let keychain = crate::keychain::default_keychain();
    crate::provider_setup_ui::run(conn, ctx, keychain.as_ref(), vendor, tier)?;
    Ok(CommandOutcome::Continue)
}

/// What `/provider test` says to a line carrying more than an id (REQ-581).
///
/// A second word is a typo, and reading it as anything would mean guessing which
/// provider the user meant — for a command that spends. The same reason
/// [`PROVIDER_SETUP_USAGE`] exists, with a sharper edge.
const PROVIDER_TEST_USAGE: &str =
    "`/provider test` takes one provider id — `/provider test kimi`. Nothing was sent.";

/// The `/provider test <id>` handler: one consented call to a registered
/// provider (REQ-581 BR-2/BR-7).
///
/// User-only by construction, exactly as `/provider setup` is: it is a slash
/// command, and tool dispatch has no path to this table — a model that emits a
/// tool call named `provider test` reaches the tool registry and finds no such
/// tool. The daemon does not take that on trust and gates the method on session
/// attachment anyway (architecture ADR-5), which is also what stops a
/// `teton provider test … --yes` spawned through the shell tool.
///
/// Everything it does lives in [`crate::provider_test_ui`]: the preview, the
/// default-no confirm, the call and the typed report. The handler's whole job is
/// to check the argument and hand that flow the session's connection and
/// context — the same division `/provider setup` has, and for the same reason
/// (LESSON-441: one consent gate, one implementation).
///
/// It is **not** refused on a pipe the way `/model set` is, and it does not
/// degrade the way `/provider setup` does either: the flow's own gate answers a
/// non-typed session with the `--yes` remedy and sends nothing (BR-2), because a
/// command whose entire body is an outbound request has no reduced form to
/// offer.
fn handle_provider_test(
    conn: &mut Connection,
    ctx: &mut UiContext<'_>,
    args: &str,
) -> anyhow::Result<CommandOutcome> {
    let mut words = args.split_whitespace();
    // `Args::Required` already rejected the bare line, so the first word is
    // present by the time a handler runs.
    let Some(id) = words.next() else {
        ctx.surface.line(LineKind::Error, PROVIDER_TEST_USAGE);
        return Ok(CommandOutcome::Continue);
    };
    if words.next().is_some() {
        ctx.surface.line(LineKind::Error, PROVIDER_TEST_USAGE);
        return Ok(CommandOutcome::Continue);
    }
    crate::provider_test_ui::run_in_session(conn, ctx, id)?;
    Ok(CommandOutcome::Continue)
}

/// What `/permissions` says when there is no session to read a level from.
///
/// Reachable only from a context that owns no session, the same guard
/// [`WEB_NEEDS_A_SESSION`] is — it keeps the id from being fabricated rather
/// than being a line users meet.
const PERMISSIONS_NEEDS_A_SESSION: &str =
    "`/permissions` needs a session to act on, and this command owns none.";

/// The `/permissions [level]` handler (REQ-560 BR-10 / BR-14).
///
/// Bare, it **reads**: the level is otherwise visible only in the entry frame's
/// status row, which BR-9 hides whenever stdin is not a terminal. A setting
/// whose only surface is a TTY row is unreadable to exactly the users who
/// script, so this form works on a pipe and is the non-visual read path the
/// status row is allowed to exist without.
///
/// With an argument, it **sets** — for this session only. Nothing is written to
/// config (BR-6); the daemon's gate is the one thing that changes, and the next
/// session starts from `[permissions] default_level` again.
///
/// An unrecognised level renders the four valid spellings and their summaries
/// and issues **no RPC**. Guessing at a near-miss is the one thing this must not
/// do: the argument decides whether shell commands run without asking, and a
/// lenient match would mean the spelling the user did not intend is the one
/// nobody tests.
///
/// Unlike `/model set`, this is not typed-input-only. That restriction exists
/// because `/model set` changes *machine* state; a permission level evaporates
/// with the session, and BR-10 requires the read to work on a pipe.
fn handle_permissions(
    conn: &mut Connection,
    ctx: &mut UiContext<'_>,
    args: &str,
) -> anyhow::Result<CommandOutcome> {
    let Some(session_id) = ctx.session_id.clone() else {
        ctx.surface
            .line(LineKind::Error, PERMISSIONS_NEEDS_A_SESSION);
        return Ok(CommandOutcome::Continue);
    };

    let level = if args.is_empty() {
        None
    } else {
        match PermissionLevel::parse(args) {
            Some(level) => Some(level),
            None => {
                // Rendered before any RPC: an unknown level is a typo, and the
                // session's posture must not be touched to find that out.
                ctx.surface.line(LineKind::Error, &unknown_level_line(args));
                for level in PermissionLevel::ALL {
                    ctx.surface
                        .line(LineKind::Info, &render_level_option(*level));
                }
                return Ok(CommandOutcome::Continue);
            }
        }
    };

    let answered = conn.call(SessionPermissionsParams { session_id, level }, ctx)?;
    if let Some(result) = permissions_or_report(answered, ctx.surface) {
        // The status row renders from session state, so it has to learn what the
        // daemon just told us — otherwise the row would keep showing the old
        // level until the next session. Cached from the daemon's answer rather
        // than from the request, so what is rendered is what actually happened.
        ctx.state.permission_level = Some(result.level);
        ctx.surface
            .line(LineKind::Notice, &render_permissions(&result));
    }
    Ok(CommandOutcome::Continue)
}

/// The rejection an unrecognised level gets back, quoting what was typed through
/// the same bounded, sanitised echo an unknown command name goes through.
fn unknown_level_line(typed: &str) -> String {
    format!(
        "unknown permission level: `{}` — this session was not changed.",
        echoed(typed)
    )
}

/// One level's line in the list an unrecognised argument prints.
fn render_level_option(level: PermissionLevel) -> String {
    format!("  {:<8} {}", level.name(), level.summary())
}

/// The line `/permissions` renders from the daemon's answer.
///
/// Three cases, and they are distinguishable on purpose: a read states the
/// level, a real change announces it, and a set that changed nothing says so
/// rather than confirming a change that did not happen — the same honesty
/// `was_restricted` gives `/web allow`.
fn render_permissions(result: &SessionPermissionsResult) -> String {
    if result.changed {
        format!(
            "permission level: {} — {}",
            result.level.name(),
            result.level.summary()
        )
    } else {
        format!(
            "permission level: {} (unchanged) — {}",
            result.level.name(),
            result.level.summary()
        )
    }
}

/// Unwrap the daemon's answer, or render why there is none.
///
/// The daemon-too-old arm is separate from the error arm for the reason
/// [`web_override_or_report`]'s is: a method a daemon does not serve is a
/// version fact the user can act on, not a failure of the command.
fn permissions_or_report(
    answered: Result<SessionPermissionsResult, RpcError>,
    surface: &mut dyn Surface,
) -> Option<SessionPermissionsResult> {
    match answered {
        Ok(result) => Some(result),
        Err(err) if err.code == error_code::METHOD_NOT_FOUND => {
            surface.line(LineKind::Notice, PERMISSIONS_UNAVAILABLE);
            None
        }
        Err(err) => {
            surface.line(
                LineKind::Error,
                &format!("the permission level is unavailable: {}", err.message),
            );
            None
        }
    }
}

/// What `/permissions` says to a daemon that predates the method.
const PERMISSIONS_UNAVAILABLE: &str =
    "this daemon does not serve permission levels — restart it after upgrading to use \
     /permissions.";

/// The `/web refresh <url>` handler: drop a cached document (BR-12 / AC-10).
///
/// The URL is the user's own typed argument and travels to the daemon verbatim;
/// it is never echoed back, and the daemon's answer names an outcome alone.
/// Sent unvalidated on purpose — the cache is keyed by a normalization the
/// daemon owns, and a client-side opinion about what a URL is would be a second
/// definition that could disagree with the one the entry was written under.
fn handle_web_refresh(
    conn: &mut Connection,
    ctx: &mut UiContext<'_>,
    args: &str,
) -> anyhow::Result<CommandOutcome> {
    // A bare `/web refresh` never reaches here: `Args::Required` rejects it at
    // resolve time with the usage line.
    let answered = conn.call(
        WebRefreshParams {
            url: args.trim().to_owned(),
        },
        ctx,
    )?;
    if let Some(result) = web_refresh_or_report(answered, ctx.surface) {
        ctx.surface.line(
            LineKind::Notice,
            match result.outcome {
                WebRefreshOutcome::Evicted => WEB_REFRESH_EVICTED,
                WebRefreshOutcome::Absent => WEB_REFRESH_ABSENT,
            },
        );
    }
    Ok(CommandOutcome::Continue)
}

/// The `web/override` request for a session, or `None` when there is no session
/// to name.
///
/// Split out so the refusal is testable without a socket — the same reason
/// [`crate::cost_report_or_report`] is split out. The `None` arm is the one a
/// test process can otherwise not reach, and it is the arm that guarantees no
/// session id is ever fabricated.
fn web_override_params(session_id: Option<SessionId>) -> Option<WebOverrideParams> {
    session_id.map(|session_id| WebOverrideParams { session_id })
}

/// What `/web allow` says when no session exists to lift a restriction on.
///
/// Reachable only from a context that owns no session — `teton cost` and the
/// other passive commands run no slash handlers, so in practice this is the
/// guard that keeps the id from being fabricated rather than a line users meet.
const WEB_NEEDS_A_SESSION: &str =
    "`/web allow` needs a session to act on, and this command owns none.";

/// `/web refresh` found and removed a stored copy.
const WEB_REFRESH_EVICTED: &str =
    "web cache: the stored copy was dropped; the next lookup of that URL re-fetches.";

/// `/web refresh` found nothing — a fact, not a failure.
const WEB_REFRESH_ABSENT: &str = "web cache: nothing was stored for that URL; the next lookup \
                                  was already going to fetch it fresh.";

/// The line `/web allow` renders from the daemon's answer.
///
/// Split out for the reason [`crate::cost_report_or_report`] is: the wording is
/// the behaviour, and both arms are asserted without a socket.
fn render_web_override(result: &WebOverrideResult) -> String {
    if !result.was_restricted {
        return "nothing was restricted: this session has not read privacy-boundary content, \
                so model-composed web lookups were never disabled."
            .to_owned();
    }
    if result.tiers_restored.is_empty() {
        return "web taint restriction lifted for this session. No web tier is configured, so \
                nothing resumed — set `[web] tier` to grant one."
            .to_owned();
    }
    let named = result
        .tiers_restored
        .iter()
        .map(|t| format!("`{}`", web_tier_name(*t)))
        .collect::<Vec<_>>()
        .join(", ");
    format!(
        "web taint restriction lifted for this session; model-composed lookups resume at: \
         {named}. This granted no new tier, and a fresh session starts restricted again."
    )
}

/// Unwrap a `web/override` answer, reporting a daemon too old to have the method
/// as a notice and any other failure as an error.
///
/// The three-arm split is [`crate::cost_report_or_report`]'s, for its reason: a
/// build without the method is a version fact and not a failure, so it must not
/// wear an `error:` prefix (BUG-152).
fn web_override_or_report(
    answered: Result<WebOverrideResult, RpcError>,
    surface: &mut dyn Surface,
) -> Option<WebOverrideResult> {
    match answered {
        Ok(result) => Some(result),
        Err(err) if err.code == error_code::METHOD_NOT_FOUND => {
            surface.line(LineKind::Notice, WEB_METHODS_UNAVAILABLE);
            None
        }
        Err(err) => {
            surface.line(
                LineKind::Error,
                &format!(
                    "the web taint restriction could not be lifted: {}",
                    err.message
                ),
            );
            None
        }
    }
}

/// Unwrap a `web/refresh` answer; same three arms as [`web_override_or_report`].
fn web_refresh_or_report(
    answered: Result<WebRefreshResult, RpcError>,
    surface: &mut dyn Surface,
) -> Option<WebRefreshResult> {
    match answered {
        Ok(result) => Some(result),
        Err(err) if err.code == error_code::METHOD_NOT_FOUND => {
            surface.line(LineKind::Notice, WEB_METHODS_UNAVAILABLE);
            None
        }
        Err(err) => {
            surface.line(
                LineKind::Error,
                &format!("the cached document could not be dropped: {}", err.message),
            );
            None
        }
    }
}

/// What both web commands say to a daemon built before REQ-563.
const WEB_METHODS_UNAVAILABLE: &str =
    "this daemon build does not expose the web lookup controls yet.";

/// The `/clear` handler: drop this session's retained conversation (REQ-567
/// BR-8 / AC-6).
///
/// ## It renders nothing when it succeeds, and that is the point
///
/// A clear produces two things a client could draw: this RPC's answer, carrying
/// `blocks_dropped`, and the broadcast `context_cleared` event, carrying the
/// same number. Rendering both would print the same fact twice to the one client
/// that typed the command, which is the drift `/web allow` avoids from the other
/// direction — there the *answer* is authoritative (`was_restricted` is a fact
/// only the daemon's reply carries) and the event is verbose-gated for the
/// bystanders.
///
/// Here the event is the authoritative line, because it says everything there is
/// to say: the count is on the event too, and every attached client has to stop
/// describing a conversation the next prompt will not carry (`session_ui`'s
/// `context_cleared` arm). So the issuing client and a second attached client
/// see *the same one line*, drawn by the same code, and this handler is silent
/// on success. The daemon's reader loop fences a request's events ahead of its
/// response, so that line has already been drawn by [`Connection::call`]'s own
/// event pump by the time this returns — the ordering is the server's, not a
/// hopeful assumption here.
///
/// Only failures are rendered, and the busy one is a **notice** rather than an
/// error (BUG-152): a session already running a turn is not something the user
/// broke or has to fix, it resolves by itself, and the daemon's own sentence
/// names the turn holding it and says to retry. The class is matched on the
/// daemon's code, never on its wording, for [`crate::render_turn_failure`]'s
/// reason (LESSON-456).
///
/// ## Why there is no typed-input gate
///
/// `/model set` is refused on piped stdin because it changes **daemon** state
/// that outlives the session — the selection every later session and every other
/// client inherits, on a machine whose RAM floor the user was asked about. A
/// clear is none of that. It is session-scoped, it takes no consent, it spends
/// no money, and what it destroys is conversational convenience: the retained
/// blocks, and nothing else.
///
/// ## What the summary line promises, and what it does not
///
/// "Drop this session's retained conversation; the next prompt starts fresh" is
/// the whole claim, and it is exact: the conversation is what every later prompt
/// is assembled from, and after this there is none. It deliberately does **not**
/// promise that the cleared bytes have left the machine's memory. The local
/// engine's prefix cache still holds the cleared conversation's tokens until the
/// next prompt's prefill overwrites them (architecture D-5): process memory
/// only, never disk, and unreachable through any prompt, because the cache is
/// keyed by comparing token ids against the *new* prompt and nothing past the
/// common prefix can be decoded from. Nobody is told otherwise, and the one-line
/// summary is left as-is rather than qualified — a help row that hedged about
/// KV residency would trade a promise nobody made for a sentence nobody can
/// act on. A user who needs the bytes gone ends the session (BR-9).
///
/// OQ-4 is resolved to exactly that — the session's
/// privacy taint, its pasted-URL set, and its remembered permission grants all
/// survive a clear, so a `/clear` a script could type can widen no boundary and
/// grant no permission. It is therefore pipe-friendly like every other command
/// (BR-9), and gating it would buy nothing while making the one documented
/// exception to BR-9 into a pattern.
fn handle_clear(
    conn: &mut Connection,
    ctx: &mut UiContext<'_>,
    _args: &str,
) -> anyhow::Result<CommandOutcome> {
    let Some(params) = session_clear_params(ctx.session_id.clone()) else {
        ctx.surface.line(LineKind::Error, CLEAR_NEEDS_A_SESSION);
        return Ok(CommandOutcome::Continue);
    };
    // The session's own connection and context (D-4). The `context_cleared`
    // event lands on this pump, so the notice is drawn before the answer arrives
    // — and the answer, on success, is deliberately dropped.
    if let Err(err) = conn.call(params, ctx)? {
        report_clear_refusal(&err, ctx.surface);
    }
    Ok(CommandOutcome::Continue)
}

/// The `session/clear` request for a session, or `None` when there is no session
/// to name.
///
/// Split out for [`web_override_params`]'s reason: the `None` arm is the one a
/// test process cannot otherwise reach, and it is the arm that guarantees no
/// session id is ever fabricated.
fn session_clear_params(session_id: Option<SessionId>) -> Option<SessionClearParams> {
    session_id.map(|session_id| SessionClearParams { session_id })
}

/// What `/clear` says when there is no session whose conversation to drop.
const CLEAR_NEEDS_A_SESSION: &str =
    "`/clear` needs a session to act on, and this command owns none.";

/// What `/clear` says to a daemon built before REQ-567.
///
/// A version fact, not a failure, so it wears no `error:` prefix (BUG-152) — and
/// it says what is true of such a daemon rather than only that the call failed:
/// a build with no `session/clear` retains nothing across prompts either, so
/// there is nothing there to clear.
const CLEAR_UNAVAILABLE: &str = "this daemon build does not retain a conversation across prompts, \
                                 so there is nothing to clear.";

/// Render the reason a `/clear` did not happen — and nothing at all when it did.
///
/// Split out of the handler so all three arms are asserted without a socket, for
/// the reason [`crate::cost_report_or_report`] is: the wording and the line class
/// *are* the behaviour here, and the busy arm is the one an e2e can only reach by
/// racing a turn.
fn report_clear_refusal(err: &RpcError, surface: &mut dyn Surface) {
    match err.code {
        // A build without the method (BUG-152's class), and a session that is
        // simply busy right now — both transient or informational, neither a
        // failure the user caused or can fix.
        error_code::METHOD_NOT_FOUND => surface.line(LineKind::Notice, CLEAR_UNAVAILABLE),
        // The daemon's sentence names the turn holding the session and says to
        // retry when it finishes, so this adds only the fact the daemon cannot
        // know the user is waiting on: that nothing was dropped.
        error_code::SESSION_BUSY => surface.line(
            LineKind::Notice,
            &format!("nothing was cleared: {}", err.message),
        ),
        _ => surface.line(
            LineKind::Error,
            &format!("the conversation could not be cleared: {}", err.message),
        ),
    }
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
/// `typed_input` is the honest name for what is actually known: the session's
/// stdin was a terminal when the process started. It bounds the rule without
/// implementing it — a pty opened by `expect(1)`, a tmux `send-keys`, or a
/// paste into a real terminal all satisfy it. The spec's Permissions row records
/// that residual as accepted: what this buys is that the common unattended
/// shapes (a heredoc, a `<<<` string, a piped file, a CI step) cannot change the
/// selection without naming the auditable surface instead.
///
/// `seams_allowed` is the escape hatch the e2e suite drives the flow through —
/// [`test_seams_allowed`], never a plain environment variable, so a shipped
/// binary cannot be talked out of the gate.
///
/// Pure, so both answers are unit-tested without a terminal, a pipe, or a
/// daemon: the branch that matters is the one a test process cannot otherwise
/// reach.
fn model_set_gate(typed_input: bool, seams_allowed: bool) -> ModelSetGate {
    if typed_input || seams_allowed {
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
///
/// **Invariant for any future caller.** Silently ignoring the switch is
/// fail-closed *only because* this function's sole consumer reads it with one
/// polarity: `seams_allowed` makes [`model_set_gate`] looser, so dropping it on
/// a release build can only refuse something that would otherwise have run. A
/// consumer that used the switch to make behaviour *stricter* would invert that
/// — ignoring it would silently loosen the shipped binary — and must mirror the
/// daemon's posture instead (`tetond`'s `test_seams_enabled`: refuse to run at
/// all rather than run with an unhonoured seam).
///
/// REQ-572 adds the second consumer, [`crate::web_setup_ui::gate`], with the
/// **same** polarity: the seam lets the walkthrough run on a pipe, so a release
/// build ignoring the switch can only fall back to printing instructions. The
/// invariant above is what made that reuse legitimate rather than convenient.
pub(crate) fn test_seams_allowed() -> bool {
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
    // Only the tests name a tier now: the tier vocabulary itself moved to
    // `session_ui`, so the production code here never mentions the type.
    use teton_protocol::events::WebTier;

    /// The session's own context (D-4). No answers are scripted: none of the
    /// client-local commands asks a question.
    ///
    /// `typed_input` stands for a session someone is typing into, which is what
    /// the client-local commands tested here run under. No test in this module
    /// reaches the `/model set` gate through a context — the gate is pure and is
    /// pinned directly, both answers, in
    /// [`model_set_runs_only_from_a_terminal_or_under_the_test_seam`].
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
            typed_input: true,
            session_id: None,
        }
    }

    /// REQ-559 BR-9: `/effort` must dispatch **both** bare (read) and with a
    /// level (set). `Args::Optional` exists for exactly this shape, and a row
    /// that only worked one way would fail half the requirement silently —
    /// `Required` would reject the read, `None` would reject the set.
    #[test]
    fn an_optional_argument_row_dispatches_both_ways() {
        let optional: Vec<&str> = COMMANDS
            .iter()
            .filter(|s| matches!(s.args, Args::Optional))
            .map(|s| s.name)
            .collect();
        assert!(
            optional.contains(&"effort"),
            "REQ-559 owns the /effort row and its bare read path (BR-9)",
        );
        assert!(
            optional.contains(&"permissions"),
            "REQ-560 owns the /permissions row and its bare read path (BR-10)",
        );
        for name in optional {
            for line in [format!("/{name}"), format!("/{name} high")] {
                let Input::Command { name: n, args } = classify(&line) else {
                    panic!("`{line}` did not classify as a command");
                };
                assert!(
                    matches!(resolve(n, args), Resolution::Run(..)),
                    "`{line}` must dispatch, not be rejected",
                );
            }
        }
    }

    /// BR-9 / REQ-555 BR-7: `/help` is generated from `COMMANDS`, so the
    /// `/effort` row appears there by construction. Asserted rather than
    /// assumed — the claim is what makes a separate `/help` edit unnecessary.
    #[test]
    fn effort_appears_in_help_because_help_is_generated_from_the_table() {
        let mut surface = RecordingSurface::new();
        render_help(&mut surface);
        assert!(
            surface
                .lines_of(LineKind::Info)
                .iter()
                .any(|l| l.contains("/effort")),
            "the /effort row must reach /help without a separate edit",
        );
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
                // REQ-559: an `Optional` row is reachable both ways, and the
                // *bare* form is the one BR-9 requires to work (it is the read
                // path). The bare case is what this loop types; the argument
                // case is covered by `an_optional_argument_row_dispatches_both_ways`
                // below, which asserts the pair rather than only one side.
                Args::Optional => "",
            };
            // Every spelling, not just the canonical one: an alias that is in
            // the table but unreachable from typed input is the same defect as
            // an unreachable row, and it fails here for the same reason.
            for spelling in spec.spellings() {
                let typed = format!("/{spelling} {expected_args}");
                let typed = typed.trim_end();
                let Input::Command { name, args } = classify(typed) else {
                    panic!("`{typed}` did not classify as a command");
                };
                // An alias canonicalises on the way through, so what dispatches
                // is the row's own name whichever spelling was typed.
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
    }

    // BR-7 for the aliases: `/help` is generated from the same rows that
    // dispatch, so a spelling that works must be listed. The `/quit` row is the
    // only one carrying an alias today, and the promise it makes is concrete —
    // someone who types `/exit` out of habit ends the session.
    #[test]
    fn help_lists_every_alias_that_dispatches() {
        let mut surface = RecordingSurface::new();
        render_help(&mut surface);
        let listing = surface.lines_of(LineKind::Info).join("\n");
        for spec in COMMANDS {
            for alias in spec.aliases {
                assert!(
                    listing.contains(&format!("/{alias}")),
                    "/{alias} dispatches but /help never mentions it:\n{listing}"
                );
            }
        }
        // Both directions, as everything else in this module is pinned
        // (LESSON-479): the listing must not promise a spelling that is not in
        // the table either.
        assert!(
            COMMANDS
                .iter()
                .any(|spec| spec.aliases.contains(&"exit") && spec.name == "quit"),
            "/exit must resolve to the quit row, not to a row of its own"
        );
    }

    // BUG-153: `/exit` is what the user typed to leave, and on the build they
    // were running it reached the model, which answered
    // conversationally and did not exit. It must resolve to the *same row* as
    // `/quit` — not to a parallel one that happens to return `Quit` today and
    // could drift tomorrow. Same row ⇒ same handler ⇒ the same silent exit,
    // which `slash_quit_and_ctrl_d_leave_by_the_same_path` pins end to end.
    #[test]
    fn exit_resolves_to_the_very_same_row_as_quit() {
        let Input::Command { name, args } = classify("/exit") else {
            panic!("`/exit` must be a command line, never a prompt");
        };
        assert_eq!(name, "quit", "`/exit` must canonicalise to the quit row");
        let (Resolution::Run(from_exit, exit_args), Resolution::Run(from_quit, quit_args)) =
            (resolve(name, args), resolve("quit", ""))
        else {
            panic!("`/exit` or `/quit` did not dispatch");
        };
        assert!(
            std::ptr::eq(from_exit, from_quit),
            "`/exit` reached a different row than `/quit`"
        );
        assert_eq!(exit_args, quit_args, "neither spelling takes an argument");
    }

    // The loop above proves every row in the table is reachable — and stays
    // green if a row is *deleted*. This is the other half of that invariant
    // (LESSON-479): the commands this REQ promises are in the table at all.
    #[test]
    fn the_table_carries_every_command_this_req_promises() {
        let names: Vec<&str> = COMMANDS.iter().map(|spec| spec.name).collect();
        let promised = [
            "help",
            "cost",
            "model",
            "model set",
            "verbose",
            "quit",
            // REQ-563 BR-13 / BR-12: the two user-only web actions. Added here
            // first, deliberately — this list is where a new row is declared to
            // be a spec decision rather than a drive-by.
            "web allow",
            "web refresh",
            // REQ-572 BR-2 / OQ-3: the guided enablement walkthrough, declared
            // here for the reason the rows above were. The spelling is the
            // settled one — it joins the `/web` family rather than opening a
            // `/setup` namespace this REQ would then own alone.
            "web setup",
            // REQ-567 BR-8: the user-only clear, declared here for the same
            // reason the web rows were — a new command is a spec decision.
            "clear",
            // REQ-559 BR-9: this REQ owns `/effort` — the row, its bare-argument
            // read path, and its `/help` entry. Declared here first, as the
            // rows above were. REQ-560 renders the effort *value* in its status
            // line and adds `/permissions`; it does not add or alias this one,
            // and both specs previously claimed the command, which is why the
            // ownership is stated rather than assumed.
            "effort",
            // REQ-560 BR-14: the session's permission level, and the other half
            // of the pair above — one REQ owns each row, and the status line
            // renders both values without either REQ growing a second way to
            // type the other's command.
            "permissions",
            // REQ-579 BR-3 / AC-14: the guided provider registration, declared
            // here for the reason every row above it was. It is the second
            // instance of `/web setup`'s pattern and the first `/provider` row —
            // REQ-555 deferred the namespace ("stays shell-only in v1 … if
            // promoted later"), and this is that promotion, so the spelling is a
            // spec decision and not a drive-by.
            "provider setup",
            // REQ-581 BR-7 / AC-9: the connection test, declared here for the
            // reason every row above it was. It is the second `/provider` row
            // and the first command in the table whose body is an outbound
            // request, so the spelling is a spec decision and not a drive-by.
            "provider test",
        ];
        for expected in promised {
            assert!(
                names.contains(&expected),
                "/{expected} is missing from the dispatch table: {names:?}"
            );
        }
        // The count closes the third direction: the loop above proves the named
        // rows are present and the reachability loop proves each row dispatches,
        // but neither notices an *unlisted* row. The command surface is
        // deliberately small, so a new row is a spec decision and lands in the
        // list above first.
        assert_eq!(
            COMMANDS.len(),
            promised.len(),
            "the table grew past the commands the specs scope: {names:?}"
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
        let session = SessionId::from("sess-under-test");
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
        // The refusal names the surface that does the same thing unattended —
        // and the flag that surface needs, because a script that runs `teton
        // model set` without `--yes` meets the above-floor confirmation on a
        // stdin nobody is typing into and is silently declined by the EOF.
        assert!(MODEL_SET_TYPED_ONLY.contains("teton model set"));
        assert!(MODEL_SET_TYPED_ONLY.contains("--yes"));
        assert_eq!(MODEL_SET_TYPED_ONLY.lines().count(), 1);
        // What the message claims is what the gate checks: a terminal, not a
        // human. Over-claiming here is how a control becomes load-bearing for
        // something it never enforced.
        assert!(MODEL_SET_TYPED_ONLY.contains("not a terminal"));
    }

    // The unknown-command arm echoes bytes the user chose, and a `Surface`
    // writes what it is given. So the echo is bounded and inert: an escape
    // sequence must not reach the terminal as an escape sequence, and a pasted
    // paragraph must not be replayed in full.
    #[test]
    fn a_rejection_echo_is_bounded_and_carries_no_control_characters() {
        let tail = "very-long-tail-".repeat(20);
        let typed = format!("/ \x1b[31mX {tail}");

        let Input::Command { name, args } = classify(&typed) else {
            panic!("a line opening with a slash is always a command line");
        };
        let Resolution::Rejected(hint) = resolve(name, args) else {
            panic!("`{typed}` reached a handler");
        };
        // The classifier hands the whole remainder over as the name — that is
        // deliberate (it is what lets `/ /foo` be quoted faithfully) and is
        // exactly why the echo, not the classifier, is where the bound lives.
        assert!(name.len() > ECHO_MAX_CHARS);

        let mut surface = RecordingSurface::new();
        render_rejection(&hint, &mut surface);
        assert_eq!(surface.calls.len(), 1, "the hint is the only output");

        assert!(!hint.contains('\x1b'), "an escape byte survived: {hint:?}");
        assert!(
            !hint.chars().any(char::is_control),
            "a control character survived: {hint:?}"
        );
        assert!(
            hint.contains(ECHO_REPLACEMENT),
            "the stripped escape left no visible trace: {hint:?}"
        );
        // Bounded: the static hint text plus at most the echo's own budget.
        assert!(
            hint.chars().count() < HELP_HINT.chars().count() + ECHO_MAX_CHARS + 32,
            "the echo replayed the line rather than quoting it: {hint:?}"
        );
        assert!(!hint.contains(&tail), "{hint:?}");
        assert!(hint.contains("unknown command"), "{hint}");
        assert!(hint.contains("/help"), "{hint}");

        // A name short enough to quote is still quoted whole, with nothing
        // added: the bound is a ceiling, not a reformatting.
        assert_eq!(typed_token("frobnicate"), "/frobnicate");
        assert_eq!(typed_token(" /foo"), "/ /foo");
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
                // A one-word alias on the one-word row, spelled so that it is
                // also a prefix of the two-word row's first word: if the
                // longest-match key were taken from the row's canonical name
                // rather than from the spelling that matched, this alias would
                // start winning lines that belong to `model set`.
                name: "model",
                aliases: &["mo"],
                summary: "show the current model",
                args: Args::None,
                handler: handle_help,
            },
            CommandSpec {
                name: "model set",
                aliases: &[],
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
        // An alias canonicalises to its row's name, argument intact.
        assert_eq!(split_name("mo", FIXTURE), ("model", ""));
        assert_eq!(split_name("mo now", FIXTURE), ("model", "now"));
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
            // The summary is the line's body; a row carrying aliases appends
            // its "(also …)" clause after it, so the assertion is about where
            // the summary sits rather than about the line ending there.
            assert!(line.contains(spec.summary), "{line}");
            let tail = line.split_once(spec.summary).expect("the summary").1;
            match spec.aliases {
                [] => assert!(tail.is_empty(), "a row with no alias trails: {line}"),
                aliases => {
                    for alias in aliases {
                        assert!(tail.contains(&format!("/{alias}")), "{line}");
                    }
                }
            }
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
    // ------------------------------------------------------------------
    // REQ-563: the two web commands (BR-12 / BR-13, AC-10 / AC-12)
    // ------------------------------------------------------------------

    /// BUG-153's discoverability rule, applied to the new rows: a command a user
    /// cannot find in `/help` is a command they do not have. The generic help
    /// test proves every row renders; this names the two this REQ adds, so
    /// deleting a row fails here rather than quietly shrinking the listing.
    #[test]
    fn help_lists_both_web_commands_with_their_usage() {
        let mut surface = RecordingSurface::new();
        render_help(&mut surface);
        let listing = surface.lines_of(LineKind::Info).join("\n");

        assert!(listing.contains("/web allow"), "{listing}");
        assert!(listing.contains("/web refresh"), "{listing}");
        // The refresh row has to show it takes a URL, or the argument is a
        // secret the user has to guess.
        assert!(listing.contains("/web refresh <url>"), "{listing}");
        // And `/web allow`'s summary must not imply it grants anything (BR-13).
        assert!(listing.contains("grants no new tier"), "{listing}");
    }

    /// REQ-572 BR-2: the enablement path has to be discoverable from inside the
    /// session that just refused to do something, which means `/help`. Its
    /// summary must also say that nothing is written without a confirmation —
    /// a command that edits config is one users should be able to try.
    #[test]
    fn help_lists_the_setup_command_and_promises_the_confirmation() {
        let mut surface = RecordingSurface::new();
        render_help(&mut surface);
        let listing = surface.lines_of(LineKind::Info).join("\n");

        assert!(listing.contains("/web setup"), "{listing}");
        assert!(
            listing.contains("confirm before anything is written"),
            "the summary must promise the confirm step: {listing}"
        );
    }

    // ------------------------------------------------------------------
    // REQ-579: the guided provider registration (AC-3, AC-9, AC-14)
    // ------------------------------------------------------------------

    /// AC-14 / REQ-555 BR-7: `/help` is generated from `COMMANDS`, so this row
    /// reaches it without a second edit. Its summary must also promise the
    /// confirmation — a command that writes config and takes a key is one users
    /// should be able to try.
    #[test]
    fn help_lists_the_provider_setup_row_and_promises_the_confirmation() {
        let mut surface = RecordingSurface::new();
        render_help(&mut surface);
        let listing = surface.lines_of(LineKind::Info).join("\n");

        assert!(listing.contains("/provider setup"), "{listing}");
        assert!(
            listing.contains("[vendor] [tier]"),
            "the row must show what it takes, or the arguments are a secret: {listing}"
        );
        assert!(
            listing.contains("confirm before anything is written"),
            "the summary must promise the confirm step: {listing}"
        );
    }

    /// Every form the spec names is a real command line: bare (AC-3), one
    /// argument, and two. The longest-match rule keeps `provider setup` apart
    /// from any future `provider` row, and a third word is a typo rather than a
    /// silently ignored argument.
    #[test]
    fn the_provider_setup_command_parses_up_to_two_arguments() {
        for (line, expected) in [
            ("/provider setup", ""),
            ("/provider setup kimi", "kimi"),
            ("/provider setup kimi think", "kimi think"),
            ("/provider  setup  kimi  think", "kimi  think"),
        ] {
            let Input::Command { name, args } = classify(line) else {
                panic!("`{line}` did not classify as a command");
            };
            assert_eq!(name, "provider setup", "{line}");
            assert_eq!(args, expected, "{line}");
            assert!(
                matches!(resolve(name, args), Resolution::Run(..)),
                "`{line}` must dispatch, not be rejected"
            );
        }

        // The handler's own split is what turns those bytes into two arguments,
        // and it rejects a third rather than guessing.
        assert!(PROVIDER_SETUP_USAGE.contains("/provider setup kimi think"));
        let mut words = "kimi think spare".split_whitespace();
        assert_eq!(words.next(), Some("kimi"));
        assert_eq!(words.next(), Some("think"));
        assert!(
            words.next().is_some(),
            "a third word must be visible to the handler"
        );
    }

    /// AC-9 / BR-11 at the dispatch level: `/provider setup` is governed by the
    /// **same** typed-input gate `/web setup` is, so a piped session degrades to
    /// printed instructions and reads no stdin. The gate is pure, so the branch a
    /// test process with a piped stdin cannot otherwise reach on purpose is
    /// pinned here; what the degraded path *renders* is pinned in
    /// `provider_setup_ui`, where the seam lives.
    #[test]
    fn provider_setup_degrades_on_a_pipe_through_the_same_gate_web_setup_uses() {
        use crate::web_setup_ui::{gate, Gate};
        assert_eq!(gate(true, false), Gate::Walk);
        // The e2e suite's allowance, and nothing else in the wild.
        assert_eq!(gate(false, true), Gate::Walk);
        // The shape that matters: piped input, no seam, no question asked.
        assert_eq!(gate(false, false), Gate::Instructions);
        // And it is a degradation, not the `/model set` refusal: nothing here
        // routes a non-typed `/provider setup` to `MODEL_SET_TYPED_ONLY`.
        assert!(COMMANDS
            .iter()
            .any(|spec| spec.name == "provider setup" && matches!(spec.args, Args::Optional)));
    }

    // ------------------------------------------------------------------
    // REQ-581: the provider connection test (AC-9)
    // ------------------------------------------------------------------

    /// AC-9 / REQ-555 BR-7: `/help` is generated from `COMMANDS`, so this row
    /// reaches it without a second edit. Its summary must say what the command
    /// *does* — one call, consented — because "test" alone reads as a local
    /// diagnostic and this one spends.
    #[test]
    fn help_lists_the_provider_test_row_and_says_it_sends() {
        let mut surface = RecordingSurface::new();
        render_help(&mut surface);
        let listing = surface.lines_of(LineKind::Info).join("\n");

        assert!(listing.contains("/provider test"), "{listing}");
        assert!(
            listing.contains("<id>"),
            "the row must show what it takes, or the argument is a secret: {listing}"
        );
        assert!(
            listing.contains("one consented call"),
            "the summary must say the command sends: {listing}"
        );
    }

    /// The longest-match rule keeps the two `/provider` rows apart, and this one
    /// takes exactly one id: a bare line is rejected before any handler runs
    /// (there is nothing useful to test without a name), and a second word is a
    /// typo rather than a silently ignored argument — for a command that spends,
    /// guessing which of two ids was meant is the one thing it must not do.
    #[test]
    fn the_provider_test_command_takes_exactly_one_id() {
        let Input::Command { name, args } = classify("/provider test kimi") else {
            panic!("`/provider test kimi` did not classify as a command");
        };
        assert_eq!((name, args), ("provider test", "kimi"));
        assert!(matches!(resolve(name, args), Resolution::Run(_, "kimi")));

        // The bare form is rejected at `resolve`, so no RPC is issued and the
        // hint says what to type.
        let Input::Command { name, args } = classify("/provider test") else {
            panic!("`/provider test` did not classify as a command");
        };
        let Resolution::Rejected(hint) = resolve(name, args) else {
            panic!("a bare `/provider test` reached the handler");
        };
        assert!(hint.contains("/provider test <id>"), "{hint}");

        // The two rows are distinct: `/provider setup kimi` must never be read
        // as the test row, nor the reverse.
        let Input::Command { name, .. } = classify("/provider setup kimi") else {
            panic!("`/provider setup kimi` did not classify as a command");
        };
        assert_eq!(name, "provider setup");

        // A second word reaches the handler, which rejects it rather than
        // guessing which id was meant.
        assert!(PROVIDER_TEST_USAGE.contains("/provider test kimi"));
        assert!(PROVIDER_TEST_USAGE.contains("Nothing was sent"));
        let mut words = "kimi deepseek".split_whitespace();
        assert_eq!(words.next(), Some("kimi"));
        assert!(
            words.next().is_some(),
            "a second word must be visible to the handler"
        );
    }

    /// The longest-match rule keeps the third `/web` row apart from the other
    /// two, and the row takes no argument — `/web setup search` is a typo, not a
    /// way to skip the menu, and answering it as one would set a tier from a
    /// line nobody previewed.
    #[test]
    fn the_setup_command_parses_and_takes_no_arguments() {
        let Input::Command { name, args } = classify("/web setup") else {
            panic!("`/web setup` did not classify as a command");
        };
        assert_eq!((name, args), ("web setup", ""));
        assert!(matches!(resolve(name, args), Resolution::Run(_, "")));

        let Input::Command { name, args } = classify("/web setup search") else {
            panic!("did not classify as a command");
        };
        let Resolution::Rejected(hint) = resolve(name, args) else {
            panic!("`/web setup search` reached the handler");
        };
        assert!(hint.contains("takes no arguments"), "{hint}");
    }

    /// Both spellings parse to their own row, and the argument rules are the
    /// ones the table declares — rejected at resolve time, before any RPC.
    #[test]
    fn the_web_commands_parse_and_police_their_arguments() {
        let Input::Command { name, args } = classify("/web allow") else {
            panic!("`/web allow` did not classify as a command");
        };
        assert_eq!((name, args), ("web allow", ""));
        assert!(matches!(resolve(name, args), Resolution::Run(_, "")));

        // `/web allow` takes nothing: a trailing word is a typo, not a tier.
        let Input::Command { name, args } = classify("/web allow search") else {
            panic!("did not classify as a command");
        };
        let Resolution::Rejected(hint) = resolve(name, args) else {
            panic!("`/web allow search` reached the handler");
        };
        assert!(hint.contains("takes no arguments"), "{hint}");

        // The URL reaches the handler verbatim — the daemon owns URL
        // normalization, so nothing here second-guesses it.
        let Input::Command { name, args } = classify("/web refresh https://docs.rs/serde") else {
            panic!("did not classify as a command");
        };
        assert_eq!((name, args), ("web refresh", "https://docs.rs/serde"));
        let Resolution::Run(spec, run_args) = resolve(name, args) else {
            panic!("a well-formed `/web refresh` was rejected");
        };
        assert_eq!(spec.name, "web refresh");
        assert_eq!(run_args, "https://docs.rs/serde");

        // A bare refresh gets the usage line, not an RPC with an empty URL.
        let Input::Command { name, args } = classify("/web refresh") else {
            panic!("did not classify as a command");
        };
        let Resolution::Rejected(hint) = resolve(name, args) else {
            panic!("a bare `/web refresh` reached the handler");
        };
        assert!(hint.contains("/web refresh"), "{hint}");
        assert!(hint.contains("<url>"), "{hint}");
        assert!(hint.contains("/help"), "{hint}");
    }

    /// The longest-match rule keeps `/web allow` and `/web refresh` apart, and
    /// leaves a bare `/web` an unknown command rather than a silent alias for
    /// one of them.
    #[test]
    fn a_bare_web_is_not_a_command() {
        let Input::Command { name, args } = classify("/web") else {
            panic!("`/web` did not classify as a command line");
        };
        let Resolution::Rejected(hint) = resolve(name, args) else {
            panic!("`/web` reached a handler");
        };
        assert!(hint.contains("unknown command"), "{hint}");
        assert!(hint.contains("/help"), "{hint}");
    }

    /// AC-12's two answers. `was_restricted` is what tells "the restriction is
    /// gone" from "there was none", and the CLI must not confirm a lift that
    /// never happened.
    #[test]
    fn web_allow_confirms_a_lift_and_says_so_when_nothing_was_restricted() {
        let lifted = render_web_override(&WebOverrideResult {
            was_restricted: true,
            tiers_restored: vec![WebTier::FetchUserUrl, WebTier::FetchAnyUrl],
        });
        assert!(lifted.contains("lifted"), "{lifted}");
        assert!(lifted.contains("fetch_user_url"), "{lifted}");
        assert!(lifted.contains("fetch_any_url"), "{lifted}");
        // BR-13: it restores, it never grants — and it does not persist.
        assert!(lifted.contains("granted no new tier"), "{lifted}");
        assert!(
            lifted.contains("fresh session starts restricted"),
            "{lifted}"
        );

        let nothing = render_web_override(&WebOverrideResult::default());
        assert!(
            nothing.contains("nothing was restricted"),
            "an unrestricted session must not be told a restriction was lifted: {nothing}"
        );
        assert!(!nothing.contains("lifted"), "{nothing}");

        // Restricted, but holding no tiers: a real lift that restores nothing.
        // Distinct from both of the above, which is the whole reason
        // `was_restricted` is a separate field.
        let empty = render_web_override(&WebOverrideResult {
            was_restricted: true,
            tiers_restored: Vec::new(),
        });
        assert!(empty.contains("lifted"), "{empty}");
        assert!(empty.contains("nothing resumed"), "{empty}");
        assert!(!empty.contains("nothing was restricted"), "{empty}");
    }

    // ---- REQ-560: /permissions ---------------------------------------------

    /// REQ-560 BR-10: bare `/permissions` reads, and the read states the level
    /// plainly — this is the surface that works on a pipe, where BR-9 hides the
    /// status row entirely.
    #[test]
    fn permissions_states_the_level_and_distinguishes_a_change_from_a_read() {
        let changed = render_permissions(&SessionPermissionsResult {
            level: PermissionLevel::Full,
            changed: true,
        });
        assert!(changed.contains("full"), "{changed}");
        assert!(
            changed.contains(PermissionLevel::Full.summary()),
            "{changed}"
        );
        assert!(!changed.contains("unchanged"), "{changed}");

        // A read, and a set that changed nothing, must not confirm a change
        // that did not happen — the honesty `was_restricted` gives `/web allow`.
        let unchanged = render_permissions(&SessionPermissionsResult {
            level: PermissionLevel::Guarded,
            changed: false,
        });
        assert!(unchanged.contains("guarded"), "{unchanged}");
        assert!(unchanged.contains("unchanged"), "{unchanged}");
    }

    /// Every level is renderable, driven off `ALL` so a fifth one is covered the
    /// moment it exists (REQ-560 AC-17).
    #[test]
    fn every_level_renders_a_line_naming_itself() {
        for level in PermissionLevel::ALL {
            let line = render_permissions(&SessionPermissionsResult {
                level: *level,
                changed: true,
            });
            assert!(line.contains(level.name()), "{line}");
            assert!(line.contains(level.summary()), "{line}");

            let option = render_level_option(*level);
            assert!(option.contains(level.name()), "{option}");
            assert!(option.contains(level.summary()), "{option}");
        }
    }

    /// An unrecognised level is a typo, and a typo must not change the session's
    /// posture on the way to being reported.
    ///
    /// The echo goes through the same bounding and control-character stripping an
    /// unknown *command* name does, because the argument is equally arbitrary
    /// bytes reaching a `Surface`.
    #[test]
    fn an_unknown_level_is_quoted_safely_and_never_guessed_at() {
        let line = unknown_level_line("gaurded");
        assert!(line.contains("gaurded"), "{line}");
        assert!(
            line.contains("not changed"),
            "the user must be told the session is untouched: {line}"
        );
        // No nearest-match guess: `gaurded` names nothing, and the argument
        // decides whether shell commands run without asking.
        assert!(!line.contains("guarded — reads"), "{line}");

        // Control characters cannot reach the surface, and a long paste is
        // quoted rather than replayed.
        let nasty = unknown_level_line("full\u{1b}[31m\u{7}");
        assert!(!nasty.contains('\u{1b}'), "{nasty}");
        assert!(!nasty.contains('\u{7}'), "{nasty}");
        let long = unknown_level_line(&"x".repeat(ECHO_MAX_CHARS * 3));
        assert!(long.contains('…'), "a long argument must be elided: {long}");
    }

    /// REQ-560: `/permissions` is reachable in **both** forms, and neither is
    /// rejected at resolve time — the bare form is BR-10's read path, so
    /// answering it with "needs an argument" would refuse the requirement.
    #[test]
    fn both_forms_of_permissions_dispatch() {
        for (typed, expected_args) in [
            ("/permissions", ""),
            ("/permissions edits", "edits"),
            // An unknown level still dispatches: the handler rejects it, after
            // deciding not to send anything.
            ("/permissions bogus", "bogus"),
        ] {
            let Input::Command { name, args } = classify(typed) else {
                panic!("`{typed}` did not classify as a command");
            };
            assert_eq!(name, "permissions");
            let Resolution::Run(spec, run_args) = resolve(name, args) else {
                panic!("`{typed}` must dispatch, not be rejected at resolve time");
            };
            assert_eq!(spec.name, "permissions");
            assert_eq!(run_args, expected_args);
        }
    }

    /// REQ-560 BR-14, the fence: `/effort` and `/permissions` are **one row
    /// each**, owned by REQ-559 and REQ-560 respectively.
    ///
    /// Written as a uniqueness claim rather than an absence one, and that
    /// framing is the point. While REQ-559 was unlanded this could assert
    /// "`/effort` does not appear" — but that phrasing expires the moment the
    /// other REQ merges, and an expired fence either fails for the wrong reason
    /// or gets deleted. What BR-14 actually forbids is a *second* row: "a second
    /// name for it is an alias on the same row, never a second row." So count.
    ///
    /// This also covers the direction the concurrent development made likely —
    /// a rebase resolving two `COMMANDS` edits by keeping both copies of one
    /// row, which is the BUG-153 shape (`/help` listing a command twice, two
    /// handlers to drift apart) and which no compiler would catch.
    #[test]
    fn effort_and_permissions_are_one_row_each() {
        for owned in ["effort", "permissions"] {
            let rows = COMMANDS.iter().filter(|s| s.name == owned).count();
            assert_eq!(
                rows, 1,
                "`/{owned}` must be exactly one row in COMMANDS, found {rows}"
            );
            // …and no *other* row may reach it by alias, which would be a second
            // spelling with a second handler behind it.
            let spellings = COMMANDS
                .iter()
                .flat_map(CommandSpec::spellings)
                .filter(|sp| *sp == owned)
                .count();
            assert_eq!(
                spellings, 1,
                "`/{owned}` is reachable by {spellings} spellings; BR-14 allows one row \
                 and aliases only on that same row"
            );
        }
    }

    /// A daemon too old to serve `session/permissions` is a **notice**, not an
    /// `error:` line — a version fact is not a failure (BUG-152), the same
    /// treatment the web methods get.
    #[test]
    fn a_daemon_without_session_permissions_is_a_notice_not_an_error() {
        let mut surface = RecordingSurface::new();
        assert!(permissions_or_report(
            Err(RpcError::new(
                error_code::METHOD_NOT_FOUND,
                "no such method"
            )),
            &mut surface
        )
        .is_none());
        assert!(surface.lines_of(LineKind::Error).is_empty());
        assert!(surface
            .lines_of(LineKind::Notice)
            .join("\n")
            .contains("does not serve permission levels"));
    }

    /// `/web allow` never fabricates a session id: with no session there is no
    /// request to build, so nothing is sent.
    #[test]
    fn web_allow_without_a_session_builds_no_request() {
        assert!(
            web_override_params(None).is_none(),
            "a command with no session must not invent one to act on"
        );
        assert!(WEB_NEEDS_A_SESSION.contains("needs a session"));

        let named = web_override_params(Some(SessionId::from("sess-under-test")))
            .expect("a session is all this request needs");
        assert_eq!(named.session_id, SessionId::from("sess-under-test"));
    }

    /// Both web commands report a daemon too old to know them as a **notice**,
    /// not an `error:` line — a version fact is not a failure (BUG-152).
    #[test]
    fn a_daemon_without_the_web_methods_is_a_notice_not_an_error() {
        let too_old = || RpcError::new(error_code::METHOD_NOT_FOUND, "no such method");

        let mut surface = RecordingSurface::new();
        assert!(web_override_or_report(Err(too_old()), &mut surface).is_none());
        assert!(web_refresh_or_report(Err(too_old()), &mut surface).is_none());
        assert_eq!(
            surface.lines_of(LineKind::Notice).len(),
            2,
            "both commands report it, and both as notices: {:?}",
            surface.calls
        );
        assert!(
            surface.lines_of(LineKind::Error).is_empty(),
            "a build without a method is not a failure: {:?}",
            surface.calls
        );

        // Any other failure keeps the error line.
        let mut surface = RecordingSurface::new();
        assert!(web_override_or_report(
            Err(RpcError::new(error_code::INTERNAL_ERROR, "boom")),
            &mut surface
        )
        .is_none());
        assert!(web_refresh_or_report(
            Err(RpcError::new(error_code::INTERNAL_ERROR, "boom")),
            &mut surface
        )
        .is_none());
        assert_eq!(surface.lines_of(LineKind::Error).len(), 2);
    }

    /// AC-10: the two refresh answers are different sentences, and neither is an
    /// error — "there was never a copy" is a fact about why the next fetch is
    /// live, not a failure.
    #[test]
    fn the_two_refresh_outcomes_read_differently() {
        assert_ne!(WEB_REFRESH_EVICTED, WEB_REFRESH_ABSENT);
        assert!(WEB_REFRESH_EVICTED.contains("re-fetches"));
        assert!(WEB_REFRESH_ABSENT.contains("nothing was stored"));
    }

    /// `/clear` never fabricates a session id either: with no session there is
    /// nothing to clear, so nothing is sent.
    #[test]
    fn clear_without_a_session_builds_no_request() {
        assert!(
            session_clear_params(None).is_none(),
            "a command with no session must not invent one to clear"
        );
        assert!(CLEAR_NEEDS_A_SESSION.contains("needs a session"));

        let named = session_clear_params(Some(SessionId::from("sess-under-test")))
            .expect("a session is all this request needs");
        assert_eq!(named.session_id, SessionId::from("sess-under-test"));
    }

    /// **REQ-567 BR-8, the render decision.** A `/clear` that worked prints
    /// nothing here — the `context_cleared` event is the one line every attached
    /// client draws, the issuer included — so this pins the arms that *do*
    /// render, and their line classes.
    ///
    /// The busy arm is the one that matters: a session already running a turn is
    /// transient, resolves by itself, and needs no fixing, so it is a **notice**
    /// (BUG-152). Shipping it as an `error:` line would tell a user who typed
    /// `/clear` a second too early that something had gone wrong.
    ///
    /// Matched on the daemon's code rather than on its sentence, so the daemon's
    /// wording can change without silently reclassifying the line (LESSON-456);
    /// the sentence is passed through because it names the turn holding the
    /// session and says to retry, which no string written here could.
    #[test]
    fn a_busy_clear_is_a_notice_and_a_real_failure_is_an_error() {
        let mut surface = RecordingSurface::new();
        report_clear_refusal(
            &RpcError::new(
                error_code::SESSION_BUSY,
                "session sess-under-test is already running turn turn-2; one session runs one turn at a \
                 time — retry when it finishes",
            ),
            &mut surface,
        );
        let busy = surface.lines_of(LineKind::Notice).join("\n");
        assert!(
            busy.contains("nothing was cleared"),
            "a refused clear must say the conversation is still there: {busy}"
        );
        assert!(
            busy.contains("turn-2"),
            "the daemon's sentence names the turn holding the session: {busy}"
        );
        assert!(
            surface.lines_of(LineKind::Error).is_empty(),
            "a busy session is not a failure (BUG-152): {:?}",
            surface.calls
        );

        // A daemon built before REQ-567 retains nothing across prompts, so
        // "there is nothing to clear" is the true thing to say — and it is a
        // version fact, not a failure.
        let mut surface = RecordingSurface::new();
        report_clear_refusal(
            &RpcError::new(error_code::METHOD_NOT_FOUND, "no such method"),
            &mut surface,
        );
        assert_eq!(surface.lines_of(LineKind::Notice).len(), 1);
        assert!(
            surface.lines_of(LineKind::Error).is_empty(),
            "a build without the method is not a failure: {:?}",
            surface.calls
        );

        // Everything else keeps the error line, carrying the daemon's reason.
        let mut surface = RecordingSurface::new();
        report_clear_refusal(
            &RpcError::new(error_code::INTERNAL_ERROR, "boom"),
            &mut surface,
        );
        let failed = surface.lines_of(LineKind::Error).join("\n");
        assert!(failed.contains("could not be cleared"), "{failed}");
        assert!(failed.contains("boom"), "{failed}");
        assert!(
            surface.lines_of(LineKind::Notice).is_empty(),
            "a real failure must not be softened into a notice: {:?}",
            surface.calls
        );
    }
}
