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
//! Ten rows are **mirrors** of `teton` subcommands (REQ-582 BR-1): `/provider
//! list`, `/provider add`, `/boundary list`, `/boundary add`, `/policy show`,
//! `/policy set-tier`, `/policy set-category`, `/model list`, `/model status`
//! and `/doctor`. They carry [`Args::Cli`] and a [`Mirror`], and their handlers
//! live in [`crate::cli_rows`], which parses their arguments with the binary's
//! own clap tree and runs the same `<sub>_on(conn, ctx, …)` body the subcommand
//! runs — the pattern above, extended to the namespaces REQ-555 deferred. A
//! mirrored row's name *is* its twin's subcommand path, which is what lets a
//! typed `teton provider list` be recognized by walking clap's tree rather than
//! by a second matcher (ADR-1).
//!
//! That recognition is [`cli_line`], and it is why [`Input`] has six variants
//! rather than three (REQ-582 BR-4; a sixth since the verify pass): a line
//! whose first word is `teton` and whose following words name a subcommand path
//! runs that path's row ([`Input::CliLine`]) after one notice naming the `/`
//! spelling, a path with no session form — `teton uninstall`, a family typed
//! bare — is one refusing line ([`Input::CliRefused`]), and a family followed by
//! `--help` is the parser's own page for it ([`Input::CliHelp`]). None of them
//! ever reaches the model, and none spawns a `teton` process or opens a second
//! connection: a recognized line dispatches through this table, on the
//! session's own connection, so BR-5 holds by construction rather than by a
//! check (BUG-177's shape is what a subprocess would cost). Everything else
//! that opens with `teton` is a question about the product — "teton is slow
//! today" — and reaches the model byte-identically to today.
//!
//! A recognized line runs through [`run_cli_line`] rather than straight through
//! [`dispatch`]: the ten mirrored rows parse their own argument with clap, but
//! the rows that predate this REQ (`/cost`, `/effort`, `/model set`, `/provider
//! test`) read a plain string, so their typed argv is validated whole by the
//! binary's parser first and the row is handed what the parser derived — never
//! `qwen --yes` as a model name (verify M2).
//!
//! This module and [`crate::session_ui`] depend on each other on purpose (verify
//! m17): the hand-off nudge reads [`mirrored_rows`] so the spellings it names
//! are the table's, and this module renders `session_ui`'s tier vocabulary. The
//! alternative — a third module owning the row table — would move the one list
//! that dispatches away from the handlers it dispatches to, for the sake of a
//! dependency graph no reader of either file has to hold in their head. The
//! same holds one module over: [`crate::cli_rows`] is the handler module for
//! the mirrored rows (the table names its handlers, and it reads the table's
//! [`rows_under`], [`HELP_HINT`] and [`test_seams_allowed`] to compose its
//! refusals), so the slash↔cli_rows dependency is deliberate for the same
//! reason.
//!
//! Five commands are deliberately narrower than BR-9's "identical on a TTY and
//! on piped stdin": the four mirrored rows that write and `/model set` are
//! **refused when the session's stdin is not a terminal** (spec Permissions;
//! security review 2026-08-04; REQ-582 ADR-4). They are the commands that change
//! daemon or machine state, so on a pipe each renders one rejection pointing at
//! its `teton` twin and sends nothing. That is what is enforced, and it is
//! narrower than what it is enforced *for*: the check separates a pipe from a
//! pty, not a machine from a human — `expect(1)`, a tmux `send-keys` and a
//! pasted line all present a terminal and pass. The spec records that residual
//! and names the shell commands as the auditable surface for the unattended
//! case. Every other command is pipe-friendly exactly as BR-9 says.

// REQ-582 verify M2: `run_cli_line` validates a pre-REQ row's typed argv with
// the binary's own parser before the row runs.
use clap::Parser;
use teton_core::session_root::{resolve_cwd_argument, CwdArgError};
use teton_protocol::effort::EffortLevel;
use teton_protocol::jsonrpc::{error_code, RpcError};
use teton_protocol::methods::ProjectsListParams;
use teton_protocol::methods::{
    ConfigGetParams, ConfigSetParams, ConfigUpdate, ModelStatusParams, PromptBlock,
    PromptTurnParams, SessionClearParams, SessionPermissionsParams, SessionPermissionsResult,
    SessionRoot, SessionSetCwdParams, SkillSkipped, SkillSource, SkillView, SkillsListResult,
    WebOverrideParams, WebOverrideResult, WebRefreshOutcome, WebRefreshParams, WebRefreshResult,
};
use teton_protocol::methods::{
    ContextAction, RepoContextStateKind, SessionContextParams, SessionContextResult,
};
use teton_protocol::methods::{SessionTranscriptParams, SessionTranscriptResult, TranscriptAction};
use teton_protocol::permissions::PermissionLevel;
use teton_protocol::SessionId;

use crate::banner;
use crate::cli_rows::{self, Mirror, WriteGate};
use crate::client::{Connection, UiContext};
use crate::model_ui;
use crate::render::{LineKind, Surface};
use crate::session_ui::web_tier_name;

/// The one line `/help` prints about the `//` escape hatch (BR-1b).
const ESCAPE_FOOTER: &str =
    "//text sends text as a prompt with one leading slash — //usr/bin/foo asks about /usr/bin/foo.";

/// The one line `/help` prints about how a row's arguments are read (REQ-582
/// ADR-2 / OQ-5).
///
/// The mirrored rows take real CLI arguments — positionals, `--flags`, value
/// enums — and this is the one place the session's tokenization differs from a
/// shell's: it splits on whitespace and interprets no quotes. No mirrored
/// subcommand takes a whitespace-bearing value today, so the limitation costs
/// nothing in practice; it is documented rather than hidden because the shape it
/// forbids (a glob with a space) is legal in principle and the shell twin does
/// accept it.
///
/// REQ-585 ADR-12 **qualifies** it: a skill row takes its line as typed (BR-4),
/// so the sentence is true of the built-in rows and false of the section below
/// them. The qualification is *appended* rather than folded into the subject —
/// `cli_e2e` pins the original clause as a substring, and rewriting the opening
/// into "Built-in command arguments…" would move a byte a test elsewhere reads.
/// One sentence of scope, added where the reader already is.
const ARGUMENT_FOOTER: &str =
    "Command arguments are split on whitespace and quotes are not interpreted — a value with a \
     space in it has to be given to `teton` in a shell. That is how the built-in rows above read \
     an argument; a skill row is handed its line as typed.";

/// The tail every rejected command line carries, so an unknown command and a
/// misused one point at the same place (BR-2).
///
/// `pub(crate)` since REQ-582: a refused `teton …` line ends in the same
/// pointer, and it is composed in [`cli_rows`] because the *reason* it carries
/// is read off clap's tree. One pointer sentence, one place to change it.
pub(crate) const HELP_HINT: &str = "type /help for the commands this session knows.";

/// The binary's own name: the first token that makes a typed line a candidate
/// `teton …` command (REQ-582 BR-4).
const TETON: &str = "teton";

/// The binary's own flags — the tokens that name no subcommand and yet are
/// plainly not a question (ADR-1).
///
/// Deliberately only the flags. `teton help me read this backtrace` opens with a
/// word that *is* a subcommand at runtime (clap generates one) and is also an
/// ordinary English sentence, so refusing on it would take a legitimate prompt
/// away from the model — the failure BR-4's "teton is slow today" clause exists
/// to prevent. No sentence opens `--help`.
const CLI_FLAGS: &[&str] = &["--help", "-h", "--version", "-V"];

/// The two flags a `teton …` line may carry **before** its subcommand and still
/// be that subcommand (verify m5).
///
/// `teton -y policy set-tier build kimi` and `teton --verbose doctor` are the
/// shell's own spellings — both flags are `global`, so clap accepts them ahead
/// of the subcommand — and a walk that started at `-y` found no subcommand and
/// sent the line to the model as a question. The classifier steps over them and
/// carries them on the recognized line ([`Input::CliLine::shell_flags`]) so the
/// row's own parse still sees them and says they were ignored. Exact spellings
/// only: a combined short form (`-yv`) is not one anybody types at a prompt, and
/// widening this to "anything starting with `-`" would turn `teton -whatever`
/// into a refusal rather than the prompt it is.
const LEADING_GLOBAL_FLAGS: &[&str] = &["-y", "--yes", "-v", "--verbose"];

/// What a bare `teton` typed at the prompt gets back (BR-4).
///
/// It is the line that opens a session, and the person typing it is in one. The
/// answer says that rather than "unknown command", because the user did not
/// mistype anything — they asked for something they already have.
const ALREADY_IN_A_SESSION: &str =
    "`teton` on its own opens a session, and you are already in one — type /help for the commands \
     it knows.";

/// What `teton --help` / `teton --version` typed at the prompt get back (BR-4).
///
/// Both are answerable — one by `/help`, one by a shell — and neither is a
/// question for the model, which is the whole of why they are intercepted: the
/// harness was asked and something else answering is the BUG-146 shape.
const CLI_FLAGS_ARE_SHELL_ONLY: &str =
    "`teton --help` and `teton --version` are the binary's own flags, and this session is already \
     running — type /help for the commands it knows, or run `teton --version` from a shell.";

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
    /// A typed `teton …` line whose subcommand path names a table row (REQ-582
    /// BR-4, ADR-1): the row's name, and the text after the path words.
    ///
    /// Carried apart from [`Self::Command`] because the entry loop owes it one
    /// extra line — the notice naming the `/` spelling that just ran — and
    /// because "how a line was typed" is the classifier's fact to report rather
    /// than something the loop re-derives.
    CliLine {
        /// The row name, which is also the subcommand path clap walked to.
        name: &'a str,
        /// Everything after the path words, trimmed; the row's own grammar
        /// judges it (BR-3).
        args: &'a str,
        /// The binary's global flags typed **before** the path (`teton -y policy
        /// set-tier …`), as they were typed, or `""` (verify m5).
        ///
        /// Carried rather than dropped so the row's own parse still meets them:
        /// they are legal argv the shell accepts, and the row reports them as
        /// ignored ([`cli_rows::shell_flags_line`]) instead of the classifier
        /// silently eating a `--yes` the user meant. Kept apart from `args`
        /// because the two are not adjacent in the line — the path sits between
        /// them — and a classifier that borrows cannot splice.
        shell_flags: &'a str,
    },
    /// A typed `teton …` line naming a real command with no session form
    /// (REQ-582 BR-4): the one line saying why, and where to go instead.
    ///
    /// Owned rather than borrowed because the reason is *composed* — from
    /// [`cli_rows::SHELL_ONLY`], or from the subcommands clap's tree lists under
    /// the family that was typed bare — and a classifier that returned a
    /// `&'static str` here could only carry reasons written in advance.
    CliRefused(String),
    /// A typed `teton <family> --help` (verify T6): clap's own help page for that
    /// family, ready to render line by line as information.
    ///
    /// A variant of its own because it is neither a row to run nor a refusal to
    /// print — a user who asked for help got what they asked for, and no line
    /// of a help page is an error. It is the one CLI outcome that renders more
    /// than a line, which is also why it cannot ride [`Self::CliRefused`].
    CliHelp(String),
    /// A `/` line whose first token names a registered, **unshadowed** skill
    /// (REQ-585 BR-10, ADR-13): the registry's spelling of the name, and the
    /// rest of the line as typed.
    ///
    /// Owned, and deliberately so. The registry is a snapshot the session
    /// re-fetches when its root moves (`/cd`), so a borrow taken from it would
    /// have to outlive the fetch that replaces it — and the only ways to give a
    /// borrowed name the `'static` the table's rows have are to leak the
    /// registry or to intern the name, both of which would keep dispatching a
    /// skill the session no longer has. Two `String`s per invocation, once per
    /// typed line, is the price of a snapshot that can be dropped.
    ///
    /// `raw_arguments` is the line's own bytes after the name, trimmed at the
    /// edges and untouched inside: interior whitespace runs and quotes survive,
    /// because BR-4 hands them to the skill body rather than tokenizing them
    /// (which is what [`ARGUMENT_FOOTER`]'s qualification tells the user).
    Skill {
        /// The dispatchable name, as the registry spells it.
        name: String,
        /// Everything after the name, as typed.
        raw_arguments: String,
    },
    /// A prompt that opened with the `//` escape, with exactly the leading pair
    /// collapsed to one `/` (BR-1b).
    EscapedPrompt(&'a str),
    /// A plain prompt — the input line's own bytes, untouched.
    Prompt(&'a str),
}

/// What this session's skill registry looks like from the client (REQ-585
/// ADR-1/ADR-2).
///
/// The daemon owns the registry — it reads the files, decides the name
/// contests, and bounds every field that came out of one — and this is the whole
/// of what crosses to the client: enough to classify a `/name` line and to print
/// a `/help` row. No body, no absolute path, no second reader of `~/.claude`.
///
/// It is a **snapshot**, refreshed after `session/create` and again after every
/// `session_root_changed` (TASK-207), which is why nothing derived from it is
/// ever `'static`: a `/cd` replaces it, and a name that outlived its snapshot
/// would dispatch a skill the session no longer has.
///
/// The default is empty, and empty is a load-bearing state rather than a
/// degenerate one: a user with no `~/.claude` has one, and so does a client
/// talking to a daemon that answers `skills/list` with `METHOD_NOT_FOUND`
/// (ADR-2). An empty snapshot makes [`classify`] incapable of returning
/// [`Input::Skill`] and renders no `/help` section at all, which is what makes
/// "byte-for-byte what it is today" true for both of them.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SkillSnapshot {
    /// Every registered skill, shadowed ones included — `/help` lists them and
    /// [`classify`] refuses them, and BR-3 needs both to be reading one list.
    /// Ordered by the daemon (LESSON-540: a client-side sort would be a
    /// platform-flaky `/help`).
    skills: Vec<SkillView>,
    /// Everything discovery found and did not register, with why. Feeds
    /// `/help`'s diagnostic line and BR-10's unknown-command hint.
    skipped: Vec<SkillSkipped>,
}

impl SkillSnapshot {
    /// The snapshot every session starts with, and the one an old daemon
    /// leaves it with (ADR-2).
    #[must_use]
    pub const fn empty() -> Self {
        Self {
            skills: Vec::new(),
            skipped: Vec::new(),
        }
    }

    /// Nothing was found: no skills, and nothing skipped either.
    ///
    /// The predicate `/help` reads (ADR-12) — a section announcing `0 skills`
    /// to the majority of users who have no `~/.claude` would be the line that
    /// makes "`/help` is unchanged" false for them.
    fn is_empty(&self) -> bool {
        self.skills.is_empty() && self.skipped.is_empty()
    }

    /// The skill a `/name` line reaches, or `None` — the **only** way a skill
    /// is looked up (BR-2, REQ-587 BR-3).
    ///
    /// A row the user may not type is listed and never returned, whichever of
    /// the two reasons applies: something else owns the spelling, or the file
    /// said `user-invocable: false`. [`user_dispatch`] is the one predicate that
    /// answers the user's question, and asking it here is what keeps `/help`'s
    /// mark and the dispatcher from disagreeing — a row `/help` marks is a row
    /// this function declines, from the same call.
    fn dispatchable(&self, name: &str) -> Option<&SkillView> {
        self.skills
            .iter()
            .find(|view| view.name == name && user_dispatch(view) == UserDispatch::Allowed)
    }

    /// The registered row of this name, marked or not (REQ-587 BR-3).
    ///
    /// [`Self::dispatchable`] answers "may the user type this?"; this answers
    /// "does the session have a row spelled this at all?", which is the question
    /// [`model_only_hint`] has to ask in order to say *why* a listed name did
    /// not run.
    fn listed(&self, name: &str) -> Option<&SkillView> {
        self.skills.iter().find(|view| view.name == name)
    }

    /// The skipped entry a typed name would have been, or `None` (AC-17).
    ///
    /// Matches the **carried** `name`, not one re-derived from the path.
    /// BR-2's naming rule (directory for `skills/`, stem for `commands/`)
    /// belongs to discovery, which is the only place that knows which of the
    /// four roots an entry came from; a copy here would be a second home for
    /// it in the crate that cannot see them (LESSON-546). It is also strictly
    /// weaker — a symlinked `commands/<name>.md` is refused before it is ever
    /// a file that was opened, and its path alone cannot give the name back.
    fn skipped_named(&self, name: &str) -> Option<&SkillSkipped> {
        self.skipped
            .iter()
            // Never on an empty name: a root-level diagnostic (an unreadable
            // directory, a truncated listing) names no skill and carries the
            // empty string, which would otherwise match a bare `/` and answer
            // for a line nobody meant to type.
            .find(|entry| !entry.name.is_empty() && entry.name == name)
    }

    /// How many registered skills came from each root, as `/help`'s diagnostic
    /// line counts them: `(user, project)`.
    fn source_counts(&self) -> (usize, usize) {
        let user = self
            .skills
            .iter()
            .filter(|view| view.source == SkillSource::User)
            .count();
        (user, self.skills.len() - user)
    }
}

/// A snapshot is built from the wire result and from nothing else — there is no
/// second constructor a test could reach that production does not (LESSON-544).
impl From<SkillsListResult> for SkillSnapshot {
    fn from(result: SkillsListResult) -> Self {
        Self {
            skills: result.skills,
            skipped: result.skipped,
        }
    }
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
    /// The argument grammar is the shell twin's own clap definition (REQ-582
    /// BR-3): positionals, `--flags` and value enums, parsed by the very code
    /// the binary parses `teton …` with.
    ///
    /// Like [`Args::Optional`] this never rejects at [`resolve`] time, and for a
    /// stronger reason: there is nothing useful this table could say about a
    /// mirrored row's argument that clap does not say better. A missing
    /// positional, an unknown flag and a value outside an enum are all reported
    /// by the parser, in the parser's own words — which is the whole of AC-7,
    /// and the reason there is no second hand-written parser of `teton …`
    /// arguments anywhere in the client (LESSON-529).
    Cli,
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
    /// The `teton …` command this row mirrors, or `None` for a session-only row
    /// (REQ-582 BR-1).
    ///
    /// It is what the typed-input refusal points at, what the hand-off nudge
    /// translates *from* (BR-8), and — because it is `"teton "` + [`Self::name`]
    /// — what makes a typed `teton provider list` resolvable to this row by
    /// walking clap's tree rather than by matching words (ADR-1).
    ///
    /// `None` on `/help`, `/clear`, `/cd`, `/verbose`, `/permissions`, `/web …`,
    /// `/provider setup`, `/provider test`, `/quit` — and on `/cost`, `/effort`
    /// and `/model set`, which do have `teton` twins but whose session rows
    /// predate this REQ and carry gates and flows of their own (`/model set`'s
    /// above-RAM-floor confirmation, `/provider test`'s consent). A `Some` here
    /// means "this row *is* its twin, parsed and rendered by the twin's code".
    mirror: Option<Mirror>,
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
        mirror: None,
        handler: handle_help,
    },
    CommandSpec {
        name: "cost",
        aliases: &[],
        summary: "Show the daemon's cost report, exactly as `teton cost` does.",
        args: Args::None,
        mirror: None,
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
        mirror: None,
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
        mirror: None,
        handler: handle_model,
    },
    CommandSpec {
        name: "model set",
        aliases: &[],
        summary: "Switch the local tier to a catalog model: /model set <name>.",
        args: Args::Required(
            "a catalog name — `/model set <name>`, and `teton model list` names them",
        ),
        mirror: None,
        handler: handle_model_set,
    },
    // REQ-582 BR-1: the two `teton model` reads the session had no form for.
    // They sit with the rows above because `/help` groups by family (first
    // word), and because the four together are the whole of what a user can ask
    // or say about the local model from here: `/model` is the one-line answer
    // REQ-555 chose deliberately (OQ-3), `/model list` is the catalog, `/model
    // status` is the full report, and `/model set` is the only one that writes.
    CommandSpec {
        name: "model list",
        aliases: &[],
        summary: "Show the model catalog, each entry's fit for this machine, and the selection.",
        args: Args::Cli,
        mirror: Some(cli_rows::MODEL_LIST),
        handler: cli_rows::handle_model_list,
    },
    CommandSpec {
        name: "model status",
        aliases: &[],
        summary: "Report the recorded model decision and the weights' install state.",
        args: Args::Cli,
        mirror: Some(cli_rows::MODEL_STATUS),
        handler: cli_rows::handle_model_status,
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
        mirror: None,
        handler: handle_clear,
    },
    // REQ-583's move of a live session's root (BR-7). Beside `/clear` because it
    // *is* a clear with a destination: every carried block's provenance identity
    // is relative to the root it was minted under, so the conversation is
    // dropped and the daemon reports it in `/clear`'s own shape. Session-only —
    // its shell twin is `teton --cwd <path>` at launch, a flag rather than a
    // subcommand, so there is no `Mirror` and a typed `teton cd` is a prompt.
    // The bare form is a read (the current root), which is what `Args::Optional`
    // is for (`/effort`, `/permissions`).
    CommandSpec {
        name: "cd",
        aliases: &[],
        summary: "Move this session's root — the directory tools are scoped to; clears the \
                  conversation. Bare form prints the current root.",
        args: Args::Optional,
        mirror: None,
        handler: handle_cd,
    },
    // REQ-584 BR-9. `Args::Optional` for `/cd`'s reason: the bare form is the
    // whole list, the argument filters it.
    CommandSpec {
        name: "projects",
        aliases: &[],
        summary: "List the projects this machine knows about, newest first, each with the \
                  `/cd` that moves there. Optional argument filters by name.",
        args: Args::Optional,
        mirror: None,
        handler: handle_projects,
    },
    CommandSpec {
        name: "verbose",
        aliases: &[],
        summary: "Toggle the routing and turn-end notices for this session.",
        args: Args::None,
        mirror: None,
        handler: handle_verbose,
    },
    // REQ-611's session-lifetime transcript switch. Beside `/verbose` and
    // `/permissions` because it, too, is about *this session* and is never
    // written to disk; the durable default is `teton transcript enable`.
    CommandSpec {
        name: "transcript",
        aliases: &[],
        summary:
            "Record this session to a file, or stop: /transcript [on|off]; bare, show the state.",
        args: Args::Optional,
        mirror: None,
        handler: handle_transcript,
    },
    // REQ-612's session-lifetime repository-notes switch, beside `/transcript`
    // for the reason `/transcript` sits beside `/verbose`: it is about *this
    // session* and is never written to disk. The durable default is
    // `teton context enable` (ADR-6's two lifetimes).
    //
    // `context` is a unique first word, so [`help_family`] gives it the empty
    // family `/transcript` already has and `/help`'s built-in section gains one
    // row and no new blank line.
    CommandSpec {
        name: "context",
        aliases: &[],
        summary: "Carry this repository's notes in the prompt, or stop: /context [on|off]; \
                  bare, show the state.",
        args: Args::Optional,
        mirror: None,
        handler: handle_context,
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
        mirror: None,
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
        mirror: None,
        handler: handle_web_setup,
    },
    CommandSpec {
        name: "web allow",
        aliases: &[],
        summary: "Lift this session's web taint restriction (grants no new tier).",
        args: Args::None,
        mirror: None,
        handler: handle_web_allow,
    },
    CommandSpec {
        name: "web refresh",
        aliases: &[],
        summary: "Drop a URL's cached copy so the next lookup re-fetches: /web refresh <url>.",
        args: Args::Required("a URL — `/web refresh <url>`"),
        mirror: None,
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
        mirror: None,
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
        mirror: None,
        handler: handle_provider_test,
    },
    // REQ-582 BR-1: the two `teton provider` commands the session had no form
    // for, beside the two it already had. `/provider setup` stays the guided
    // answer the live A/B settled on (REQ-579 ADR-9) and this is the by-hand
    // one — every flag its shell twin takes, none of them a key: the credential
    // is read echo-off through the session's prompter (BR-6), so
    // `/provider add … --key` is not a thing and never will be.
    CommandSpec {
        name: "provider list",
        aliases: &[],
        summary: "List the providers registered on this machine, with what each one calls.",
        args: Args::Cli,
        mirror: Some(cli_rows::PROVIDER_LIST),
        handler: cli_rows::handle_provider_list,
    },
    CommandSpec {
        name: "provider add",
        aliases: &[],
        summary: "Register a provider by hand: /provider add <id> --kind <kind> --endpoint <url> \
                  --model <name> [--max-context <tokens>]; the key is asked for, never typed on \
                  the line.",
        args: Args::Cli,
        mirror: Some(cli_rows::PROVIDER_ADD),
        handler: cli_rows::handle_provider_add,
    },
    // REQ-582 BR-1: the `/boundary` family, promoted from shell-only. REQ-555
    // deferred exactly this namespace ("in-session management commands
    // (`/provider`, `/boundary`, `/policy`) … follow the same shared-flow
    // pattern if promoted later"), and this is that promotion, on that pattern.
    CommandSpec {
        name: "boundary list",
        aliases: &[],
        summary: "List the privacy boundaries: the path globs whose content never leaves this \
                  machine.",
        args: Args::Cli,
        mirror: Some(cli_rows::BOUNDARY_LIST),
        handler: cli_rows::handle_boundary_list,
    },
    CommandSpec {
        name: "boundary add",
        aliases: &[],
        summary: "Add a privacy boundary over a path glob: /boundary add <glob> [--mode \
                  local-only|redact-then-remote].",
        args: Args::Cli,
        mirror: Some(cli_rows::BOUNDARY_ADD),
        handler: cli_rows::handle_boundary_add,
    },
    // REQ-582 BR-1: the `/policy` family. `show` leads it because it is the
    // command a user reaches for first — the question "where does this session
    // send my turns?" is the one the routing table answers — and the two set
    // rows follow in the order the CLI documents them: a tier binding is the
    // setting most users want, a category override is the exception to it.
    CommandSpec {
        name: "policy show",
        aliases: &[],
        summary: "Show the effective routing table: every tier, every category, and where each \
                  one resolves right now.",
        args: Args::Cli,
        mirror: Some(cli_rows::POLICY_SHOW),
        handler: cli_rows::handle_policy_show,
    },
    CommandSpec {
        name: "policy set-tier",
        aliases: &[],
        summary: "Route a tier to a provider: /policy set-tier <tier> <provider> [--fallback \
                  <id>].",
        args: Args::Cli,
        mirror: Some(cli_rows::POLICY_SET_TIER),
        handler: cli_rows::handle_policy_set_tier,
    },
    CommandSpec {
        name: "policy set-category",
        aliases: &[],
        summary: "Route one category ahead of its tier: /policy set-category <category> \
                  <provider> [--fallback <id>].",
        args: Args::Cli,
        mirror: Some(cli_rows::POLICY_SET_CATEGORY),
        handler: cli_rows::handle_policy_set_category,
    },
    // REQ-582 BR-1 / BR-7: the diagnosis, over the connection this session
    // already holds. It is the last mirrored row and a family of one, so it sits
    // beside `/quit` in the ungrouped block `/help` lists last.
    CommandSpec {
        name: "doctor",
        aliases: &[],
        summary: "Diagnose the daemon, socket, model state, and providers from this session.",
        args: Args::Cli,
        mirror: Some(cli_rows::DOCTOR),
        handler: cli_rows::handle_doctor,
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
        mirror: None,
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
///
/// A line that is not a command line is offered to [`cli_line`] before it
/// becomes a prompt (REQ-582 BR-4, ADR-1): a typed `teton provider list` runs
/// the row it names instead of being answered by the model with "that one's for
/// you to run", which is the failure this REQ exists to remove. Recognition is
/// the parser's own — clap's tree decides which words are a subcommand path —
/// and everything it does not recognize reaches the model with its bytes
/// unchanged.
///
/// # The registry is consulted last, and that is the whole of BR-2
///
/// REQ-585 adds one bucket and one parameter. The order inside is `//` escape →
/// [`cli_line`] → [`split_name`] against [`COMMANDS`] → **then** `registry`
/// (ADR-13), and the order is the requirement: a built-in match *returns*
/// before the snapshot is reachable, so "reserved names always win" is a
/// property of this function's shape rather than a list somewhere that has to
/// stay in step with the table. A skill can only be reached by a name no row
/// claims — which is also why the client does not have to trust the daemon to
/// have marked `/cost` shadowed before it refuses to dispatch it.
///
/// The registry is borrowed and nothing borrowed from it is returned: the
/// output lifetime is the *input line's* (`'a`), and [`Input::Skill`] owns its
/// two strings. A snapshot is replaced on `/cd`, so a name that outlived one
/// would name a skill the session no longer has.
#[must_use]
pub fn classify<'a>(input: &'a str, registry: &SkillSnapshot) -> Input<'a> {
    let Some(rest) = input.strip_prefix('/') else {
        return cli_line(input).unwrap_or(Input::Prompt(input));
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
    // OQ-1, resolved here: `/teton provider list` is the line a user who has
    // learned "commands start with `/`" types for the command they read in a
    // shell recipe, and it names one thing unambiguously. It costs this one
    // line, because a `teton …` line is recognized by the same function either
    // way. What it does *not* do is widen BR-1: a `/teton …` line that is not a
    // recognized command falls through to the table below and is rejected as an
    // unknown command, exactly as it was — a line opening with `/` still never
    // reaches the model.
    if let Some(recognized) = cli_line(rest) {
        return recognized;
    }
    let (name, args) = split_name(rest, COMMANDS);
    // The built-in table, first and structurally (ADR-13). `split_name` has
    // already canonicalised any alias, so this is the same question `resolve`
    // asks one step later — asked here so that the `return` below happens
    // before the registry is in scope at all.
    if let Some(spec) = builtin_row(name) {
        return Input::Command {
            name: spec.name,
            args,
        };
    }
    // Only now, and only for a name nothing in the table claims (BR-2) — which
    // is a wider set than the rows: `provider` is spelled by no row, so
    // `builtin_row` above says nothing about it, and `/provider foo` would
    // reach a skill while `/provider list` stayed with the table. The daemon
    // cannot close this (it has no `COMMANDS` to read — `tetond` does not
    // depend on `teton`), so the reserved set is enforced here or nowhere.
    if table_claim(name).is_none() {
        if let Some(view) = registry.dispatchable(name) {
            return Input::Skill {
                name: view.name.clone(),
                raw_arguments: args.to_owned(),
            };
        }
    }
    // Names no row and no skill: still a command line, still rejected by
    // `resolve` with the bytes it has always used (BR-10).
    Input::Command { name, args }
}

