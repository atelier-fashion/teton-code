//! What `teton provider add` stores when a user pastes a vendor's base URL
//! (REQ-578 BR-1/BR-2/BR-3).
//!
//! Teton's `--endpoint` is the **absolute request URL** the adapter POSTs
//! verbatim — nothing joins a path onto it at request time, and REQ-577 pinned
//! that contract with a test (`a_configured_endpoint_is_the_request_url_verbatim`).
//! Every vendor, meanwhile, documents a *base* URL, every OpenAI-compatible SDK
//! takes a *base* URL, and this product's own README shipped base URLs for
//! months (BUG-170). A user who pastes `https://api.moonshot.ai/v1` therefore
//! registers cleanly and 404s on their first turn, one step removed from the
//! cause (LESSON-523).
//!
//! This module closes that gap **at the registration seam and nowhere else**:
//! [`compose_endpoint`] turns what the user typed into what gets persisted, and
//! the persisted value is always the literal request URL. No downstream
//! consumer changes shape — `Config::validate`, the adapters' verbatim POST,
//! egress origin-binding and the REQ-577 seam tests all keep seeing exactly the
//! value they see today.
//!
//! # Conservative by construction
//!
//! Composition fills in only what is *unambiguously* missing (BR-2):
//!
//! | Class | Input's path | Result |
//! |---|---|---|
//! | (a) | already ends with the kind's canonical request path | stored verbatim, `changed: false` |
//! | (b) | absent, a bare `/`, or a bare `/v1`(`/`) | the canonical path is appended, `changed: true` |
//! | (c) | anything else | stored verbatim, `changed: false` |
//!
//! Class (c) is the load-bearing one. Self-hosted gateways and proxies serve
//! chat completions at arbitrary paths, so an explicit path is trusted rather
//! than "corrected" — a normalizer that knew better than its user would break
//! the deployments Teton exists to support. Malformed input (no scheme, no
//! authority) lands in class (c) too: it is stored as typed and refused later
//! by the same validation that refuses it today, so this module adds **no new
//! fatal class** (BR-6).
//!
//! # Pure by construction
//!
//! `teton-core` carries no HTTP dependencies (crate docs), and this module adds
//! none: the classifier is deliberate string work — find `://`, then the first
//! `/`, `?` or `#` after the authority — because the question it answers is only
//! "does a path exist, and is it one of three trivial shapes?". A URL parser
//! would buy correctness this code does not need and a dependency the crate has
//! a reason not to have. Everything here is a pure function of its arguments,
//! so the whole rule is testable with no daemon, no config and no network
//! (LESSON-481).
//!
//! # One spelling of the rule
//!
//! The canonical request paths used to exist only inside the recipe catalog's
//! values and the hand-written match in its seam test
//! (`crates/tetond/src/provider_recipes.rs`). This module is now their source;
//! `crates/tetond/tests/endpoint_composition_bridge.rs` pins the two spellings
//! against each other so drift between them fails a test rather than a user's
//! first turn (REQ-578 ADR-2).
//!
//! # Vendor facts, verified in both halves
//!
//! LESSON-523's rule is that a path fact is checked against the vendor's docs
//! *and* against this product's own contract. Both halves were re-checked on
//! 2026-08-15 while writing this module; the per-constant doc comments record
//! what was read.

use crate::entities::ProviderKind;

/// The terminal request path an OpenAI-compatible provider serves.
///
/// Verified 2026-08-15 against OpenAI's API reference
/// (developers.openai.com/api/reference), whose Chat Completions curl posts to
/// `https://api.openai.com/v1/chat/completions`, and against DeepSeek's API
/// docs (api-docs.deepseek.com), whose "Your First API Call" curl posts to
/// `https://api.deepseek.com/chat/completions` with the OpenAI-format
/// `base_url` given as `https://api.deepseek.com` — no `/v1` anywhere in the
/// current documentation.
///
/// Those two facts are why this constant is the **terminal** path and not
/// `/v1/chat/completions`: the version segment belongs to the vendor's base
/// URL, which is the part the user pastes, and it is present for OpenAI, Kimi,
/// Ollama and xAI while absent for DeepSeek. Appending only `/chat/completions`
/// is therefore the one rule that is right for every entry in the catalog when
/// the user supplies the base URL the vendor documents.
///
/// Product half: every OpenAI-compatible entry in `recipe_catalog()` ends with
/// this exact string, and the REQ-577 seam test requires it of them.
pub const OPENAI_COMPATIBLE_REQUEST_PATH: &str = "/chat/completions";

