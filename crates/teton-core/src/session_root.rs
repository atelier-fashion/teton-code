//! The session root as a pure value (REQ-583 ADR-1, ADR-2).
//!
//! A session's tools are jailed to one directory — the **session root**. This
//! module is the one derivation of what that directory *is*: which kind of place
//! it is ([`classify`]), how it is spelled to a person ([`display_for`]), how
//! its kind is said in the user's words ([`kind_phrase`]), how a
//! user-controlled value from it is bounded before it lands in a prompt or a
//! refusal ([`bounded_field`], [`bounded_field_bytes`]), which names make a
//! directory a project ([`PROJECT_MARKERS`]), how a `--cwd`/`/cd` argument
//! becomes a path ([`resolve_cwd_argument`]), and what the daemon says when
//! that path may not be a root ([`CwdRefusal`] — the sentence, not the check).
//!
//! Everything here is pure: no filesystem, no environment. The daemon's
//! `tetond::session_root::probe` supplies the I/O (does a marker exist, what
//! does `.git/HEAD` say) and calls in here for the answers, so the CLI banner,
//! the daemon's environment block, the launch notice and every jail refusal
//! print one spelling built once (ADR-2 "built once") — a client linking this
//! crate cannot drift from the daemon that enforces the jail.

use std::path::{Component, Path, PathBuf};

use teton_protocol::methods::{RootKind, SessionRoot};

/// The names that make a directory a project (REQ-583 BR-4): a VCS directory or
/// a top-level build manifest.
///
/// **The one table.** Nothing else decides "is this a project"; the probe asks
/// whether any of these exists (as a file *or* a directory — a linked git
/// worktree's `.git` is a file), and the AC-7 test exercises every name here by
/// iterating this slice.
pub const PROJECT_MARKERS: &[&str] = &[
    ".git",
    ".hg",
    ".svn",
    "Cargo.toml",
    "package.json",
    "pyproject.toml",
    "go.mod",
    "pom.xml",
    "build.gradle",
    "Gemfile",
    "mix.exs",
    ".adlc",
];

/// The ceiling, in characters, on a root's display spelling wherever it is
/// printed (ADR-2 bounding). A 200-character path is middle-elided to this.
pub const DISPLAY_MAX_CHARS: usize = 80;

/// The ceiling, in characters, on a project name or a branch name.
pub const NAME_MAX_CHARS: usize = 32;

/// The elision marker [`middle_elide`] inserts. One character, so a bounded
/// field is never longer than its ceiling.
const ELISION: char = '…';

/// The byte ceiling that goes with a character ceiling of `max_chars`: what an
/// ASCII value elided to `max_chars` costs — `max_chars - 1` one-byte
/// characters and the three-byte [`ELISION`] mark.
///
/// The character ceilings ([`DISPLAY_MAX_CHARS`], [`NAME_MAX_CHARS`]) are for
/// the person reading; the resident-prompt ceiling the environment block is
/// paid under is counted in **bytes**, and its two sweeps measure an ASCII
/// 200-character root cut to the character ceiling. Without a byte bound an
/// all-multibyte value — up to four bytes a character — rendered longer than
/// that row, so the row was not the worst case it was said to be. This bound
/// ([`bounded_field_bytes`] holds it) makes it exactly the worst: an all-ASCII
/// value at the ceiling costs `max_chars` bytes and one cut to it costs
/// `max_chars + 2`, and nothing in any script may cost more. (A wider byte cap
/// — say twice the character ceiling — would need the block's worst row, and
/// the ceiling that pays for it, to grow by over a hundred bytes; the margin
/// there is one.)
#[must_use]
pub const fn byte_ceiling(max_chars: usize) -> usize {
    max_chars + ELISION.len_utf8() - 1
}

/// Whether `c` is a character that renders as nothing or re-orders what is
/// around it — the format and line characters a control-character check does
/// not catch, and the ones a path or a branch name could use to hide a frame
/// label or make a refusal read backwards. [`neutralized`] replaces each one
/// with `?`.
///
/// **Best-effort, not exhaustive.** There is no Unicode-tables crate in this
/// workspace, so the set is a hand-kept range match over the characters that
/// hide or re-order text: the zero-width and joiner marks (U+200B–U+200F), the
/// word joiner and invisible operators (U+2060–U+2064), the deprecated format
/// controls (U+206A–U+206F), the bidi embedding, override and isolate
/// controls (U+202A–U+202E, U+2066–U+2069), the byte-order mark (U+FEFF), the
/// interlinear annotation marks (U+FFF9–U+FFFB), the line and paragraph
/// separators (U+2028, U+2029), the soft hyphen (U+00AD), the combining
/// grapheme joiner (U+034F), the Arabic letter mark (U+061C), the Mongolian
/// vowel separator (U+180E), the Tags block (U+E0000–U+E007F, invisible
/// modifier characters) and the blank Hangul fillers (U+115F, U+1160, U+3164,
/// U+FFA0 — render as a blank the width of a letter). Not a `Cf` table: the
/// remaining format characters do not hide text or reverse it.
///
/// **Two trade-offs, made on purpose.** The zero-width non-joiner and joiner
/// (U+200C, U+200D) are legitimate in Persian and Indic text and inside emoji
/// sequences, and they are neutralised anyway: in a path or a branch name read
/// back to a person they are far likelier to be hiding two spellings of one
/// name than joining a ligature, and a `?` in a refusal costs nothing. The
/// variation selectors (U+FE00–U+FE0F, U+E0100–U+E01EF) are **left alone**:
/// they break legitimate emoji (`❤️` is U+2764 U+FE0F) and neither hide text
/// nor re-order it. Kept private so the one list is [`bounded_field`]'s.
const fn is_hidden_or_bidi(c: char) -> bool {
    matches!(
        c,
        '\u{00AD}'
            | '\u{034F}'
            | '\u{061C}'
            | '\u{115F}'
            | '\u{1160}'
            | '\u{180E}'
            | '\u{200B}'..='\u{200F}'
            | '\u{2028}'
            | '\u{2029}'
            | '\u{202A}'..='\u{202E}'
            | '\u{2060}'..='\u{2064}'
            | '\u{2066}'..='\u{206F}'
            | '\u{3164}'
            | '\u{FEFF}'
            | '\u{FFA0}'
            | '\u{FFF9}'..='\u{FFFB}'
            | '\u{E0000}'..='\u{E007F}'
    )
}

/// `s` with every character that could break, hide or re-order the line it
/// sits on replaced by `?`: the control characters (newlines and carriage
/// returns included) and the hidden/bidi set [`is_hidden_or_bidi`] names.
fn neutralized(s: &str) -> String {
    s.chars()
        .map(|c| {
            if c.is_control() || is_hidden_or_bidi(c) {
                '?'
            } else {
                c
            }
        })
        .collect()
}

