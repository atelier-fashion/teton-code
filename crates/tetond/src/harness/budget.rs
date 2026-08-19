//! Route-budget **policy**: the one derivation that turns a route's window
//! facts into the context budget the harness runs under (REQ-586 BR-8, AC-12).
//!
//! This module is pure — no router, no clock, no I/O — on the
//! `teton_core::effort::resolve_effort` precedent: the router is the one
//! caller for routes (`Router::budget_for`, TASK-186), and every surface that
//! prints a budget or its bound reads the [`RouteBudget`] the router stamped
//! rather than re-deriving it (LESSON-456: one classifier per fact).
//!
//! ## The derivation (BR-1/BR-2/BR-4/BR-5)
//!
//! ```text
//! derive(inputs):
//!   if is_local                → default pair, bound = LocalEngine
//!   elif window == 0           → default pair, bound = DefaultUnknown
//!   else:
//!     window_eff = cap if 0 < cap < window (bound = UserCap) else window (Window)
//!     usable     = window_eff − reservation   (saturating; 0 → default pair,
//!                                              DefaultUnknown)
//!     tokens     = usable × 2 / 3             (÷ REMOTE_TOKENS_PER_WORD, 3/2)
//!     bytes      = usable × 2                 (× DUTY_REQUEST_BYTES_PER_TOKEN)
//!     if redact_scan and bytes > REDACT_SCANNABLE_CONTEXT_BYTES:
//!         bytes = REDACT_SCANNABLE_CONTEXT_BYTES; bound = RedactScan
//!         (applies LAST; the word component stays window-derived — BR-4)
//!   digest thresholds = today's fraction of the pair, capped by the
//!                       absolute ceiling
//! ```
//!
//! Precedence, stated once and tested pairwise: `LocalEngine` >
//! `DefaultUnknown` > (`RedactScan` when it bites) > `UserCap` > `Window`.
//! The cap is a **window ceiling** — the pair is recomputed from
//! `window_eff = min(window, cap)`, not clamped after the fact — and the
//! redact clamp is applied last so it names the bound whenever it is the
//! thing that actually bit.
//!
//! ## Two currencies, deliberately
//!
//! The budget is a `(whitespace-words, bytes)` pair because no single ratio
//! covers what the harness carries (AC-3, ADR-10): prose runs ≈1.2 tokens per
//! word (covered by the 3/2 word ratio alone), while minified JSON and
//! path-heavy shell output run 20–45 tokens per "word" and are covered only
//! by the 2 B/token byte floor. `max(words × 3/2, bytes / 2) ≥ tokens` holds
//! for every corpus class except random base64 (≈1.45 B/token — the
//! documented gap, backstopped by the digest threshold and the typed
//! `context_length_exceeded` outcome).

use teton_protocol::events::BudgetBound;

use super::context::APPROX_BYTES_PER_TOKEN;
use super::duty::DUTY_REQUEST_BYTES_PER_TOKEN;
use crate::egress::redact::REDACT_SCANNABLE_CONTEXT_BYTES;

/// The default context budget in whitespace-approximated tokens — **the one
/// home** of the local pair's word half (LESSON-456).
///
/// This is the weak-model native budget every route without a better fact
/// runs under: the local tier (whose real window is the engine's `n_ctx`, not
/// the provider's declaration), a remote provider with `max_context = 0`
/// (BR-3: defaulted, and stated), and the unresolvable-route
/// `HarnessConfig::default()`. 4,096 words × [`APPROX_BYTES_PER_TOKEN`] fits
/// the local engine's 16,384-token window with headroom. Pinned by AC-1
/// ("`max_context = 0` yields today's `(4096, 32768)`") and the existing
/// margin tests; `HarnessConfig::default()` reads it from here — the literal
/// has no second home (TASK-192's one-home grep).
pub const LOCAL_BUDGET_TOKENS: usize = 4_096;

