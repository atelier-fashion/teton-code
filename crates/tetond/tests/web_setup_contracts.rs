//! REQ-572 AC-8 / BR-9: **every backend the product suggests is one the
//! shipped request builder can actually drive** (TASK-133).
//!
//! The motivating defect is on the record: REQ-563 shipped a Bearer-only search
//! credential header while its own spec named example backends that want three
//! different shapes, so two of the three "suggestions" were endpoints this
//! daemon could reach and never authenticate against. BUG-165 replaced the fixed
//! header with the `[web] search_auth` template; what stops the same class of
//! defect returning is not the template but *this file* — a suggestion is only a
//! suggestion once a test drives the production builder against it.
//!
//! ## The enumeration is the point (AC-8's "a suggestion with no passing
//! contract test fails CI")
//!
//! A hand-maintained list of backends would drift from the shipped text the
//! first time someone added a fourth. So the list is **parsed out of the shipped
//! bytes** — the daemon's bundled guide and the CLI's own suggestion block, both
//! read with `include_str!` from the same files that ship — and every suggestion
//! found there must match a fixture below. Adding `Serper \`X-API-KEY: {key}\``
//! to `self_config.md`, or another URL to the flow's `ENDPOINT_HELP`, turns
//! [`every_suggested_auth_template_has_a_contract`] and
//! [`every_suggested_endpoint_has_a_contract`] red until the backend is driven
//! here.
//!
//! ## AC → test map
//!
//! | AC | Test |
//! |----|------|
//! | AC-8 (enumeration, daemon guide) | [`every_suggested_auth_template_has_a_contract`] |
//! | AC-8 (enumeration, flow suggestions) | [`every_suggested_endpoint_has_a_contract`] |
//! | AC-8 (request shape, per backend) | [`every_suggested_backend_drives_the_production_search_request`] |
//! | AC-8 (auth header shape, per backend) | [`every_suggested_backend_gets_the_header_it_documents`] |
//! | BR-9 (a suggestion is a config a user could write) | [`every_suggested_backend_is_a_config_this_daemon_would_load`] |
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

use teton_protocol::SessionId;
use tetond::egress::{
    Authorship, Egress, HttpTransport, LookupContext, LookupRequest, NoopSink, RedactionGate,
    RedactionVerdict, TaintView,
};

// ---------------------------------------------------------------------------
// The shipped text, read from the files that ship
// ---------------------------------------------------------------------------

/// The daemon's bundled setup guide — the same bytes `build_system_prompt`
/// embeds (BR-2), not a copy.
const BUNDLED_GUIDE: &str = include_str!("../src/harness/self_config.md");

/// The CLI's own suggestion block. Read as source text rather than linked,
/// because `teton` is a separate crate this one does not depend on — and the
/// alternative, trusting that the two lists agree, is the thing AC-8 exists to
/// stop.
const FLOW_SUGGESTIONS: &str = include_str!("../../teton/src/web_setup_ui.rs");

// ---------------------------------------------------------------------------
// The contracts
// ---------------------------------------------------------------------------

/// The fixture secret. Planted so an assertion about *where* it appears is a
/// search for a string nothing else could have produced.
const SECRET: &str = "fixture-search-key-9f3a";

/// One backend the product suggests, and the contract it documents.
struct Backend {
    /// The label the bundled guide gives it, lowercased.
    label: &'static str,
    /// The endpoint the flow suggests for it (or, for the unnamed default, a
    /// representative one). This is what the request builder is driven with.
    endpoint: &'static str,
    /// The `[web] search_auth` template, or `None` when the backend rides the
    /// daemon's default (which is itself one of the documented suggestions).
    auth: Option<&'static str>,
    /// Whether this backend takes a credential at all.
    keyed: bool,
    /// The header name the backend documents, lowercased — headers are
    /// case-insensitive on the wire and the shape parser lowercases them.
    header: &'static str,
    /// The header **value**, with the secret in the position the backend
    /// documents. This is the assertion the Bearer-only defect would have
    /// failed for two of the four rows.
    value: &'static str,
}

