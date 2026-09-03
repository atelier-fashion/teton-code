//! Skills — the user's own `/name` commands, read from four fixed directories
//! (REQ-585 BR-1/BR-2, ADR-4/ADR-5/ADR-6).
//!
//! A skill is a Markdown file with optional frontmatter that the user (or a
//! repo) put in one of four places:
//!
//! ```text
//! ~/.claude/skills/<name>/SKILL.md          source = user
//! ~/.claude/commands/<name>.md              source = user
//! <session-root>/.claude/skills/<name>/SKILL.md   source = project
//! <session-root>/.claude/commands/<name>.md       source = project
//! ```
//!
//! Typing `/<name> …` runs that file's body as one user-role prompt turn. This
//! module holds the half of that with no I/O in it beyond a seam: the shapes
//! ([`Skill`], [`SkillRegistry`], [`Skipped`]), the name rule, the permission
//! key, and the two bounds. [`discovery`] turns four directory listings into a
//! registry; [`frontmatter`] turns one file's bytes into a header and a body;
//! [`expand`] turns a registry row and a typed argument string into the one
//! [`Expansion`] a turn is composed from; [`dynamic`] holds the `` !`cmd` ``
//! grammar and the single I/O edge that runs those commands.
//!
//! # Nothing here is a setting
//!
//! A skill is **user-role content**. Every frontmatter key other than `name`,
//! `description`, `argument-hint` and the two invocation flags is inert and is
//! merely *listed* ([`Skill::ignored_keys`]): a file that says `model: opus` or
//! `allowed-tools: Bash` changes no routing decision, no permission level and
//! no boundary. That is the whole reason the parser can afford to be as narrow
//! as it is — nothing it reads can grant anything.
//!
//! The two flags REQ-587 BR-3 added ([`Skill::model_invocable`],
//! [`Skill::user_invocable`]) are not counter-examples: neither grants
//! anything either. They only ever **narrow** who may reach a body the user
//! already put on their own disk, and the parser's unreadable-value readings
//! narrow in the same direction (`frontmatter`'s module doc holds the table).
//!
//! # Two resolvers, because there are two questions (REQ-587 ADR-12)
//!
//! "May this be invoked" stopped being one question when BR-3 gave a row two
//! flags, so it is asked through two names and never one:
//!
//! | row | `dispatchable_by_user` | `invocable_by_model` |
//! |---|---|---|
//! | ordinary | yes | yes |
//! | `user-invocable: false` — **model-only** | no | yes |
//! | `disable-model-invocation: true` — hidden from the model | yes | no |
//! | both flags, or shadowed | no | no |
//!
//! [`Skill::is_dispatchable`] keeps its REQ-585 meaning — *nothing shadows this
//! name* — and **neither flag is folded into it**. Folding `user_invocable`
//! there is the arm that reads correct and kills the model-only row silently:
//! `/delta` refuses, `/help` still lists it, and the model's call for it
//! resolves to nothing, with no assertion anywhere going red. It also loses the
//! name contest in [`assemble`], which quietly hands `/delta` to a *different
//! file*.
//!
//! For the **user's** question the answer is three-valued, not two — allowed,
//! shadowed, model-only — and that is [`Skill::user_dispatch`]. Its precedence,
//! shadowing before model-only, is decided there rather than at each surface
//! that renders a mark.
//!
//! # Why this lives at `src/skills/`, not under `harness/`
//!
//! `sessions.rs` owns the registry's lifetime, `server.rs` answers
//! `skills/list` from it and `runtime.rs` expands a turn out of it. A module
//! three non-harness callers reach for does not belong inside the harness.

pub mod discovery;
pub mod dynamic;
pub mod expand;
pub mod frontmatter;

use std::fmt;
use std::path::{Path, PathBuf};

use teton_protocol::methods::RootKind;

pub use discovery::{discover, provenance_of, DirLister, Entry, ListError, ReadError, RealFs};
pub use dynamic::{run_all, Command, DynamicOutcome};
pub use expand::{expand, Expansion, Pending, PENDING_PLACEHOLDER};

/// Which of the two discovery roots a skill was found under.
///
/// Re-exported from the protocol rather than re-declared here: the registry's
/// rows go on the wire as `SkillView`, the source is half the permission key
/// (ADR-6), and a daemon-local twin would need a mapping function that can
/// drift from the wire spelling the moment either side gains a variant
/// (LESSON-528 — two spellings of one decision are identical only until one of
/// them is edited).
pub use teton_protocol::methods::SkillSource;

/// The largest skill file discovery will read: 128 KiB.
///
/// A file past it is **named** (`over 128 KiB (N B)`), never silently truncated
/// — half a body is a body the user did not write. The figure is generous for
/// a prompt (the largest shipped ADLC skill is a few tens of KiB) and small
/// enough that four directories' worth cannot be a memory event.
///
/// It was 64 KiB while the local byte budget was 32,768 B. The ceiling has to
/// sit **well above** that budget (`harness::budget::derive`'s local arm, now
/// 63,488 B): a body between the budget and the ceiling is *measured* and
/// draws REQ-589's over-budget offer, whose remedy is to bind the tier remote,
/// while a body past the ceiling is *skipped* at discovery and never reaches
/// that door. At 64 KiB the two would have coincided, and the offer would have
/// had a band of a few hundred bytes to fire in.
pub const SKILL_MAX_BYTES: u64 = 128 * 1024;

/// The most entries discovery will consider under one root: 512.
///
/// The bound exists because a root is a *user-supplied path* — `~/.claude` can
/// be a symlink into anything, including a directory with a hundred thousand
/// entries — and discovery runs on the session-create path, where a user is
/// waiting. Hitting it is a named diagnostic (`root truncated at 512 entries`),
/// and entries are sorted **before** the cap applies so which 512 survive does
/// not depend on the filesystem's listing order (LESSON-540).
pub const MAX_ENTRIES_PER_ROOT: usize = 512;

