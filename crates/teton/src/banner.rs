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
use std::path::PathBuf;

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

/// The working directory as shown in the banner, `~`-abbreviated. `None` when
/// the cwd is unreadable — the banner simply omits the line.
#[must_use]
pub fn cwd_display() -> Option<String> {
    let cwd = std::env::current_dir().ok()?;
    if let Some(home) = std::env::var_os("HOME").map(PathBuf::from) {
        if let Ok(rest) = cwd.strip_prefix(&home) {
            return Some(if rest.as_os_str().is_empty() {
                "~".to_owned()
            } else {
                format!("~/{}", rest.display())
            });
        }
    }
    Some(cwd.display().to_string())
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
