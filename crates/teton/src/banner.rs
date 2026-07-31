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
//! `TERM=dumb`.

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
pub fn print(surface: &mut dyn Surface, version: &str, cwd: Option<&str>, color: bool) {
    for line in lines(version, cwd, color) {
        surface.line(LineKind::Info, &line);
    }
}

/// The banner's lines, fully formatted. Pure — the testable core.
fn lines(version: &str, cwd: Option<&str>, color: bool) -> Vec<String> {
    let (peak, bold, dim, reset) = if color {
        ("\x1b[36m", "\x1b[1m", "\x1b[2m", "\x1b[0m")
    } else {
        ("", "", "", "")
    };
    let mut out = Vec::with_capacity(SKYLINE.len() + 4);
    out.push(String::new());
    for row in SKYLINE {
        out.push(format!("{peak}{row}{reset}"));
    }
    out.push(String::new());
    out.push(format!("  {bold}Teton Code{reset} v{version} — {TAGLINE}"));
    if let Some(cwd) = cwd {
        out.push(format!("  {dim}cwd: {cwd}{reset}"));
    }
    out.push(String::new());
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
        print(&mut surface, "9.9.9", Some("~/somewhere"), false);
        assert!(surface.any_line_contains(LineKind::Info, "Teton Code"));
        assert!(surface.any_line_contains(LineKind::Info, "v9.9.9"));
        assert!(surface.any_line_contains(LineKind::Info, "cwd: ~/somewhere"));
    }

    #[test]
    fn banner_omits_the_cwd_line_when_unknown() {
        let mut surface = RecordingSurface::new();
        print(&mut surface, "0.1.0", None, false);
        assert!(!surface.any_line_contains(LineKind::Info, "cwd:"));
    }

    #[test]
    fn uncolored_banner_contains_no_escape_codes_and_fits_a_narrow_terminal() {
        for line in lines("0.1.0", Some("~/x"), false) {
            assert!(!line.contains('\x1b'), "escape code in {line:?}");
            assert!(line.chars().count() <= 60, "line too wide: {line:?}");
        }
    }

    #[test]
    fn colored_banner_resets_every_styled_line() {
        for line in lines("0.1.0", Some("~/x"), true) {
            if line.contains('\x1b') {
                let last_style = line.rfind("\x1b[").unwrap();
                let last_reset = line.rfind("\x1b[0m");
                assert_eq!(last_reset, Some(last_style), "unreset style in {line:?}");
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