/// The most `` !`command` `` slots one skill body may declare (BUG-185).
///
/// Nothing bounded the count before, and `run_all` runs every slot
/// sequentially with its own 30 s timeout inside one `spawn_blocking`. A body
/// holding thousands of slots turned **one** approved consent — or one `/name`
/// at `full`, the documented automation posture — into hours of wall time on a
/// blocking-pool thread that cannot be cancelled, wedging the session
/// (`SESSION_BUSY` for every later prompt) and holding the daemon awake through
/// its `ActivityGuard`.
///
/// Refused at **discovery**, in the shape [`MAX_ENTRIES_PER_ROOT`] already uses:
/// the row never registers, so the file is never invocable and its commands
/// never reach a consent prompt. That is what also closes the consent-flooding
/// surface — a hostile project skill cannot list 400 innocuous commands with a
/// dangerous one buried, because it is not listed at all.
///
/// 32 against a shipped maximum of 6 (`template-drift`): generous enough that
/// no real skill is near it, small enough that the worst case is bounded even
/// before [`crate::skills::dynamic::INVOCATION_BUDGET_MS`] applies.
pub const MAX_DYNAMIC_COMMANDS: usize = 32;

/// The longest dispatchable name: 64 characters.
pub const MAX_NAME_LEN: usize = 64;

/// One discovered, registered command.
///
/// `path` is the file actually read, absolute and **local-only**: it is the
/// jail-relative fact the expander needs and the identity provenance is minted
/// from. [`Skill::path_display`] beside it is the only spelling allowed on a
/// surface. An absolute path carries a username, or the location of the user's
/// working tree, into a transcript or a remote payload; this struct is the last
/// place it is allowed to be one.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Skill {
    /// The dispatchable spelling: the directory name (`skills/<name>/SKILL.md`)
    /// or the file stem (`commands/<name>.md`), always matching
    /// [`is_valid_skill_name`].
    pub name: String,
    /// Which root it came from.
    pub source: SkillSource,
    /// The file that was read.
    pub path: PathBuf,
    /// [`Skill::path`] as a surface may spell it (BR-1's entity table): a
    /// **project** skill relative to the session root
    /// (`.claude/skills/x/SKILL.md`), a **user** skill relative to the home
    /// folder (`~/.claude/skills/x/SKILL.md`), and — only when neither base
    /// applies — whatever `session_root::display_under` could not shorten.
    ///
    /// **Derived here, by the one function that knows all three inputs.**
    /// Discovery holds the source, the session root and `HOME` together;
    /// nothing downstream does. `skills/list` answers from a stored snapshot
    /// and has no root to hand, and the turn path had only `HOME` — which is
    /// why a project skill under a root outside `$HOME` reached the wire
    /// absolute until BUG-187. Built once, at the root that decided it, so two
    /// surfaces cannot spell one file two ways (REQ-583 ADR-2).
    ///
    /// Unbounded here and bounded at each surface, exactly as the description
    /// is: this is a *path*, not a rendering, and the character ceiling belongs
    /// where the rendering happens (`DISPLAY_MAX_CHARS`).
    pub path_display: String,
    /// The frontmatter `description`, verbatim; bounded and neutralized by the
    /// surface that renders it, not here.
    pub description: Option<String>,
    /// The frontmatter `argument-hint`, verbatim.
    pub argument_hint: Option<String>,
    /// Everything after the frontmatter block, verbatim (BR-13 — the body is
    /// passed as written).
    pub body: String,
    /// Whether the model may invoke this skill through the `skill` tool
    /// (REQ-587 BR-3) — `false` for `disable-model-invocation: true`, and for a
    /// value of that key this parser could not read.
    ///
    /// The **flag as the file wrote it**, not the composed answer: ask
    /// [`Skill::invocable_by_model`] for whether the model can actually reach
    /// this row, because a shadowed row's name resolves elsewhere whatever its
    /// frontmatter says.
    pub model_invocable: bool,
    /// Whether the user may dispatch this skill by typing `/name` (REQ-587
    /// BR-3) — `false` for `user-invocable: false`, which is the *model-only*
    /// state: listed by `/help`, marked, and refused from `/name`.
    ///
    /// The flag as the file wrote it, with the same caveat as
    /// [`Skill::model_invocable`]: [`Skill::dispatchable_by_user`] is the
    /// composed answer.
    pub user_invocable: bool,
    /// Frontmatter keys Teton does not honor, in the order they appeared.
    /// Listed by `/verbose`; otherwise inert.
    ///
    /// An invocation flag appears here only when its *value* was not a boolean
    /// literal — the file registered, the flag took its safe reading, and this
    /// is where the user is told (`frontmatter`'s module doc).
    pub ignored_keys: Vec<String>,
    /// Set when the frontmatter declared a `name` that is not the one the file
    /// dispatches under — a note for `/verbose`, never a second spelling
    /// (BR-2: one spelling reaches one handler).
    pub name_note: Option<String>,
    /// What owns this name instead, when something does. `Some` means **listed
    /// but never dispatchable**: `/help` marks the row and `classify` must not
    /// return it.
    pub shadowed: Option<ShadowedBy>,
}

// NB: there is deliberately no `Skill::permission_key()` (REQ-587 BR-5).
// REQ-585 shipped one, and it read as *the* key a skill's dynamic context runs
// under. BR-5 made that false: the key a grant is remembered under is
// `Expansion::grant_key`, which carries a digest of the **substituted** command
// set whenever the arguments had a hand in it, and only the expansion holds
// those facts. A method on the row that answered the *base* key was a spelling
// both call sites could reach for and neither would notice using — the gate
// accepts either and pins whichever it is given, so the mistake keeps REQ-585's
// behaviour with nothing red. [`permission_key_for`] is still here for the
// surfaces whose question really is "the base key for this (source, name)".
impl Skill {
    /// True when nothing shadows this row — i.e. this file owns its name.
    ///
    /// **Exactly that, and not "may be invoked".** REQ-587 BR-3's two flags are
    /// deliberately *not* folded in here (ADR-12), and the reason is that this
    /// predicate has a second caller: [`assemble`] uses it to decide the name
    /// contest. A model-only row must still **win** its name — it is the file
    /// that answers `delta` — so folding `user_invocable` in would let a
    /// lower-precedence file quietly take the spelling for the user while the
    /// model kept reaching the first one. Ask [`Self::dispatchable_by_user`] or
    /// [`Self::invocable_by_model`] for who may invoke it.
    #[must_use]
    pub fn is_dispatchable(&self) -> bool {
        self.shadowed.is_none()
    }