/// What kind of place `path` is (BR-4).
///
/// `Home` when `path == home`, `FilesystemRoot` when `path == /`, `Project`
/// when the caller found a [`PROJECT_MARKERS`] entry there, else `Plain`.
///
/// **Home wins over a marker.** A `~/.git` (or a `~/package.json`) must not
/// turn the whole home folder into "a project" — that would silence the launch
/// notice (BR-5) for exactly the root it exists to announce, and the walkers'
/// home-tree pruning keys on this kind (ADR-3). Comparison is component-wise
/// (`Path`'s `==`), so a trailing slash on either side does not matter.
#[must_use]
pub fn classify(path: &Path, home: Option<&Path>, has_marker: bool) -> RootKind {
    if home.is_some_and(|home| home == path) {
        return RootKind::Home;
    }
    if path == Path::new("/") {
        return RootKind::FilesystemRoot;
    }
    if has_marker {
        RootKind::Project
    } else {
        RootKind::Plain
    }
}

/// The spelling of `path` a person reads: `~` for the home folder itself,
/// `~/rest` for anything under it, the absolute path otherwise.
///
/// The rule the CLI banner's `cwd:` line has always used, lifted here so the
/// banner, the environment block, the launch notice, the jail refusals and the
/// `/cd` line print one spelling (ADR-1). `home` is whatever the caller read
/// from `HOME` — `None` (or an empty value) means "not under a home", never a
/// guess.
#[must_use]
pub fn display_for(path: &Path, home: Option<&Path>) -> String {
    if let Some(home) = home.filter(|home| !home.as_os_str().is_empty()) {
        if let Ok(rest) = path.strip_prefix(home) {
            return if rest.as_os_str().is_empty() {
                "~".to_owned()
            } else {
                format!("~/{}", rest.display())
            };
        }
    }
    path.display().to_string()
}

/// The spelling of `path` a person reads when it sits under a directory the
/// session already names — the part **below `base`**, with no leading `./`, and
/// `.` for `base` itself.
///
/// The second half of the display rule (REQ-585 BR-1, BUG-187). [`display_for`]
/// can only shorten what is under the home folder, so a file under a session
/// root *outside* `$HOME` — `/tmp/build/repo/.claude/skills/x/SKILL.md`, a
/// checkout on an external volume, a CI workspace — came back absolute, and an
/// absolute path is the one form the entity table forbids on a surface. Where
/// the caller has a base the reader already knows (the session root, printed in
/// the banner and resident in the environment block), the honest spelling is
/// relative to it: it says everything the reader needs and repeats nothing the
/// surface has already told them.
///
/// `base` of `None` (or an empty path), or a `path` that is not under `base`,
/// falls through to [`display_for`] — so a caller with no base, and a caller
/// whose value simply lives elsewhere, get the home-relative rule and its
/// absolute last resort, unchanged. Which base a caller passes is the caller's
/// decision, not this function's: `tetond::skills` picks it from the skill's
/// **source**, because "a project skill is under the session root" is
/// discovery's fact and re-deriving it here from a path comparison would be a
/// second copy of it (LESSON-546).
///
/// Component-wise, like [`display_for`]: a sibling that merely shares a prefix
/// string (`/tmp/repo-old` against a base of `/tmp/repo`) is not under it.
#[must_use]
pub fn display_under(path: &Path, base: Option<&Path>, home: Option<&Path>) -> String {
    if let Some(base) = base.filter(|base| !base.as_os_str().is_empty()) {
        if let Ok(rest) = path.strip_prefix(base) {
            return if rest.as_os_str().is_empty() {
                ".".to_owned()
            } else {
                rest.display().to_string()
            };
        }
    }
    display_for(path, home)
}

/// `s`, made safe to print mid-line in a refusal, a banner line or a notice
/// (ADR-2 bounding) — bounded in **characters**, the unit a person reads.
///
/// Every control character — newlines and carriage returns included — and
/// every hidden or bidi format character ([`is_hidden_or_bidi`]: zero-width
/// marks, direction overrides and isolates, the line separators, the BOM)
/// becomes `?`, so a path can neither break the line it sits on, smuggle a
/// frame label to column 0, nor make the line read backwards; then the result
/// is [`middle_elide`]d to at most `max_chars` characters. Counted in
/// characters and not bytes on purpose: the CLI banner, the launch notice, the
/// `/cd` line and every jail refusal are for a person, and a CJK path should
/// show them its full [`DISPLAY_MAX_CHARS`] characters, not a third of them.
/// The three user-controlled root values (display, project name, branch) pass
/// through here, with [`DISPLAY_MAX_CHARS`] or [`NAME_MAX_CHARS`]; the one
/// surface counted in bytes — the resident environment block — uses
/// [`bounded_field_bytes`] instead. Idempotent: a value already within the
/// bound comes back unchanged, and a value bounded by [`bounded_field_bytes`]
/// is within it.
#[must_use]
pub fn bounded_field(s: &str, max_chars: usize) -> String {
    middle_elide(&neutralized(s), max_chars)
}

/// [`bounded_field`] and, on top of it, a **byte** bound: at most `max_chars`
/// characters *and* at most [`byte_ceiling`]`(max_chars)` bytes.
///
/// For the one place a root value is paid for in bytes — the environment
/// block resident in every turn's prompt, whose ceiling sweeps measure an
/// ASCII row cut to the character ceiling. The byte bound is met by eliding
/// further, at character boundaries, still around one [`ELISION`] mark; an
/// all-ASCII value never meets it (it *is* the ASCII cost), so the rendering
/// of an ASCII path is what it always was, and no script can render past the
/// row the sweeps measure. Idempotent, and a value it bounded passes through
/// [`bounded_field`] unchanged — so a phrase built by [`kind_phrase`] from a
/// byte-bounded root keeps the byte bound.
#[must_use]
pub fn bounded_field_bytes(s: &str, max_chars: usize) -> String {
    let neutral = neutralized(s);
    let max_bytes = byte_ceiling(max_chars);
    // Largest character budget whose elision fits the byte ceiling. Below the
    // value's own length an elision's byte count only grows with the budget
    // (each step keeps one more character), so the first fit from the top is
    // the longest one; a budget of zero renders the empty string, which always
    // fits, so the search cannot fall through.
    (0..=max_chars)
        .rev()
        .map(|keep| middle_elide(&neutral, keep))
        .find(|out| out.len() <= max_bytes)
        .unwrap_or_default()
}