/// The default context budget's byte half: [`LOCAL_BUDGET_TOKENS`] bridged at
/// [`APPROX_BYTES_PER_TOKEN`] (8 B per whitespace word) = 32,768 bytes.
///
/// The word bridge, not the BPE floor: on the *local* pair bytes are derived
/// from words (a whitespace word of code averages ~7–8 bytes), which is the
/// pre-REQ-586 rule unchanged (OQ-3). Remote pairs derive bytes from the
/// window with [`DUTY_REQUEST_BYTES_PER_TOKEN`] instead — see
/// [`derive`]. Pinned by AC-1 and the redact margin tests, which measure the
/// default (local) shape (AC-13).
pub const LOCAL_BUDGET_BYTES: usize = LOCAL_BUDGET_TOKENS * APPROX_BYTES_PER_TOKEN;

/// The default `digest` threshold in whitespace words — **the one home** of
/// the 1,500 literal (LESSON-456).
///
/// A tool result above this is condensed through the `digest` duty before it
/// enters context. As a fraction of [`LOCAL_BUDGET_TOKENS`] this is ≈36.6%,
/// and that *fraction* is what scales to other routes (BR-6): the threshold on
/// any route is `budget × LOCAL_DIGEST_THRESHOLD_TOKENS / LOCAL_BUDGET_TOKENS`,
/// so the default route stays exactly 1,500 (AC-9: "byte-identical to
/// today"), pinned by the digest-threshold tests here and in `context.rs`.
pub const LOCAL_DIGEST_THRESHOLD_TOKENS: usize = 1_500;

/// The default `digest` threshold's byte twin: [`LOCAL_DIGEST_THRESHOLD_TOKENS`]
/// × [`APPROX_BYTES_PER_TOKEN`] = 12,000 bytes — the same ≈36.6% of
/// [`LOCAL_BUDGET_BYTES`].
///
/// A byte twin exists because the word threshold alone would let a dense
/// (minified JSON, base64) result slide into context raw at the edge of the
/// byte budget (BR-6, gotcha #3: the twin used to be recomputed as
/// `threshold_tokens × 8` at the call site — after REQ-586 it travels
/// explicitly so remote routes can scale it from `budget_bytes`, never from
/// words). AC-9 pins the default route at exactly 12,000.
pub const LOCAL_DIGEST_THRESHOLD_BYTES: usize =
    LOCAL_DIGEST_THRESHOLD_TOKENS * APPROX_BYTES_PER_TOKEN;

/// Safety ratio between whitespace words and real BPE tokens, numerator: a
/// word budget of N claims at most `N × 3/2` provider tokens.
///
/// Measured (ADR-10, tiktoken 0.14.0 `o200k_base`): prose runs 1.21
/// tokens/word and Rust 1.69 — 3/2 covers prose on its own (the byte guard
/// covers everything denser), and AC-3's corpus test pins it from below
/// (mutation 3/2 → 1/1 fails on prose). Integer num/den because the budget
/// arithmetic is integer-only. `tests/token_corpus.rs` restates the pair as
/// literals until TASK-192 swaps it to read these constants.
pub const REMOTE_TOKENS_PER_WORD_NUM: usize = 3;

/// Denominator of the words→tokens safety ratio. See
/// [`REMOTE_TOKENS_PER_WORD_NUM`].
pub const REMOTE_TOKENS_PER_WORD_DEN: usize = 2;

/// Absolute ceiling on the scaled `digest` threshold, word half: 20,000
/// whitespace words (OQ-7).
///
/// Any single tool result above this is digested on **every** route, however
/// large the window: ≈ the largest single file a code task legitimately reads
/// whole; above it a raw fold displaces more conversation than it informs. A
/// placeholder until the corpus says otherwise. On a 200k window the words
/// fraction (≈48.6k) exceeds this and the ceiling binds — pinned by the
/// digest-threshold table test here (task AC: "the ceiling binds on 200k for
/// words").
pub const DIGEST_ABSOLUTE_CEILING_TOKENS: usize = 20_000;

