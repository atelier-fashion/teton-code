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
//! the backend is driven here.
//!
//! Two files are still read as bytes, both of them prose, because prose cannot
//! be enumerated: the daemon's own bundled guide
//! ([`the_bundled_guide_and_the_catalog_agree`]) and the README's backend rows
//! ([`the_readme_backend_rows_and_the_catalog_agree`]). Neither is a source of
//! truth — each is checked **against** the catalog, in both directions, and each
//! failure names which side to edit.
//!
//! Before REQ-573 this suite parsed `ENDPOINT_HELP` out of the CLI crate's
//! source with `include_str!`. That is gone with the constant it parsed: a
//! cross-crate source parse is a gate that fails open the day someone renames
//! the thing it looks for (architecture ADR-A). The README parse is the same
//! technique and does not inherit that failure mode, because it is anchored and
//! **fails closed**: a missing anchor panics with the instruction to re-anchor,
//! rather than finding no rows and passing.
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
//! | REQ-573 AC-5 / BR-5 (README rows ↔ catalog, bidirectional) | [`the_readme_backend_rows_and_the_catalog_agree`] |
//! | AC-8 (request shape, per backend) | [`every_suggested_backend_drives_the_production_search_request`] |
//! | AC-8 (auth header shape, per backend) | [`every_suggested_backend_gets_the_header_it_documents`] |
//! | BR-9 (a suggestion is a config a user could write) | [`every_suggested_backend_is_a_config_this_daemon_would_load`] |
//! | REQ-573 BR-4, the keyless entry's leg (REQ-572 AC-8) | [`the_keyless_suggestion_needs_no_credential_anywhere`] |
//! | BUG-165 continuity (the unnamed backend's default) | [`the_unnamed_backend_default_is_a_whole_contract`] |
//! | REQ-577 BR-2 / AC-7 (providers topic ↔ recipe catalog, bidirectional) | [`the_providers_topic_and_the_recipe_catalog_agree`] |
//! | REQ-577 BR-2 (web topic ↔ suggestion catalog auth shapes) | [`the_web_topic_and_the_suggestion_catalog_agree`] |
//! | REQ-577 BR-1 / BR-2 / AC-7 (bundled guide recipes ↔ recipe catalog, bidirectional) | [`the_bundled_guide_and_the_recipe_catalog_agree`] |
//! | REQ-577 BR-2 / AC-7 (README walkthrough ↔ recipe catalog, bidirectional) | [`the_readme_recipes_and_the_catalog_agree`] |
//!
//! ## A third prose surface, on a second catalog (REQ-577)
//!
//! The `teton_docs` topics are the same kind of artifact as the guide and the
//! README rows — hand-written prose carrying third-party facts — so they are
//! checked here, in the file that already owns that pattern, rather than in a
//! suite of their own. `providers.md` is gated against
//! [`tetond::provider_recipes::recipe_catalog`] and `web.md` against the search
//! catalog this file was built for. Two catalogs, one rule: a fact spelled in
//! prose must be a fact some typed source ships, and the reverse.
//!
//! The same REQ puts recipe facts into the two older surfaces as well — the
//! guide's resident recipe line and the README's provider walkthrough — so each
//! of those now carries two gates: the web-setup one it was built for, and
//! [`the_bundled_guide_and_the_recipe_catalog_agree`] /
//! [`the_readme_recipes_and_the_catalog_agree`] beside it. What every one of
//! them pins is the **third-party** half — an endpoint, a header shape, a model
//! id. Provider ids are the user's own namespace and stay deliberately free.
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
use teton_core::entities::ProviderKind;
use teton_providers::transport::{
    HttpMethod, Transport, TransportError, TransportRequest, TransportResponse,
};

use teton_protocol::methods::WebBackendSuggestion;
use teton_protocol::SessionId;
use tetond::egress::{
    Authorship, Egress, HttpTransport, LookupContext, LookupRequest, NoopSink, RedactionGate,
    RedactionVerdict, TaintView,
};
use tetond::provider_recipes::recipe_catalog;
use tetond::web_setup_catalog::suggestion_catalog;

// ---------------------------------------------------------------------------
// The shipped prose, read from the file that ships
// ---------------------------------------------------------------------------

/// The daemon's bundled setup guide — the same bytes `build_system_prompt`
/// embeds (BR-2), not a copy.
const BUNDLED_GUIDE: &str = include_str!("../src/harness/self_config.md");

/// The shipped README, whose backend rows BR-5 puts under CI.
///
/// Reached across the crate boundary deliberately, the way `boundary_coverage.rs`
/// reaches `../../teton-core/src/boundary.rs`: the rows are prose about the
/// daemon's catalog, and the pairing is only checkable from a place that can see
/// both.
const README: &str = include_str!("../../../README.md");