/// The table row a classified name belongs to, or `None`.
///
/// One home for "is this spelling the table's" — [`classify`] asks it to decide
/// whether the registry is reachable at all, [`resolve`] to decide whether to
/// run or to reject, [`run_cli_line`] to find the row clap walked to, and
/// [`render_help`] to decide whether a skill row is dispatchable. Several callers
/// of one lookup, so `/help`'s shadow mark cannot claim something the
/// dispatcher does not (BR-3).
///
/// Takes the **canonical** name: [`split_name`] resolves aliases before anyone
/// looks a name up, so `exit` reaches here as `quit`.
fn builtin_row(name: &str) -> Option<&'static CommandSpec> {
    COMMANDS.iter().find(|spec| spec.name == name)
}

/// What the built-in table claims a name for, or `None` when a skill may have
/// it — BR-2's reserved set, as a lookup rather than as a set, so a caller can
/// say *why* a name is taken.
///
/// Three kinds, because BR-2 names three and they are taken for different
/// reasons:
///
/// * a **row or alias** spelled exactly this — `/cost` runs the row;
/// * a **family word**, the first word of a multi-word row. No row is spelled
///   bare `provider`, so nothing above catches it, and a skill holding the name
///   would take `/provider foo` while `/provider list` stayed with the table —
///   one spelling reaching two handlers, which is the thing REQ-555 forbids;
/// * **`teton`**, claimed by REQ-582's `cli_line` before the table is consulted
///   at all, so a skill with that name is unreachable by construction and must
///   be *seen* to be unreachable rather than silently listed.
///
/// Derived from [`COMMANDS`] and cached, never hand-listed: a hand-written copy
/// is a second home for a fact whose first home is the table, and it ships
/// green the day a row is added (LESSON-546, LESSON-456).
fn table_claim(name: &str) -> Option<TableClaim> {
    static CLAIMS: std::sync::OnceLock<std::collections::BTreeMap<&'static str, TableClaim>> =
        std::sync::OnceLock::new();
    CLAIMS
        .get_or_init(|| {
            let mut claims = std::collections::BTreeMap::new();
            for spec in COMMANDS {
                for spelling in spec.spellings() {
                    let first = first_word(spelling);
                    if first != spelling {
                        claims.entry(first).or_insert(TableClaim::Family(first));
                    }
                }
            }
            // Rows last, and overwriting: a word that is both a family word and
            // a row in its own right is the row (`/model` exists beside
            // `/model set`), and the mark should say so.
            for spec in COMMANDS {
                for spelling in spec.spellings() {
                    claims.insert(spelling, TableClaim::Row(spec));
                }
            }
            claims.insert(TETON, TableClaim::TetonCli);
            claims
        })
        .get(name)
        .copied()
}

/// What [`table_claim`] found, and the words for it.
///
/// No `PartialEq`: [`CommandSpec`] carries a function pointer and does not have
/// one, and nothing here needs to compare two claims — callers ask whether a
/// name is claimed and, if so, how to say it.
#[derive(Debug, Clone, Copy)]
enum TableClaim {
    /// A row or one of its aliases, spelled exactly.
    Row(&'static CommandSpec),
    /// The first word of one or more multi-word rows.
    Family(&'static str),
    /// REQ-582's `teton …` recognition, which runs before the table.
    TetonCli,
}

impl TableClaim {
    /// How `/help` names the claim in a shadow mark.
    fn words(self) -> String {
        match self {
            Self::Row(spec) => format!("the built-in `/{}`", spec.name),
            Self::Family(word) => format!("the `/{word}` commands"),
            Self::TetonCli => "the `teton` command line".to_owned(),
        }
    }
}

/// Sort a line whose first word may be `teton` into its CLI bucket, or `None`
/// when it is not one of ours to answer (REQ-582 BR-4 / ADR-1).
///
/// The whole decision table, in one place, in the order the ADR states it:
///
/// | the line | bucket |
/// |---|---|
/// | first word is not `teton` (`tetonx …`, `Teton …`, `teton-code`) | `None` — never ours |
/// | a subcommand path that names a row (any [`LEADING_GLOBAL_FLAGS`] ahead of it stepped over) | [`Input::CliLine`] |
/// | a family path followed by `--help`/`-h` (`teton provider --help`) | [`Input::CliHelp`] |
/// | a subcommand path with no row (`uninstall`, a bare family) | [`Input::CliRefused`] |
/// | no path, and the next token is one of [`CLI_FLAGS`] | [`Input::CliRefused`] |
/// | no path, and nothing follows | [`Input::CliRefused`] — bare `teton` |
/// | no path, and something else follows (`teton is slow today`) | `None` — a question about the product |
///
/// `None` is the caller's decision to make, and the two callers make it
/// differently: a plain line becomes [`Input::Prompt`] with its own bytes, and a
/// `/`-prefixed one goes on to the table (OQ-1). That is why this returns an
/// `Option` rather than a bucket — "not a CLI line" is not the same statement as
/// "a prompt".
///
/// **Which words are the command is clap's answer, not this function's**
/// ([`cli_rows::cli_path`]). A hand-written matcher here would be a second
/// parser of one string, drifting out of agreement with the binary's own the
/// first time a subcommand is renamed (LESSON-529). Which words are the
/// *argument* follows from the same answer: whatever the path did not consume,
/// judged by the row's own grammar (BR-3).
///
/// Pure and total, like the classifier it serves.
fn cli_line(line: &str) -> Option<Input<'_>> {
    // `match_name_words` is the table's own word matcher, so `teton` is matched
    // on a word boundary exactly as a row name is: `tetonx provider list` and
    // `teton-code` are not this, and neither is `Teton provider list` — a
    // command is lowercase, and a capitalised mention is prose (REQ-581's
    // reply-side rule, applied to the entry line).
    let rest = match_name_words(line, TETON)?;
    // The shell's own global flags may precede the subcommand (`teton -y policy
    // set-tier …`); the walk starts after them and they ride the recognized
    // line so the row's parse still sees them (m5).
    let (shell_flags, rest) = split_leading_flags(rest);
    let tokens: Vec<&str> = rest.split_whitespace().collect();
    let path = cli_rows::cli_path(&tokens);
    if path.is_empty() {
        return match tokens.first() {
            None => Some(Input::CliRefused(ALREADY_IN_A_SESSION.to_owned())),
            Some(token) if CLI_FLAGS.contains(token) => {
                Some(Input::CliRefused(CLI_FLAGS_ARE_SHELL_ONLY.to_owned()))
            }
            // "teton is slow today" — a legitimate question about the product,
            // and the model is who answers it (BR-4).
            Some(_) => None,
        };
    }
    // Word-wise against the path rather than against a joined string: the
    // comparison a row name and a subcommand path are equal *by* is their words,
    // and doing it this way costs no allocation on a line typed every prompt.
    let matched = COMMANDS
        .iter()
        .find(|spec| spec.name.split_whitespace().eq(path.iter().copied()));
    let Some(spec) = matched else {
        // A family followed by an explicit help request gets the family's own
        // page (T6); a family typed bare, or with a word that names nothing
        // under it, gets the session's rows under it (BR-4).
        if let Some(flag @ ("--help" | "-h")) = tokens.get(path.len()).copied() {
            if let Some(help) = cli_rows::family_help(&path, flag) {
                return Some(Input::CliHelp(help));
            }
        }
        return Some(Input::CliRefused(cli_rows::refusal_for_path(&path)));
    };
    Some(Input::CliLine {
        name: spec.name,
        args: after_words(rest, path.len()),
        shell_flags,
    })
}

/// Split the [`LEADING_GLOBAL_FLAGS`] off the front of `rest` (the text after
/// `teton`), returning `(flags as typed, the remainder)` — `("", rest)` when
/// none lead (verify m5).
///
/// Word-wise and exact: a leading token that is not one of the four spellings
/// ends the flag run, so `teton -whatever …` keeps `-whatever` as the first
/// token of the walk and stays whatever the walk says it is (a prompt).
fn split_leading_flags(rest: &str) -> (&str, &str) {
    let mut consumed = 0;
    let mut remainder = rest;
    loop {
        let trimmed = remainder.trim_start();
        let end = trimmed.find(char::is_whitespace).unwrap_or(trimmed.len());
        let token = &trimmed[..end];
        if token.is_empty() || !LEADING_GLOBAL_FLAGS.contains(&token) {
            break;
        }
        consumed = rest.len() - trimmed.len() + end;
        remainder = &trimmed[end..];
    }
    (rest[..consumed].trim(), remainder.trim_start())
}

/// Every row sitting **under** `prefix` — the rows a family typed bare should
/// point at (REQ-582 BR-4).
///
/// Generated from [`COMMANDS`] for BR-7's reason, one surface further along: a
/// row added to the `/provider` family is named by the refusal without a second
/// list to maintain. It is deliberately the *table's* rows rather than clap's
/// subcommands, because what a user can type here is what the table lists —
/// `/provider setup` is a session row with no CLI subcommand at all, and it is
/// the most likely thing someone typing `teton provider …` actually wants.
///
/// Word-wise, so `policy set` does not claim the `policy set-tier` row.
pub(crate) fn rows_under(prefix: &[&str]) -> Vec<&'static str> {
    COMMANDS
        .iter()
        .filter(|spec| {
            let mut words = spec.name.split_whitespace();
            prefix.iter().all(|word| words.next() == Some(*word)) && words.next().is_some()
        })
        .map(|spec| spec.name)
        .collect()
}

/// `line` with its first `count` whitespace-separated words dropped, trimmed.
///
/// The argument of a recognized CLI line: what the subcommand path did not
/// consume. Counted rather than matched against the row's name, because the two
/// are the same words only when the user typed the canonical spelling —
/// [`cli_rows::cli_path`] honours clap's aliases, so `teton p list kimi` (were
/// `p` ever an alias of `provider`) resolves to the `provider list` row while
/// the line itself says something else. Matching the row's name against the line
/// would silently drop that line's argument; counting the words the walk
/// consumed cannot.
///
/// Whitespace runs collapse, exactly as they do between a two-word row's words
/// in [`match_name_words`].
fn after_words(line: &str, count: usize) -> &str {
    let mut rest = line;
    for _ in 0..count {
        rest = rest.trim_start();
        let end = rest.find(char::is_whitespace).unwrap_or(rest.len());
        rest = &rest[end..];
    }
    rest.trim()
}

/// Build the `prompt/turn` request a classified **prompt** line becomes.
///
/// The one place the entry loop turns a *prompt* into a request, so what reaches
/// the daemon is the classifier's output and nothing else: a plain line arrives
/// byte-identically to what was typed (AC-7) and an escaped line arrives with
/// exactly the leading pair collapsed (AC-7b). `text` is the payload of an
/// [`Input::Prompt`] or [`Input::EscapedPrompt`]; a command never reaches here
/// at all (BR-1).
///
/// An [`Input::Skill`] does not reach here either, and has no sibling
/// constructor beside this one: its request carries **no prompt at all**
/// (REQ-585 ADR-3 — `prompt: vec![]` and a `skill` naming the invocation), so
/// there is no line for a builder to turn into content. The entry loop composes
/// it at the same place it calls this, from the classifier's own `name` and
/// `raw_arguments`.
#[must_use]
pub fn prompt_turn_params(session_id: &SessionId, text: &str) -> PromptTurnParams {
    PromptTurnParams {
        session_id: session_id.clone(),
        prompt: vec![PromptBlock::Text {
            text: text.to_owned(),
        }],
        // A typed line is never a skill invocation. The two fields are mutually
        // exclusive and the daemon refuses a request carrying both (ADR-3).
        skill: None,
    }
}

/// Run a classified command line, or render the reason it cannot run.
///
/// The table is consulted through [`resolve`]; a line that resolves to no row
/// renders one hint and issues no RPC (BR-2).
///
/// `registry` is the session's snapshot, and it is read for exactly two things,
/// both of them a name this session **has** and did not run:
///
/// * a name discovery *found and did not register* says why, rather than
///   "unknown command" (REQ-585 BR-10 / AC-17);
/// * a name it registered as **model-only** says which flag made it so
///   (REQ-587 BR-3 / AC-12) — the one refusal that has to name a line of the
///   user's own file, because nothing else on any surface would tell them why a
///   skill they can see in `/help` does not run.
///
/// A name with no entry at all takes the pre-REQ branch with the pre-REQ bytes,
/// and a *shadowed* name never arrives here at all — the row or the project
/// skill ran (BR-2), which is why there is no shadow branch to word.
///
/// # Errors
///
/// Propagates any transport error a handler's RPC raises.
pub fn dispatch(
    name: &str,
    args: &str,
    registry: &SkillSnapshot,
    conn: &mut Connection,
    ctx: &mut UiContext<'_>,
) -> anyhow::Result<CommandOutcome> {
    if let Some(hint) = skipped_skill_hint(name, registry) {
        render_rejection(&hint, ctx.surface);
        return Ok(CommandOutcome::Continue);
    }
    if let Some(hint) = model_only_hint(name, registry) {
        render_rejection(&hint, ctx.surface);
        return Ok(CommandOutcome::Continue);
    }
    match resolve(name, args) {
        Resolution::Run(spec, args) => (spec.handler)(conn, ctx, args),
        Resolution::Rejected(hint) => {
            render_rejection(&hint, ctx.surface);
            Ok(CommandOutcome::Continue)
        }
    }
}

/// The one line a name discovery skipped gets instead of "unknown command"
/// (REQ-585 BR-10, AC-17), or `None` when the pre-REQ answer is the right one.
///
/// Three guards, in this order, and each is a claim:
///
/// * a name the **table** claims is not this — the row runs, and a file that
///   happened to share its name is the daemon's to mark shadowed, not this
///   function's to report;
/// * a name with **no skipped entry** is `None`, so `resolve` composes the
///   unknown-command bytes it always has (AC-17's "unchanged" leg);
/// * only a name that matches a skipped entry is answered here, in the daemon's
///   own words for why.
///
/// The reason is the daemon's own words, bounded to one line where it was
/// composed (TASK-203) and defused again at the writer, because
/// [`render_rejection`] renders through [`Surface::line`]. It keeps
/// [`HELP_HINT`]'s tail because `/help`'s diagnostic line is where the skipped
/// file is named in full — the pointer points somewhere useful.
///
/// One residual, recorded rather than fixed: [`typed_token`] bounds the echoed
/// name at [`ECHO_MAX_CHARS`] (40) while BR-2 admits a name up to 64, so a
/// skipped skill with a name longer than 40 characters is quoted with an
/// ellipsis. AC-17's "pre-REQ bytes unchanged" holds for every name under that
/// bound, and widening the constant would loosen the echo guard for every
/// rejection in this module for the sake of a name nobody types.
fn skipped_skill_hint(name: &str, registry: &SkillSnapshot) -> Option<String> {
    if table_claim(name).is_some() {
        return None;
    }
    let entry = registry.skipped_named(name)?;
    Some(format!(
        "`{}` is a skill that was skipped: {} — {HELP_HINT}",
        typed_token(name),
        entry.reason,
    ))
}

/// The line a **model-only** name gets instead of "unknown command"
/// (REQ-587 BR-3, AC-12), or `None` when this is not that case.
///
/// It names the frontmatter key, because that is the only actionable fact: the
/// file is the user's own (or their repository's), the flag is one line of it,
/// and a refusal that said only "unknown command" would send the author looking
/// for a typo in a name that is spelled correctly and listed in `/help` two
/// lines above.
///
/// The same three guards [`skipped_skill_hint`] applies, for the same reasons —
/// a table-claimed name is the row's, and a name this session does not list at
/// all keeps the pre-REQ bytes. The third is [`user_dispatch`] rather than the
/// flag: a project skill that both shadows a user skill *and* says
/// `user-invocable: false` is refused as **shadowed** long before it reaches
/// here, and answering it with the flag would name the wrong file.
///
/// The two-flag row is worded apart from the one-flag row for
/// [`dispatch_mark`]'s reason: "nobody may run this" and "only the model may
/// run this" are different states of the author's file, and one sentence for
/// both would tell the author of a two-flag file that the model is using a skill
/// no roster contains.
fn model_only_hint(name: &str, registry: &SkillSnapshot) -> Option<String> {
    if table_claim(name).is_some() {
        return None;
    }
    let view = registry.listed(name)?;
    if user_dispatch(view) != UserDispatch::ModelOnly {
        return None;
    }
    let who = if view.model_invocable {
        "only the model may invoke it"
    } else {
        "nobody may invoke it — its frontmatter also says `disable-model-invocation: true`"
    };
    Some(format!(
        "`{}` is a skill whose frontmatter says `user-invocable: false`, so {who} — {HELP_HINT}",
        typed_token(name),
    ))
}