/// What kind of place `root` is, in the user's words (BR-1's phrases; BR-3:
/// "project" names a kind only when the kind *is* project).
///
/// - `project {name}, branch {branch}` / `project {name}` for a project, with
///   the name and branch re-bounded here through [`bounded_field`] at
///   [`NAME_MAX_CHARS`] (idempotent on the probe's bounded values, and it
///   protects the phrase against a root that arrived unbounded);
/// - `a project, branch {branch}` / `a project` for a project the probe could
///   not name — the wire says a project always carries its name, so this arm
///   is defensive: still a project, still never an invented name;
/// - `your home folder`, `the filesystem root`, `not a project` otherwise.
///
/// **The one phrase.** The daemon's environment block and the CLI's banner
/// notice, `session_root_changed` line and `/cd` all print this — the model
/// and the person read the same words for the same root, and neither side
/// can grow a second vocabulary. An empty name or branch (after bounding)
/// counts as absent, so no phrase ever reads `project ` with nothing after it.
#[must_use]
pub fn kind_phrase(root: &SessionRoot) -> String {
    match root.kind {
        RootKind::Project => {
            let bounded_name = |value: &str| {
                let bounded = bounded_field(value, NAME_MAX_CHARS);
                (!bounded.is_empty()).then_some(bounded)
            };
            let name = root.project_name.as_deref().and_then(bounded_name);
            let branch = root.vcs_branch.as_deref().and_then(bounded_name);
            match (name, branch) {
                (Some(name), Some(branch)) => format!("project {name}, branch {branch}"),
                (Some(name), None) => format!("project {name}"),
                (None, Some(branch)) => format!("a project, branch {branch}"),
                (None, None) => "a project".to_owned(),
            }
        }
        RootKind::Home => "your home folder".to_owned(),
        RootKind::FilesystemRoot => "the filesystem root".to_owned(),
        RootKind::Plain => "not a project".to_owned(),
    }
}

/// `s` if it fits in `max_chars`, otherwise its head and tail around one `…`,
/// `max_chars` characters in total.
///
/// Cut in the middle rather than at the end because both ends of a path carry
/// meaning: the front says where it hangs, the back names the directory. Counted
/// in `char`s because the thing bounded is what a person reads. A `max_chars` of
/// zero yields the empty string.
#[must_use]
pub fn middle_elide(s: &str, max_chars: usize) -> String {
    let total = s.chars().count();
    if total <= max_chars {
        return s.to_owned();
    }
    if max_chars == 0 {
        return String::new();
    }
    let keep = max_chars - 1;
    let tail = keep / 2;
    let head = keep - tail;
    let mut out: String = s.chars().take(head).collect();
    out.push(ELISION);
    out.extend(s.chars().skip(total - tail));
    out
}

/// Why a resolved path may not be a session's root (BR-6/BR-7, ADR-4) — the
/// refusal the daemon's one validator (`tetond::sessions::validate_session_cwd`,
/// behind `session/create` and `session/set_cwd`) answers with, typed here so
/// the CLI's own fail-fast for a `--cwd` that names no directory renders the
/// **same** sentence by constructing the same value rather than retyping it.
///
/// Pure: the type carries the verdict and its wording; the I/O that reaches a
/// verdict (`is_absolute`, `is_dir`) stays in the daemon. Its `Display` is the
/// wire message and the user-facing line, root-neutral on purpose: it says
/// *path*, never "cwd" (wire jargon a user of `--cwd` or `/cd` never typed) and
/// never "session root" (the thing this path failed to become).
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum CwdRefusal {
    /// Not absolute after the client's own resolution.
    #[error("path `{0}` must be an absolute path")]
    NotAbsolute(PathBuf),
    /// Missing, or present but not a directory.
    #[error("path `{0}` does not exist or is not a directory")]
    NotADirectory(PathBuf),
}

/// Why a `--cwd`/`/cd` argument could not become a path (BR-6/BR-7).
///
/// Each message names the argument, so the refusal a user reads says which
/// spelling was refused and why. Existence and directory-ness are **not** judged
/// here — the daemon validates the resolved path (one validator, ADR-4) and
/// answers with a [`CwdRefusal`].
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum CwdArgError {
    /// The argument was empty (or only whitespace).
    #[error("a path is required")]
    Empty,
    /// The argument names the home folder (`~`, `~/…`) but no home is known.
    #[error("`{0}` names the home folder, but HOME is not set")]
    NoHome(String),
    /// The argument did not resolve to an absolute path — only possible when
    /// the shell's own working directory was relative.
    #[error("`{0}` did not resolve to an absolute path")]
    NotAbsolute(String),
}

/// Whether `raw` is a **bare name** rather than a path spelling (REQ-584 BR-8).
///
/// The gate on the registry reading. A bare name contains no path separator and
/// does not open with `~`, `.` or `-`:
///
/// - `teton-code` → a name, and a candidate for the registry;
/// - `src`, likewise — but `./src` under the root still **wins**, because the
///   path reading is tried first and only its failure reaches the registry;
/// - `~/x`, `./x`, `../x`, `/abs`, `a/b` → path spellings, never names;
/// - `-x` → not a name either: a leading `-` is a flag's shape, and reading it
///   as a project would make a typo'd flag move the session.
///
/// Deliberately **narrower** than "not a valid path": the question is what the
/// user meant, and anything that looks like a path is read as one. That is what
/// keeps REQ-583's grammar unchanged wherever it applied — the new reading is
/// reachable only from spellings the old one could never resolve to anything
/// but a sibling directory.
#[must_use]
pub fn is_bare_project_name(raw: &str) -> bool {
    let arg = raw.trim();
    !arg.is_empty()
        && !arg.contains('/')
        && !arg.contains('\\')
        && !arg.starts_with('~')
        && !arg.starts_with('.')
        && !arg.starts_with('-')
}

/// The refusal a `/cd <name>` earns when **neither** reading resolved (BR-8).
///
/// One composer, because the sentence has to name both readings and a caller
/// assembling it from two halves is the drift LESSON-529 names. The daemon
/// raises it; the client renders what it is given.
#[must_use]
pub fn cd_two_reading_refusal(name: &str) -> String {
    format!(
        "no directory `{name}` under the session root, and no known project named \
         `{name}` — `/projects` lists what is known"
    )
}

