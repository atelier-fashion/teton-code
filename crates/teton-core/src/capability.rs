//! The one web-capability classifier (REQ-572 BR-3).
//!
//! Four surfaces need the same answer to "can this machine look things up on
//! the web right now, and if not, why not?": the system prompt's refusal
//! clause, the status surface, the `/web setup` flow, and the decision to
//! register the web tool at all. Before this module each of them re-derived it
//! from [`WebConfig`] separately, which is the shape LESSON-456 warns about —
//! a second classifier over the same inputs disagrees with the first
//! eventually, and the disagreement surfaces as a refusal that names a state
//! the machine is not in.
//!
//! So the derivation lives here, once, as a pure function over the config table
//! plus one runtime fact (is there a local model?). It is deliberately
//! feature-free and I/O-free — the whole crate is (see the crate docs) — so the
//! daemon and its tests can table-test every cell without a socket, a
//! filesystem, or a loaded model.
//!
//! # What this type is not
//!
//! It is not a *grant*. [`WebTier`] is the configured ceiling and consent is
//! separate session state (REQ-563 BR-3/BR-4), so [`WebCapabilityState::Ready`]
//! says "the tool exists and the ceiling permits this tier", never "the next
//! lookup will proceed without asking".
//!
//! It is also not the wire type. The twin that travels in `web/setup_plan` and
//! `ConfigSnapshot` lives in `teton-protocol` (clients must not depend on this
//! crate), and the conversion happens at the daemon's boundary — the same
//! arrangement [`WebTier`] and its `teton_protocol::events::WebTier` twin
//! already have.

use crate::config::{WebConfig, WebTier};

/// What the machine's web-lookup capability is, derived from the config table
/// and the presence of a local model.
///
/// # Why there is no "partially configured" variant
///
/// The obvious missing cell is `tier = "search"` with no `search_endpoint`, and
/// it is missing on purpose: [`crate::Config::validate`] refuses that document
/// at load with [`crate::ConfigError::WebSearchTierWithoutEndpoint`], so a
/// running daemon can never hold one. A variant for it would be a state this
/// enum could describe but never observe, and every match site would carry an
/// arm that no test could reach honestly. The partially-configured *experience*
/// is real, but it belongs to the setup flow, where a candidate config is
/// validated before it is ever written — which is a preview-time question, not
/// a runtime one.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WebCapabilityState {
    /// The capability is configured and its ceiling is serviceable; carries the
    /// ceiling. The web tool registers (see
    /// [`WebCapabilityState::exposes_web_tool`]) and lookups up to this tier
    /// proceed through the ordinary consent gate.
    Ready(WebTier),
    /// [`WebTier::Off`] — including the fresh-install case where the config
    /// names no `[web]` table at all, which deserializes to the same default
    /// (REQ-563 BR-1).
    ///
    /// The name is the point: the capability is **available and switched off**,
    /// not absent. This is the state behind the observed failure this REQ
    /// exists for — the model, told only that it has no web tool, hunts the
    /// repository for knowledge that cannot be there instead of naming the
    /// opt-in.
    OffAvailable,
    /// The ceiling names `search`, but the search leg cannot serve, for the
    /// named reason.
    ///
    /// Note what is still true here, and what the refusal text must therefore
    /// say: **the fetch tiers inside the ceiling stay serviceable**. REQ-563
    /// BR-14 hard-couples search to the redact scan, and the scan runs on the
    /// local model, so a machine with no local tier gets no search — per query,
    /// with a stated notice, rather than by having the capability withdrawn.
    /// The tool still registers, `fetch_user_url` and `fetch_any_url` lookups
    /// still work, and only the search leg is blocked. Rendering this state as
    /// "web lookup is off" would be a lie in the direction that costs the user
    /// a capability they have.
    SearchUnavailable {
        /// The named missing piece — never "something is wrong".
        reason: SearchGap,
    },
}

impl WebCapabilityState {
    /// Whether the web tool is registered for a turn in this state — the single
    /// predicate `register_web_tool` consumes (BR-3).
    ///
    /// Exposure is `tier > Off`, which is *not* the same question as
    /// [`Self::Ready`]: [`Self::SearchUnavailable`] is a configured, exposed
    /// capability whose search leg blocks per query, so a registration
    /// predicate written as "is it Ready" would silently withdraw the fetch
    /// tiers from every machine without a local model. Keeping the predicate a
    /// method here rather than a `matches!` at each call site is what makes
    /// "one classifier" literal — the prompt clause, the status surface and the
    /// registry all ask this function, and a future variant forces every one of
    /// them to be revisited through one edit.
    #[must_use]
    pub const fn exposes_web_tool(self) -> bool {
        !matches!(self, Self::OffAvailable)
    }
}