/// The `teton_docs` providers topic — the same bytes `DocsTool` serves
/// (REQ-577 BR-2), not a copy of them.
const PROVIDERS_TOPIC: &str = include_str!("../src/harness/docs/providers.md");

/// The `teton_docs` web topic, gated on the search catalog's auth shapes.
const WEB_TOPIC: &str = include_str!("../src/harness/docs/web.md");

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

/// The `[web]` table for a backend the catalog does not name.
///
/// A real endpoint, a key reference, and **no** `search_auth` at all — which is
/// the shape every pre-BUG-165 config has, and the reason the default template
/// exists. Written once because two tests drive it: the header shape it
/// inherits, and the whole contract around it.
fn unnamed_backend_config() -> WebConfig {
    WebConfig {
        tier: WebTier::Search,
        search_endpoint: Some(UNNAMED_BACKEND_ENDPOINT.to_owned()),
        search_key_ref: Some("keychain://teton/web-search".to_owned()),
        search_auth: None,
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
// README rows ↔ catalog (REQ-573 AC-5 / BR-5)
// ---------------------------------------------------------------------------

/// The line that introduces the README's backend table.
///
/// The region parse is anchored on it and **panics** when it is gone rather than
/// quietly finding no rows: a parse that fails open is the `ENDPOINT_HELP`
/// mistake wearing a different filename.
const README_TABLE_ANCHOR: &str = "Backends whose shapes are known to work:";

/// The README's backend table — the markdown table that follows the anchor, and
/// nothing else in the file.
///
/// Scoped deliberately. The README quotes a suggested endpoint again in the
/// hand-edit TOML block and names an unrelated provider's endpoint in the remote
/// section; unscoped, either would answer for a row that is not there.
fn readme_backend_rows() -> Vec<&'static str> {
    let (_, after) = README.split_once(README_TABLE_ANCHOR).unwrap_or_else(|| {
        panic!(
            "the README no longer introduces its backend table with \
             `{README_TABLE_ANCHOR}`, so this check has nothing to read. Restore the line, \
             or re-anchor this test on whatever replaced it — do not leave the table \
             unchecked."
        )
    });
    let rows: Vec<&str> = after
        .lines()
        .skip_while(|line| line.trim().is_empty())
        .take_while(|line| line.trim_start().starts_with('|'))
        .collect();
    // Non-vacuity: a header, a separator, and one row per suggested backend.
    assert!(
        rows.len() >= 2 + suggestion_catalog().backends.len(),
        "the README's backend table parsed as {} lines, which is fewer than a header, a \
         separator and one row per suggestion. Either the table moved out from under \
         `{README_TABLE_ANCHOR}` or rows were dropped.\nparsed: {rows:?}",
        rows.len()
    );
    rows
}

/// Every `http(s)` URL in `text`, ended at the first character markdown or prose
/// would put after one.
///
/// Forgiving on purpose: the rows are a hand-written table and their URLs are
/// backtick-quoted inside `|` cells, so the terminator set is what surrounds a
/// URL rather than what a URL may legally contain.
fn http_urls(text: &str) -> Vec<&str> {
    const AFTER_A_URL: [char; 10] = ['`', '|', '"', '\'', '(', ')', '<', '>', ',', ';'];

    let mut found = Vec::new();
    let mut rest = text;
    while let Some(at) = rest.find("http") {
        let candidate = &rest[at..];
        if candidate.starts_with("http://") || candidate.starts_with("https://") {
            let end = candidate
                .find(|c: char| c.is_whitespace() || AFTER_A_URL.contains(&c))
                .unwrap_or(candidate.len());
            found.push(&candidate[..end]);
            rest = &candidate[end..];
        } else {
            rest = &candidate["http".len()..];
        }
    }
    found
}

/// **The README's backend rows and the daemon catalog say the same thing, in
/// both directions** (REQ-573 AC-5, discharging BR-5's README half).
///
/// BR-5 says derived surfaces cannot drift and names the README rows among them.
/// They are the surface a user reads *before* running anything, so a stale row
/// is BUG-165 in its original form: prose offering a header the product does not
/// send. The table stays hand-written — it is prose, with a "keyless" cell no
/// catalog field spells — and is mechanically checked instead, which is the
/// other option BR-5 allows.
#[test]
fn the_readme_backend_rows_and_the_catalog_agree() {
    let catalog = suggestion_catalog();
    let table = readme_backend_rows().join("\n");

    // Catalog → README: a backend the daemon offers whose endpoint or header the
    // table does not show is a row nobody wrote.
    for backend in &catalog.backends {
        assert!(
            table.contains(backend.endpoint.as_str()),
            "the daemon catalog offers `{endpoint}` for `{id}` and the README's backend \
             table does not show it. Edit the README table — the catalog \
             (crates/tetond/src/web_setup_catalog.rs) is the source, and it moves \
             first.\ntable:\n{table}",
            endpoint = backend.endpoint,
            id = backend.id
        );
        if let Some(template) = &backend.auth_template {
            assert!(
                table.contains(template.as_str()),
                "the daemon catalog offers `{template}` for `{id}` and the README's \
                 backend table shows a different header shape or none. Edit the README \
                 table; a row that names the wrong header is exactly what BUG-165 \
                 was.\ntable:\n{table}",
                id = backend.id
            );
        }
    }

    // The default shape is not a row — it is what a backend the catalog does not
    // name inherits — so it is checked against the whole file, where the
    // walkthrough and the hand-edit block both name it.
    assert!(
        README.contains(catalog.default_auth_template.as_str()),
        "the catalog's default template `{}` appears nowhere in the README, so the shape \
         offered for every unnamed backend is one the README never shows. Update the \
         README's walkthrough, or the catalog's `default_auth_template` if the default \
         changed.",
        catalog.default_auth_template
    );

    // README → catalog: a URL in the table that no suggestion offers is a
    // backend this daemon never drives, recommended in print.
    let offered: BTreeSet<&str> = catalog
        .backends
        .iter()
        .map(|b| b.endpoint.as_str())
        .collect();
    let in_table = http_urls(&table);
    assert!(
        in_table.len() >= catalog.backends.len(),
        "the URL parse found {} URLs in a table of {} suggestions, so it is reading less \
         than the table holds: {in_table:?}",
        in_table.len(),
        catalog.backends.len()
    );
    for url in in_table {
        assert!(
            offered.contains(url),
            "the README's backend table shows `{url}` and the daemon catalog offers no \
             backend with that endpoint. Edit the README row if the suggestion was \
             dropped or renamed, or the catalog \
             (crates/tetond/src/web_setup_catalog.rs) if the backend is real and \
             unlisted.\noffered: {offered:?}"
        );
    }
}

// ---------------------------------------------------------------------------
// Guide + README recipes ↔ the recipe catalog (REQ-577 BR-1 / BR-2 / AC-7)
// ---------------------------------------------------------------------------

/// The line that opens the bundled guide's numbered recipe step.
///
/// The recipe facts are asserted **inside this one line**, not against the whole
/// file, so an endpoint that survives only somewhere else in the guide — in a
/// `[web]` example, in a sentence about something adjacent — does not answer for
/// the step a model actually reads when it is composing a `provider add`.
const GUIDE_RECIPE_ANCHOR: &str = "1. `teton provider add";

/// The bundled guide's recipe step, as one line.
///
/// **Panics** when the anchor is gone rather than returning an empty string: a
/// parse that fails open is the `ENDPOINT_HELP` mistake this suite's header
/// describes, and every assertion downstream would then hold over nothing.
fn guide_recipe_line() -> &'static str {
    BUNDLED_GUIDE
        .lines()
        .find(|line| line.trim_start().starts_with(GUIDE_RECIPE_ANCHOR))
        .unwrap_or_else(|| {
            panic!(
                "the bundled guide (crates/tetond/src/harness/self_config.md) no longer \
                 opens its recipe step with `{GUIDE_RECIPE_ANCHOR}`, so this check has \
                 nothing to read. Restore the step, or re-anchor this test on whatever \
                 replaced it — do not leave the recipes unchecked."
            )
        })
}

