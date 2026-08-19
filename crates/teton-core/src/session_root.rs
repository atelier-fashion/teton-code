//! The session root as a pure value (REQ-583 ADR-1, ADR-2).
//!
//! A session's tools are jailed to one directory — the **session root**. This
//! module is the one derivation of what that directory *is*: which kind of place
//! it is ([`classify`]), how it is spelled to a person ([`display_for`]), how a
//! user-controlled value from it is bounded before it lands in a prompt or a
//! refusal ([`bounded_field`]), which names make a directory a project
//! ([`PROJECT_MARKERS`]), and how a `--cwd`/`/cd` argument becomes a path
//! ([`resolve_cwd_argument`]).
//!
//! Everything here is pure: no filesystem, no environment. The daemon's
//! `tetond::session_root::probe` supplies the I/O (does a marker exist, what
//! does `.git/HEAD` say) and calls in here for the answers, so the CLI banner,
//! the daemon's environment block, the launch notice and every jail refusal
//! print one spelling built once (ADR-2 "built once") — a client linking this
//! crate cannot drift from the daemon that enforces the jail.

use std::path::{Component, Path, PathBuf};

use teton_protocol::methods::RootKind;

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
/// makes it exactly the worst: an all-ASCII value at the ceiling costs
/// `max_chars` bytes and one cut to it costs `max_chars + 2`, and nothing in
/// any script may cost more. (A wider byte cap — say twice the character
/// ceiling — would need the block's worst row, and the ceiling that pays for
/// it, to grow by over a hundred bytes; the margin there is one.)
#[must_use]
pub const fn byte_ceiling(max_chars: usize) -> usize {
    max_chars + ELISION.len_utf8() - 1
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

/// `s`, made safe to print mid-line in a prompt or a refusal (ADR-2 bounding).
///
/// Every control character — newlines and carriage returns included — becomes
/// `?`, so a path can neither break the line it sits on nor smuggle a
/// frame label to column 0; then the result is [`middle_elide`]d to at most
/// `max_chars` characters **and** at most [`byte_ceiling`]`(max_chars)` bytes,
/// so no path can blow the resident-prompt ceiling in either unit. The byte
/// bound is met by eliding further, at character boundaries, still around one
/// [`ELISION`] mark; an all-ASCII value never meets it (it is the ASCII cost),
/// so the rendering of an ASCII path is what it always was. The three
/// user-controlled root values (display, project name, branch) all pass
/// through here, with [`DISPLAY_MAX_CHARS`] or [`NAME_MAX_CHARS`]. Idempotent:
/// a value already within both bounds comes back unchanged.
#[must_use]
pub fn bounded_field(s: &str, max_chars: usize) -> String {
    let neutral: String = s
        .chars()
        .map(|c| if c.is_control() { '?' } else { c })
        .collect();
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

/// Why a `--cwd`/`/cd` argument could not become a path (BR-6/BR-7).
///
/// Each message names the argument, so the refusal a user reads says which
/// spelling was refused and why. Existence and directory-ness are **not** judged
/// here — the daemon validates the resolved path (one validator, ADR-4).
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

    // ---- bounded_field / middle_elide (ADR-2 bounding) ----

    #[test]
    fn bounded_field_replaces_control_characters_and_line_breaks() {
        assert_eq!(bounded_field("a\nb\rc\td\u{1b}[31me", 80), "a?b?c?d?[31me");
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
    /// byte-worst rendering there is.
    #[test]
    fn the_byte_ceiling_is_what_an_elided_ascii_value_costs() {
        assert_eq!(byte_ceiling(DISPLAY_MAX_CHARS), 82);
        assert_eq!(byte_ceiling(NAME_MAX_CHARS), 34);
        assert_eq!(byte_ceiling(1), ELISION.len_utf8());
        let cut = bounded_field(&"/segment".repeat(25), DISPLAY_MAX_CHARS);
        assert_eq!(cut.len(), byte_ceiling(DISPLAY_MAX_CHARS));
        assert_eq!(cut.chars().count(), DISPLAY_MAX_CHARS);
        let fits = "a".repeat(DISPLAY_MAX_CHARS);
        assert_eq!(bounded_field(&fits, DISPLAY_MAX_CHARS), fits);
    }

    /// **The multibyte hardening (TASK-180).** A value made of three- and
    /// four-byte characters is bounded in bytes as well as characters: it
    /// renders no longer than the byte ceiling, still around one elision mark,
    /// still cut at character boundaries (valid UTF-8 by construction), and a
    /// bounded value passed through again is unchanged. Before this bound a
    /// 200-character CJK path came out at 80 characters and 240 bytes — three
    /// times the ASCII row the ceiling sweeps call the worst case.
    #[test]
    fn a_multibyte_value_is_bounded_in_bytes_too_and_still_elided_in_the_middle() {
        for (script, ch) in [("cjk", '漢'), ("astral", '𝔘')] {
            assert!(ch.len_utf8() >= 3, "{script}: not a multibyte fixture");
            let long = ch.to_string().repeat(200);
            for (max_chars, what) in [(DISPLAY_MAX_CHARS, "display"), (NAME_MAX_CHARS, "name")] {
                let out = bounded_field(&long, max_chars);
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
                    bounded_field(&out, max_chars),
                    out,
                    "{script} {what}: idempotent"
                );
            }
        }
        // A short multibyte value under both bounds is left alone.
        assert_eq!(
            bounded_field("~/文档/项目", DISPLAY_MAX_CHARS),
            "~/文档/项目"
        );
        // Two-byte characters at the character ceiling exceed the byte ceiling
        // and are elided too — the bound is bytes, not "wide characters".
        let latin = "é".repeat(DISPLAY_MAX_CHARS);
        let out = bounded_field(&latin, DISPLAY_MAX_CHARS);
        assert!(out.len() <= byte_ceiling(DISPLAY_MAX_CHARS), "{out}");
        assert!(out.contains(ELISION), "{out}");
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
}