    /// True when the **user** may reach this row by typing `/name`: it owns its
    /// name and its frontmatter did not say `user-invocable: false`.
    #[must_use]
    pub fn dispatchable_by_user(&self) -> bool {
        matches!(self.user_dispatch(), UserDispatch::Allowed)
    }

    /// True when the **model** may reach this row through the `skill` tool: it
    /// owns its name and its frontmatter did not say
    /// `disable-model-invocation: true`.
    ///
    /// The shadowing half is not decoration. A hidden `alpha` that shadows a
    /// second `alpha` must not let the *loser* answer the model's call for
    /// `alpha`: the name resolves to one file or to none.
    #[must_use]
    pub fn invocable_by_model(&self) -> bool {
        self.is_dispatchable() && self.model_invocable
    }

    /// Which of BR-3's three states this row is in, for the **user's**
    /// question.
    ///
    /// One home for the precedence, decided here rather than at each surface
    /// that renders a mark: **shadowing wins**. A row that is both shadowed and
    /// model-only reads as shadowed, because that is the stronger and more
    /// actionable fact — the name belongs to another file entirely, so
    /// "model-only" would name a capability this row does not have either.
    ///
    /// On the wire the two facts ride separately (`SkillView::shadowed` and
    /// `SkillView::user_invocable`, both verbatim), so a client composing a
    /// `/help` mark applies this same order and does not need a predicate of
    /// its own.
    #[must_use]
    pub fn user_dispatch(&self) -> UserDispatch {
        // The precedence lives in `teton_protocol` (BUG-192), because the
        // client composes the same three states from the same two wire facts
        // and the two copies could drift with both suites green.
        teton_protocol::methods::user_dispatch(self.shadowed, self.user_invocable)
    }

    /// The diagnostic this row carries when something else owns its name, in
    /// the same vocabulary [`SkipReason`] uses for everything else discovery
    /// declined — so `/help`'s row mark and the skipped list cannot drift into
    /// two spellings of one fact.
    ///
    /// Answers the **shadowing** question only, and stays `None` for a
    /// model-only row: BR-3's third state is not a kind of shadowing (nothing
    /// took the name, and the model still reaches the file), and folding it in
    /// would put "model-only" into a sentence that reads `shadowed by …` at
    /// every surface that renders one. [`Self::user_dispatch`] is the
    /// three-valued answer.
    #[must_use]
    pub fn shadow_reason(&self) -> Option<SkipReason> {
        self.shadowed.map(|by| SkipReason::Shadowed { by })
    }
}

/// Whether a registered row is the **user's** to type, and why not when it is
/// not (REQ-587 BR-3).
///
/// Three states in one value, because `Option<…>` is two and BR-3 has three:
/// the shape a caller reaches for when it must tell "another file owns this
/// name" from "this file is the model's, not yours". See
/// [`Skill::user_dispatch`] for the precedence between them.
///
/// The daemon's spelling of the shared three-state answer, carrying its typed
/// [`ShadowedBy`]. The **rule** — shadowing wins over model-only — is
/// `teton_protocol::methods::user_dispatch`, one home for both sides (BUG-192).
pub type UserDispatch = teton_protocol::methods::UserDispatch<ShadowedBy>;

/// What beat a skill to its name (BR-2, ADR-6).
///
/// The third shadowing case — a **reserved** built-in name — is decided by the
/// client, where the built-in table lives (ADR-12 derives the reserved set from
/// `COMMANDS`), so it has no variant here. The daemon knows only the two
/// contests it can see from four directory listings.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ShadowedBy {
    /// A project skill of the same name. Project beats user (BR-2): the repo
    /// you are standing in is the more specific answer.
    ProjectSkill,
    /// The `skills/` entry of the same name **in the same source**. The four
    /// globs make `~/.claude/skills/status/SKILL.md` and
    /// `~/.claude/commands/status.md` a legal pair — same name, same source and
    /// therefore the same permission key — so one of them has to lose, or a
    /// remembered grant would authorize whichever file happened to win and move
    /// silently to the other if that ever changed (ADR-6, LESSON-495).
    SkillsDirectory,
    /// A built-in command of the same name (BR-2's third case).
    ///
    /// Decided here rather than only in the client, which is where it used to
    /// live: the registry's user-facing resolver — [`SkillRegistry::
    /// dispatchable_by_user`], spelled `dispatchable` at the time — answered
    /// for a skill named `cost`,
    /// so a client that does not carry `teton`'s table — the phase-2 one, or
    /// any third party — could dispatch a repo-supplied `.claude/skills/cost`
    /// by name. ADR-1's rule is that every rule with teeth lives in the daemon,
    /// and this one had none here (REQ-585 verify). The names come from
    /// `teton_protocol::methods::RESERVED_SKILL_NAMES`, which `teton` asserts is
    /// its own derivation from `COMMANDS`.
    Builtin,
}

impl fmt::Display for ShadowedBy {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::ProjectSkill => f.write_str("a project skill of the same name"),
            Self::SkillsDirectory => f.write_str("the skills/ entry of the same name"),
            // Deliberately not naming *which* built-in: the daemon knows only
            // that the table claims the name, not whether it is a row, an alias
            // or a family word. The client, which has the table, replaces this
            // with the specific sentence (`table_claim`'s `words()`); a client
            // without one still gets a true mark.
            Self::Builtin => f.write_str("a built-in command of the same name"),
        }
    }
}