/// The request path the Anthropic Messages API serves.
///
/// Verified 2026-08-15 against Anthropic's Messages API reference
/// (platform.claude.com/docs/en/api/messages), whose curl posts to
/// `https://api.anthropic.com/v1/messages`.
///
/// Versioned as a unit, unlike [`OPENAI_COMPATIBLE_REQUEST_PATH`]: this kind
/// names one vendor's protocol rather than a shape many hosts imitate, so the
/// `/v1` is part of the path this adapter asks for rather than part of an
/// address the user chooses. A reader pattern-matching this onto its
/// OpenAI-compatible neighbours gets a 404, which is why the catalog's
/// Anthropic entry carries a note saying so.
pub const ANTHROPIC_REQUEST_PATH: &str = "/v1/messages";

/// The endpoint `--kind anthropic` registers when the user supplies none
/// (BR-3).
///
/// [`ANTHROPIC_REQUEST_PATH`] on `https://api.anthropic.com`, the origin of the
/// same verified curl example. Written **explicitly into config** at
/// registration time rather than applied at runtime: the stored config stays
/// the declared identity (REQ-574's durable-document posture), and the add path
/// can no longer reach `Config::validate`'s missing-endpoint rejection — the
/// rejection that used to fire *after* the user's key had already been read
/// into the keychain (BUG-170).
///
/// Equal by construction to the catalog's Anthropic recipe endpoint; the bridge
/// test asserts it.
pub const ANTHROPIC_DEFAULT_ENDPOINT: &str = "https://api.anthropic.com/v1/messages";

/// What a registration should persist as its endpoint, and whether that differs
/// from what the user typed.
///
/// `changed` exists for the user, not for the code: BR-4 requires the CLI to
/// echo the stored value whenever composition applied or the Anthropic default
/// filled in, so the user learns what will be called at the moment it is
/// decided rather than from a downstream 404 (LESSON-456, BUG-146). It is also
/// `doctor`'s advisory predicate (BR-6): a stored endpoint that would still
/// change if it were re-composed is one of BR-2's class (b) shapes, and
/// `stored` is then the exact full form to tell the user about.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ComposedEndpoint {
    /// The absolute request URL to persist, or `None` when there is nothing to
    /// store. A `None` here is not an error: the missing-endpoint refusal stays
    /// where it already lives, in `Config::validate` (BR-6).
    pub stored: Option<String>,
    /// Whether `stored` differs from the caller's input.
    pub changed: bool,
}

/// The canonical request path for a kind, or `None` for a kind that has none.
///
/// A match on [`ProviderKind`] rather than a lookup table, so a new kind cannot
/// be added without deciding what its request path looks like — the same
/// compile-time technique the recipe catalog's seam test uses.
///
/// `Local` has no endpoint at all, and `Custom` names an operator's own adapter
/// whose protocol Teton does not know. Composing for either would be a guess,
/// and a guess written into a user's config is worse than the base URL they
/// typed: it is a value nobody chose. Both therefore pass through untouched.
#[must_use]
pub fn canonical_request_path(kind: ProviderKind) -> Option<&'static str> {
    match kind {
        ProviderKind::OpenaiCompatible => Some(OPENAI_COMPATIBLE_REQUEST_PATH),
        ProviderKind::Anthropic => Some(ANTHROPIC_REQUEST_PATH),
        ProviderKind::Local | ProviderKind::Custom => None,
    }
}

