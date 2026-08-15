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
//! | REQ-577 BR-2 (recipe notes ↔ the prose that echoes them, bidirectional) | [`the_recipe_notes_and_the_prose_that_echoes_them_agree`] |
//! | REQ-577 BR-2 (policy topic ↔ `Category::tier()`) | [`the_policy_topic_files_every_category_under_its_own_tier`] |
//! | REQ-577 follow-up (price table ↔ recipe catalog example models) | [`the_price_table_and_the_recipe_catalogs_example_models_agree`] |
//!
//! ## Facts are checked **paired**, not as sets (phase-5)
//!
//! The guide and README gates originally asked two separate questions — is this
//! endpoint somewhere in the text, is this model somewhere in the text — and a
//! surface can answer yes to both while pairing Moonshot's endpoint with
//! Anthropic's model. Every individual fact checks out and the command is dead.
//! So the guide's recipe line is now split per vendor and each endpoint's model
//! must be in **its own** segment, and the README's block is read one
//! `provider add` at a time with `(kind, endpoint, model)` required to be a
//! combination some single recipe ships. Both sweep example models in the
//! reverse direction as well, which the endpoint-only reverse legs did not.
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
use tetond::cost::prices::PriceTable;
use tetond::provider_recipes::{recipe_catalog, ProviderRecipe};
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

/// The bundled guide's recipe **list**, split into one segment per vendor.
///
/// The list is everything from the first catalog endpoint the line carries
/// onward, which is derived rather than anchored on a phrase — one less piece of
/// prose that can be reworded out from under this suite. Segments are cut on
/// `;`, which is how the line separates vendors, so each vendor's endpoint and
/// its example model land in the **same** segment or the pairing check fails.
///
/// That pairing is the whole reason this exists. Asserting "the line contains
/// this endpoint" and "the line contains this model" separately passes a line
/// that names Moonshot's endpoint beside DeepSeek's model — six right facts in
/// the wrong six places, which is a worse failure than a missing one because
/// every individual fact checks out.
///
/// **Panics** when no catalog endpoint appears at all: the forward direction
/// below would then hold vacuously over an empty list.
fn guide_recipe_segments(catalog: &[ProviderRecipe]) -> Vec<&'static str> {
    let line = guide_recipe_line();
    // The first vendor in the list, found by whichever recipe's endpoint appears
    // earliest. Derived rather than anchored on a phrase, so there is one less
    // piece of prose that can be reworded out from under this suite.
    let (first_endpoint, first_recipe) = catalog
        .iter()
        .filter_map(|r| line.find(r.endpoint.as_deref()?).map(|at| (at, r)))
        .min_by_key(|(at, _)| *at)
        .unwrap_or_else(|| {
            panic!(
                "the bundled guide's recipe step carries none of the catalog's endpoints, \
                 so there is no recipe list to split. Restore the recipes in \
                 crates/tetond/src/harness/self_config.md, or re-anchor this \
                 check.\nline: {line}"
            )
        });
    // Start at that vendor's **name**, not at its URL. Two reasons, and both are
    // failures this parse has already had. The name is a fact the pairing check
    // below asserts, so a slice that began at the URL would cut the first
    // vendor's name out of its own segment. And the URL is wrapped in backticks:
    // cutting between the opening backtick and the URL inverts the parity of
    // every backtick after it, so the token parse reads prose as models and
    // models as prose — quietly, yielding one fewer candidate than there are
    // vendors rather than an error.
    let start = line[..first_endpoint]
        .rfind(first_recipe.guide_spelling.as_str())
        .unwrap_or_else(|| {
            panic!(
                "the guide's first recipe is `{}` (its endpoint appears earliest) and the \
                 line does not name it as `{}` anywhere before that URL, so the recipe list \
                 has no findable start. Either the guide dropped the vendor's name or the \
                 catalog's `guide_spelling` is stale.\nline: {line}",
                first_recipe.id_suggestion, first_recipe.guide_spelling
            )
        });
    line[start..].split(';').collect()
}