/// One entry discovery found and did not register, with why.
///
/// `path` is the file (or, for a root-level refusal, the directory) as
/// discovery opened it. Like [`Skill::path`] it is absolute here, and
/// [`Skipped::path_display`] beside it is what a surface may show.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Skipped {
    /// What was skipped.
    pub path: PathBuf,
    /// [`Skipped::path`] as a surface may spell it, under the rule
    /// [`Skill::path_display`] states — and for the same reason: `/help`'s
    /// skipped list is a user-visible surface and a screenshot of it must not
    /// carry a username or a working-tree location (BR-1's entity table).
    ///
    /// A root-level refusal (an unreadable directory, a truncated root) spells
    /// the directory, so the diagnostic still names which of the four it was.
    pub path_display: String,
    /// The spelling this entry would have been typed as, when discovery got far
    /// enough to know one — `None` for a whole root (refused, or truncated),
    /// which names no single skill.
    ///
    /// Carried rather than re-derived from [`Skipped::path`]. BR-2's naming
    /// rule (directory name for `skills/`, file stem for `commands/`) belongs
    /// to discovery, and a reader that reconstructs it owns a second copy that
    /// can disagree — LESSON-546's shape. It is also strictly weaker: the path
    /// cannot express the name of a symlinked `commands/<name>` entry, which
    /// discovery refuses before it ever becomes a `.md` file it opened.
    ///
    /// **Untrusted.** In the [`SkipReason::InvalidName`] case this *is* the
    /// invalid spelling, so every surface bounds it (BR-3).
    pub name: Option<String>,
    /// Why.
    pub reason: SkipReason,
}

/// Why an entry discovery found is not a registered skill (BR-1, ADR-4).
///
/// **Named, never silent.** A skill that vanishes without a diagnostic is the
/// LESSON-481 shape: a feature the user cannot see is one the suite cannot see
/// either, and the failure mode this taxonomy exists for — a `~/Documents`
/// symlink behind macOS's TCC consent — is invisible by construction.
///
/// Two things deliberately produce **no** entry, because they are the normal
/// case rather than a fault: a root that does not exist, and a directory under
/// a `skills/` root with no `SKILL.md` in it (the ADLC toolkit's `agents/`,
/// `partials/` and `templates/` are not broken skills; they are not skills).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SkipReason {
    /// The path could not be opened. The parenthetical names the cause this
    /// reason exists for: a root that resolves into a consent-guarded tree
    /// (`~/Documents` on macOS) is refused with `EPERM` until the user answers
    /// a dialog Teton never sees.
    Unreadable,
    /// Past [`SKILL_MAX_BYTES`]. Carries the file's real size, because "too
    /// big" without a figure does not tell the user what to cut.
    Oversize {
        /// The file's size in bytes, as `metadata` reported it.
        bytes: u64,
    },
    /// The file is not valid UTF-8.
    NotUtf8,
    /// The frontmatter block is not one this parser can read whole (ADR-5).
    MalformedFrontmatter,
    /// The directory name or file stem does not match [`is_valid_skill_name`].
    InvalidName,
    /// The entry is a symlink. A user root is followed; entries are not (BR-1).
    SymlinkEntry,
    /// A **project** root resolves outside the session root (REQ-587 BR-10).
    ///
    /// Reported against the root, like [`Self::RootTruncated`], and never for a
    /// **user** root: `~/.claude/skills` being a symlink into a checked-out
    /// toolkit is the shape the follow-the-root exemption exists for, and it is
    /// the home directory's shape, not a repository's. A cloned repo's
    /// `.claude/commands -> ../../..` is refused here rather than registering
    /// every `*.md` under the target as a project skill the model may call.
    EscapingRoot,
    /// Something else already owns this name.
    Shadowed {
        /// What owns it.
        by: ShadowedBy,
    },
    /// The root held more than [`MAX_ENTRIES_PER_ROOT`] entries; the rest were
    /// not considered. Reported once, against the root.
    RootTruncated,
    /// The body declares more than [`MAX_DYNAMIC_COMMANDS`] `` !`command` ``
    /// slots (BUG-185). Carries the real count, for the reason
    /// [`Self::Oversize`] carries its byte figure: "too many" without a number
    /// does not tell the author what to cut.
    TooManyCommands {
        /// How many slots the body declares.
        count: usize,
    },
}

impl fmt::Display for SkipReason {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Unreadable => f.write_str("unreadable (permission denied)"),
            Self::Oversize { bytes } => write!(
                f,
                "over {} KiB ({} B)",
                SKILL_MAX_BYTES / 1024,
                teton_protocol::events::thousands(*bytes)
            ),
            Self::NotUtf8 => f.write_str("not UTF-8"),
            Self::MalformedFrontmatter => f.write_str("malformed frontmatter"),
            Self::InvalidName => f.write_str("invalid name"),
            Self::SymlinkEntry => f.write_str("symlink not followed"),
            Self::EscapingRoot => f.write_str("resolves outside the session root"),
            Self::Shadowed { by } => write!(f, "shadowed by {by}"),
            Self::RootTruncated => {
                write!(f, "root truncated at {MAX_ENTRIES_PER_ROOT} entries")
            }
            Self::TooManyCommands { count } => write!(
                f,
                "declares {count} dynamic commands, over the limit of {MAX_DYNAMIC_COMMANDS}"
            ),
        }
    }
}

/// Every skill this session knows about, and everything it found and declined.
///
/// A **snapshot**, not a live view: it is built at `session/create` and rebuilt
/// when the session root moves (`/cd`). There is no file watcher — a registry
/// that changed under a turn would make "the body you consented to" and "the
/// body that ran" two different files.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SkillRegistry {
    skills: Vec<Skill>,
    skipped: Vec<Skipped>,
    read_under: Option<PathBuf>,
}

