//! The startup banner.
//!
//! An interactive session opens the way the range does: the outline of the
//! Tetons, then a short identity block — product, version, working directory —
//! before the first prompt. Everything renders through the [`Surface`] seam
//! like the rest of the UI, so tests assert on semantic lines and a future
//! ratatui front-end can draw the same banner its own way.
//!
//! The banner is cosmetic and must never make output worse somewhere else, so
//! the caller gates it on stdout being a terminal: piped output, subcommands,
//! and the e2e suites see exactly the byte stream they saw before. Colour is a
//! second, independent gate ([`color_enabled`]) honouring `NO_COLOR` and
//! `TERM=dumb`, and it is applied by the surface: this module names each line's
//! [`LineKind`] and never writes an escape sequence itself. It cannot — the
//! surface neutralizes control characters in the text it is handed, so a banner
//! that spelled its own `\x1b[36m` would print a literal `[36m` to the user.

use std::io::IsTerminal;
use std::path::Path;

use teton_core::session_root::{bounded_field, kind_phrase, DISPLAY_MAX_CHARS};
use teton_protocol::methods::{RootKind, SessionRoot};

use crate::render::{LineKind, Surface};

/// The Cathedral Group from the valley floor, south to north: South Teton,
/// Middle Teton, the Grand, Mount Owen, Teewinot. Generated as the upper
/// envelope of five unit-slope peaks so every stroke lines up; edit the peaks,
/// not the strokes.
const SKYLINE: [&str; 9] = [
    r"                          /\",
    r"                         /  \",
    r"                        /    \     /\",
    r"               /\      /      \   /  \",
    r"              /  \    /        \ /    \     /\",
    r"        /\   /    \  /                 \   /  \",
    r"       /  \ /      \/                   \ /    \",
    r"      /                                          \",
    r"______                                           ____",
];

/// One line, under the peaks, saying what this is.
const TAGLINE: &str = "local-first AI coding agent";

/// Emit the banner: skyline, identity line, working directory, and a blank
/// line of breathing room on each side.
pub fn print(surface: &mut dyn Surface, version: &str, cwd: Option<&str>) {
    for (kind, line) in lines(version, cwd) {
        surface.line(kind, &line);
    }
}

/// The banner's lines, each tagged with the class it is drawn as. Pure — the
/// testable core. No line carries styling of its own; see the module docs.
fn lines(version: &str, cwd: Option<&str>) -> Vec<(LineKind, String)> {
    let mut out = Vec::with_capacity(SKYLINE.len() + 4);
    out.push((LineKind::Info, String::new()));
    for row in SKYLINE {
        out.push((LineKind::BannerArt, row.to_owned()));
    }
    out.push((LineKind::Info, String::new()));
    out.push((
        LineKind::BannerTitle,
        format!("  Teton Code v{version} — {TAGLINE}"),
    ));
    if let Some(cwd) = cwd {
        out.push((LineKind::BannerMeta, format!("  cwd: {cwd}")));
    }
    out.push((LineKind::Info, String::new()));
    out
}

/// Whether colour codes should be emitted: a terminal on stdout, no `NO_COLOR`
/// veto, and a `TERM` that is not `dumb`.
#[must_use]
pub fn color_enabled() -> bool {
    std::io::stdout().is_terminal()
        && std::env::var_os("NO_COLOR").is_none_or(|v| v.is_empty())
        && std::env::var("TERM").map_or(true, |t| t != "dumb")
}

/// The session root as shown in the banner's `cwd:` line, `~`-abbreviated and
/// bounded like every other spelling of a root.
///
/// A thin wrapper over [`teton_core::session_root::display_for`] — the one
/// spelling the daemon's environment block, the launch notice, the jail refusals
/// and `/cd` all print (REQ-583 ADR-1) — passed through [`bounded_field`] at
/// [`DISPLAY_MAX_CHARS`] as every other root spelling is (ADR-2), so a
/// 200-character or control-laden path cannot run away with the banner line
/// either. The banner draws before the session exists, so this is the one root
/// fact the client computes locally, and it computes it with the daemon's own
/// functions rather than a rule of its own. The caller supplies the root (the
/// resolved `--cwd`, or the shell's directory); the home folder is read from
/// `HOME` here, as it always was.
#[must_use]
pub fn cwd_display(session_root: &Path) -> String {
    let home = crate::home_dir();
    bounded_field(
        &teton_core::session_root::display_for(session_root, home.as_deref()),
        DISPLAY_MAX_CHARS,
    )
}

