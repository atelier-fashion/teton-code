//! Reasoning effort: the canonical ladder, the per-provider clamp, and the one
//! resolution every surface shares (REQ-559).
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
//! ## The three types that carry the design
//!
//! - [`EffortLevel`] — the canonical ladder `low < medium < high < xhigh < max`
//!   (BR-3). The only vocabulary the router, the config, the CLI and the events
//!   speak; provider-native spellings exist only inside the adapters.
//! - [`EffortLadder`] — the levels one provider actually accepts, as a **bitset**
//!   so the type stays `Copy`. That is not a micro-optimization: a `Vec` here
//!   breaks `Copy` on [`crate::ProviderCapabilities`] and on the adapter-side
//!   `CapabilityProfile`, rippling across ~30 `ProviderCapabilities::default()`
//!   sites for no benefit (REQ-559 ADR-C). A closed five-element ordered set is
//!   what a bitset is for.
//! - [`ResolvedEffort`] — **what goes on the wire**, as a return type rather
//!   than a pair of flags (ADR-A). Kimi K2.5/K2.6 answer HTTP 400 when both
//!   `thinking` and `reasoning_effort` are sent, so the mutual exclusion is a
//!   correctness constraint (BR-4). No variant of this enum names two fields, so
//!   an adapter matching on it *cannot* emit both — the illegal state is
//!   unrepresentable rather than merely tested. This is the same structural
//!   posture as the harness's frame containment (architecture ADR-009).
//!
//! ## Where the clamp runs
//!
//! Exactly once, at route time, in [`resolve_effort`] (ADR-G). The resulting
//! [`ResolvedEffort`] then flows to three consumers that must not be able to
//! disagree: the `route_decided` event, the adapter's request body, and the
//! `teton effort` / `/effort` surfaces. Two components computing one fact
//! separately is the drift LESSON-456 is about; one value flowing to three
//! readers cannot disagree with itself.

use serde::{Deserialize, Deserializer, Serialize, Serializer};

use crate::entities::{ProviderCapabilities, ProviderKind};

/// The canonical effort ladder (BR-3): ordered, closed, and the only vocabulary
/// outside an adapter.
///
/// Anthropic's five-level set, chosen as the superset of every target
/// provider's ladder. Provider-native spellings happen to coincide with these
/// for Anthropic, DeepSeek and Kimi, so no per-provider mapping table exists —
/// see [`ResolvedEffort`].
///
/// `minimal` is deliberately **absent**: it exists only on OpenAI, ladder
/// members are canonical levels by definition, and adding a sixth rung for one
/// vendor would force the per-provider spelling table this design otherwise
/// does not need (ADR-E).
///
/// # Ordering
///
/// The derived [`Ord`] is the ladder order, and that is load-bearing for
/// [`EffortLadder::clamp`] — it is correct **only** because the variants are
/// declared in ascending order. [`ladder_order_matches_declaration_order`]
/// pins it so a future reorder goes red instead of silently inverting the clamp.
///
/// [`ladder_order_matches_declaration_order`]: #
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EffortLevel {
    /// Least reasoning; cheapest.
    Low,
    /// Between `low` and `high`.
    Medium,
    /// The declared default (BR-1) and Anthropic's own default.
    High,
    /// Above `high`.
    Xhigh,
    /// Most reasoning; most expensive.
    Max,
}

impl Default for EffortLevel {
    /// `high` — the declared default (BR-1).
    ///
    /// This is the value the resolution chain lands on when the user has set
    /// nothing. It is emphatically **not** an absent field: the whole point of
    /// this REQ is that an unstated effort inherits the provider's default, and
    /// at least one provider's is `max`.
    fn default() -> Self {
        Self::High
    }
}

/// Every canonical level, ascending. The single source of the ladder's order and
/// membership — iteration, parsing, rendering and the bitset all read it.
pub const ALL_LEVELS: [EffortLevel; 5] = [
    EffortLevel::Low,
    EffortLevel::Medium,
    EffortLevel::High,
    EffortLevel::Xhigh,
    EffortLevel::Max,
];

impl EffortLevel {
    /// This level's rung index, `0` (lowest) through `4` (highest).
    #[must_use]
    const fn rung(self) -> u8 {
        match self {
            Self::Low => 0,
            Self::Medium => 1,
            Self::High => 2,
            Self::Xhigh => 3,
            Self::Max => 4,
        }
    }

    /// The rung at `index`, or `None` when out of range.
    #[must_use]
    const fn from_rung(index: u8) -> Option<Self> {
        match index {
            0 => Some(Self::Low),
            1 => Some(Self::Medium),
            2 => Some(Self::High),
            3 => Some(Self::Xhigh),
            4 => Some(Self::Max),
            _ => None,
        }
    }