/// The segment of the guide's recipe list that carries `endpoint`, and the proof
/// that exactly one does.
///
/// Two matches would mean the split is not separating vendors — which would make
/// every pairing assertion below true of a line that pairs nothing.
fn guide_segment_for<'a>(segments: &[&'a str], endpoint: &str, id: &str) -> &'a str {
    let matches: Vec<&&str> = segments.iter().filter(|s| s.contains(endpoint)).collect();
    assert_eq!(
        matches.len(),
        1,
        "`{id}`'s endpoint `{endpoint}` appears in {} of the guide's recipe segments, not \
         exactly one. Either the recipe is missing from \
         crates/tetond/src/harness/self_config.md, or the `;` separators no longer divide \
         the list one vendor per segment — in which case this pairing check is reading \
         several vendors as one and would pass a line that gave a vendor its neighbour's \
         model.\nsegments: {segments:?}",
        matches.len()
    );
    matches[0]
}

/// The backticked tokens in `text` that could be a model id.
///
/// Everything in the guide's recipe list is written in backticks — URLs, model
/// ids, and the odd path fragment like `/v1` — so the model ids are what is left
/// after removing what a model id demonstrably is not: a URL, a path, a flag, or
/// a whole command (which has spaces in it). The filter is deliberately about
/// *shape* rather than a list of known non-models: a needle list would have to
/// be extended by whoever adds the next parenthetical, and forgetting to do so
/// fails open.
fn backticked_model_candidates(text: &str) -> Vec<&str> {
    text.split('`')
        .skip(1)
        .step_by(2)
        .filter(|token| {
            !token.is_empty()
                && !token.contains("://")
                && !token.starts_with('/')
                && !token.starts_with('-')
                && !token.contains(char::is_whitespace)
        })
        .collect()
}

/// `block` with its `#` comment lines dropped.
///
/// The flag sweeps below claim to read *the commands a reader pastes*, and the
/// block's comments are prose about those commands — prose that legitimately
/// names flags ("every remote kind needs `--kind`, `--endpoint` and `--model`").
/// Parsing a flag out of a sentence and then asserting its "value" is a real
/// endpoint is how this gate reports `--endpoint and`. URLs are still swept over
/// the **whole** block, comments included, because a stale URL in a comment is
/// exactly as copyable as one in a command.
fn without_comments(block: &str) -> String {
    block
        .lines()
        .filter(|line| !line.trim_start().starts_with('#'))
        .collect::<Vec<_>>()
        .join("\n")
}