impl SkillRegistry {
    /// Every registered skill, **including** the shadowed ones, ordered by
    /// name.
    ///
    /// Shadowed rows are here rather than in [`Self::skipped`] because BR-3
    /// requires `/help` to show them — a name that silently does nothing is
    /// worse than one marked as taken — and because they are otherwise
    /// complete: they have a body, a source and a path. `classify` filters on
    /// [`Skill::is_dispatchable`]; `/help` prints the mark.
    ///
    /// The order is the daemon's, not the client's: APFS lists in hash order
    /// and ext4 does not, so an order re-derived downstream would be a
    /// platform-flaky `/help` (LESSON-540).
    #[must_use]
    pub fn skills(&self) -> &[Skill] {
        &self.skills
    }

    /// Everything found and not registered, in discovery order.
    #[must_use]
    pub fn skipped(&self) -> &[Skipped] {
        &self.skipped
    }

    /// The skill a **user's** `/name` reaches, or `None` when nothing does —
    /// including when a row of that name exists but is shadowed, or is
    /// model-only (REQ-587 BR-3).
    ///
    /// Named for its caller rather than for its filter, per ADR-12. REQ-585 had
    /// one resolver, `dispatchable`, and one caller; BR-3 gives the registry
    /// two questions whose answers differ for exactly one state, and a resolver
    /// spelled `dispatchable` is the one the `skill` tool would have reached
    /// for by reflex — returning `unknown_skill` for every model-only skill,
    /// with nothing red anywhere.
    #[must_use]
    pub fn dispatchable_by_user(&self, name: &str) -> Option<&Skill> {
        self.skills
            .iter()
            .find(|skill| skill.name == name && skill.dispatchable_by_user())
    }

    /// The skill a **model's** `skill { name }` call reaches, or `None` — the
    /// other half of ADR-12's pair.
    ///
    /// `None` here and `Some` from [`Self::dispatchable_by_user`] is BR-3's
    /// `disable-model-invocation: true`; the reverse is `user-invocable:
    /// false`. Both are ordinary registered rows and both are listed by
    /// `/help`.
    #[must_use]
    pub fn invocable_by_model(&self, name: &str) -> Option<&Skill> {
        self.skills
            .iter()
            .find(|skill| skill.name == name && skill.invocable_by_model())
    }

    /// True when nothing was found at all — the state a session on a machine
    /// with no `~/.claude` is in, and the state the version handshake puts a
    /// new client against an old daemon in (ADR-2).
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.skills.is_empty() && self.skipped.is_empty()
    }

    /// The session root **as discovery resolved it** — the tree every project
    /// body in this snapshot was read out of — or `None` when it did not
    /// resolve (REQ-589 D-13).
    ///
    /// # Why the snapshot carries this at all
    ///
    /// Because a snapshot's bodies and the identity that authorizes them must be
    /// one fact, and for one release they were two. Bodies are read **once**, at
    /// `session/create` and at `/cd`
    /// (`discovery_is_paid_at_create_and_at_cd_and_never_per_turn`); the durable
    /// trust name was minted **per turn**, by canonicalising the path the
    /// session registry stores — which `ProbedRoot::probe` deliberately leaves
    /// unresolved. So a link at the session root could be re-pointed at any
    /// point in the session's life, and the name minted afterwards named a tree
    /// the bodies had never been read from: an unlisted repository's text
    /// running unattended on a row written for somebody else's repository.
    /// `a_root_re_pointed_after_discovery_cannot_spend_the_listed_trees_trust`
    /// is that attack.
    ///
    /// This is the resolution [`discovery::discover`] took *once* and used for
    /// its own containment test: every project root it listed resolved to at or
    /// under this path. The trust door names **this**, never the path as
    /// spelled, which is what makes the identity that authorizes the bodies the
    /// identity the bodies were read under.
    ///
    /// The window that leaves is discovery's own, and it is inherent to reading
    /// by path: a root is resolved for the containment test and then listed and
    /// read through its unresolved spelling, so a link re-pointed *between those
    /// two syscalls* still moves the bytes. That is microseconds inside one
    /// function rather than the whole life of a session, it is the same race
    /// [`discovery::discover`]'s containment test has always had, and closing it
    /// would take `openat`-relative reads off a held descriptor rather than a
    /// name. What this field removes is the part that was not a race at all —
    /// an identity minted hours later, from a path nothing had been read
    /// through.
    ///
    /// `None` is fail-closed twice over: a session root that will not resolve
    /// mints no durable name and matches no row, *and* registers no project
    /// skill at all, because the containment test has nothing to compare
    /// against.
    #[must_use]
    pub fn read_under(&self) -> Option<&Path> {
        self.read_under.as_deref()
    }
}

/// The permission-gate key a skill's dynamic context runs under:
/// `skill:<source>:<name>` (ADR-6).
///
/// # Why the source is in the key
///
/// LESSON-495's rule is that a remembered key must encode the **whole**
/// question. `skill:analyze` does not: after a `/cd` the same string names a
/// different file, so a grant remembered in one repo would silently authorize
/// another repo's commands. Encoding the source narrows the collision to
/// project-vs-project, and dropping every `skill:project:` grant at `/cd`
/// closes that (carried state sheds its invariants silently — LESSON-501).
///
/// It is never the `shell` tool's key, in either direction: a remembered
/// `shell` grant must not un-ask a skill's commands, and a remembered skill
/// grant must not free a model-issued `shell` call.
#[must_use]
pub fn permission_key_for(source: SkillSource, name: &str) -> String {
    // Delegated, not re-spelled: the client memoizes an answer under this exact
    // string and must forget it at the same moment the daemon does, so the
    // spelling lives above both crates (`teton_protocol::methods`).
    teton_protocol::methods::skill_permission_key(source, name)
}

/// The wire and key spelling of a source: `user` or `project`.
#[must_use]
pub fn source_word(source: SkillSource) -> &'static str {
    match source {
        SkillSource::User => "user",
        SkillSource::Project => "project",
    }
}

