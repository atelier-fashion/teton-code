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

use teton_protocol::methods::EffortView;
use teton_protocol::permissions::PermissionLevel;

/// The label the permission field carries.
const PERMISSIONS_LABEL: &str = "permissions";

/// The label the effort field carries (REQ-559).
const EFFORT_LABEL: &str = "effort";

/// What separates the fields.
const FIELD_SEPARATOR: &str = "  ·  ";

/// What the effort field shows when the setting reaches no model (REQ-559 BR-6).
///
/// Short because the row has to stay readable at 80 columns, and *present*
/// rather than omitted because the two facts differ: an absent field means this
/// daemon has no effort setting at all, while this means the setting exists and
/// is not being applied. Showing the level instead would be the misattribution
/// BR-6 forbids — the user set something and something else happened.
const EFFORT_NOT_APPLICABLE: &str = "n/a";

/// The effort value the status row should show, from the daemon's own view.
///
/// Derived rather than re-computed: [`EffortView`] is produced by
/// `teton_core::effort::resolve_effort`, the same function the router calls per
/// model call, so this reads the daemon's answer instead of forming a second
/// opinion about clamping (REQ-559's one-resolver chain, LESSON-456).
///
/// Three outcomes, and they are three different facts:
///
/// - `None` — the daemon sent no view. It predates the setting, so the row shows
///   the permission field alone rather than inventing a level.
/// - `Some("n/a")` — a view exists, but **nothing is receiving it**: either no
///   provider is registered, or every registered provider's resolution omits the
///   field (a local-only session is the case AC-7 names). BR-6 requires this be
///   reported as ignored rather than displayed as a level.
/// - `Some(level)` — the level being requested.
#[must_use]
pub fn effort_field(view: Option<&EffortView>) -> Option<String> {
    let view = view?;
    let reaches_a_model = view
        .providers
        .iter()
        .any(|row| row.resolved.level().is_some());
    Some(if reaches_a_model {
        view.level.to_string()
    } else {
        EFFORT_NOT_APPLICABLE.to_owned()
    })
}

/// The status row's content, or `None` when it does not fit `width`.
///
/// `effort` is the reasoning-effort value, normally produced by
/// [`effort_field`]. It stays an `Option<&str>` rather than taking an
/// [`EffortView`] directly so this function keeps no opinion about effort at
/// all: it lays out whatever string it is handed, which is what lets the layout
/// be tested exhaustively without constructing a provider table.
///
/// `width` is the terminal's column count, passed in rather than queried so this
/// function stays pure — the whole point of the module.
#[must_use]
pub fn status_line(level: PermissionLevel, effort: Option<&str>, width: usize) -> Option<String> {
    let mut row = format!("{PERMISSIONS_LABEL}: {}", level.name());
    if let Some(effort) = effort {
        row.push_str(FIELD_SEPARATOR);
        row.push_str(&format!("{EFFORT_LABEL}: {effort}"));
    }
    // Counted in characters, not bytes: a terminal column holds a character, and
    // `len()` would drop the row early on any non-ASCII effort label.
    (row.chars().count() <= width).then_some(row)
}

/// Reading the **client's** own sources, for the seam assertion below.
///
/// The client twin of `tetond`'s `call_sites::scan`, and a separate copy rather
/// than a shared one because the two live in different crates and each has to
/// walk its own `CARGO_MANIFEST_DIR`. The rule they implement — production
/// source only, test items stripped — is deliberately identical, and both rest
/// on the same house convention that `#[cfg(test)]` items come last in a file.
#[cfg(test)]
pub(crate) mod scan {
    use std::path::{Path, PathBuf};

    /// The client's own `src/` directory.
    fn client_src() -> PathBuf {
        Path::new(env!("CARGO_MANIFEST_DIR")).join("src")
    }

    /// Every `.rs` file under `dir`, recursively.
    fn rust_files(dir: &Path, out: &mut Vec<PathBuf>) {
        for entry in std::fs::read_dir(dir).expect("readable source dir") {
            let path = entry.expect("readable dir entry").path();
            if path.is_dir() {
                rust_files(&path, out);
            } else if path.extension().is_some_and(|ext| ext == "rs") {
                out.push(path);
            }
        }
    }

    /// `text` truncated at its first `#[cfg(test)]` item.
    fn strip_test_modules(text: &str) -> String {
        match text.find("\n#[cfg(test)]") {
            Some(at) => text[..at].to_owned(),
            None => text.to_owned(),
        }
    }

