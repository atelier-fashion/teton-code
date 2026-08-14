//! The search backends `/web setup` suggests, written down once (REQ-573 BR-1).
//!
//! A user configuring the `search` tier has to know two things no error message
//! can teach them: what a working endpoint looks like for the backend they have,
//! and which header their key rides in. Both were shipped three times over — a
//! help block in the CLI, a lookup table beside it, and a sentence in the
//! daemon's bundled guide — and BUG-165 is what that costs: Brave's header
//! corrected in one copy while the other two kept offering `Authorization:
//! Bearer {key}` to a backend that answers 401 for it.
//!
//! This module is the one copy. The daemon hands the list to clients on
//! `web/setup_plan` (ADR-D: that call is the flow's entry point, so no second
//! discovery surface is needed), a client renders what it was handed, and the
//! contract suite enumerates *this* rather than parsing anyone's source text.
//!
//! # Pure by construction
//!
//! [`suggestion_catalog`] takes nothing and reads nothing — no config, no env,
//! no TTY, no daemon state. That is LESSON-481's rule applied ahead of the fact:
//! product data behind a gate is data the test suite cannot see, and the whole
//! value of one definition is that one test can pin it. The signature is the
//! proof, and the test module below is the pin — it calls the factory with no
//! setup at all.
//!
//! # What may not live here
//!
//! Never a credential, and never a field that could hold one (BR-6). An
//! `auth_template` names the *shape* a key rides in, with `{key}` marking where
//! substitution happens when a request is built; the key itself comes from the
//! keychain reference in the user's config and never passes through this list.

use teton_protocol::methods::{WebBackendSuggestion, WebSetupCatalog};
use teton_protocol::GENERIC_SEARCH_AUTH_TEMPLATE;

