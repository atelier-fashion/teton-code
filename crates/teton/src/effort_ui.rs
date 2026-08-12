//! Rendering for the global reasoning-effort setting (REQ-559 BR-9).
//!
//! **One** function renders it, and both surfaces call it: `teton effort` and
//! the in-session `/effort`. Two surfaces describing one setting must not be
//! able to disagree (LESSON-456, REQ-555 BR-4), and the way to guarantee that is
//! for there to be only one of them.
//!
//! The thing being rendered — [`EffortView`] — is likewise computed once, by the
//! daemon, using the same `resolve_effort` the router calls per model call. So
//! the chain is: one resolver in `teton-core`, one view on the wire, one
//! renderer here. Nothing in this module clamps, consults a ladder, or knows a
//! `ProviderKind` default; a client that re-derived any of that would be a
//! second implementation of the policy, and the clients are thin renderers by
//! architecture.
//!
//! ## Why each row says what it says
//!
//! BR-6 is the rule that shapes the wording: a setting the provider ignores must
//! be **reported** as ignored rather than displayed as a level the model is not
//! receiving. A row that showed `max` for a local provider — or for one that
//! refused the field — would be the misattribution family of BUG-146 and
//! BUG-153: the user set something and something else happened.

use teton_protocol::effort::{EffortOmission, ResolvedEffort};
use teton_protocol::methods::EffortView;

use crate::render::{LineKind, Surface};

/// The line shown when the daemon sends no effort view at all — an older daemon
/// that predates the setting. Distinct from every per-provider line below,
/// because "this build has no effort setting" is a different fact from "effort
/// does not apply to this provider".
const NO_VIEW: &str =
    "This daemon does not report a reasoning-effort setting (it predates the feature).";

/// What one provider's row says (REQ-559 BR-6, BR-9).
///
/// The resolution carries both the level being sent and the level the user
/// asked for, so a clamped row can say so without the renderer being handed the
/// setting separately — a row that silently showed the lower number would leave
/// a user wondering why `xhigh` did nothing.
#[must_use]
pub fn provider_line(provider_id: &str, resolved: ResolvedEffort) -> String {
    match resolved {
        // Clamped. Naming both numbers is the point: the user asked for one
        // thing and is getting another, and BR-5 says the effective level is
        // what counts. The pair travels on the value, so this reads the clamp
        // off the resolution rather than comparing against a setting the
        // renderer would otherwise have to be handed — a comparison the caller
        // could get wrong, and one more thing two surfaces could disagree on.
        ResolvedEffort::Effort { level, requested } if level != requested => {
            format!("{provider_id}: {level} (clamped from {requested} — this provider's ladder stops there)")
        }
        ResolvedEffort::Effort { level, .. } => format!("{provider_id}: {level}"),
        // No level on the wire, so no level in the row (BR-6).
        ResolvedEffort::ThinkingFlag => {
            format!("{provider_id}: thinking on (this provider takes a flag, not a level)")
        }
        // AC-5 asks for exactly this wording for the local provider.
        ResolvedEffort::Omit {
            reason: EffortOmission::ShapeNone,
        } => format!("{provider_id}: not applicable (this tier has no effort setting)"),
        ResolvedEffort::Omit {
            reason: EffortOmission::EmptyLadder,
        } => format!("{provider_id}: not applicable (its declared effort ladder is empty)"),
        // ADR-F's visibility condition. A runtime-discovered no-op is reported
        // exactly as loudly as a declared one, and the row says the refusal is
        // scoped to this session so a user knows a restart retries it.
        ResolvedEffort::Omit {
            reason: EffortOmission::RefusedThisSession,
        } => format!(
            "{provider_id}: refused the effort field this session — sending none \
             (the next session tries again)"
        ),
    }
}