/// True when `name` may be dispatched: `^[a-z0-9][a-z0-9_-]{0,63}$`.
///
/// Deliberately narrower than what a filesystem allows. The name is typed after
/// a `/` at a terminal and matched against the built-in table, so it may not
/// contain whitespace (which would make `/two words` ambiguous with an
/// argument), a `/` (which would make it a path), a `.` (which would make
/// `foo.md` and `foo` two spellings of one file) or an uppercase letter (which
/// would make `/Status` and `/status` two names on a case-insensitive
/// filesystem and one on a case-sensitive one).
///
/// A directory or stem that fails this is **named** rather than ignored: the
/// user who wrote `~/.claude/commands/Deploy Prod.md` gets told why `/deploy`
/// is not there.
#[must_use]
pub fn is_valid_skill_name(name: &str) -> bool {
    let mut chars = name.chars();
    let Some(first) = chars.next() else {
        return false;
    };
    if !first.is_ascii_lowercase() && !first.is_ascii_digit() {
        return false;
    }
    name.len() <= MAX_NAME_LEN
        && chars.all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-' || c == '_')
}

/// Build a registry from candidates already in precedence order (highest
/// first), running the name contest and ordering the result.
///
/// Kept here beside [`ShadowedBy`] rather than in [`discovery`]: it is the
/// *rule* half of BR-2 and has nothing to do with the filesystem.
fn assemble(candidates: Vec<Skill>, skipped: Vec<Skipped>) -> SkillRegistry {
    let mut skills: Vec<Skill> = Vec::with_capacity(candidates.len());
    for mut candidate in candidates {
        // The table wins before any file does — a skill can never take a
        // built-in's name, whatever the other files say (BR-2).
        if teton_protocol::methods::is_reserved_skill_name(&candidate.name) {
            candidate.shadowed = Some(ShadowedBy::Builtin);
            skills.push(candidate);
            continue;
        }
        // `is_dispatchable`, and deliberately not `dispatchable_by_user`: the
        // contest is about who **owns the spelling**, which a model-only row
        // does. A row that lost this contest is not the model's either
        // (`invocable_by_model` reads the same predicate), so a name still
        // resolves to one file or to none, for either caller.
        if let Some(winner) = skills
            .iter()
            .find(|s| s.name == candidate.name && s.is_dispatchable())
        {
            candidate.shadowed = Some(if winner.source == candidate.source {
                ShadowedBy::SkillsDirectory
            } else {
                ShadowedBy::ProjectSkill
            });
        }
        skills.push(candidate);
    }
    // Stable, and by name only: rows that share a name keep the precedence
    // order they arrived in, so the dispatchable one is always the first of its
    // group and `/help` reads winner-then-shadowed.
    skills.sort_by(|a, b| a.name.cmp(&b.name));
    // `None` here rather than a parameter: the contest is the *rule* half of
    // BR-2 and knows nothing about the filesystem. [`discovery::discover`] is
    // the one caller that resolved anything, and it fills the field in.
    SkillRegistry {
        skills,
        skipped,
        read_under: None,
    }
}

/// The four roots, in **precedence order** (highest first), for a session on
/// `session_root` with `home` as the user's home directory.
///
/// `RootKind::Home` yields the user pair only. That is AC-3's de-dup: a session
/// whose root *is* `$HOME` would otherwise reach `~/.claude/skills` twice and
/// register every skill under both sources — every name shadowing itself, and
/// two permission keys for one file.
///
/// The kind is the authority, not a path comparison of our own:
/// `teton_core::session_root::classify` already decided what this root is, and
/// a second predicate spelling the same decision here is the mirrored shape
/// LESSON-528 is about.
fn roots(home: Option<&Path>, session_root: &Path, root_kind: RootKind) -> Vec<RootSpec> {
    let mut specs = Vec::with_capacity(4);
    if root_kind != RootKind::Home {
        // A **project** skill is spelled relative to the session root — the
        // directory the banner, the environment block and every jail refusal
        // already name — so `.claude/skills/x/SKILL.md` is the whole of what a
        // surface needs to say, and it says it identically whether the checkout
        // is under `$HOME`, on an external volume or in a CI workspace
        // (BUG-187: only the first of those had a spelling before).
        specs.push(RootSpec::new(
            SkillSource::Project,
            Shape::Skills,
            session_root,
            Some(session_root),
            home,
        ));
        specs.push(RootSpec::new(
            SkillSource::Project,
            Shape::Commands,
            session_root,
            Some(session_root),
            home,
        ));
    }
    if let Some(home) = home {
        // A **user** skill is spelled `~/…`, and is given no base on purpose:
        // it is not part of the project the session stands in, and a session
        // root that happens to be an ancestor of `$HOME` (`/Users`, `/`) would
        // otherwise turn `~/.claude/skills/x/SKILL.md` into
        // `someone/.claude/skills/x/SKILL.md` — the username this rule exists
        // to keep off a surface.
        specs.push(RootSpec::new(
            SkillSource::User,
            Shape::Skills,
            home,
            None,
            Some(home),
        ));
        specs.push(RootSpec::new(
            SkillSource::User,
            Shape::Commands,
            home,
            None,
            Some(home),
        ));
    }
    specs
}

/// One of the four directories discovery opens, and the spelling every row
/// found under it is displayed with.
///
/// The display rule travels with the root rather than being re-decided per row:
/// which base a path is shown relative to is a property of *which of the four
/// this is* (BUG-187), and [`roots`] is where that is known.
struct RootSpec {
    source: SkillSource,
    shape: Shape,
    dir: PathBuf,
    /// What paths from this root are spelled relative to, or `None` for the
    /// home-relative rule. See [`roots`] for why the two sources differ.
    display_base: Option<PathBuf>,
    /// The home folder, for [`teton_core::session_root::display_under`]'s
    /// fall-through — carried so [`RootSpec::display`] is total and no caller
    /// downstream has to hold `HOME` to spell a path.
    home: Option<PathBuf>,
}