/// The backends this build suggests, plus the header shape to offer for the
/// ones it does not name.
///
/// Ordered as a client should show them: the keyless self-hosted option first,
/// because it is the one a user can reach without buying anything, then the two
/// hosted APIs whose header shapes are the ones people actually get wrong.
///
/// A `String` per field rather than `&'static str`: the protocol types are the
/// wire's types, and owning them here keeps the seam free of a lifetime that
/// exists only because the data happens to be static today (ADR-A).
#[must_use]
pub fn suggestion_catalog() -> WebSetupCatalog {
    WebSetupCatalog {
        // The shape for a backend nothing here describes, carried as data so a
        // client never declares a second copy of it (ADR-B).
        default_auth_template: GENERIC_SEARCH_AUTH_TEMPLATE.to_owned(),
        backends: vec![
            WebBackendSuggestion {
                id: "searxng".to_owned(),
                label: "self-hosted SearxNG".to_owned(),
                // `format=json` is not decoration: a SearxNG instance answers
                // HTML without it, and the parse then finds no results at all.
                endpoint: "http://localhost:8888/search?format=json".to_owned(),
                // No host to match on, deliberately: a self-hosted instance
                // lives wherever its owner runs it, so the endpoint above is an
                // example rather than an address, and matching `localhost`
                // would claim every other local backend as SearxNG.
                host: None,
                // Absent because the backend wants no header at all — not
                // because the header is unknown.
                auth_template: None,
                needs_key: false,
                // No notes on any shipped entry: REQ-573 moves ownership of
                // the suggestions, not their content, so the rendered block
                // stays line-identical to v0.1.14 (spec Assumptions).
                notes: None,
            },
            WebBackendSuggestion {
                id: "brave".to_owned(),
                label: "Brave Search API".to_owned(),
                endpoint: "https://api.search.brave.com/res/v1/web/search".to_owned(),
                host: Some("api.search.brave.com".to_owned()),
                // BUG-165: Brave rejects a Bearer header. This spelling is the
                // fix, and it is now spelled in exactly one place.
                auth_template: Some("X-Subscription-Token: {key}".to_owned()),
                needs_key: true,
                notes: None,
            },
            WebBackendSuggestion {
                id: "kagi".to_owned(),
                label: "Kagi Search API".to_owned(),
                endpoint: "https://kagi.com/api/v0/search".to_owned(),
                host: Some("kagi.com".to_owned()),
                // `Bot`, not `Bearer` — the other half of BUG-165.
                auth_template: Some("Authorization: Bot {key}".to_owned()),
                needs_key: true,
                notes: None,
            },
        ],
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use std::collections::BTreeSet;

    /// **AC-6 at the daemon's altitude: the shipped strings, byte for byte.**
    ///
    /// Golden rather than derived. Every value here is a fact about a third
    /// party's API — Brave's header name, Kagi's `Bot` scheme, SearxNG's
    /// `format=json` — which no rule in this repository can regenerate, so the
    /// only honest way to guard them is to write them out a second time and
    /// require the two spellings to agree. A reworded label is meant to be a
    /// one-line diff here; a reworded *header* is meant to be a failure.
    #[test]
    fn the_catalog_ships_the_three_backends_verbatim() {
        let catalog = suggestion_catalog();

        assert_eq!(
            catalog.default_auth_template, "Authorization: Bearer {key}",
            "the generic fallback shape changed"
        );
        assert_eq!(
            catalog.default_auth_template, GENERIC_SEARCH_AUTH_TEMPLATE,
            "the default must be the shared protocol constant, not a re-typed copy (ADR-B)"
        );

        let ids: Vec<&str> = catalog.backends.iter().map(|b| b.id.as_str()).collect();
        assert_eq!(
            ids,
            ["searxng", "brave", "kagi"],
            "the suggestions, in the order clients show them"
        );

        let searxng = &catalog.backends[0];
        assert_eq!(searxng.label, "self-hosted SearxNG");
        assert_eq!(searxng.endpoint, "http://localhost:8888/search?format=json");
        assert_eq!(
            searxng.host, None,
            "a self-hosted instance answers for no fixed host"
        );
        assert_eq!(searxng.auth_template, None);
        assert!(!searxng.needs_key);

        let brave = &catalog.backends[1];
        assert_eq!(brave.label, "Brave Search API");
        assert_eq!(
            brave.endpoint,
            "https://api.search.brave.com/res/v1/web/search"
        );
        assert_eq!(brave.host.as_deref(), Some("api.search.brave.com"));
        assert_eq!(
            brave.auth_template.as_deref(),
            Some("X-Subscription-Token: {key}"),
            "BUG-165: Brave refuses a Bearer header"
        );
        assert!(brave.needs_key);

        let kagi = &catalog.backends[2];
        assert_eq!(kagi.label, "Kagi Search API");
        assert_eq!(kagi.endpoint, "https://kagi.com/api/v0/search");
        assert_eq!(kagi.host.as_deref(), Some("kagi.com"));
        assert_eq!(
            kagi.auth_template.as_deref(),
            Some("Authorization: Bot {key}"),
            "BUG-165: Kagi's scheme is `Bot`"
        );
        assert!(kagi.needs_key);

        assert!(
            catalog.backends.iter().all(|b| b.notes.is_none()),
            "REQ-573 moves ownership, not content: a note would add a rendered \
             line the v0.1.14 flow never showed"
        );
    }

    /// Ids are what callers and tests key on, so two entries sharing one would
    /// make a lookup answer with whichever came first — silently, and
    /// differently depending on iteration order.
    #[test]
    fn the_ids_are_unique() {
        let catalog = suggestion_catalog();
        let distinct: BTreeSet<&str> = catalog.backends.iter().map(|b| b.id.as_str()).collect();
        assert_eq!(
            distinct.len(),
            catalog.backends.len(),
            "duplicate suggestion id in: {:?}",
            catalog.backends.iter().map(|b| &b.id).collect::<Vec<_>>()
        );
    }

    /// `needs_key` is the flow's default answer and `auth_template` is what it
    /// offers next; an entry where they disagree either asks for a key it has
    /// nowhere to put, or skips the question for a backend that will 401.
    #[test]
    fn a_template_is_present_exactly_when_a_key_is_needed() {
        for backend in suggestion_catalog().backends {
            assert_eq!(
                backend.auth_template.is_some(),
                backend.needs_key,
                "`{}` has needs_key={} but auth_template={:?}",
                backend.id,
                backend.needs_key,
                backend.auth_template
            );
        }
    }

    /// A template without `{key}` is not a template: the request builder
    /// substitutes on that marker, so its absence ships a header that carries
    /// the user's key nowhere and fails as an auth error they cannot explain.
    #[test]
    fn every_template_marks_where_the_key_goes() {
        let catalog = suggestion_catalog();
        let templates = catalog
            .backends
            .iter()
            .filter_map(|b| b.auth_template.as_deref())
            .chain(std::iter::once(catalog.default_auth_template.as_str()));
        for template in templates {
            assert!(
                template.contains("{key}"),
                "template {template:?} has no `{{key}}` placeholder"
            );
            assert!(
                template.contains(": "),
                "template {template:?} is not a `Name: value` header shape"
            );
        }
    }

    /// **BR-6: no field may carry a credential.**
    ///
    /// The catalog is sent to every client that asks for a plan and is quoted
    /// back in help text, so a key pasted into it during a debugging session
    /// would travel further than any config value does. The check is shape-based
    /// rather than a name denylist on purpose — `X-Subscription-Token` would
    /// trip a denylist while carrying nothing — so it looks for what a secret
    /// actually is: a long opaque run of characters, or a URL with credentials
    /// in its userinfo.
    #[test]
    fn no_field_carries_anything_secret_shaped() {
        let catalog = suggestion_catalog();
        let mut fields: Vec<&str> = vec![catalog.default_auth_template.as_str()];
        for backend in &catalog.backends {
            fields.push(&backend.id);
            fields.push(&backend.label);
            fields.push(&backend.endpoint);
            fields.extend(backend.host.as_deref());
            fields.extend(backend.auth_template.as_deref());
            fields.extend(backend.notes.as_deref());
        }
        // Non-vacuity: the sweep must actually be reading the catalog.
        assert!(
            fields.len() >= 3 * 4,
            "the field sweep collected {fields:?}"
        );

        for field in fields {
            assert!(
                !has_opaque_run(field),
                "{field:?} contains a run long enough to be a credential"
            );
            assert!(
                !has_userinfo(field),
                "{field:?} embeds credentials in a URL's userinfo"
            );
        }
    }

    /// Whether `text` holds an unbroken alphanumeric run long enough to be a
    /// real key. Every legitimate value here is English words, host labels, and
    /// header names, none of which reach twenty characters between separators.
    fn has_opaque_run(text: &str) -> bool {
        text.split(|c: char| !(c.is_ascii_alphanumeric() || c == '_'))
            .any(|run| run.len() >= 20)
    }

    /// Whether `text` is a URL carrying `user:pass@` before its host — the shape
    /// a credential takes when it hides inside an endpoint.
    fn has_userinfo(text: &str) -> bool {
        let Some((_scheme, after)) = text.split_once("://") else {
            return false;
        };
        let authority = after.split(['/', '?', '#']).next().unwrap_or("");
        matches!(authority.split_once('@'), Some((userinfo, _)) if userinfo.contains(':'))
    }

    /// **The purity pin (LESSON-481).**
    ///
    /// Every test above calls the factory with no fixture, no environment, and
    /// no daemon — this one says so out loud: two calls with nothing done
    /// between them return the same catalog. Add a config read or a TTY check
    /// and the signature would have to change to accommodate it, which is the
    /// point: the compiler refuses the gate before this test has to catch it.
    #[test]
    fn the_factory_needs_no_setup_and_answers_the_same_every_time() {
        assert_eq!(suggestion_catalog(), suggestion_catalog());
    }
}