/// Why a structurally permitted search leg cannot serve.
///
/// Deliberately **closed** (no `#[non_exhaustive]`) and deliberately an enum
/// rather than a string: one variant today, and the next gap a REQ discovers —
/// an unreachable endpoint, a withdrawn key — should not compile until every
/// site that renders a gap has been told about it. A `String` here would let a
/// new gap ship with a message no surface knows how to phrase.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SearchGap {
    /// No local model, so the redact scan REQ-563 BR-14 hard-couples to search
    /// cannot run. A guard that cannot run is a block, not a skip (LESSON-492),
    /// so the search tier is withheld rather than served unscanned.
    NoLocalModel,
}

impl SearchGap {
    /// The gap as the sentence a surface shows, so the daemon, the status line
    /// and the setup flow do not each invent their own phrasing of the same
    /// fact. It names the missing piece, per the spec's "the missing piece,
    /// named" rule — never a bare "unavailable".
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::NoLocalModel => "search needs the local model",
        }
    }
}

/// Derive the web capability state from the `[web]` table and whether a local
/// model is present.
///
/// `local_model_present` is the caller's fact, not this crate's: whether a
/// local tier exists is an I/O question (weights on disk, a model loaded), and
/// this crate performs no I/O. Passing it in is what keeps every cell of the
/// matrix testable — including the ones a developer machine cannot produce.
#[must_use]
pub fn web_capability_state(web: &WebConfig, local_model_present: bool) -> WebCapabilityState {
    match web.tier {
        // The only disabled state (REQ-563 D-9: no separate `enabled` bool to
        // disagree with the tier), and the state an absent `[web]` table
        // deserializes to.
        WebTier::Off => WebCapabilityState::OffAvailable,
        // BR-14: search is hard-coupled to the redact scan, which runs on the
        // local model. The ceiling is honoured for everything below search.
        WebTier::Search if !local_model_present => WebCapabilityState::SearchUnavailable {
            reason: SearchGap::NoLocalModel,
        },
        tier => WebCapabilityState::Ready(tier),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Config;

    /// A `[web]` table at `tier`, with whatever else that tier needs to be a
    /// document the validator would accept — so no cell of the matrix below is
    /// secretly testing a config the daemon could never hold.
    fn web_at(tier: WebTier) -> WebConfig {
        WebConfig {
            tier,
            search_endpoint: (tier == WebTier::Search)
                .then(|| "https://search.example.com/search".to_owned()),
            ..WebConfig::default()
        }
    }

    #[test]
    fn every_tier_crossed_with_local_model_presence_has_one_stated_answer() {
        // The full matrix, written out rather than re-derived: a table that
        // recomputed the expectation with the same `match` would pass for a
        // classifier that is wrong in exactly the same way.
        let cells = [
            (WebTier::Off, false, WebCapabilityState::OffAvailable),
            (WebTier::Off, true, WebCapabilityState::OffAvailable),
            (
                WebTier::FetchUserUrl,
                false,
                WebCapabilityState::Ready(WebTier::FetchUserUrl),
            ),
            (
                WebTier::FetchUserUrl,
                true,
                WebCapabilityState::Ready(WebTier::FetchUserUrl),
            ),
            (
                WebTier::FetchAnyUrl,
                false,
                WebCapabilityState::Ready(WebTier::FetchAnyUrl),
            ),
            (
                WebTier::FetchAnyUrl,
                true,
                WebCapabilityState::Ready(WebTier::FetchAnyUrl),
            ),
            (
                WebTier::Search,
                false,
                WebCapabilityState::SearchUnavailable {
                    reason: SearchGap::NoLocalModel,
                },
            ),
            (
                WebTier::Search,
                true,
                WebCapabilityState::Ready(WebTier::Search),
            ),
        ];

        for (tier, local_model_present, expected) in cells {
            assert_eq!(
                web_capability_state(&web_at(tier), local_model_present),
                expected,
                "tier {tier:?} with local_model_present={local_model_present}"
            );
        }

        // The sweep guard: a tier added to the ladder without a row here would
        // otherwise leave its two cells unclassified and untested. `WebTier::ALL`
        // exists for exactly this (see its docs), so the matrix is checked
        // against it rather than against a hand-counted number.
        for tier in WebTier::ALL {
            let rows = cells.iter().filter(|(cell, ..)| *cell == tier).count();
            assert_eq!(
                rows, 2,
                "tier {tier:?} is missing a cell — every tier needs both local-model answers"
            );
        }
    }

    #[test]
    fn a_config_with_no_web_table_is_off_but_available() {
        // Pinned through the loader rather than through `WebConfig::default()`,
        // because the claim being made is about a *document* that never
        // mentions `[web]` — the fresh-install case the OffAvailable clause is
        // written for.
        let cfg = Config::load("").expect("an empty config must load");
        assert!(cfg.web.is_unset());
        for local_model_present in [false, true] {
            assert_eq!(
                web_capability_state(&cfg.web, local_model_present),
                WebCapabilityState::OffAvailable,
                "a document with no [web] table is the off-but-available state"
            );
        }
    }

    #[test]
    fn a_fetch_tier_is_ready_whether_or_not_a_local_model_is_present() {
        // REQ-563 BR-14 couples *search* to the redact scan and says in as many
        // words that fetch tiers are unaffected. If this test ever needs the
        // local-model flag to pass, the coupling has leaked one tier down.
        for tier in [WebTier::FetchUserUrl, WebTier::FetchAnyUrl] {
            for local_model_present in [false, true] {
                assert_eq!(
                    web_capability_state(&web_at(tier), local_model_present),
                    WebCapabilityState::Ready(tier),
                    "tier {tier:?} must not depend on the local model"
                );
            }
        }
    }

    #[test]
    fn search_without_a_local_model_is_blocked_per_query_and_still_exposed() {
        let state = web_capability_state(&web_at(WebTier::Search), false);
        assert_eq!(
            state,
            WebCapabilityState::SearchUnavailable {
                reason: SearchGap::NoLocalModel
            }
        );
        // The distinction this REQ turns on: the capability is configured and
        // the tool is still registered, so the fetch tiers inside the ceiling
        // keep working and only the search leg blocks. Collapsing this into
        // OffAvailable would take a capability away from every machine without
        // a local model.
        assert!(
            state.exposes_web_tool(),
            "a configured search ceiling must not withdraw the fetch tiers"
        );
        assert_ne!(state, WebCapabilityState::OffAvailable);
        assert_eq!(
            SearchGap::NoLocalModel.as_str(),
            "search needs the local model"
        );
    }

    #[test]
    fn tool_exposure_is_exactly_a_tier_above_off_in_every_cell() {
        // AC-3: this is `register_web_tool`'s registration condition —
        // `config.tier == WebTier::Off` → not registered (REQ-563 BR-1/D-1) —
        // stated once, here, so the daemon consumes it instead of re-deriving
        // it (TASK-131 does that consumption).
        //
        // Note the predicate is exposure, not `Ready`. `Ready ⟺ tier > Off`
        // holds in every cell except `search` with no local model, which is a
        // configured, exposed capability with one blocked leg; writing the
        // registration test against `Ready` would pin a daemon that
        // un-registers the tool on exactly those machines.
        for tier in WebTier::ALL {
            for local_model_present in [false, true] {
                let state = web_capability_state(&web_at(tier), local_model_present);
                assert_eq!(
                    state.exposes_web_tool(),
                    tier > WebTier::Off,
                    "exposure disagreed with the registration predicate at {tier:?} \
                     (local_model_present={local_model_present})"
                );
            }
        }
    }

    #[test]
    fn the_partially_configured_cell_this_enum_omits_cannot_be_loaded() {
        // The absent variant, pinned by the reason it is absent: a search tier
        // with no endpoint is refused at load, so no running daemon holds a
        // `WebConfig` this classifier would have to describe. If this document
        // ever starts loading, `WebCapabilityState` owes it a variant.
        let err = Config::load("[web]\ntier = \"search\"\n")
            .expect_err("tier = search with no endpoint must be refused at load");
        assert!(
            err.to_string().contains("search_endpoint"),
            "the refusal must name the missing key: {err}"
        );
    }
}