    /// The wire / config spelling. Canonical spellings are also the wire
    /// spellings on Anthropic, DeepSeek and Kimi (BR-3).
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Low => "low",
            Self::Medium => "medium",
            Self::High => "high",
            Self::Xhigh => "xhigh",
            Self::Max => "max",
        }
    }
}

impl std::fmt::Display for EffortLevel {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// The error [`std::str::FromStr`] returns for an unrecognised effort spelling.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("unknown effort level `{got}` — expected one of: {}", crate::effort::level_list())]
pub struct ParseEffortLevelError {
    /// What was typed.
    pub got: String,
}

/// The five canonical spellings, comma-separated — the one place a user-facing
/// "expected one of" list is built, so a new rung cannot appear in the enum
/// without appearing in the error.
#[must_use]
pub fn level_list() -> String {
    ALL_LEVELS
        .iter()
        .map(|l| l.as_str())
        .collect::<Vec<_>>()
        .join(", ")
}

impl std::str::FromStr for EffortLevel {
    type Err = ParseEffortLevelError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        ALL_LEVELS
            .into_iter()
            .find(|l| l.as_str() == s.trim())
            .ok_or_else(|| ParseEffortLevelError { got: s.to_owned() })
    }
}

/// Which reasoning field(s) a provider's request body accepts (BR-4).
///
/// **Declared per provider, never sniffed from a response.** A capability
/// conclusion drawn from one HTTP status is a guess that outlives the condition
/// that produced it; see [`EffortOmission::RefusedThisSession`] for the runtime
/// degradation that deliberately does *not* mutate this declaration.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ReasoningShape {
    /// Send the effort field alone (`reasoning_effort` /
    /// `output_config.effort`). The default for every remote kind (ADR-E).
    EffortOnly,
    /// Send the thinking flag alone. For providers that accept a boolean and
    /// 400 when an effort field rides along (Kimi K2.5/K2.6).
    ThinkingFlagOnly,
    /// Send neither. The local tier: llama.cpp exposes no effort parameter, and
    /// thinking is a property of the chat template (BR-6).
    None,
}

/// The set of canonical levels one provider accepts, as a bitset (ADR-C).
///
/// `Copy` is the whole reason for the representation — see the module docs.
/// Serializes as an ascending, de-duplicated `Vec<EffortLevel>`, so the config
/// and wire spelling is the obvious list.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct EffortLadder(u8);

impl EffortLadder {
    /// A ladder accepting nothing. Resolves to
    /// [`EffortOmission::EmptyLadder`], never to a silently-dropped setting.
    pub const EMPTY: Self = Self(0);

    /// Build from any slice; order is irrelevant and duplicates collapse.
    #[must_use]
    pub fn from_levels(levels: &[EffortLevel]) -> Self {
        let mut bits = 0u8;
        for level in levels {
            bits |= 1 << level.rung();
        }
        Self(bits)
    }

    /// Every canonical level, `low` through `max`.
    #[must_use]
    pub fn all() -> Self {
        Self::from_levels(&ALL_LEVELS)
    }

    /// Whether this ladder accepts `level`.
    #[must_use]
    pub const fn contains(self, level: EffortLevel) -> bool {
        self.0 & (1 << level.rung()) != 0
    }

    /// Whether this ladder accepts nothing.
    #[must_use]
    pub const fn is_empty(self) -> bool {
        self.0 == 0
    }

    /// The accepted levels, ascending.
    pub fn levels(self) -> impl Iterator<Item = EffortLevel> {
        ALL_LEVELS.into_iter().filter(move |l| self.contains(*l))
    }

    /// Clamp a canonical level into this ladder (BR-5).
    ///
    /// **Nearest supported at-or-below, then nearest supported above.** The
    /// direction is cost-conservative and deliberate (OQ-3, closed): a clamp
    /// that rounded *up* on the user's behalf would bill them for a rung they
    /// did not ask for, and a user who wants the higher rung can name it.
    ///
    /// `None` only for an empty ladder — the one case where there is no rung to
    /// send at all.
    #[must_use]
    pub fn clamp(self, requested: EffortLevel) -> Option<EffortLevel> {
        if self.is_empty() {
            return None;
        }
        // Down first: the highest supported rung at or below the request.
        let below = (0..=requested.rung())
            .rev()
            .filter_map(EffortLevel::from_rung)
            .find(|l| self.contains(*l));
        if below.is_some() {
            return below;
        }
        // Nothing lower exists, so take the lowest supported rung above.
        (requested.rung()..ALL_LEVELS.len() as u8)
            .filter_map(EffortLevel::from_rung)
            .find(|l| self.contains(*l))
    }
}