/// Absolute ceiling on the scaled `digest` threshold, byte half: 160 KiB
/// (OQ-7).
///
/// A 160 KiB source file is ≈4k lines — the byte spelling of the same "largest
/// file worth folding raw" judgement as [`DIGEST_ABSOLUTE_CEILING_TOKENS`].
/// On a 200k window the bytes fraction is 145,734 < 163,840, so this ceiling
/// does **not** bind there (pinned both ways by the table test here) — the
/// words ceiling is the one that bites first on today's windows.
pub const DIGEST_ABSOLUTE_CEILING_BYTES: usize = 160 * 1024;

/// The elision marker's name for the local pair's window.
///
/// The pre-REQ-586 hard-coded string (gotcha #4), now the derivation's to
/// hand out: BR-7 says the marker names the *route's* window, and this is the
/// route-is-local (and defensive no-provider) spelling.
const LOCAL_WINDOW_LABEL: &str = "the local context window";

/// The elision marker's name for a redact-scan-bounded window (BR-4).
const REDACT_WINDOW_LABEL: &str = "the redact-scannable window";

/// What the router knows about a route when it asks for its budget.
///
/// A plain data carrier so [`derive`] stays pure and table-testable: the
/// router reads these off `capability_of(id)`, its `redact_scan` flag, and
/// the routing table's local classification (gotcha #9: `is_local` comes from
/// `table.local_provider_id`, never from "capabilities == default").
#[derive(Debug, Clone, Copy)]
pub struct BudgetInputs<'a> {
    /// The provider's declared context window in provider tokens
    /// (`capabilities.max_context`); 0 = not declared (BR-3).
    pub window: u32,
    /// The user's `capabilities.context_budget_cap` in provider tokens; 0 =
    /// none. A **window ceiling**: it bounds `window`, and the pair is
    /// recomputed from the smaller value (BR-5).
    pub cap: u32,
    /// Provider tokens reserved for generation — the `max_tokens` the
    /// adapters send (ADR-1: `HarnessConfig::default().gen_params.max_tokens`).
    pub reservation: u32,
    /// Whether the route is the local tier (routing-table classification).
    pub is_local: bool,
    /// Whether `[privacy] redact = true` bounds this route's bytes (BR-4).
    pub redact_scan: bool,
    /// The provider id, for the window label — `None` for the local tier or
    /// an unresolvable route.
    pub provider_id: Option<&'a str>,
}

impl BudgetInputs<'_> {
    /// The local route's inputs: what `HarnessConfig::default()` derives its
    /// budget from, and the shape the router uses for the local tier.
    #[must_use]
    pub const fn local() -> Self {
        Self {
            window: 0,
            cap: 0,
            reservation: 0,
            is_local: true,
            redact_scan: false,
            provider_id: None,
        }
    }
}

/// The per-route budget fact: the pair, what bound it, the window's name for
/// the elision marker, and the scaled `digest` thresholds.
///
/// Derived once where the route is decided (BR-8) and carried on the `Route`,
/// the `HarnessConfig`, and `route_decided` — every surface reads this value,
/// none re-derives it (AC-12).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RouteBudget {
    /// Context budget in whitespace-approximated tokens (words).
    pub budget_tokens: usize,
    /// Context budget in bytes — the currency the redact scan and the BPE
    /// floor are denominated in.
    pub budget_bytes: usize,
    /// Which constraint bound the pair (BR-8; rides `route_decided`).
    pub bound: BudgetBound,
    /// What the in-prompt truncation/elision marker calls the window (BR-7):
    /// `"the local context window"`, `"<id>'s context window"`, or
    /// `"the redact-scannable window"` when [`BudgetBound::RedactScan`] binds.
    pub window_label: String,
    /// `digest` threshold in words: today's fraction of [`Self::budget_tokens`],
    /// capped at [`DIGEST_ABSOLUTE_CEILING_TOKENS`] (BR-6).
    pub digest_threshold_tokens: usize,
    /// `digest` threshold in bytes: today's fraction of [`Self::budget_bytes`],
    /// capped at [`DIGEST_ABSOLUTE_CEILING_BYTES`] (BR-6).
    pub digest_threshold_bytes: usize,
}