impl RootSpec {
    fn new(
        source: SkillSource,
        shape: Shape,
        base: &Path,
        display_base: Option<&Path>,
        home: Option<&Path>,
    ) -> Self {
        let dir = base.join(".claude").join(match shape {
            Shape::Skills => "skills",
            Shape::Commands => "commands",
        });
        Self {
            source,
            shape,
            dir,
            display_base: display_base.map(Path::to_path_buf),
            home: home.map(Path::to_path_buf),
        }
    }

    /// How a surface may spell `path`, which came from this root.
    ///
    /// The one derivation ([`Skill::path_display`]): every `Skill` and every
    /// `Skipped` row gets its display from here, so a registered skill and a
    /// skipped one under the same root cannot disagree about how their
    /// directory is written.
    fn display(&self, path: &Path) -> String {
        teton_core::session_root::display_under(
            path,
            self.display_base.as_deref(),
            self.home.as_deref(),
        )
    }
}

/// Which of the two layouts a root holds.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Shape {
    /// `<root>/<name>/SKILL.md` — a directory per skill.
    Skills,
    /// `<root>/<name>.md` — a file per skill.
    Commands,
}

impl Shape {
    /// The name an entry called `entry_name` would dispatch under, or `None`
    /// when the entry is not a candidate at all under this shape.
    ///
    /// One home for "where a skill's name comes from", so the two shapes cannot
    /// disagree about it: a `skills/` entry is named by its **directory**, a
    /// `commands/` entry by its **file stem** — and a `commands/` entry that is
    /// not Markdown is not a candidate, rather than a candidate with a bad name.
    fn name_of(self, entry_name: &str) -> Option<&str> {
        match self {
            Self::Skills => Some(entry_name),
            Self::Commands => entry_name.strip_suffix(".md"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// An ordinary registered row: it owns its name and both flags are on.
    fn row(name: &str, source: SkillSource) -> Skill {
        Skill {
            name: name.to_owned(),
            source,
            path: PathBuf::from("/h/.claude/skills")
                .join(name)
                .join("SKILL.md"),
            // BUG-187: the spelling a surface shows, carried on the row.
            path_display: format!("~/.claude/skills/{name}/SKILL.md"),
            description: None,
            argument_hint: None,
            body: String::new(),
            model_invocable: true,
            user_invocable: true,
            ignored_keys: Vec::new(),
            name_note: None,
            shadowed: None,
        }
    }

    /// **ADR-12, both directions on one fixture.** `user-invocable: false` is a
    /// third state, not a second spelling of "not dispatchable": the user's
    /// `/delta` must refuse **and** the model's `skill { name: "delta" }` must
    /// resolve, off the same row.
    ///
    /// Every assertion but the middle one stays green if `user_invocable` is
    /// folded into `is_dispatchable` — the refusal, the listing, and even a
    /// `/help` mark — which is exactly why the model's half is asserted here
    /// and not left to the tool's task.
    #[test]
    fn a_model_only_skill_refuses_the_user_and_still_resolves_for_the_model() {
        let mut delta = row("delta", SkillSource::User);
        delta.user_invocable = false;
        let registry = assemble(vec![delta], Vec::new());

        assert!(
            registry.dispatchable_by_user("delta").is_none(),
            "`/delta` must refuse: the file says the user may not type it"
        );
        assert!(
            registry.invocable_by_model("delta").is_some(),
            "…and the model must still reach it — this is BR-3's model-only \
             state, and it is dead the moment `user_invocable` is folded into \
             `is_dispatchable`"
        );
        assert_eq!(registry.skills().len(), 1, "still listed by `/help` (AC-1)");

        let delta = &registry.skills()[0];
        assert!(
            delta.is_dispatchable(),
            "`is_dispatchable` answers the shadowing question only (ADR-12); \
             nothing shadows this row"
        );
        assert_eq!(delta.shadow_reason(), None, "and it carries no shadow mark");
        assert_eq!(
            delta.user_dispatch(),
            UserDispatch::ModelOnly,
            "the third state, named"
        );
    }

    /// The mirror image: `disable-model-invocation: true` takes the model's
    /// half and leaves the user's alone.
    #[test]
    fn a_hidden_skill_refuses_the_model_and_still_dispatches_for_the_user() {
        let mut beta = row("beta", SkillSource::User);
        beta.model_invocable = false;
        let registry = assemble(vec![beta], Vec::new());

        assert!(registry.invocable_by_model("beta").is_none());
        assert!(registry.dispatchable_by_user("beta").is_some());
        assert_eq!(registry.skills()[0].user_dispatch(), UserDispatch::Allowed);
    }

    /// Both flags off is a real state — invocable by nobody — and it is a
    /// **listed row with a named state**, not a silent drop (BR-3).
    #[test]
    fn a_row_both_flags_deny_is_listed_and_invocable_by_nobody() {
        let mut nobody = row("nobody", SkillSource::Project);
        nobody.model_invocable = false;
        nobody.user_invocable = false;
        let registry = assemble(vec![nobody], Vec::new());

        assert_eq!(registry.skills().len(), 1, "listed, not dropped");
        assert!(registry.dispatchable_by_user("nobody").is_none());
        assert!(registry.invocable_by_model("nobody").is_none());
        assert_eq!(
            registry.skills()[0].user_dispatch(),
            UserDispatch::ModelOnly,
            "the user's half of the answer is still model-only, and the model's \
             half is a separate question the row answers `false` to"
        );
        assert!(
            registry.skipped().is_empty(),
            "nothing was skipped: the file registered and can be named"
        );
    }

    /// **The fold's second casualty.** A model-only row still *wins* its name
    /// contest, because it is the file that answers that spelling.
    ///
    /// With `user_invocable` inside `is_dispatchable`, the project row stops
    /// winning, the user's row is never marked shadowed — and `/delta` silently
    /// becomes a **different file** than the one the model reaches under the
    /// same name.
    #[test]
    fn a_model_only_row_still_wins_its_name_contest() {
        let mut project = row("delta", SkillSource::Project);
        project.user_invocable = false;
        let user = row("delta", SkillSource::User);
        let registry = assemble(vec![project, user], Vec::new());

        assert_eq!(registry.skills().len(), 2, "both rows are listed");
        let loser = registry
            .skills()
            .iter()
            .find(|s| s.source == SkillSource::User)
            .expect("the user row is listed");
        assert_eq!(
            loser.shadowed,
            Some(ShadowedBy::ProjectSkill),
            "the project row won the spelling even though the user may not type it"
        );
        assert!(
            registry.dispatchable_by_user("delta").is_none(),
            "so `/delta` reaches nothing at all — never the shadowed file"
        );
        assert_eq!(
            registry.invocable_by_model("delta").map(|s| s.source),
            Some(SkillSource::Project),
            "and the model reaches the winner, not the loser"
        );
    }

    /// A shadowed row is not the model's either: a name resolves to one file or
    /// to none, for both callers.
    #[test]
    fn a_shadowed_row_is_not_the_models_and_reads_as_shadowed() {
        let mut winner = row("status", SkillSource::Project);
        winner.model_invocable = false;
        let mut loser = row("status", SkillSource::User);
        loser.user_invocable = false;
        let registry = assemble(vec![winner, loser], Vec::new());

        assert!(
            registry.invocable_by_model("status").is_none(),
            "the winner hid itself from the model; the loser must not answer in \
             its place"
        );
        assert_eq!(
            registry
                .dispatchable_by_user("status")
                .map(|skill| skill.source),
            Some(SkillSource::Project),
            "the user still reaches the winner — the loser answers neither \
             question, whatever its own flags say"
        );

        let loser = registry
            .skills()
            .iter()
            .find(|s| s.source == SkillSource::User)
            .expect("listed");
        assert_eq!(
            loser.user_dispatch(),
            UserDispatch::Shadowed(ShadowedBy::ProjectSkill),
            "shadowing wins over model-only — the precedence every surface \
             composing a mark reads off this one decision"
        );
    }

    #[test]
    fn a_name_is_lowercase_ascii_and_at_most_sixty_four_characters() {
        for good in ["status", "a", "0", "req-585", "with_underscore", "x9"] {
            assert!(is_valid_skill_name(good), "{good} must be a valid name");
        }
        assert!(is_valid_skill_name(&"a".repeat(MAX_NAME_LEN)));
        for bad in [
            "",
            "-leading",
            "_leading",
            "Status",
            "with space",
            "with.dot",
            "with/slash",
            "émoji",
            "trailing\n",
        ] {
            assert!(
                !is_valid_skill_name(bad),
                "{bad:?} must not be a valid name"
            );
        }
        assert!(
            !is_valid_skill_name(&"a".repeat(MAX_NAME_LEN + 1)),
            "one character past the cap is refused"
        );
    }

    /// The key carries the source, and the two sources never collide — the
    /// property ADR-6 exists for.
    #[test]
    fn a_permission_key_names_the_source_and_the_skill() {
        assert_eq!(
            permission_key_for(SkillSource::User, "status"),
            "skill:user:status"
        );
        assert_eq!(
            permission_key_for(SkillSource::Project, "status"),
            "skill:project:status"
        );
        assert_ne!(
            permission_key_for(SkillSource::User, "status"),
            permission_key_for(SkillSource::Project, "status")
        );
        assert!(!permission_key_for(SkillSource::User, "shell").starts_with("shell"));
    }

    /// Every reason renders the words BR-1's diagnostic table promises, and the
    /// two derived from a constant derive from it (so raising the cap cannot
    /// leave the sentence lying).
    #[test]
    fn every_skip_reason_renders_its_named_words() {
        assert_eq!(
            SkipReason::Unreadable.to_string(),
            "unreadable (permission denied)"
        );
        assert_eq!(
            SkipReason::Oversize { bytes: 135_184 }.to_string(),
            "over 128 KiB (135,184 B)"
        );
        assert_eq!(SkipReason::NotUtf8.to_string(), "not UTF-8");
        assert_eq!(
            SkipReason::MalformedFrontmatter.to_string(),
            "malformed frontmatter"
        );
        assert_eq!(SkipReason::InvalidName.to_string(), "invalid name");
        assert_eq!(SkipReason::SymlinkEntry.to_string(), "symlink not followed");
        assert_eq!(
            SkipReason::RootTruncated.to_string(),
            "root truncated at 512 entries"
        );
        assert_eq!(
            SkipReason::Shadowed {
                by: ShadowedBy::ProjectSkill
            }
            .to_string(),
            "shadowed by a project skill of the same name"
        );
        assert_eq!(
            SkipReason::Shadowed {
                by: ShadowedBy::SkillsDirectory
            }
            .to_string(),
            "shadowed by the skills/ entry of the same name"
        );
    }

    #[test]
    fn a_home_kind_session_has_no_project_roots() {
        let home = Path::new("/h");
        let user_only = roots(Some(home), home, RootKind::Home);
        assert_eq!(user_only.len(), 2, "the user pair only");
        assert!(user_only.iter().all(|r| r.source == SkillSource::User));

        let both = roots(Some(home), Path::new("/repo"), RootKind::Project);
        assert_eq!(both.len(), 4);
        assert_eq!(
            both.iter().map(|r| r.dir.clone()).collect::<Vec<_>>(),
            vec![
                PathBuf::from("/repo/.claude/skills"),
                PathBuf::from("/repo/.claude/commands"),
                PathBuf::from("/h/.claude/skills"),
                PathBuf::from("/h/.claude/commands"),
            ],
            "project beats user, and skills beats commands within a source"
        );

        let no_home = roots(None, Path::new("/repo"), RootKind::Project);
        assert_eq!(no_home.len(), 2, "no home, no user roots, no diagnostic");
    }
}
