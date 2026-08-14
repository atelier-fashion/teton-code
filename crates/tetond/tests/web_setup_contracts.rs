//! REQ-572 AC-8 / BR-9, re-pointed by REQ-573: **every backend the product
//! suggests is one the shipped request builder can actually drive** (TASK-137).
//!
//! The motivating defect is on the record: REQ-563 shipped a Bearer-only search
//! credential header while its own spec named example backends that want three
//! different shapes, so two of the three "suggestions" were endpoints this
//! daemon could reach and never authenticate against. BUG-165 replaced the fixed
//! header with the `[web] search_auth` template; what stops the same class of
//! defect returning is not the template but *this file* — a suggestion is only a
//! suggestion once a test drives the production builder against it.
//!
//! ## Where the suggestion list comes from (REQ-573 BR-1/BR-4)
//!
//! There is exactly one: [`tetond::web_setup_catalog::suggestion_catalog`]. The
//! daemon hands it to clients on `web/setup_plan` and the CLI renders what it
//! was handed over RPC, so this suite reads no client source text — it
//! enumerates the typed catalog directly, and a fourth entry added there turns
//! [`every_suggestion_has_a_contract_and_every_contract_a_suggestion`] red until
//! the backend is driven here. The one file still read as bytes is the daemon's
//! own bundled guide, because prose cannot be enumerated; it is checked
//! **against** the catalog, in both directions ([`the_bundled_guide_and_the_catalog_agree`]).
//!
//! Before REQ-573 this suite parsed `ENDPOINT_HELP` out of the CLI crate's
//! source with `include_str!`. That is gone with the constant it parsed: a
//! cross-crate source parse is a gate that fails open the day someone renames
//! the thing it looks for (architecture ADR-A).
//!
//! ## Expectations are written down, not derived (LESSON-512)
//!
//! [`CONTRACTS`] pins the header **name and value** each backend documents, by
//! hand, keyed by catalog `id`. Deriving them by running `auth_template` through
//! the production parser would assert only that the code agrees with itself —
//! and agreeing with itself is precisely what the Bearer-only daemon did. The
//! zip is exhaustive both ways: a catalog entry with no row fails, and a row
//! with no catalog entry fails as a stale table.
//!
//! ## AC → test map
//!
//! | AC | Test |
//! |----|------|
//! | REQ-573 AC-3 (typed enumeration, both ways) | [`every_suggestion_has_a_contract_and_every_contract_a_suggestion`] |
//! | REQ-573 AC-4 (guide ↔ catalog, bidirectional) | [`the_bundled_guide_and_the_catalog_agree`] |
//! | AC-8 (request shape, per backend) | [`every_suggested_backend_drives_the_production_search_request`] |
//! | AC-8 (auth header shape, per backend) | [`every_suggested_backend_gets_the_header_it_documents`] |
//! | BR-9 (a suggestion is a config a user could write) | [`every_suggested_backend_is_a_config_this_daemon_would_load`] |
//! | REQ-573 AC-5 (the keyless one takes no credential) | [`the_keyless_suggestion_needs_no_credential_anywhere`] |
//!
//! ## What "the production builder" means here, exactly
//!
//! * **The request**: `search_request` is private to `egress::lookup`, and it
//!   is reached the only way anything reaches it — through [`Egress::lookup`],
//!   with a recording transport behind the choke point. What is asserted is the
//!   `TransportRequest` the daemon would have put on the wire: its method, its
//!   URL, and its headers.
//! * **The credential**: `DaemonRuntime::search_auth` is private, and what it
//!   does with a template is exactly two public calls —
//!   `WebConfig::search_auth_shape()` then `SearchAuthShape::header_value()`.
//!   Those two are driven here on the same `WebConfig` a user would have
//!   written. The *binding* of that header to the endpoint's origin is
//!   `HttpTransport`'s and is unit-tested there (`outbound_headers` is
//!   crate-private); what this file owns is the shape, which is the half the
//!   backends disagree about.

use std::collections::BTreeSet;
use std::sync::{Arc, Mutex};

use async_trait::async_trait;

use teton_core::config::{Config, WebConfig, WebTier};
use teton_providers::transport::{
    HttpMethod, Transport, TransportError, TransportRequest, TransportResponse,
};