/// The heading the README's provider walkthrough lives under.
const README_RECIPES_ANCHOR: &str = "### Hooking up an external model";

/// The README's provider walkthrough: the anchored heading through to the next
/// `###`, and nothing else in the file.
///
/// Scoped for the same reason [`readme_backend_rows`] is. The README names
/// `claude-opus-5` again in the local-model prose and shows unrelated URLs in
/// the web section; unscoped, either would answer for a recipe fact that is not
/// there. **Panics** on a missing anchor.
fn readme_recipe_section() -> &'static str {
    let (_, after) = README.split_once(README_RECIPES_ANCHOR).unwrap_or_else(|| {
        panic!(
            "the README no longer carries a `{README_RECIPES_ANCHOR}` heading, so this \
             check has nothing to read. Restore it, or re-anchor this test on whatever \
             replaced it — do not leave the walkthrough unchecked."
        )
    });
    after.split("\n### ").next().unwrap_or(after)
}

/// The fenced `bash` block inside the README's provider walkthrough — the
/// commands a reader copies, which is where a stale vendor fact does its damage.
///
/// **Panics** when the fence is gone: a walkthrough with no commands in it is a
/// section this gate must not silently pass.
fn readme_recipe_commands() -> &'static str {
    let section = readme_recipe_section();
    let (_, after) = section.split_once("```bash\n").unwrap_or_else(|| {
        panic!(
            "the README's `{README_RECIPES_ANCHOR}` section no longer opens a ```bash \
             block, so the commands this gate pins are unreadable. Restore the block, or \
             re-anchor this test — do not leave the commands unchecked.\nsection:\n{section}"
        )
    });
    after.split_once("```").map_or(after, |(block, _)| block)
}