/// Every backend named in the bundled guide or suggested by the flow.
///
/// The endpoints for Brave and Kagi are the ones `web_setup_ui`'s
/// `ENDPOINT_HELP` puts in front of the user, verbatim — a contract test
/// against a *different* endpoint than the one suggested would be testing a
/// backend nobody was offered.
const BACKENDS: &[Backend] = &[
    Backend {
        // The template a config with no `search_auth` key gets (BUG-165's
        // continuity promise: every pre-template config keeps working).
        label: "default",
        endpoint: "https://search.example.org/api",
        auth: None,
        keyed: true,
        header: "authorization",
        value: "Bearer fixture-search-key-9f3a",
    },
    Backend {
        label: "brave",
        endpoint: "https://api.search.brave.com/res/v1/web/search",
        auth: Some("X-Subscription-Token: {key}"),
        keyed: true,
        header: "x-subscription-token",
        // Brave's header carries the bare key — no scheme word. A daemon that
        // sent `Bearer <key>` here would be refused by the backend, which is
        // exactly the failure BUG-165 was filed for.
        value: "fixture-search-key-9f3a",
    },
    Backend {
        label: "kagi",
        endpoint: "https://kagi.com/api/v0/search",
        auth: Some("Authorization: Bot {key}"),
        keyed: true,
        header: "authorization",
        value: "Bot fixture-search-key-9f3a",
    },
    Backend {
        label: "searxng",
        endpoint: "http://localhost:8888/search?format=json",
        auth: None,
        // The keyless one, and the reason the flow asks "does this backend need
        // an API key?" at all. Its contract is that no credential is composed.
        keyed: false,
        header: "",
        value: "",
    },
];

fn backend(label: &str) -> Option<&'static Backend> {
    BACKENDS.iter().find(|b| b.label == label)
}

/// The `[web]` table a user following this suggestion would end up with.
fn config_for(b: &Backend) -> WebConfig {
    WebConfig {
        tier: WebTier::Search,
        search_endpoint: Some(b.endpoint.to_owned()),
        search_key_ref: b.keyed.then(|| "keychain://teton/web-search".to_owned()),
        search_auth: b.auth.map(str::to_owned),
        ..WebConfig::default()
    }
}

// ---------------------------------------------------------------------------
// Parsing the shipped text
// ---------------------------------------------------------------------------

/// Every backtick-quoted span in `text` that is a **credential-header
/// template**: `Header-Name: …{key}…`, the grammar `search_auth_shape` parses.
///
/// The `{key}` marker alone is not a suggestion — both texts quote it on its
/// own while explaining what it means — so the header-name half is required.
/// That is also what keeps this parse honest in the other direction: anything
/// that would actually be *accepted* as a `search_auth` value is caught.
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

/// Every absolute `http(s)` URL inside the flow's `ENDPOINT_HELP` block.
///
/// Scoped to that constant rather than to the whole file so a URL in a doc
/// comment somewhere else is not read as a suggestion — what the user is
/// offered is what this list holds.
fn suggested_endpoints(source: &str) -> BTreeSet<String> {
    let start = source
        .find("const ENDPOINT_HELP")
        .expect("the flow's suggestion block must still be named `ENDPOINT_HELP`");
    let block = &source[start..];
    let end = block
        .find("];")
        .expect("the suggestion block must still be a terminated slice literal");
    block[..end]
        .split_whitespace()
        .filter(|token| token.starts_with("http://") || token.starts_with("https://"))
        .map(|token| token.trim_end_matches([',', '"', ')']).to_owned())
        .collect()
}

// ---------------------------------------------------------------------------
// The enumeration (AC-8's CI gate)
// ---------------------------------------------------------------------------

