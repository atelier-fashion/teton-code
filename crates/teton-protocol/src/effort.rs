//! Reasoning-effort vocabulary (REQ-559).
//!
//! The canonical ladder, the per-provider ladder bitset, the declared request
//! shape, and the closed enum describing **what one call puts on the wire**.
//!
//! ## Why the vocabulary lives in the protocol crate
//!
//! BR-3 says the canonical ladder is "the only vocabulary the router, the
//! config, the CLI, and the events speak". Four consumers, and they do not share
//! a crate: `teton-core` holds the router and the config, `teton-protocol` holds
//! the events, and the CLI depends on the protocol but not on core. The
//! architecture forbids `teton-protocol` from depending back on `teton-core`, so
//! defining these types in core would have forced a wire twin plus a converter
//! — two definitions of one vocabulary, which is precisely the drift BR-3 rules
//! out.
//!
//! So the **vocabulary** (these types, and the clamp, which is pure over them)
//! lives here in the shared leaf, and the **policy** that needs
//! `ProviderKind` — the per-kind defaults and `resolve_effort` — lives in
//! `teton_core::effort`, which re-exports these so `teton_core::EffortLevel`
//! remains the stable path for daemon-side code. One definition, one clamp, no
//! converter.

use serde::{Deserialize, Deserializer, Serialize, Serializer};

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
    pub(crate) const fn rung(self) -> u8 {
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
#[error(
    "unknown effort level `{got}` — expected one of: {}",
    crate::effort::level_list()
)]
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
        /// The clamped level to send — what actually goes on the wire.
        level: EffortLevel,
        /// The level the user asked for, before the per-provider clamp.
        ///
        /// Carried alongside rather than compared against a global elsewhere,
        /// so "was this clamped?" is answerable from the value itself. Two
        /// consumers need it and neither should have to reach for the setting:
        /// the surface, which says "clamped from X" (BR-9), and BR-12's typed
        /// error, which must name the requested level at a layer that never
        /// saw it. Equal to `level` whenever no clamping happened.
        requested: EffortLevel,
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
    /// An unclamped resolution: the level asked for is the level sent.
    #[must_use]
    pub const fn effort(level: EffortLevel) -> Self {
        Self::Effort {
            level,
            requested: level,
        }
    }

    /// A clamped resolution: `requested` was asked for, `level` is being sent.
    #[must_use]
    pub const fn clamped(requested: EffortLevel, level: EffortLevel) -> Self {
        Self::Effort { level, requested }
    }

    /// Whether the per-provider ladder moved the level the user asked for
    /// (REQ-559 BR-5). The surface says so; a silent clamp would leave a user
    /// who set `xhigh` with no explanation of why nothing changed.
    #[must_use]
    pub const fn was_clamped(self) -> bool {
        matches!(self, Self::Effort { level, requested } if !matches!(
            (level, requested),
            (EffortLevel::Low, EffortLevel::Low)
                | (EffortLevel::Medium, EffortLevel::Medium)
                | (EffortLevel::High, EffortLevel::High)
                | (EffortLevel::Xhigh, EffortLevel::Xhigh)
                | (EffortLevel::Max, EffortLevel::Max)
        ))
    }

    /// The level the user asked for, when this resolution sends one.
    #[must_use]
    pub const fn requested(self) -> Option<EffortLevel> {
        match self {
            Self::Effort { requested, .. } => Some(requested),
            Self::ThinkingFlag | Self::Omit { .. } => None,
        }
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
            Self::Effort { level, .. } => Some(level),
            Self::ThinkingFlag | Self::Omit { .. } => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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

    #[test]
    fn clamp_table() {
        let three =
            EffortLadder::from_levels(&[EffortLevel::Low, EffortLevel::High, EffortLevel::Max]);
        let floor_high = EffortLadder::from_levels(&[EffortLevel::High, EffortLevel::Max]);
        let only_medium = EffortLadder::from_levels(&[EffortLevel::Medium]);
        let full = EffortLadder::all();

        let cases: &[(&str, EffortLadder, EffortLevel, Option<EffortLevel>)] = &[
            // The three cases AC-3 names explicitly.
            (
                "AC-3: xhigh into low/high/max",
                three,
                EffortLevel::Xhigh,
                Some(EffortLevel::High),
            ),
            (
                "AC-3: medium into low/high/max",
                three,
                EffortLevel::Medium,
                Some(EffortLevel::Low),
            ),
            (
                "AC-3: low into a high-floor ladder",
                floor_high,
                EffortLevel::Low,
                Some(EffortLevel::High),
            ),
            // three-rung ladder, remaining levels
            (
                "low into low/high/max",
                three,
                EffortLevel::Low,
                Some(EffortLevel::Low),
            ),
            (
                "high into low/high/max",
                three,
                EffortLevel::High,
                Some(EffortLevel::High),
            ),
            (
                "max into low/high/max",
                three,
                EffortLevel::Max,
                Some(EffortLevel::Max),
            ),
            // high-floor ladder: everything below `high` rounds up to it
            (
                "medium into high/max",
                floor_high,
                EffortLevel::Medium,
                Some(EffortLevel::High),
            ),
            (
                "high into high/max",
                floor_high,
                EffortLevel::High,
                Some(EffortLevel::High),
            ),
            (
                "xhigh into high/max",
                floor_high,
                EffortLevel::Xhigh,
                Some(EffortLevel::High),
            ),
            (
                "max into high/max",
                floor_high,
                EffortLevel::Max,
                Some(EffortLevel::Max),
            ),
            // single-rung ladder: every request lands on it, from both directions
            (
                "low into {medium}",
                only_medium,
                EffortLevel::Low,
                Some(EffortLevel::Medium),
            ),
            (
                "medium into {medium}",
                only_medium,
                EffortLevel::Medium,
                Some(EffortLevel::Medium),
            ),
            (
                "high into {medium}",
                only_medium,
                EffortLevel::High,
                Some(EffortLevel::Medium),
            ),
            (
                "xhigh into {medium}",
                only_medium,
                EffortLevel::Xhigh,
                Some(EffortLevel::Medium),
            ),
            (
                "max into {medium}",
                only_medium,
                EffortLevel::Max,
                Some(EffortLevel::Medium),
            ),
            // full ladder: identity, the one case an identity clamp gets right
            (
                "low into the full ladder",
                full,
                EffortLevel::Low,
                Some(EffortLevel::Low),
            ),
            (
                "max into the full ladder",
                full,
                EffortLevel::Max,
                Some(EffortLevel::Max),
            ),
            // empty ladder: nothing to send
            (
                "anything into the empty ladder",
                EffortLadder::EMPTY,
                EffortLevel::High,
                None,
            ),
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
            assert_eq!(
                serde_json::from_str::<ResolvedEffort>(&json).unwrap(),
                value
            );
        }
    }
}
