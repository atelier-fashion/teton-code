//! Reasoning-effort **policy**: the per-kind capability defaults and the one
//! resolution every surface shares (REQ-559).
//!
//! The vocabulary itself — [`EffortLevel`], [`EffortLadder`], [`ReasoningShape`],
//! [`ResolvedEffort`] — lives in `teton_protocol::effort` and is re-exported
//! here, so `teton_core::EffortLevel` stays the stable path for daemon-side
//! code. See that module for why the split exists (short version: the CLI and
//! the events reach the protocol crate but not this one, and two definitions of
//! one vocabulary is the drift BR-3 rules out).
//!
//! What lives *here* is everything that needs [`ProviderKind`]: the per-kind
//! default table (ADR-E) and [`resolve_effort`], the single function the router,
//! `teton effort` and `/effort` all call (BR-9, ADR-G).
//!
//! ## Why effort is sent at all
//!
//! Teton sends an effort value on **every** model call to a provider that
//! accepts one (BR-1). Omitting the field is not "no opinion" — it inherits the
//! provider's default, and at least one target provider (Kimi K3) defaults to
//! `max`. A request that declines to state its effort therefore silently bills
//! at the most expensive setting on that axis, which for a product whose
//! headline promise is cost control is worse than not supporting effort at all.
//! That is LESSON-443's shape: a behavior predicated on the *absence* of a
//! field, correct only while the field does not exist.
//!
//! ## Where the clamp runs
//!
//! Exactly once, at route time, in [`resolve_effort`] (ADR-G). The resulting
//! [`ResolvedEffort`] then flows to three consumers that must not be able to
//! disagree: the `route_decided` event, the adapter's request body, and the
//! `teton effort` / `/effort` surfaces. Two components computing one fact
//! separately is the drift LESSON-456 is about; one value flowing to three
//! readers cannot disagree with itself.

pub use teton_protocol::effort::{
    level_list, EffortLadder, EffortLevel, EffortOmission, ParseEffortLevelError, ReasoningShape,
    ResolvedEffort, ALL_LEVELS,
};

use crate::entities::{ProviderCapabilities, ProviderKind};

/// The reasoning shape a provider of this kind uses when it declares none
/// (ADR-E, resolving OQ-2).
///
/// An unknown OpenAI-compatible endpoint **states its effort**. Defaulting to
/// [`ReasoningShape::None`] would reintroduce the Kimi-defaults-to-`max` hazard
/// at exactly the BYOM endpoint Teton knows least about — the defect this REQ
/// exists to fix, reappearing at the worst provider. The opposite risk is
/// bounded and already handled: a server that rejects the unknown field answers
/// 400, which BR-12 turns into a typed error and a fallback. A stated effort
/// some endpoints refuse is recoverable; an unstated effort that silently bills
/// at `max` is not.
#[must_use]
pub const fn default_shape_for(kind: ProviderKind) -> ReasoningShape {
    match kind {
        // llama.cpp exposes no effort parameter; thinking is a chat-template
        // property (BR-6). A declared no-op, not a silent one.
        ProviderKind::Local => ReasoningShape::None,
        ProviderKind::OpenaiCompatible | ProviderKind::Anthropic | ProviderKind::Custom => {
            ReasoningShape::EffortOnly
        }
    }
}

/// The effort ladder a provider of this kind uses when it declares none (ADR-E,
/// resolving OQ-1).
///
/// # Why an unknown endpoint gets `{low, high}` and not the full ladder
///
/// The tempting default is the whole canonical set, and it is wrong in this
/// REQ's own failure direction. Teton has no Kimi provider kind, so a Kimi K3 is
/// registered as `openai-compatible`. With a permissive default it would receive
/// `xhigh`, which K3 does not accept → 400 → BR-12 falls back to sending no
/// effort → the call lands back on Kimi's `max` default. The originating defect,
/// reached the long way round.
///
/// `{low, high}` is the **intersection** of every published target ladder —
/// OpenAI (`minimal/low/medium/high`), Kimi K3 (`low/high/max`) and DeepSeek V4
/// (`low/high/xhigh/max`) all contain both rungs. With the default effort of
/// `high`, an undeclared endpoint receives `high`: accepted everywhere, and on
/// Kimi a real downgrade from `max`, which is the defect fixed. A user who wants
/// the higher rungs declares the ladder in config, which is OQ-1's override
/// doing its job.
#[must_use]
pub fn default_ladder_for(kind: ProviderKind) -> EffortLadder {
    match kind {
        ProviderKind::Local => EffortLadder::EMPTY,
        // The published Anthropic set: all five, which is why the canonical
        // ladder is Anthropic's.
        ProviderKind::Anthropic => EffortLadder::all(),
        // The conservative intersection — see the doc comment above.
        ProviderKind::OpenaiCompatible | ProviderKind::Custom => {
            EffortLadder::from_levels(&[EffortLevel::Low, EffortLevel::High])
        }
    }
}