use teton_protocol::methods::WebBackendSuggestion;
use teton_protocol::SessionId;
use tetond::egress::{
    Authorship, Egress, HttpTransport, LookupContext, LookupRequest, NoopSink, RedactionGate,
    RedactionVerdict, TaintView,
};
use tetond::web_setup_catalog::suggestion_catalog;

// ---------------------------------------------------------------------------
// The shipped prose, read from the file that ships
// ---------------------------------------------------------------------------

/// The daemon's bundled setup guide — the same bytes `build_system_prompt`
/// embeds (BR-2), not a copy. The only `include_str!` in the suite, and it
/// reads a file inside this crate.
const BUNDLED_GUIDE: &str = include_str!("../src/harness/self_config.md");

// ---------------------------------------------------------------------------
// The contracts
// ---------------------------------------------------------------------------

/// The fixture secret. Planted so an assertion about *where* it appears is a
/// search for a string nothing else could have produced.
const SECRET: &str = "fixture-search-key-9f3a";

/// What a suggested backend does with a credential — written by hand, never
/// computed from the catalog's `auth_template` (LESSON-512).
enum Credential {
    /// Takes none at all: no key reference, no header, nothing to leak.
    Keyless,
    /// Rides `name`, valued `value`, with [`SECRET`] in the position this
    /// particular backend documents.
    Header {
        /// Lowercased: header names are case-insensitive on the wire and the
        /// shape parser lowercases them.
        name: &'static str,
        /// The full header value. This is the assertion a Bearer-only daemon
        /// fails for Brave and Kagi both, having passed Kagi's header *name*.
        value: &'static str,
    },
}

/// One catalog entry's expected wire behaviour, keyed by its stable `id`.
struct Contract {
    /// The catalog `id` this row pins. Not the label — labels are display text
    /// and may be reworded without any wire consequence.
    id: &'static str,
    credential: Credential,
}

/// The expectation table: one row per suggestion the daemon catalog offers.
///
/// Spelled out rather than derived. Every value is a fact about a third party's
/// API that nothing in this repository can regenerate, so the only honest guard
/// is a second, independent spelling that must agree with the first.
const CONTRACTS: &[Contract] = &[
    Contract {
        id: "searxng",
        // The keyless one, and the reason the flow asks "does this backend need
        // an API key?" at all.
        credential: Credential::Keyless,
    },
    Contract {
        id: "brave",
        credential: Credential::Header {
            name: "x-subscription-token",
            // Brave's header carries the bare key — no scheme word. A daemon
            // that sent `Bearer <key>` here would be refused by the backend,
            // which is exactly the failure BUG-165 was filed for.
            value: "fixture-search-key-9f3a",
        },
    },
    Contract {
        id: "kagi",
        credential: Credential::Header {
            name: "authorization",
            value: "Bot fixture-search-key-9f3a",
        },
    },
];

/// The header a config that names no `search_auth` inherits — BUG-165's
/// continuity promise, and the catalog's `default_auth_template` in wire form.
/// Not a catalog entry, so deliberately not a [`CONTRACTS`] row; pinned beside
/// them in [`every_suggested_backend_gets_the_header_it_documents`].
const DEFAULT_CREDENTIAL: Credential = Credential::Header {
    name: "authorization",
    value: "Bearer fixture-search-key-9f3a",
};

/// A representative endpoint for the backend nothing in the catalog names — the
/// case [`DEFAULT_CREDENTIAL`] covers.
const UNNAMED_BACKEND_ENDPOINT: &str = "https://search.example.org/api";

impl Credential {
    /// The shape assertions, run against the `[web]` table `web` — the two
    /// public calls the daemon's private `search_auth` makes on it before
    /// handing the endpoint-bound transport a header and a resolved secret.
    fn assert_shape_matches(&self, id: &str, web: &WebConfig) {
        let shape = web.search_auth_shape();
        match self {
            Self::Keyless => {
                assert!(
                    web.search_key_ref.is_none(),
                    "{id}: a keyless suggestion must reference no key — with one set the \
                     daemon resolves a secret and attaches it to a backend that never \
                     asked for one"
                );
            }
            Self::Header { name, value } => {
                let shape = shape.unwrap_or_else(|| {
                    panic!(
                        "{id}: a suggested template must parse to a shape — a suggestion \
                         the daemon reads as `attach no credential` is a suggestion that \
                         401s"
                    )
                });
                assert_eq!(
                    shape.header, *name,
                    "{id}: the credential must ride the header the backend documents"
                );
                assert_eq!(
                    shape.header_value(SECRET),
                    *value,
                    "{id}: the secret must sit where the backend documents it (this is \
                     the assertion a Bearer-only daemon fails)"
                );
            }
        }
    }
}