/// Turn a route's window facts into its budget — the one classifier (BR-8,
/// AC-12); see the module docs for the derivation and the precedence.
#[must_use]
pub fn derive(inputs: BudgetInputs<'_>) -> RouteBudget {
    if inputs.is_local {
        return default_pair(BudgetBound::LocalEngine, LOCAL_WINDOW_LABEL.to_owned());
    }
    let provider_label = || match inputs.provider_id {
        Some(id) => format!("{id}'s context window"),
        // Defensive: a remote route always has an id in practice (the window
        // came from `capability_of(id)`); an id-less caller gets the default
        // pair's historical name rather than a nameless marker.
        None => LOCAL_WINDOW_LABEL.to_owned(),
    };
    if inputs.window == 0 {
        return default_pair(BudgetBound::DefaultUnknown, provider_label());
    }

    // The cap is a window ceiling: the pair derives from the smaller of the
    // two, so a capped budget keeps both guards proportionate (BR-5).
    let (window_eff, mut bound) = if inputs.cap > 0 && inputs.cap < inputs.window {
        (inputs.cap, BudgetBound::UserCap)
    } else {
        (inputs.window, BudgetBound::Window)
    };
    let usable = window_eff.saturating_sub(inputs.reservation) as usize;
    if usable == 0 {
        // A reservation that swallows the window leaves nothing to derive
        // from; the default pair applies, and the fact is stated (BR-3).
        return default_pair(BudgetBound::DefaultUnknown, provider_label());
    }

    // Words: usable ÷ (3/2) — the safety ratio guarantees N words claim at
    // most `usable` provider tokens (AC-3). Bytes: the 2 B/token BPE floor
    // (AC-3; reused from duty.rs — gotcha #12: not a third number).
    let budget_tokens = usable * REMOTE_TOKENS_PER_WORD_DEN / REMOTE_TOKENS_PER_WORD_NUM;
    let mut budget_bytes = usable * DUTY_REQUEST_BYTES_PER_TOKEN;

    // The redact clamp applies LAST and names the bound only when it bites
    // (BR-4): the scan is byte-denominated, so only bytes clamp — the word
    // component stays window-derived and the byte guard binds.
    let mut window_label = provider_label();
    if inputs.redact_scan && budget_bytes > REDACT_SCANNABLE_CONTEXT_BYTES {
        budget_bytes = REDACT_SCANNABLE_CONTEXT_BYTES;
        bound = BudgetBound::RedactScan;
        window_label = REDACT_WINDOW_LABEL.to_owned();
    }

    let (digest_threshold_tokens, digest_threshold_bytes) =
        digest_thresholds(budget_tokens, budget_bytes);
    RouteBudget {
        budget_tokens,
        budget_bytes,
        bound,
        window_label,
        digest_threshold_tokens,
        digest_threshold_bytes,
    }
}

/// The default (local) pair with the given bound and label — the
/// `LocalEngine`/`DefaultUnknown` arms of [`derive`].
fn default_pair(bound: BudgetBound, window_label: String) -> RouteBudget {
    let (digest_threshold_tokens, digest_threshold_bytes) =
        digest_thresholds(LOCAL_BUDGET_TOKENS, LOCAL_BUDGET_BYTES);
    RouteBudget {
        budget_tokens: LOCAL_BUDGET_TOKENS,
        budget_bytes: LOCAL_BUDGET_BYTES,
        bound,
        window_label,
        digest_threshold_tokens,
        digest_threshold_bytes,
    }
}