/// Turn what a user supplied at registration into what should be persisted.
///
/// The BR-2 classes in one place. Never fails, never panics, and never returns
/// a value that is not either the caller's input verbatim or the caller's input
/// with a canonical path appended — the two shapes a reviewer has to check.
///
/// ```
/// use teton_core::{compose_endpoint, ProviderKind, ANTHROPIC_DEFAULT_ENDPOINT};
///
/// // A vendor base URL gains the kind's request path.
/// let base = Some("https://api.moonshot.ai/v1");
/// let composed = compose_endpoint(ProviderKind::OpenaiCompatible, base);
/// assert_eq!(
///     composed.stored.as_deref(),
///     Some("https://api.moonshot.ai/v1/chat/completions")
/// );
/// assert!(composed.changed);
///
/// // A full request URL is already what gets POSTed, so it is stored as typed.
/// let composed = compose_endpoint(ProviderKind::Anthropic, Some(ANTHROPIC_DEFAULT_ENDPOINT));
/// assert_eq!(composed.stored.as_deref(), Some(ANTHROPIC_DEFAULT_ENDPOINT));
/// assert!(!composed.changed);
/// ```
#[must_use]
pub fn compose_endpoint(kind: ProviderKind, input: Option<&str>) -> ComposedEndpoint {
    let Some(canonical) = canonical_request_path(kind) else {
        // No canonical path: pass through, including `None`.
        return ComposedEndpoint {
            stored: input.map(str::to_owned),
            changed: false,
        };
    };

    let Some(input) = input else {
        // BR-3: only Anthropic has an official address to default to. For
        // OpenAI-compatible there is no such thing as "the" host, so a missing
        // endpoint stays missing and `Config::validate` refuses it exactly as
        // it does today.
        return match kind {
            ProviderKind::Anthropic => ComposedEndpoint {
                stored: Some(ANTHROPIC_DEFAULT_ENDPOINT.to_owned()),
                changed: true,
            },
            _ => ComposedEndpoint {
                stored: None,
                changed: false,
            },
        };
    };

    match compose_path(input, canonical) {
        Some(composed) => ComposedEndpoint {
            stored: Some(composed),
            changed: true,
        },
        None => ComposedEndpoint {
            stored: Some(input.to_owned()),
            changed: false,
        },
    }
}

/// The composed URL for a BR-2 class (b) input, or `None` for classes (a) and
/// (c) — the two that are stored verbatim, and so need no new string.
fn compose_path(input: &str, canonical: &str) -> Option<String> {
    // Class (a) first, and spelled as `ends_with` because that is the check the
    // REQ-577 seam test makes of every shipped recipe: whatever else is true of
    // the URL, one that already ends in the request path is already the URL the
    // adapter will POST.
    if input.ends_with(canonical) {
        return None;
    }

    let path = path_after_authority(input)?;

    match path {
        // No path or a bare `/`: the canonical path is the whole path.
        "" | "/" => Some(format!("{}{canonical}", input.trim_end_matches('/'))),
        // A bare version segment. The user pasted the base URL their vendor's
        // SDK quickstart takes; what is missing is the part the SDK would have
        // appended. For Anthropic that part is the canonical path *minus* the
        // `/v1` already present — appending it whole would produce
        // `/v1/v1/messages`, a URL that is wrong in a way this module exists to
        // prevent.
        "/v1" | "/v1/" => {
            let suffix = canonical
                .strip_prefix("/v1")
                .filter(|rest| rest.starts_with('/'));
            let suffix = suffix.unwrap_or(canonical);
            Some(format!("{}{suffix}", input.trim_end_matches('/')))
        }
        // Class (c): an explicit path, trusted verbatim.
        _ => None,
    }
}

/// The path (with its leading `/`) of an absolute URL, or `None` when the input
/// is not one this module will compose onto.
///
/// `None` covers every shape where appending would be a guess rather than a
/// completion: no `://` (so no scheme), an empty authority, or a query/fragment
/// where the path would go. A URL carrying `?` or `#` and no path has been
/// addressed deliberately — `http://localhost:8888/search?format=json` is a
/// real endpoint shape this product ships elsewhere — and appending a path
/// after a query would produce nonsense. All of these fall to class (c) and are
/// stored exactly as typed, which is the BR-6 promise: odd input is somebody
/// else's refusal, never a panic and never a new error class here.
fn path_after_authority(input: &str) -> Option<&str> {
    let (scheme, rest) = input.split_once("://")?;
    if scheme.is_empty() {
        return None;
    }
    let authority_end = rest.find(['/', '?', '#']).unwrap_or(rest.len());
    if authority_end == 0 {
        // Empty authority — there is no origin to compose onto.
        return None;
    }
    Some(&rest[authority_end..])
}

#[cfg(test)]
mod tests {
    use super::*;

