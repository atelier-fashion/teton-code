//! Bundled **generic** ADLC templates (OQ-5).
//!
//! Structured mode needs requirement / plan / task artifacts to route
//! intelligence forward. In a repo that has never seen the ADLC, there are none —
//! so the daemon ships a generic skeleton for each and scaffolds them on demand
//! ([`ArtifactStore::scaffold`](super::artifacts::ArtifactStore::scaffold)). This
//! is deliberately **not** the author's personal ADLC toolkit: no REQ counters, no
//! `.adlc/` layout, no gate scripts — just the three phase artifacts with the
//! placeholders a spec/architect turn fills in. Anything richer is post-MVP.
//!
//! The templates are compiled into the binary with [`include_str!`] so a fresh
//! install needs nothing on disk to enter structured mode.

use super::artifacts::ArtifactKind;

/// The generic requirement skeleton (produced in the spec phase).
pub const REQUIREMENT_TEMPLATE: &str = include_str!("templates/requirement.md");
/// The generic plan skeleton (produced in the architect phase).
pub const PLAN_TEMPLATE: &str = include_str!("templates/plan.md");
/// The generic task skeleton (produced in the architect phase; consumed in the
/// implement phase — the artifact that carries intelligence to a cheap model).
pub const TASK_TEMPLATE: &str = include_str!("templates/task.md");

/// The bundled template for an artifact kind.
#[must_use]
pub fn template_for(kind: ArtifactKind) -> &'static str {
    match kind {
        ArtifactKind::Requirement => REQUIREMENT_TEMPLATE,
        ArtifactKind::Plan => PLAN_TEMPLATE,
        ArtifactKind::Task => TASK_TEMPLATE,
    }
}

/// Render a template for a fresh artifact, substituting the metadata known at
/// scaffold time (`{{id}}` and `{{title}}`).
///
/// The **content** placeholders (`{{description}}`, `{{approach}}`, …) are left
/// intact on purpose: a scaffolded artifact is a stub until a spec/architect turn
/// authors it, and the phase gate treats a still-templated artifact as invalid
/// (never auto-generated silently — see [`super::machine`]).
#[must_use]
pub fn render(template: &str, id: &str, title: &str) -> String {
    template.replace("{{id}}", id).replace("{{title}}", title)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_kind_has_a_nonempty_template() {
        for kind in [
            ArtifactKind::Requirement,
            ArtifactKind::Plan,
            ArtifactKind::Task,
        ] {
            assert!(!template_for(kind).trim().is_empty());
        }
    }

    #[test]
    fn render_substitutes_metadata_but_leaves_content_placeholders() {
        let rendered = render(REQUIREMENT_TEMPLATE, "demo-1", "A demo requirement");
        assert!(rendered.contains("demo-1"));
        assert!(rendered.contains("A demo requirement"));
        assert!(!rendered.contains("{{id}}"));
        assert!(!rendered.contains("{{title}}"));
        // A content placeholder still remains — the artifact is an unfilled stub.
        assert!(rendered.contains("{{description}}"));
    }
}