/// Scale the `digest` thresholds to a route's pair: today's fraction
/// (`LOCAL_DIGEST_THRESHOLD_* / LOCAL_BUDGET_*`, ≈36.6%) of each currency,
/// capped by the absolute ceiling (BR-6, ADR-5).
///
/// On the default pair this is exactly `(1_500, 12_000)` — byte-identical to
/// today (AC-9) — because the fraction is written as the constants' own ratio
/// rather than a restated percentage.
fn digest_thresholds(budget_tokens: usize, budget_bytes: usize) -> (usize, usize) {
    let tokens = (budget_tokens * LOCAL_DIGEST_THRESHOLD_TOKENS / LOCAL_BUDGET_TOKENS)
        .min(DIGEST_ABSOLUTE_CEILING_TOKENS);
    let bytes = (budget_bytes * LOCAL_DIGEST_THRESHOLD_BYTES / LOCAL_BUDGET_BYTES)
        .min(DIGEST_ABSOLUTE_CEILING_BYTES);
    (tokens, bytes)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::harness::turn_loop::HarnessConfig;

    /// The reservation ADR-1 names: `HarnessConfig::default().gen_params.max_tokens`.
    const RESERVATION: u32 = 1_024;

    fn remote<'a>(window: u32, cap: u32, redact_scan: bool) -> BudgetInputs<'a> {
        BudgetInputs {
            window,
            cap,
            reservation: RESERVATION,
            is_local: false,
            redact_scan,
            provider_id: Some("kimi"),
        }
    }

    /// The derivation table (task AC rows): each row is
    /// `(name, inputs, expected_tokens, expected_bytes, expected_bound)`.
    /// All expectations are integer arithmetic done by hand in the comments.
    #[test]
    fn derivation_table() {
        let rows: &[(&str, BudgetInputs<'_>, usize, usize, BudgetBound)] = &[
            (
                "local route yields the default pair",
                BudgetInputs::local(),
                LOCAL_BUDGET_TOKENS,
                LOCAL_BUDGET_BYTES,
                BudgetBound::LocalEngine,
            ),
            (
                "window 0 defaults, stated",
                remote(0, 0, false),
                LOCAL_BUDGET_TOKENS,
                LOCAL_BUDGET_BYTES,
                BudgetBound::DefaultUnknown,
            ),
            (
                // usable = 128,000 − 1,024 = 126,976; ×2/3 = 84,650; ×2 = 253,952.
                "128k window, 1,024 reservation",
                remote(128_000, 0, false),
                84_650,
                253_952,
                BudgetBound::Window,
            ),
            (
                // window_eff = 40,000; usable = 38,976; ×2/3 = 25,984; ×2 = 77,952.
                "cap 40k on 200k binds as a window ceiling",
                remote(200_000, 40_000, false),
                25_984,
                77_952,
                BudgetBound::UserCap,
            ),
            (
                // Words stay window-derived (84,650); bytes 253,952 clamp to
                // the scannable bound (≈89 KB).
                "redact on 128k clamps bytes only",
                remote(128_000, 0, true),
                84_650,
                REDACT_SCANNABLE_CONTEXT_BYTES,
                BudgetBound::RedactScan,
            ),
            (
                // usable = 58,976; ×2/3 = 39,317; ×2 = 117,952 > scannable →
                // the clamp applies after the cap and names the bound.
                "cap 60k + redact on 200k: the clamp is last",
                remote(200_000, 60_000, true),
                39_317,
                REDACT_SCANNABLE_CONTEXT_BYTES,
                BudgetBound::RedactScan,
            ),
            (
                // Capped bytes 77,952 stay under the scannable bound, so the
                // redact clamp never bites and the cap keeps the bound.
                "cap under the scannable bound + redact stays user_cap",
                remote(200_000, 40_000, true),
                25_984,
                77_952,
                BudgetBound::UserCap,
            ),
            (
                // usable = 198,976; ×2/3 = 132,650; ×2 = 397,952.
                "cap above the window is inert",
                remote(200_000, 300_000, false),
                132_650,
                397_952,
                BudgetBound::Window,
            ),
            (
                "cap equal to the window is inert",
                remote(200_000, 200_000, false),
                132_650,
                397_952,
                BudgetBound::Window,
            ),
            (
                "reservation at the window defaults, stated",
                remote(1_000, 0, false),
                LOCAL_BUDGET_TOKENS,
                LOCAL_BUDGET_BYTES,
                BudgetBound::DefaultUnknown,
            ),
        ];
        for (name, inputs, tokens, bytes, bound) in rows {
            let got = derive(*inputs);
            assert_eq!(got.budget_tokens, *tokens, "{name}: budget_tokens");
            assert_eq!(got.budget_bytes, *bytes, "{name}: budget_bytes");
            assert_eq!(got.bound, *bound, "{name}: bound");
        }
    }

    /// Precedence pinned pairwise: `LocalEngine` > `DefaultUnknown` >
    /// (`RedactScan` when it bites) > `UserCap` > `Window`. Each row sets up
    /// exactly the two contenders and asserts the winner.
    #[test]
    fn precedence_is_pinned_pairwise() {
        // LocalEngine > DefaultUnknown: local with window 0.
        let local_all = BudgetInputs {
            window: 0,
            cap: 40_000,
            reservation: RESERVATION,
            is_local: true,
            redact_scan: true,
            provider_id: Some("kimi"),
        };
        assert_eq!(derive(local_all).bound, BudgetBound::LocalEngine);
        // LocalEngine > RedactScan / UserCap / Window: local with everything
        // set — the pair is the default, unclamped.
        let got = derive(BudgetInputs {
            window: 200_000,
            ..local_all
        });
        assert_eq!(got.bound, BudgetBound::LocalEngine);
        assert_eq!(
            (got.budget_tokens, got.budget_bytes),
            (LOCAL_BUDGET_TOKENS, LOCAL_BUDGET_BYTES)
        );
        // DefaultUnknown > UserCap: no window, a cap set — the cap has
        // nothing to ceiling.
        assert_eq!(
            derive(remote(0, 40_000, false)).bound,
            BudgetBound::DefaultUnknown
        );
        // DefaultUnknown > RedactScan: no window, redact on — the default
        // pair's bytes sit under the scannable bound, the clamp never bites.
        assert_eq!(
            derive(remote(0, 0, true)).bound,
            BudgetBound::DefaultUnknown
        );
        // RedactScan > UserCap: both bite, the clamp is last and names it.
        assert_eq!(
            derive(remote(200_000, 60_000, true)).bound,
            BudgetBound::RedactScan
        );
        // RedactScan > Window.
        assert_eq!(
            derive(remote(128_000, 0, true)).bound,
            BudgetBound::RedactScan
        );
        // UserCap > Window.
        assert_eq!(
            derive(remote(200_000, 40_000, false)).bound,
            BudgetBound::UserCap
        );
        // And the base case: nothing else set → Window.
        assert_eq!(derive(remote(128_000, 0, false)).bound, BudgetBound::Window);
    }

    /// AC-9's default-route half: the local pair's thresholds are exactly
    /// today's literals — byte-identical, not merely proportional.
    #[test]
    fn digest_thresholds_on_the_default_route_are_todays() {
        for inputs in [BudgetInputs::local(), remote(0, 0, false)] {
            let got = derive(inputs);
            assert_eq!(got.digest_threshold_tokens, 1_500);
            assert_eq!(got.digest_threshold_bytes, 12_000);
        }
        assert_eq!(LOCAL_DIGEST_THRESHOLD_TOKENS, 1_500);
        assert_eq!(LOCAL_DIGEST_THRESHOLD_BYTES, 12_000);
        assert_eq!(LOCAL_BUDGET_BYTES, 32_768);
    }

    /// The thresholds scale as `min(fraction, ceiling)` on both currencies:
    /// on 128k the words fraction (30,999) is already above the 20,000
    /// ceiling while bytes (93,000) are not; on 200k the words ceiling binds
    /// (fraction 48,577) and the bytes ceiling does not (145,734 < 163,840).
    #[test]
    fn digest_thresholds_scale_with_the_pair_under_the_ceiling() {
        let on_128k = derive(remote(128_000, 0, false));
        let words_fraction_128k =
            on_128k.budget_tokens * LOCAL_DIGEST_THRESHOLD_TOKENS / LOCAL_BUDGET_TOKENS;
        assert_eq!(words_fraction_128k, 30_999);
        assert!(words_fraction_128k > DIGEST_ABSOLUTE_CEILING_TOKENS);
        assert_eq!(
            on_128k.digest_threshold_tokens,
            DIGEST_ABSOLUTE_CEILING_TOKENS
        );
        // Bytes: 253,952 × 12,000 / 32,768 = 93,000 — the fraction, uncapped.
        assert_eq!(on_128k.digest_threshold_bytes, 93_000);
        assert!(
            on_128k.digest_threshold_bytes < DIGEST_ABSOLUTE_CEILING_BYTES,
            "the bytes fraction on 128k stays under the ceiling"
        );

        let on_200k = derive(remote(200_000, 0, false));
        let words_fraction_200k =
            on_200k.budget_tokens * LOCAL_DIGEST_THRESHOLD_TOKENS / LOCAL_BUDGET_TOKENS;
        assert_eq!(words_fraction_200k, 48_577);
        assert_eq!(
            on_200k.digest_threshold_tokens, DIGEST_ABSOLUTE_CEILING_TOKENS,
            "the words ceiling binds on 200k"
        );
        assert_eq!(
            on_200k.digest_threshold_bytes, 145_734,
            "the bytes fraction on 200k stays under the ceiling"
        );
        assert!(
            on_200k.digest_threshold_bytes < DIGEST_ABSOLUTE_CEILING_BYTES,
            "the bytes ceiling does not bind on 200k"
        );
    }

    /// BR-7: the marker's window name follows the route — local, the
    /// provider's, or the redact-scannable window when that bound bit.
    #[test]
    fn window_labels_name_the_routes_window() {
        assert_eq!(
            derive(BudgetInputs::local()).window_label,
            "the local context window"
        );
        assert_eq!(
            derive(remote(128_000, 0, false)).window_label,
            "kimi's context window"
        );
        assert_eq!(
            derive(remote(0, 0, false)).window_label,
            "kimi's context window",
            "a defaulted remote route still names the route's window"
        );
        assert_eq!(
            derive(remote(200_000, 40_000, false)).window_label,
            "kimi's context window",
            "a user cap is a ceiling on the provider's window, not a rename"
        );
        assert_eq!(
            derive(remote(128_000, 0, true)).window_label,
            "the redact-scannable window"
        );
        assert_eq!(
            derive(BudgetInputs {
                provider_id: None,
                ..remote(128_000, 0, false)
            })
            .window_label,
            "the local context window",
            "an id-less remote route falls back to the historical name"
        );
    }

    /// One source (task AC): `HarnessConfig::default()` carries `derive(local)`
    /// and its pair is the `LOCAL_*` constants — the literals moved here and
    /// nothing recomputes them.
    #[test]
    fn harness_config_default_reads_this_module() {
        let config = HarnessConfig::default();
        assert_eq!(config.budget, derive(BudgetInputs::local()));
        assert_eq!(
            (config.context_budget_tokens, config.context_budget_bytes),
            (LOCAL_BUDGET_TOKENS, LOCAL_BUDGET_BYTES)
        );
        assert_eq!(
            (
                config.summarize_threshold_tokens,
                config.summarize_threshold_bytes
            ),
            (LOCAL_DIGEST_THRESHOLD_TOKENS, LOCAL_DIGEST_THRESHOLD_BYTES)
        );
    }

    /// `with_route_budget` is the router's one entry point (TASK-186): it
    /// sets the pair, both thresholds, and the budget fact itself.
    #[test]
    fn with_route_budget_stamps_pair_thresholds_and_fact() {
        let budget = derive(remote(128_000, 0, false));
        let config = HarnessConfig::default().with_route_budget(budget.clone());
        assert_eq!(config.context_budget_tokens, budget.budget_tokens);
        assert_eq!(config.context_budget_bytes, budget.budget_bytes);
        assert_eq!(
            config.summarize_threshold_tokens,
            budget.digest_threshold_tokens
        );
        assert_eq!(
            config.summarize_threshold_bytes,
            budget.digest_threshold_bytes
        );
        assert_eq!(config.budget, budget);
    }
}