impl Serialize for EffortLadder {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        self.levels().collect::<Vec<_>>().serialize(serializer)
    }
}

impl<'de> Deserialize<'de> for EffortLadder {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let levels = Vec::<EffortLevel>::deserialize(deserializer)?;
        Ok(Self::from_levels(&levels))
    }
}

/// Why a call carries no reasoning field.
///
/// A *reason*, not a bare absence: BR-6 requires a setting the provider ignores
/// to be **reported** as ignored rather than displayed as a level the model is
/// not receiving. A reasonless `None` would give the surface nothing to say,
/// which is the misattribution family of BUG-146 and BUG-153 — the user set
/// something and something else happened.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EffortOmission {
    /// The provider's declared shape is [`ReasoningShape::None`] — the local
    /// tier (BR-6), and the cap that keeps a global bump to `max` from
    /// inflating a local-pinned category (BR-7).
    ShapeNone,
    /// The provider declares an empty ladder: no rung to send.
    EmptyLadder,
    /// This provider refused the effort field earlier in this session (BR-12).
    ///
    /// **Session-scoped, and the declared shape is unchanged** (ADR-F). BR-12
    /// forbids *silent retries* — making a failing request again and hoping —
    /// and remembering does the opposite: it declines a request already known to
    /// fail. Persisting the refusal, or downgrading the declared
    /// [`ReasoningShape`], would be sniffing a capability from a response, which
    /// BR-4 forbids in as many words. The next session tries again, so a
    /// provider that gains support self-heals with no config edit.
    RefusedThisSession,
}

/// What one call puts in its request body.
///
/// Exhaustive by construction: **no variant names two fields**, so "never both
/// shapes" (BR-4, AC-2) is a property of the type rather than of a test that has
/// to keep passing. Adapters `match` on this with no wildcard arm, so adding a
/// fourth reasoning shape later is a compile error until every adapter decides
/// what it emits — which is the desired failure mode for a wire-shape change.
///
/// Deliberately implements **no [`Default`]** (ADR-B): it is a required field of
/// `TurnRequest`, so a call path that has not thought about effort does not
/// compile. BR-1's "every call states its effort" is a compile error to violate
/// rather than a test to remember.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum ResolvedEffort {
    /// Send the effort field at this **already-clamped** level.
    ///
    /// Adapters do not clamp — the level arriving here was resolved once at
    /// route time (ADR-G). An adapter that re-clamped would be a second
    /// implementation of BR-5, which is the drift AC-8 exists to prevent.
    Effort {
        /// The clamped level to send.
        level: EffortLevel,
    },
    /// Send the thinking flag alone.
    ///
    /// Carries **no** level on purpose: this provider takes a boolean, and
    /// reporting a level the wire does not carry is exactly the
    /// silently-ignored-setting family BR-6 rules out.
    ThinkingFlag,
    /// Send neither field, and say why.
    Omit {
        /// The reason, for the event and the surface. Never sent on the wire.
        reason: EffortOmission,
    },
}

impl ResolvedEffort {
    /// Shorthand for [`ResolvedEffort::Effort`].
    #[must_use]
    pub const fn effort(level: EffortLevel) -> Self {
        Self::Effort { level }
    }

    /// Shorthand for [`ResolvedEffort::Omit`].
    #[must_use]
    pub const fn omit(reason: EffortOmission) -> Self {
        Self::Omit { reason }
    }