/// The value following every occurrence of `flag ` in `text`, taken to the next
/// whitespace.
///
/// The trailing space in the needle is load-bearing: the README's own comment
/// writes `--model;` in prose, and a needle without it would collect a
/// semicolon and assert about a value nobody wrote.
fn flag_values<'a>(text: &'a str, flag: &str) -> Vec<&'a str> {
    let needle = format!("{flag} ");
    text.match_indices(&needle)
        .map(|(at, _)| {
            let rest = &text[at + needle.len()..];
            rest.split_whitespace().next().unwrap_or("")
        })
        .filter(|value| !value.is_empty())
        .collect()
}

/// **The bundled guide's recipe line and the recipe catalog say the same thing,
/// in both directions** (REQ-577 BR-1, BR-2, AC-7).
///
/// The guide is the only recipe surface that is *resident* — it is in every
/// system prompt, so it is what answers "hook up Kimi" with no tool call at all,
/// and it is the copy under a hard byte ceiling. Being hand-written under a
/// ceiling is exactly why it cannot be trusted to stay true on its own (ADR-2):
/// what keeps it true is this test.
///
/// Both directions fail differently, so both are checked and each message names
/// the side to edit. A catalog endpoint the guide never prints is a vendor the
/// model cannot name without a `teton_docs` round trip; a URL the guide prints
/// that no recipe ships is a third-party fact nothing in this repository
/// verified, pasted straight into somebody's shell.
///
/// What is pinned is the **third-party** half — endpoints and example models.
/// The suggested provider ids (`opus`, `kimi`) are the user's own namespace and
/// are deliberately free to differ between surfaces; pinning them would gate a
/// naming choice as though it were a fact about Moonshot.
#[test]
fn the_bundled_guide_and_the_recipe_catalog_agree() {
    let catalog = recipe_catalog();
    // Non-vacuity: an empty catalog would satisfy every loop below.
    assert!(
        catalog.len() >= 6,
        "the recipe catalog ships {} entries; the sweep below is narrower than the \
         roster it is supposed to cover",
        catalog.len()
    );
    let line = guide_recipe_line();

    // Catalog → guide: every vendor's endpoint and example model is on the line.
    for recipe in &catalog {
        match &recipe.endpoint {
            Some(endpoint) => assert!(
                line.contains(endpoint.as_str()),
                "the catalog gives `{}` the endpoint `{endpoint}` and the bundled guide's \
                 recipe step does not carry it, so a model composing that command has to \
                 invent a URL. Edit crates/tetond/src/harness/self_config.md — the catalog \
                 (crates/tetond/src/provider_recipes.rs) is the source and it moves first, \
                 and mind the byte ceiling the two prompt-margin tests pin.\nline: {line}",
                recipe.id_suggestion
            ),
            // The absent endpoint is a fact the guide has to state, not a gap it
            // may leave: a model that has never been told the `anthropic` kind
            // carries its own address will pattern-match `--endpoint` onto it
            // from the five neighbours that do.
            None => assert!(
                line.contains("no `--endpoint`"),
                "the catalog gives `{}` no endpoint — its kind knows its own address — and \
                 the guide's recipe step never says so, so the one vendor whose command \
                 takes no `--endpoint` reads like the five that do.\nline: {line}",
                recipe.id_suggestion
            ),
        }
        assert!(
            line.contains(recipe.example_model.as_str()),
            "the catalog's example model for `{}` is `{}` and the bundled guide's recipe \
             step names a different one or none. A retired model id is a 404 the user \
             cannot debug from the message. Edit \
             crates/tetond/src/harness/self_config.md.\nline: {line}",
            recipe.id_suggestion,
            recipe.example_model
        );
    }

    // Guide → catalog, over the **whole** guide rather than the anchored line:
    // any URL this file prints is one a model will read out to a user, wherever
    // in the file it sits. The web catalog is admitted as a second source
    // because the guide's `[web]` half legitimately names search endpoints.
    let from_recipes: BTreeSet<&str> = catalog
        .iter()
        .filter_map(|r| r.endpoint.as_deref())
        .collect();
    let web = suggestion_catalog();
    let from_web: BTreeSet<&str> = web.backends.iter().map(|b| b.endpoint.as_str()).collect();
    let in_guide = http_urls(BUNDLED_GUIDE);
    assert!(
        in_guide.len() >= from_recipes.len(),
        "the URL parse found {} URLs in a guide that must teach {} recipe endpoints, so it \
         is reading less than the guide holds: {in_guide:?}",
        in_guide.len(),
        from_recipes.len()
    );
    for url in in_guide {
        assert!(
            from_recipes.contains(url) || from_web.contains(url),
            "the bundled guide prints `{url}` and neither the recipe catalog \
             (crates/tetond/src/provider_recipes.rs) nor the web suggestion catalog \
             (crates/tetond/src/web_setup_catalog.rs) ships a vendor with that endpoint. \
             Edit crates/tetond/src/harness/self_config.md if the recipe was dropped or \
             renamed, or the catalog if the vendor is real and unlisted — every URL this \
             guide prints reaches a user's shell unverified.\nrecipes: {from_recipes:?}\n\
             web: {from_web:?}"
        );
    }
}