/// The `[web]` table a user following this suggestion would end up with.
///
/// The *inputs* come from the catalog — endpoint and template are what is under
/// test — while whether a key reference is present comes from the expectation
/// table, so a catalog that flipped `needs_key` changes the assertion's subject
/// rather than the assertion.
fn config_for(backend: &WebBackendSuggestion, contract: &Contract) -> WebConfig {
    WebConfig {
        tier: WebTier::Search,
        search_endpoint: Some(backend.endpoint.clone()),
        search_key_ref: matches!(contract.credential, Credential::Header { .. })
            .then(|| "keychain://teton/web-search".to_owned()),
        search_auth: backend.auth_template.clone(),
        ..WebConfig::default()
    }
}

// ---------------------------------------------------------------------------
// The enumeration (AC-8's CI gate, over the typed catalog)
// ---------------------------------------------------------------------------

/// Every catalog entry beside the contract row that pins it.
///
/// Panics on drift in **either** direction, so every test below inherits the
/// gate: an unpinned suggestion cannot reach a green assertion by being skipped.
fn contracts_paired_with_the_catalog() -> Vec<(WebBackendSuggestion, &'static Contract)> {
    let catalog = suggestion_catalog();

    let mut paired = Vec::new();
    for backend in catalog.backends {
        let Some(contract) = CONTRACTS.iter().find(|c| c.id == backend.id) else {
            panic!(
                "AC-8: `{id}` is a suggestion with no contract test. The daemon catalog \
                 (crates/tetond/src/web_setup_catalog.rs) offers it to every client that \
                 asks for a setup plan, and nothing here drives the production request \
                 builder against it — a backend the product names and CI never drives is \
                 the BUG-165 defect returning. Add a `Contract` row for `{id}` to \
                 `CONTRACTS` in this file.",
                id = backend.id
            );
        };
        paired.push((backend, contract));
    }

    let offered: BTreeSet<&str> = paired.iter().map(|(b, _)| b.id.as_str()).collect();
    for contract in CONTRACTS {
        assert!(
            offered.contains(contract.id),
            "the `CONTRACTS` row for `{}` pins a suggestion the daemon catalog no longer \
             offers — this table is stale, not the catalog. If the entry was removed \
             deliberately, remove its row here too; do not re-add it to the catalog to \
             make this pass.\noffered: {offered:?}",
            contract.id
        );
    }

    paired
}

/// **A suggestion with no contract fails, and a contract with no suggestion
/// fails** (REQ-573 AC-3).
///
/// This is the enforcement half of AC-8, and after REQ-573 it enumerates the one
/// definition rather than parsing anyone's source text. Not "the three we know
/// about are covered" — that claim decays the moment a fourth is added — but
/// "everything the daemon offers is covered", checked against the daemon.
#[test]
fn every_suggestion_has_a_contract_and_every_contract_a_suggestion() {
    let paired = contracts_paired_with_the_catalog();

    // Non-vacuity: an emptied catalog must not make every loop below a no-op.
    // (The stale-row half above catches it too; this says so at the altitude a
    // reader looks for it.)
    assert!(
        paired.len() >= 3,
        "the daemon catalog must still offer the backends it documents; paired: {:?}",
        paired.iter().map(|(b, _)| &b.id).collect::<Vec<_>>()
    );

    for (backend, contract) in &paired {
        // The catalog's own answer to "does this need a key?" must agree with
        // the independently written expectation — the two ways to be wrong are
        // asking for a key with nowhere to put it, and skipping the question
        // for a backend that will 401.
        assert_eq!(
            backend.needs_key,
            matches!(contract.credential, Credential::Header { .. }),
            "`{}` is offered with needs_key={} and its contract row says otherwise; one \
             of the two is wrong about the backend",
            backend.id,
            backend.needs_key
        );
        assert_eq!(
            backend.auth_template.is_some(),
            matches!(contract.credential, Credential::Header { .. }),
            "`{}` is offered with auth_template={:?} and its contract row says otherwise",
            backend.id,
            backend.auth_template
        );
    }

    // And the fixture values really are the fixture: a row whose expected value
    // lost the sentinel would assert about a secret nothing plants.
    for contract in CONTRACTS {
        if let Credential::Header { value, .. } = contract.credential {
            assert!(
                value.contains(SECRET),
                "`{}`'s expected header value {value:?} does not carry the fixture \
                 secret, so the assertion it drives proves nothing",
                contract.id
            );
        }
    }
}