/// Turn a `--cwd`/`/cd` argument into the absolute path the daemon is asked to
/// validate (BR-6, BR-7 — one grammar, two spellings; AC-12).
///
/// - `~` is `home`; `~/rest` is `home/rest`.
/// - A relative path joins onto `shell_cwd` (the shell's working directory, not
///   the session's root — `/cd src` from a session rooted elsewhere is still
///   relative to where the user's shell is, exactly as `--cwd src` would be).
/// - An absolute path is taken as given.
/// - `.` and `..` are collapsed lexically, as a shell's `cd` does, so the
///   display of the result reads as a place and not as a route.
///
/// **No filesystem check.** Whether the result exists and is a directory is the
/// daemon's to say, with the one validator `session/create` uses; this function
/// never canonicalizes, so a symlinked path is sent as spelled. The
/// specification of this grammar as data is [`CWD_ARGUMENT_GRAMMAR`].
///
/// # Errors
/// [`CwdArgError::Empty`] for an empty argument, [`CwdArgError::NoHome`] when
/// `~` is used and `home` is `None`, [`CwdArgError::NotAbsolute`] when the
/// join could not produce an absolute path.
pub fn resolve_cwd_argument(
    raw: &str,
    shell_cwd: &Path,
    home: Option<&Path>,
) -> Result<PathBuf, CwdArgError> {
    let arg = raw.trim();
    if arg.is_empty() {
        return Err(CwdArgError::Empty);
    }
    let expanded = if arg == "~" {
        home.ok_or_else(|| CwdArgError::NoHome(arg.to_owned()))?
            .to_path_buf()
    } else if let Some(rest) = arg.strip_prefix("~/") {
        // `~//x` is `$HOME/x` in a shell; a leading `/` on the remainder would
        // make `join` discard the home instead.
        home.ok_or_else(|| CwdArgError::NoHome(arg.to_owned()))?
            .join(rest.trim_start_matches('/'))
    } else {
        PathBuf::from(arg)
    };
    let joined = if expanded.is_absolute() {
        expanded
    } else {
        shell_cwd.join(expanded)
    };
    let normalized = lexical_normalize(&joined);
    if normalized.is_absolute() {
        Ok(normalized)
    } else {
        Err(CwdArgError::NotAbsolute(arg.to_owned()))
    }
}

/// Collapse `.` and `..` components without touching the filesystem.
///
/// A `..` at the root stays at the root (`/..` is `/`, as every shell agrees);
/// a `..` with nothing left to pop in a relative path is kept, so the caller's
/// `is_absolute` check still tells the truth.
fn lexical_normalize(path: &Path) -> PathBuf {
    let mut out = PathBuf::new();
    for component in path.components() {
        match component {
            Component::CurDir => {}
            Component::ParentDir => {
                let ends_in_a_name =
                    matches!(out.components().next_back(), Some(Component::Normal(_)));
                if ends_in_a_name {
                    out.pop();
                } else if !out.has_root() {
                    out.push("..");
                }
            }
            other => out.push(other.as_os_str()),
        }
    }
    out
}

/// One row of the `--cwd`/`/cd` argument grammar (AC-12).
///
/// `expect` is judged with `shell_cwd = `[`CWD_GRAMMAR_SHELL_CWD`] and
/// `home = `[`CWD_GRAMMAR_HOME`]: `Ok(path)` is the resolved spelling,
/// `Err(fragment)` is text the refusal must contain.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CwdGrammarRow {
    /// The argument as the user typed it.
    pub raw: &'static str,
    /// What [`resolve_cwd_argument`] must answer.
    pub expect: Result<&'static str, &'static str>,
}

/// The shell working directory every [`CWD_ARGUMENT_GRAMMAR`] row is judged in.
pub const CWD_GRAMMAR_SHELL_CWD: &str = "/work/here";

/// The home folder every [`CWD_ARGUMENT_GRAMMAR`] row is judged with.
pub const CWD_GRAMMAR_HOME: &str = "/home/u";

/// The grammar both `--cwd` and `/cd` obey, as data (BR-6, BR-7, AC-12).
///
/// One table drives three tests: this module's, the CLI's `--cwd` test and its
/// `/cd` test — the REQ-582 "one grammar, two spellings" rule made literal.
/// The rows are the AC-12 spellings (`~`, `~/x`, `rel`, `/abs`, the empty
/// argument) plus the lexical cases a shell user reaches for.
pub const CWD_ARGUMENT_GRAMMAR: &[CwdGrammarRow] = &[
    CwdGrammarRow {
        raw: "~",
        expect: Ok("/home/u"),
    },
    CwdGrammarRow {
        raw: "~/x",
        expect: Ok("/home/u/x"),
    },
    CwdGrammarRow {
        raw: "~/",
        expect: Ok("/home/u"),
    },
    CwdGrammarRow {
        raw: "rel",
        expect: Ok("/work/here/rel"),
    },
    CwdGrammarRow {
        raw: "rel/deeper/",
        expect: Ok("/work/here/rel/deeper"),
    },
    CwdGrammarRow {
        raw: "/abs",
        expect: Ok("/abs"),
    },
    CwdGrammarRow {
        raw: ".",
        expect: Ok("/work/here"),
    },
    CwdGrammarRow {
        raw: "..",
        expect: Ok("/work"),
    },
    CwdGrammarRow {
        raw: "../sibling",
        expect: Ok("/work/sibling"),
    },
    CwdGrammarRow {
        raw: "/../abs",
        expect: Ok("/abs"),
    },
    CwdGrammarRow {
        raw: "",
        expect: Err("a path is required"),
    },
    CwdGrammarRow {
        raw: "   ",
        expect: Err("a path is required"),
    },
];

#[cfg(test)]
mod tests {
    use super::*;

    // ---- classify (AC-7, BR-4) ----

    #[test]
    fn classify_returns_the_four_kinds_for_the_four_fixture_roots() {
        let home = Path::new("/Users/someone");
        assert_eq!(classify(home, Some(home), false), RootKind::Home);
        assert_eq!(
            classify(Path::new("/"), Some(home), false),
            RootKind::FilesystemRoot
        );
        assert_eq!(
            classify(Path::new("/Users/someone/repo"), Some(home), true),
            RootKind::Project
        );
        assert_eq!(
            classify(Path::new("/Users/someone/scratch"), Some(home), false),
            RootKind::Plain
        );
    }

    /// AC-7's second half: `project` for a directory holding **each** marker
    /// name — the table is exercised by name, so a row added to it is covered
    /// without a test being written for it, and a row removed cannot leave a
    /// stale assertion behind.
    #[test]
    fn every_project_marker_makes_a_directory_a_project() {
        let home = Path::new("/home/u");
        for marker in PROJECT_MARKERS {
            let root = Path::new("/home/u/proj");
            // The caller's probe answers `has_marker` from the table; here the
            // table itself is the universe being iterated.
            assert_eq!(
                classify(root, Some(home), true),
                RootKind::Project,
                "marker {marker} must classify as a project"
            );
        }
        // The table is closed and non-trivial: the two VCS names the spec leads
        // with are present, and nothing in it is empty.
        assert!(PROJECT_MARKERS.contains(&".git"));
        assert!(PROJECT_MARKERS.contains(&".adlc"));
        assert!(PROJECT_MARKERS.iter().all(|m| !m.is_empty()));
    }

    /// Home wins over a marker: a `~/.git` must not make the home folder a
    /// project (that would silence the BR-5 notice for the root it exists for).
    #[test]
    fn home_wins_over_a_marker() {
        let home = Path::new("/Users/someone");
        assert_eq!(classify(home, Some(home), true), RootKind::Home);
    }