/// **The README's provider walkthrough and the recipe catalog say the same
/// thing, in both directions** (REQ-577 BR-2, AC-7).
///
/// This gate is the one with a defect already on its record. The README shipped
/// `--model kimi-k2` against Moonshot's real endpoint from before this REQ was
/// written, and it was still there when the catalog was: a reader who copied the
/// block got a correct-looking command and a 404 on a retired model. Nothing
/// caught it because nothing was looking, which is the whole argument for
/// checking prose mechanically rather than carefully.
///
/// Scope, decided rather than drifted into: what is pinned is the facts a third
/// party owns — endpoints, example models, and the roster of vendors the binary
/// claims to know. The `<id>` in each command (`opus`, `kimi`) is a *user-chosen
/// alias*, free to differ from the catalog's suggestion and from the guide's,
/// and gating it would turn a naming choice into a false failure.
#[test]
fn the_readme_recipes_and_the_catalog_agree() {
    let catalog = recipe_catalog();
    assert!(
        catalog.len() >= 6,
        "the recipe catalog ships {} entries; the sweep below is narrower than the \
         roster it is supposed to cover",
        catalog.len()
    );
    let section = readme_recipe_section();
    let commands = readme_recipe_commands();

    // Catalog → README, the roster: the section claims which vendors ship, and a
    // vendor the binary knows that the README never names is a recipe nobody
    // reading the README will ever ask for.
    for recipe in &catalog {
        assert!(
            section.contains(recipe.label.as_str()),
            "the recipe catalog ships `{}` and the README's `{README_RECIPES_ANCHOR}` \
             section never names it, so the roster in print is smaller than the roster in \
             the binary. Edit the README — crates/tetond/src/provider_recipes.rs is the \
             source and it moves first.\nsection:\n{section}",
            recipe.label
        );
    }

    // Catalog → README, the worked example: the block registers Moonshot, and
    // both of its third-party facts have to be the catalog's. This is the entry
    // that drifted, so it is the entry named explicitly rather than left to the
    // reverse direction, which would pass a block that dropped it entirely.
    let kimi = catalog
        .iter()
        .find(|r| r.label.contains("Kimi"))
        .expect("the catalog still ships the Moonshot recipe the README works through");
    let kimi_endpoint = kimi
        .endpoint
        .as_deref()
        .expect("Moonshot is an openai-compatible kind and carries an endpoint");
    assert!(
        commands.contains(kimi_endpoint),
        "the catalog gives Moonshot the endpoint `{kimi_endpoint}` and the README's \
         command block does not carry it. Edit the README block.\ncommands:\n{commands}"
    );
    assert!(
        commands.contains(kimi.example_model.as_str()),
        "the catalog's example model for Moonshot is `{}` and the README's command block \
         names a different one. This is exactly the drift that shipped — the block carried \
         `kimi-k2` after the vendor had moved on — so update the README, or \
         crates/tetond/src/provider_recipes.rs if the vendor moved \
         again.\ncommands:\n{commands}",
        kimi.example_model
    );

    // README → catalog: every third-party fact the block spells must be one this
    // repository verified. Parsed off the flags rather than by scanning prose,
    // so what is checked is precisely what a reader would paste.
    let endpoints: BTreeSet<&str> = catalog
        .iter()
        .filter_map(|r| r.endpoint.as_deref())
        .collect();
    let models: BTreeSet<&str> = catalog.iter().map(|r| r.example_model.as_str()).collect();

    let shown_endpoints = flag_values(commands, "--endpoint");
    let shown_models = flag_values(commands, "--model");
    // Non-vacuity: the block registers at least the two providers it routes
    // between, one of them remote.
    assert!(
        !shown_endpoints.is_empty() && shown_models.len() >= 2,
        "the flag parse read {shown_endpoints:?} and {shown_models:?} out of the README's \
         command block, which is less than the two registrations it walks through — the \
         parse is reading something other than the block.\ncommands:\n{commands}"
    );
    for endpoint in shown_endpoints {
        assert!(
            endpoints.contains(endpoint),
            "the README's command block passes `--endpoint {endpoint}` and the recipe \
             catalog ships no vendor with that endpoint. Edit the README if the recipe was \
             dropped or renamed, or crates/tetond/src/provider_recipes.rs if the vendor is \
             real and unlisted.\ncatalog: {endpoints:?}"
        );
    }
    for model in shown_models {
        assert!(
            models.contains(model),
            "the README's command block passes `--model {model}` and no recipe offers that \
             example. A model id in print outlives the release that served it: re-verify \
             against the vendor's current models page, then update \
             crates/tetond/src/provider_recipes.rs and the README together.\ncatalog: \
             {models:?}"
        );
    }
    // And the same claim once more over the raw URLs, so a vendor fact smuggled
    // into the block as prose rather than as a flag value is caught too.
    for url in http_urls(commands) {
        assert!(
            endpoints.contains(url),
            "the README's command block shows `{url}` and the recipe catalog ships no \
             vendor with that endpoint.\ncatalog: {endpoints:?}"
        );
    }
}