    /// The level actually being sent, or `None` when the wire carries no level.
    ///
    /// [`ResolvedEffort::ThinkingFlag`] yields `None` — it sends a boolean, and
    /// a caller that wants "what level is this provider on" must be told there
    /// isn't one rather than handed the pre-clamp request.
    #[must_use]
    pub const fn level(self) -> Option<EffortLevel> {
        match self {
            Self::Effort { level } => Some(level),
            Self::ThinkingFlag | Self::Omit { .. } => None,
        }
    }
}

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
    #[test]
    fn ladder_order_matches_declaration_order() {
        assert!(EffortLevel::Low < EffortLevel::Medium);
        assert!(EffortLevel::Medium < EffortLevel::High);
        assert!(EffortLevel::High < EffortLevel::Xhigh);
        assert!(EffortLevel::Xhigh < EffortLevel::Max);
        // ALL_LEVELS is the single source of order; assert it agrees.
        let mut sorted = ALL_LEVELS;
        sorted.sort_unstable();
        assert_eq!(sorted, ALL_LEVELS, "ALL_LEVELS must be ascending");
        for (i, level) in ALL_LEVELS.iter().enumerate() {
            assert_eq!(level.rung() as usize, i);
        }
    }

    #[test]
    fn default_level_is_high() {
        // BR-1: the absence of a user setting resolves to the declared default,
        // never to an absent field.
        assert_eq!(EffortLevel::default(), EffortLevel::High);
    }

    #[test]
    fn levels_round_trip_through_their_spelling() {
        for level in ALL_LEVELS {
            assert_eq!(level.as_str().parse::<EffortLevel>().unwrap(), level);
            assert_eq!(level.to_string(), level.as_str());
        }
        assert!("MAX".parse::<EffortLevel>().is_err());
        assert!("minimal".parse::<EffortLevel>().is_err(), "no minimal rung");
        // The error names every accepted spelling.
        let err = "nope".parse::<EffortLevel>().unwrap_err().to_string();
        for level in ALL_LEVELS {
            assert!(err.contains(level.as_str()), "error must list {level}");
        }
    }

    #[test]
    fn ladder_membership_and_emptiness() {
        let l = EffortLadder::from_levels(&[EffortLevel::Low, EffortLevel::Max]);
        assert!(l.contains(EffortLevel::Low));
        assert!(l.contains(EffortLevel::Max));
        assert!(!l.contains(EffortLevel::High));
        assert!(!l.is_empty());
        assert!(EffortLadder::EMPTY.is_empty());
        assert_eq!(EffortLadder::default(), EffortLadder::EMPTY);
        assert_eq!(EffortLadder::all().levels().count(), ALL_LEVELS.len());
    }

    #[test]
    fn ladder_normalizes_order_and_duplicates() {
        let messy = EffortLadder::from_levels(&[
            EffortLevel::Max,
            EffortLevel::Low,
            EffortLevel::Max,
            EffortLevel::High,
            EffortLevel::Low,
        ]);
        assert_eq!(
            messy.levels().collect::<Vec<_>>(),
            vec![EffortLevel::Low, EffortLevel::High, EffortLevel::Max],
        );
    }

    /// A hand-written serde pair is where silent drift lives (ADR-C), so both
    /// directions are pinned against a literal.
    #[test]
    fn ladder_serde_round_trips_and_matches_a_literal() {
        let ladder =
            EffortLadder::from_levels(&[EffortLevel::Max, EffortLevel::Low, EffortLevel::High]);
        let json = serde_json::to_string(&ladder).unwrap();
        assert_eq!(json, r#"["low","high","max"]"#, "ascending, de-duplicated");
        assert_eq!(
            serde_json::from_str::<EffortLadder>(r#"["max","low","high"]"#).unwrap(),
            ladder,
            "input order is irrelevant",
        );
        // Round-trip through the public constructor.
        assert_eq!(
            EffortLadder::from_levels(&ladder.levels().collect::<Vec<_>>()),
            ladder,
        );
        assert_eq!(serde_json::to_string(&EffortLadder::EMPTY).unwrap(), "[]");
        assert_eq!(
            serde_json::from_str::<EffortLadder>("[]").unwrap(),
            EffortLadder::EMPTY,
        );
    }

    // -- AC-3: the clamp table ----------------------------------------------

    /// AC-3 / BR-5. Table-driven across all five canonical levels × four
    /// ladders, including the spec's three named cases.
    ///
    /// The ladders are deliberately **narrow and irregular**. A table whose
    /// ladders were all close to the full canonical set would pass under an
    /// identity clamp, which would make AC-12's second mutation undetectable.
    #[test]
    fn clamp_table() {
        let three = EffortLadder::from_levels(&[EffortLevel::Low, EffortLevel::High, EffortLevel::Max]);
        let floor_high = EffortLadder::from_levels(&[EffortLevel::High, EffortLevel::Max]);
        let only_medium = EffortLadder::from_levels(&[EffortLevel::Medium]);
        let full = EffortLadder::all();

        let cases: &[(&str, EffortLadder, EffortLevel, Option<EffortLevel>)] = &[
            // The three cases AC-3 names explicitly.
            ("AC-3: xhigh into low/high/max", three, EffortLevel::Xhigh, Some(EffortLevel::High)),
            ("AC-3: medium into low/high/max", three, EffortLevel::Medium, Some(EffortLevel::Low)),
            ("AC-3: low into a high-floor ladder", floor_high, EffortLevel::Low, Some(EffortLevel::High)),
            // three-rung ladder, remaining levels
            ("low into low/high/max", three, EffortLevel::Low, Some(EffortLevel::Low)),
            ("high into low/high/max", three, EffortLevel::High, Some(EffortLevel::High)),
            ("max into low/high/max", three, EffortLevel::Max, Some(EffortLevel::Max)),
            // high-floor ladder: everything below `high` rounds up to it
            ("medium into high/max", floor_high, EffortLevel::Medium, Some(EffortLevel::High)),
            ("high into high/max", floor_high, EffortLevel::High, Some(EffortLevel::High)),
            ("xhigh into high/max", floor_high, EffortLevel::Xhigh, Some(EffortLevel::High)),
            ("max into high/max", floor_high, EffortLevel::Max, Some(EffortLevel::Max)),
            // single-rung ladder: every request lands on it, from both directions
            ("low into {medium}", only_medium, EffortLevel::Low, Some(EffortLevel::Medium)),
            ("medium into {medium}", only_medium, EffortLevel::Medium, Some(EffortLevel::Medium)),
            ("high into {medium}", only_medium, EffortLevel::High, Some(EffortLevel::Medium)),
            ("xhigh into {medium}", only_medium, EffortLevel::Xhigh, Some(EffortLevel::Medium)),
            ("max into {medium}", only_medium, EffortLevel::Max, Some(EffortLevel::Medium)),
            // full ladder: identity, the one case an identity clamp gets right
            ("low into the full ladder", full, EffortLevel::Low, Some(EffortLevel::Low)),
            ("max into the full ladder", full, EffortLevel::Max, Some(EffortLevel::Max)),
            // empty ladder: nothing to send
            ("anything into the empty ladder", EffortLadder::EMPTY, EffortLevel::High, None),
        ];

        for (name, ladder, requested, expected) in cases {
            assert_eq!(
                ladder.clamp(*requested),
                *expected,
                "{name}: {requested} into {:?}",
                ladder.levels().collect::<Vec<_>>(),
            );
        }
    }

    /// BR-5's direction, stated as a property rather than as rows: the clamp
    /// never rounds up while a supported rung at-or-below exists. A clamp that
    /// preferred the nearest rung in either direction would pass the named
    /// AC-3 cases by luck on some tables; this catches it everywhere.
    #[test]
    fn clamp_prefers_down_whenever_anything_below_is_supported() {
        for bits in 1u8..32 {
            let ladder = EffortLadder::from_levels(
                &ALL_LEVELS
                    .into_iter()
                    .filter(|l| bits & (1 << l.rung()) != 0)
                    .collect::<Vec<_>>(),
            );
            for requested in ALL_LEVELS {
                let got = ladder.clamp(requested).expect("non-empty ladder");
                let anything_below = ladder.levels().any(|l| l <= requested);
                if anything_below {
                    assert!(got <= requested, "{got} must not exceed {requested}");
                } else {
                    assert!(got > requested, "nothing below, so must round up");
                }
                assert!(ladder.contains(got), "clamp must land inside the ladder");
            }
        }
    }

    // -- ADR-E: per-kind defaults -------------------------------------------

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

        assert_eq!(default_ladder_for(ProviderKind::Anthropic), EffortLadder::all());
        let conservative = EffortLadder::from_levels(&[EffortLevel::Low, EffortLevel::High]);
        assert_eq!(default_ladder_for(ProviderKind::OpenaiCompatible), conservative);
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
            assert!(ladder.contains(EffortLevel::Low), "{kind:?} must accept low");
            assert!(ladder.contains(EffortLevel::High), "{kind:?} must accept high");
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
            resolve_effort(EffortLevel::Max, ProviderKind::Local, &caps(None, None), false),
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
            resolve_effort(EffortLevel::Xhigh, ProviderKind::OpenaiCompatible, &deepseek, false),
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
    #[test]
    fn only_the_effort_variant_carries_a_level() {
        assert_eq!(
            ResolvedEffort::effort(EffortLevel::Low).level(),
            Some(EffortLevel::Low),
        );
        assert_eq!(ResolvedEffort::ThinkingFlag.level(), None);
        assert_eq!(
            ResolvedEffort::omit(EffortOmission::ShapeNone).level(),
            None,
        );
    }

    #[test]
    fn resolved_effort_round_trips_through_serde() {
        for value in [
            ResolvedEffort::effort(EffortLevel::Xhigh),
            ResolvedEffort::ThinkingFlag,
            ResolvedEffort::omit(EffortOmission::RefusedThisSession),
        ] {
            let json = serde_json::to_string(&value).unwrap();
            assert_eq!(serde_json::from_str::<ResolvedEffort>(&json).unwrap(), value);
        }
    }

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