    #[test]
    fn filesystem_root_wins_over_a_marker_and_home_wins_over_filesystem_root() {
        // A `/Cargo.toml` does not make `/` a project.
        assert_eq!(
            classify(Path::new("/"), Some(Path::new("/home/u")), true),
            RootKind::FilesystemRoot
        );
        // A user whose home *is* `/` (a container) is at home.
        assert_eq!(
            classify(Path::new("/"), Some(Path::new("/")), false),
            RootKind::Home
        );
    }

    #[test]
    fn classify_compares_paths_by_component_not_by_string() {
        let home = Path::new("/Users/someone");
        assert_eq!(
            classify(Path::new("/Users/someone/"), Some(home), true),
            RootKind::Home
        );
    }

    #[test]
    fn classify_without_a_home_never_says_home() {
        assert_eq!(
            classify(Path::new("/Users/someone"), None, false),
            RootKind::Plain
        );
        assert_eq!(
            classify(Path::new("/Users/someone"), None, true),
            RootKind::Project
        );
    }

    // ---- display_for (the banner rule) ----

    #[test]
    fn display_is_tilde_for_home_itself() {
        let home = Path::new("/Users/someone");
        assert_eq!(display_for(home, Some(home)), "~");
        assert_eq!(display_for(Path::new("/Users/someone/"), Some(home)), "~");
    }

    #[test]
    fn display_is_tilde_relative_under_home() {
        let home = Path::new("/Users/someone");
        assert_eq!(
            display_for(
                Path::new("/Users/someone/Documents/GitHub/teton-code"),
                Some(home)
            ),
            "~/Documents/GitHub/teton-code"
        );
    }

    #[test]
    fn display_is_absolute_outside_home_or_without_one() {
        let home = Path::new("/Users/someone");
        assert_eq!(display_for(Path::new("/opt/x"), Some(home)), "/opt/x");
        assert_eq!(
            display_for(Path::new("/Users/someone/x"), None),
            "/Users/someone/x"
        );
        assert_eq!(display_for(Path::new("/"), Some(home)), "/");
    }

    /// A sibling that merely shares a prefix string is not "under" home.
    #[test]
    fn display_does_not_confuse_a_string_prefix_with_an_ancestor() {
        let home = Path::new("/Users/some");
        assert_eq!(
            display_for(Path::new("/Users/someone/x"), Some(home)),
            "/Users/someone/x"
        );
    }

    #[test]
    fn an_empty_home_is_no_home() {
        assert_eq!(
            display_for(Path::new("/opt/x"), Some(Path::new(""))),
            "/opt/x"
        );
    }

    // ---- display_under (the base-relative rule, BUG-187) ----

    #[test]
    fn display_under_is_the_part_below_the_base() {
        let base = Path::new("/tmp/tc-4f2a/proj");
        assert_eq!(
            display_under(
                Path::new("/tmp/tc-4f2a/proj/.claude/skills/validate/SKILL.md"),
                Some(base),
                Some(Path::new("/Users/someone"))
            ),
            ".claude/skills/validate/SKILL.md",
            "a root outside `$HOME` is exactly the case `display_for` cannot \
             shorten, and the one this rule exists for"
        );
    }

    /// The base being *inside* the home folder changes nothing: the base wins
    /// where it applies, so one skill file has one spelling wherever the
    /// checkout happens to live.
    #[test]
    fn display_under_prefers_the_base_over_home() {
        assert_eq!(
            display_under(
                Path::new("/Users/someone/code/repo/.claude/commands/ship.md"),
                Some(Path::new("/Users/someone/code/repo")),
                Some(Path::new("/Users/someone"))
            ),
            ".claude/commands/ship.md"
        );
    }

    #[test]
    fn display_under_is_a_dot_for_the_base_itself() {
        let base = Path::new("/opt/proj");
        assert_eq!(display_under(base, Some(base), None), ".");
        assert_eq!(
            display_under(Path::new("/opt/proj/"), Some(base), None),
            "."
        );
    }

    /// Outside the base — or with no base at all — this *is* [`display_for`],
    /// down to the absolute last resort. A user skill is passed `None` on
    /// purpose: `~/…` is its spelling, and a session root of `/Users` must not
    /// turn it into `someone/.claude/…`, which would carry the username the
    /// whole rule exists to keep off a surface.
    #[test]
    fn display_under_falls_through_to_the_home_rule() {
        let home = Path::new("/Users/someone");
        assert_eq!(
            display_under(
                Path::new("/Users/someone/.claude/skills/x/SKILL.md"),
                None,
                Some(home)
            ),
            "~/.claude/skills/x/SKILL.md"
        );
        assert_eq!(
            display_under(
                Path::new("/Users/someone/.claude/skills/x/SKILL.md"),
                Some(Path::new("/tmp/proj")),
                Some(home)
            ),
            "~/.claude/skills/x/SKILL.md"
        );
        assert_eq!(
            display_under(
                Path::new("/opt/x/SKILL.md"),
                Some(Path::new("/tmp/proj")),
                Some(home)
            ),
            "/opt/x/SKILL.md",
            "neither base nor home applies: absolute, and the caller bounds it"
        );
    }

    /// A sibling that merely shares a prefix string is not under the base —
    /// the same claim `display_does_not_confuse_a_string_prefix_with_an_ancestor`
    /// makes about home, on the other half of the rule.
    #[test]
    fn display_under_does_not_confuse_a_string_prefix_with_an_ancestor() {
        assert_eq!(
            display_under(
                Path::new("/tmp/repo-old/f.md"),
                Some(Path::new("/tmp/repo")),
                None
            ),
            "/tmp/repo-old/f.md"
        );
    }

    #[test]
    fn an_empty_base_is_no_base() {
        let home = Path::new("/Users/someone");
        assert_eq!(
            display_under(
                Path::new("/Users/someone/x"),
                Some(Path::new("")),
                Some(home)
            ),
            "~/x"
        );
    }

    // ---- bounded_field / middle_elide (ADR-2 bounding) ----

    #[test]
    fn bounded_field_replaces_control_characters_and_line_breaks() {
        assert_eq!(bounded_field("a\nb\rc\td\u{1b}[31me", 80), "a?b?c?d?[31me");
        assert_eq!(
            bounded_field_bytes("a\nb\rc\td\u{1b}[31me", 80),
            "a?b?c?d?[31me"
        );
    }