/// Run a recognized `teton …` line — the entry loop's [`Input::CliLine`] arm
/// (REQ-582 verify, M2).
///
/// `name`, `args` and `shell_flags` are the classifier's ([`cli_line`]). Three
/// shapes, decided by the row and by clap's tree, never by matching words:
///
/// * a **mirrored** row parses its own argument with clap inside its handler,
///   so it is dispatched as `/<name> <args>` — with any leading global flags
///   spliced onto the argument, because clap accepts a global flag after the
///   subcommand as readily as before it and the row's own parse is then the one
///   place that sees them and says they were ignored;
/// * a pre-REQ row whose name is a **leaf** in the tree (`cost`, `effort`,
///   `model set`, `provider test`) reads a plain string, so its whole typed argv
///   is validated by [`crate::Cli::try_parse_from`] first. `Err` renders the
///   parser's own message and dispatches nothing — `teton effort low extra`,
///   `teton cost extra` (clap takes `effort`'s level as a free `Option<String>`,
///   so `teton effort bogus` *parses* and the row's own vocabulary rejects
///   `bogus` one step later, exactly as the shell does). `Ok` derives the row's
///   argument from the parsed [`crate::Command`] — the level, the model name,
///   the provider id — and dispatches that, so `teton model set qwen --yes`
///   reaches `/model set` as `qwen` and the `--yes` is reported as ignored
///   ([`cli_rows::shell_flags_line`]) rather than forwarded as part of a model
///   name; the row's handler still validates the value against its own
///   vocabulary, exactly as it does for a `/` line;
/// * a pre-REQ row whose name is a **family** — `model`, whose bare form is
///   REQ-555's one-line answer — cannot be parsed to a command (clap answers
///   `teton model` with the family's help page), so it dispatches directly, as
///   TASK-170 had it — after two decisions: `teton model --help` / `-h` gets the
///   family's own page ([`cli_rows::family_help`]), and any leading global
///   flags (`teton -y model`) are judged by the binary's tree on their own,
///   reported as ignored, and dropped, so the row is dispatched on its argument
///   alone.
///
/// This is not a second parser of the line (BR-3): the argv is the classifier's
/// tokens, the judge is the binary's own clap tree, and what the row receives is
/// what that tree parsed.
///
/// `registry` is passed straight through to [`dispatch`] and is never read on
/// this path: a recognized `teton …` line names a table row by construction, so
/// the skipped-skill hint it feeds has nothing to say about one. It is threaded
/// rather than faked with an empty snapshot, because a second entry point that
/// quietly disagrees with `dispatch` about what the session knows is the drift
/// this module exists to avoid.
///
/// # Errors
///
/// Propagates any transport error the row's handler raises, as [`dispatch`]
/// does.
pub fn run_cli_line(
    name: &str,
    args: &str,
    shell_flags: &str,
    registry: &SkillSnapshot,
    conn: &mut Connection,
    ctx: &mut UiContext<'_>,
) -> anyhow::Result<CommandOutcome> {
    let Some(spec) = builtin_row(name) else {
        // Unreachable: `cli_line` only ever names a table row. Rendered rather
        // than `unreachable!`ed because a panic in a session is worse than a
        // sentence in it.
        render_rejection(
            &format!("unknown command: `{}` — {HELP_HINT}", typed_token(name)),
            ctx.surface,
        );
        return Ok(CommandOutcome::Continue);
    };
    let words: Vec<&str> = name.split_whitespace().collect();
    if spec.mirror.is_some() {
        // The row parses for itself: any leading global flags are spliced onto
        // the argument so its own clap parse meets them and says they were
        // ignored (m5).
        if shell_flags.is_empty() {
            return dispatch(name, args, registry, conn, ctx);
        }
        let with_flags = format!("{shell_flags} {args}");
        return dispatch(name, with_flags.trim(), registry, conn, ctx);
    }
    if !cli_rows::is_leaf_path(&words) {
        // A pre-REQ row whose name is a **family** — `model`, whose bare form
        // is REQ-555's one-line answer. Clap cannot parse `teton model` to a
        // command (it answers with the family's help page), so the row is
        // dispatched directly (TASK-170) — with two things decided first
        // (verify residue, correctness Minor 3):
        //
        // * `teton model --help` / `-h` asked for the family's page, and gets
        //   the parser's own (T6's shape, on the family that is also a row);
        // * `teton -y model` carried the shell's global flags, which the row
        //   has no grammar to meet: they are judged by the binary's own tree
        //   (`teton -y` is legal argv — a session with `--yes` — so a bare
        //   `Cli::try_parse_from` over the flags alone is what the shell
        //   would accept or refuse), reported as ignored, and dropped, so the
        //   row runs on `args` alone rather than being handed `-y` as an
        //   argument it takes none of.
        if matches!(args, "--help" | "-h") {
            if let Some(page) = cli_rows::family_help(&words, args) {
                cli_rows::render_clap_text(&page, false, ctx.surface);
                return Ok(CommandOutcome::Continue);
            }
        }
        if !shell_flags.is_empty() {
            let flags_only = std::iter::once(TETON).chain(shell_flags.split_whitespace());
            match crate::Cli::try_parse_from(flags_only) {
                Err(err) => {
                    cli_rows::render_clap_error(&err, ctx.surface);
                    return Ok(CommandOutcome::Continue);
                }
                Ok(cli) => ctx
                    .surface
                    .line(LineKind::Info, &cli_rows::shell_flags_line(name, cli.yes)),
            }
        }
        return dispatch(name, args, registry, conn, ctx);
    }
    let argv: Vec<&str> = std::iter::once(TETON)
        .chain(shell_flags.split_whitespace())
        .chain(words.iter().copied())
        .chain(args.split_whitespace())
        .collect();
    let cli = match crate::Cli::try_parse_from(argv) {
        Err(err) => {
            cli_rows::render_clap_error(&err, ctx.surface);
            return Ok(CommandOutcome::Continue);
        }
        Ok(cli) => cli,
    };
    // The row's argument, read off what clap parsed rather than off the line.
    //
    // The match is **exhaustive with no wildcard**, mirroring
    // `run_mirrored_command`'s (verify residue, arch Minor): the leaves that
    // are pre-REQ rows are exactly the four named first, and every other
    // variant is a mirrored row (dispatched before this parse), a shell-only
    // or retired command (refused by the classifier before any row runs), or
    // `None` (a leaf path always parses to a command). Those arms are
    // unreachable from every caller — but they are *named*, so a subcommand
    // added later cannot ship without a decision about what its typed line
    // does here. One sentence for all of them rather than a panic, for the
    // reason every other unreachable arm in this client gives.
    let row_args = match cli.command {
        Some(crate::Command::Cost) => String::new(),
        Some(crate::Command::Effort { level }) => level.unwrap_or_default(),
        Some(crate::Command::Transcript { action }) => match action {
            crate::TranscriptCli::Enable => "enable".to_owned(),
            crate::TranscriptCli::Disable => "disable".to_owned(),
            crate::TranscriptCli::Status => "status".to_owned(),
        },
        // REQ-612: the transcript family's shape one feature over. Shell-only,
        // so this is unreachable from the classifier for the same reason — the
        // arm exists so the leaf is *named* rather than swept into the
        // catch-all below.
        Some(crate::Command::Context { action }) => match action {
            crate::ContextCli::Enable => "enable".to_owned(),
            crate::ContextCli::Disable => "disable".to_owned(),
            crate::ContextCli::Status => "status".to_owned(),
        },
        Some(crate::Command::Model {
            action: crate::ModelAction::Set { name },
        }) => name,
        Some(crate::Command::Provider {
            action: crate::ProviderAction::Test { id },
        }) => id,
        None
        | Some(
            crate::Command::Provider {
                action: crate::ProviderAction::Add { .. } | crate::ProviderAction::List,
            }
            | crate::Command::Boundary {
                action: crate::BoundaryAction::Add { .. } | crate::BoundaryAction::List,
            }
            | crate::Command::Policy {
                action:
                    crate::PolicyAction::SetTier { .. }
                    | crate::PolicyAction::SetCategory { .. }
                    | crate::PolicyAction::Show
                    | crate::PolicyAction::Set { .. },
            }
            | crate::Command::Model {
                action: crate::ModelAction::List | crate::ModelAction::Status,
            }
            | crate::Command::Doctor
            | crate::Command::Uninstall { .. },
        ) => {
            render_rejection(
                &format!(
                    "`teton {name}` parsed as a command this session has no row for — {HELP_HINT}"
                ),
                ctx.surface,
            );
            return Ok(CommandOutcome::Continue);
        }
    };
    if cli.yes || cli.verbose {
        ctx.surface
            .line(LineKind::Info, &cli_rows::shell_flags_line(name, cli.yes));
    }
    dispatch(name, &row_args, registry, conn, ctx)
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
///
/// Built-in only, and stays that way (REQ-585 ADR-13): it returns a
/// `&'static CommandSpec`, which a `String`-backed skill name cannot be, and
/// widening it to take one would mean either leaking the registry or handing
/// every rejection path a lifetime it has no use for. A skill never reaches
/// here — [`classify`] has already sorted it into [`Input::Skill`].
fn resolve<'a>(name: &str, args: &'a str) -> Resolution<'a> {
    let Some(spec) = builtin_row(name) else {
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
    echoed_within(text, ECHO_MAX_CHARS)
}