// ---------------------------------------------------------------------------
// teton_docs topics ↔ the two catalogs (REQ-577 BR-2 / AC-7)
// ---------------------------------------------------------------------------

/// The `--kind` value a recipe's [`ProviderKind`] is spelled as on the command
/// line, taken from the serde casing rather than a second table — the flag the
/// CLI parses and the flag the prose prints are one string.
fn kind_flag(kind: ProviderKind) -> String {
    serde_json::to_value(kind)
        .ok()
        .and_then(|v| v.as_str().map(str::to_owned))
        .expect("a provider kind serializes to its kebab-case wire name")
}

/// The `teton provider add <id> …` command as the providers topic prints it:
/// the line that starts it plus the line that continues it, because the recipes
/// wrap after `--kind`.
///
/// Panics when the id is absent rather than returning an empty string. A parse
/// that fails open is the `ENDPOINT_HELP` mistake this suite's header describes:
/// every assertion below would then hold vacuously over nothing.
fn provider_add_command(text: &str, id: &str) -> String {
    let needle = format!("teton provider add {id} ");
    let lines: Vec<&str> = text.lines().collect();
    let at = lines
        .iter()
        .position(|line| line.contains(&needle))
        .unwrap_or_else(|| {
            panic!(
                "the providers topic (crates/tetond/src/harness/docs/providers.md) has no \
                 `{needle}` command, so the catalog ships a recipe the topic never teaches. \
                 Add the recipe block, or remove the entry from \
                 crates/tetond/src/provider_recipes.rs if the vendor was dropped."
            )
        });
    lines[at..(at + 2).min(lines.len())].join(" ")
}

