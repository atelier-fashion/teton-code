//! The entry frame's status row — content only (REQ-560 BR-8).
//!
//! What this module renders is a **pure function of session state**: no
//! terminal, no clock, no I/O, nothing to mock. Placement — where the row goes,
//! and the cursor arithmetic that keeps the frame from stranding it — belongs to
//! [`crate::prompt::FramedStdinPrompter`], which is the `Prompter` seam and the
//! only thing here that touches a terminal.
//!
//! ## Why the split is a requirement rather than a preference
//!
//! The entry frame renders only when stdin is a TTY, and `cli_e2e` drives
//! `teton` over pipes. A status row written the obvious way — content composed
//! at the point it is printed — would therefore ship with **no automated
//! coverage at all**, and the TTY gate would be the reason: the thing that hides
//! the feature from piped users hides it from the test suite too (LESSON-481,
//! earned by REQ-556 one REQ earlier).
//!
//! So the content is decided here, where a unit test can read it with the gate
//! out of the way, and only the few bytes that reach the terminal stay gated.
//!
//! ## Degrading, not truncating
//!
//! A terminal too narrow for the row gets **no row** ([`status_line`] answers
//! `None`), never a clipped one. `permissions: fu` is worse than nothing: it is
//! a security-relevant label that reads as a different, more permissive posture
//! than the one in force. The value stays reachable either way — bare
//! `/permissions` prints it and works on a pipe (BR-10), which is what makes
//! dropping the row an acceptable degradation rather than a loss.

use teton_protocol::permissions::PermissionLevel;

/// The label the permission field carries.
const PERMISSIONS_LABEL: &str = "permissions";

/// The label the effort field carries (REQ-559).
const EFFORT_LABEL: &str = "effort";

/// What separates the fields.
const FIELD_SEPARATOR: &str = "  ·  ";

/// The status row's content, or `None` when it does not fit `width`.
///
/// `effort` is the reasoning-effort value REQ-559 renders. It is `Option` rather
/// than a required field because **the permission half of this REQ is
/// independently shippable**: until REQ-559 lands there is no effort level to
/// show, and the row is the permission field alone. REQ-559 fills the parameter
/// in; nothing here needs to change when it does, and this REQ deliberately adds
/// no `/effort` command to go with it (BR-14).
///
/// `width` is the terminal's column count, passed in rather than queried so this
/// function stays pure — the whole point of the module.
#[must_use]
pub fn status_line(
    level: PermissionLevel,
    effort: Option<&str>,
    width: usize,
) -> Option<String> {
    let mut row = format!("{PERMISSIONS_LABEL}: {}", level.name());
    if let Some(effort) = effort {
        row.push_str(FIELD_SEPARATOR);
        row.push_str(&format!("{EFFORT_LABEL}: {effort}"));
    }
    // Counted in characters, not bytes: a terminal column holds a character, and
    // `len()` would drop the row early on any non-ASCII effort label.
    (row.chars().count() <= width).then_some(row)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// REQ-560 AC-7: the content function answers for every (level × effort)
    /// pair with no terminal involved.
    ///
    /// Driven off `PermissionLevel::ALL` so a fifth level is covered the moment
    /// it joins the array (AC-17), and over the effort values REQ-559 will
    /// supply — including the BR-6 "not applicable" rendering a local-only
    /// session gets, which is a string like any other to this function.
    #[test]
    fn every_level_and_effort_pair_renders_at_a_generous_width() {
        for level in PermissionLevel::ALL {
            for effort in [None, Some("low"), Some("high"), Some("n/a")] {
                let row = status_line(*level, effort, 200)
                    .unwrap_or_else(|| panic!("{level}/{effort:?} rendered nothing at width 200"));
                assert!(
                    row.contains(level.name()),
                    "the row must name the level: {row}"
                );
                assert!(row.starts_with(PERMISSIONS_LABEL), "{row}");
                match effort {
                    Some(value) => {
                        assert!(row.contains(EFFORT_LABEL), "{row}");
                        assert!(row.contains(value), "{row}");
                    }
                    // Until REQ-559 lands the row is the permission field alone
                    // — and it must not carry an empty or placeholder effort
                    // field, which would advertise a setting that does not exist.
                    None => assert!(!row.contains(EFFORT_LABEL), "{row}"),
                }
            }
        }
    }

    #[test]
    fn the_permission_only_row_is_exactly_the_label_and_the_level() {
        assert_eq!(
            status_line(PermissionLevel::Guarded, None, 80).as_deref(),
            Some("permissions: guarded")
        );
        assert_eq!(
            status_line(PermissionLevel::Full, Some("high"), 80).as_deref(),
            Some("permissions: full  ·  effort: high")
        );
    }

    /// REQ-560 AC-12 / BR-13: a terminal too narrow for the row produces no row,
    /// no panic, and no truncation.
    #[test]
    fn a_row_that_does_not_fit_is_dropped_whole_never_clipped() {
        let full = status_line(PermissionLevel::Guarded, None, usize::MAX)
            .expect("an unbounded width always fits");
        let exact = full.chars().count();

        // Fits at exactly its own width, and at nothing narrower.
        assert_eq!(
            status_line(PermissionLevel::Guarded, None, exact).as_deref(),
            Some(full.as_str())
        );
        for width in 0..exact {
            assert_eq!(
                status_line(PermissionLevel::Guarded, None, width),
                None,
                "width {width} should have dropped the row, not clipped it"
            );
        }
    }

    /// The dropped-row rule holds for every level, so no level is the one that
    /// silently clips.
    #[test]
    fn every_level_drops_rather_than_clips_at_width_zero() {
        for level in PermissionLevel::ALL {
            assert_eq!(status_line(*level, None, 0), None, "{level}");
            assert_eq!(status_line(*level, Some("high"), 0), None, "{level}");
        }
    }

    /// A wide effort label is measured in characters, so a multi-byte value does
    /// not drop a row that would have fitted.
    #[test]
    fn width_is_measured_in_characters_not_bytes() {
        let effort = "нормальный";
        let row = status_line(PermissionLevel::Guarded, Some(effort), 200)
            .expect("a generous width fits");
        let chars = row.chars().count();
        assert!(
            row.len() > chars,
            "this fixture is only meaningful if the label is multi-byte"
        );
        assert!(
            status_line(PermissionLevel::Guarded, Some(effort), chars).is_some(),
            "a row measured in bytes would have been dropped here"
        );
    }
}