    /// **The hidden and bidi characters are neutralised too.** A right-to-left
    /// override in a branch name would make the rest of the line — the
    /// platform, a frame label, the closing bracket — read backwards on a
    /// terminal and in a prompt; a zero-width joiner or a BOM would hide in a
    /// name and make two spellings of one path look identical. None of them
    /// is a control character, so `is_control` alone let them through; each
    /// becomes `?` like a control character does, in both bounding functions.
    #[test]
    fn bounded_field_neutralizes_bidi_overrides_and_zero_width_characters() {
        // An RLO in a branch name, then a plausible tail.
        let branch = "feat/\u{202E}niam-ot-egrem";
        let out = bounded_field(branch, NAME_MAX_CHARS);
        assert_eq!(out, "feat/?niam-ot-egrem", "{out:?}");
        assert_eq!(bounded_field_bytes(branch, NAME_MAX_CHARS), out);
        // Every character in the named set, one at a time, between two
        // letters: each is `?`, nothing else moves.
        let hidden = [
            '\u{200B}',
            '\u{200C}',
            '\u{200D}',
            '\u{200E}',
            '\u{200F}',
            '\u{202A}',
            '\u{202B}',
            '\u{202C}',
            '\u{202D}',
            '\u{202E}',
            '\u{2060}',
            '\u{2061}',
            '\u{2062}',
            '\u{2063}',
            '\u{2064}',
            '\u{2066}',
            '\u{2067}',
            '\u{2068}',
            '\u{2069}',
            '\u{FEFF}',
            '\u{2028}',
            '\u{2029}',
            '\u{061C}',
            '\u{180E}',
            // The verify-pass additions: soft hyphen, combining grapheme
            // joiner, the deprecated format controls, the interlinear
            // annotation marks, the Tags block's ends and a middle, and the
            // blank Hangul fillers.
            '\u{00AD}',
            '\u{034F}',
            '\u{206A}',
            '\u{206B}',
            '\u{206C}',
            '\u{206D}',
            '\u{206E}',
            '\u{206F}',
            '\u{FFF9}',
            '\u{FFFA}',
            '\u{FFFB}',
            '\u{E0000}',
            '\u{E0001}',
            '\u{E0020}',
            '\u{E007F}',
            '\u{115F}',
            '\u{1160}',
            '\u{3164}',
            '\u{FFA0}',
        ];
        for c in hidden {
            assert!(
                !c.is_control(),
                "{c:?} is a control character; the test is vacuous for it"
            );
            let s = format!("a{c}b");
            assert_eq!(bounded_field(&s, 80), "a?b", "U+{:04X}", c as u32);
            assert_eq!(bounded_field_bytes(&s, 80), "a?b", "U+{:04X}", c as u32);
        }
        // Ordinary non-ASCII letters and marks are not touched: the set is the
        // hidden and re-ordering characters, not "anything unusual".
        assert_eq!(bounded_field("é漢字-ß", 80), "é漢字-ß");
        assert_eq!(
            bounded_field("~/文档/项目", DISPLAY_MAX_CHARS),
            "~/文档/项目"
        );
        // The variation selectors are deliberately left alone: they are what
        // makes an emoji presentation (`❤️` is U+2764 U+FE0F), and they neither
        // hide text nor re-order it — see `is_hidden_or_bidi`.
        for emoji in ["\u{2764}\u{FE0F}", "a\u{FE0E}b", "\u{1F600}\u{E0100}"] {
            assert_eq!(bounded_field(emoji, 80), emoji, "{emoji:?}");
            assert_eq!(bounded_field_bytes(emoji, 80), emoji, "{emoji:?}");
        }
    }

    /// The daemon's refusal sentences, typed here so the CLI's fail-fast and
    /// the daemon's validator render one value: each names the path and says
    /// *path* — never "cwd", never "session root".
    #[test]
    fn a_cwd_refusal_names_the_path_in_one_root_neutral_sentence_per_arm() {
        let not_absolute = CwdRefusal::NotAbsolute(PathBuf::from("relative/dir"));
        assert_eq!(
            not_absolute.to_string(),
            "path `relative/dir` must be an absolute path"
        );
        let not_a_dir = CwdRefusal::NotADirectory(PathBuf::from("/nope"));
        assert_eq!(
            not_a_dir.to_string(),
            "path `/nope` does not exist or is not a directory"
        );
        for refusal in [&not_absolute, &not_a_dir] {
            let text = refusal.to_string();
            assert!(
                !text.contains("cwd"),
                "wire jargon in a user-facing refusal: {text}"
            );
            assert!(!text.contains("session root"), "{text}");
        }
    }

    #[test]
    fn bounded_field_leaves_a_short_clean_value_alone() {
        assert_eq!(
            bounded_field("~/Documents/GitHub/teton-code", DISPLAY_MAX_CHARS),
            "~/Documents/GitHub/teton-code"
        );
    }

    #[test]
    fn a_two_hundred_char_path_is_middle_elided_to_the_display_ceiling() {
        let long = format!("/{}", "segment/".repeat(25));
        assert!(long.chars().count() >= 200, "{}", long.len());
        let bounded = bounded_field(&long, DISPLAY_MAX_CHARS);
        assert!(bounded.chars().count() <= DISPLAY_MAX_CHARS, "{bounded}");
        assert_eq!(bounded.chars().count(), DISPLAY_MAX_CHARS);
        assert!(bounded.contains(ELISION), "{bounded}");
        assert!(bounded.starts_with("/segment/"), "{bounded}");
        assert!(bounded.ends_with("segment/"), "{bounded}");
    }

    #[test]
    fn a_long_name_is_cut_to_the_name_ceiling() {
        let long = "x".repeat(100);
        let bounded = bounded_field(&long, NAME_MAX_CHARS);
        assert_eq!(bounded.chars().count(), NAME_MAX_CHARS);
        assert!(bounded.contains(ELISION));
    }

    #[test]
    fn middle_elide_counts_chars_not_bytes() {
        let s = "é".repeat(10);
        let out = middle_elide(&s, 5);
        assert_eq!(out.chars().count(), 5);
        assert_eq!(out, "éé…éé");
    }

    /// The byte ceiling is the ASCII cost of the character ceiling: an ASCII
    /// value that fits costs at most `max_chars` bytes, one that was cut costs
    /// `max_chars + 2` (the three-byte mark for one character), and that is
    /// the ceiling — so the ASCII row the resident-prompt sweeps measure is the
    /// byte-worst rendering there is. On ASCII the two bounding functions
    /// agree byte for byte: the byte bound is never the one that bites.
    #[test]
    fn the_byte_ceiling_is_what_an_elided_ascii_value_costs() {
        assert_eq!(byte_ceiling(DISPLAY_MAX_CHARS), 82);
        assert_eq!(byte_ceiling(NAME_MAX_CHARS), 34);
        assert_eq!(byte_ceiling(1), ELISION.len_utf8());
        let long = "/segment".repeat(25);
        let cut = bounded_field_bytes(&long, DISPLAY_MAX_CHARS);
        assert_eq!(cut.len(), byte_ceiling(DISPLAY_MAX_CHARS));
        assert_eq!(cut.chars().count(), DISPLAY_MAX_CHARS);
        assert_eq!(bounded_field(&long, DISPLAY_MAX_CHARS), cut);
        let fits = "a".repeat(DISPLAY_MAX_CHARS);
        assert_eq!(bounded_field(&fits, DISPLAY_MAX_CHARS), fits);
        assert_eq!(bounded_field_bytes(&fits, DISPLAY_MAX_CHARS), fits);
    }