// ---------------------------------------------------------------------------
// Guide ↔ catalog (REQ-573 AC-4 / BR-5)
// ---------------------------------------------------------------------------

/// Every backtick-quoted span in `text` that is a **credential-header
/// template**: `Header-Name: …{key}…`, the grammar `search_auth_shape` parses.
///
/// The `{key}` marker alone is not a suggestion — the guide quotes it on its own
/// while explaining what it means — so the header-name half is required. That is
/// also what keeps this parse honest in the other direction: anything that would
/// actually be *accepted* as a `search_auth` value is caught.
fn suggested_auth_templates(text: &str) -> BTreeSet<String> {
    text.split('`')
        // Odd-indexed spans are the ones *between* backticks.
        .skip(1)
        .step_by(2)
        .filter(|span| span.contains("{key}"))
        .filter(|span| {
            span.split_once(": ").is_some_and(|(header, value)| {
                !header.is_empty()
                    && header
                        .chars()
                        .all(|c| c.is_ascii_alphanumeric() || c == '-')
                    && value.contains("{key}")
            })
        })
        .map(str::to_owned)
        .collect()
}

/// **The bundled guide and the daemon catalog say the same thing, in both
/// directions** (REQ-573 AC-4).
///
/// The guide is authored prose under a hard byte ceiling, so it is hand-written
/// and CI-checked rather than generated (ADR-C). What that costs is this test,
/// and what it buys is that drift fails whichever side moves: a template added
/// to the guide with no catalog entry fails, and a catalog template the guide
/// never mentions fails too. Each message names the side to update, because the
/// answer differs — one is a doc edit, the other is a product change.
#[test]
fn the_bundled_guide_and_the_catalog_agree() {
    let catalog = suggestion_catalog();
    let in_guide = suggested_auth_templates(BUNDLED_GUIDE);
    assert!(
        in_guide.len() >= 3,
        "the guide must still name the auth templates it documents; found: {in_guide:?}"
    );

    let in_catalog: BTreeSet<&str> = catalog
        .backends
        .iter()
        .filter_map(|b| b.auth_template.as_deref())
        .collect();
    // Non-vacuity for the catalog → guide direction below: a catalog whose
    // every entry went keyless would satisfy it by having nothing to check.
    assert!(
        !in_catalog.is_empty(),
        "the catalog must still offer at least one keyed backend"
    );

    // Guide → catalog: a shape the prompt tells a model to suggest, that the
    // daemon does not offer, is a suggestion no client can render.
    for template in &in_guide {
        assert!(
            in_catalog.contains(template.as_str()) || *template == catalog.default_auth_template,
            "the bundled guide (crates/tetond/src/harness/self_config.md) documents \
             `{template}` and the daemon catalog offers no backend with that template \
             and no such default. Update the catalog \
             (crates/tetond/src/web_setup_catalog.rs) if the backend is real, or the \
             guide if the line is stale.\ncatalog: {in_catalog:?} + default \
             `{}`",
            catalog.default_auth_template
        );
    }

    // Catalog → guide: a backend the daemon offers that the guide never names is
    // one the model cannot talk a user through, which is the half a one-way
    // check used to miss.
    for template in &in_catalog {
        assert!(
            in_guide.contains(*template),
            "the daemon catalog offers `{template}` and the bundled guide \
             (crates/tetond/src/harness/self_config.md) does not name it — the model \
             answering a setup question has never heard of it. Add it to the guide line \
             (mind the byte ceiling the prompt-size test pins).\nguide: {in_guide:?}"
        );
    }
    assert!(
        in_guide.contains(&catalog.default_auth_template),
        "the catalog's default template `{}` is not in the bundled guide, so the shape \
         offered for every unnamed backend is one the guide never explains. Update the \
         guide line, or the catalog's `default_auth_template` if the default \
         changed.\nguide: {in_guide:?}",
        catalog.default_auth_template
    );

    // The keyless backend is named in prose rather than as a template, and needs
    // the same agreement: the guide's name must be a catalog id that takes no
    // key.
    let keyless = BUNDLED_GUIDE
        .split(" needs none")
        .next()
        .and_then(|before| before.split_whitespace().next_back())
        .map(str::to_lowercase)
        .expect("the guide names its keyless backend");
    // Non-vacuity: the parse read the backend the guide actually names, not an
    // empty string that trivially matches nothing.
    assert_eq!(keyless, "searxng", "the keyless parse read: {keyless:?}");
    let entry = catalog
        .backends
        .iter()
        .find(|b| b.id == keyless)
        .unwrap_or_else(|| {
            panic!(
                "the guide names `{keyless}` as its keyless backend and the catalog has \
                 no such entry. Update the guide if the suggestion was dropped, or the \
                 catalog if it was renamed."
            )
        });
    assert!(
        !entry.needs_key,
        "the guide says `{keyless}` needs no key and the catalog says it does; one of \
         the two is about to hand a user a 401 or a pointless key prompt"
    );

    // SearxNG's `format=json` is not decoration — an instance answers HTML
    // without it and the parse then finds nothing — so the shape is pinned on
    // both sides.
    const ENDPOINT_SHAPE: &str = "/search?format=json";
    assert!(
        BUNDLED_GUIDE.contains(ENDPOINT_SHAPE),
        "the bundled guide no longer shows the `{ENDPOINT_SHAPE}` endpoint shape, so a \
         user following it configures a SearxNG instance that answers HTML. Restore the \
         guide line."
    );
    assert!(
        entry.endpoint.ends_with(ENDPOINT_SHAPE),
        "the catalog's `{keyless}` endpoint `{}` no longer ends `{ENDPOINT_SHAPE}` while \
         the guide still documents that shape. Update the catalog if the backend \
         changed, and the guide with it.",
        entry.endpoint
    );
}