/// Every `teton provider add …` command in `block`, each rejoined into one
/// logical line.
///
/// The README wraps its longer registrations with a trailing `\`, so a
/// line-by-line read would see the id and the kind on one line and the endpoint
/// and the model on the next — and a per-command check that split a command in
/// half would be checking two fragments against a whole recipe. Continuations
/// are folded back before anything is parsed.
///
/// **Panics** when the block contains no registration: this is the input to a
/// pairing sweep, and a sweep over nothing passes.
fn readme_provider_add_commands(block: &str) -> Vec<String> {
    let mut commands: Vec<String> = Vec::new();
    let mut current: Option<String> = None;
    for line in block.lines() {
        let trimmed = line.trim();
        let continues = trimmed.ends_with('\\');
        let body = trimmed.trim_end_matches('\\').trim_end();
        match current.as_mut() {
            Some(open) => {
                open.push(' ');
                open.push_str(body);
            }
            None if body.starts_with("teton provider add ") => {
                current = Some(body.to_owned());
            }
            None => continue,
        }
        if !continues {
            if let Some(done) = current.take() {
                commands.push(done);
            }
        }
    }
    // A command left open by a trailing `\` at the end of the block is still a
    // command, and dropping it would be the fail-open this suite is written
    // against.
    if let Some(open) = current.take() {
        commands.push(open);
    }
    assert!(
        !commands.is_empty(),
        "the README's command block contains no `teton provider add` line, so the \
         per-command pairing sweep would pass over nothing.\nblock:\n{block}"
    );
    commands
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
    let segments = guide_recipe_segments(&catalog);

    // A lost `;` merges two vendors into one segment, and every per-segment
    // assertion below would then still pass — the merged segment contains both
    // endpoints and both models, so each vendor "finds" its pair. The split has
    // to be shown to have happened before anything is concluded from it.
    assert!(
        segments.len() >= catalog.len(),
        "the guide's recipe list split into {} segments for {} recipes, so at least two \
         vendors share one. Every pairing check below would pass over a merged segment \
         while the line itself reads as one vendor's endpoint beside another's model. \
         Restore the `;` separators in crates/tetond/src/harness/self_config.md.\n\
         segments: {segments:?}",
        segments.len(),
        catalog.len()
    );

    // Catalog → guide, **paired per vendor**: each endpoint is on the line, and
    // the vendor's name and the example model that belong to it are in the
    // *same* segment.
    for recipe in &catalog {
        let endpoint = recipe.endpoint.as_deref().unwrap_or_else(|| {
            panic!(
                "the catalog gives `{}` no endpoint. Every kind a recipe may name is \
                 `is_remote()`, and `Config::validate` demands an endpoint for those — a \
                 recipe without one is a `provider add` the daemon refuses after the key \
                 is already in the keychain (BUG-170). Fix \
                 crates/tetond/src/provider_recipes.rs.",
                recipe.id_suggestion
            )
        });
        assert!(
            line.contains(endpoint),
            "the catalog gives `{}` the endpoint `{endpoint}` and the bundled guide's \
             recipe step does not carry it, so a model composing that command has to \
             invent a URL. Edit crates/tetond/src/harness/self_config.md — the catalog \
             (crates/tetond/src/provider_recipes.rs) is the source and it moves first, \
             and mind the byte ceiling the two prompt-margin tests pin.\nline: {line}",
            recipe.id_suggestion
        );
        let segment = guide_segment_for(&segments, endpoint, &recipe.id_suggestion);
        assert!(
            segment.contains(recipe.example_model.as_str()),
            "the guide names `{}`'s endpoint and names `{}` somewhere, but not in the same \
             recipe segment — so the line pairs that endpoint with some other vendor's \
             model. Every fact would check out individually and the command the model \
             composes would still be wrong. Edit \
             crates/tetond/src/harness/self_config.md.\nsegment: {segment}",
            recipe.id_suggestion,
            recipe.example_model
        );

        // The vendor's *name* is the third fact in the pair, and the one a user
        // reads first. An endpoint and a model can both be right and be filed
        // under the wrong company — "Moonshot/Kimi `https://api.x.ai/…`" is six
        // verified facts arranged into two wrong recipes, and the endpoint and
        // model checks above pass on it, because each still finds its partner.
        assert!(
            segment.contains(recipe.guide_spelling.as_str()),
            "the guide's recipe segment carrying `{}`'s endpoint does not name it. The \
             catalog says the guide spells this vendor `{}` (ProviderRecipe::guide_spelling) \
             — either the guide moved a name off its own recipe, or the spelling changed \
             and the catalog has not. Edit \
             crates/tetond/src/harness/self_config.md, or the catalog if the guide's \
             wording is the intended one.\nsegment: {segment}",
            recipe.id_suggestion,
            recipe.guide_spelling
        );
        for other in &catalog {
            if other.id_suggestion == recipe.id_suggestion {
                continue;
            }
            assert!(
                !segment.contains(other.guide_spelling.as_str()),
                "the guide's recipe segment for `{}` also names `{}` (`{}`). Two vendors in \
                 one segment means a name has been swapped onto a neighbour's URL, or the \
                 `;` separators no longer divide the list one vendor per \
                 segment.\nsegment: {segment}",
                recipe.id_suggestion,
                other.id_suggestion,
                other.guide_spelling
            );
            // And the same claim for the model ids, over the raw segment rather
            // than the backticked tokens: a model id that leaked into a
            // neighbour's prose is still a model id a reader would attach to the
            // wrong endpoint, and dropping the backticks is how it would evade
            // the token parse below.
            assert!(
                !segment.contains(other.example_model.as_str()),
                "the guide's recipe segment for `{}` also names `{}`, which is `{}`'s \
                 example model. Whichever of the two a reader pastes, one of them is going \
                 to the wrong vendor.\nsegment: {segment}",
                recipe.id_suggestion,
                other.example_model,
                other.id_suggestion
            );
        }
    }

    // Guide → catalog, the models: a model id printed here is one a user pastes
    // into `--model`, so it has to be an example this repository verified. The
    // endpoints get the same treatment further down, over the whole file.
    let models: BTreeSet<&str> = catalog.iter().map(|r| r.example_model.as_str()).collect();
    let named = segments
        .iter()
        .flat_map(|segment| backticked_model_candidates(segment))
        .collect::<Vec<_>>();
    assert!(
        named.len() >= catalog.len(),
        "the guide's recipe list yielded {} model candidates for {} recipes, so the parse \
         is reading less than the list holds: {named:?}",
        named.len(),
        catalog.len()
    );
    for model in named {
        assert!(
            models.contains(model),
            "the bundled guide's recipe list names `{model}` and no recipe offers that \
             example. A model id in the resident prompt is one the agent reads out on \
             every provider question: re-verify it against the vendor's current models \
             page, then update crates/tetond/src/provider_recipes.rs and the guide \
             together.\ncatalog: {models:?}"
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
    // The flag parses read commands; the URL parse further down reads the whole
    // block. See `without_comments` for why the two differ.
    let runnable = without_comments(commands);

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

    // README → catalog, **per command**: a `provider add` line is a unit, and
    // its three third-party facts have to come from one recipe. Checking the
    // endpoint set and the model set separately passes a command that gives
    // Moonshot's endpoint Anthropic's model — every fact verified, the command
    // still dead. This is the check that would have caught the shipped defect
    // directly rather than by way of the model id that happened to also be
    // stale (BUG-170).
    for command in readme_provider_add_commands(&runnable) {
        let Some(kind) = flag_values(&command, "--kind")
            .first()
            .map(|k| (*k).to_owned())
        else {
            panic!(
                "a `teton provider add` line in the README's block passes no `--kind`, so \
                 the command names no adapter at all.\ncommand: {command}"
            );
        };
        let endpoint = flag_values(&command, "--endpoint")
            .first()
            .map(|e| (*e).to_owned());
        let model = flag_values(&command, "--model")
            .first()
            .map(|m| (*m).to_owned());
        let paired = catalog.iter().any(|recipe| {
            kind_flag(recipe.kind) == kind
                && recipe.endpoint.as_deref() == endpoint.as_deref()
                && Some(recipe.example_model.as_str()) == model.as_deref()
        });
        assert!(
            paired,
            "the README's command block registers a provider with \
             kind={kind:?} endpoint={endpoint:?} model={model:?}, and no single recipe \
             ships that combination. Each of the three may be a fact this repository \
             verified and the command still be one that cannot work — a base URL where a \
             request URL belongs, or one vendor's model against another's endpoint. Edit \
             the README, or crates/tetond/src/provider_recipes.rs if the vendor \
             moved.\ncommand: {command}\ncatalog: {:?}",
            catalog
                .iter()
                .map(|r| (
                    kind_flag(r.kind),
                    r.endpoint.clone(),
                    r.example_model.clone()
                ))
                .collect::<Vec<_>>()
        );
    }

    // README → catalog: every third-party fact the block spells must be one this
    // repository verified. Parsed off the flags rather than by scanning prose,
    // so what is checked is precisely what a reader would paste.
    let endpoints: BTreeSet<&str> = catalog
        .iter()
        .filter_map(|r| r.endpoint.as_deref())
        .collect();
    let models: BTreeSet<&str> = catalog.iter().map(|r| r.example_model.as_str()).collect();

    let shown_endpoints = flag_values(&runnable, "--endpoint");
    let shown_models = flag_values(&runnable, "--model");
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
            // Unreachable while every registerable kind is `is_remote()`, which
            // is every kind a recipe may name — kept because the arm is where a
            // future non-remote kind would land, and because a `_ => {}` here
            // would be the fail-open this suite is written against. Round 1's
            // catalog did reach it, on the strength of a belief that the
            // `anthropic` adapter carried its own address; it does not, and the
            // recipe that said so produced a registration `config/set` refused
            // (BUG-170).
            None => assert!(
                !command.contains("--endpoint"),
                "the catalog gives `{}` no endpoint and the topic prints one anyway. Note \
                 that a *remote* kind with no endpoint is itself a defect the recipe \
                 catalog's own sweep catches first.\ncommand: {command}",
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

// ---------------------------------------------------------------------------
// The consent rule, as every surface states it (REQ-577 phase 5)
// ---------------------------------------------------------------------------

/// `text` with its whitespace collapsed to single spaces.
///
/// Unlike [`normalized`] below, backticks and case survive: these are pins on
/// sentences whose exact wording is the point, and the only thing being
/// forgiven is the line wrapping that markdown imposes on a paragraph.
fn collapsed(text: &str) -> String {
    text.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// The consent rule as the `web` topic must state it.
const TOPIC_CONSENT_RULE: &str = "a lookup asks before anything leaves the machine \
     **unless** that tier has already been granted — for the session, or permanently via \
     `[web] permission_allow`";

/// The same rule as the README must state it.
const README_CONSENT_RULE: &str = "A lookup asks before anything leaves the machine unless \
     that tier has already been granted — for the session, or permanently via `[web] \
     permission_allow`.";

/// The absolute neither surface may return to.
const FALSE_ABSOLUTE: &str = "every lookup still asks";

/// **Both consent surfaces state the rule with its exception, and neither
/// states the absolute that was false** (REQ-577 phase 5).
///
/// The defect this pins is a *claim*, not a fact about a third party, so it
/// drifts differently from everything else in this file: nobody has to change a
/// vendor's API for it to go wrong, only to tidy a sentence. Both surfaces
/// carried "every lookup still asks before anything leaves the machine", and
/// both were wrong the moment `permission_allow` shipped — a tier listed there,
/// or granted for the session, is not asked about again. Overstating a privacy
/// guarantee is worse than understating one: it is the sentence a user relies on
/// when deciding whether to enable a tier at all.
///
/// Pinned as **collapsed-whitespace equality on the claim**, the markdown
/// analogue of the whole-line equality the prompt clauses use (BUG-168 residual
/// (d)): a substring needle on "asks before anything leaves the machine" would
/// be satisfied by the false absolute itself, which is exactly how this survived
/// the first pass. The negative half is checked too, because a surface can
/// acquire a correct sentence and keep the wrong one next to it.
#[test]
fn both_consent_surfaces_state_the_rule_with_its_exception() {
    let topic = collapsed(WEB_TOPIC);
    let readme = collapsed(README);

    assert!(
        topic.contains(&collapsed(TOPIC_CONSENT_RULE)),
        "the `web` topic (crates/tetond/src/harness/docs/web.md) no longer states the \
         consent rule with its exception. If the wording changed deliberately, update this \
         expectation — and keep the exception in it: a tier granted for the session or \
         listed in `[web] permission_allow` is not asked about again, so any sentence \
         promising that every lookup asks is false. Do not delete the \
         assertion.\nexpected: {}",
        collapsed(TOPIC_CONSENT_RULE)
    );
    assert!(
        readme.contains(&collapsed(README_CONSENT_RULE)),
        "the README no longer states the consent rule with its exception. Same rule as the \
         `web` topic's, and the README is the surface a user reads *before* enabling \
         anything.\nexpected: {}",
        collapsed(README_CONSENT_RULE)
    );

    for (surface, text) in [("the `web` topic", &topic), ("the README", &readme)] {
        assert!(
            !text.to_lowercase().contains(FALSE_ABSOLUTE),
            "{surface} says {FALSE_ABSOLUTE:?} again. That absolute is false: a tier \
             granted for the session, or listed in `[web] permission_allow`, is not asked \
             about again. State the exception with the rule."
        );
    }

    // The durable key has to be documented where it is claimed, including how a
    // user takes it back — a consent switch with no documented off is a switch
    // a user cannot audit.
    assert!(
        topic.contains("Removing a tier from it restores asking")
            && topic.contains("after the daemon next starts"),
        "the `web` topic documents `permission_allow` without saying how to revoke it or \
         when a revocation takes effect. A hand edit of config.toml is read at daemon \
         start, and a user who removes a line and sees the tier still silent needs that \
         sentence."
    );
    assert!(
        readme.contains("Removing a tier from `permission_allow` restores asking"),
        "the README documents `permission_allow` without saying how to revoke it."
    );
}

// ---------------------------------------------------------------------------
// Recipe notes ↔ the prose that echoes them (REQ-577 BR-2)
// ---------------------------------------------------------------------------

/// `text` with backticks and asterisks removed, lowercased, whitespace
/// collapsed — the form the note-echo rows are matched in.
///
/// The same claim is written three ways by three conventions: a catalog note is
/// plain prose in a Rust string, the topic wraps flags and paths in backticks,
/// and the guide does too under a byte ceiling that decides where the backticks
/// go. Matching raw would make this gate a formatting check, which is the kind
/// that gets deleted the first time it fires for a reason nobody cares about.
fn normalized(text: &str) -> String {
    text.replace(['`', '*'], "")
        .to_lowercase()
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
}

/// One recipe note and the phrase that must survive into each prose surface.
///
/// Hand-written per vendor rather than derived, the [`CONTRACTS`] posture: a
/// note is a sentence and its prose echo is a different sentence, so "the same
/// claim" is a judgment somebody makes once and writes down — not a substring
/// relation a rule could compute.
struct NoteEcho {
    /// The recipe this row is about, keyed by suggested id.
    id: &'static str,
    /// The claim, as it must appear in the catalog note.
    in_note: &'static str,
    /// The claim, as it must appear in the `providers` topic.
    in_topic: &'static str,
    /// The claim, as it must appear in the guide's segment for this vendor —
    /// or `None` where the guide states the fact **structurally** rather than
    /// in prose and has no bytes to spare for saying it twice.
    in_guide: Option<&'static str>,
}

/// The note-echo table: one row per recipe that carries a note.
///
/// A note exists because the command shape alone does not say something. That
/// makes it exactly the kind of fact that is true in the typed source and
/// missing from the prose a user actually reads — the drift this REQ's gates
/// exist for, one altitude below the endpoints.
const NOTE_ECHOES: &[NoteEcho] = &[
    // The guide row is `None` here on purpose: the guide carries this fact in
    // the endpoint itself (`…/v1/messages` beside five `…/chat/completions`),
    // which the pairing gate above already pins byte for byte. Spending resident
    // prompt on a prose restatement of a URL the same line prints would be
    // paying twice, with 93 bytes of margin left.
    NoteEcho {
        id: "anthropic",
        in_note: "messages api path",
        in_topic: "messages api",
        in_guide: None,
    },
    NoteEcho {
        id: "deepseek",
        in_note: "no /v1",
        in_topic: "no /v1",
        in_guide: Some("no /v1"),
    },
    NoteEcho {
        id: "ollama",
        in_note: "ignores the key",
        in_topic: "ignores the key",
        // The guide's shortest true spelling. It has to say *something*: round 1
        // said "local and keyless", and a user told there is no key step meets
        // an echo-off prompt the recipe promised would not come.
        in_guide: Some("any key"),
    },
];

/// **A recipe's note and the prose that repeats it say the same thing**
/// (REQ-577 BR-2, extended).
///
/// The endpoint gates above pin the facts a command *contains*. A note is the
/// fact a command does not contain — why this path has no `/v1`, why Ollama
/// still asks for a key — and it reaches a user only through prose. So it drifts
/// the same way an endpoint does, silently and in either direction, and it is
/// gated the same way.
///
/// Bidirectional at the table level as well as the surface level: a recipe that
/// gains a note with no row here fails, and a row naming a recipe that has no
/// note fails as stale. That is what stops the table quietly describing a
/// catalog that has moved on.
#[test]
fn the_recipe_notes_and_the_prose_that_echoes_them_agree() {
    let catalog = recipe_catalog();
    let segments = guide_recipe_segments(&catalog);
    let topic = normalized(PROVIDERS_TOPIC);

    // Catalog → table: every note has a row.
    for recipe in &catalog {
        let Some(note) = recipe.notes.as_deref() else {
            continue;
        };
        let row = NOTE_ECHOES
            .iter()
            .find(|row| row.id == recipe.id_suggestion)
            .unwrap_or_else(|| {
                panic!(
                    "the catalog gives `{}` the note {note:?} and `NOTE_ECHOES` has no row \
                     for it. A note is a fact the command shape does not carry, so it \
                     reaches the user only through prose — add the row and the prose, or \
                     drop the note if it was not worth saying.",
                    recipe.id_suggestion
                )
            });

        let note = normalized(note);
        assert!(
            note.contains(row.in_note),
            "`{}`'s note no longer says {:?}. Either the note was reworded and this row is \
             stale, or the claim was dropped and the prose below still makes it — update \
             both together.\nnote: {note}",
            recipe.id_suggestion,
            row.in_note
        );
        assert!(
            topic.contains(row.in_topic),
            "the catalog note for `{}` claims {:?} and the `providers` topic never says \
             it. The topic is where a note gets said at length — it is a tool result, not \
             resident prompt, so it has the room. Edit \
             crates/tetond/src/harness/docs/providers.md.",
            recipe.id_suggestion,
            row.in_topic
        );
        if let Some(expected) = row.in_guide {
            let endpoint = recipe
                .endpoint
                .as_deref()
                .expect("every recipe carries an endpoint; the sweep above pins that");
            let segment = normalized(guide_segment_for(
                &segments,
                endpoint,
                &recipe.id_suggestion,
            ));
            assert!(
                segment.contains(expected),
                "the catalog note for `{}` claims {expected:?} and the guide's own recipe \
                 segment for it does not. This row is `Some` because the guide cannot \
                 state this one structurally, so dropping it drops the fact from the \
                 resident prompt — where it is the only copy a turn sees without a tool \
                 call. Edit crates/tetond/src/harness/self_config.md (mind the margin), or \
                 set this row's `in_guide` to `None` and say why.\nsegment: {segment}",
                recipe.id_suggestion
            );
        }
    }

    // Table → catalog: a row for a recipe with no note is a row describing a
    // claim nothing makes any more.
    for row in NOTE_ECHOES {
        let recipe = catalog
            .iter()
            .find(|r| r.id_suggestion == row.id)
            .unwrap_or_else(|| {
                panic!(
                    "`NOTE_ECHOES` has a row for `{}` and the catalog ships no such recipe. \
                     Remove the row; do not re-add the recipe to satisfy it.",
                    row.id
                )
            });
        assert!(
            recipe.notes.is_some(),
            "`NOTE_ECHOES` has a row for `{}` and that recipe carries no note, so this row \
             pins prose against nothing. Remove the row, or restore the note if it was \
             dropped by accident.",
            row.id
        );
    }
}

// ---------------------------------------------------------------------------
// The policy topic ↔ the category table (REQ-577 BR-2, third typed source)
// ---------------------------------------------------------------------------

/// The `teton_docs` policy topic, gated on the compiled-in category table.
const POLICY_TOPIC: &str = include_str!("../src/harness/docs/policy.md");

/// **Every category is documented on the tier it actually inherits from**
/// (REQ-577 BR-2).
///
/// `Category::tier()` is a `const fn` — a category's tier is compile-time, not
/// configuration — which makes the policy topic's four "Carries …" lines a
/// hand-written second spelling of a table the binary already ships. That is
/// precisely the shape this file exists to gate, and it is free: no third-party
/// fact, no verification round, just two spellings that must agree.
///
/// The claim is stronger than "appears somewhere": a category must appear under
/// **its own** tier and under no other. A user reading that `edit` is carried by
/// `think` binds the wrong tier and pays a frontier price for every file write,
/// and the topic would have been "correct" by any containment check.
#[test]
fn the_policy_topic_files_every_category_under_its_own_tier() {
    use teton_core::category::{Category, ConfigurableCategory, Tier};

    // The tier list, and *only* the tier list. Scoped to its own section first:
    // the `think` bullet is last, so a segment that ran to the end of the file
    // would swallow the binding examples and the "nine bindable categories"
    // sentence — and then contain every category name, which makes the
    // wrong-tier half of this test pass on any topic at all.
    const TIERS_HEADING: &str = "## The four tiers";
    let (_, after_heading) = POLICY_TOPIC.split_once(TIERS_HEADING).unwrap_or_else(|| {
        panic!(
            "the policy topic (crates/tetond/src/harness/docs/policy.md) no longer has a \
             `{TIERS_HEADING}` heading, so this check has nothing to scope to. Restore it, \
             or re-anchor — do not leave the tier table unchecked."
        )
    });
    let tier_list = after_heading.split("\n## ").next().unwrap_or(after_heading);

    let tier_segment = |tier: Tier| -> String {
        let needle = format!("- `{}`", tier.as_str());
        let (_, after) = tier_list.split_once(needle.as_str()).unwrap_or_else(|| {
            panic!(
                "the policy topic has no `{needle}` bullet under `{TIERS_HEADING}`, so a \
                 tier the binary ships is one the topic never describes. Add it, or \
                 re-anchor this check — do not leave the table unchecked."
            )
        });
        after.split("\n- `").next().unwrap_or(after).to_owned()
    };
    let segments: Vec<(Tier, String)> = Tier::ALL
        .into_iter()
        .map(|tier| (tier, tier_segment(tier)))
        .collect();
    // Non-vacuity: four bullets that between them name every category once. A
    // scoping mistake shows up here rather than as a silently permissive sweep.
    let named: usize = Category::ALL
        .into_iter()
        .filter(|c| {
            segments
                .iter()
                .any(|(_, seg)| seg.contains(&format!("`{}`", c.as_str())))
        })
        .count();
    assert_eq!(
        named,
        Category::ALL.len(),
        "the four tier bullets name {named} of {} categories between them; the section \
         parse is reading less than the topic holds.\nsegments: {segments:?}",
        Category::ALL.len()
    );

    for category in Category::ALL {
        let name = format!("`{}`", category.as_str());
        for (tier, segment) in &segments {
            let documented = segment.contains(name.as_str());
            let belongs = *tier == category.tier();
            assert_eq!(
                documented,
                belongs,
                "`Category::{category:?}` inherits the `{}` tier, and the policy topic's \
                 `{}` bullet {} it. A category filed under the wrong tier sends a reader \
                 to bind the wrong one — and every containment check in this file would \
                 still pass, because the name is present. Edit \
                 crates/tetond/src/harness/docs/policy.md; \
                 `Category::tier()` in teton-core is the source and it is a `const fn`, \
                 so it does not move for prose.\nsegment: {segment}",
                category.tier().as_str(),
                tier.as_str(),
                if documented { "names" } else { "omits" },
            );
        }
    }

    // The topic also counts the bindable categories out loud, and the count is
    // the kind of number that survives a variant being added.
    for category in ConfigurableCategory::ALL {
        assert!(
            POLICY_TOPIC.contains(&format!("`{}`", category.as_str())),
            "the policy topic never names the bindable category `{}`, so a user reading it \
             cannot reach a binding the CLI accepts.",
            category.as_str()
        );
    }
    assert!(
        POLICY_TOPIC.contains(&format!(
            "The {} bindable categories",
            spelled_out(ConfigurableCategory::ALL.len())
        )),
        "the policy topic's bindable-category count no longer matches \
         `ConfigurableCategory::ALL` ({}). Update the sentence — a wrong count reads as \
         authority and sends somebody hunting for a category that does not exist.",
        ConfigurableCategory::ALL.len()
    );
}

/// The English spelling of a small count, for the one sentence in the policy
/// topic that writes its number out.
fn spelled_out(n: usize) -> &'static str {
    match n {
        8 => "eight",
        9 => "nine",
        10 => "ten",
        11 => "eleven",
        _ => panic!(
            "the policy topic spells its category count in words and this helper has no \
             spelling for {n}; add one rather than switching the prose to a digit"
        ),
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

// ---------------------------------------------------------------------------
// The price table ↔ the recipe catalog's example models (REQ-577 follow-up)
// ---------------------------------------------------------------------------

/// **Every remote recipe's example model has a price row** (REQ-577 follow-up).
///
/// The gates above pin *prose* against the catalog. This one pins *data*:
/// `data/prices.toml` is keyed on the exact model string a provider declares
/// (REQ-557 ADR-A), and a row keyed on a retired vendor id does not error —
/// the calls it used to price silently become unpriced, so the cost meter
/// under-reports and nobody sees it. Exactly that happened: the table carried
/// `kimi-k2` for months after Moonshot discontinued the id, and by the time it
/// was swept every row in the file was dead. A recipe's example model is the
/// one string our own docs put into a user's config, so it is the one string
/// the table must always price.
///
/// Deliberately one-directional, unlike the prose gates: the table may price
/// models the catalog does not exemplify (pinned snapshots, cheaper tiers of
/// the same vendor), so a reverse sweep would forbid legitimate rows. The
/// local recipe is the exception, gated in the *other* direction: its example
/// model must NOT be priced, because the local tier is deliberately empty
/// (BUG-155) — keyed on the model alone, a row for it would bill any remote
/// gateway serving the same model name.
///
/// Red here? Re-verify the vendor's public pricing page first (the REQ-577
/// BR-3 discipline), then edit `data/prices.toml` — never the catalog — and
/// remember its own header rules: micro-USD per Mtok, no zero rows, no
/// duplicate model keys.
#[test]
fn the_price_table_and_the_recipe_catalogs_example_models_agree() {
    let table = PriceTable::bundled();
    for recipe in recipe_catalog() {
        // The local entry is the one whose endpoint is an on-device example
        // address; on-device inference is never metered, so its model stays
        // out of the table (BUG-155).
        let is_local = recipe
            .endpoint
            .as_deref()
            .is_some_and(|endpoint| endpoint.starts_with("http://localhost"));
        if is_local {
            assert!(
                table.entry(&recipe.example_model).is_none(),
                "the local recipe `{}` has a price row for its example model `{}`. The \
                 local tier of data/prices.toml is deliberately empty (BUG-155): lookup \
                 keys on the model alone, so this row would price any remote gateway \
                 declaring the same model name. Remove the row.",
                recipe.id_suggestion,
                recipe.example_model,
            );
        } else {
            assert!(
                table.entry(&recipe.example_model).is_some(),
                "recipe `{}`: example model `{}` has no row in data/prices.toml, so the \
                 cost meter silently under-reports for exactly the model our own recipes \
                 tell a user to type. Verify the vendor's current public pricing page, \
                 then add the row (micro-USD per Mtok) — do not edit the catalog to make \
                 this pass.",
                recipe.id_suggestion,
                recipe.example_model,
            );
        }
    }
}
