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

    /// Hosts are what a *typed* endpoint is matched back to, so two entries
    /// claiming one host would decide by iteration order which header shape the
    /// user is offered — silently, and for a backend that answers 401 to the
    /// other one. That is BUG-165's failure with a different cause: not a stale
    /// copy of a template, but the right template shadowed by a second entry.
    ///
    /// `None` is excluded rather than deduplicated: it means "answers for no
    /// fixed host" (a self-hosted backend), and several entries may say that
    /// without any of them shadowing another.
    #[test]
    fn the_hosts_are_unique() {
        let catalog = suggestion_catalog();
        let hosts: Vec<&str> = catalog
            .backends
            .iter()
            .filter_map(|b| b.host.as_deref())
            .collect();
        // Non-vacuity: a catalog whose every host went `None` would satisfy the
        // uniqueness check by having nothing to check, and would also have
        // nothing for the host-match offer to match on.
        assert!(
            !hosts.is_empty(),
            "no suggestion declares a host, so a typed endpoint can match none of them"
        );
        let distinct: BTreeSet<&str> = hosts.iter().copied().collect();
        assert_eq!(
            distinct.len(),
            hosts.len(),
            "two suggestions claim the same host, so which template a typed endpoint is \
             offered depends on iteration order: {hosts:?}"
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
    /// actually is: a long opaque run of characters, a URL with credentials in
    /// its userinfo, or a query parameter *named* like one.
    ///
    /// The field list is **destructured**, not hand-picked. A sweep that names
    /// its fields is complete only until the next field is added, and a field
    /// added to a wire type is exactly the moment a sweep must not be silently
    /// partial — so a new `WebBackendSuggestion` field breaks this build until
    /// somebody decides whether it can carry a secret.
    #[test]
    fn no_field_carries_anything_secret_shaped() {
        let WebSetupCatalog {
            default_auth_template,
            backends,
        } = suggestion_catalog();

        let backend_count = backends.len();
        let mut fields: Vec<String> = vec![default_auth_template];
        for backend in backends {
            let WebBackendSuggestion {
                id,
                label,
                endpoint,
                host,
                auth_template,
                needs_key,
                notes,
            } = backend;
            fields.push(id);
            fields.push(label);
            fields.push(endpoint);
            fields.extend(host);
            fields.extend(auth_template);
            fields.extend(notes);
            // The one field swept by inspection rather than by content: a bool
            // has no room for a secret.
            let _ = needs_key;
        }

        // Non-vacuity, derived rather than a bare literal so it moves with the
        // catalog: every backend contributes its three non-optional strings and
        // the default template contributes one, so a sweep reading less than
        // that is reading something other than this catalog.
        const ALWAYS_PRESENT_PER_BACKEND: usize = 3; // id, label, endpoint
        const CATALOG_LEVEL_FIELDS: usize = 1; // default_auth_template
        let floor = CATALOG_LEVEL_FIELDS + ALWAYS_PRESENT_PER_BACKEND * backend_count;
        assert!(
            backend_count >= 3,
            "the sweep has {backend_count} backends to sweep; the catalog documents three"
        );
        assert!(
            fields.len() >= floor,
            "the field sweep collected {} values for {backend_count} backends, fewer than \
             the {floor} they always carry: {fields:?}",
            fields.len()
        );

        for field in &fields {
            assert!(
                !has_opaque_run(field),
                "{field:?} contains a run long enough to be a credential"
            );
            assert!(
                !has_userinfo(field),
                "{field:?} embeds credentials in a URL's userinfo"
            );
            assert!(
                !query_names_a_credential(field),
                "{field:?} carries a query parameter named like a credential; a suggested \
                 endpoint puts its query in every config file, shell history and \
                 `web_lookup` destination string it ever produces"
            );
        }
    }

    /// Whether `text` holds an unbroken run of key-ish characters long enough to
    /// be a real key.
    ///
    /// Two lengths because there are two shapes. A bare token — base64, hex, an
    /// `sk_live_…` — is alphanumeric throughout, and nothing legitimate here
    /// reaches twenty such characters between separators. A *segmented* key —
    /// a UUID, `sk-ant-…`, a dotted JWT — would slip a split that treats `-`
    /// and `.` as separators, so those are measured as run-internal instead;
    /// that raises the ceiling, because `api.search.brave.com` and
    /// `X-Subscription-Token` are both exactly twenty characters that way, and
    /// the segmented threshold sits above them rather than under a UUID's
    /// thirty-six.
    fn has_opaque_run(text: &str) -> bool {
        const BARE_RUN: usize = 20;
        const SEGMENTED_RUN: usize = 24;

        let bare = text
            .split(|c: char| !(c.is_ascii_alphanumeric() || c == '_'))
            .any(|run| run.len() >= BARE_RUN);
        let segmented = text
            .split(|c: char| !(c.is_ascii_alphanumeric() || matches!(c, '_' | '-' | '.')))
            .any(|run| run.len() >= SEGMENTED_RUN);
        bare || segmented
    }

    /// Whether `text` is a URL carrying userinfo before its host — the shape a
    /// credential takes when it hides inside an endpoint.
    ///
    /// **Any** non-empty userinfo, not just `user:pass@`: a bare-token userinfo
    /// (`https://a1b2c3tok@kagi.com/…`) is the form an API key actually takes
    /// when someone pastes one into a URL, and it carries no colon at all.
    fn has_userinfo(text: &str) -> bool {
        url_parts(text).is_some_and(|(authority, _query)| {
            matches!(authority.split_once('@'), Some((userinfo, _)) if !userinfo.is_empty())
        })
    }

    /// Whether `text` is a URL whose query carries a parameter *named* like a
    /// credential.
    ///
    /// Name-based, and the value is never read — the same rule, and the same
    /// list, as the `/web setup` preview's warning
    /// (`runtime::endpoint_query_names_a_credential`). The list is shared rather
    /// than mirrored: a warning and a gate disagreeing about what a credential
    /// is named is how one of them ends up wrong.
    fn query_names_a_credential(text: &str) -> bool {
        let Some((_authority, query)) = url_parts(text) else {
            return false;
        };
        query
            .split(['&', ';'])
            .filter_map(|pair| pair.split('=').next())
            .any(|name| {
                let name = name.trim().to_ascii_lowercase();
                crate::runtime::CREDENTIAL_QUERY_KEYS.contains(&name.as_str())
            })
    }

    /// The authority and the query of `text`, when it is shaped like a URL.
    ///
    /// A hand-split rather than a URL parse, for the reason the whole sweep is
    /// shape-based: what has to be caught is the string a human pasted, which
    /// may well not parse.
    fn url_parts(text: &str) -> Option<(&str, &str)> {
        let (_scheme, after) = text.split_once("://")?;
        let authority = after.split(['/', '?', '#']).next().unwrap_or("");
        let query = after
            .split_once('?')
            .map_or("", |(_, rest)| rest.split('#').next().unwrap_or(""));
        Some((authority, query))
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