    /// What a maintainer standing over a red cell in the table below needs
    /// told: which kind, which input, and what the rule says should have come
    /// out of it.
    fn assert_composes(
        kind: ProviderKind,
        input: Option<&str>,
        stored: Option<&str>,
        changed: bool,
    ) {
        let composed = compose_endpoint(kind, input);
        assert_eq!(
            composed,
            ComposedEndpoint {
                stored: stored.map(str::to_owned),
                changed,
            },
            "compose_endpoint({kind:?}, {input:?}) is the registration seam's whole rule \
             (REQ-578 BR-2). Expected {stored:?} (changed: {changed}). A composed value that \
             is wrong here is persisted, POSTed verbatim, and shows up as a 404 on the \
             user's first turn — re-read BR-2's classes before changing this expectation."
        );
    }

    /// Every (kind × BR-2 class) cell, in one table (BR-8).
    ///
    /// Read down a kind's rows and the rule is visible as a whole: canonical in
    /// / canonical out, bare shapes completed, everything else untouched. The
    /// per-property tests below exist to *diagnose* a failure here, not to
    /// substitute for it.
    #[test]
    fn the_composition_table_holds_for_every_kind_and_class() {
        use ProviderKind::{Anthropic, Custom, Local, OpenaiCompatible};

        let table: &[(ProviderKind, Option<&str>, Option<&str>, bool)] = &[
            // --- openai-compatible ---------------------------------------
            // (a) canonical already: every shipped recipe's shape.
            (
                OpenaiCompatible,
                Some("https://api.openai.com/v1/chat/completions"),
                Some("https://api.openai.com/v1/chat/completions"),
                false,
            ),
            (
                OpenaiCompatible,
                Some("https://api.deepseek.com/chat/completions"),
                Some("https://api.deepseek.com/chat/completions"),
                false,
            ),
            (
                OpenaiCompatible,
                Some("http://localhost:11434/v1/chat/completions"),
                Some("http://localhost:11434/v1/chat/completions"),
                false,
            ),
            // (b) no path.
            (
                OpenaiCompatible,
                Some("https://api.deepseek.com"),
                Some("https://api.deepseek.com/chat/completions"),
                true,
            ),
            // (b) bare `/`.
            (
                OpenaiCompatible,
                Some("https://api.deepseek.com/"),
                Some("https://api.deepseek.com/chat/completions"),
                true,
            ),
            // (b) bare `/v1` — AC-1's input.
            (
                OpenaiCompatible,
                Some("https://api.moonshot.ai/v1"),
                Some("https://api.moonshot.ai/v1/chat/completions"),
                true,
            ),
            // (b) bare `/v1/`.
            (
                OpenaiCompatible,
                Some("https://api.moonshot.ai/v1/"),
                Some("https://api.moonshot.ai/v1/chat/completions"),
                true,
            ),
            // (c) explicit custom path — AC-4.
            (
                OpenaiCompatible,
                Some("https://gw.example.com/llm/proxy"),
                Some("https://gw.example.com/llm/proxy"),
                false,
            ),
            // (c) missing scheme.
            (
                OpenaiCompatible,
                Some("api.moonshot.ai/v1"),
                Some("api.moonshot.ai/v1"),
                false,
            ),
            // no input at all.
            (OpenaiCompatible, None, None, false),
            // --- anthropic -----------------------------------------------
            // (a) canonical already.
            (
                Anthropic,
                Some(ANTHROPIC_DEFAULT_ENDPOINT),
                Some(ANTHROPIC_DEFAULT_ENDPOINT),
                false,
            ),
            // (b) no path.
            (
                Anthropic,
                Some("https://api.anthropic.com"),
                Some(ANTHROPIC_DEFAULT_ENDPOINT),
                true,
            ),
            // (b) bare `/`.
            (
                Anthropic,
                Some("https://api.anthropic.com/"),
                Some(ANTHROPIC_DEFAULT_ENDPOINT),
                true,
            ),
            // (b) bare `/v1` — the segment is not doubled.
            (
                Anthropic,
                Some("https://api.anthropic.com/v1"),
                Some(ANTHROPIC_DEFAULT_ENDPOINT),
                true,
            ),
            // (b) bare `/v1/`.
            (
                Anthropic,
                Some("https://api.anthropic.com/v1/"),
                Some(ANTHROPIC_DEFAULT_ENDPOINT),
                true,
            ),
            // (c) explicit custom path — AC-4.
            (
                Anthropic,
                Some("https://gw.example.com/llm/proxy"),
                Some("https://gw.example.com/llm/proxy"),
                false,
            ),
            // (c) missing scheme.
            (
                Anthropic,
                Some("api.anthropic.com/v1"),
                Some("api.anthropic.com/v1"),
                false,
            ),
            // no input at all — BR-3's default.
            (Anthropic, None, Some(ANTHROPIC_DEFAULT_ENDPOINT), true),
            // --- local ---------------------------------------------------
            // The on-device tier has no endpoint; whatever is passed comes back.
            (
                Local,
                Some("https://api.moonshot.ai/v1"),
                Some("https://api.moonshot.ai/v1"),
                false,
            ),
            (Local, Some(""), Some(""), false),
            (Local, None, None, false),
            // --- custom --------------------------------------------------
            // Remote, but of a protocol Teton does not know — never composed.
            (
                Custom,
                Some("https://gw.example.com/v1"),
                Some("https://gw.example.com/v1"),
                false,
            ),
            (Custom, None, None, false),
        ];

        for &(kind, input, stored, changed) in table {
            assert_composes(kind, input, stored, changed);
        }
    }