    /// **The person's bound is characters (verify finding S).** The banner,
    /// the launch notice, the `/cd` line and a jail refusal are read by a
    /// person, so an 80-character CJK path shows all 80 of its characters —
    /// `bounded_field` never trades characters for bytes — while the same
    /// value through [`bounded_field_bytes`] is cut to the byte ceiling, and
    /// that is the environment block's concern alone.
    #[test]
    fn bounded_field_counts_characters_so_a_cjk_path_shows_its_full_width() {
        let cjk = format!("/{}", "漢".repeat(DISPLAY_MAX_CHARS - 1));
        assert_eq!(cjk.chars().count(), DISPLAY_MAX_CHARS);
        assert_eq!(
            bounded_field(&cjk, DISPLAY_MAX_CHARS),
            cjk,
            "a value at the character ceiling is shown whole"
        );
        let longer = format!("/{}", "漢".repeat(199));
        let shown = bounded_field(&longer, DISPLAY_MAX_CHARS);
        assert_eq!(shown.chars().count(), DISPLAY_MAX_CHARS, "{shown}");
        assert!(shown.len() > byte_ceiling(DISPLAY_MAX_CHARS), "{shown}");
        // The prompt's function cuts the same value to the byte ceiling.
        let paid = bounded_field_bytes(&longer, DISPLAY_MAX_CHARS);
        assert!(paid.len() <= byte_ceiling(DISPLAY_MAX_CHARS), "{paid}");
        assert!(paid.chars().count() < shown.chars().count(), "{paid}");
        // And what the byte function bounded, the character function leaves
        // alone: a phrase built from a byte-bounded root keeps its bound.
        assert_eq!(bounded_field(&paid, DISPLAY_MAX_CHARS), paid);
    }

    /// **The multibyte hardening (TASK-180), on the prompt's function.** A
    /// value made of three- and four-byte characters is bounded in bytes as
    /// well as characters: it renders no longer than the byte ceiling, still
    /// around one elision mark, still cut at character boundaries (valid UTF-8
    /// by construction), and a bounded value passed through again is
    /// unchanged. Before this bound a 200-character CJK path came out at 80
    /// characters and 240 bytes — three times the ASCII row the ceiling sweeps
    /// call the worst case.
    #[test]
    fn a_multibyte_value_is_bounded_in_bytes_too_and_still_elided_in_the_middle() {
        for (script, ch) in [("cjk", '漢'), ("astral", '𝔘')] {
            assert!(ch.len_utf8() >= 3, "{script}: not a multibyte fixture");
            let long = ch.to_string().repeat(200);
            for (max_chars, what) in [(DISPLAY_MAX_CHARS, "display"), (NAME_MAX_CHARS, "name")] {
                let out = bounded_field_bytes(&long, max_chars);
                let ceiling = byte_ceiling(max_chars);
                assert!(
                    out.len() <= ceiling,
                    "{script} {what}: {} bytes over a {ceiling}-byte ceiling: {out}",
                    out.len()
                );
                assert!(out.chars().count() <= max_chars, "{script} {what}: {out}");
                assert_eq!(
                    out.matches(ELISION).count(),
                    1,
                    "{script} {what}: one elision mark, kept: {out}"
                );
                assert!(
                    out.starts_with(ch) && out.ends_with(ch),
                    "{script} {what}: cut in the middle, not at an end: {out}"
                );
                // As long as the byte ceiling allows: one more character on
                // either side would cross it.
                assert!(
                    out.len() + ch.len_utf8() > ceiling,
                    "{script} {what}: elided further than the byte ceiling requires: {} bytes",
                    out.len()
                );
                assert_eq!(
                    bounded_field_bytes(&out, max_chars),
                    out,
                    "{script} {what}: idempotent"
                );
                assert_eq!(
                    bounded_field(&out, max_chars),
                    out,
                    "{script} {what}: the character bound leaves a byte-bounded value alone"
                );
            }
        }
        // A short multibyte value under both bounds is left alone.
        assert_eq!(
            bounded_field_bytes("~/文档/项目", DISPLAY_MAX_CHARS),
            "~/文档/项目"
        );
        // Two-byte characters at the character ceiling exceed the byte ceiling
        // and are elided too — the bound is bytes, not "wide characters".
        let latin = "é".repeat(DISPLAY_MAX_CHARS);
        let out = bounded_field_bytes(&latin, DISPLAY_MAX_CHARS);
        assert!(out.len() <= byte_ceiling(DISPLAY_MAX_CHARS), "{out}");
        assert!(out.contains(ELISION), "{out}");
    }

    // ---- kind_phrase (BR-1's phrases, BR-3's one term) ----

    fn root(kind: RootKind, display: &str) -> SessionRoot {
        SessionRoot {
            display: display.to_owned(),
            kind,
            project_name: None,
            vcs_branch: None,
        }
    }

    /// One phrase per kind; "project" names a kind only when the kind is
    /// project (BR-3), with the branch when it was read and without it
    /// otherwise; a nameless project is `a project`, never an invented name.
    #[test]
    fn kind_phrase_spells_each_kind_once() {
        assert_eq!(kind_phrase(&root(RootKind::Home, "~")), "your home folder");
        assert_eq!(
            kind_phrase(&root(RootKind::FilesystemRoot, "/")),
            "the filesystem root"
        );
        assert_eq!(
            kind_phrase(&root(RootKind::Plain, "/opt/x")),
            "not a project"
        );
        let mut project = root(RootKind::Project, "~/Documents/GitHub/teton-code");
        assert_eq!(kind_phrase(&project), "a project");
        project.vcs_branch = Some("main".to_owned());
        assert_eq!(kind_phrase(&project), "a project, branch main");
        project.project_name = Some("teton-code".to_owned());
        assert_eq!(kind_phrase(&project), "project teton-code, branch main");
        project.vcs_branch = None;
        assert_eq!(kind_phrase(&project), "project teton-code");
        // An empty name or branch is absent, so no phrase dangles.
        project.project_name = Some(String::new());
        project.vcs_branch = Some("dev".to_owned());
        assert_eq!(kind_phrase(&project), "a project, branch dev");
        project.vcs_branch = Some(String::new());
        assert_eq!(kind_phrase(&project), "a project");
        for kind in [RootKind::Home, RootKind::FilesystemRoot] {
            assert!(!kind_phrase(&root(kind, "~")).contains("project"));
        }
    }