/// [`echoed`] with the bound supplied (verify m2).
///
/// The one place a rejection's quoting bound is chosen is [`ECHO_MAX_CHARS`];
/// clap's rendered lines need the same defusing under a wider bound — a usage
/// clause or a help line is longer than any command name and is the binary's
/// own text, while a stray positional inside it is the user's — and a second
/// copy of the loop for a second number is how the control-character rule gets
/// applied on one path and forgotten on another.
pub(crate) fn echoed_within(text: &str, max_chars: usize) -> String {
    let mut out = String::with_capacity(text.len());
    let mut chars = text.chars();
    for ch in chars.by_ref().take(max_chars) {
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

/// The family a row is listed under in `/help` — its first word, when more than
/// one row shares that word, and `""` otherwise (REQ-582 BR-1).
///
/// The rule is deliberately about *sharing* rather than about a name having a
/// space in it. `/model` is one word and belongs with `/model list`, `/model
/// set` and `/model status`; `/doctor` is one word and belongs with nothing, and
/// giving it a group heading of its own would turn a listing into an index.
/// Every row that shares no first word falls into the unnamed group, and those
/// list together wherever the table puts them — the plain session commands at
/// the top, `/doctor` and `/quit` at the bottom.
fn help_family(name: &'static str) -> &'static str {
    let first = first_word(name);
    let shared = COMMANDS
        .iter()
        .filter(|spec| first_word(spec.name) == first)
        .count();
    if shared > 1 {
        first
    } else {
        ""
    }
}

/// A row name's first word.
fn first_word(name: &'static str) -> &'static str {
    name.split_whitespace().next().unwrap_or(name)
}

/// BR-2's reserved set: every name a skill may not have, **derived** from
/// [`COMMANDS`] rather than listed (REQ-585, ADR-13).
///
/// Three parts, and each is a claim about a way a name could be taken:
///
/// * every **spelling** of every row — the canonical name and every alias, so
///   `exit` is reserved because `/exit` is `/quit`;
/// * the **first word** of every row, so a skill named `provider` cannot take
///   `/provider foo` and lose `/provider list` to longest-match — the one case
///   where the table claims a name it does not spell as a row;
/// * [`TETON`], which REQ-582's [`cli_line`] claims before the table is
///   consulted at all.
///
/// It is derived because a hand-written copy is a second home for a fact whose
/// first home is right here, and the copy ships green the day a row is added
/// (LESSON-546, LESSON-456).
///
/// It is `#[cfg(test)]` because production reads the same derivation through
/// [`table_claim`], which answers *which* claim rather than merely whether one
/// exists. This function is the set-shaped view of the same map, kept so the
/// test below can quantify over it.
///
/// **The daemon cannot read either of them**, and that is the reason the
/// enforcement lives here: `tetond` does not depend on `teton`, so it has no
/// `COMMANDS` to compare a name against, and `skills::mod` deliberately leaves
/// the reserved case to this crate. There is no second copy over there — and
/// no daemon-side mark to lean on, which is why the tests below use a registry
/// that offers reserved names *unshadowed*: that is the shape the daemon really
/// sends.
#[cfg(test)]
fn reserved_names() -> std::collections::BTreeSet<&'static str> {
    let mut names = std::collections::BTreeSet::new();
    for spec in COMMANDS {
        for spelling in spec.spellings() {
            names.insert(spelling);
            names.insert(first_word(spelling));
        }
    }
    names.insert(TETON);
    // The two derivations must agree, or the test below quantifies over a set
    // production does not enforce.
    for name in &names {
        assert!(
            table_claim(name).is_some(),
            "`{name}` is reserved here and unclaimed by `table_claim`",
        );
    }
    names
}

/// `/help`: the command list, generated from [`COMMANDS`] (BR-7), grouped by
/// family (REQ-582 BR-1), plus the footer lines documenting how arguments are
/// read (ADR-2) and the `//` escape (BR-1b).
///
/// Aliases are rendered from the same rows, so BR-7 covers them too: a spelling
/// that dispatches cannot be absent from `/help`, and `/help` cannot promise one
/// that does not dispatch.
///
/// The grouping is a blank line at each family boundary and nothing else — no
/// headings, no indentation. At ~25 rows the listing needs air rather than
/// structure, and a heading would be a second name for a family whose rows
/// already begin with it.
///
/// REQ-585 adds one section *below* the built-in rows and *above* both footers
/// (ADR-12), and nothing else: the built-in half is generated by the same loop
/// over the same table it always was, and [`help_family`] takes a
/// `&'static str`, so a skill named `provider` cannot re-group the four
/// `/provider` rows — the type says it will never be offered one.
///
/// An **empty** registry renders no section at all — no header, no `0 skills`
/// line. That is the state of every user with no `~/.claude`, and the state
/// ADR-2 leaves a client in against a daemon that does not serve `skills/list`,
/// where the claim is that `/help` is byte-for-byte what it is today. A section
/// announcing nothing would make that claim false for most of the people it is
/// made to.
fn render_help(surface: &mut dyn Surface, registry: &SkillSnapshot) {
    // Names pad to the widest row, so a later two-word row (`model set`)
    // re-aligns the whole list instead of breaking out of it.
    let width = COMMANDS
        .iter()
        .map(|spec| spec.name.len())
        .max()
        .unwrap_or(0);
    let mut previous: Option<&str> = None;
    for spec in COMMANDS {
        let family = help_family(spec.name);
        if previous.is_some_and(|prev| prev != family) {
            surface.line(LineKind::Info, "");
        }
        previous = Some(family);
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
    render_skills_section(surface, registry);
    // The footers are not rows, and the same blank line that separates two
    // families separates them from the last one.
    surface.line(LineKind::Info, "");
    surface.line(LineKind::Info, ARGUMENT_FOOTER);
    // Last, as it has been since REQ-555: the escape hatch is the line a user
    // scrolling to the bottom of `/help` is looking for.
    surface.line(LineKind::Info, ESCAPE_FOOTER);
}

/// The one line that opens `/help`'s skills section (REQ-585 ADR-12).
///
/// It carries the *positive* half of what [`ARGUMENT_FOOTER`]'s qualification
/// says, at the top of the rows it is true of, so the two never sit adjacent and
/// contradict each other (BR-3).
const SKILLS_HEADER: &str = "skills — arguments are passed through as typed:";

/// `/help`'s skills section: header, one row per skill, one diagnostic line —
/// or nothing at all for an empty registry (REQ-585 BR-3, ADR-12).
///
/// Every field that came out of a file — the name, the hint, the description,
/// the shadow reason, a skipped path — is rendered through [`Surface::line`],
/// which defuses it. The daemon bounded them to one line when it read them
/// (TASK-203); this is the second end of that, and it is the end that touches
/// the terminal (LESSON-517, ADR-009's shape).
fn render_skills_section(surface: &mut dyn Surface, registry: &SkillSnapshot) {
    if registry.is_empty() {
        return;
    }
    surface.line(LineKind::Info, "");
    surface.line(LineKind::Info, SKILLS_HEADER);
    for view in &registry.skills {
        surface.line(LineKind::Info, &skill_row(view));
    }
    surface.line(LineKind::Info, &skills_diagnostic(registry));
}

/// One `/help` skill row: `/name [hint] — description (source)`, with a row the
/// user may not type marked inside the same parenthetical (AC-1, REQ-587 AC-12).
///
/// The mark shares the source's parentheses rather than taking an em-dash of its
/// own, because the description already owns that punctuation and a row with
/// both would read as two sentences about different things.
///
/// **What counts as shadowed is not only what the daemon said.** A row the
/// client's own table claims is marked too ([`claimed_by_a_row`]) — BR-3's rule
/// is that `/help` cannot list a dispatchable skill the table does not resolve,
/// and the client is the thing that resolves. The two normally agree; where they
/// cannot is version skew (ADR-2's own scenario), a client carrying a row its
/// daemon has never heard of, and there the client is right.
fn skill_row(view: &SkillView) -> String {
    let mut row = format!("/{}", view.name);
    if let Some(hint) = &view.argument_hint {
        row.push(' ');
        row.push_str(hint);
    }
    if let Some(description) = &view.description {
        row.push_str(" — ");
        row.push_str(description);
    }
    row.push_str(" (");
    row.push_str(source_word(view.source));
    if let Some(mark) = dispatch_mark(view) {
        row.push_str(", ");
        row.push_str(&mark);
    }
    row.push(')');
    row
}

/// Why `/help` says the user cannot type this row, or `None` when they can
/// (REQ-587 BR-3, AC-12).
///
/// **Rendered from [`user_dispatch`] and from nothing else**, which is what
/// fixes the precedence at this surface: a row that is both shadowed and
/// model-only reads `shadowed by …`, because the name belongs to another file
/// entirely and "model-only" would name a capability *this* row does not have
/// either. A mark composed here from `!view.user_invocable` would call another
/// file's name a model-only skill
/// (`shadowing_wins_over_model_only_in_the_mark` fails on exactly that).
///
/// BR-3's third state gets its **own** words rather than borrowing the second's.
/// A row both flags deny is listed, invocable by nobody, and is a named
/// diagnostic rather than a silent drop — so calling it `model-only` would tell
/// a reader the model can reach a file the model cannot reach, and leave the
/// author of a two-flag file with nothing on any surface to act on. This is the
/// only surface that renders the combination at all: `classify` refuses the name
/// either way, and the daemon's roster simply omits it.
///
/// `model_invocable` is read here and nowhere else in the composition: it
/// answers a *different* question ("may the model?"), and it is asked only once
/// [`user_dispatch`] has already answered the user's with `ModelOnly`.
fn dispatch_mark(view: &SkillView) -> Option<String> {
    match user_dispatch(view) {
        UserDispatch::Allowed => None,
        UserDispatch::Shadowed(by) => Some(format!("shadowed by {by}")),
        UserDispatch::ModelOnly => Some(model_only_words(view.model_invocable).to_owned()),
    }
}

/// What a row the **user** may not type is called, once that much is settled:
/// `model-only`, or `invocable by nobody` when the model cannot reach it either
/// (REQ-587 BR-3).
///
/// The second half of [`dispatch_mark`]'s decision, split out because a *second*
/// surface renders it: BR-9's `/verbose` block names the same file's flags one
/// line under the echo line, from `SkillInvoked`'s copy of the same two facts
/// rather than from a [`SkillView`]. Two spellings of this precedence would
/// disagree only in the case that matters — the file both flags deny, where one
/// surface would be telling the user the model is running a skill no roster
/// contains (LESSON-528).
///
/// `model_invocable` is read **here and nowhere else** in the composition. It
/// answers a different question from the user's, and it is asked only after
/// [`user_dispatch`] has answered the user's with `ModelOnly`.
pub(crate) fn model_only_words(model_invocable: bool) -> &'static str {
    if model_invocable {
        "model-only"
    } else {
        "invocable by nobody"
    }
}

/// Whether the **user** may reach a listed row by typing `/name`, and why not
/// when they may not (REQ-587 BR-3).
///
/// The client's copy of `Skill::user_dispatch`, which is where TASK-212 decided
/// this: the user's question is three-valued, not two, and the two facts ride
/// the wire **separately and verbatim** ([`SkillView::shadowed`] and
/// [`SkillView::user_invocable`]) precisely so that a client composes them here
/// rather than being handed a pre-composed sentence it would have to re-parse
/// (LESSON-529).
///
/// The order is the decision, not a detail: **shadowing wins**. Only once
/// nothing owns the spelling is `user-invocable: false` the reason, so no
/// surface can read "model-only" off a row whose name resolves to a different
/// file. One composition, consulted by [`SkillSnapshot::dispatchable`],
/// [`dispatch_mark`] and [`model_only_hint`] alike — three surfaces, one answer,
/// which is the whole of BR-3's both-directions pin.
///
/// The client's spelling of the shared three-state answer, carrying the
/// rendered sentence this crate can be more specific about than the wire can.
/// The **rule** — shadowing wins over model-only — is
/// `teton_protocol::methods::user_dispatch`, one home for both sides (BUG-192).
type UserDispatch = teton_protocol::methods::UserDispatch<String>;

/// [`UserDispatch`] for one row.
///
/// [`shadow_reason`] runs **first and separately**: the built-in table claim is
/// this client's precondition, composed on top of the shared rule rather than
/// folded into it (BUG-192). Folding it in is what made the mirror diverge from
/// the daemon's copy in the first place.
fn user_dispatch(view: &SkillView) -> UserDispatch {
    teton_protocol::methods::user_dispatch(shadow_reason(view), view.user_invocable)
}

/// What owns a skill's name instead of the skill, or `None` when the skill owns
/// it.
///
/// The **shadowing** half of [`user_dispatch`], and only that half — this is the
/// client's copy of `Skill::shadow_reason`, and it stays two-valued for the
/// daemon's reason: folding `user-invocable: false` in here would render
/// "model-only" inside a sentence that reads `shadowed by …` at every surface
/// that draws one. Owning the name and being the user's to type are two
/// questions; [`user_dispatch`] is where they compose, in that order.
fn shadow_reason(view: &SkillView) -> Option<String> {
    // This crate's own sentence first, where it has one. The daemon marks a
    // reserved name too (it must — a client without a command table would
    // otherwise dispatch one), but it knows only *that* the table claims the
    // name, not whether it is a row, an alias or a family word. Preferring the
    // wire's generic mark here would trade `the built-in `/quit`` for `a
    // built-in command of the same name` on the one surface that can be
    // specific.
    if let Some(claim) = table_claim(&view.name) {
        return Some(claim.words());
    }
    view.shadowed.clone()
}

/// How a source is spelled wherever this client names one.
///
/// `/help`'s rows and diagnostic line, the consent prompt's subject block, and
/// BR-12's echo line all read it: one spelling of "user"/"project", so a user
/// who approved `skill \`status\` (user)` reads the same word back in
/// `/status → skill status (user, …)`. A second copy in `session_ui` would be
/// two homes for one vocabulary (LESSON-546).
pub(crate) fn source_word(source: SkillSource) -> &'static str {
    match source {
        SkillSource::User => "user",
        SkillSource::Project => "project",
    }
}

/// [`source_word`], with BR-9's shadowing clause where it applies:
/// `project — shadows your user skill`.
///
/// One spelling in the three places this client names the swap — BR-4's
/// acknowledgment prompt, BR-9's echo line, and `/verbose` under it — so a user
/// who acknowledged `validate (project — shadows your user skill)` reads the
/// same words back when the expansion lands. The daemon has its own copy
/// (`tools::skill::SkillFrame::source_clause`) for the *frame the model reads*,
/// which is a different audience and deliberately a separate rendering of the
/// same typed fact (LESSON-529); what must not exist is two copies on **this**
/// side.
///
/// A **user** skill never carries the clause, matching that counterpart arm for
/// arm: the swap BR-4 is about is a repository taking a name from the shelf the
/// user installed. A same-source name contest (`skills/` beating `commands/`)
/// is real, and it is `/help`'s `shadowed by` mark rather than this one — the
/// row that lost is not the row being invoked.
pub(crate) fn source_words(source: SkillSource, shadows_user_skill: bool) -> String {
    match (source, shadows_user_skill) {
        (SkillSource::User, _) | (SkillSource::Project, false) => source_word(source).to_owned(),
        (SkillSource::Project, true) => {
            format!("{} — shadows your user skill", source_word(source))
        }
    }
}

/// The line closing the skills *section* (BR-3): what was registered, from
/// where, and what was found and not registered.
///
/// The skipped entries are named rather than counted alone, because a count is
/// a diagnostic a user cannot act on — LESSON-481's shape, and the reason BR-1
/// makes discovery name every file it drops.
fn skills_diagnostic(registry: &SkillSnapshot) -> String {
    let (user, project) = registry.source_counts();
    let total = registry.skills.len();
    let mut line = format!(
        "{total} skill{} (user {user}, project {project}); {} skipped",
        if total == 1 { "" } else { "s" },
        registry.skipped.len(),
    );
    if !registry.skipped.is_empty() {
        line.push_str(": ");
        line.push_str(
            &registry
                .skipped
                .iter()
                .map(|entry| format!("{} — {}", entry.path, entry.reason))
                .collect::<Vec<_>>()
                .join("; "),
        );
    }
    line
}

/// Every mirrored row, as `(session name, shell twin)`, in table order (REQ-582
/// BR-8).
///
/// The hand-off nudge is built from this rather than from a list of its own: a
/// row added to the table is nudged for without a second list to maintain, which
/// is BR-7's rule ("`/help` is generated from the table") applied to the other
/// surface that names commands.
///
/// Its one caller is [`crate::session_ui::hand_off_after_turn`]'s generic arm.
pub(crate) fn mirrored_rows() -> impl Iterator<Item = (&'static str, &'static str)> {
    COMMANDS
        .iter()
        .filter_map(|spec| spec.mirror.map(|mirror| (spec.name, mirror.shell)))
}

/// Every spelling the table dispatches: each row's canonical name and each of
/// its aliases (REQ-612 AC-11).
///
/// [`mirrored_rows`]'s counterpart for the README cross-check, and derived from
/// [`COMMANDS`] for the reason `reserved_names` is: the table is the list that
/// decides what runs, so a documentation check reading a copy would go green the
/// day a row was renamed.
#[cfg(test)]
pub(crate) fn builtin_spellings() -> Vec<&'static str> {
    COMMANDS
        .iter()
        .flat_map(|spec| std::iter::once(spec.name).chain(spec.aliases.iter().copied()))
        .collect()
}

/// Every row with **no** shell twin — the commands that exist only inside a
/// session (REQ-612 AC-11).
///
/// The complement of [`mirrored_rows`] over the same table, so the two cannot
/// come to disagree about which half a row is in. What it is for: the resident
/// guide may name a mirrored row's `teton …` form as a footnote (REQ-582 BR-9's
/// mapping), and must never name a `teton …` form for one of these, because
/// there is no such command to run — BUG-181's shape, delivered by Teton's own
/// instructions.
#[cfg(test)]
pub(crate) fn session_only_rows() -> impl Iterator<Item = &'static str> {
    COMMANDS
        .iter()
        .filter(|spec| spec.mirror.is_none())
        .map(|spec| spec.name)
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
///
/// The section is rendered from the session's own snapshot (ADR-12), which is
/// the same value [`classify`] dispatches from — BR-3's "a skill cannot be
/// dispatchable without appearing in `/help`" holds because there is one list,
/// not because two readers agree. An empty snapshot renders no section at all,
/// which is the state of a user with no `~/.claude` and the state ADR-2 leaves
/// a new CLI in against an old daemon: `/help` is then byte-for-byte what it
/// was before this REQ.
fn handle_help(
    _conn: &mut Connection,
    ctx: &mut UiContext<'_>,
    _args: &str,
) -> anyhow::Result<CommandOutcome> {
    render_help(ctx.surface, &ctx.skills);
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
/// and no RPC. See [`cli_rows::write_gate`] for why that outranks BR-9 here —
/// REQ-582 generalized this gate to the four mirrored write rows, and this row
/// keeps only its own richer sentence.
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
    if cli_rows::write_gate(ctx.typed_input, test_seams_allowed()) == WriteGate::Refuse {
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
    let Some(id) = provider_test_id(args) else {
        ctx.surface.line(LineKind::Error, PROVIDER_TEST_USAGE);
        return Ok(CommandOutcome::Continue);
    };
    crate::provider_test_ui::run_in_session(conn, ctx, id)?;
    Ok(CommandOutcome::Continue)
}

/// The argument line of `/provider test`, read: exactly one word, or nothing.
///
/// Pure and separate from [`handle_provider_test`] so the rule can be *tested*
/// rather than restated. Its unit test used to re-run `split_whitespace` over a
/// fixture and assert on what that produced, which pins `split_whitespace` and
/// says nothing about the command — the handler itself needs a live
/// [`Connection`] and cannot be called from a unit test, so the parse had to
/// come out to be reachable at all.
///
/// `None` covers both ways of getting it wrong, because they have one answer:
/// no word at all (`Args::Required` catches this first, so it is defence in
/// depth) and a second word, which is a typo. Reading either as an id would mean
/// guessing which provider a user meant for a command that spends.
fn provider_test_id(args: &str) -> Option<&str> {
    let mut words = args.split_whitespace();
    let id = words.next()?;
    words.next().is_none().then_some(id)
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
/// `/transcript [on|off]` (REQ-611 BR-2, BR-3, BR-15). A session command
/// and nothing else: no tool reaches it, and its answer — the path included —
/// comes back on this connection as the RPC result, while the bus carries
/// only `transcript_state` (rendered by `session_ui`). Piped input may drive
/// it (spec OQ-2): it writes an owner-only file, not an escalation.
fn handle_transcript(
    conn: &mut Connection,
    ctx: &mut UiContext<'_>,
    args: &str,
) -> anyhow::Result<CommandOutcome> {
    let Some(session_id) = ctx.session_id.clone() else {
        ctx.surface
            .line(LineKind::Error, TRANSCRIPT_NEEDS_A_SESSION);
        return Ok(CommandOutcome::Continue);
    };
    let action = match args.trim() {
        "" => TranscriptAction::Status,
        "on" => TranscriptAction::On,
        "off" => TranscriptAction::Off,
        other => {
            ctx.surface.line(
                LineKind::Error,
                &format!(
                    "unknown transcript argument `{other}` — use `/transcript`, \
                     `/transcript on` or `/transcript off`."
                ),
            );
            return Ok(CommandOutcome::Continue);
        }
    };
    let is_status = matches!(action, TranscriptAction::Status);
    match conn.call(SessionTranscriptParams { session_id, action }, ctx)? {
        Ok(result) => ctx
            .surface
            .line(LineKind::Notice, &render_transcript(is_status, &result)),
        Err(err) if err.code == error_code::METHOD_NOT_FOUND => {
            ctx.surface.line(LineKind::Notice, TRANSCRIPT_UNAVAILABLE);
        }
        Err(err) => ctx.surface.line(
            LineKind::Error,
            &format!("the transcript is unavailable: {}", err.message),
        ),
    }
    Ok(CommandOutcome::Continue)
}

/// The one line `/transcript` draws. A bare call reports the state, the path,
/// the record count and any degraded reason (AC-5); `on`/`off` report what
/// the daemon now holds, so a switch refused by a degraded session says
/// `off` and why rather than echoing the request.
fn render_transcript(is_status: bool, result: &SessionTranscriptResult) -> String {
    let mut line = String::from("transcript: ");
    line.push_str(if result.enabled { "on" } else { "off" });
    match (&result.path, is_status) {
        (Some(path), true) => line.push_str(&format!(" — {path} ({} records)", result.records)),
        (Some(path), false) if result.enabled => {
            line.push_str(&format!(" — recording to {path}"));
        }
        (Some(_), false) => line.push_str(" — stopped"),
        (None, _) => {}
    }
    if let Some(reason) = &result.degraded {
        line.push_str(&format!(" — degraded: {reason}"));
    }
    line
}

const TRANSCRIPT_NEEDS_A_SESSION: &str =
    "no session is attached, so there is no transcript to switch or show.";

const TRANSCRIPT_UNAVAILABLE: &str =
    "this daemon does not serve transcripts — restart it after upgrading to use /transcript.";

/// `/context [on|off]` (REQ-612 BR-2, architecture ADR-6). The session-lifetime
/// half of the repository-notes switch, shaped as [`handle_transcript`] line for
/// line.
///
/// **It never calls `config/set`, and that is the whole of BR-2's two-switch
/// rule at this surface.** `/context off` moves *this session* and writes
/// nothing to `config.toml`; the durable default is `teton context disable`,
/// which is a different command with a different blast radius. A `config/set`
/// here would make one session's choice a machine-wide one, silently, from a
/// line the user typed inside a session — the failure REQ-611 BR-2 named and
/// this REQ inherits.
///
/// Bare `/context` is `Status` and is the **non-visual read path** REQ-560 BR-10
/// requires: it works on a pipe, because the status row is TTY-gated and a fact
/// the row might one day show has to be reachable without a terminal. It is also
/// the only surface that names the file (`SessionContextResult::file`) — the
/// broadcast `repo_context_state` event deliberately does not.
///
/// Piped input may drive all three actions: the switch changes what this
/// session's prompt carries and writes nothing, so it is not an escalation.
fn handle_context(
    conn: &mut Connection,
    ctx: &mut UiContext<'_>,
    args: &str,
) -> anyhow::Result<CommandOutcome> {
    let Some(session_id) = ctx.session_id.clone() else {
        ctx.surface.line(LineKind::Error, CONTEXT_NEEDS_A_SESSION);
        return Ok(CommandOutcome::Continue);
    };
    let action = match args.trim() {
        "" => ContextAction::Status,
        "on" => ContextAction::On,
        "off" => ContextAction::Off,
        other => {
            ctx.surface.line(
                LineKind::Error,
                &format!(
                    "unknown context argument `{other}` — use `/context`, \
                     `/context on` or `/context off`."
                ),
            );
            return Ok(CommandOutcome::Continue);
        }
    };
    match conn.call(SessionContextParams { session_id, action }, ctx)? {
        Ok(result) => {
            for line in render_context(&result) {
                ctx.surface.line(LineKind::Notice, &line);
            }
        }
        Err(err) if err.code == error_code::METHOD_NOT_FOUND => {
            ctx.surface.line(LineKind::Notice, CONTEXT_UNAVAILABLE);
        }
        Err(err) => ctx.surface.line(
            LineKind::Error,
            &format!("the repository notes are unavailable: {}", err.message),
        ),
    }
    Ok(CommandOutcome::Continue)
}

/// The lines `/context` draws: the state first, then — when there are byte
/// figures worth reading — the file, its size, what is resident and the cap.
///
/// **One or two lines, never more** (BR-2's list: file, source, bytes on disk,
/// resident bytes, cap, state). The split is by *whether the daemon has figures
/// to give*: `absent` and `withheld_off` opened no file, so there is nothing to
/// measure and a second line would be four zeroes pretending to be facts.
///
/// Every figure is the daemon's. `cap` in particular is the route's **effective**
/// cap, which no client can derive — it is `min(REPO_CONTEXT_MAX_BYTES,
/// budget_bytes / 4)` and needs the router's budget — so it travels and is
/// printed, never recomputed (ADR-5's one-derivation rule).
///
/// **All three actions render identically**, which is where this parts company
/// with [`render_transcript`] and is a decision rather than an omission. That
/// renderer varies by action because a transcript's *path* means something
/// different in a read than in a switch. Here it would not: the answer to
/// `/context off` is the state after the switch, which is exactly what a bare
/// `/context` asks for, so one rendering means an `on` refused by a boundary
/// says `withheld` and why instead of echoing what was requested — and the two
/// surfaces cannot word one state two ways.
fn render_context(result: &SessionContextResult) -> Vec<String> {
    let mut lines = vec![format!("context: {}", context_state_words(result.state))];
    // The figures line, only where there are figures. `file` is `None` exactly
    // when nothing was opened, so it is the one predicate rather than a second
    // opinion about which states carry bytes.
    if let Some(file) = &result.file {
        let mut line = format!("context: {file}");
        // The source is BR-2's sixth field and is normally the same word as the
        // file — the daemon spells the path root-relative, and the path of a
        // file at the root *is* its name. Printed only when the two differ, so
        // the common line reads `TETON.md` rather than `TETON.md (TETON.md)`;
        // a daemon that ever spells the path differently still says which of
        // the two names it read.
        if let Some(source) = result.source {
            let name = context_source_words(source);
            if name != file {
                line.push_str(&format!(" ({name})"));
            }
        }
        line.push_str(&format!(
            " — {} bytes on disk, {} resident, cap {}",
            result.bytes_on_disk, result.resident_bytes, result.cap
        ));
        if result.truncated {
            line.push_str(" (truncated)");
        }
        lines.push(line);
    }
    lines
}

/// One word for each state, in the vocabulary BR-2 lists.
///
/// The three withheld shapes keep their reasons apart on purpose: they name
/// three different remedies — a boundary to relax, a switch to flip, a file to
/// fix — and folding them would send a user to the wrong one
/// ([`RepoContextStateKind`]'s own argument).
fn context_state_words(state: RepoContextStateKind) -> &'static str {
    match state {
        RepoContextStateKind::Loaded => "on — the repository notes are resident",
        RepoContextStateKind::Truncated => "on — the repository notes are resident, truncated",
        RepoContextStateKind::Absent => "off — no TETON.md or AGENTS.md at the session root",
        RepoContextStateKind::WithheldBoundary => {
            "off — withheld: a local-only boundary covers the file"
        }
        RepoContextStateKind::WithheldOff => "off — the switch is off, so the file is not opened",
        RepoContextStateKind::Unreadable => "off — withheld: the file could not be read",
    }
}

/// Which of the two names was read, spelled for a person.
fn context_source_words(source: teton_protocol::methods::RepoContextSource) -> &'static str {
    match source {
        teton_protocol::methods::RepoContextSource::TetonMd => "TETON.md",
        teton_protocol::methods::RepoContextSource::AgentsMd => "AGENTS.md",
    }
}

const CONTEXT_NEEDS_A_SESSION: &str =
    "no session is attached, so there are no repository notes to switch or show.";

const CONTEXT_UNAVAILABLE: &str = "this daemon does not serve repository notes — restart it \
     after upgrading to use /context.";

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

/// `/cd [path]` (REQ-583 BR-7): move this session's root, or print it.
///
/// **Bare** it is a read of the root the daemon last described —
/// `SessionCreateResult.root`, refreshed by every `session_root_changed` — from
/// the state cache, with no RPC (`/permissions`' shape). A root nobody knows
/// (an older daemon that reported none) is said to be unknown, never guessed:
/// the client does not derive kind (ADR-1).
///
/// **With an argument** the path goes through [`resolve_cwd_argument`] — the
/// grammar `--cwd` uses, so the two accept and reject the same spellings
/// (AC-12) — and a spelling that cannot become an absolute path is one error
/// line and no RPC. Otherwise `session/set_cwd` is sent on the session's own
/// connection and context (D-4), and on success **nothing** is printed here:
/// the daemon publishes `context_cleared` and `session_root_changed` before it
/// answers, and those events draw the lines — the clear in its existing shape,
/// then the new root and (when it is not a project) the launch notice again
/// (BR-8) — on every attached client, the issuer included. A second rendering
/// from the answer would say the same thing twice to the one person who typed
/// it (`/clear`'s rule).
///
/// No typed-input gate, for `/clear`'s reason: session-scoped, no consent, no
/// money, and the daemon validates the path (BR-6's one validator). Available at
/// every permission level — it moves the jail, it does not mutate files.
/// `/projects [query]` — the machine's known projects (REQ-584 BR-9).
///
/// **Asks the daemon; never reads the registry file.** The file is the daemon's
/// (the REQ's Permissions table), and routing through it is also what keeps the
/// dev-folder scan in the one place BR-3 bounds it.
///
/// The daemon returns the answer already rendered, from the same composition the
/// `projects` tool reads — so a row the model sees and a row the user sees
/// cannot come to disagree (BR-9's one-renderer rule). This function styles the
/// lines; it does not restate the facts.
fn handle_projects(
    conn: &mut Connection,
    ctx: &mut UiContext<'_>,
    args: &str,
) -> anyhow::Result<CommandOutcome> {
    let query = args.trim();
    let params = ProjectsListParams {
        query: (!query.is_empty()).then(|| query.to_owned()),
        // The user asked, which is BR-3's gate on the scan.
        allow_scan: true,
    };
    match conn.call(params, ctx)? {
        Ok(result) => {
            for line in result.rendered.lines() {
                // `Surface::line` defuses; the daemon bounded and neutralised
                // the values, and this is the second pass every user-controlled
                // string on a surface gets.
                ctx.surface.line(LineKind::Info, line);
            }
        }
        Err(err) => ctx.surface.line(LineKind::Error, &err.message),
    }
    Ok(CommandOutcome::Continue)
}

fn handle_cd(
    conn: &mut Connection,
    ctx: &mut UiContext<'_>,
    args: &str,
) -> anyhow::Result<CommandOutcome> {
    let args = args.trim();
    if args.is_empty() {
        let (kind, line) = current_root_line(ctx.state.root.as_ref());
        ctx.surface.line(kind, &line);
        return Ok(CommandOutcome::Continue);
    }
    let Some(session_id) = ctx.session_id.clone() else {
        ctx.surface.line(LineKind::Error, CD_NEEDS_A_SESSION);
        return Ok(CommandOutcome::Continue);
    };
    // The shell's directory and home, read here at the edge: a relative `/cd
    // src` is relative to where the user's shell is, exactly as `--cwd src`
    // would be, and `~` is the same `~` (BR-7: one grammar, two spellings).
    let shell_cwd = std::env::current_dir().unwrap_or_default();
    let home = crate::home_dir();
    let cwd = match resolve_cwd_argument(args, &shell_cwd, home.as_deref()) {
        Ok(cwd) => cwd,
        Err(err) => {
            // One line, no RPC — the same shape every other rejected command
            // line takes (BR-2). The error names the argument it refused.
            ctx.surface
                .line(LineKind::Error, &cd_argument_refusal(&err));
            return Ok(CommandOutcome::Continue);
        }
    };
    // REQ-584 BR-8: send the bare name alongside the path reading. The daemon
    // tries the path first and reaches for the registry only if it is not a
    // directory, so `/cd src` with a `./src` present is unchanged.
    let name_hint =
        teton_core::session_root::is_bare_project_name(args).then(|| args.trim().to_owned());
    if let Err(err) = conn.call(
        SessionSetCwdParams {
            session_id,
            cwd,
            name_hint,
        },
        ctx,
    )? {
        report_cd_refusal(&err, ctx.surface);
    }
    Ok(CommandOutcome::Continue)
}

/// The line a bare `/cd` prints: the cached root and its kind, or — when no
/// daemon has described one — a notice saying so.
///
/// Split out so both arms are asserted without a socket. The known arm is
/// [`banner::root_line`]'s spelling, so `/cd`, the launch notice and the
/// `session_root_changed` line describe one root one way.
fn current_root_line(root: Option<&SessionRoot>) -> (LineKind, String) {
    match root {
        Some(root) => (
            LineKind::Info,
            format!("session root: {}", banner::root_line(root)),
        ),
        None => (LineKind::Notice, CD_ROOT_UNKNOWN.to_owned()),
    }
}

/// The rejection a `/cd` argument that cannot become a path gets back — the
/// grammar's own sentence (it names the argument), plus what to type instead.
fn cd_argument_refusal(err: &CwdArgError) -> String {
    format!(
        "/cd: {err} — `/cd <path>` takes an absolute path, a path relative to your shell, or `~`."
    )
}

/// What `/cd <path>` says when there is no session whose root to move.
const CD_NEEDS_A_SESSION: &str = "`/cd` needs a session to act on, and this command owns none.";

/// What a bare `/cd` says when the daemon never described the root — a build
/// older than `SessionCreateResult.root`, or a context that owns no session.
///
/// A version fact, not a failure (BUG-152's rule), so it is a notice: nothing is
/// wrong with the session, the client simply was not told.
const CD_ROOT_UNKNOWN: &str = "the session root is not known yet — this daemon did not report one \
                               when the session started.";

/// What `/cd <path>` says to a daemon built before REQ-583.
///
/// `CLEAR_UNAVAILABLE`'s shape: a version fact rather than a failure, and it says
/// what is true of such a daemon — the root a session starts with is the root it
/// keeps, so the remedy is to start one where the work is.
const CD_UNAVAILABLE: &str = "this daemon build cannot move a session root — start a session from \
                              the directory instead (`teton --cwd <path>`).";

/// Render the reason a `/cd <path>` did not happen — and nothing at all when it
/// did ([`report_clear_refusal`]'s shape, class matched on the code, never the
/// wording).
fn report_cd_refusal(err: &RpcError, surface: &mut dyn Surface) {
    match err.code {
        error_code::METHOD_NOT_FOUND => surface.line(LineKind::Notice, CD_UNAVAILABLE),
        // The daemon's sentence names the turn holding the session and says to
        // retry when it finishes; this adds only that the root did not move.
        error_code::SESSION_BUSY => surface.line(
            LineKind::Notice,
            &format!("the session root was not moved: {}", err.message),
        ),
        // A refused path (not a directory, does not exist) and anything else:
        // the daemon's own reason, which names the path (BR-6), on one error
        // line. The root and the conversation are unchanged.
        _ => surface.line(
            LineKind::Error,
            &format!("the session root could not be moved: {}", err.message),
        ),
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
/// fail-closed *only because* every consumer reads it with one polarity:
/// `seams_allowed` makes [`cli_rows::write_gate`] looser, so dropping it on a
/// release build can only refuse something that would otherwise have run. A
/// consumer that used the switch to make behaviour *stricter* would invert that
/// — ignoring it would silently loosen the shipped binary — and must mirror the
/// daemon's posture instead (`tetond`'s `test_seams_enabled`: refuse to run at
/// all rather than run with an unhonoured seam).
///
/// REQ-572 adds the second consumer, [`crate::web_setup_ui::gate`], with the
/// **same** polarity: the seam lets the walkthrough run on a pipe, so a release
/// build ignoring the switch can only fall back to printing instructions. The
/// invariant above is what made that reuse legitimate rather than convenient.
///
/// REQ-582 generalizes the first consumer instead of adding a third: the
/// `/model set` gate became [`cli_rows::write_gate`], which the four mirrored
/// write rows share with it. Same function, same polarity, one seam — the
/// invariant is unchanged and now covers five commands.
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
    // REQ-582: the recognition tests compare against what the **binary's** own
    // parser says about the same argv, which is the only ground truth for
    // "the same error the shell prints" (BR-3, AC-6).
    use clap::Parser;
    // Only the tests name a tier now: the tier vocabulary itself moved to
    // `session_ui`, so the production code here never mentions the type.
    use teton_protocol::events::WebTier;

    /// The registry of a session that found nothing — the state of every user
    /// with no `~/.claude`, and the state ADR-2 leaves a client in against a
    /// daemon that does not serve `skills/list`.
    ///
    /// Every pre-REQ-585 assertion in this module runs against it, which is the
    /// point: with an empty snapshot [`classify`] cannot return
    /// [`Input::Skill`] and `/help` renders no section, so "byte-for-byte what
    /// it is today" is not a claim about a code path nobody takes — it is what
    /// these ~66 call sites exercise.
    fn no_skills() -> SkillSnapshot {
        SkillSnapshot::empty()
    }

    /// A registry built the only way production builds one: out of the wire
    /// result (LESSON-544 — a fixture that reaches past the constructor leaves
    /// the constructor unguarded).
    fn registry(skills: Vec<SkillView>, skipped: Vec<SkillSkipped>) -> SkillSnapshot {
        SkillSnapshot::from(SkillsListResult { skills, skipped })
    }

    /// One registered skill, with everything optional left out: the ordinary
    /// row, invocable from both doors, which is what a file declaring neither
    /// of BR-3's flags registers as.
    fn skill(name: &str, source: SkillSource) -> SkillView {
        SkillView {
            name: name.to_owned(),
            source,
            description: None,
            argument_hint: None,
            shadowed: None,
            model_invocable: true,
            user_invocable: true,
        }
    }

    /// A `user-invocable: false` row: BR-3's third state, listed and marked,
    /// refused from `/name`, and still the model's.
    fn model_only(name: &str, source: SkillSource) -> SkillView {
        SkillView {
            user_invocable: false,
            ..skill(name, source)
        }
    }

    /// A row **both** flags deny — listed, invocable by nobody. Representable,
    /// on the wire (`both_invocation_flags_reach_the_client`), and rendered by
    /// exactly one surface.
    fn invocable_by_nobody(name: &str, source: SkillSource) -> SkillView {
        SkillView {
            model_invocable: false,
            ..model_only(name, source)
        }
    }

    /// A `disable-model-invocation: true` row: hidden from the model, and an
    /// ordinary row to every surface `/help` and [`classify`] own — the fixture
    /// that keeps the two flags from being read as one.
    fn user_only(name: &str, source: SkillSource) -> SkillView {
        SkillView {
            model_invocable: false,
            ..skill(name, source)
        }
    }

    /// [`skill`] with the two file-authored fields a `/help` row renders.
    fn described(name: &str, source: SkillSource, hint: &str, description: &str) -> SkillView {
        SkillView {
            argument_hint: (!hint.is_empty()).then(|| hint.to_owned()),
            description: Some(description.to_owned()),
            ..skill(name, source)
        }
    }

    /// A skill the **daemon** marked shadowed, as it does for every reserved
    /// name (BR-2) — the realistic AC-2 fixture.
    fn shadowed(name: &str, source: SkillSource, by: &str) -> SkillView {
        SkillView {
            shadowed: Some(by.to_owned()),
            ..skill(name, source)
        }
    }

    /// A daemon that offered a name it should have shadowed.
    ///
    /// Not a paranoid fixture: it is the only one that can tell BR-2's
    /// structural claim from a claim about the daemon's diligence. If the
    /// registry were consulted before the table, these entries would dispatch,
    /// and every assertion written against a correctly-shadowed fixture would
    /// still pass (ADR-13).
    fn offered_unshadowed(names: &[&str]) -> SkillSnapshot {
        registry(
            names
                .iter()
                .map(|name| skill(name, SkillSource::User))
                .collect(),
            Vec::new(),
        )
    }

    /// The `/help` lines a `RecordingSurface` recorded, blank lines kept.
    fn help_lines(registry: &SkillSnapshot) -> Vec<String> {
        let mut surface = RecordingSurface::new();
        render_help(&mut surface, registry);
        surface
            .lines_of(LineKind::Info)
            .iter()
            .map(|line| (*line).to_owned())
            .collect()
    }

    /// The rows of `/help`'s skills section: everything between
    /// [`SKILLS_HEADER`] and the diagnostic line that closes it.
    fn skill_rows(lines: &[String]) -> Vec<String> {
        let Some(start) = lines.iter().position(|line| line == SKILLS_HEADER) else {
            return Vec::new();
        };
        lines[start + 1..]
            .iter()
            .take_while(|line| line.starts_with('/'))
            .cloned()
            .collect()
    }

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
            // Empty by default; a test that needs a registry assigns
            // `ctx.skills` itself (REQ-585 — `/help` renders from the field).
            skills: SkillSnapshot::empty(),
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
                let Input::Command { name: n, args } = classify(&line, &no_skills()) else {
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
        render_help(&mut surface, &no_skills());
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
                // REQ-582: a mirrored row never rejects at resolve time either —
                // whatever follows the name is the shell twin's grammar to judge
                // (BR-3), so the bare form is what proves the row *dispatches*.
                // What the parser then says about an empty argument is pinned in
                // `cli_rows`, against clap's own message (AC-7).
                Args::Cli => "",
            };
            // Every spelling, not just the canonical one: an alias that is in
            // the table but unreachable from typed input is the same defect as
            // an unreachable row, and it fails here for the same reason.
            for spelling in spec.spellings() {
                let typed = format!("/{spelling} {expected_args}");
                let typed = typed.trim_end();
                let Input::Command { name, args } = classify(typed, &no_skills()) else {
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
        render_help(&mut surface, &no_skills());
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
        let Input::Command { name, args } = classify("/exit", &no_skills()) else {
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
            "transcript",
            // REQ-612 BR-2: the session-lifetime repository-notes switch,
            // declared here for the reason every row above it was — a new row
            // is a spec decision rather than a drive-by.
            "context",
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
            // REQ-583 BR-7: the move of a live session's root, declared here
            // for the reason every row above it was. Session-only by design:
            // its shell twin is the `--cwd` flag at launch, not a subcommand,
            // so it carries no mirror and a typed `teton cd` stays a prompt.
            "cd",
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
            // REQ-582 BR-1: the ten mirrored rows, declared here for the reason
            // every row above them was. Each name is *also* the subcommand path
            // its shell twin has, which is not a coincidence but the mechanism —
            // ADR-1 recognizes a typed `teton …` line by walking clap's tree to
            // a path and looking that path up here, so a row renamed away from
            // its twin would silently stop being reachable that way.
            "model list",
            "model status",
            "provider list",
            "provider add",
            "boundary list",
            "boundary add",
            "policy show",
            "policy set-tier",
            "policy set-category",
            "doctor",
            // REQ-584 BR-9: the locator's user surface, declared here first for
            // the reason this list exists — a new row is a spec decision.
            "projects",
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
            let Input::Command { name, args } = classify(typed, &no_skills()) else {
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
        let Input::Command { name, args } = classify("/ /foo", &no_skills()) else {
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
            let Input::Command { name, args } = classify(typed, &no_skills()) else {
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
            let Input::Prompt(text) = classify(line, &no_skills()) else {
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
            let Input::EscapedPrompt(text) = classify(typed, &no_skills()) else {
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
        assert_eq!(cli_rows::write_gate(true, false), WriteGate::Run);
        assert_eq!(cli_rows::write_gate(true, true), WriteGate::Run);
        // The e2e suite's allowance, and nothing else in the wild: a release
        // build's `test_seams_allowed` is false whatever the environment says.
        assert_eq!(cli_rows::write_gate(false, true), WriteGate::Run);
        // The shape that matters: piped input, no seam, no write.
        assert_eq!(cli_rows::write_gate(false, false), WriteGate::Refuse);
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

        let Input::Command { name, args } = classify(&typed, &no_skills()) else {
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
            assert_eq!(classify(line, &no_skills()), Input::Prompt(line));
        }
    }

    // BR-8, reverse direction, escape-hatch leg (BR-1b, AC-7b): `//` collapses
    // EXACTLY the leading pair; slashes anywhere else are untouched.
    #[test]
    fn the_double_slash_escape_collapses_only_the_leading_pair() {
        assert_eq!(
            classify("//usr/local/bin/x — why?", &no_skills()),
            Input::EscapedPrompt("/usr/local/bin/x — why?")
        );
        assert_eq!(classify("//", &no_skills()), Input::EscapedPrompt("/"));
        assert_eq!(
            classify("///etc", &no_skills()),
            Input::EscapedPrompt("//etc")
        );
        assert_eq!(
            classify("//help", &no_skills()),
            Input::EscapedPrompt("/help")
        );
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
                mirror: None,
                handler: handle_help,
            },
            CommandSpec {
                name: "model set",
                aliases: &[],
                summary: "change the current model",
                args: Args::Required("a catalog name"),
                mirror: None,
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
    //
    // REQ-582 AC-8 adds the grouping, so the row lines are read out from between
    // the blank separators rather than zipped against every rendered line — the
    // property is still "one line per command, in table order, and nothing else
    // but the footers".
    //
    // REQ-585 ADR-12 **widens** it rather than relaxing it, which is a
    // distinction worth writing down because the two edits look alike from a red
    // test: the count and the row zip are re-scoped to the built-in *prefix*
    // slice, and both footer assertions keep reading the **whole** rendered
    // list. Dropping them to the prefix too would let a skills section slip
    // below `ESCAPE_FOOTER` with every assertion still green — and the escape
    // hatch being the last line of `/help` is the one thing a user scrolling to
    // the bottom is looking for. It runs over both registries for the same
    // reason: the built-in half is the same bytes either way.
    #[test]
    fn help_renders_every_table_row_and_the_escape_footer() {
        let three = registry(
            vec![
                described("alpha", SkillSource::User, "[topic]", "Draft a note."),
                skill("beta", SkillSource::User),
                skill("gamma", SkillSource::Project),
            ],
            Vec::new(),
        );
        for (label, snapshot) in [("empty", no_skills()), ("three skills", three)] {
            let all = help_lines(&snapshot);
            let lines: Vec<&str> = all
                .iter()
                .map(String::as_str)
                .filter(|line| !line.is_empty())
                .collect();
            // Re-scoped to the built-in prefix: the rows this table generates
            // are the first `COMMANDS.len()` non-empty lines, in table order,
            // and nothing else is asserted *about them* by a section below.
            let builtin = &lines[..COMMANDS.len()];
            for (spec, line) in COMMANDS.iter().zip(builtin) {
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
            // Over the whole list, not the prefix: last, and second-last.
            assert_eq!(
                lines.last(),
                Some(&ESCAPE_FOOTER),
                "the escape footer is not the last line of /help with a {label} registry"
            );
            assert!(ESCAPE_FOOTER.contains("//"));
            // REQ-582 ADR-2 / OQ-5: the one way a session argument differs from
            // the shell's, said once, above the escape footer.
            assert_eq!(
                lines[lines.len() - 2],
                ARGUMENT_FOOTER,
                "the argument footer is not second-last with a {label} registry"
            );
            assert!(ARGUMENT_FOOTER.contains("whitespace"));
        }
        // And the count, kept exact where it can be: with nothing registered
        // there is no section, so the listing is still one line per command
        // plus the two footers — the pre-REQ number, unchanged.
        let empty = help_lines(&no_skills());
        assert_eq!(
            empty.iter().filter(|line| !line.is_empty()).count(),
            COMMANDS.len() + 2,
            "one line per command plus the argument and escape footers"
        );
    }

    // -----------------------------------------------------------------------
    // REQ-585: classify over a registry, and /help's skills section
    // -----------------------------------------------------------------------

    /// ADR-12: an empty registry renders **no section at all** — no header, no
    /// `0 skills` line.
    ///
    /// The default state of every user with no `~/.claude`, and the state ADR-2
    /// leaves a client in against a daemon that answers `skills/list` with
    /// `METHOD_NOT_FOUND`. Both are promised `/help` byte-for-byte as it is
    /// today, so this is pinned as an equality against the whole listing rather
    /// than as an absence of the header: a `0 skills` line, a stray blank, or a
    /// section header with nothing under it would each break the promise while
    /// passing a `!contains("skills —")`.
    #[test]
    fn an_empty_registry_renders_no_skills_section_at_all() {
        let lines = help_lines(&no_skills());
        assert!(
            !lines.iter().any(|line| line == SKILLS_HEADER),
            "an empty registry announced a section: {lines:#?}"
        );
        assert!(
            !lines.iter().any(|line| line.contains("skipped")),
            "an empty registry rendered a diagnostic line: {lines:#?}"
        );
        // Nothing at all: the listing ends at the footers it always ended at.
        assert_eq!(lines.last().map(String::as_str), Some(ESCAPE_FOOTER));
        assert_eq!(
            lines[lines.len() - 2].as_str(),
            ARGUMENT_FOOTER,
            "{lines:#?}"
        );
        assert_eq!(
            lines.iter().filter(|line| !line.is_empty()).count(),
            COMMANDS.len() + 2,
            "an empty registry changed the shape of /help: {lines:#?}"
        );
    }

    /// BR-2 / ADR-13, structurally: a built-in match **returns** before the
    /// registry is consulted.
    ///
    /// The fixture is a daemon that offered four reserved names *unshadowed*,
    /// which is the only fixture that can tell the structural claim from a
    /// claim about the daemon's diligence: against a correctly-shadowed
    /// registry, consulting the snapshot first would pass every assertion
    /// anyone would think to write. Move the `registry.dispatchable` lookup
    /// above `split_name` in `classify` and this is the test that goes red.
    #[test]
    fn a_built_in_row_is_matched_before_the_registry_is_consulted() {
        let hostile = offered_unshadowed(&["cost", "exit", "provider", "teton", "model", "help"]);

        // A row name, an alias, and a row whose two words beat the one-word
        // skill that shares its first word.
        assert_eq!(
            classify("/cost", &hostile),
            Input::Command {
                name: "cost",
                args: ""
            }
        );
        assert_eq!(
            classify("/exit", &hostile),
            Input::Command {
                name: "quit",
                args: ""
            }
        );
        assert_eq!(
            classify("/provider list", &hostile),
            Input::Command {
                name: "provider list",
                args: ""
            }
        );
        assert_eq!(
            classify("/model set qwen", &hostile),
            Input::Command {
                name: "model set",
                args: "qwen"
            }
        );
        assert_eq!(
            classify("/help", &hostile),
            Input::Command {
                name: "help",
                args: ""
            }
        );
        // `teton` is claimed one step earlier still — by `cli_line`, before the
        // table is consulted at all (REQ-582).
        let Input::CliLine { name, .. } = classify("teton provider list", &hostile) else {
            panic!("a typed `teton provider list` stopped being recognized");
        };
        assert_eq!(name, "provider list");
        assert!(matches!(classify("/teton", &hostile), Input::CliRefused(_)));
    }

    /// AC-2, with the fixture the daemon actually produces: four skills named
    /// for reserved spellings, each marked shadowed.
    ///
    /// They are **listed** — BR-3 says `/help` shows what the registry holds —
    /// and none of them dispatches. The four lines the AC names are asserted to
    /// be what they are today, which is the whole of "byte-identical".
    #[test]
    fn a_skill_with_a_reserved_name_is_listed_shadowed_and_never_dispatches() {
        // **Unshadowed**, which is the only honest fixture: the daemon cannot
        // mark these. `tetond` does not depend on `teton`, so it has no
        // `COMMANDS` to compare a name against and it deliberately leaves the
        // reserved case to this crate (`skills::mod`'s header says so). A
        // fixture that arrived pre-marked would be testing the daemon's
        // diligence about a judgement the daemon never makes, and every
        // assertion below would pass with the whole reserved set deleted.
        let snapshot = offered_unshadowed(&["cost", "exit", "provider", "teton"]);

        for line in ["/cost", "/exit", "/provider list", "/teton"] {
            assert_eq!(
                classify(line, &snapshot),
                classify(line, &no_skills()),
                "`{line}` classified differently once a same-named skill existed"
            );
        }
        let Input::CliLine { name, .. } = classify("teton provider list", &snapshot) else {
            panic!("a typed `teton provider list` stopped being recognized");
        };
        assert_eq!(name, "provider list");

        // A **family word** is the case no row spelling covers: nothing is
        // spelled bare `provider`, so `/provider foo` reaches neither
        // `split_name`'s longest match nor `builtin_row`, and without the
        // reserved set it would land on the skill while `/provider list` stayed
        // with the table — one spelling reaching two handlers.
        assert!(
            !matches!(classify("/provider foo", &snapshot), Input::Skill { .. }),
            "`/provider foo` reached a skill named after a family word",
        );

        // Listed, and every one of them marked — from the same predicate that
        // refused to dispatch them, so `/help` cannot promise what `classify`
        // declines (BR-3).
        let rows = skill_rows(&help_lines(&snapshot));
        assert_eq!(rows.len(), 4, "{rows:#?}");
        for row in &rows {
            assert!(row.contains("shadowed by"), "{row}");
        }
        assert!(
            rows.iter()
                .any(|row| row.contains("the `/provider` commands")),
            "a family word's mark did not name the family: {rows:#?}",
        );
        assert!(
            rows.iter()
                .any(|row| row.contains("the `teton` command line")),
            "`teton`'s mark did not name why it is unreachable: {rows:#?}",
        );
    }

    /// The wire's reserved list and this crate's derivation are the same set.
    ///
    /// `tetond` cannot read `COMMANDS`, so BR-2's reserved half is enforced
    /// daemon-side from `teton_protocol::methods::RESERVED_SKILL_NAMES` — a
    /// list. This is what keeps the list honest: add a row to `COMMANDS`
    /// without adding its spelling there and this fails, in the crate that owns
    /// the row. Without it the daemon's copy is exactly the hand-written second
    /// home LESSON-546 is about.
    #[test]
    fn the_wire_reserved_list_is_this_crates_derivation() {
        use std::collections::BTreeSet;

        // Only the spellings a skill *could* have. A skill name is
        // `^[a-z0-9][a-z0-9_-]{0,63}$`, so `boundary add` can never collide
        // with one — the daemon has nothing to defend against there, and
        // listing it on the wire would reserve a name nobody can type.
        let derived: BTreeSet<&str> = reserved_names()
            .into_iter()
            .filter(|name| {
                let mut chars = name.chars();
                chars
                    .next()
                    .is_some_and(|c| c.is_ascii_lowercase() || c.is_ascii_digit())
                    && name.chars().all(|c| {
                        c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-' || c == '_'
                    })
            })
            .collect();
        let on_the_wire: BTreeSet<&str> = teton_protocol::methods::RESERVED_SKILL_NAMES
            .iter()
            .copied()
            .collect();

        let missing: Vec<_> = derived.difference(&on_the_wire).collect();
        assert!(
            missing.is_empty(),
            "the table claims names the daemon would let a skill take: {missing:?}"
        );
        let extra: Vec<_> = on_the_wire.difference(&derived).collect();
        assert!(
            extra.is_empty(),
            "the wire reserves names no row claims, so a legal skill name is \
             refused for no reason: {extra:?}"
        );
    }

    /// LESSON-546: BR-2's reserved set is **derived**, and the derivation is
    /// tested against what `classify` does — in both directions.
    ///
    /// Direction one is that nothing the table claims is missing from the set:
    /// every spelling of every row, the first word of every row, and `teton`.
    /// Replace the derivation with a hand-written list and the first row added
    /// to `COMMANDS` afterwards fails here — which is the failure the grep in a
    /// task file could not produce.
    ///
    /// Direction two is that nothing is in the set without the table putting it
    /// there, and then that the set says what the classifier *does*: offered a
    /// skill by a **row spelling** it should have shadowed, `classify` still
    /// returns the row. The family words (`provider`, `web`, `boundary`,
    /// `policy`) and `teton` are reserved for the other half of BR-2 — they are
    /// names the table claims without spelling as a row — and what is asserted
    /// for them is the thing reserving them protects: the family's rows still
    /// win their lines. That scoping is deliberate and is the honest reading of
    /// what this crate enforces; the client does not depend on it, because a
    /// bare `/provider` is not a line any built-in answers.
    #[test]
    fn the_reserved_set_is_derived_from_the_table_and_says_what_classify_does() {
        let reserved = reserved_names();

        for spec in COMMANDS {
            for spelling in spec.spellings() {
                assert!(
                    reserved.contains(spelling),
                    "`{spelling}` dispatches but is not reserved",
                );
                assert!(
                    reserved.contains(first_word(spelling)),
                    "`{}` opens a row and is not reserved",
                    first_word(spelling),
                );
            }
        }
        assert!(reserved.contains(TETON), "`teton` is claimed by `cli_line`");

        for name in &reserved {
            let justified = *name == TETON
                || COMMANDS.iter().any(|spec| {
                    spec.spellings()
                        .any(|spelling| spelling == *name || first_word(spelling) == *name)
                });
            assert!(justified, "`{name}` is reserved and no table row says why");
        }

        for name in &reserved {
            let hostile = offered_unshadowed(&[name]);
            let spelled_by_a_row = COMMANDS
                .iter()
                .any(|spec| spec.spellings().any(|spelling| spelling == *name));
            if spelled_by_a_row {
                let line = format!("/{name}");
                assert!(
                    !matches!(classify(&line, &hostile), Input::Skill { .. }),
                    "`{line}` reached a skill even though the table spells the name",
                );
            } else {
                // A family word, or `teton`. Reserving it protects two things:
                // the family's own rows, and the family word itself — a bare
                // `/<word> …` must not reach a skill either, which is the half
                // no row spelling can enforce.
                let bare = format!("/{name} anything");
                assert!(
                    !matches!(classify(&bare, &hostile), Input::Skill { .. }),
                    "`{bare}` reached a skill even though `{name}` is reserved",
                );
                for spec in COMMANDS {
                    if first_word(spec.name) != *name || spec.name == *name {
                        continue;
                    }
                    let line = format!("/{}", spec.name);
                    assert_eq!(
                        classify(&line, &hostile),
                        Input::Command {
                            name: spec.name,
                            args: ""
                        },
                        "a skill named `{name}` took `{line}` from its family",
                    );
                }
            }
        }
    }

    /// AC-1: the section's shape, in full, for the fixture the AC describes —
    /// a user `skills/alpha`, a user `commands/beta`, a project
    /// `.claude/skills/gamma`.
    ///
    /// Pinned as literal lines rather than as `contains` checks: the row format
    /// (`/name [hint] — description (source)`) and the diagnostic line are what
    /// a user reads, and a `contains` would survive losing the hint, the source
    /// or the em-dash.
    #[test]
    fn help_lists_every_skill_with_its_hint_source_and_description() {
        let snapshot = registry(
            vec![
                described(
                    "alpha",
                    SkillSource::User,
                    "[topic]",
                    "Draft a release note.",
                ),
                described("beta", SkillSource::User, "", "Summarize the diff."),
                described(
                    "gamma",
                    SkillSource::Project,
                    "<path>",
                    "Check the working tree.",
                ),
            ],
            Vec::new(),
        );
        let lines = help_lines(&snapshot);
        let start = lines
            .iter()
            .position(|line| line == SKILLS_HEADER)
            .expect("the skills header");

        assert_eq!(
            &lines[start..start + 5],
            &[
                SKILLS_HEADER.to_owned(),
                "/alpha [topic] — Draft a release note. (user)".to_owned(),
                "/beta — Summarize the diff. (user)".to_owned(),
                "/gamma <path> — Check the working tree. (project)".to_owned(),
                "3 skills (user 2, project 1); 0 skipped".to_owned(),
            ],
        );
        // AC-1's order: the built-in rows above, then a blank, then the
        // section, then a blank, then the two footers — in that order and with
        // nothing between the diagnostic line and the footers but the blank.
        assert_eq!(lines[start - 1], "", "{lines:#?}");
        assert_eq!(lines[start + 5], "", "{lines:#?}");
        assert_eq!(lines[start + 6], ARGUMENT_FOOTER);
        assert_eq!(lines[start + 7], ESCAPE_FOOTER);
        assert_eq!(lines.len(), start + 8, "{lines:#?}");
    }

    /// **REQ-587 AC-12 / BR-3: a row the user may not type is marked in the
    /// source parenthetical, and the two reasons are worded apart.**
    ///
    /// Four rows in one fixture, because BR-3's states are only distinguishable
    /// side by side: an ordinary row; a `disable-model-invocation: true` row,
    /// which is an **ordinary** row to this surface (that flag is about the
    /// model, and a mark here would tell the user they cannot type a name they
    /// can); a model-only row; and the row both flags deny, which is listed,
    /// invocable by nobody, and rendered nowhere else — `classify` refuses it
    /// like any other model-only name and the daemon's roster simply omits it,
    /// so `/help` is the only surface that can name the combination at all.
    ///
    /// Pinned as literal lines, for `help_lists_every_skill_…`'s reason: the
    /// mark rides inside the parentheses the source already owns, and a
    /// `contains` would survive it drifting into an em-dash aside of its own or
    /// losing the source word beside it.
    #[test]
    fn help_marks_the_rows_the_user_may_not_type_and_words_the_two_reasons_apart() {
        let snapshot = registry(
            vec![
                skill("alpha", SkillSource::User),
                user_only("beta", SkillSource::User),
                model_only("delta", SkillSource::Project),
                invocable_by_nobody("mute", SkillSource::User),
            ],
            Vec::new(),
        );
        let lines = help_lines(&snapshot);

        assert_eq!(
            skill_rows(&lines),
            vec![
                "/alpha (user)".to_owned(),
                "/beta (user)".to_owned(),
                "/delta (project, model-only)".to_owned(),
                "/mute (user, invocable by nobody)".to_owned(),
            ],
        );
        // Listed **and counted**: a row nobody may invoke is a named diagnostic
        // rather than a silent drop, so it is in the totals like every other
        // registered row — it was never skipped.
        assert!(
            lines
                .iter()
                .any(|line| line == "4 skills (user 3, project 1); 0 skipped"),
            "{lines:#?}"
        );
        // And the mark is the dispatcher's own answer, read from the other end:
        // exactly the unmarked rows classify as skills.
        for (name, dispatches) in [
            ("/alpha", true),
            ("/beta", true),
            ("/delta", false),
            ("/mute", false),
        ] {
            assert_eq!(
                matches!(classify(name, &snapshot), Input::Skill { .. }),
                dispatches,
                "`{name}`"
            );
        }
    }

    /// **TASK-212's precedence, at the surface that renders it: shadowing
    /// wins.**
    ///
    /// A row that is both shadowed and model-only reads `shadowed by …` and
    /// never `model-only` — the name belongs to another file entirely, so
    /// "model-only" would name a capability *this* row does not have either, and
    /// would point the reader at their own frontmatter for a collision it does
    /// not explain.
    ///
    /// **Mutation.** Render the mark from a re-derived predicate —
    /// `if !view.user_invocable { "model-only" }` at [`skill_row`], or a
    /// `shadow_reason` that folded the flag in — and both rows here change.
    /// Both shadowing sources are covered, because they arrive by different
    /// routes: the daemon marks what it can see, and the client's own table
    /// claims what `tetond` has no `COMMANDS` to know about.
    #[test]
    fn shadowing_wins_over_model_only_in_the_mark() {
        let by_daemon = SkillView {
            shadowed: Some("the project skill".to_owned()),
            ..model_only("analyze", SkillSource::User)
        };
        // Unmarked by the daemon, and claimed by this crate's table: the skew
        // case ADR-2 describes, where the client is the one that is right.
        let by_table = model_only("cost", SkillSource::User);
        let snapshot = registry(vec![by_daemon, by_table], Vec::new());

        assert_eq!(
            skill_rows(&help_lines(&snapshot)),
            vec![
                "/analyze (user, shadowed by the project skill)".to_owned(),
                "/cost (user, shadowed by the built-in `/cost`)".to_owned(),
            ],
        );
        // The refusal follows the same order: neither name is answered with the
        // flag, because the flag is not why the user cannot reach the file.
        assert_eq!(model_only_hint("analyze", &snapshot), None);
        assert_eq!(model_only_hint("cost", &snapshot), None);
    }

    /// **The mirror's contract, enumerated: all eight combinations of the three
    /// facts a dispatch answer is drawn from.**
    ///
    /// [`user_dispatch`] is this crate's copy of `tetond`'s
    /// `Skill::user_dispatch` (`crates/tetond/src/skills/mod.rs`) — the same
    /// three-valued answer, over the same two facts, in the same order:
    /// **shadowing wins over model-only**. It cannot be one shared function.
    /// `Skill` is the daemon's type, holds a `PathBuf` into the daemon's world
    /// and never crosses the wire; the two facts ride `SkillView` *separately*
    /// exactly so that a client composes them rather than re-parsing a
    /// pre-composed sentence (LESSON-529). For the same reason this is not a
    /// bridge test: nothing in this crate can construct a `Skill` to ask it the
    /// same question.
    ///
    /// So the rule is written down instead. Every row's answer is spelled out
    /// rather than computed, so a one-sided change to the *rule* — swapping the
    /// two arms, folding a third fact in — reddens here while the daemon's own
    /// unit suite stays green. That is LESSON-528's shape, met as far as one
    /// side can meet it.
    ///
    /// `model_invocable` appears in the table and in **no** expected value: it
    /// answers the model's question, not the user's, and a `user_dispatch` that
    /// consulted it would make one of each pair of rows disagree with the other.
    ///
    /// The last block is the precondition the client **adds**: [`shadow_reason`]
    /// asks this crate's command table before it reads the daemon's mark, so a
    /// name a built-in owns is `Shadowed` here on a row the daemon left
    /// unmarked. `tetond` has no `COMMANDS` to consult and cannot make that
    /// call, which is why the mirror is not — and cannot be — an exact copy.
    #[test]
    fn user_dispatch_answers_all_eight_flag_combinations_as_the_daemons_rule_does() {
        // A name no built-in claims, so the daemon's mark is the only shadowing
        // source in the table below. Asserted rather than assumed: if the table
        // ever claimed this word, every row would pass for the wrong reason.
        const NAME: &str = "zeta";
        const BY: &str = "the project skill";
        assert!(
            table_claim(NAME).is_none(),
            "the command table's own claim would answer every row of the table"
        );

        for (shadowed, user_invocable, model_invocable, expected) in [
            (false, true, true, UserDispatch::Allowed),
            (false, true, false, UserDispatch::Allowed),
            (false, false, true, UserDispatch::ModelOnly),
            (false, false, false, UserDispatch::ModelOnly),
            (true, true, true, UserDispatch::Shadowed(BY.to_owned())),
            (true, true, false, UserDispatch::Shadowed(BY.to_owned())),
            (true, false, true, UserDispatch::Shadowed(BY.to_owned())),
            (true, false, false, UserDispatch::Shadowed(BY.to_owned())),
        ] {
            let view = SkillView {
                shadowed: shadowed.then(|| BY.to_owned()),
                user_invocable,
                model_invocable,
                ..skill(NAME, SkillSource::User)
            };
            assert_eq!(
                user_dispatch(&view),
                expected,
                "shadowed={shadowed}, user_invocable={user_invocable}, \
                 model_invocable={model_invocable}: the daemon's \
                 `Skill::user_dispatch` answers this row the same way",
            );
        }

        // The client's extra precondition, on both the ordinary and the
        // model-only row: the table is consulted first, so the answer is
        // `Shadowed` where the daemon — seeing `shadowed: None` — would say
        // `Allowed` and `ModelOnly`.
        let claim = UserDispatch::Shadowed("the built-in `/cost`".to_owned());
        assert_eq!(
            user_dispatch(&skill("cost", SkillSource::User)),
            claim,
            "a name this crate's table owns is shadowed even when the daemon \
             did not mark it (ADR-2's skew case)",
        );
        assert_eq!(
            user_dispatch(&model_only("cost", SkillSource::User)),
            claim,
            "and the table's claim outranks the flag, in the same order the \
             daemon's copy ranks its own mark",
        );
    }

    /// **AC-12's `/delta` half.** A model-only name does not dispatch, and the
    /// refusal names the line of the user's own file that made it so — the only
    /// actionable fact about a name that is spelled correctly and listed in
    /// `/help` two lines above.
    ///
    /// The two-flag row gets its own sentence for [`dispatch_mark`]'s reason:
    /// telling the author of a `disable-model-invocation: true` file that "only
    /// the model may invoke it" would name a door that file closed.
    #[test]
    fn a_model_only_name_does_not_dispatch_and_the_hint_names_the_flag() {
        let snapshot = registry(
            vec![
                model_only("delta", SkillSource::User),
                invocable_by_nobody("mute", SkillSource::User),
            ],
            Vec::new(),
        );

        assert!(
            matches!(classify("/delta", &snapshot), Input::Command { name, .. } if name == "delta"),
            "a model-only name is still a command line, and still names no row",
        );
        assert_eq!(
            model_only_hint("delta", &snapshot).as_deref(),
            Some(
                "`/delta` is a skill whose frontmatter says `user-invocable: false`, so only \
                 the model may invoke it — type /help for the commands this session knows."
            ),
        );
        assert_eq!(
            model_only_hint("mute", &snapshot).as_deref(),
            Some(
                "`/mute` is a skill whose frontmatter says `user-invocable: false`, so nobody \
                 may invoke it — its frontmatter also says `disable-model-invocation: true` — \
                 type /help for the commands this session knows."
            ),
        );
        // An ordinary row, and a name this session does not have, keep the bytes
        // they have always had: this branch adds a case, it does not reword one.
        assert_eq!(model_only_hint("frobnicate", &snapshot), None);
        assert_eq!(
            model_only_hint(
                "delta",
                &registry(vec![skill("delta", SkillSource::User)], vec![])
            ),
            None,
        );

        // Through `dispatch`, which is where it reaches a user: one line, and
        // no RPC — a refusal this client composes never asks the daemon.
        let (mut conn, peer) = Connection::scripted(&[]);
        let mut surface = RecordingSurface::new();
        let mut state = SessionState::new();
        let mut prompter = ScriptedPrompter::new(&[]);
        {
            let mut ctx = session_ctx(&mut surface, &mut state, &mut prompter);
            let outcome = dispatch("delta", "", &snapshot, &mut conn, &mut ctx)
                .expect("a refusal is not an error");
            assert_eq!(outcome, CommandOutcome::Continue);
        }
        let errors = surface.lines_of(LineKind::Error);
        assert_eq!(errors.len(), 1, "one line, not a paragraph: {errors:?}");
        assert!(
            errors[0].contains("`user-invocable: false`"),
            "{}",
            errors[0]
        );
        assert!(
            !errors[0].contains("unknown command"),
            "the name is known — it is listed in /help: {}",
            errors[0]
        );
        assert!(
            crate::client::methods_written(&peer).is_empty(),
            "a client-side refusal issued an RPC"
        );
    }

    /// BR-1's "counted and named": the diagnostic line names what discovery
    /// dropped and why, and it renders even when nothing registered — a user
    /// whose only skill file is broken has to be able to see that.
    #[test]
    fn the_diagnostic_line_names_every_skipped_entry() {
        let snapshot = registry(
            Vec::new(),
            vec![
                SkillSkipped {
                    path: "~/.claude/skills/broken/SKILL.md".to_owned(),
                    name: "broken".to_owned(),
                    reason: "malformed frontmatter".to_owned(),
                },
                SkillSkipped {
                    path: "~/.claude/commands/huge.md".to_owned(),
                    name: "huge".to_owned(),
                    reason: "over 128 KiB (135,184 B)".to_owned(),
                },
            ],
        );
        let lines = help_lines(&snapshot);
        assert!(lines.iter().any(|line| line == SKILLS_HEADER), "{lines:#?}");
        assert!(
            lines.iter().any(|line| line
                == "0 skills (user 0, project 0); 2 skipped: \
                    ~/.claude/skills/broken/SKILL.md — malformed frontmatter; \
                    ~/.claude/commands/huge.md — over 128 KiB (135,184 B)"),
            "{lines:#?}"
        );
    }

    /// BR-3, both directions, scoped to **unmarked** rows (LESSON-524: the
    /// likely repair for a red both-directions pin is to relax it rather than
    /// to scope it, so the scope is written into the test's name and its
    /// comment rather than discovered later).
    ///
    /// Forward: every name `classify` returns `Input::Skill` for appears as a
    /// `/help` row. Backward: every `/help` skill row that carries **no mark**
    /// classifies as `Input::Skill`. The unqualified backward claim is false for
    /// the shadowed rows AC-2 mandates and for REQ-587's model-only ones — they
    /// are listed and do not dispatch, which is the point of listing them — and
    /// BR-3's claim is about dispatchable skills.
    ///
    /// "Marked" is read off the **rendered** row as "the source parenthetical
    /// carries more than the source", not as a search for today's two mark
    /// spellings: a third mark added later inherits this pin instead of
    /// silently escaping it.
    #[test]
    fn every_dispatchable_skill_is_a_help_row_and_every_unmarked_row_dispatches() {
        let snapshot = registry(
            vec![
                described("alpha", SkillSource::User, "[topic]", "Draft a note."),
                skill("beta", SkillSource::User),
                shadowed("cost", SkillSource::User, "the built-in `/cost`"),
                shadowed("analyze", SkillSource::User, "the project skill"),
                model_only("delta", SkillSource::User),
                invocable_by_nobody("mute", SkillSource::Project),
                skill("gamma", SkillSource::Project),
            ],
            Vec::new(),
        );
        let unmarked = |row: &str| row.ends_with("(user)") || row.ends_with("(project)");
        let lines = help_lines(&snapshot);
        let rows = skill_rows(&lines);
        assert_eq!(rows.len(), 7, "every registered skill is listed: {rows:#?}");

        // Forward, and it is the **unmarked** row that has to be there: a name
        // that dispatches while `/help` marks it is the same disagreement read
        // from the other end, and a bare "a row exists" check would pass it (a
        // shadowed skill made dispatchable keeps its row and only loses its
        // mark).
        for view in &snapshot.skills {
            let line = format!("/{}", view.name);
            if matches!(classify(&line, &snapshot), Input::Skill { .. }) {
                let row = rows
                    .iter()
                    .find(|row| row.starts_with(&line))
                    .unwrap_or_else(|| {
                        panic!("`{line}` dispatches and is not in /help: {rows:#?}")
                    });
                assert!(
                    unmarked(row),
                    "`{line}` dispatches and /help marks it not dispatchable: {row}",
                );
            }
        }

        // Backward, over the rendered rows rather than over the fixture: the
        // section is read back the way a user reads it.
        let mut plain = 0;
        for row in &rows {
            if !unmarked(row) {
                continue;
            }
            plain += 1;
            let name = row
                .split_whitespace()
                .next()
                .expect("a row names a skill")
                .to_owned();
            assert!(
                matches!(classify(&name, &snapshot), Input::Skill { .. }),
                "/help lists `{name}` as dispatchable and it does not dispatch",
            );
        }
        assert_eq!(plain, 3, "the fixture must exercise both sides");
    }

    /// ADR-12: `help_family` never sees a skill.
    ///
    /// BR-2's reserved set stops a skill named `provider` from *dispatching*;
    /// only a section of its own stops it from re-grouping the four built-in
    /// `/provider` rows. Asserted as byte-equality of the built-in half — the
    /// grouping is blank lines, so a re-grouping shows up as moved blanks and
    /// nothing else, which a `contains` check cannot see.
    #[test]
    fn a_skill_never_regroups_the_built_in_rows() {
        let baseline = help_lines(&no_skills());
        let with_families = help_lines(&registry(
            vec![
                skill("provider", SkillSource::User),
                skill("model", SkillSource::User),
                skill("web", SkillSource::Project),
                skill("doctor", SkillSource::User),
            ],
            Vec::new(),
        ));
        let built_in: Vec<&String> = with_families
            .iter()
            .take_while(|line| *line != SKILLS_HEADER)
            .collect();
        // The section's own leading blank is the last of those, and the
        // baseline's is the blank before the footers.
        assert_eq!(
            built_in,
            baseline.iter().take(built_in.len()).collect::<Vec<_>>(),
            "a skill re-grouped the built-in listing",
        );
        // And `help_family` is typed so it cannot be handed one: its argument
        // is `&'static str`, which a registry name is not.
        assert_eq!(help_family("provider list"), "provider");
        assert_eq!(help_family("doctor"), "");
    }

    /// ADR-12: `ARGUMENT_FOOTER` is qualified to name the rows it describes,
    /// and qualified by **appending**.
    ///
    /// Two assertions, and the first is about a file this task does not own:
    /// `cli_e2e` pins the original clause as a substring, so a qualification
    /// that rewrote the subject (`Built-in command arguments…`) would go green
    /// here and red there. Drop the appended clause and the second assertion
    /// fails — the footer would then sit two lines from the skill rows it
    /// contradicts (BR-4 hands a skill its line as typed).
    #[test]
    fn the_argument_footer_is_qualified_by_appending() {
        assert!(
            ARGUMENT_FOOTER.starts_with(
                "Command arguments are split on whitespace and quotes are not \
                     interpreted"
            ),
            "the qualification rewrote the subject; `cli_e2e` pins this clause: {ARGUMENT_FOOTER}",
        );
        assert!(
            ARGUMENT_FOOTER.contains("built-in rows") && ARGUMENT_FOOTER.contains("skill row"),
            "the footer does not say which rows it is true of: {ARGUMENT_FOOTER}",
        );
        // And the positive statement is the section header's, at the top of the
        // rows it is true of, so the two never sit adjacent and disagree.
        assert!(SKILLS_HEADER.contains("as typed"), "{SKILLS_HEADER}");
    }

    /// AC-17: a name discovery **skipped** says why; a name with no entry at
    /// all gets the pre-REQ bytes.
    ///
    /// The second half is the one that has to be pinned as an equality: the
    /// unknown-command line is what every user of every build has seen, and the
    /// new case is allowed to add a case, not to reword the old one.
    #[test]
    fn a_skipped_name_says_why_and_an_unknown_one_says_what_it_always_did() {
        let snapshot = registry(
            Vec::new(),
            vec![SkillSkipped {
                path: "~/.claude/skills/analyze/SKILL.md".to_owned(),
                name: "analyze".to_owned(),
                reason: "malformed frontmatter".to_owned(),
            }],
        );
        assert_eq!(
            skipped_skill_hint("analyze", &snapshot).as_deref(),
            Some(
                "`/analyze` is a skill that was skipped: malformed frontmatter — \
                 type /help for the commands this session knows."
            ),
        );
        // No entry: nothing to say, so `resolve` composes what it always has.
        assert_eq!(skipped_skill_hint("frobnicate", &snapshot), None);
        let Resolution::Rejected(hint) = resolve("frobnicate", "") else {
            panic!("an unknown name resolved to a row");
        };
        assert_eq!(
            hint,
            "unknown command: `/frobnicate` — type /help for the commands this session knows."
        );
        // A name a row owns never reaches the hint at all, even when a file by
        // that name was skipped: the row runs (BR-2).
        let owned = registry(
            Vec::new(),
            vec![SkillSkipped {
                path: "~/.claude/commands/cost.md".to_owned(),
                name: "cost".to_owned(),
                reason: "not UTF-8".to_owned(),
            }],
        );
        assert_eq!(skipped_skill_hint("cost", &owned), None);
    }

    /// A skipped entry is matched by the **name discovery carried**, not by one
    /// this crate read back off the path.
    ///
    /// BR-2's naming rule belongs to discovery, which is the only place that
    /// knows which of the four roots an entry came from. A copy here would be a
    /// second home for it in the crate that cannot see them (LESSON-546), and a
    /// strictly weaker one: the two entries below are the cases a path-reader
    /// gets wrong. A symlinked `commands/status.md` is refused before it is
    /// ever a file that was opened, and an entry skipped *because* its name is
    /// invalid is named by the invalid spelling — which is what the user typed
    /// and what they have to be told about.
    #[test]
    fn a_skipped_entry_is_matched_by_the_name_discovery_carried() {
        let snapshot = registry(
            Vec::new(),
            vec![
                SkillSkipped {
                    path: "~/.claude/commands/status.md".to_owned(),
                    name: "status".to_owned(),
                    reason: "symlink not followed".to_owned(),
                },
                SkillSkipped {
                    path: "~/.claude/skills".to_owned(),
                    name: String::new(),
                    reason: "unreadable (permission denied)".to_owned(),
                },
            ],
        );

        let hint = skipped_skill_hint("status", &snapshot).expect("a skipped name says why");
        assert!(hint.contains("symlink not followed"), "{hint}");

        // A root-level diagnostic names no skill, and the empty name must not
        // become a wildcard that answers for every typed line.
        assert_eq!(skipped_skill_hint("", &snapshot), None);
        assert_eq!(skipped_skill_hint("anything", &snapshot), None);
    }

    /// BR-4: a skill is handed its line **as typed** — interior whitespace runs
    /// and quotes survive, and only the edges are trimmed.
    ///
    /// This is the clause `ARGUMENT_FOOTER`'s qualification exists for: the
    /// built-in rows split on whitespace and interpret no quotes, and a skill
    /// row does neither.
    #[test]
    fn a_skill_is_handed_its_line_as_typed() {
        let snapshot = registry(vec![skill("alpha", SkillSource::User)], Vec::new());
        assert_eq!(
            classify(r#"/alpha teton  code "repo"  "#, &snapshot),
            Input::Skill {
                name: "alpha".to_owned(),
                raw_arguments: r#"teton  code "repo""#.to_owned(),
            }
        );
        assert_eq!(
            classify("/alpha", &snapshot),
            Input::Skill {
                name: "alpha".to_owned(),
                raw_arguments: String::new(),
            }
        );
    }

    /// ADR-13: nothing is leaked to satisfy a lifetime.
    ///
    /// A leaked registry would survive `/cd` and dispatch a skill the session no
    /// longer has, so `Input::Skill` owns its two strings and the classifier's
    /// output lifetime is the *input line's*. The property is a compile-time
    /// one, written as a test because that is where a future edit will look:
    /// the registry is dropped and the classification is still readable.
    #[test]
    fn a_classification_outlives_the_registry_it_came_from() {
        let line = String::from("/alpha take this");
        let classified = {
            let snapshot = registry(vec![skill("alpha", SkillSource::User)], Vec::new());
            classify(&line, &snapshot)
        };
        assert_eq!(
            classified,
            Input::Skill {
                name: "alpha".to_owned(),
                raw_arguments: "take this".to_owned(),
            }
        );
    }

    /// A snapshot is built from the wire result, and an old daemon's answer is
    /// an empty one rather than an error (ADR-2).
    #[test]
    fn a_snapshot_is_the_wire_result_and_the_default_is_empty() {
        assert_eq!(
            SkillSnapshot::from(SkillsListResult::default()),
            SkillSnapshot::empty()
        );
        assert!(SkillSnapshot::empty().is_empty());
        assert!(SkillSnapshot::default().is_empty());
        let one = registry(vec![skill("alpha", SkillSource::User)], Vec::new());
        assert!(!one.is_empty());
        assert_eq!(one.source_counts(), (1, 0));
        assert!(one.dispatchable("alpha").is_some());
        assert!(one.dispatchable("beta").is_none());
        // A shadowed entry is listed and never looked up.
        let marked = registry(
            vec![shadowed("alpha", SkillSource::User, "the project skill")],
            Vec::new(),
        );
        assert!(marked.dispatchable("alpha").is_none());
        assert_eq!(marked.skills.len(), 1);
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
        let Input::Command { name, args } = classify("/frobnicate", &no_skills()) else {
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
        let Input::Command { name, args } = classify("/", &no_skills()) else {
            panic!("a bare `/` did not classify as a command");
        };
        assert!(matches!(resolve(name, args), Resolution::Rejected(_)));
    }

    // A command that takes no argument says so rather than ignoring it.
    #[test]
    fn a_trailing_argument_to_an_arg_less_command_is_rejected() {
        let Input::Command { name, args } = classify("/help extra", &no_skills()) else {
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
            let Input::Command { name, args } = classify(typed, &no_skills()) else {
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
        let Input::Command { name, args } = classify("/model set qwen2.5-coder-3b", &no_skills())
        else {
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
        let Input::Command { name, args } = classify("/model set", &no_skills()) else {
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
        let Input::Command { name, args } = classify("/model extra", &no_skills()) else {
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
        render_help(&mut surface, &no_skills());
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
        render_help(&mut surface, &no_skills());
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
        render_help(&mut surface, &no_skills());
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
            let Input::Command { name, args } = classify(line, &no_skills()) else {
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
        render_help(&mut surface, &no_skills());
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
        let Input::Command { name, args } = classify("/provider test kimi", &no_skills()) else {
            panic!("`/provider test kimi` did not classify as a command");
        };
        assert_eq!((name, args), ("provider test", "kimi"));
        assert!(matches!(resolve(name, args), Resolution::Run(_, "kimi")));

        // The bare form is rejected at `resolve`, so no RPC is issued and the
        // hint says what to type.
        let Input::Command { name, args } = classify("/provider test", &no_skills()) else {
            panic!("`/provider test` did not classify as a command");
        };
        let Resolution::Rejected(hint) = resolve(name, args) else {
            panic!("a bare `/provider test` reached the handler");
        };
        assert!(hint.contains("/provider test <id>"), "{hint}");

        // The two rows are distinct: `/provider setup kimi` must never be read
        // as the test row, nor the reverse.
        let Input::Command { name, .. } = classify("/provider setup kimi", &no_skills()) else {
            panic!("`/provider setup kimi` did not classify as a command");
        };
        assert_eq!(name, "provider setup");

        assert!(PROVIDER_TEST_USAGE.contains("/provider test kimi"));
        assert!(PROVIDER_TEST_USAGE.contains("Nothing was sent"));
    }

    /// The handler's own argument rule, exercised rather than re-derived.
    ///
    /// One id is taken; nothing and two words are both refused, and the refusal
    /// is the same because the answer is: this command spends, so it may not
    /// guess which of two ids was meant, and it may not test "whatever was
    /// registered first" for a line that named none.
    #[test]
    fn provider_test_takes_one_id_and_refuses_none_or_two() {
        for (args, expected) in [
            ("kimi", Some("kimi")),
            ("  kimi  ", Some("kimi")),
            ("deepseek", Some("deepseek")),
            ("", None),
            ("   ", None),
            ("kimi deepseek", None),
            ("kimi --yes", None),
            ("kimi deepseek anthropic", None),
        ] {
            assert_eq!(
                provider_test_id(args),
                expected,
                "`/provider test {args}` parsed wrongly"
            );
        }
    }

    /// The longest-match rule keeps the third `/web` row apart from the other
    /// two, and the row takes no argument — `/web setup search` is a typo, not a
    /// way to skip the menu, and answering it as one would set a tier from a
    /// line nobody previewed.
    #[test]
    fn the_setup_command_parses_and_takes_no_arguments() {
        let Input::Command { name, args } = classify("/web setup", &no_skills()) else {
            panic!("`/web setup` did not classify as a command");
        };
        assert_eq!((name, args), ("web setup", ""));
        assert!(matches!(resolve(name, args), Resolution::Run(_, "")));

        let Input::Command { name, args } = classify("/web setup search", &no_skills()) else {
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
        let Input::Command { name, args } = classify("/web allow", &no_skills()) else {
            panic!("`/web allow` did not classify as a command");
        };
        assert_eq!((name, args), ("web allow", ""));
        assert!(matches!(resolve(name, args), Resolution::Run(_, "")));

        // `/web allow` takes nothing: a trailing word is a typo, not a tier.
        let Input::Command { name, args } = classify("/web allow search", &no_skills()) else {
            panic!("did not classify as a command");
        };
        let Resolution::Rejected(hint) = resolve(name, args) else {
            panic!("`/web allow search` reached the handler");
        };
        assert!(hint.contains("takes no arguments"), "{hint}");

        // The URL reaches the handler verbatim — the daemon owns URL
        // normalization, so nothing here second-guesses it.
        let Input::Command { name, args } =
            classify("/web refresh https://docs.rs/serde", &no_skills())
        else {
            panic!("did not classify as a command");
        };
        assert_eq!((name, args), ("web refresh", "https://docs.rs/serde"));
        let Resolution::Run(spec, run_args) = resolve(name, args) else {
            panic!("a well-formed `/web refresh` was rejected");
        };
        assert_eq!(spec.name, "web refresh");
        assert_eq!(run_args, "https://docs.rs/serde");

        // A bare refresh gets the usage line, not an RPC with an empty URL.
        let Input::Command { name, args } = classify("/web refresh", &no_skills()) else {
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
        let Input::Command { name, args } = classify("/web", &no_skills()) else {
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
            let Input::Command { name, args } = classify(typed, &no_skills()) else {
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

    // ------------------------------------------------------------------
    // REQ-583: `/cd` (BR-7, AC-10's rendering, AC-12)
    // ------------------------------------------------------------------

    fn a_root(kind: teton_protocol::methods::RootKind, display: &str) -> SessionRoot {
        use teton_protocol::methods::RootKind;
        SessionRoot {
            display: display.to_owned(),
            kind,
            project_name: (kind == RootKind::Project).then(|| "teton-code".to_owned()),
            vcs_branch: (kind == RootKind::Project).then(|| "main".to_owned()),
        }
    }

    /// The row is in the table, beside `/clear`, optional-argument, session-only
    /// (no mirror — its shell twin is a flag), and `/help` lists it with a
    /// summary that says both what it does and that it clears.
    #[test]
    fn cd_is_a_session_only_optional_argument_row_beside_clear() {
        let cd = COMMANDS
            .iter()
            .find(|spec| spec.name == "cd")
            .expect("the /cd row");
        assert!(matches!(cd.args, Args::Optional), "the bare form is a read");
        assert!(cd.mirror.is_none(), "`teton cd` is not a subcommand");
        assert!(cd.aliases.is_empty());
        assert!(cd.summary.contains("session's root"), "{}", cd.summary);
        assert!(
            cd.summary.contains("clears the conversation"),
            "{}",
            cd.summary
        );
        let names: Vec<&str> = COMMANDS.iter().map(|spec| spec.name).collect();
        let clear = names.iter().position(|n| *n == "clear").unwrap();
        assert_eq!(names[clear + 1], "cd", "beside /clear: {names:?}");

        let mut surface = RecordingSurface::new();
        render_help(&mut surface, &no_skills());
        assert!(
            surface
                .lines_of(LineKind::Info)
                .iter()
                .any(|line| line.starts_with("/cd ") && line.contains(cd.summary)),
            "/cd must reach /help through the table"
        );
        // A typed `teton cd …` is not a recognized CLI line: there is no such
        // subcommand, so it is a prompt (BR-4's rule), never a dispatch.
        assert!(!matches!(
            classify("teton cd ~", &no_skills()),
            Input::CliLine { .. }
        ));
    }

    /// AC-12, the `/cd` half: the argument goes through the very grammar table
    /// `--cwd`'s test in `main.rs` and teton-core's own test iterate. Every
    /// spelling the table resolves, `/cd` resolves to the same path; the
    /// spellings the table refuses are the *empty* ones, and for `/cd` an empty
    /// argument is the bare form — a read, not a refusal — because unlike a
    /// flag it has something useful to do with no path.
    #[test]
    fn cd_arguments_obey_the_shared_grammar_table() {
        use std::path::Path;
        use teton_core::session_root::{
            CWD_ARGUMENT_GRAMMAR, CWD_GRAMMAR_HOME, CWD_GRAMMAR_SHELL_CWD,
        };
        let shell_cwd = Path::new(CWD_GRAMMAR_SHELL_CWD);
        let home = Some(Path::new(CWD_GRAMMAR_HOME));
        for row in CWD_ARGUMENT_GRAMMAR {
            let typed = format!("/cd {}", row.raw);
            let Input::Command { name, args } = classify(&typed, &no_skills()) else {
                panic!("`{typed}` must be a command line");
            };
            let Resolution::Run(spec, args) = resolve(name, args) else {
                panic!("`{typed}` must dispatch to the row, whatever the argument");
            };
            assert_eq!(spec.name, "cd");
            match row.expect {
                Ok(path) => {
                    assert_eq!(
                        args,
                        row.raw.trim(),
                        "the argument reaches the handler as typed"
                    );
                    assert_eq!(
                        resolve_cwd_argument(args, shell_cwd, home).as_deref(),
                        Ok(Path::new(path)),
                        "/cd {:?} must resolve to {path}, as --cwd does",
                        row.raw
                    );
                }
                Err(fragment) => {
                    // The table refuses only the empty spellings; the same
                    // function says so for `/cd`, and the handler's bare-form
                    // check is the same emptiness test.
                    assert!(args.trim().is_empty(), "row {:?} → {args:?}", row.raw);
                    let err = resolve_cwd_argument(args, shell_cwd, home).unwrap_err();
                    assert!(err.to_string().contains(fragment), "{err}");
                    assert!(
                        cd_argument_refusal(&err).contains(fragment),
                        "the refusal carries the grammar's own sentence"
                    );
                }
            }
        }
    }

    /// A refused argument is one error line naming the argument, and no RPC:
    /// the refusal sentence is the grammar's own (it names what was typed) and
    /// says what `/cd` takes.
    #[test]
    fn a_cd_argument_that_is_no_path_is_one_error_line_naming_it() {
        use std::path::Path;
        let err = resolve_cwd_argument("~/x", Path::new("/work"), None).unwrap_err();
        let line = cd_argument_refusal(&err);
        assert!(line.starts_with("/cd: "), "{line}");
        assert!(line.contains("`~/x`"), "{line}");
        assert!(line.contains("HOME is not set"), "{line}");
        assert!(line.contains("`/cd <path>`"), "{line}");
        assert_eq!(line.lines().count(), 1);
    }

    /// **BR-7: the bare form.** `/cd` alone prints the current root and kind from
    /// the cache the daemon fills — one info line in the one spelling every
    /// surface uses — and, when no daemon has described the root, one notice
    /// saying so rather than a panic or a guess (an older daemon omits
    /// `SessionCreateResult.root`).
    #[test]
    fn a_bare_cd_prints_the_current_root_or_says_it_is_unknown() {
        use teton_protocol::methods::RootKind;
        let (kind, line) = current_root_line(Some(&a_root(
            RootKind::Project,
            "~/Documents/GitHub/teton-code",
        )));
        assert_eq!(kind, LineKind::Info);
        assert_eq!(
            line,
            "session root: ~/Documents/GitHub/teton-code (project teton-code, branch main)"
        );
        let (kind, line) = current_root_line(Some(&a_root(RootKind::Home, "~")));
        assert_eq!(kind, LineKind::Info);
        assert_eq!(line, "session root: ~ (your home folder)");
        let (kind, line) = current_root_line(Some(&a_root(RootKind::FilesystemRoot, "/")));
        assert_eq!(
            (kind, line.as_str()),
            (LineKind::Info, "session root: / (the filesystem root)")
        );
        let (kind, line) = current_root_line(Some(&a_root(RootKind::Plain, "/opt/x")));
        assert_eq!(
            (kind, line.as_str()),
            (LineKind::Info, "session root: /opt/x (not a project)")
        );

        let (kind, line) = current_root_line(None);
        assert_eq!(kind, LineKind::Notice);
        assert_eq!(line, CD_ROOT_UNKNOWN);
        assert!(line.contains("not known"), "{line}");
        assert_eq!(line.lines().count(), 1);

        // Through the handler: the bare form reads the cache and touches no
        // socket — a `Connection` is never needed for it, which is why the
        // dispatch can be exercised here through `resolve` alone.
        let Input::Command { name, args } = classify("/cd", &no_skills()) else {
            panic!("`/cd` must be a command line");
        };
        assert!(matches!(resolve(name, args), Resolution::Run(spec, "") if spec.name == "cd"));
        let mut state = SessionState::new();
        state.root = Some(a_root(RootKind::Home, "~"));
        assert_eq!(
            current_root_line(state.root.as_ref()).1,
            "session root: ~ (your home folder)"
        );
    }

    /// **The render decision (BR-7's refusal half).** A `/cd` that worked prints
    /// nothing here — `context_cleared` then `session_root_changed` draw the
    /// lines on every attached client — so this pins the arms that *do* render,
    /// and their classes, in `report_clear_refusal`'s shape: a build without the
    /// method and a busy session are notices; a refused path is an error line
    /// carrying the daemon's reason, which names the path (BR-6).
    #[test]
    fn a_cd_refusal_renders_by_class_and_a_worked_cd_renders_nothing_here() {
        let mut surface = RecordingSurface::new();
        report_cd_refusal(
            &RpcError::new(error_code::METHOD_NOT_FOUND, "no such method"),
            &mut surface,
        );
        let notice = surface.lines_of(LineKind::Notice).join("\n");
        assert!(notice.contains("cannot move a session root"), "{notice}");
        assert!(notice.contains("--cwd"), "the remedy is named: {notice}");
        assert!(
            surface.lines_of(LineKind::Error).is_empty(),
            "{:?}",
            surface.calls
        );

        let mut surface = RecordingSurface::new();
        report_cd_refusal(
            &RpcError::new(
                error_code::SESSION_BUSY,
                "session s1 is already running turn turn-2; retry when it finishes",
            ),
            &mut surface,
        );
        let busy = surface.lines_of(LineKind::Notice).join("\n");
        assert!(busy.contains("was not moved"), "{busy}");
        assert!(busy.contains("turn-2"), "{busy}");
        assert!(
            surface.lines_of(LineKind::Error).is_empty(),
            "{:?}",
            surface.calls
        );

        let mut surface = RecordingSurface::new();
        report_cd_refusal(
            &RpcError::new(
                error_code::INVALID_PARAMS,
                "path `/nope` does not exist or is not a directory",
            ),
            &mut surface,
        );
        let failed = surface.lines_of(LineKind::Error).join("\n");
        assert_eq!(
            failed,
            "the session root could not be moved: path `/nope` does not exist or is not a \
             directory",
            "the daemon's reason, naming the path, after this line's own words"
        );
        assert!(
            surface.lines_of(LineKind::Notice).is_empty(),
            "{:?}",
            surface.calls
        );

        assert!(CD_NEEDS_A_SESSION.contains("needs a session"));
    }

    // ------------------------------------------------------------------
    // REQ-582: the ten mirrored rows (BR-1, AC-4, AC-8; ADR-8)
    // ------------------------------------------------------------------

    /// **The mechanism, not a convention.** A mirrored row's name *is* the
    /// subcommand path of its twin, because ADR-1 recognizes a typed `teton …`
    /// line by walking clap's tree to a path and looking that path up in this
    /// table. A row named anything else would still dispatch from `/`, and would
    /// silently stop being reachable from the line this REQ exists to answer.
    #[test]
    fn every_mirror_names_teton_plus_its_own_row() {
        let mirrored: Vec<&str> = COMMANDS
            .iter()
            .filter(|spec| spec.mirror.is_some())
            .map(|spec| spec.name)
            .collect();
        assert_eq!(
            mirrored.len(),
            10,
            "BR-1 names ten mirrored rows; found {mirrored:?}"
        );
        for spec in COMMANDS {
            let Some(mirror) = spec.mirror else { continue };
            assert_eq!(
                mirror.shell,
                format!("teton {}", spec.name),
                "/{} mirrors a command it is not named after",
                spec.name
            );
        }
        // And the hand-off's view of the same table agrees with it (BR-8), so
        // TASK-171's nudge cannot name a spelling that dispatches to nothing.
        let pairs: Vec<(&str, &str)> = mirrored_rows().collect();
        assert_eq!(pairs.len(), mirrored.len());
        for (name, shell) in pairs {
            assert_eq!(shell, format!("teton {name}"));
        }
    }

    /// AC-4's declaration half: exactly four rows write, and they are the four
    /// the Permissions table names. The *behaviour* — one line, no RPC, naming
    /// the shell twin — is pinned in `cli_rows`, where the gate runs.
    #[test]
    fn exactly_the_four_writing_rows_are_marked_as_writes() {
        let writes: Vec<&str> = COMMANDS
            .iter()
            .filter(|spec| spec.mirror.is_some_and(|mirror| mirror.writes))
            .map(|spec| spec.name)
            .collect();
        assert_eq!(
            writes,
            vec![
                "provider add",
                "boundary add",
                "policy set-tier",
                "policy set-category"
            ],
            "the write rows are the ones that change daemon or machine state"
        );
    }

    /// AC-8: every mirrored row reaches `/help` — generated from the table, so
    /// this is a property rather than a second listing — and the listing is
    /// grouped, with each family's rows in one contiguous run separated by a
    /// blank line.
    #[test]
    fn help_lists_every_mirrored_row_grouped_by_family() {
        let mut surface = RecordingSurface::new();
        render_help(&mut surface, &no_skills());
        let rendered = surface.lines_of(LineKind::Info);

        for spec in COMMANDS {
            assert!(
                rendered
                    .iter()
                    .any(|line| line.starts_with(&format!("/{}", spec.name))
                        && line.contains(spec.summary)),
                "/{} is missing from /help with its summary:\n{}",
                spec.name,
                rendered.join("\n")
            );
        }

        // The families, read back off the rendered listing: every group of rows
        // between blank lines shares one family, and no family appears twice.
        let mut seen: Vec<&str> = Vec::new();
        let mut group: Vec<&str> = Vec::new();
        let mut groups: Vec<Vec<&str>> = Vec::new();
        for line in &rendered {
            if line.is_empty() {
                groups.push(std::mem::take(&mut group));
            } else if let Some(name) = line.strip_prefix('/') {
                group.push(name.split_whitespace().next().unwrap_or(name));
            }
        }
        groups.push(group);
        for group in groups {
            let families: Vec<&str> = group
                .iter()
                .map(|first| {
                    if COMMANDS
                        .iter()
                        .filter(|spec| first_word(spec.name) == *first)
                        .count()
                        > 1
                    {
                        *first
                    } else {
                        ""
                    }
                })
                .collect();
            assert!(
                families.windows(2).all(|pair| pair[0] == pair[1]),
                "a rendered group mixes families: {group:?}"
            );
            if let Some(family) = families.first().filter(|family| !family.is_empty()) {
                assert!(
                    !seen.contains(family),
                    "the `{family}` family is split across the listing"
                );
                seen.push(family);
            }
        }
        // The four families a reader should be able to find as blocks.
        assert!(seen.contains(&"model"), "{seen:?}");
        assert!(seen.contains(&"provider"), "{seen:?}");
        assert!(seen.contains(&"boundary"), "{seen:?}");
        assert!(seen.contains(&"policy"), "{seen:?}");
        assert!(seen.contains(&"web"), "{seen:?}");
    }

    /// A mirrored row's summary says what the command does, in one line, and
    /// does **not** name the shell twin: the session is where the user is, and
    /// a listing that spelled every row twice would teach the shell form to a
    /// user who no longer needs it. The one place a twin *is* named is the
    /// typed-input refusal, where it is the remedy (ADR-4).
    ///
    /// Scoped to the mirrored rows on purpose: `/cost`'s summary names `teton
    /// cost` and has since REQ-555, where naming the twin was the point ("the
    /// daemon's cost report, exactly as `teton cost` does").
    #[test]
    fn a_mirrored_summary_is_one_line_and_names_no_shell_command() {
        for spec in COMMANDS {
            assert_eq!(
                spec.summary.lines().count(),
                1,
                "/{} has a multi-line summary",
                spec.name
            );
            if spec.mirror.is_some() {
                assert!(
                    !spec.summary.contains("teton "),
                    "/{} names its shell twin in the listing: {}",
                    spec.name,
                    spec.summary
                );
            }
        }
    }

    /// **ADR-8's completeness half.** Walk the CLI parser's own tree and require
    /// every leaf subcommand to be either a row in this table or an explicit
    /// shell-only exception. The compile-time half is `run_mirrored_command`'s
    /// wildcard-free match; this is the half that catches a subcommand added
    /// with no session decision at all — it would land here as a leaf nobody
    /// listed, rather than as a command users quietly cannot reach.
    ///
    /// Hidden leaves are exempt, and `policy set` is the only one: it is the
    /// retired phase form, kept solely to explain itself to muscle memory
    /// (REQ-558 AC-9). Requiring a session row for a command the CLI does not
    /// offer would be requiring a row nobody is told about — and `SHELL_ONLY` is
    /// for *visible* commands deliberately left in the shell, which is a
    /// different statement. The exemption is asserted narrowly, so a second
    /// hidden leaf still surfaces the decision here.
    #[test]
    fn every_cli_leaf_is_a_session_row_or_an_explicit_shell_only_exception() {
        let names: Vec<&str> = COMMANDS.iter().map(|spec| spec.name).collect();
        let leaves = cli_rows::leaf_command_paths();
        assert!(
            leaves.len() > 10,
            "the tree walk found almost nothing: {leaves:?}"
        );

        let hidden: Vec<&str> = leaves
            .iter()
            .filter(|(_, hidden)| *hidden)
            .map(|(path, _)| path.as_str())
            .collect();
        assert_eq!(
            hidden,
            vec!["policy set"],
            "a new hidden subcommand needs a decision: exempt it here, or list it"
        );

        for (path, hidden) in &leaves {
            if *hidden {
                continue;
            }
            assert!(
                names.contains(&path.as_str()) || cli_rows::SHELL_ONLY.contains(&path.as_str()),
                "`teton {path}` has no session row and is not listed as shell-only"
            );
        }

        // Both directions (LESSON-479): a `SHELL_ONLY` entry that no longer
        // names a real subcommand is a stale exemption, and would silently
        // exempt nothing.
        for shell_only in cli_rows::SHELL_ONLY {
            assert!(
                leaves.iter().any(|(path, _)| path == shell_only),
                "`{shell_only}` is exempted from a subcommand that no longer exists"
            );
            assert!(
                !names.contains(shell_only),
                "`{shell_only}` is both shell-only and a session row"
            );
        }
    }

    // ------------------------------------------------------------------
    // REQ-582 TASK-170 — recognizing a typed `teton …` line (BR-4, ADR-1)
    // ------------------------------------------------------------------

    /// **ADR-8, forward direction, for the CLI line.** Every row is reachable
    /// from `teton <row>` exactly as it is from `/<row>`, and reaches the same
    /// resolution.
    ///
    /// Scoped to the rows whose names are subcommand paths — the mirrored ten
    /// plus the older rows that predate this REQ and share their twin's spelling
    /// (`cost`, `effort`, `model`, `model set`, `provider test`). The
    /// session-only rows (`/help`, `/clear`, `/web setup`, …) are *deliberately*
    /// unreachable this way: they have no `teton` form, so `teton help` is a
    /// prompt and BR-4 says so.
    ///
    /// The membership itself is asserted from clap's tree rather than from a
    /// list here: a row is reachable iff its name is a path in the parser, which
    /// is the whole of ADR-1's rule.
    #[test]
    fn every_row_that_names_a_subcommand_is_reachable_from_a_typed_teton_line() {
        let mut reachable = 0;
        for spec in COMMANDS {
            let words: Vec<&str> = spec.name.split_whitespace().collect();
            let is_subcommand = cli_rows::cli_path(&words).len() == words.len();
            // A row that requires an argument is only reachable *with* one, the
            // same way it is from a `/` line — the resolve-time rejection is the
            // row's, not the recognition's.
            let expected_args = match spec.args {
                Args::Required(_) => "qwen2.5-coder-3b",
                Args::None | Args::Optional | Args::Cli => "",
            };
            let typed = format!("teton {} {expected_args}", spec.name);
            let typed = typed.trim_end();
            if !is_subcommand {
                // The session-only rows — `/help`, `/clear`, `/web setup`,
                // `/provider setup` — have no `teton` form (BR-4). The ones
                // whose first word *is* a family are refused with that family's
                // session rows; the rest are prompts.
                assert!(
                    !matches!(classify(typed, &no_skills()), Input::CliLine { .. }),
                    "`{typed}` names no subcommand path and must not be recognized"
                );
                continue;
            }
            reachable += 1;
            assert_eq!(
                classify(typed, &no_skills()),
                Input::CliLine {
                    name: spec.name,
                    args: expected_args,
                    shell_flags: "",
                },
                "`{typed}` did not reach its row"
            );
            // And the row it reaches dispatches, which is what makes the notice
            // line's `/<name>` a spelling that works (AC-5).
            let Input::CliLine { name, args, .. } = classify(typed, &no_skills()) else {
                unreachable!("just asserted");
            };
            let Resolution::Run(resolved, run_args) = resolve(name, args) else {
                panic!("`{typed}` was recognized but did not dispatch");
            };
            assert_eq!(resolved.name, spec.name);
            assert_eq!(run_args, expected_args);
        }
        // Every mirrored row is in that set by construction (a mirror's `shell`
        // is `teton ` + its name), so the count cannot silently fall to zero.
        assert!(
            reachable >= COMMANDS.iter().filter(|spec| spec.mirror.is_some()).count(),
            "only {reachable} rows were reachable from a `teton …` line"
        );
    }

    /// `teton model` is **not** a refusal, and that is the table's rule working
    /// rather than an exception to it: `model` names a row (`/model`, REQ-555's
    /// one-line current-model answer), so ADR-1's first arm applies and the row
    /// runs. A shell prints the family's help for the same words because a shell
    /// has no `/model`; the session has one, and answering the question is
    /// better than describing the family.
    #[test]
    fn a_family_word_that_is_itself_a_row_runs_that_row() {
        assert_eq!(
            classify("teton model", &no_skills()),
            Input::CliLine {
                name: "model",
                args: "",
                shell_flags: "",
            }
        );
        // And the families that are *not* rows still refuse.
        assert!(matches!(
            classify("teton provider", &no_skills()),
            Input::CliRefused(_)
        ));
    }

    /// The argument is whatever the subcommand path did not consume, and the
    /// row's own grammar judges it (BR-3). The spec's own examples, plus the two
    /// pre-REQ rows whose twins take an argument (LESSON-512).
    #[test]
    fn a_typed_teton_line_carries_the_words_the_path_did_not_consume() {
        for (typed, name, args) in [
            (
                "teton policy set-tier build kimi --fallback local",
                "policy set-tier",
                "build kimi --fallback local",
            ),
            ("teton model set qwen", "model set", "qwen"),
            ("teton provider test kimi", "provider test", "kimi"),
            (
                "teton boundary add src/** --mode local-only",
                "boundary add",
                "src/** --mode local-only",
            ),
            // ADR-1's amendment: a stray word does not un-recognize a command.
            // The path is still `provider list`, and clap says what it thinks of
            // `please` (AC-6).
            ("teton provider list please", "provider list", "please"),
            // Whitespace between the words is normalised the same way a two-word
            // row's is, and the argument keeps its own spacing after trimming.
            ("teton  policy   show", "policy show", ""),
        ] {
            assert_eq!(
                classify(typed, &no_skills()),
                Input::CliLine {
                    name,
                    args,
                    shell_flags: "",
                },
                "`{typed}`"
            );
        }
    }

    /// BR-4's refusals: a real command with no session form, the bare binary,
    /// and its own flags. One line each, and never the model.
    #[test]
    fn a_teton_line_with_no_session_form_is_refused_with_the_reason() {
        let refusal = |line: &str| match classify(line, &no_skills()) {
            Input::CliRefused(text) => text,
            other => panic!("`{line}` classified as {other:?} rather than a refusal"),
        };

        // `teton uninstall` — the one `SHELL_ONLY` command. It names itself as
        // the shell command to run, because the reason it cannot run here (it
        // stops this session's daemon) is not a reason to leave the user stuck.
        let uninstall = refusal("teton uninstall");
        assert_eq!(uninstall.lines().count(), 1, "{uninstall}");
        assert!(uninstall.contains("teton uninstall"), "{uninstall}");
        assert!(uninstall.contains("shell"), "{uninstall}");

        // A family typed bare names the session's rows under it — generated
        // from the table, so a row added to the family is named here without a
        // second list to maintain (BR-7's rule, one surface further along).
        let family = refusal("teton provider");
        assert_eq!(family.lines().count(), 1, "{family}");
        for row in ["/provider add", "/provider list", "/provider test"] {
            assert!(
                family.contains(row),
                "`teton provider` omits `{row}`: {family}"
            );
        }
        // Including the row the CLI has no subcommand for at all, which is the
        // one a user typing `teton provider …` most likely wants (REQ-579).
        assert!(family.contains("/provider setup"), "{family}");
        for bare in ["teton policy", "teton boundary"] {
            let line = refusal(bare);
            assert_eq!(line.lines().count(), 1, "{line}");
            assert!(line.contains("family"), "{line}");
            assert!(line.contains("in this session"), "{line}");
        }
        // A family plus a word that names no subcommand leaves the walk on the
        // family, and gets the same answer — which is how `teton provider
        // setup`, a session-only command with no CLI form, is answered with its
        // own `/` spelling instead of being sent to the model (BR-4).
        let setup = refusal("teton provider setup");
        assert!(setup.contains("/provider setup"), "{setup}");

        // The retired phase form (REQ-558 AC-9), hidden from the CLI's own
        // listing: the answer is the retirement sentence itself — the axis
        // changed, not the user's typing — and never "no session form" (verify
        // m6). The same words the shell prints for the same argv.
        let retired = refusal("teton policy set build kimi");
        assert_eq!(retired, crate::POLICY_SET_RETIRED);
        assert!(retired.contains("set-tier"), "{retired}");

        // The family wording says what was typed and what it is (verify m4):
        // the words name a family, and the family's session rows follow.
        assert!(family.contains("`teton provider …`"), "{family}");
        assert!(
            family.contains("names a family rather than a command"),
            "{family}"
        );

        // The bare binary opens a session, and the user is in one.
        assert_eq!(refusal("teton"), ALREADY_IN_A_SESSION);
        // Its own flags: `/help` answers one, a shell answers the other.
        for flag in ["teton --help", "teton -h", "teton --version", "teton -V"] {
            assert_eq!(refusal(flag), CLI_FLAGS_ARE_SHELL_ONLY, "`{flag}`");
        }
    }

    /// The uninstall sentence is written for the one entry `SHELL_ONLY` has, and
    /// says why *that* command cannot run here. A second entry would inherit a
    /// reason that is not its own, so it has to fail here first.
    #[test]
    fn shell_only_still_names_exactly_the_command_its_refusal_explains() {
        assert_eq!(
            cli_rows::SHELL_ONLY,
            [
                "uninstall",
                "transcript enable",
                "transcript disable",
                "transcript status",
                "context enable",
                "context disable",
                "context status",
            ],
            "a new shell-only command needs its own reason in `refusal_for_path`"
        );
        // REQ-611: each family names its own reason, never the other's.
        let uninstall = cli_rows::refusal_for_path(&["uninstall"]);
        assert!(uninstall.contains("removes its data"), "{uninstall}");
        for leaf in ["enable", "disable", "status"] {
            let line = cli_rows::refusal_for_path(&["transcript", leaf]);
            assert!(
                line.contains("durable transcript default") && line.contains("/transcript on"),
                "{line}"
            );
            assert!(!line.contains("removes its data"), "{line}");
        }
        // REQ-612: the third family, and its sentence names its own two
        // lifetimes rather than borrowing the transcript's.
        for leaf in ["enable", "disable", "status"] {
            let line = cli_rows::refusal_for_path(&["context", leaf]);
            assert!(
                line.contains("durable repository-notes default") && line.contains("/context on"),
                "{line}"
            );
            assert!(
                !line.contains("removes its data") && !line.contains("transcript"),
                "{line}"
            );
        }
    }

    /// **ADR-8, reverse direction.** A line that opens with `teton` but names no
    /// subcommand is a prompt, byte-identical to today — and so is anything that
    /// merely looks like the binary's name.
    ///
    /// "teton is slow today" is the case the ADR names: it is a legitimate
    /// question about the product, and answering it with a parser error would be
    /// the mirror image of the failure this REQ removes.
    #[test]
    fn a_teton_line_that_names_no_subcommand_is_a_byte_identical_prompt() {
        for line in [
            "teton is slow today",
            "teton keeps asking me about providers",
            // The word must be the whole first token.
            "tetonx provider list",
            "teton-code provider list",
            "tetonprovider list",
            // A command is lowercase; a capitalised mention is prose.
            "Teton provider list",
            // And `teton` anywhere but the front is just a word.
            "why is teton provider list so slow?",
            // Clap generates a `help` subcommand at runtime and this tree does
            // not carry it — which is what keeps `teton help me read this` a
            // question rather than a refusal.
            "teton help me read this backtrace",
        ] {
            assert_eq!(
                classify(line, &no_skills()),
                Input::Prompt(line),
                "`{line}`"
            );
        }
    }

    /// BR-11 / REQ-555 BR-1b: recognition is checked **after** the escape hatch,
    /// so `//teton …` still reaches the model with exactly the leading pair
    /// collapsed. A `teton` line needs no escape of its own — only a strict
    /// parse intercepts — but a user who escapes one anyway must get what the
    /// escape promises.
    #[test]
    fn the_double_slash_escape_still_outranks_recognition() {
        assert_eq!(
            classify("//teton provider list", &no_skills()),
            Input::EscapedPrompt("/teton provider list")
        );
        assert_eq!(
            classify("//teton uninstall", &no_skills()),
            Input::EscapedPrompt("/teton uninstall")
        );
    }

    /// **OQ-1, resolved.** `/teton provider list` is the same command with the
    /// slash a user has learned to type, and it runs the same row.
    ///
    /// What it does not do is turn a `/` line into a prompt: a `/teton …` line
    /// that names no subcommand is still an unknown command, rejected with the
    /// hint, exactly as it was before this REQ (BR-1).
    #[test]
    fn a_slashed_cli_line_runs_the_same_row_and_an_unrecognized_one_still_rejects() {
        assert_eq!(
            classify("/teton provider list", &no_skills()),
            Input::CliLine {
                name: "provider list",
                args: "",
                shell_flags: "",
            }
        );
        assert_eq!(
            classify("/teton policy set-tier build kimi", &no_skills()),
            Input::CliLine {
                name: "policy set-tier",
                args: "build kimi",
                shell_flags: "",
            }
        );
        assert!(matches!(
            classify("/teton", &no_skills()),
            Input::CliRefused(_)
        ));

        let Input::Command { name, args } = classify("/teton frobnicate", &no_skills()) else {
            panic!("a `/` line that names no subcommand is still a command line");
        };
        assert_eq!((name, args), ("teton", "frobnicate"));
        let Resolution::Rejected(hint) = resolve(name, args) else {
            panic!("`/teton frobnicate` must be rejected as an unknown command");
        };
        assert!(hint.contains("unknown command: `/teton`"), "{hint}");
    }

    /// The argument split, as the rule it is: drop the words the walk consumed,
    /// whatever they were spelled. Counting rather than matching the row's name
    /// is what keeps an aliased spelling's argument (`teton p list kimi`, were
    /// `p` ever an alias) from being silently dropped.
    #[test]
    fn after_words_drops_exactly_the_words_the_path_consumed() {
        assert_eq!(after_words("provider list please", 2), "please");
        assert_eq!(after_words("p list kimi", 2), "kimi");
        assert_eq!(after_words("doctor", 1), "");
        assert_eq!(
            after_words("  policy   set-tier   build kimi ", 2),
            "build kimi"
        );
        assert_eq!(after_words("provider list", 2), "");
        assert_eq!(after_words("anything", 0), "anything");
    }

    /// **REQ-585 ADR-12 / AC-14.** `/help` renders its section from the
    /// **session's own** snapshot — the same value [`classify`] dispatches from
    /// — so a `/cd` that re-derived the registry is reflected without a
    /// restart, and BR-3's "a skill cannot be dispatchable without appearing in
    /// `/help`" holds because there is one list rather than two readers who
    /// agree today. Rendering from a locally-built empty snapshot instead would
    /// leave `/help` claiming the session has no skills while `/alpha` ran one.
    ///
    /// Driven through [`dispatch`] rather than [`render_help`] directly,
    /// because the claim is about the *row*, not the renderer — and it asserts
    /// the row issues no RPC, which is the `assert_no_turn_ran` posture `/help`
    /// has always had.
    #[test]
    fn help_renders_the_sessions_own_snapshot() {
        let snapshot = registry(
            vec![described(
                "alpha",
                SkillSource::User,
                "[topic]",
                "Draft a note.",
            )],
            Vec::new(),
        );
        let (mut conn, peer) = Connection::scripted(&[]);
        let mut surface = RecordingSurface::new();
        let mut state = SessionState::new();
        let mut prompter = ScriptedPrompter::new(&[]);
        {
            let mut ctx = session_ctx(&mut surface, &mut state, &mut prompter);
            ctx.skills = snapshot.clone();
            // The registry argument is `dispatch`'s own (it feeds BR-10's
            // hint); `/help` must read the one on the context, so this one is
            // deliberately empty. Reading the argument instead fails here.
            let outcome =
                dispatch("help", "", &no_skills(), &mut conn, &mut ctx).expect("/help renders");
            assert_eq!(outcome, CommandOutcome::Continue);
        }

        let lines: Vec<String> = surface
            .lines_of(LineKind::Info)
            .iter()
            .map(|line| (*line).to_owned())
            .collect();
        assert!(
            lines.iter().any(|line| line == SKILLS_HEADER),
            "the session's snapshot renders a section: {lines:?}"
        );
        assert_eq!(
            skill_rows(&lines),
            skill_rows(&help_lines(&snapshot)),
            "the row is the one the renderer draws for this snapshot"
        );
        assert!(
            crate::client::methods_written(&peer).is_empty(),
            "/help issues no RPC and runs no turn"
        );
    }

    /// AC-6's third clause, at the seam that produces it: a recognized line with
    /// a stray word dispatches, and the row's clap parse prints the parser's own
    /// `unexpected argument` — the same text the shell prints for that argv.
    ///
    /// Driven through [`dispatch`] rather than asserted on the classifier alone,
    /// because "recognized" is only half the claim: the other half is that the
    /// row it reaches judges the argument (BR-3).
    #[test]
    fn a_recognized_line_with_a_stray_word_prints_the_parsers_own_error() {
        let Input::CliLine { name, args, .. } =
            classify("teton provider list please", &no_skills())
        else {
            panic!("a stray word does not un-recognize a command (ADR-1)");
        };

        let (mut conn, _peer) = Connection::scripted(&[]);
        let mut surface = RecordingSurface::new();
        let mut state = SessionState::new();
        let mut prompter = ScriptedPrompter::new(&[]);
        {
            let mut ctx = session_ctx(&mut surface, &mut state, &mut prompter);
            let outcome = dispatch(name, args, &no_skills(), &mut conn, &mut ctx)
                .expect("a parse error never fails the command");
            assert_eq!(outcome, CommandOutcome::Continue);
        }

        let expected = crate::Cli::try_parse_from(["teton", "provider", "list", "please"])
            .expect_err("the shell rejects it too")
            .render()
            .to_string();
        let first = expected
            .lines()
            .find(|line| !line.trim().is_empty())
            .expect("clap says something")
            .strip_prefix("error: ")
            .expect("clap leads with its own error: prefix");
        assert_eq!(surface.lines_of(LineKind::Error), vec![first]);
        assert!(
            first.contains("unexpected argument 'please'"),
            "AC-6 names this text: {first}"
        );
    }

    // ------------------------------------------------------------------
    // REQ-582 verify — leading global flags (m5), family help (T6), and the
    // full-argv validation of a pre-REQ row (M2)
    // ------------------------------------------------------------------

    /// **m5.** The shell's own global flags ahead of the subcommand do not
    /// un-recognize a line: `teton -y policy set-tier build kimi` is the
    /// `policy set-tier` row, and the flag rides the recognized line rather
    /// than sending the whole thing to the model as a question.
    #[test]
    fn a_leading_global_flag_is_stepped_over_and_carried() {
        assert_eq!(
            classify("teton -y policy set-tier build kimi", &no_skills()),
            Input::CliLine {
                name: "policy set-tier",
                args: "build kimi",
                shell_flags: "-y",
            }
        );
        assert_eq!(
            classify("teton --verbose doctor", &no_skills()),
            Input::CliLine {
                name: "doctor",
                args: "",
                shell_flags: "--verbose",
            }
        );
        // Two flags, both carried as typed.
        assert_eq!(
            classify("teton --yes -v model set qwen", &no_skills()),
            Input::CliLine {
                name: "model set",
                args: "qwen",
                shell_flags: "--yes -v",
            }
        );
        // The bare binary with a flag is still the bare binary: the user is in
        // the session `teton -y` would have opened.
        assert_eq!(
            classify("teton -y", &no_skills()),
            Input::CliRefused(ALREADY_IN_A_SESSION.to_owned())
        );
        assert_eq!(
            classify("teton -y --help", &no_skills()),
            Input::CliRefused(CLI_FLAGS_ARE_SHELL_ONLY.to_owned())
        );
        // A leading token that merely starts with `-` is not a flag this
        // classifier knows, so the line is what the walk says: a prompt.
        assert_eq!(
            classify("teton -whatever is going on", &no_skills()),
            Input::Prompt("teton -whatever is going on")
        );
        // The split itself, on its edges.
        assert_eq!(split_leading_flags("-y policy show"), ("-y", "policy show"));
        assert_eq!(
            split_leading_flags("  --yes   -v   doctor "),
            ("--yes   -v", "doctor ")
        );
        assert_eq!(split_leading_flags("policy show"), ("", "policy show"));
        assert_eq!(split_leading_flags(""), ("", ""));
        assert_eq!(split_leading_flags("-y"), ("-y", ""));
    }

    /// **T6.** A family followed by an explicit help request gets clap's own
    /// page for that family, as information — not the bare-family refusal,
    /// which would swallow the ask. A leaf's help is its row's business, and a
    /// shell-only command's `--help` is still the shell-only refusal: the
    /// reason it cannot run here matters more than its usage.
    #[test]
    fn a_family_help_request_renders_the_familys_own_help_page() {
        for line in [
            "teton provider --help",
            "teton provider -h",
            "teton policy -h",
        ] {
            let Input::CliHelp(text) = classify(line, &no_skills()) else {
                panic!(
                    "`{line}` did not classify as a help page: {:?}",
                    classify(line, &no_skills())
                );
            };
            let words: Vec<&str> = line.split_whitespace().skip(1).collect();
            let expected =
                crate::Cli::try_parse_from(std::iter::once("teton").chain(words.iter().copied()))
                    .expect_err("clap reports help as an Err")
                    .render()
                    .to_string();
            assert_eq!(text, expected, "`{line}` is not clap's own page");
            assert!(text.contains("Usage:"), "`{line}`: {text}");
        }
        // The family's rows are named on the page — it is the page a shell
        // prints, and it lists the subcommands.
        let Input::CliHelp(provider) = classify("teton provider --help", &no_skills()) else {
            unreachable!("just asserted");
        };
        for sub in ["add", "list", "test"] {
            assert!(
                provider.contains(sub),
                "the provider page omits `{sub}`: {provider}"
            );
        }
        // Not a family: `uninstall --help` is still refused for the reason
        // `uninstall` is, and `provider setup --help` (a word that names no
        // subcommand) is still the family's session rows.
        assert!(
            matches!(classify("teton uninstall --help", &no_skills()), Input::CliRefused(text) if text.contains("shell-only"))
        );
        assert!(
            matches!(classify("teton provider setup --help", &no_skills()), Input::CliRefused(text) if text.contains("/provider setup"))
        );
        // And a family typed bare is unchanged by this: the session's rows.
        assert!(
            matches!(classify("teton provider", &no_skills()), Input::CliRefused(text) if text.contains("/provider list"))
        );
        // The helper says `None` for anything that is not a family, so a caller
        // falls back to the refusal rather than to an empty page.
        assert!(cli_rows::family_help(&["doctor"], "--help").is_none());
        assert!(cli_rows::family_help(&["nope"], "--help").is_none());
        assert!(cli_rows::family_help(&["policy"], "--help").is_some());
    }

    /// **M2.** A recognized line for a row that predates this REQ is validated
    /// whole by the binary's own parser before its row runs.
    ///
    /// The four leaves in question: `model set`, `provider test`, `effort`,
    /// `cost`. Each case states what reached the row (or that nothing did) and
    /// what was rendered about the shell flags. The tail of the test is the one
    /// pre-REQ **family** row, `model`: dispatched directly, but with a leading
    /// flag reported and dropped, and with `--help` answered by the family's
    /// own page.
    #[test]
    fn a_pre_req_row_has_its_whole_argv_validated_before_it_runs() {
        /// Run `run_cli_line` over what `classify(line, &no_skills())` produced.
        fn run(
            line: &str,
            scripted: &[serde_json::Value],
            typed_input: bool,
        ) -> (RecordingSurface, Vec<String>) {
            let Input::CliLine {
                name,
                args,
                shell_flags,
            } = classify(line, &no_skills())
            else {
                panic!(
                    "`{line}` was not recognized: {:?}",
                    classify(line, &no_skills())
                );
            };
            let (mut conn, peer) = Connection::scripted(scripted);
            let mut surface = RecordingSurface::new();
            let mut state = SessionState::new();
            let mut prompter = ScriptedPrompter::new(&[]);
            {
                let mut ctx = session_ctx(&mut surface, &mut state, &mut prompter);
                ctx.typed_input = typed_input;
                let outcome =
                    run_cli_line(name, args, shell_flags, &no_skills(), &mut conn, &mut ctx)
                        .unwrap_or_else(|err| panic!("`{line}` failed: {err:#}"));
                assert_eq!(outcome, CommandOutcome::Continue);
            }
            conn.assert_all_consumed();
            (surface, crate::client::methods_written(&peer))
        }

        // `teton model set qwen --yes` → `/model set qwen`, plus the flag line.
        // The empty catalog makes the row's own validation the evidence: its one
        // error names `qwen` — the argument the row was handed — and never
        // `--yes`, which the parser took as the flag it is.
        let (surface, methods) = run(
            "teton model set qwen --yes",
            &[serde_json::to_value(crate::model_ui::testing::list_result()).unwrap()],
            true,
        );
        assert_eq!(methods, vec!["model/list"], "{:?}", surface.calls);
        let errors = surface.lines_of(LineKind::Error);
        assert_eq!(errors.len(), 1, "{:?}", surface.calls);
        assert!(errors[0].contains("named `qwen`"), "{}", errors[0]);
        assert!(
            !errors[0].contains("--yes"),
            "the flag reached the row as part of the name: {}",
            errors[0]
        );
        let infos = surface.lines_of(LineKind::Info);
        assert_eq!(
            infos,
            vec![cli_rows::shell_flags_line("model set", true)],
            "{:?}",
            surface.calls
        );
        assert!(
            infos[0].contains("--yes") && infos[0].contains("/model set"),
            "{}",
            infos[0]
        );

        // The same with the flag ahead of the subcommand (m5 meets M2).
        let (surface, methods) = run(
            "teton -y model set qwen",
            &[serde_json::to_value(crate::model_ui::testing::list_result()).unwrap()],
            true,
        );
        assert_eq!(methods, vec!["model/list"]);
        assert_eq!(
            surface.lines_of(LineKind::Info),
            vec![cli_rows::shell_flags_line("model set", true)]
        );

        // `teton effort` bare → the read: one `config/get`, no set.
        let (surface, methods) = run(
            "teton effort",
            &[serde_json::to_value(teton_protocol::methods::ConfigGetResult::default()).unwrap()],
            true,
        );
        assert_eq!(methods, vec!["config/get"], "{:?}", surface.calls);
        assert!(
            surface.lines_of(LineKind::Error).is_empty(),
            "{:?}",
            surface.calls
        );

        // `teton effort bogus` parses — the level is a free string to clap, and
        // its vocabulary is the row's (`teton effort bogus` in a shell fails the
        // same way, one step later) — so the row's own one-line rejection is
        // what renders, and no RPC is issued.
        let (surface, methods) = run("teton effort bogus", &[], true);
        assert!(methods.is_empty(), "{:?}", surface.calls);
        let errors = surface.lines_of(LineKind::Error);
        assert_eq!(errors.len(), 1, "{:?}", surface.calls);
        assert!(
            errors[0].contains("bogus") && errors[0].contains("max"),
            "the row's own error names the value and the vocabulary: {}",
            errors[0]
        );

        // `teton effort low extra` → clap's own error, and nothing dispatched:
        // the whole argv was judged before the row saw any of it.
        let (surface, methods) = run("teton effort low extra", &[], true);
        assert!(methods.is_empty(), "{:?}", surface.calls);
        let expected = crate::Cli::try_parse_from(["teton", "effort", "low", "extra"])
            .expect_err("the shell rejects it too")
            .render()
            .to_string();
        assert_eq!(
            surface.lines_of(LineKind::Error),
            vec![expected
                .lines()
                .next()
                .unwrap()
                .strip_prefix("error: ")
                .unwrap()],
            "{:?}",
            surface.calls
        );
        assert!(
            surface.lines_of(LineKind::Error)[0].contains("unexpected argument 'extra'"),
            "{:?}",
            surface.calls
        );

        // `teton cost extra` → clap's own error, nothing dispatched.
        let (surface, methods) = run("teton cost extra", &[], true);
        assert!(methods.is_empty(), "{:?}", surface.calls);
        assert!(
            surface.lines_of(LineKind::Error)[0].contains("unexpected argument 'extra'"),
            "{:?}",
            surface.calls
        );

        // `teton provider test kimi --yes` → `/provider test kimi` plus the
        // generic flag line. The test context owns no session, so the row's own
        // first line is what proves it was reached; nothing goes on the wire.
        let (surface, methods) = run("teton provider test kimi --yes", &[], true);
        assert!(methods.is_empty(), "{:?}", surface.calls);
        assert!(
            surface
                .lines_of(LineKind::Error)
                .iter()
                .any(|line| line.contains("`/provider test` needs a session")),
            "the row did not run: {:?}",
            surface.calls
        );
        assert_eq!(
            surface.lines_of(LineKind::Info),
            vec![cli_rows::shell_flags_line("provider test", true)]
        );
        assert!(
            surface.lines_of(LineKind::Info)[0].contains("/provider test"),
            "{:?}",
            surface.calls
        );

        // A mirrored row with a leading flag: the flag reaches the row's own
        // clap parse (spliced onto the argument) and the row says it was
        // ignored — one place, one sentence — before running as usual.
        let (surface, methods) = run(
            "teton -y provider list",
            &[serde_json::to_value(teton_protocol::methods::ConfigGetResult::default()).unwrap()],
            true,
        );
        assert_eq!(methods, vec!["config/get"], "{:?}", surface.calls);
        assert!(
            surface
                .lines_of(LineKind::Info)
                .contains(&cli_rows::shell_flags_line("provider list", true).as_str()),
            "{:?}",
            surface.calls
        );

        // And the family that is itself a row keeps TASK-170's direct dispatch:
        // `teton model` is `/model`, one `model/status`, no parse to fail.
        let (surface, methods) = run(
            "teton model",
            &[
                serde_json::to_value(teton_protocol::methods::ModelStatusResult::default())
                    .unwrap(),
            ],
            true,
        );
        assert_eq!(methods, vec!["model/status"], "{:?}", surface.calls);
        assert!(
            surface.lines_of(LineKind::Error).is_empty(),
            "{:?}",
            surface.calls
        );

        // `teton -y model` (verify residue, correctness Minor): the family that
        // is a row is still `/model` — one `model/status` — and the flag is
        // reported as ignored rather than handed to a row that takes no
        // argument. Before this the line was rejected with "takes no
        // arguments" and ran nothing.
        let (surface, methods) = run(
            "teton -y model",
            &[
                serde_json::to_value(teton_protocol::methods::ModelStatusResult::default())
                    .unwrap(),
            ],
            true,
        );
        assert_eq!(methods, vec!["model/status"], "{:?}", surface.calls);
        assert!(
            surface.lines_of(LineKind::Error).is_empty(),
            "{:?}",
            surface.calls
        );
        assert!(
            surface
                .lines_of(LineKind::Info)
                .contains(&cli_rows::shell_flags_line("model", true).as_str()),
            "the dropped flag must be reported: {:?}",
            surface.calls
        );
        // `--verbose` ahead of it takes the general sentence (`yes` is false).
        let (surface, methods) = run(
            "teton --verbose model",
            &[
                serde_json::to_value(teton_protocol::methods::ModelStatusResult::default())
                    .unwrap(),
            ],
            true,
        );
        assert_eq!(methods, vec!["model/status"], "{:?}", surface.calls);
        assert!(
            surface
                .lines_of(LineKind::Info)
                .contains(&cli_rows::shell_flags_line("model", false).as_str()),
            "{:?}",
            surface.calls
        );

        // `teton model --help` / `-h`: the family's own page, as the shell
        // prints it — Info lines, no error, no RPC, and no "takes no
        // arguments" rejection for a question the user asked (T6, on the
        // family that is also a row).
        for flag in ["--help", "-h"] {
            let (surface, methods) = run(&format!("teton model {flag}"), &[], true);
            assert!(methods.is_empty(), "{:?}", surface.calls);
            let expected = crate::Cli::try_parse_from(["teton", "model", flag])
                .expect_err("clap reports help as an Err")
                .render()
                .to_string();
            assert_eq!(
                surface.lines_of(LineKind::Info),
                expected.lines().collect::<Vec<_>>(),
                "`teton model {flag}` did not render the family's own page: {:?}",
                surface.calls
            );
            assert!(
                surface.lines_of(LineKind::Error).is_empty(),
                "asking for help is not an error: {:?}",
                surface.calls
            );
            assert!(
                surface
                    .lines_of(LineKind::Info)
                    .iter()
                    .any(|line| line.starts_with("Usage: teton model")),
                "{:?}",
                surface.calls
            );
        }
    }

    /// **The classifier's flag spellings are the parser's** (verify residue,
    /// arch Minor). [`LEADING_GLOBAL_FLAGS`] is the set of spellings the
    /// classifier steps over ahead of a subcommand, and it is only right while
    /// it equals the set of `global = true` arguments the clap tree declares —
    /// a global flag added to `Cli` without an entry here would send `teton
    /// --new-flag doctor` to the model. So the set is derived from the tree
    /// and pinned both ways. [`CLI_FLAGS`] likewise: the help and version
    /// flags clap builds into the root, read off the built tree's own
    /// `Help`/`Version` actions.
    #[test]
    fn the_leading_global_flags_and_cli_flags_are_the_clap_trees_own() {
        use clap::CommandFactory;
        use std::collections::BTreeSet;

        fn spellings(arg: &clap::Arg) -> Vec<String> {
            let mut out = Vec::new();
            if let Some(long) = arg.get_long() {
                out.push(format!("--{long}"));
            }
            if let Some(short) = arg.get_short() {
                out.push(format!("-{short}"));
            }
            out
        }

        let mut root = crate::Cli::command();
        // `build` is what attaches clap's own `--help`/`--version` arguments to
        // the root; the derived arguments are there either way.
        root.build();

        let global_from_tree: BTreeSet<String> = root
            .get_arguments()
            .filter(|arg| arg.is_global_set())
            .flat_map(spellings)
            .collect();
        let global_pinned: BTreeSet<String> = LEADING_GLOBAL_FLAGS
            .iter()
            .map(|s| (*s).to_owned())
            .collect();
        assert_eq!(
            global_from_tree, global_pinned,
            "LEADING_GLOBAL_FLAGS must equal the clap tree's global arguments' spellings \
             (both directions): tree {global_from_tree:?} vs pinned {global_pinned:?}"
        );
        assert_eq!(
            LEADING_GLOBAL_FLAGS.len(),
            global_pinned.len(),
            "LEADING_GLOBAL_FLAGS carries a duplicate spelling"
        );

        let is_help_or_version = |arg: &&clap::Arg| {
            matches!(
                arg.get_action(),
                clap::ArgAction::Help
                    | clap::ArgAction::HelpShort
                    | clap::ArgAction::HelpLong
                    | clap::ArgAction::Version
            )
        };
        let cli_from_tree: BTreeSet<String> = root
            .get_arguments()
            .filter(is_help_or_version)
            .flat_map(spellings)
            .collect();
        let cli_pinned: BTreeSet<String> = CLI_FLAGS.iter().map(|s| (*s).to_owned()).collect();
        assert!(
            !cli_from_tree.is_empty(),
            "the built root declares no help/version arguments, so this pin holds over nothing"
        );
        assert_eq!(
            cli_from_tree, cli_pinned,
            "CLI_FLAGS must equal the root's own help/version spellings (both directions): \
             tree {cli_from_tree:?} vs pinned {cli_pinned:?}"
        );
        assert_eq!(
            CLI_FLAGS.len(),
            cli_pinned.len(),
            "CLI_FLAGS carries a duplicate spelling"
        );
    }
}

#[cfg(test)]
mod transcript_render_tests {
    use super::*;

    /// **Mutation (run 2026-09-03):** dropping the `degraded` suffix reddened
    /// the last assertion; restored.
    #[test]
    fn the_transcript_line_reports_state_path_count_and_degradation() {
        let on = SessionTranscriptResult {
            enabled: true,
            path: Some("/d/t/x.jsonl".to_owned()),
            records: 12,
            degraded: None,
        };
        assert_eq!(
            render_transcript(true, &on),
            "transcript: on — /d/t/x.jsonl (12 records)"
        );
        assert_eq!(
            render_transcript(false, &on),
            "transcript: on — recording to /d/t/x.jsonl"
        );
        let off = SessionTranscriptResult {
            enabled: false,
            path: Some("/d/t/x.jsonl".to_owned()),
            records: 12,
            degraded: None,
        };
        assert_eq!(render_transcript(false, &off), "transcript: off — stopped");
        let never = SessionTranscriptResult {
            enabled: false,
            path: None,
            records: 0,
            degraded: Some("directory refused: mode 0755".to_owned()),
        };
        assert_eq!(
            render_transcript(true, &never),
            "transcript: off — degraded: directory refused: mode 0755"
        );
    }
}
