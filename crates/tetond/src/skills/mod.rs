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
//! `description` and `argument-hint` is inert and is merely *listed*
//! ([`Skill::ignored_keys`]): a file that says `model: opus` or
//! `allowed-tools: Bash` changes no routing decision, no permission level and
//! no boundary. That is the whole reason the parser can afford to be as narrow
//! as it is — nothing it reads can grant anything.
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

pub use discovery::{discover, DirLister, Entry, ListError, ReadError, RealFs};
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

/// The largest skill file discovery will read: 64 KiB.
///
/// A file past it is **named** (`over 64 KiB (N B)`), never silently truncated
/// — half a body is a body the user did not write. The figure is generous for
/// a prompt (the largest shipped ADLC skill is a few tens of KiB) and small
/// enough that four directories' worth cannot be a memory event.
pub const SKILL_MAX_BYTES: u64 = 64 * 1024;

/// The most entries discovery will consider under one root: 512.
///
/// The bound exists because a root is a *user-supplied path* — `~/.claude` can
/// be a symlink into anything, including a directory with a hundred thousand
/// entries — and discovery runs on the session-create path, where a user is
/// waiting. Hitting it is a named diagnostic (`root truncated at 512 entries`),
/// and entries are sorted **before** the cap applies so which 512 survive does
/// not depend on the filesystem's listing order (LESSON-540).
pub const MAX_ENTRIES_PER_ROOT: usize = 512;

/// The longest dispatchable name: 64 characters.
pub const MAX_NAME_LEN: usize = 64;

/// One discovered, registered command.
///
/// `path` is the file actually read, absolute and **local-only**: it is the
/// jail-relative fact the expander needs and the identity provenance is minted
/// from, and it is reduced to a home-relative display (`session_root::
/// display_for`) at every surface that shows it. An absolute path carries a
/// username into a transcript or a remote payload; this struct is the last
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
    /// The frontmatter `description`, verbatim; bounded and neutralized by the
    /// surface that renders it, not here.
    pub description: Option<String>,
    /// The frontmatter `argument-hint`, verbatim.
    pub argument_hint: Option<String>,
    /// Everything after the frontmatter block, verbatim (BR-13 — the body is
    /// passed as written).
    pub body: String,
    /// Frontmatter keys Teton does not honor, in the order they appeared.
    /// Listed by `/verbose`; otherwise inert.
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

impl Skill {
    /// The permission-gate key this skill's dynamic context runs under
    /// (ADR-6). See [`permission_key_for`].
    #[must_use]
    pub fn permission_key(&self) -> String {
        permission_key_for(self.source, &self.name)
    }

    /// True when this row may be dispatched — i.e. nothing shadows it.
    #[must_use]
    pub fn is_dispatchable(&self) -> bool {
        self.shadowed.is_none()
    }

    /// The diagnostic this row carries when it is *not* dispatchable, in the
    /// same vocabulary [`SkipReason`] uses for everything else discovery
    /// declined — so `/help`'s row mark and the skipped list cannot drift into
    /// two spellings of one fact.
    #[must_use]
    pub fn shadow_reason(&self) -> Option<SkipReason> {
        self.shadowed.map(|by| SkipReason::Shadowed { by })
    }
}

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
}

impl fmt::Display for ShadowedBy {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::ProjectSkill => f.write_str("a project skill of the same name"),
            Self::SkillsDirectory => f.write_str("the skills/ entry of the same name"),
        }
    }
}

/// One entry discovery found and did not register, with why.
///
/// `path` is the file (or, for a root-level refusal, the directory) as
/// discovery opened it. Like [`Skill::path`] it is absolute here and reduced to
/// a home-relative display at the wire.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Skipped {
    /// What was skipped.
    pub path: PathBuf,
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
    /// The entry is a symlink. Roots are followed; entries are not (BR-1).
    SymlinkEntry,
    /// Something else already owns this name.
    Shadowed {
        /// What owns it.
        by: ShadowedBy,
    },
    /// The root held more than [`MAX_ENTRIES_PER_ROOT`] entries; the rest were
    /// not considered. Reported once, against the root.
    RootTruncated,
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
            Self::Shadowed { by } => write!(f, "shadowed by {by}"),
            Self::RootTruncated => {
                write!(f, "root truncated at {MAX_ENTRIES_PER_ROOT} entries")
            }
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

    /// The skill `name` dispatches to, or `None` when nothing does — including
    /// when a row of that name exists but is shadowed.
    #[must_use]
    pub fn dispatchable(&self, name: &str) -> Option<&Skill> {
        self.skills
            .iter()
            .find(|skill| skill.name == name && skill.is_dispatchable())
    }

    /// True when nothing was found at all — the state a session on a machine
    /// with no `~/.claude` is in, and the state the version handshake puts a
    /// new client against an old daemon in (ADR-2).
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.skills.is_empty() && self.skipped.is_empty()
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
    format!("skill:{}:{name}", source_word(source))
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
    SkillRegistry { skills, skipped }
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
        specs.push(RootSpec::new(
            SkillSource::Project,
            Shape::Skills,
            session_root,
        ));
        specs.push(RootSpec::new(
            SkillSource::Project,
            Shape::Commands,
            session_root,
        ));
    }
    if let Some(home) = home {
        specs.push(RootSpec::new(SkillSource::User, Shape::Skills, home));
        specs.push(RootSpec::new(SkillSource::User, Shape::Commands, home));
    }
    specs
}

/// One of the four directories discovery opens.
struct RootSpec {
    source: SkillSource,
    shape: Shape,
    dir: PathBuf,
}

impl RootSpec {
    fn new(source: SkillSource, shape: Shape, base: &Path) -> Self {
        let dir = base.join(".claude").join(match shape {
            Shape::Skills => "skills",
            Shape::Commands => "commands",
        });
        Self { source, shape, dir }
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
            SkipReason::Oversize { bytes: 67_184 }.to_string(),
            "over 64 KiB (67,184 B)"
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