    /// BR-3: the Anthropic default is written explicitly, and it is the same
    /// string a user would get by pasting the vendor's base URL.
    #[test]
    fn anthropic_defaults_to_the_official_messages_url() {
        let defaulted = compose_endpoint(ProviderKind::Anthropic, None);
        assert_eq!(
            defaulted.stored.as_deref(),
            Some(ANTHROPIC_DEFAULT_ENDPOINT)
        );
        assert!(
            defaulted.changed,
            "a filled-in default differs from what the user typed (nothing), so BR-4's echo \
             has to fire — this is the flag the CLI reads to decide"
        );
        assert_eq!(
            compose_endpoint(ProviderKind::Anthropic, Some("https://api.anthropic.com")).stored,
            defaulted.stored,
            "the default and the composed base URL are the same address; if these two ever \
             disagree, one of them is a typo"
        );
    }

    /// The missing-endpoint refusal stays where it lives (BR-6): for a kind
    /// with no official host, composition does not invent one.
    #[test]
    fn openai_compatible_without_an_endpoint_stays_missing() {
        assert_eq!(
            compose_endpoint(ProviderKind::OpenaiCompatible, None),
            ComposedEndpoint {
                stored: None,
                changed: false,
            },
            "there is no such thing as *the* OpenAI-compatible host. Defaulting to one would \
             register a provider pointed at somebody else's vendor; `Config::validate` owns \
             this refusal and keeps owning it."
        );
    }

    /// The constants agree with each other, so a future edit to one of them
    /// cannot silently split the pair.
    #[test]
    fn the_anthropic_default_is_the_anthropic_path_on_the_official_origin() {
        assert!(
            ANTHROPIC_DEFAULT_ENDPOINT.ends_with(ANTHROPIC_REQUEST_PATH),
            "the default endpoint has to *be* a Messages URL; `{ANTHROPIC_DEFAULT_ENDPOINT}` \
             does not end with `{ANTHROPIC_REQUEST_PATH}`"
        );
        assert_eq!(
            ANTHROPIC_DEFAULT_ENDPOINT.strip_suffix(ANTHROPIC_REQUEST_PATH),
            Some("https://api.anthropic.com"),
            "verified 2026-08-15 against platform.claude.com/docs/en/api/messages, whose curl \
             posts to https://api.anthropic.com/v1/messages"
        );
    }

    /// Only the two kinds a vendor recipe may name have a path; the other two
    /// are declared pathless rather than falling through a `_` arm.
    #[test]
    fn only_the_registerable_remote_kinds_have_a_canonical_path() {
        assert_eq!(
            canonical_request_path(ProviderKind::OpenaiCompatible),
            Some("/chat/completions")
        );
        assert_eq!(
            canonical_request_path(ProviderKind::Anthropic),
            Some("/v1/messages")
        );
        assert_eq!(canonical_request_path(ProviderKind::Local), None);
        assert_eq!(canonical_request_path(ProviderKind::Custom), None);
    }