/// **The** effort resolution (BR-9, ADR-G).
///
/// Called once per model call at route time, and again — with
/// `refused_this_session = false` — by `teton effort` / `/effort` to build their
/// per-provider view. Two surfaces describing one setting must not be able to
/// drift, so they do not each compute it (LESSON-456, REQ-555 BR-4).
///
/// `kind` is passed separately from `caps` because [`ProviderCapabilities`] does
/// not know its own kind; keeping both as parameters is what leaves this
/// function pure and independently testable.
///
/// `refused_this_session` is a bare `bool` because the session memo belongs to
/// the daemon (ADR-F) — this module holds no state.
#[must_use]
pub fn resolve_effort(
    requested: EffortLevel,
    kind: ProviderKind,
    caps: &ProviderCapabilities,
    refused_this_session: bool,
) -> ResolvedEffort {
    // Checked first: a session refusal outranks any declared shape, because the
    // provider has already told us it will not accept the field (ADR-F).
    if refused_this_session {
        return ResolvedEffort::omit(EffortOmission::RefusedThisSession);
    }

    let shape = caps
        .reasoning_shape
        .unwrap_or_else(|| default_shape_for(kind));

    match shape {
        ReasoningShape::None => ResolvedEffort::omit(EffortOmission::ShapeNone),
        ReasoningShape::ThinkingFlagOnly => ResolvedEffort::ThinkingFlag,
        ReasoningShape::EffortOnly => {
            let ladder = caps
                .effort_ladder
                .unwrap_or_else(|| default_ladder_for(kind));
            // Clamped unconditionally. A "skip the clamp when the ladder is
            // complete" shortcut would be a guard keyed on a condition that
            // stops holding the moment a provider declares a narrower ladder
            // (LESSON-443), and would make the AC-12 identity-clamp mutation
            // undetectable on the common path.
            match ladder.clamp(requested) {
                Some(level) => ResolvedEffort::effort(level),
                None => ResolvedEffort::omit(EffortOmission::EmptyLadder),
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn caps(shape: Option<ReasoningShape>, ladder: Option<EffortLadder>) -> ProviderCapabilities {
        ProviderCapabilities {
            reasoning_shape: shape,
            effort_ladder: ladder,
            ..ProviderCapabilities::default()
        }
    }

    // -- the ladder itself ---------------------------------------------------

    /// The derived `Ord` on `EffortLevel` is the ladder order, and `clamp`
    /// depends on it. A future reorder of the variants must go red here rather
    /// than silently inverting every clamp.
    /// A hand-written serde pair is where silent drift lives (ADR-C), so both
    /// directions are pinned against a literal.
    /// AC-3 / BR-5. Table-driven across all five canonical levels × four
    /// ladders, including the spec's three named cases.
    ///
    /// The ladders are deliberately **narrow and irregular**. A table whose
    /// ladders were all close to the full canonical set would pass under an
    /// identity clamp, which would make AC-12's second mutation undetectable.
    /// BR-5's direction, stated as a property rather than as rows: the clamp
    /// never rounds up while a supported rung at-or-below exists. A clamp that
    /// preferred the nearest rung in either direction would pass the named
    /// AC-3 cases by luck on some tables; this catches it everywhere.
    #[test]
    fn per_kind_default_table() {
        assert_eq!(default_shape_for(ProviderKind::Local), ReasoningShape::None);
        assert!(default_ladder_for(ProviderKind::Local).is_empty());

        for kind in [
            ProviderKind::OpenaiCompatible,
            ProviderKind::Anthropic,
            ProviderKind::Custom,
        ] {
            assert_eq!(
                default_shape_for(kind),
                ReasoningShape::EffortOnly,
                "{kind:?} must state its effort (BR-4 / OQ-2)",
            );
        }

        assert_eq!(
            default_ladder_for(ProviderKind::Anthropic),
            EffortLadder::all()
        );
        let conservative = EffortLadder::from_levels(&[EffortLevel::Low, EffortLevel::High]);
        assert_eq!(
            default_ladder_for(ProviderKind::OpenaiCompatible),
            conservative
        );
        assert_eq!(default_ladder_for(ProviderKind::Custom), conservative);
    }

    /// The property ADR-E's choice rests on: `{low, high}` is a subset of every
    /// non-empty default ladder, so a value clamped into the conservative
    /// default is accepted by every provider whose ladder we do know.
    #[test]
    fn conservative_default_is_a_subset_of_every_non_empty_default_ladder() {
        for kind in [
            ProviderKind::Local,
            ProviderKind::OpenaiCompatible,
            ProviderKind::Anthropic,
            ProviderKind::Custom,
        ] {
            let ladder = default_ladder_for(kind);
            if ladder.is_empty() {
                continue;
            }
            assert!(
                ladder.contains(EffortLevel::Low),
                "{kind:?} must accept low"
            );
            assert!(
                ladder.contains(EffortLevel::High),
                "{kind:?} must accept high"
            );
        }
    }

    /// With the default effort of `high`, an undeclared endpoint of every remote
    /// kind sends `high` — accepted by OpenAI, Kimi K3 and DeepSeek alike. This
    /// is the concrete statement of ADR-E's intersection argument.
    #[test]
    fn undeclared_remote_endpoint_sends_high_by_default() {
        for kind in [
            ProviderKind::OpenaiCompatible,
            ProviderKind::Anthropic,
            ProviderKind::Custom,
        ] {
            assert_eq!(
                resolve_effort(EffortLevel::default(), kind, &caps(None, None), false),
                ResolvedEffort::effort(EffortLevel::High),
                "{kind:?}",
            );
        }
    }

    // -- resolve_effort ------------------------------------------------------

    /// BR-1's direct regression, and the reason OQ-2 closed the way it did: an
    /// undeclared remote endpoint must never resolve to "send nothing", because
    /// sending nothing inherits the provider's default and Kimi K3's is `max`.
    #[test]
    fn undeclared_remote_provider_never_omits_by_shape() {
        for kind in [
            ProviderKind::OpenaiCompatible,
            ProviderKind::Anthropic,
            ProviderKind::Custom,
        ] {
            for requested in ALL_LEVELS {
                let got = resolve_effort(requested, kind, &caps(None, None), false);
                assert!(
                    matches!(got, ResolvedEffort::Effort { .. }),
                    "{kind:?} at {requested} resolved to {got:?}, but an undeclared \
                     remote endpoint must state its effort (BR-1, BR-4/OQ-2)",
                );
            }
        }
    }

    #[test]
    fn local_kind_is_a_declared_no_op() {
        // BR-6 / BR-7: even at `max`, the local tier sends nothing — and says so.
        assert_eq!(
            resolve_effort(
                EffortLevel::Max,
                ProviderKind::Local,
                &caps(None, None),
                false
            ),
            ResolvedEffort::omit(EffortOmission::ShapeNone),
        );
    }

    #[test]
    fn declared_shape_overrides_the_kind_default() {
        // A remote provider declared `none` sends nothing...
        assert_eq!(
            resolve_effort(
                EffortLevel::Max,
                ProviderKind::OpenaiCompatible,
                &caps(Some(ReasoningShape::None), None),
                false,
            ),
            ResolvedEffort::omit(EffortOmission::ShapeNone),
        );
        // ...and one declared `thinking_flag_only` sends the flag, carrying no
        // level, whatever was requested.
        for requested in ALL_LEVELS {
            assert_eq!(
                resolve_effort(
                    requested,
                    ProviderKind::OpenaiCompatible,
                    &caps(Some(ReasoningShape::ThinkingFlagOnly), None),
                    false,
                ),
                ResolvedEffort::ThinkingFlag,
            );
        }
    }

    #[test]
    fn declared_ladder_overrides_the_kind_default() {
        let deepseek = caps(
            None,
            Some(EffortLadder::from_levels(&[
                EffortLevel::Low,
                EffortLevel::High,
                EffortLevel::Xhigh,
                EffortLevel::Max,
            ])),
        );
        // Without the override this would clamp to `high` (the {low, high}
        // default); with it, `xhigh` goes through.
        assert_eq!(
            resolve_effort(
                EffortLevel::Xhigh,
                ProviderKind::OpenaiCompatible,
                &deepseek,
                false
            ),
            ResolvedEffort::effort(EffortLevel::Xhigh),
        );
        assert_eq!(
            resolve_effort(
                EffortLevel::Xhigh,
                ProviderKind::OpenaiCompatible,
                &caps(None, None),
                false,
            ),
            ResolvedEffort::effort(EffortLevel::High),
            "the conservative default still clamps",
        );
    }

    #[test]
    fn explicitly_empty_ladder_is_reported_not_silently_dropped() {
        assert_eq!(
            resolve_effort(
                EffortLevel::High,
                ProviderKind::OpenaiCompatible,
                &caps(None, Some(EffortLadder::EMPTY)),
                false,
            ),
            ResolvedEffort::omit(EffortOmission::EmptyLadder),
        );
    }

    /// ADR-F: a session refusal outranks every declared shape, so a provider
    /// that has already answered 400 is not asked again this session.
    #[test]
    fn session_refusal_wins_over_any_shape() {
        for shape in [
            None,
            Some(ReasoningShape::EffortOnly),
            Some(ReasoningShape::ThinkingFlagOnly),
            Some(ReasoningShape::None),
        ] {
            assert_eq!(
                resolve_effort(
                    EffortLevel::Max,
                    ProviderKind::OpenaiCompatible,
                    &caps(shape, None),
                    true,
                ),
                ResolvedEffort::omit(EffortOmission::RefusedThisSession),
                "shape {shape:?}",
            );
        }
    }

    /// ADR-A: no variant carries two wire fields, so an adapter cannot emit
    /// both. `level()` is the accessor an adapter or surface reads, and it is
    /// `Some` only for the one variant that sends a level.
    /// `ProviderCapabilities` must stay `Copy` (ADR-C). If the ladder is ever
    /// re-typed as a `Vec`, this stops compiling — which is the point.
    #[test]
    fn capabilities_stay_copy() {
        fn assert_copy<T: Copy>(_: &T) {}
        let c = caps(Some(ReasoningShape::EffortOnly), Some(EffortLadder::all()));
        assert_copy(&c);
        let copied = c;
        assert_eq!(copied, c);
    }
}