    /// The phrase bounds what it prints (ADR-2): a control character or a bidi
    /// override in a name cannot break the line, and a long name is cut to the
    /// name ceiling — in characters, since the phrase is read by a person, and
    /// idempotently on a value the prompt already byte-bounded.
    #[test]
    fn kind_phrase_bounds_the_name_and_the_branch() {
        let mut project = root(RootKind::Project, "~/x");
        project.project_name = Some("a\nb".to_owned());
        project.vcs_branch = Some("x".repeat(100));
        let phrase = kind_phrase(&project);
        assert!(!phrase.contains('\n'), "{phrase}");
        assert!(phrase.starts_with("project a?b, branch x"), "{phrase}");
        let branch = phrase.rsplit("branch ").next().unwrap();
        assert_eq!(branch.chars().count(), NAME_MAX_CHARS, "{phrase}");
        project.vcs_branch = Some("feat/\u{202E}x".to_owned());
        assert_eq!(kind_phrase(&project), "project a?b, branch feat/?x");
        // A byte-bounded name (the prompt's) comes through unchanged.
        let cjk = "漢".repeat(NAME_MAX_CHARS + 1);
        let paid = bounded_field_bytes(&cjk, NAME_MAX_CHARS);
        project.project_name = Some(paid.clone());
        project.vcs_branch = None;
        assert_eq!(kind_phrase(&project), format!("project {paid}"));
    }

    #[test]
    fn middle_elide_edge_ceilings() {
        assert_eq!(middle_elide("abc", 3), "abc");
        assert_eq!(middle_elide("abcd", 3), "a…d");
        assert_eq!(middle_elide("abcd", 1), "…");
        assert_eq!(middle_elide("abcd", 0), "");
        assert_eq!(middle_elide("", 0), "");
    }

    // ---- resolve_cwd_argument (BR-6/BR-7, AC-12) ----

    /// The grammar table, judged here; the CLI's `--cwd` and `/cd` tests
    /// iterate the same rows by name (AC-12).
    #[test]
    fn the_cwd_argument_grammar_table_holds() {
        let shell_cwd = Path::new(CWD_GRAMMAR_SHELL_CWD);
        let home = Path::new(CWD_GRAMMAR_HOME);
        for row in CWD_ARGUMENT_GRAMMAR {
            let got = resolve_cwd_argument(row.raw, shell_cwd, Some(home));
            match row.expect {
                Ok(path) => assert_eq!(
                    got.as_deref(),
                    Ok(Path::new(path)),
                    "row {:?} must resolve to {path}",
                    row.raw
                ),
                Err(fragment) => {
                    let err = got.expect_err("row must be refused");
                    assert!(
                        err.to_string().contains(fragment),
                        "row {:?}: {err} must mention {fragment:?}",
                        row.raw
                    );
                }
            }
        }
    }

    #[test]
    fn the_grammar_table_covers_the_ac_12_spellings() {
        for raw in ["~", "~/x", "rel", "/abs", ""] {
            assert!(
                CWD_ARGUMENT_GRAMMAR.iter().any(|row| row.raw == raw),
                "the AC-12 spelling {raw:?} is missing from the table"
            );
        }
    }

    #[test]
    fn a_tilde_without_a_home_is_refused_naming_the_argument() {
        let shell_cwd = Path::new("/work");
        let err = resolve_cwd_argument("~/x", shell_cwd, None).unwrap_err();
        assert_eq!(err, CwdArgError::NoHome("~/x".to_owned()));
        assert!(err.to_string().contains("`~/x`"), "{err}");
        assert!(err.to_string().contains("HOME is not set"), "{err}");
        let err = resolve_cwd_argument("~", shell_cwd, None).unwrap_err();
        assert_eq!(err, CwdArgError::NoHome("~".to_owned()));
    }

    /// `~user` and `~x` are not expanded — they are ordinary relative names.
    #[test]
    fn a_tilde_followed_by_a_name_is_a_relative_path() {
        let got = resolve_cwd_argument("~bob", Path::new("/work"), Some(Path::new("/home/u")));
        assert_eq!(got.as_deref(), Ok(Path::new("/work/~bob")));
    }

    #[test]
    fn a_doubled_slash_after_the_tilde_stays_under_home() {
        let got = resolve_cwd_argument("~//x", Path::new("/work"), Some(Path::new("/home/u")));
        assert_eq!(got.as_deref(), Ok(Path::new("/home/u/x")));
    }

    #[test]
    fn a_relative_shell_cwd_cannot_yield_an_absolute_path() {
        let err = resolve_cwd_argument("rel", Path::new("relative"), None).unwrap_err();
        assert_eq!(err, CwdArgError::NotAbsolute("rel".to_owned()));
        assert!(err.to_string().contains("`rel`"), "{err}");
    }

    #[test]
    fn resolution_never_touches_the_filesystem() {
        // A path that certainly does not exist resolves anyway: existence is the
        // daemon's judgement, not this function's.
        let got = resolve_cwd_argument(
            "/definitely/not/a/real/dir/anywhere",
            Path::new("/work"),
            None,
        );
        assert_eq!(
            got.as_deref(),
            Ok(Path::new("/definitely/not/a/real/dir/anywhere"))
        );
    }

    #[test]
    fn lexical_normalize_collapses_dots_and_keeps_the_root() {
        assert_eq!(
            lexical_normalize(Path::new("/a/./b/../c")),
            Path::new("/a/c")
        );
        assert_eq!(lexical_normalize(Path::new("/../a")), Path::new("/a"));
        assert_eq!(lexical_normalize(Path::new("/a/b/")), Path::new("/a/b"));
        assert_eq!(lexical_normalize(Path::new("../a")), Path::new("../a"));
        assert_eq!(lexical_normalize(Path::new("a/../../b")), Path::new("../b"));
    }
    /// **REQ-584 BR-8 / AC-9.** The bare-name gate is narrower than "not a path".
    ///
    /// The question is what the user *meant*, and anything shaped like a path is
    /// read as one — which is what keeps REQ-583's grammar unchanged wherever it
    /// applied. The new reading is reachable only from spellings the old one
    /// could resolve to nothing but a sibling directory.
    #[test]
    fn a_bare_name_is_narrower_than_not_a_path() {
        for name in ["teton-code", "src", "api", "a", "under_score", "CAPS"] {
            assert!(is_bare_project_name(name), "`{name}` is a bare name");
        }
        for path in [
            "~", "~/x", "./x", "../x", "/abs", "a/b", "x/", "-x", "--cwd", "", "   ",
        ] {
            assert!(
                !is_bare_project_name(path),
                "`{path}` is a path spelling or a flag, never a project name"
            );
        }
        // Whitespace is trimmed, as `resolve_cwd_argument` trims it — the two
        // readings must agree about what the argument even is.
        assert!(is_bare_project_name("  teton  "));
    }

    /// **AC-9.** The refusal names **both** readings, from one composer.
    #[test]
    fn the_two_reading_refusal_names_both_readings() {
        let line = cd_two_reading_refusal("nothing-known");
        assert!(
            line.contains("no directory `nothing-known` under the session root"),
            "{line}"
        );
        assert!(
            line.contains("no known project named `nothing-known`"),
            "{line}"
        );
        assert!(
            line.contains("/projects"),
            "and it points at the surface that would have listed them: {line}"
        );
    }
}