// ---------------------------------------------------------------------------
// The contracts themselves
// ---------------------------------------------------------------------------

/// **Every suggested backend is driven by the production request builder, and
/// gets the request its documentation describes.**
///
/// A GET, the terms carried as `q`, and the endpoint's own query parameters
/// preserved — which is the whole of SearxNG's contract, since `format=json` is
/// what makes its answer parseable and a builder that dropped it would return
/// HTML to a JSON reader.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn every_suggested_backend_drives_the_production_search_request() {
    for (backend, _) in contracts_paired_with_the_catalog() {
        let id = &backend.id;
        let sent = search_request_for(&backend.endpoint).await;
        assert_eq!(
            sent.len(),
            1,
            "{id}: the suggested endpoint must produce exactly one request"
        );
        let request = &sent[0];
        assert_eq!(
            request.method,
            HttpMethod::Get,
            "{id}: the search contract is a GET"
        );
        assert!(
            request.body.is_empty(),
            "{id}: and carries no body: {:?}",
            request.body
        );
        assert!(
            request.url.contains("q=rust"),
            "{id}: the terms must ride as `q`: {}",
            request.url
        );
        // The endpoint's own path and parameters survive verbatim — the
        // property SearxNG's `?format=json` depends on, asserted for every
        // backend so a builder that special-cased one of them is visible.
        let (base, params) = backend
            .endpoint
            .split_once('?')
            .map_or((backend.endpoint.as_str(), ""), |p| p);
        assert!(
            request.url.starts_with(base),
            "{id}: the suggested endpoint's path must survive: {}",
            request.url
        );
        for param in params.split('&').filter(|p| !p.is_empty()) {
            assert!(
                request.url.contains(param),
                "{id}: the endpoint's own `{param}` must survive: {}",
                request.url
            );
        }
        assert!(
            request
                .headers
                .iter()
                .any(|(name, value)| name == "accept" && value == "application/json"),
            "{id}: a search asks for JSON: {:?}",
            request.headers
        );
        // The credential is not composed here, for any backend: it is attached
        // by the endpoint-bound transport, which is what keeps it off a page
        // fetch (REQ-563 BR-7). The next test is where the shape is asserted.
        assert!(
            !request
                .headers
                .iter()
                .any(|(_, value)| value.contains(SECRET)),
            "{id}: the request builder must compose no credential: {:?}",
            request.headers
        );
    }
}