    /// `source` with whole-line comments removed, so an explanation of a rule is
    /// not mistaken for a violation of it.
    pub(crate) fn code_only(source: &str) -> String {
        source
            .lines()
            .filter(|line| !line.trim_start().starts_with("//"))
            .collect::<Vec<_>>()
            .join("\n")
    }

    /// Every production source in the client, as `(relative path, text)`.
    ///
    /// **A file that vanishes between the walk and the read is skipped, loudly**
    /// (LESSON-489). This repo's primary verification technique is the mutation
    /// pass — apply a change to `src/`, run the suite, observe red, revert — and
    /// that rewrites source files between `cargo test` invocations *by
    /// construction*. A scanner that `expect`ed every listed file to still be
    /// readable therefore fails at exactly the moment someone is relying on it,
    /// and the failure looks like a real finding: it was mistaken for one twice
    /// in REQ-561 before anyone recognised it (BUG-159).
    ///
    /// The tolerance is narrow on purpose. It covers a file that is *gone* from
    /// a fresh listing, not one this process merely could not read — that stays
    /// a loud failure, because a scanner silently skipping its target is worse
    /// than no scanner.
    pub(crate) fn production_sources() -> Vec<(String, String)> {
        let root = client_src();
        let mut files = Vec::new();
        rust_files(&root, &mut files);
        files.sort();
        files
            .iter()
            .filter_map(|path| {
                let text = match std::fs::read_to_string(path) {
                    Ok(text) => text,
                    Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
                        let mut current = Vec::new();
                        rust_files(&root, &mut current);
                        if !current.contains(path) {
                            eprintln!(
                                "status::scan: {} vanished between the directory listing \
                                 and the read, and is gone from a fresh listing; skipped",
                                path.display()
                            );
                            return None;
                        }
                        // Still listed — the window was a rename, not a deletion.
                        std::fs::read_to_string(path).unwrap_or_else(|err| {
                            panic!(
                                "source file {} is listed but unreadable: {err}",
                                path.display()
                            )
                        })
                    }
                    Err(err) => panic!("unreadable source {}: {err}", path.display()),
                };
                let rel = path
                    .strip_prefix(&root)
                    .expect("a file under src/")
                    .to_string_lossy()
                    .into_owned();
                Some((rel, strip_test_modules(&text)))
            })
            .collect()
    }
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

    // ---- REQ-560 AC-7, the effort half (now that REQ-559 has landed) --------

    use teton_protocol::effort::{EffortLevel, EffortOmission, ResolvedEffort};
    use teton_protocol::methods::ProviderEffortView;
    use teton_protocol::ProviderId;

    fn view(level: EffortLevel, rows: &[(&str, ResolvedEffort)]) -> EffortView {
        EffortView {
            level,
            providers: rows
                .iter()
                .map(|(id, resolved)| ProviderEffortView {
                    provider_id: ProviderId::from(*id),
                    resolved: *resolved,
                })
                .collect(),
        }
    }

    /// A daemon that predates the setting sends no view, and the row then shows
    /// the permission field alone rather than inventing a level.
    #[test]
    fn no_effort_view_means_no_effort_field() {
        assert_eq!(effort_field(None), None);
        assert_eq!(
            status_line(PermissionLevel::Guarded, None, 80).as_deref(),
            Some("permissions: guarded")
        );
    }

    /// The ordinary case: something is receiving the setting, so the row names
    /// the level the user asked for.
    #[test]
    fn a_level_that_reaches_a_model_is_shown() {
        let v = view(
            EffortLevel::High,
            &[("anthropic", ResolvedEffort::effort(EffortLevel::High))],
        );
        assert_eq!(effort_field(Some(&v)).as_deref(), Some("high"));
        assert_eq!(
            status_line(
                PermissionLevel::Edits,
                effort_field(Some(&v)).as_deref(),
                80
            )
            .as_deref(),
            Some("permissions: edits  ·  effort: high")
        );
    }

    /// **AC-7's named case**: a local-only session. `ShapeNone` is the omission
    /// REQ-559 BR-6 attaches to the local tier by name, so this is that case
    /// rather than a stand-in for it. Every provider's resolution omits the
    /// field, so the setting reaches no model — and REQ-559 BR-6
    /// requires that be reported as ignored rather than displayed as a level the
    /// model is not receiving. Showing `high` here would be the misattribution
    /// family of BUG-146: the user set something and something else happened.
    #[test]
    fn a_setting_that_reaches_no_model_renders_as_not_applicable() {
        let local_only = view(
            EffortLevel::High,
            &[("local", ResolvedEffort::omit(EffortOmission::ShapeNone))],
        );
        assert_eq!(effort_field(Some(&local_only)).as_deref(), Some("n/a"));
        assert_eq!(
            status_line(
                PermissionLevel::Guarded,
                effort_field(Some(&local_only)).as_deref(),
                80
            )
            .as_deref(),
            Some("permissions: guarded  ·  effort: n/a")
        );

        // No providers registered at all is the same fact: nothing receives it.
        let nobody = view(EffortLevel::Max, &[]);
        assert_eq!(effort_field(Some(&nobody)).as_deref(), Some("n/a"));
    }

    /// A mixed machine still shows the level: *some* model is receiving it, and
    /// the per-provider detail is `/effort`'s job, not a status row's — the row
    /// has to stay readable at 80 columns (OQ-4).
    #[test]
    fn one_provider_receiving_the_setting_is_enough_to_show_it() {
        let mixed = view(
            EffortLevel::Medium,
            &[
                ("local", ResolvedEffort::omit(EffortOmission::ShapeNone)),
                ("anthropic", ResolvedEffort::effort(EffortLevel::Medium)),
            ],
        );
        assert_eq!(effort_field(Some(&mixed)).as_deref(), Some("medium"));
    }

    /// A clamped resolution still reaches a model, so the row shows the level
    /// rather than `n/a` — clamping is a narrowing, not an omission, and
    /// `/effort` is where the per-provider clamp is spelled out.
    #[test]
    fn a_clamped_provider_still_counts_as_reached() {
        let clamped = view(
            EffortLevel::Max,
            &[(
                "openai",
                ResolvedEffort::clamped(EffortLevel::Max, EffortLevel::High),
            )],
        );
        assert_eq!(effort_field(Some(&clamped)).as_deref(), Some("max"));
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

    /// **REQ-560 AC-16: no status-line write reaches stdout outside the
    /// `Surface` / `Prompter` seams.**
    ///
    /// BR-12's point is forward-looking: the anticipated ratatui front-end
    /// inherits every line this UI draws by implementing the same two seams, and
    /// it inherits exactly nothing that went straight to stdout. A
    /// direct-to-stdout side channel would not fail any test today — it would
    /// look identical in a terminal — and would surface as a missing row in a
    /// front-end nobody has written yet.
    ///
    /// So the assertion is structural, in the same shape as AC-5's
    /// egress-predicate scan: this module composes content and never prints, and
    /// the only code allowed to put a status row on a terminal is
    /// `FramedStdinPrompter`, which **is** the `Prompter` seam.
    #[test]
    fn the_status_module_never_writes_to_a_terminal() {
        let source = scan::production_sources()
            .into_iter()
            .find(|(rel, _)| rel == "status.rs")
            .map(|(_, src)| scan::code_only(&src))
            .expect("this module is a production source");

        for needle in [
            "print!",
            "println!",
            "eprint!",
            "eprintln!",
            "io::stdout",
            "io::stderr",
            "stdout()",
            "stderr()",
        ] {
            assert!(
                !source.contains(needle),
                "status.rs names `{needle}`. The status row's content is a pure \
                 function (BR-8) and its bytes go out through the Prompter seam \
                 (BR-12); a direct-to-stdout side channel is invisible to the \
                 ratatui front-end that will implement those seams."
            );
        }
    }

    /// The other half of AC-16: the row reaches a terminal from exactly **one**
    /// place, and that place is the `Prompter` seam.
    ///
    /// Scans the whole client rather than one file, because the failure this
    /// guards against is a *new* call site somewhere else — the direction a rule
    /// like this actually rots in.
    #[test]
    fn only_the_prompter_seam_draws_the_status_row() {
        let sources = scan::production_sources();
        assert!(
            !sources.is_empty(),
            "the client source scan matched nothing — it is no longer looking at \
             anything"
        );

        let mut writers: Vec<&str> = Vec::new();
        for (rel, src) in &sources {
            let code = scan::code_only(src);
            // `set_status` hands *content* to the seam; only `draw` may emit it.
            if code.contains("below_rows") && rel != "prompt.rs" {
                writers.push(rel);
            }
        }
        assert!(
            writers.is_empty(),
            "the frame's below-row geometry is owned by prompt.rs (the Prompter \
             seam); these files also touch it and would be a second, drifting \
             copy: {writers:?}"
        );

        // Non-vacuity: the seam really does own it.
        let prompt = sources
            .iter()
            .find(|(rel, _)| rel == "prompt.rs")
            .map(|(_, src)| scan::code_only(src))
            .expect("prompt.rs is a production source");
        assert!(
            prompt.contains("below_rows"),
            "this assertion is only meaningful while prompt.rs owns the geometry"
        );
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