    /// AC-2's property, stated as a property: whatever comes out of the seam
    /// survives going through it again unchanged. Every previously documented
    /// full-URL command therefore behaves byte-identically (BR-7), and a config
    /// re-registered from its own stored value cannot drift.
    #[test]
    fn composing_a_composed_endpoint_changes_nothing() {
        let inputs = [
            "https://api.moonshot.ai/v1",
            "https://api.deepseek.com",
            "https://api.anthropic.com/v1/",
            "https://gw.example.com/llm/proxy",
            "api.moonshot.ai/v1",
        ];

        for kind in [ProviderKind::OpenaiCompatible, ProviderKind::Anthropic] {
            for input in inputs {
                let once = compose_endpoint(kind, Some(input));
                let twice = compose_endpoint(kind, once.stored.as_deref());
                assert_eq!(
                    twice.stored, once.stored,
                    "composing {kind:?}'s `{input}` twice moved it. The seam runs on every \
                     `provider add`, including one a user re-runs from the value the last one \
                     printed — a rule that is not a fixed point rewrites its own output."
                );
                assert!(
                    !twice.changed,
                    "`{input}` composed to a value that would still compose again, so the \
                     BR-4 echo (and doctor's BR-6 advisory, which reads the same flag) would \
                     fire on an endpoint that is already correct"
                );
            }
        }
    }

    /// BR-6: odd input is stored as typed. No panic, no error, no new class —
    /// whatever refuses these today keeps refusing them.
    #[test]
    fn malformed_input_is_stored_verbatim_and_never_panics() {
        let odd = [
            // No scheme: `://` is the only thing this classifier looks for
            // first, and without it there is no authority to reason about.
            "api.moonshot.ai/v1",
            "localhost:11434",
            "/v1",
            "",
            // Scheme present, authority empty.
            "https:///v1",
            "://api.moonshot.ai",
            // A query where the path would go: deliberately addressed already
            // (this product ships `?format=json` endpoints elsewhere), so
            // appending after it would produce nonsense.
            "https://gw.example.com?tenant=a",
            "https://gw.example.com#frag",
            // Not a URL at all.
            "not a url",
            "https://",
            // An empty path segment is not one of BR-2's three bare shapes.
            // Deliberate: the class (b) list is exhaustive rather than
            // "roughly empty-looking", so anything outside it is somebody's
            // explicit address until BR-2 says otherwise.
            "https://api.deepseek.com//",
        ];

        for kind in [ProviderKind::OpenaiCompatible, ProviderKind::Anthropic] {
            for input in odd {
                assert_composes(kind, Some(input), Some(input), false);
            }
        }
    }

    /// The trailing slash never becomes a doubled separator — the failure mode
    /// a naive `format!("{input}{path}")` would ship, and one that produces a
    /// URL some hosts 404 on and others silently normalize.
    #[test]
    fn a_trailing_slash_does_not_double_the_separator() {
        for input in ["https://api.deepseek.com/", "https://api.moonshot.ai/v1/"] {
            let stored = compose_endpoint(ProviderKind::OpenaiCompatible, Some(input))
                .stored
                .expect("a class (b) input always stores something");
            assert!(
                !stored.contains("//chat"),
                "`{input}` composed to `{stored}`, which carries a doubled separator"
            );
            assert!(
                stored.ends_with(OPENAI_COMPATIBLE_REQUEST_PATH),
                "`{input}` composed to `{stored}`, which is not a request URL"
            );
        }
    }

    /// The `/v1` spelling, both kinds, stated on its own because the two kinds
    /// have to disagree here: one canonical path starts with `/v1` and the
    /// other does not, and getting that wrong yields `/v1/v1/messages`.
    #[test]
    fn a_bare_version_segment_is_completed_without_doubling_it() {
        assert_eq!(
            compose_endpoint(
                ProviderKind::Anthropic,
                Some("https://api.anthropic.com/v1")
            )
            .stored
            .as_deref(),
            Some("https://api.anthropic.com/v1/messages")
        );
        assert_eq!(
            compose_endpoint(
                ProviderKind::OpenaiCompatible,
                Some("https://api.moonshot.ai/v1")
            )
            .stored
            .as_deref(),
            Some("https://api.moonshot.ai/v1/chat/completions"),
            "the OpenAI-compatible path does not carry a version segment — the vendor's base \
             URL does, and DeepSeek's has none at all"
        );
    }

    /// `Local` is not merely uncomposed, it is untouched: the on-device tier
    /// reaches nothing off the machine, so there is no request URL to complete.
    #[test]
    fn the_local_kind_passes_its_input_through() {
        for input in [
            Some("https://api.moonshot.ai/v1"),
            Some("anything at all"),
            Some(""),
            None,
        ] {
            let composed = compose_endpoint(ProviderKind::Local, input);
            assert_eq!(composed.stored.as_deref(), input);
            assert!(!composed.changed);
        }
    }
}