/// **Every suggested backend gets the credential header its documentation
/// describes** — name and value, with the secret in the position that backend
/// puts it.
///
/// The two calls behind [`Credential::assert_shape_matches`] are, verbatim, what
/// `DaemonRuntime::search_auth` makes with a resolved secret. A Bearer-only
/// daemon passes the `kagi` header *name* and fails every value assertion —
/// which is the discrimination this test exists to have.
#[test]
fn every_suggested_backend_gets_the_header_it_documents() {
    for (backend, contract) in contracts_paired_with_the_catalog() {
        let web = config_for(&backend, contract);
        contract.credential.assert_shape_matches(&backend.id, &web);

        // And the endpoint-bound transport the daemon builds from that pair
        // accepts it — the last step before the wire.
        if let Credential::Header { name, value } = contract.credential {
            assert!(
                HttpTransport::for_lookup_with_endpoint_auth(
                    &backend.endpoint,
                    vec![(name.to_owned(), value.to_owned())],
                )
                .is_ok(),
                "{}: the lookup transport must bind this credential to this endpoint",
                backend.id
            );
        }
    }

    // The backend the catalog does not name rides its `default_auth_template`,
    // reached by a config with no `search_auth` key at all — BUG-165's
    // continuity promise, that every pre-template config keeps working.
    let default = WebConfig {
        tier: WebTier::Search,
        search_endpoint: Some(UNNAMED_BACKEND_ENDPOINT.to_owned()),
        search_key_ref: Some("keychain://teton/web-search".to_owned()),
        search_auth: None,
        ..WebConfig::default()
    };
    DEFAULT_CREDENTIAL.assert_shape_matches("default", &default);
    // The catalog offers clients that same shape as a literal, so the two
    // spellings of "what an unnamed backend gets" must agree.
    let offered_default = WebConfig {
        search_auth: Some(suggestion_catalog().default_auth_template),
        ..default.clone()
    };
    assert_eq!(
        offered_default.search_auth_shape(),
        default.search_auth_shape(),
        "the catalog's `default_auth_template` ({:?}) parses to a different shape than \
         the one a config with no `search_auth` inherits — a user who accepts the \
         offered default would get a different header than one who leaves the key out",
        offered_default.search_auth
    );
}

/// **BR-9's other half: a suggestion is a config a user could actually write.**
///
/// A backend whose documented shape this daemon refuses at config load is not a
/// suggestion, it is a trap — the user follows the walkthrough, the commit is
/// rejected, and the product has recommended something it forbids.
#[test]
fn every_suggested_backend_is_a_config_this_daemon_would_load() {
    for (backend, contract) in contracts_paired_with_the_catalog() {
        let config = Config {
            web: config_for(&backend, contract),
            ..Config::default()
        };
        assert!(
            config.validate().is_ok(),
            "{}: the suggested shape must be one `Config::validate` accepts: {:?}",
            backend.id,
            config.validate().err()
        );
        // And the secret is nowhere in the document — the reference is
        // (BR-6/REQ-563 BR-7). Asserted here because this is the one test that
        // renders a suggested backend's whole config.
        let document = config.to_toml().expect("the suggested config renders");
        assert!(
            !document.contains(SECRET),
            "{}: a rendered config must carry a reference, never a key:\n{document}",
            backend.id
        );
        if let Credential::Header { .. } = contract.credential {
            assert!(
                document.contains("keychain://teton/web-search"),
                "{}: the rendered config must carry the keychain reference the user \
                 configured:\n{document}",
                backend.id
            );
        }
    }
}