/// The root spelled for a person: `{display} ({kind phrase})` — `~ (your home
/// folder)`, `/ (the filesystem root)`, `~/scratch (not a project)`,
/// `~/Documents/GitHub/teton-code (project teton-code, branch main)`.
///
/// One function for the three lines that say where a session is — the launch
/// notice, the `session_root_changed` line and `/cd`'s bare form — so a root is
/// never described two ways (BR-3's one-term rule, applied to the kind
/// vocabulary). The kind phrase is [`kind_phrase`], the very function the
/// daemon's environment block prints, so the person and the model read one
/// vocabulary for one root. The daemon builds the display and names bounded
/// (ADR-2), and [`bounded_field`] is idempotent on a bounded value, so passing
/// them through again costs nothing and protects the line against a daemon
/// that did not.
#[must_use]
pub fn root_line(root: &SessionRoot) -> String {
    format!(
        "{} ({})",
        bounded_field(&root.display, DISPLAY_MAX_CHARS),
        kind_phrase(root)
    )
}

/// The one-line notice a non-project root earns under the banner (REQ-583 BR-5,
/// ADR-5) — `None` for a project, which needs no announcing.
///
/// Pure content: it names the root — by the term every surface uses for it,
/// the *session root* (BR-3) — states the consequence in the user's terms, and
/// names both remedies (`teton --cwd <path>` and `/cd <path>`). The bytes are
/// TTY-gated by the caller — this function is not a banner line (the ≤ 60
/// column rule is [`lines`]'s alone), which is why it stands apart from
/// [`print`]. `/cd` re-fires it through the same function when the new root is
/// not a project (BR-8), so launch and move announce with one voice.
#[must_use]
pub fn root_notice(root: &SessionRoot) -> Option<String> {
    let walks = match root.kind {
        RootKind::Project => return None,
        RootKind::FilesystemRoot => "the whole filesystem",
        RootKind::Home | RootKind::Plain => "all of it",
    };
    Some(format!(
        "Not inside a project — the session root is {}; tools are scoped to it: every search \
         walks {walks}, and privacy boundaries declared for a project do not apply here. Run \
         teton from the project, `teton --cwd <path>`, or `/cd <path>` here.",
        root_line(root)
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::render::RecordingSurface;

    #[test]
    fn banner_names_the_product_version_and_cwd() {
        let mut surface = RecordingSurface::new();
        print(&mut surface, "9.9.9", Some("~/somewhere"));
        assert!(surface.any_line_contains(LineKind::BannerTitle, "Teton Code"));
        assert!(surface.any_line_contains(LineKind::BannerTitle, "v9.9.9"));
        assert!(surface.any_line_contains(LineKind::BannerMeta, "cwd: ~/somewhere"));
    }

    #[test]
    fn banner_omits_the_cwd_line_when_unknown() {
        let mut surface = RecordingSurface::new();
        print(&mut surface, "0.1.0", None);
        assert!(!surface.any_line_contains(LineKind::BannerMeta, "cwd:"));
    }

    /// The regression this module exists downstream of: the banner used to write
    /// its own SGR codes into the line text, and the surface's control-character
    /// guard replaced each ESC with a space — so the user read a literal `[36m`
    /// at the head of every ridge line and a `[0m` at its tail. No banner line
    /// may carry an escape, and none may carry the visible debris of one.
    #[test]
    fn banner_text_carries_no_escape_codes_and_fits_a_narrow_terminal() {
        for (_, line) in lines("0.1.0", Some("~/x")) {
            assert!(!line.contains('\x1b'), "escape code in {line:?}");
            for debris in ["[36m", "[0m", "[1m", "[2m"] {
                assert!(!line.contains(debris), "escape debris in {line:?}");
            }
            assert!(line.chars().count() <= 60, "line too wide: {line:?}");
        }
    }

    /// The skyline is art: a stray space at the head of a row shears the ridge.
    /// Every row must start with the glyph the art intends, which is exactly what
    /// the ESC-to-space substitution used to break.
    #[test]
    fn skyline_rows_are_not_shifted() {
        for (kind, line) in lines("0.1.0", Some("~/x")) {
            if kind == LineKind::BannerArt {
                assert!(
                    line.trim_start().starts_with(['/', '\\', '_']),
                    "ridge row does not begin on a stroke: {line:?}"
                );
            }
        }
    }

    // ---- REQ-583: the session root under the banner ----

    fn root(kind: RootKind, display: &str) -> SessionRoot {
        SessionRoot {
            display: display.to_owned(),
            kind,
            project_name: None,
            vcs_branch: None,
        }
    }

    /// AC-8: the notice is `None` for a project and, for each other kind, one
    /// line naming the root's display, the consequence, and both remedies.
    #[test]
    fn root_notice_is_none_for_a_project_and_names_root_consequence_and_both_remedies_otherwise() {
        assert_eq!(
            root_notice(&SessionRoot {
                display: "~/Documents/GitHub/teton-code".to_owned(),
                kind: RootKind::Project,
                project_name: Some("teton-code".to_owned()),
                vcs_branch: Some("main".to_owned()),
            }),
            None,
            "a project root needs no announcing"
        );
        for (kind, display, phrase) in [
            (RootKind::Home, "~", "your home folder"),
            (RootKind::FilesystemRoot, "/", "the filesystem root"),
            (RootKind::Plain, "~/scratch", "not a project"),
        ] {
            let notice =
                root_notice(&root(kind, display)).expect("a non-project root is announced");
            assert_eq!(notice.lines().count(), 1, "one line: {notice:?}");
            assert!(notice.starts_with("Not inside a project"), "{notice}");
            // (a) the root, by display and kind, called what every surface
            // calls it — the session root (BR-3) — and what the scoping is.
            assert!(
                notice.contains(&format!(
                    "the session root is {display} ({phrase}); tools are scoped to it"
                )),
                "{notice}"
            );
            // (b) the consequence, in the user's terms.
            assert!(notice.contains("every search walks"), "{notice}");
            assert!(
                notice.contains("privacy boundaries declared for a project do not apply here"),
                "{notice}"
            );
            // (c) both remedies.
            assert!(notice.contains("`teton --cwd <path>`"), "{notice}");
            assert!(notice.contains("`/cd <path>`"), "{notice}");
        }
    }

    /// The filesystem root's consequence is spelled for what it is: a search
    /// there does not walk "all of it" — it walks the whole filesystem.
    #[test]
    fn the_filesystem_root_notice_says_the_whole_filesystem() {
        let notice = root_notice(&root(RootKind::FilesystemRoot, "/")).unwrap();
        assert!(
            notice.contains("every search walks the whole filesystem"),
            "{notice}"
        );
        let home = root_notice(&root(RootKind::Home, "~")).unwrap();
        assert!(home.contains("every search walks all of it"), "{home}");
    }

    /// The notice is not a banner line: [`lines`] never carries it, so the
    /// ≤ 60-column rule above is untouched, and the notice may run long.
    #[test]
    fn the_notice_is_not_a_banner_line() {
        assert!(lines("0.1.0", Some("~"))
            .iter()
            .all(|(_, line)| !line.contains("Not inside")));
        assert!(
            root_notice(&root(RootKind::Home, "~"))
                .unwrap()
                .chars()
                .count()
                > 60
        );
    }

    /// One phrase per kind, and "project" only for a project (BR-3): with the
    /// branch when the daemon read one, without it otherwise.
    #[test]
    fn root_line_spells_each_kind_once() {
        assert_eq!(
            root_line(&root(RootKind::Home, "~")),
            "~ (your home folder)"
        );
        assert_eq!(
            root_line(&root(RootKind::FilesystemRoot, "/")),
            "/ (the filesystem root)"
        );
        assert_eq!(
            root_line(&root(RootKind::Plain, "/opt/x")),
            "/opt/x (not a project)"
        );
        let mut project = root(RootKind::Project, "~/Documents/GitHub/teton-code");
        project.project_name = Some("teton-code".to_owned());
        assert_eq!(
            root_line(&project),
            "~/Documents/GitHub/teton-code (project teton-code)"
        );
        project.vcs_branch = Some("main".to_owned());
        assert_eq!(
            root_line(&project),
            "~/Documents/GitHub/teton-code (project teton-code, branch main)"
        );
        // "project" names a kind only when the kind is project (BR-3): the other
        // phrases either omit the word or negate it.
        for kind in [RootKind::Home, RootKind::FilesystemRoot] {
            assert!(!root_line(&root(kind, "~")).contains("project"));
        }
        assert!(root_line(&root(RootKind::Plain, "~/x")).ends_with("(not a project)"));
    }

    /// **One kind phrase (verify finding E).** The parenthesis in `root_line`
    /// is `teton_core::session_root::kind_phrase` — the function the daemon's
    /// environment block prints its kind with — for every kind, with and
    /// without a branch, and for the defensive nameless-project arm. The
    /// daemon's own test pins the same for its block, so the two surfaces
    /// agree on the phrase for the same root by construction.
    #[test]
    fn root_line_kind_is_the_shared_kind_phrase() {
        let mut project = root(RootKind::Project, "~/Documents/GitHub/teton-code");
        let mut roots = vec![
            root(RootKind::Home, "~"),
            root(RootKind::FilesystemRoot, "/"),
            root(RootKind::Plain, "/opt/x"),
            project.clone(),
        ];
        project.project_name = Some("teton-code".to_owned());
        roots.push(project.clone());
        project.vcs_branch = Some("main".to_owned());
        roots.push(project.clone());
        project.project_name = None;
        roots.push(project);
        for root in roots {
            assert_eq!(
                root_line(&root),
                format!("{} ({})", root.display, kind_phrase(&root)),
                "{root:?}"
            );
        }
        // The nameless arm reads as the daemon's does — `a project`, never an
        // invented name and never a dangling `project `.
        let mut nameless = root(RootKind::Project, "~/x");
        assert_eq!(root_line(&nameless), "~/x (a project)");
        nameless.vcs_branch = Some("dev".to_owned());
        assert_eq!(root_line(&nameless), "~/x (a project, branch dev)");
    }

    /// The fixed words a `root_line` adds around its bounded values: the
    /// space-paren-paren and the longest kind phrase's own words
    /// (`project , branch `) — what the length assertion below allows on top of
    /// the display and two name ceilings.
    const ROOT_LINE_FIXED_CHARS: usize = " ()".len() + "project , branch ".len();

    /// The line bounds what it prints (ADR-2): a control character cannot break
    /// it and an unbounded display cannot run away with it.
    #[test]
    fn root_line_bounds_the_display_and_the_names() {
        use teton_core::session_root::NAME_MAX_CHARS;

        let mut long = root(RootKind::Plain, &"/very-long-segment".repeat(20));
        assert!(long.display.chars().count() > DISPLAY_MAX_CHARS);
        let line = root_line(&long);
        let display_part = line.split(" (").next().unwrap();
        assert!(display_part.chars().count() <= DISPLAY_MAX_CHARS, "{line}");
        long.kind = RootKind::Project;
        long.project_name = Some("a\nb".to_owned());
        long.vcs_branch = Some("x".repeat(100));
        let line = root_line(&long);
        assert!(!line.contains('\n'), "{line}");
        assert!(line.contains("project a?b, branch x"), "{line}");
        assert!(
            line.chars().count() <= DISPLAY_MAX_CHARS + 2 * NAME_MAX_CHARS + ROOT_LINE_FIXED_CHARS,
            "{line}"
        );
        // A bidi override in a branch cannot make the line read backwards.
        long.vcs_branch = Some("feat/\u{202E}x".to_owned());
        assert!(
            root_line(&long).ends_with("branch feat/?x)"),
            "{}",
            root_line(&long)
        );
    }

    /// The banner's `cwd:` spelling is teton-core's `display_for` — `~` for the
    /// home folder and `~/rest` under it — so the client cannot drift from the
    /// daemon's own spelling of the same path (ADR-1); and it is bounded like
    /// every other root spelling (ADR-2), so a very long path is middle-elided
    /// on the banner too.
    #[test]
    fn cwd_display_is_the_shared_display_rule_and_is_bounded() {
        let Some(home) = crate::home_dir() else {
            return; // no HOME in this environment: nothing to abbreviate against
        };
        assert_eq!(cwd_display(&home), "~");
        assert_eq!(cwd_display(&home.join("x/y")), "~/x/y");
        assert_eq!(cwd_display(Path::new("/")), "/");
        let long = home.join("segment/".repeat(25));
        let shown = cwd_display(&long);
        assert_eq!(shown.chars().count(), DISPLAY_MAX_CHARS, "{shown}");
        assert!(shown.starts_with("~/segment/"), "{shown}");
        assert!(shown.contains('…'), "{shown}");
    }

    #[test]
    fn skyline_is_a_mountain() {
        // The summit row is the narrow apex `/\`; the base row is the widest
        // and grounded with `_`. A failure here means the art was hand-edited
        // into something that no longer reads as a skyline.
        assert_eq!(SKYLINE[0].trim(), r"/\");
        let base = SKYLINE.last().unwrap();
        assert!(base.starts_with('_'));
        assert!(SKYLINE.iter().all(|row| row.len() <= base.len()));
    }
}