/// **A credential-header template the bundled guide suggests, with no contract
/// fixture, fails the suite.**
///
/// This is the enforcement half of AC-8. Not "the three we know about are
/// covered" — that claim decays the moment a fourth is added — but "everything
/// the shipped bytes suggest is covered", checked against the shipped bytes.
#[test]
fn every_suggested_auth_template_has_a_contract() {
    let suggested = suggested_auth_templates(BUNDLED_GUIDE);
    assert!(
        suggested.len() >= 3,
        "the guide must still name the auth templates it documents; found: {suggested:?}"
    );

    let covered: BTreeSet<String> = BACKENDS
        .iter()
        .filter_map(|b| b.auth.map(str::to_owned))
        // The default's template is documented in the guide as a literal too,
        // even though the config that gets it has no `search_auth` key at all.
        .chain(std::iter::once("Authorization: Bearer {key}".to_owned()))
        .collect();

    for template in &suggested {
        assert!(
            covered.contains(template),
            "AC-8: `{template}` is suggested by the bundled guide with no contract \
             test driving it. Add a row to `BACKENDS` in this file — a backend the \
             product names and CI never drives is the BUG-165 defect returning.\n\
             covered: {covered:?}"
        );
    }

    // The keyless suggestion is named the same way and needs the same cover.
    let keyless = BUNDLED_GUIDE
        .split(" needs none")
        .next()
        .and_then(|before| before.split_whitespace().next_back())
        .map(str::to_lowercase)
        .expect("the guide names its keyless backend");
    assert!(
        backend(&keyless).is_some(),
        "AC-8: the guide suggests `{keyless}` as a keyless backend with no contract \
         fixture"
    );
    // Non-vacuity: the parse found the backend the guide actually names, not an
    // empty string that trivially matches nothing.
    assert_eq!(keyless, "searxng", "the keyless parse read: {keyless:?}");
}

/// **An endpoint the flow suggests, with no contract fixture, fails the suite.**
///
/// The guide names header shapes; the walkthrough names *URLs*, and a URL the
/// request builder cannot form is as broken a suggestion as a header the
/// backend rejects. Both lists are read from the files that ship.
#[test]
fn every_suggested_endpoint_has_a_contract() {
    let suggested = suggested_endpoints(FLOW_SUGGESTIONS);
    assert!(
        suggested.len() >= 3,
        "the flow must still suggest the endpoints it documents; found: {suggested:?}"
    );

    let covered: BTreeSet<&str> = BACKENDS.iter().map(|b| b.endpoint).collect();
    for endpoint in &suggested {
        assert!(
            covered.contains(endpoint.as_str()),
            "AC-8: `{endpoint}` is offered by `/web setup` with no contract test \
             driving the production request builder against it.\ncovered: {covered:?}"
        );
    }

    // The templates the walkthrough quotes must be the same ones the daemon's
    // guide quotes — two suggestion lists that disagree are two products.
    let flow_templates = suggested_auth_templates(FLOW_SUGGESTIONS);
    let guide_templates = suggested_auth_templates(BUNDLED_GUIDE);
    for template in &flow_templates {
        assert!(
            guide_templates.contains(template),
            "the walkthrough suggests `{template}` and the bundled guide does not; \
             guide: {guide_templates:?}"
        );
    }
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
    for b in BACKENDS {
        let capture = CaptureTransport::default();
        let egress = Egress::new(capture.clone(), Vec::new(), Arc::new(NoopSink))
            // BR-14: without a scan installed a search is a block, not a skip,
            // so the contract could not be observed at all. This gate forwards,
            // which is the permissive control — what is under test here is the
            // request, not the scan.
            .with_search_redaction_gate(Arc::new(ForwardingGate) as Arc<dyn RedactionGate>);
        let flags = Untainted;
        let ctx = LookupContext::new(SessionId::from("contracts"), &flags, &allow_any_host)
            .with_search_endpoint(b.endpoint);

        egress
            .lookup(
                &LookupRequest::search("rust pin semantics", Authorship::UserPasted),
                &ctx,
            )
            .await;

        let sent = capture.sent();
        assert_eq!(
            sent.len(),
            1,
            "{}: the suggested endpoint must produce exactly one request",
            b.label
        );
        let request = &sent[0];
        assert_eq!(
            request.method,
            HttpMethod::Get,
            "{}: the search contract is a GET",
            b.label
        );
        assert!(
            request.body.is_empty(),
            "{}: and carries no body: {:?}",
            b.label,
            request.body
        );
        assert!(
            request.url.contains("q=rust"),
            "{}: the terms must ride as `q`: {}",
            b.label,
            request.url
        );
        // The endpoint's own path and parameters survive verbatim — the
        // property SearxNG's `?format=json` depends on, asserted for every
        // backend so a builder that special-cased one of them is visible.
        let (base, params) = b.endpoint.split_once('?').map_or((b.endpoint, ""), |p| p);
        assert!(
            request.url.starts_with(base),
            "{}: the suggested endpoint's path must survive: {}",
            b.label,
            request.url
        );
        for param in params.split('&').filter(|p| !p.is_empty()) {
            assert!(
                request.url.contains(param),
                "{}: the endpoint's own `{param}` must survive: {}",
                b.label,
                request.url
            );
        }
        assert!(
            request
                .headers
                .iter()
                .any(|(name, value)| name == "accept" && value == "application/json"),
            "{}: a search asks for JSON: {:?}",
            b.label,
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
            "{}: the request builder must compose no credential: {:?}",
            b.label,
            request.headers
        );
    }
}