/// **The keyless suggestion takes no credential anywhere** (REQ-573 AC-5).
///
/// Asserted separately because every other backend's contract is about *which*
/// header the key rides, and this one's is that no key is resolved, no reference
/// is written, and nothing on the wire could carry one. A catalog that gave
/// SearxNG a template would make the flow prompt for a key the instance ignores
/// — and would put a secret in reach of a backend that never asked for one.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_keyless_suggestion_needs_no_credential_anywhere() {
    let paired = contracts_paired_with_the_catalog();
    let (backend, contract) = paired
        .iter()
        .find(|(_, c)| matches!(c.credential, Credential::Keyless))
        .expect("the catalog must still offer a backend a user can reach without a key");

    // The catalog says so.
    assert!(!backend.needs_key, "{}: expected keyless", backend.id);
    assert_eq!(
        backend.auth_template, None,
        "{}: a keyless backend wants no header at all — an absent template here is the \
         backend's answer, not a missing fact",
        backend.id
    );

    // The config a user writes for it says so: no key reference, and
    // `Config::validate` takes it. That absent reference is the gate — the
    // daemon's `search_auth` returns before a shape is ever consulted, so the
    // lookup transport is built credential-free.
    let web = config_for(backend, contract);
    assert!(web.search_key_ref.is_none(), "{}", backend.id);
    let config = Config {
        web,
        ..Config::default()
    };
    assert!(
        config.validate().is_ok(),
        "{}: a keyless search config must load — requiring a key here would forbid the \
         one backend a user can stand up themselves: {:?}",
        backend.id,
        config.validate().err()
    );

    // And the request the production builder forms for it carries no credential
    // header: not the one this backend would have used, and not any header a
    // *different* suggestion documents.
    let sent = search_request_for(&backend.endpoint).await;
    let request = sent
        .first()
        .expect("the keyless endpoint must be reachable");
    let credential_headers: BTreeSet<&str> = CONTRACTS
        .iter()
        .filter_map(|c| match c.credential {
            Credential::Header { name, .. } => Some(name),
            Credential::Keyless => None,
        })
        .collect();
    assert!(
        !credential_headers.is_empty(),
        "the credential-header sweep must have something to sweep for"
    );
    for (name, value) in &request.headers {
        assert!(
            !credential_headers.contains(name.as_str()),
            "{}: the keyless suggestion's request carries `{name}: {value}` — a \
             credential header for a backend that authenticates nothing",
            backend.id
        );
    }
}

// ---------------------------------------------------------------------------
// Fixtures
// ---------------------------------------------------------------------------

/// Everything the daemon would have put on the wire for a search against
/// `endpoint`, driven through the production choke point.
async fn search_request_for(endpoint: &str) -> Vec<TransportRequest> {
    let capture = CaptureTransport::default();
    let egress = Egress::new(capture.clone(), Vec::new(), Arc::new(NoopSink))
        // BR-14: without a scan installed a search is a block, not a skip, so
        // the contract could not be observed at all. This gate forwards, which
        // is the permissive control — what is under test here is the request,
        // not the scan.
        .with_search_redaction_gate(Arc::new(ForwardingGate) as Arc<dyn RedactionGate>);
    let flags = Untainted;
    let ctx = LookupContext::new(SessionId::from("contracts"), &flags, &allow_any_host)
        .with_search_endpoint(endpoint);

    egress
        .lookup(
            &LookupRequest::search("rust pin semantics", Authorship::UserPasted),
            &ctx,
        )
        .await;

    capture.sent()
}

/// A transport that records every request instead of sending it.
#[derive(Clone, Default)]
struct CaptureTransport {
    sent: Arc<Mutex<Vec<TransportRequest>>>,
}

impl CaptureTransport {
    fn sent(&self) -> Vec<TransportRequest> {
        self.sent.lock().unwrap().clone()
    }
}

#[async_trait]
impl Transport for CaptureTransport {
    async fn execute(
        &self,
        request: TransportRequest,
    ) -> Result<TransportResponse, TransportError> {
        self.sent.lock().unwrap().push(request);
        Ok(TransportResponse {
            status: 200,
            location: None,
            body: Box::pin(futures::stream::once(async { Ok(b"{}".to_vec()) })),
        })
    }
}

/// A session that has read no boundary content.
struct Untainted;

impl TaintView for Untainted {
    fn is_tainted(&self, _session: &SessionId) -> bool {
        false
    }

    fn is_overridden(&self, _session: &SessionId) -> bool {
        false
    }
}

/// The permissive scan control: what is under test in this file is the request
/// and the header, not the gate.
struct ForwardingGate;

#[async_trait]
impl RedactionGate for ForwardingGate {
    async fn scan(&self, _payload: &str) -> RedactionVerdict {
        RedactionVerdict::clean()
    }
}

fn allow_any_host(_host: &str) -> bool {
    true
}