/// **The `teton_docs` providers topic and the recipe catalog say the same
/// thing, in both directions** (REQ-577 BR-2, AC-7).
///
/// Same posture as the guide and README gates above, on the second catalog. The
/// topic is the surface that answers "how do I hook up Kimi" at depth, so a
/// stale endpoint here is a runnable-*looking* command whose failure arrives a
/// step away from its cause — the BUG-165 texture, which is why drift is a
/// build failure rather than a doc bug.
///
/// Both directions, because they fail differently: a catalog recipe the topic
/// never teaches is knowledge the tool cannot serve, and a URL the topic prints
/// that no recipe ships is a vendor fact nothing in this repository verified.
#[test]
fn the_providers_topic_and_the_recipe_catalog_agree() {
    let catalog = recipe_catalog();
    // Non-vacuity: an empty catalog would satisfy every loop below.
    assert!(
        catalog.len() >= 6,
        "the recipe catalog ships {} entries; the sweep below is narrower than the \
         roster it is supposed to cover",
        catalog.len()
    );

    // Catalog → topic: every recipe is a runnable pair of commands in the prose.
    for recipe in &catalog {
        let command = provider_add_command(PROVIDERS_TOPIC, &recipe.id_suggestion);
        let flag = kind_flag(recipe.kind);
        assert!(
            command.contains(&format!("--kind {flag}")),
            "the providers topic registers `{}` with a different kind than the catalog's \
             `{flag}`. A wrong kind sends the wrong auth header and 401s on a good \
             key.\ncommand: {command}",
            recipe.id_suggestion
        );
        match &recipe.endpoint {
            Some(endpoint) => assert!(
                command.contains(endpoint.as_str()),
                "the catalog gives `{}` the endpoint `{endpoint}` and the topic's command \
                 does not carry it. Edit \
                 crates/tetond/src/harness/docs/providers.md — the catalog \
                 (crates/tetond/src/provider_recipes.rs) is the source and it moves \
                 first.\ncommand: {command}",
                recipe.id_suggestion
            ),
            // The absent endpoint is a *fact*, not a gap: the `anthropic` kind
            // carries its own address, so a printed `--endpoint` is a user error
            // the topic would be teaching.
            None => assert!(
                !command.contains("--endpoint"),
                "the catalog gives `{}` no endpoint — its kind knows its own address — and \
                 the topic prints one anyway.\ncommand: {command}",
                recipe.id_suggestion
            ),
        }
        assert!(
            command.contains(&format!("--model {}", recipe.example_model)),
            "the catalog's example model for `{}` is `{}` and the topic's command names a \
             different one. A retired model id is a 404 a user cannot debug from the \
             message.\ncommand: {command}",
            recipe.id_suggestion,
            recipe.example_model
        );
        // BR-1's second half: registering a provider routes nothing, so a recipe
        // without its routing step is a recipe that appears to do nothing.
        assert!(
            PROVIDERS_TOPIC.lines().any(|line| {
                let line = line.trim();
                line.starts_with("teton policy set-tier")
                    && line.ends_with(recipe.id_suggestion.as_str())
            }),
            "the providers topic never routes a tier to `{}`, so following it registers a \
             provider that no work reaches. Add the `teton policy set-tier … {}` line.",
            recipe.id_suggestion,
            recipe.id_suggestion
        );
    }

    // Topic → catalog: a URL in the prose that no recipe ships is a vendor fact
    // nobody verified, printed into somebody's shell.
    let offered: BTreeSet<&str> = catalog
        .iter()
        .filter_map(|r| r.endpoint.as_deref())
        .collect();
    let in_topic = http_urls(PROVIDERS_TOPIC);
    assert!(
        in_topic.len() >= offered.len(),
        "the URL parse found {} URLs in a topic that must teach {} endpoints, so it is \
         reading less than the topic holds: {in_topic:?}",
        in_topic.len(),
        offered.len()
    );
    for url in in_topic {
        assert!(
            offered.contains(url),
            "the providers topic prints `{url}` and the recipe catalog ships no vendor \
             with that endpoint. Edit \
             crates/tetond/src/harness/docs/providers.md if the recipe was dropped or \
             renamed, or crates/tetond/src/provider_recipes.rs if the vendor is real and \
             unlisted — every URL this topic prints is one a user will paste into a \
             command.\noffered: {offered:?}"
        );
    }
}