/// **Every suggested backend gets the credential header its documentation
/// describes** — name and value, with the secret in the position that backend
/// puts it.
///
/// The two calls below are, verbatim, what `DaemonRuntime::search_auth` makes
/// with a resolved secret. A Bearer-only daemon passes the `default` and `kagi`
/// header *names* and fails both value assertions for `brave` — which is the
/// discrimination this test exists to have.
#[test]
fn every_suggested_backend_gets_the_header_it_documents() {
    for b in BACKENDS {
        let web = config_for(b);
        let shape = web.search_auth_shape();

        if !b.keyed {
            // A keyless backend has no `search_key_ref`, so `search_auth` returns
            // before a shape is ever consulted: no header is attached at all.
            assert!(
                web.search_key_ref.is_none(),
                "{}: the keyless suggestion must reference no key",
                b.label
            );
            continue;
        }

        let shape = shape.unwrap_or_else(|| {
            panic!(
                "{}: a suggested template must parse to a shape — a suggestion the \
                 daemon reads as `attach no credential` is a suggestion that 401s",
                b.label
            )
        });
        assert_eq!(
            shape.header, b.header,
            "{}: the credential must ride the header the backend documents",
            b.label
        );
        assert_eq!(
            shape.header_value(SECRET),
            b.value,
            "{}: the secret must sit where the backend documents it (this is the \
             assertion a Bearer-only daemon fails)",
            b.label
        );
        // And the endpoint-bound transport the daemon builds from that pair
        // accepts it — the last step before the wire.
        assert!(
            HttpTransport::for_lookup_with_endpoint_auth(
                b.endpoint,
                vec![(shape.header.clone(), shape.header_value(SECRET))],
            )
            .is_ok(),
            "{}: the lookup transport must bind this credential to this endpoint",
            b.label
        );
    }
}

/// **BR-9's other half: a suggestion is a config a user could actually write.**
///
/// A backend whose documented shape this daemon refuses at config load is not a
/// suggestion, it is a trap — the user follows the walkthrough, the commit is
/// rejected, and the product has recommended something it forbids.
#[test]
fn every_suggested_backend_is_a_config_this_daemon_would_load() {
    for b in BACKENDS {
        let config = Config {
            web: config_for(b),
            ..Config::default()
        };
        assert!(
            config.validate().is_ok(),
            "{}: the suggested shape must be one `Config::validate` accepts: {:?}",
            b.label,
            config.validate().err()
        );
        // And the secret is nowhere in the document — the reference is
        // (BR-6/REQ-563 BR-7). Asserted here because this is the one test that
        // renders a suggested backend's whole config.
        let document = config.to_toml().expect("the suggested config renders");
        assert!(
            !document.contains(SECRET),
            "{}: a rendered config must carry a reference, never a key:\n{document}",
            b.label
        );
    }
}

// ---------------------------------------------------------------------------
// Fixtures
// ---------------------------------------------------------------------------

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