/// Render the whole view: the current level, then one line per provider.
///
/// This is the function BR-9 requires both `teton effort` and `/effort` to go
/// through. It takes a `Surface` rather than writing to stdout so a future
/// ratatui front-end inherits it by implementing the same seam.
pub fn render(surface: &mut dyn Surface, view: Option<&EffortView>) {
    let Some(view) = view else {
        surface.line(LineKind::Notice, NO_VIEW);
        return;
    };
    surface.line(
        LineKind::Notice,
        &format!("Reasoning effort: {}", view.level),
    );
    if view.providers.is_empty() {
        surface.line(
            LineKind::Notice,
            "No providers are registered, so nothing is receiving it yet.",
        );
        return;
    }
    for row in &view.providers {
        surface.line(
            LineKind::Notice,
            &provider_line(&row.provider_id.0, row.resolved),
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::render::RecordingSurface;
    use teton_protocol::effort::EffortLevel;
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

    /// BR-6: no row ever shows a level the provider is not receiving. Asserted
    /// over every omission reason and the thinking-flag shape, because the
    /// failure this rules out is a row that looks fine and is wrong.
    #[test]
    fn no_row_shows_a_level_the_provider_is_not_receiving() {
        for resolved in [
            ResolvedEffort::ThinkingFlag,
            ResolvedEffort::omit(EffortOmission::ShapeNone),
            ResolvedEffort::omit(EffortOmission::EmptyLadder),
            ResolvedEffort::omit(EffortOmission::RefusedThisSession),
        ] {
            let line = provider_line("p", resolved);
            assert!(
                !line.contains("max"),
                "a provider not receiving a level must not display one: {line}",
            );
        }
    }

    /// AC-5's exact requirement: the local provider reads "not applicable",
    /// not a level.
    #[test]
    fn the_local_tier_reads_not_applicable() {
        let line = provider_line("local", ResolvedEffort::omit(EffortOmission::ShapeNone));
        assert!(line.contains("not applicable"), "{line}");
    }

    /// A clamped row names both numbers. Showing only the effective level would
    /// leave a user who set `xhigh` with no explanation of why nothing changed.
    #[test]
    fn a_clamped_row_names_both_the_effective_and_the_requested_level() {
        let line = provider_line(
            "kimi",
            ResolvedEffort::clamped(EffortLevel::Xhigh, EffortLevel::High),
        );
        assert!(line.contains("high") && line.contains("xhigh"), "{line}");
        assert!(line.contains("clamped"), "{line}");
        // An unclamped row does not cry wolf.
        let plain = provider_line("kimi", ResolvedEffort::effort(EffortLevel::High));
        assert!(!plain.contains("clamped"), "{plain}");
    }

    /// ADR-F: the refusal row says it is session-scoped, so a user knows a
    /// restart retries rather than assuming the provider is permanently off.
    #[test]
    fn the_refusal_row_says_it_is_scoped_to_this_session() {
        let line = provider_line(
            "mystery",
            ResolvedEffort::omit(EffortOmission::RefusedThisSession),
        );
        assert!(line.contains("this session"), "{line}");
        assert!(line.contains("next session"), "{line}");
    }

    #[test]
    fn the_view_renders_the_level_then_one_line_per_provider() {
        let mut surface = RecordingSurface::default();
        render(
            &mut surface,
            Some(&view(
                EffortLevel::Xhigh,
                &[
                    ("anthropic", ResolvedEffort::effort(EffortLevel::Xhigh)),
                    (
                        "kimi",
                        ResolvedEffort::clamped(EffortLevel::Xhigh, EffortLevel::High),
                    ),
                    ("local", ResolvedEffort::omit(EffortOmission::ShapeNone)),
                ],
            )),
        );
        let out = surface.lines_of(LineKind::Notice).join("\n");
        assert!(out.contains("Reasoning effort: xhigh"), "{out}");
        assert!(out.contains("anthropic: xhigh"), "{out}");
        assert!(out.contains("kimi: high (clamped from xhigh"), "{out}");
        assert!(out.contains("local: not applicable"), "{out}");
    }

    /// An older daemon sends no view. That is a different fact from "effort does
    /// not apply here", and the surface says which one it is rather than
    /// rendering an empty list that reads as "nothing is configured".
    #[test]
    fn an_absent_view_says_the_daemon_predates_the_setting() {
        let mut surface = RecordingSurface::default();
        render(&mut surface, None);
        assert!(surface.any_line_contains(LineKind::Notice, "predates"));
    }
}