/// **The `teton_docs` web topic and the search catalog agree on the auth
/// shapes** (REQ-577 BR-2).
///
/// The narrower claim of the two, and deliberately so: what the web topic adds
/// over the bundled guide is depth about tiers, the keychain reference and when
/// a change takes effect — none of which any catalog owns. The header shapes are
/// the part a third party owns, and they are the part BUG-165 got wrong, so they
/// are the part under CI.
#[test]
fn the_web_topic_and_the_suggestion_catalog_agree() {
    let catalog = suggestion_catalog();
    let in_topic = suggested_auth_templates(WEB_TOPIC);
    assert!(
        in_topic.len() >= 3,
        "the web topic must still name the auth templates it documents; found: {in_topic:?}"
    );

    let in_catalog: BTreeSet<&str> = catalog
        .backends
        .iter()
        .filter_map(|b| b.auth_template.as_deref())
        .collect();
    assert!(
        !in_catalog.is_empty(),
        "the catalog must still offer at least one keyed backend"
    );

    // Topic → catalog: a shape the topic tells a model to suggest, that the
    // daemon offers for nothing, is advice that ends in a 401.
    for template in &in_topic {
        assert!(
            in_catalog.contains(template.as_str()) || *template == catalog.default_auth_template,
            "the web topic (crates/tetond/src/harness/docs/web.md) documents `{template}` \
             and the daemon catalog offers no backend with that template and no such \
             default. Update the catalog (crates/tetond/src/web_setup_catalog.rs) if the \
             backend is real, or the topic if the line is stale.\ncatalog: {in_catalog:?} \
             + default `{}`",
            catalog.default_auth_template
        );
    }

    // Catalog → topic: a backend the daemon offers whose header the topic never
    // names is one the model cannot talk a user through.
    for template in &in_catalog {
        assert!(
            in_topic.contains(*template),
            "the daemon catalog offers `{template}` and the web topic \
             (crates/tetond/src/harness/docs/web.md) does not name it — the model reading \
             that topic has never heard of it. Add it to the topic; unlike the bundled \
             guide, the topic is not resident prompt and has room.\ntopic: {in_topic:?}"
        );
    }
    assert!(
        in_topic.contains(&catalog.default_auth_template),
        "the catalog's default template `{}` is not in the web topic, so the shape offered \
         for every unnamed backend is one the topic never explains.\ntopic: {in_topic:?}",
        catalog.default_auth_template
    );

    // The keyless entry is prose on both sides, and its endpoint *shape* is the
    // load-bearing half: without `format=json` a SearxNG instance answers HTML.
    let searxng = catalog
        .backends
        .iter()
        .find(|b| !b.needs_key)
        .expect("the catalog still offers a keyless backend");
    assert!(
        WEB_TOPIC.contains("/search?format=json"),
        "the web topic no longer shows the `/search?format=json` endpoint shape, so a user \
         following it configures an instance that answers HTML"
    );
    assert!(
        searxng.endpoint.ends_with("/search?format=json"),
        "the catalog's keyless endpoint `{}` no longer ends `/search?format=json` while the \
         topic still documents that shape",
        searxng.endpoint
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
        assert_search_request_shape(&backend.id, &backend.endpoint).await;
    }
}

/// The request-shape assertions for one endpoint, driven through the production
/// choke point.
///
/// A function rather than a loop body because two callers need it: the
/// enumeration above, and the unnamed backend below — which is not a catalog
/// entry, is therefore not enumerated, and is exactly the case that quietly lost
/// this coverage once already.
async fn assert_search_request_shape(id: &str, endpoint: &str) {
    let sent = search_request_for(endpoint).await;
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
    // The endpoint's own path and parameters survive verbatim — the property
    // SearxNG's `?format=json` depends on, asserted for every backend so a
    // builder that special-cased one of them is visible.
    let (base, params) = endpoint.split_once('?').map_or((endpoint, ""), |p| p);
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
    // The credential is not composed here, for any backend: it is attached by
    // the endpoint-bound transport, which is what keeps it off a page fetch
    // (REQ-563 BR-7). The header-shape test is where the shape is asserted.
    assert!(
        !request
            .headers
            .iter()
            .any(|(_, value)| value.contains(SECRET)),
        "{id}: the request builder must compose no credential: {:?}",
        request.headers
    );
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
    // continuity promise, that every pre-template config keeps working. Its
    // other two legs are [`the_unnamed_backend_default_is_a_whole_contract`].
    let default = unnamed_backend_config();
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

/// **The backend the catalog does not name gets the whole contract, not just a
/// header shape** (BUG-165's continuity promise).
///
/// The default is not a catalog entry, so it has no [`CONTRACTS`] row and does
/// not ride the enumeration — which is how it kept only its header leg when this
/// suite was re-pointed at the catalog, while the config it loads and the
/// request it drives went unasserted. Both are back: the same
/// `Config::validate` gate and rendered-document sweep every suggestion gets,
/// and the same production request builder.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_unnamed_backend_default_is_a_whole_contract() {
    let config = Config {
        web: unnamed_backend_config(),
        ..Config::default()
    };
    assert!(
        config.validate().is_ok(),
        "a config with no `search_auth` must still load — refusing it would break every \
         install written before the template existed: {:?}",
        config.validate().err()
    );
    let document = config.to_toml().expect("the default config renders");
    assert!(
        !document.contains(SECRET),
        "the unnamed backend's rendered config must carry a reference, never a \
         key:\n{document}"
    );
    assert!(
        document.contains("keychain://teton/web-search"),
        "the unnamed backend's rendered config must carry the keychain reference the user \
         configured:\n{document}"
    );

    assert_search_request_shape("default", UNNAMED_BACKEND_ENDPOINT).await;
}

/// **The keyless suggestion takes no credential anywhere** (REQ-573 BR-4's
/// keyless leg, inherited from REQ-572 AC-8).
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
