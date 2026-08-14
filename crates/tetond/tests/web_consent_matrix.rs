//! REQ-563 acceptance: the consent, gradation, allowlist, taint and
//! search/redact-coupling matrix (TASK-078).
//!
//! Its sibling `web_lookup_egress.rs` asks "did bytes leave"; this file asks
//! "who said they could". Both instruments are the same — a recording
//! `Transport` behind the real choke point, and a real
//! [`PermissionGate`](tetond::harness::permissions::PermissionGate) driven by a
//! task that answers its prompts the way a client does — because every claim
//! here is ultimately about a packet that did or did not happen.
//!
//! ## AC → test map
//!
//! | AC | Test |
//! |----|------|
//! | AC-2 (deny) | [`a_denied_lookup_puts_no_packet_on_the_wire`] |
//! | AC-2 (once) | [`allow_once_permits_exactly_one_lookup_and_asks_again`] |
//! | AC-2 (session) | [`allow_for_this_session_lasts_to_session_end_and_not_beyond`] |
//! | AC-2 (permanent) | [`enable_permanent_writes_a_ceiling_the_next_daemon_start_honours`] |
//! | AC-4 | [`tier_gradation_refusals_name_the_missing_tier`] |
//! | AC-9 (initial URL) | [`the_allowlist_constrains_model_chosen_destinations_only`] |
//! | AC-9 (redirect hop) | [`the_allowlist_constrains_a_redirect_target_through_the_production_hop_closure`] |
//! | AC-12 (notice, paste, override) | [`the_taint_notice_names_cause_and_effect_and_a_paste_still_works`] |
//! | AC-12 (cache exemption) | [`a_cached_page_is_served_in_a_tainted_session_and_the_same_url_uncached_is_not`] |
//! | AC-12 (RPC-only) | [`only_the_client_rpc_can_lift_the_restriction`] |
//! | AC-12 (no such tool) | [`no_tool_is_named_for_the_override_or_the_refresh`] |
//! | AC-13 (gate ⇔ tier) | [`a_search_with_no_gate_installed_is_a_block_not_a_skip`] |
//! | AC-13 (Unavailable) | [`an_unavailable_scan_blocks_the_query_and_sends_nothing`] |
//! | AC-13 (loaderless) | [`on_a_loaderless_build_the_real_search_gate_refuses_every_query`] |
//!
//! ## REQ-572 (TASK-133) — what the guided setup flow does *not* change here
//!
//! | AC | Test |
//! |----|------|
//! | REQ-572 BR-7 / AC-3 (a commit grants nothing) | [`a_setup_commit_enables_the_tier_and_answers_no_consent_question`] |
//! | REQ-572 AC-4 (no setup tool exists) | [`no_tool_is_named_for_the_setup_flow`] |
//!
//! The rest of REQ-572's matrix is elsewhere, and deliberately: `web_setup_flow.rs`
//! drives plan → preview → commit → live use against a spawned daemon that owns a
//! config file, `web_setup_contracts.rs` pins AC-8's suggested backends against
//! the production request builder, and AC-4's user-only gate is asserted at the
//! two seams that enforce it (`server.rs`'s mutation-checked unit tests and
//! `multi_client.rs`'s socket-level rejection + event). What belongs *here* are
//! the two REQ-572 claims this file's own subject answers: that enabling a
//! capability is not answering a question about a lookup, and — AC-4's
//! model-tool-call leg — that the tool registry a model can reach names no setup
//! method at all, which is the claim the override and refresh already make one
//! table up.
//!
//! ## Falsification (LESSON-479)
//!
//! Every refusal below is paired in the same test with the *permissive* run of
//! the same scenario, and that run asserts the lookup genuinely reached the
//! transport. Only the thing under test is flipped — a tier, an allowlist, a
//! session flag, a consent answer — and never production code.

use std::collections::VecDeque;
use std::path::PathBuf;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use serde_json::{json, Value};
use tokio::runtime::Handle;

use teton_core::category::CategoryTable;
use teton_core::config::{Config, WebConfig, WebTier};
use teton_protocol::events::WebTier as WireWebTier;
use teton_protocol::events::{
    BlockCause, Event, WebConsentScope, WebLookupOutcome, OPTION_ID_ENABLE_PERMANENT,
};
use teton_protocol::methods::{
    PermissionOutcome, RpcMethod, WebOverrideParams, WebSetupCommitParams, WebSetupPlanParams,
    WebSetupPreviewParams,
};
use teton_protocol::{RequestId, SessionId};
use teton_providers::transport::{Transport, TransportError, TransportRequest, TransportResponse};

use tetond::broadcast::EventBus;
use tetond::egress::{
    Authorship, Egress, LookupContext, LookupDetail, LookupRecord, LookupRecorder, LookupRequest,
    NoopSink, RedactionGate, RedactionVerdict, TaintView,
};
use tetond::harness::permissions::{
    PendingPermissions, PermissionConfig, PermissionGate, PermissionPolicy, WebTierPersistence,
};
use tetond::harness::tools::web::{
    register_web_tool, SeamError, WebLookupSeam, PERMISSION_KEY_FETCH_ANY_URL,
    PERMISSION_KEY_FETCH_USER_URL, WEB_TOOL_NAME,
};
use tetond::harness::{Tool, ToolContext, ToolOutcome, ToolRegistry};
use tetond::router::Router;
use tetond::runtime::DaemonRuntime;
use tetond::web::{UserUrls, WebCache};

// ---------------------------------------------------------------------------
// Fixtures
// ---------------------------------------------------------------------------

type ScriptedAnswer = Result<(u16, Option<String>, Vec<u8>), TransportError>;

/// A `Transport` that records every request instead of sending it — the
/// `egress_capture.rs` instrument, with a script so a lookup can be answered.
#[derive(Clone, Default)]
struct LookupCapture {
    sent: Arc<Mutex<Vec<TransportRequest>>>,
    script: Arc<Mutex<VecDeque<ScriptedAnswer>>>,
}

impl LookupCapture {
    fn answering(body: &str) -> Self {
        Self {
            sent: Arc::new(Mutex::new(Vec::new())),
            script: Arc::new(Mutex::new(
                std::iter::repeat_n(Ok((200, None, body.as_bytes().to_vec())), 8).collect(),
            )),
        }
    }

    /// A transport that answers from `script` in order — what a redirect chain
    /// needs and [`Self::answering`] cannot express: the `Location` a hop
    /// follows is per-answer.
    ///
    /// An exhausted script keeps answering `200` with an empty body rather than
    /// panicking, so a test asserting "there was no second request" reports the
    /// second request rather than "the fixture ran out".
    fn scripted(script: Vec<ScriptedAnswer>) -> Self {
        Self {
            sent: Arc::new(Mutex::new(Vec::new())),
            script: Arc::new(Mutex::new(script.into_iter().collect())),
        }
    }

    fn calls(&self) -> usize {
        self.sent.lock().unwrap().len()
    }

    fn urls(&self) -> Vec<String> {
        self.sent
            .lock()
            .unwrap()
            .iter()
            .map(|r| r.url.clone())
            .collect()
    }
}

#[async_trait]
impl Transport for LookupCapture {
    async fn execute(
        &self,
        request: TransportRequest,
    ) -> Result<TransportResponse, TransportError> {
        self.sent.lock().unwrap().push(request);
        let next = self
            .script
            .lock()
            .unwrap()
            .pop_front()
            .unwrap_or(Ok((200, None, Vec::new())));
        let (status, location, body) = next?;
        Ok(TransportResponse {
            status,
            location,
            body: Box::pin(futures::stream::once(async move { Ok(body) })),
        })
    }
}

/// The two session flags the taint gate reads.
#[derive(Default)]
struct Flags {
    tainted: bool,
    overridden: bool,
}

impl TaintView for Flags {
    fn is_tainted(&self, _session: &SessionId) -> bool {
        self.tainted
    }

    fn is_overridden(&self, _session: &SessionId) -> bool {
        self.overridden
    }
}

#[derive(Default)]
struct Recorder {
    records: Mutex<Vec<LookupRecord>>,
}

impl Recorder {
    fn outcomes(&self) -> Vec<WebLookupOutcome> {
        self.records
            .lock()
            .unwrap()
            .iter()
            .map(|r| r.outcome)
            .collect()
    }
}

impl LookupRecorder for Recorder {
    fn web_lookup(&self, _session_id: &SessionId, record: &LookupRecord) {
        self.records.lock().unwrap().push(record.clone());
    }
}

/// The harness-facing seam over the real choke point.
struct CaptureSeam {
    egress: Egress<LookupCapture>,
    taint: Flags,
    session: SessionId,
    endpoint: Option<String>,
    recorder: Arc<Recorder>,
}

#[async_trait]
impl WebLookupSeam for CaptureSeam {
    async fn lookup(
        &self,
        request: &LookupRequest,
        hop_allowed: &(dyn for<'h> Fn(&'h str) -> bool + Send + Sync),
    ) -> Result<tetond::egress::LookupOutcome, SeamError> {
        let mut ctx = LookupContext::new(self.session.clone(), &self.taint, hop_allowed);
        if let Some(endpoint) = &self.endpoint {
            ctx = ctx.with_search_endpoint(endpoint);
        }
        Ok(self.egress.lookup(request, &ctx).await)
    }

    fn record_without_egress(&self, record: &LookupRecord) {
        self.recorder.web_lookup(&self.session, record);
    }
}

/// A gate that forwards everything.
struct ForwardingGate;

#[async_trait]
impl RedactionGate for ForwardingGate {
    async fn scan(&self, _payload: &str) -> RedactionVerdict {
        RedactionVerdict::clean()
    }
}

/// A gate whose scan **could not run** — the verdict a loaderless machine's
/// composite scanner mints (runtime.rs pins that equivalence).
struct UnavailableGate;

#[async_trait]
impl RedactionGate for UnavailableGate {
    async fn scan(&self, _payload: &str) -> RedactionVerdict {
        RedactionVerdict::unavailable()
    }
}

/// Records the tier `enable_permanent` asked to persist, and writes it to a
/// real config file through the **production** serializer.
///
/// The daemon's own sink is `DaemonRuntime::persist_web_tier` (whose atomic
/// write and validation are covered by its unit tests). What this proves is the
/// half a unit test of that function cannot: that the tier the *consent round
/// trip* hands over is the tier a later start reads back.
struct FileTierSink {
    path: PathBuf,
    asked: Mutex<Vec<WebTier>>,
}

impl WebTierPersistence for FileTierSink {
    /// Mirrors `DaemonRuntime::persist_web_tier`'s two effects: the tier is
    /// **appended** to the per-tier consent list (never fanned out), and the
    /// ceiling is raised only if it was lower.
    fn persist_web_tier(&self, tier: WebTier) -> Result<(), String> {
        self.asked.lock().unwrap().push(tier);
        let mut config = std::fs::read_to_string(&self.path)
            .ok()
            .and_then(|text| Config::load(&text).ok())
            .unwrap_or_default();
        if !config.web.permission_allow.contains(&tier) {
            config.web.permission_allow.push(tier);
        }
        if config.web.tier < tier {
            config.web.tier = tier;
        }
        let toml = config.to_toml().map_err(|e| e.to_string())?;
        std::fs::write(&self.path, toml).map_err(|e| e.to_string())?;
        Ok(())
    }
}

/// One consent question, as it reached a client: the grant key it was asked
/// under, the description the user reads, and the option ids on offer.
#[derive(Clone)]
struct Prompt {
    key: String,
    description: String,
    options: Vec<String>,
    /// The human-readable label beside each option id, in the same order. What a
    /// user actually reads — and therefore the thing an "enable permanently"
    /// promise has to be honest in.
    labels: Vec<String>,
}

/// A task that answers permission prompts the way a client does.
///
/// It records every prompt it saw — the tool name, the description, and the
/// option ids — which is what AC-2's "the prompt shows the verbatim query/URL
/// and the destination host" is asserted against.
struct Answerer {
    prompts: Arc<Mutex<Vec<Prompt>>>,
    handle: tokio::task::JoinHandle<()>,
}

impl Answerer {
    /// Answer prompts with `script`, in order; a prompt past the end of the
    /// script is cancelled (which the gate reads as a denial).
    fn spawn(
        bus: &Arc<EventBus>,
        pending: &Arc<PendingPermissions>,
        script: Vec<&'static str>,
    ) -> Self {
        let mut sub = bus.subscribe(64);
        let prompts = Arc::new(Mutex::new(Vec::new()));
        let seen = Arc::clone(&prompts);
        let pending = Arc::clone(pending);
        let handle = tokio::spawn(async move {
            let mut script = script.into_iter();
            while let Some(envelope) = sub.recv().await {
                let Event::PermissionRequest(request) = envelope.event else {
                    continue;
                };
                seen.lock().unwrap().push(Prompt {
                    key: request.tool_name.clone(),
                    description: request.description.clone().unwrap_or_default(),
                    options: request
                        .options
                        .iter()
                        .map(|o| o.option_id.clone())
                        .collect(),
                    labels: request.options.iter().map(|o| o.label.clone()).collect(),
                });
                answer(&pending, &request.request_id, script.next());
            }
        });
        Self { prompts, handle }
    }

    fn prompts(&self) -> Vec<Prompt> {
        self.prompts.lock().unwrap().clone()
    }

    fn count(&self) -> usize {
        self.prompts.lock().unwrap().len()
    }
}

impl Drop for Answerer {
    fn drop(&mut self) {
        self.handle.abort();
    }
}

fn answer(pending: &PendingPermissions, id: &RequestId, option: Option<&'static str>) {
    let outcome = match option {
        Some(option_id) => PermissionOutcome::Selected {
            option_id: option_id.to_owned(),
        },
        None => PermissionOutcome::Cancelled,
    };
    pending.resolve(id, outcome);
}

fn scratch(tag: &str) -> PathBuf {
    static SEQ: AtomicUsize = AtomicUsize::new(0);
    let dir = std::env::temp_dir().join(format!(
        "teton-webconsent-{tag}-{}-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos(),
        SEQ.fetch_add(1, Ordering::SeqCst)
    ));
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

/// A tool, its transport, its recorder, and everything needed to answer its
/// prompts — the whole of one machine's web capability, assembled from the real
/// parts.
struct Fixture {
    tool: Arc<dyn Tool>,
    tools: ToolRegistry,
    transport: LookupCapture,
    recorder: Arc<Recorder>,
    bus: Arc<EventBus>,
    pending: Arc<PendingPermissions>,
    ctx: ToolContext,
    dir: PathBuf,
    registered: bool,
    /// The `[web]` table this fixture's tool was built from, kept so a test can
    /// open **the same** cache the tool reads — `WebCache::from_config` is a
    /// path and a TTL, so a second handle over the same directory and config is
    /// the same cache (the equivalence `web_lookup_egress.rs`'s refresh leg
    /// relies on).
    config: WebConfig,
}

impl Fixture {
    fn run(&self, args: &Value) -> ToolOutcome {
        self.tool.run(&self.ctx, args)
    }

    fn cache(&self) -> WebCache {
        WebCache::from_config(&self.dir, &self.config)
    }

    fn fetch(&self, url: &str) -> ToolOutcome {
        self.run(&json!({ "url": url }))
    }

    fn search(&self, query: &str) -> ToolOutcome {
        self.run(&json!({ "query": query }))
    }

    fn cleanup(self) {
        std::fs::remove_dir_all(&self.dir).ok();
    }
}

/// How a fixture's machine is configured. Each field is one thing an assertion
/// below flips.
struct Setup {
    tier: WebTier,
    policy: PermissionPolicy,
    allowed_domains: Option<Vec<String>>,
    search_endpoint: Option<String>,
    pasted: Vec<String>,
    tainted: bool,
    overridden: bool,
    persistence: Option<Arc<FileTierSink>>,
    /// `[web] permission_allow` as a restarted daemon would read it — the tiers
    /// an earlier `enable_permanent` already answered for.
    consented: Vec<WebTier>,
    search_gate: Option<Arc<dyn RedactionGate>>,
    /// What the network answers, in order. `None` is the default fixture's
    /// "every request is `200 <a page>`".
    script: Option<Vec<ScriptedAnswer>>,
}

impl Setup {
    fn at(tier: WebTier) -> Self {
        Self {
            tier,
            policy: PermissionPolicy::Allow,
            allowed_domains: None,
            search_endpoint: None,
            pasted: Vec::new(),
            tainted: false,
            overridden: false,
            persistence: None,
            consented: Vec::new(),
            search_gate: None,
            script: None,
        }
    }

    /// Script the network's answers — a redirect chain, in practice.
    fn answering(mut self, script: Vec<ScriptedAnswer>) -> Self {
        self.script = Some(script);
        self
    }

    fn policy(mut self, policy: PermissionPolicy) -> Self {
        self.policy = policy;
        self
    }

    fn allowing(mut self, domains: &[&str]) -> Self {
        self.allowed_domains = Some(domains.iter().map(|d| (*d).to_owned()).collect());
        self
    }

    fn pasted(mut self, url: &str) -> Self {
        self.pasted.push(url.to_owned());
        self
    }

    fn tainted(mut self) -> Self {
        self.tainted = true;
        self
    }

    fn overridden(mut self) -> Self {
        self.overridden = true;
        self
    }

    fn searching(mut self, endpoint: &str, gate: Arc<dyn RedactionGate>) -> Self {
        self.search_endpoint = Some(endpoint.to_owned());
        self.search_gate = Some(gate);
        self
    }

    fn persisting(mut self, sink: Arc<FileTierSink>) -> Self {
        self.persistence = Some(sink);
        self
    }

    /// Start this session as a restarted daemon would, having read
    /// `[web] permission_allow` from config.
    fn consented(mut self, allow: &[WebTier]) -> Self {
        self.consented = allow.to_vec();
        self
    }

    fn build(self, tag: &str) -> Fixture {
        let dir = scratch(tag);
        let config = WebConfig {
            tier: self.tier,
            allowed_domains: self.allowed_domains,
            search_endpoint: self.search_endpoint.clone(),
            ..WebConfig::default()
        };

        let transport = self.script.map_or_else(
            || LookupCapture::answering("<html><body>a page</body></html>"),
            LookupCapture::scripted,
        );
        let recorder = Arc::new(Recorder::default());
        let mut egress = Egress::new(transport.clone(), Vec::new(), Arc::new(NoopSink))
            .with_lookup_recorder(Arc::clone(&recorder) as Arc<dyn LookupRecorder>);
        if let Some(gate) = self.search_gate {
            egress = egress.with_search_redaction_gate(gate);
        }

        let session_id = SessionId::from("web-consent");
        let seam = Arc::new(CaptureSeam {
            egress,
            taint: Flags {
                tainted: self.tainted,
                overridden: self.overridden,
            },
            session: session_id.clone(),
            endpoint: self.search_endpoint,
            recorder: Arc::clone(&recorder),
        });

        let bus = Arc::new(EventBus::new());
        let pending = Arc::new(PendingPermissions::new());
        // The production fold: config's consent list becomes policy rows here
        // and nowhere else, one listed tier to its own key.
        let mut permissions = PermissionConfig::with_default(self.policy);
        permissions.apply_web_permission(&self.consented);
        let mut gate = PermissionGate::new(
            session_id,
            permissions,
            Arc::clone(&bus),
            Arc::clone(&pending),
        );
        if let Some(sink) = self.persistence {
            gate = gate.with_web_persistence(sink as Arc<dyn WebTierPersistence>);
        }

        let mut urls = UserUrls::new();
        for url in &self.pasted {
            urls.insert(url);
        }

        let mut tools = ToolRegistry::with_builtins();
        let registered = register_web_tool(
            &mut tools,
            &config,
            WebCache::from_config(&dir, &config),
            Arc::new(Mutex::new(urls)),
            Arc::new(gate),
            Arc::clone(&seam) as Arc<dyn WebLookupSeam>,
            Handle::current(),
        );
        let tool = Arc::clone(
            tools
                .get(WEB_TOOL_NAME)
                .expect("a tier above `off` registers the tool"),
        );

        Fixture {
            tool,
            tools,
            transport,
            recorder,
            bus,
            pending,
            ctx: ToolContext::new(&dir),
            dir,
            registered,
            config,
        }
    }
}

// ---------------------------------------------------------------------------
// AC-2 — the consent scopes
// ---------------------------------------------------------------------------

const DOCS_URL: &str = "https://docs.rs/tokio/latest/tokio/";

/// AC-2, deny: the user declines and **no packet leaves** — plus the prompt
/// they declined showed the verbatim URL and the destination host.
///
/// Falsified in place: the same fixture, the same URL, answered `allow_once`
/// instead, does reach the transport.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_denied_lookup_puts_no_packet_on_the_wire() {
    let fx = Setup::at(WebTier::FetchAnyUrl)
        .policy(PermissionPolicy::Ask)
        .build("deny");
    let answerer = Answerer::spawn(&fx.bus, &fx.pending, vec!["reject_once", "allow_once"]);

    // --- the denial --------------------------------------------------------
    let denied = fx.fetch(DOCS_URL);
    assert!(denied.is_error, "a declined lookup is not a result");
    assert!(
        denied.content.contains("Permission denied"),
        "{}",
        denied.content
    );
    assert_eq!(
        fx.transport.calls(),
        0,
        "AC-2: a denied lookup must put no packet on the wire"
    );
    assert!(
        fx.recorder.outcomes().is_empty(),
        "a refusal the user made is the `permission_request`'s record, not a lookup row"
    );

    // BR-4: the question was concrete — the verbatim URL and the host.
    let prompts = answerer.prompts();
    assert_eq!(prompts.len(), 1, "exactly one question was asked");
    let asked = &prompts[0];
    assert_eq!(
        asked.key, PERMISSION_KEY_FETCH_ANY_URL,
        "BR-3: the grant key is the tier's, never the tool's name — and the model \
         composed this URL, so it is the any-url tier's key and not the pasted one's"
    );
    assert!(
        asked.description.contains(DOCS_URL),
        "the prompt must show the verbatim URL: {}",
        asked.description
    );
    assert!(
        asked.description.contains("docs.rs"),
        "the prompt must name the destination host: {}",
        asked.description
    );
    assert!(
        asked
            .options
            .contains(&OPTION_ID_ENABLE_PERMANENT.to_owned()),
        "BR-4's four choices include enabling permanently: {:?}",
        asked.options
    );

    // --- falsification: the same lookup, allowed ---------------------------
    let allowed = fx.fetch(DOCS_URL);
    assert!(!allowed.is_error, "{}", allowed.content);
    assert_eq!(
        fx.transport.calls(),
        1,
        "the allow leg has to send, or the zero above measures nothing"
    );
    assert_eq!(
        answerer.count(),
        2,
        "and it was asked again, not remembered"
    );

    fx.cleanup();
}

/// AC-2, allow-once: one answer buys exactly one lookup, and the next one asks
/// again.
///
/// The second question is the falsification: a grant that had silently widened
/// to the session would show one prompt and two lookups.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn allow_once_permits_exactly_one_lookup_and_asks_again() {
    let fx = Setup::at(WebTier::FetchAnyUrl)
        .policy(PermissionPolicy::Ask)
        .build("once");
    // One `allow_once`, then nothing: the second prompt is cancelled.
    let answerer = Answerer::spawn(&fx.bus, &fx.pending, vec!["allow_once"]);

    let first = fx.fetch(DOCS_URL);
    assert!(!first.is_error, "{}", first.content);
    assert_eq!(fx.transport.calls(), 1);

    // The cache would answer the second lookup for free, so ask for a
    // *different* URL — otherwise this would be measuring BR-12, not BR-3.
    let second = fx.fetch("https://docs.rs/serde/latest/serde/");
    assert!(second.is_error, "an unanswered prompt must not allow");
    assert_eq!(
        fx.transport.calls(),
        1,
        "allow-once bought exactly one lookup"
    );
    assert_eq!(answerer.count(), 2, "and the second lookup asked again");

    fx.cleanup();
}

/// AC-2, allow-for-session: the grant lasts to session end **and not beyond**.
///
/// "Not beyond" is asserted against a second gate — which is what a second
/// session is, since grants live on the gate and nowhere else. It is the
/// falsification leg too: without it, "no second prompt" could be a gate that
/// stopped prompting altogether.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn allow_for_this_session_lasts_to_session_end_and_not_beyond() {
    let fx = Setup::at(WebTier::FetchAnyUrl)
        .policy(PermissionPolicy::Ask)
        .build("session");
    let answerer = Answerer::spawn(&fx.bus, &fx.pending, vec!["allow_always"]);

    let first = fx.fetch(DOCS_URL);
    assert!(!first.is_error, "{}", first.content);
    let second = fx.fetch("https://docs.rs/serde/latest/serde/");
    assert!(!second.is_error, "{}", second.content);

    assert_eq!(fx.transport.calls(), 2, "both lookups ran");
    assert_eq!(
        answerer.count(),
        1,
        "the second lookup was covered by the session grant"
    );

    // A grant is remembered under the *key it was asked about*, so a search
    // never inherits a fetch's answer (BR-3). Asked here because the whole
    // point of two keys is that this stays false.
    let searched = fx.search("tokio task pinning");
    assert!(
        searched.is_error,
        "a fetch grant must not authorize a search: {}",
        searched.content
    );

    // --- "not beyond": a fresh session is a fresh gate --------------------
    let next_session = Setup::at(WebTier::FetchAnyUrl)
        .policy(PermissionPolicy::Ask)
        .build("session-next");
    let next_answerer = Answerer::spawn(
        &next_session.bus,
        &next_session.pending,
        vec!["reject_once"],
    );
    let after = next_session.fetch(DOCS_URL);
    assert!(
        after.is_error,
        "a session grant must not survive the session: {}",
        after.content
    );
    assert_eq!(
        next_answerer.count(),
        1,
        "the new session asked the question again"
    );
    assert_eq!(next_session.transport.calls(), 0);

    fx.cleanup();
    next_session.cleanup();
}

/// AC-2, enable-permanently: the answer writes the ceiling to config, the
/// decision is announced at the scope it actually achieved, and a **later
/// start reading that file** has the capability.
///
/// The last clause is the one that matters and the one a memory-only test could
/// not make: the written bytes are parsed back through the production loader,
/// and `register_web_tool` — the single place the "is this machine opted in"
/// condition is expressed (D-1) — is asked about the reloaded config. It
/// answered `false` before the consent and must answer `true` after.
///
/// **Also the REQ-576 ADR-3 no-regression pin.** `config/set` became a BR-10(b)
/// commitment (REQ-576), but this consent-path `enable_permanent` write was
/// deliberately **not** brought under presence (raise-only, can't author an
/// endpoint; gating it would prompt on the ordinary "yes, permanently" answer).
/// This test proves that acceptance holds: the path persists with **no presence
/// step** — a future edit that accidentally gated it would make this go red.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn enable_permanent_writes_a_ceiling_the_next_daemon_start_honours() {
    let dir = scratch("permanent");
    let config_path = dir.join("config.toml");
    let sink = Arc::new(FileTierSink {
        path: config_path.clone(),
        asked: Mutex::new(Vec::new()),
    });

    // Non-vacuity: this machine is genuinely opted out before the answer.
    let mut before = ToolRegistry::with_builtins();
    assert!(
        !register_web_tool(
            &mut before,
            &WebConfig::default(),
            WebCache::from_config(&dir, &WebConfig::default()),
            Arc::new(Mutex::new(UserUrls::new())),
            Arc::new(PermissionGate::new(
                SessionId::from("before"),
                PermissionConfig::permissive(),
                Arc::new(EventBus::new()),
                Arc::new(PendingPermissions::new()),
            )),
            Arc::new(CaptureSeam {
                egress: Egress::new(LookupCapture::default(), Vec::new(), Arc::new(NoopSink)),
                taint: Flags::default(),
                session: SessionId::from("before"),
                endpoint: None,
                recorder: Arc::new(Recorder::default()),
            }) as Arc<dyn WebLookupSeam>,
            Handle::current(),
        ),
        "BR-1: a default machine has no web tool"
    );

    // The session that answers "permanently". Its own ceiling is `fetch_any_url`
    // — the consent option can only ever persist a tier the lookup was already
    // entitled to.
    let fx = Setup::at(WebTier::FetchAnyUrl)
        .policy(PermissionPolicy::Ask)
        .persisting(Arc::clone(&sink))
        .build("permanent-session");
    let mut events = fx.bus.subscribe(32);
    let answerer = Answerer::spawn(&fx.bus, &fx.pending, vec![OPTION_ID_ENABLE_PERMANENT]);

    let out = fx.fetch(DOCS_URL);
    assert!(!out.is_error, "{}", out.content);
    assert_eq!(fx.transport.calls(), 1);
    assert_eq!(answerer.count(), 1);

    // The tier handed to the sink is the one this lookup needed.
    assert_eq!(
        sink.asked.lock().unwrap().as_slice(),
        &[WebTier::FetchAnyUrl],
        "the write must be about the tier the user was asked about"
    );

    // The decision is announced at the scope it achieved — `persistent`, because
    // the write landed.
    let mut decided = None;
    while let Some(envelope) = events.try_recv() {
        if let Event::WebConsentDecided(d) = envelope.event {
            decided = Some(d);
        }
    }
    let decided = decided.expect("a web consent decision was published");
    assert!(decided.granted);
    assert_eq!(decided.scope, WebConsentScope::Persistent);

    // --- the restart ------------------------------------------------------
    let written = std::fs::read_to_string(&config_path).expect("the config was written");
    let reloaded = Config::load(&written).expect("the written config loads and validates");
    assert_eq!(
        reloaded.web.tier,
        WebTier::FetchAnyUrl,
        "`[web] tier` did not survive the round trip; file:\n{written}"
    );

    let mut after = ToolRegistry::with_builtins();
    assert!(
        register_web_tool(
            &mut after,
            &reloaded.web,
            WebCache::from_config(&dir, &reloaded.web),
            Arc::new(Mutex::new(UserUrls::new())),
            Arc::new(PermissionGate::new(
                SessionId::from("after"),
                PermissionConfig::permissive(),
                Arc::new(EventBus::new()),
                Arc::new(PendingPermissions::new()),
            )),
            Arc::new(CaptureSeam {
                egress: Egress::new(LookupCapture::default(), Vec::new(), Arc::new(NoopSink)),
                taint: Flags::default(),
                session: SessionId::from("after"),
                endpoint: None,
                recorder: Arc::new(Recorder::default()),
            }) as Arc<dyn WebLookupSeam>,
            Handle::current(),
        ),
        "the next start must honour the ceiling the consent wrote"
    );

    fx.cleanup();
    std::fs::remove_dir_all(&dir).ok();
}

/// **The `enable_permanent` label names the key that is actually written, and
/// the answer un-asks exactly the tier it was given at** (REQ-563 BR-3, BR-4).
///
/// Two defects, one round trip, because they were one defect: the option
/// promised `[web] tier = "…"` — the raise-only ceiling, which is checked
/// *before* any prompt exists and so is a guaranteed no-op for every prompt a
/// user can reach — while the thing that actually changed was the consent
/// posture, and that change applied to **all three** tiers at once. A user who
/// answered a question about a URL they had pasted themselves got, permanently
/// and on every future session, no more questions about URLs the *model*
/// composes or about searches.
///
/// So the assertion is in two halves that have to agree: what the label claims,
/// and what a restart reads back. A label naming a key nothing writes is exactly
/// as bad as a write nobody was told about.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn enable_permanent_names_the_key_it_writes_and_un_asks_only_that_tier() {
    let dir = scratch("permanent-per-tier");
    let config_path = dir.join("config.toml");
    let sink = Arc::new(FileTierSink {
        path: config_path.clone(),
        asked: Mutex::new(Vec::new()),
    });

    // A machine at the top of the ladder, so nothing below is refused by tier:
    // whatever still prompts, prompts because consent says so. The ceiling is
    // already in the file, as it would be on a real machine — the sink appends
    // to what is there rather than writing a config out of nothing.
    std::fs::write(
        &config_path,
        "[web]\ntier = \"search\"\nsearch_endpoint = \"https://search.example/api\"\n",
    )
    .expect("seed the config");

    const PASTED: &str = "https://pasted.example/doc";
    let fx = Setup::at(WebTier::Search)
        .policy(PermissionPolicy::Ask)
        .pasted(PASTED)
        .persisting(Arc::clone(&sink))
        .build("permanent-per-tier-session");
    let answerer = Answerer::spawn(&fx.bus, &fx.pending, vec![OPTION_ID_ENABLE_PERMANENT]);

    let out = fx.fetch(PASTED);
    assert!(!out.is_error, "{}", out.content);

    // --- half one: the label ----------------------------------------------
    let prompt = answerer.prompts().first().cloned().expect("one prompt");
    assert_eq!(
        prompt.key, PERMISSION_KEY_FETCH_USER_URL,
        "the question was asked under the pasted-URL key"
    );
    let permanent = prompt
        .options
        .iter()
        .position(|id| id == OPTION_ID_ENABLE_PERMANENT)
        .map(|at| prompt.labels[at].clone())
        .expect("the permanent option is offered when a tier is in hand");
    assert!(
        permanent.contains("permission_allow"),
        "the label must name the key the answer writes: {permanent}"
    );
    assert!(
        !permanent.contains("tier ="),
        "the label promises `[web] tier`, which this answer does not change: {permanent}"
    );
    assert!(
        permanent.contains("fetch_user_url"),
        "and the tier it writes, so consent is concrete (BR-4): {permanent}"
    );

    // --- half two: what the file says, and what a restart does with it -----
    assert_eq!(
        sink.asked.lock().unwrap().as_slice(),
        &[WebTier::FetchUserUrl]
    );
    let written = std::fs::read_to_string(&config_path).expect("the config was written");
    let reloaded = Config::load(&written).expect("the written config loads and validates");
    assert_eq!(
        reloaded.web.permission_allow,
        vec![WebTier::FetchUserUrl],
        "one answer, one tier; file:\n{written}"
    );

    // The restart: a fresh session built from what the file says. The consented
    // tier runs with no prompt at all...
    let next = Setup::at(reloaded.web.tier)
        .policy(PermissionPolicy::Ask)
        .consented(&reloaded.web.permission_allow)
        .pasted(PASTED)
        .searching("https://search.example/api", Arc::new(ForwardingGate))
        .build("permanent-per-tier-restart");
    let next_answerer = Answerer::spawn(&next.bus, &next.pending, vec!["reject_once"]);

    let unprompted = next.fetch(PASTED);
    assert!(!unprompted.is_error, "{}", unprompted.content);
    assert_eq!(
        next_answerer.count(),
        0,
        "the tier the user enabled permanently asked again"
    );

    // ...and the two tiers nobody answered for still ask. Answered `reject_once`
    // above, so the refusal here *is* the prompt having been raised.
    let model_composed = next.fetch("https://model-chose.example/page");
    assert!(
        model_composed.is_error && model_composed.content.contains("Permission denied"),
        "a `fetch_user_url` consent silently granted `fetch_any_url`: {}",
        model_composed.content
    );
    assert_eq!(next_answerer.count(), 1);

    let searched = next.search("rust lifetimes");
    assert!(
        searched.is_error && searched.content.contains("Permission denied"),
        "a `fetch_user_url` consent silently granted `search`: {}",
        searched.content
    );
    assert_eq!(next_answerer.count(), 2);

    fx.cleanup();
    next.cleanup();
    std::fs::remove_dir_all(&dir).ok();
}

// ---------------------------------------------------------------------------
// AC-4 — tier gradation
// ---------------------------------------------------------------------------

/// AC-4: a lookup above the configured ceiling is refused **naming the missing
/// tier**, before any prompt and before any packet.
///
/// Both rungs of the ladder, and both falsified by the rung above: the same
/// call at the next tier up proceeds, so the refusal is the ceiling's and not
/// the tool's.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn tier_gradation_refusals_name_the_missing_tier() {
    // --- fetch_user_url refuses a model-composed URL -----------------------
    let floor = Setup::at(WebTier::FetchUserUrl)
        .policy(PermissionPolicy::Ask)
        .build("tier-floor");
    let floor_answerer = Answerer::spawn(&floor.bus, &floor.pending, vec!["allow_once"]);

    let refused = floor.fetch(DOCS_URL);
    assert!(refused.is_error);
    assert!(
        refused.content.contains("fetch_any_url"),
        "the refusal must name the tier this needs: {}",
        refused.content
    );
    assert!(
        refused.content.contains("fetch_user_url"),
        "and the tier the machine granted: {}",
        refused.content
    );
    assert_eq!(floor.transport.calls(), 0);
    assert_eq!(
        floor_answerer.count(),
        0,
        "a lookup nobody was going to allow costs zero prompts (D-5)"
    );
    assert_eq!(
        floor.recorder.outcomes(),
        vec![WebLookupOutcome::RefusedTier],
        "AC-4's refusal is invisible unless it is recorded"
    );

    // Falsification A: the same URL, pasted by the user, clears the same floor.
    let pasted = Setup::at(WebTier::FetchUserUrl)
        .pasted(DOCS_URL)
        .build("tier-pasted");
    let allowed = pasted.fetch(DOCS_URL);
    assert!(!allowed.is_error, "{}", allowed.content);
    assert_eq!(pasted.transport.calls(), 1);

    // --- fetch_any_url refuses a search ------------------------------------
    let middle = Setup::at(WebTier::FetchAnyUrl).build("tier-middle");
    let no_search = middle.search("tokio task pinning");
    assert!(no_search.is_error);
    assert!(
        no_search.content.contains("search"),
        "{}",
        no_search.content
    );
    assert!(
        no_search.content.contains("fetch_any_url"),
        "{}",
        no_search.content
    );
    assert_eq!(middle.transport.calls(), 0);

    // Falsification B: the same query at the search tier goes out.
    let top = Setup::at(WebTier::Search)
        .searching(
            "https://search.example.test/api",
            Arc::new(ForwardingGate) as Arc<dyn RedactionGate>,
        )
        .build("tier-top");
    let searched = top.search("tokio task pinning");
    assert!(!searched.is_error, "{}", searched.content);
    assert_eq!(top.transport.calls(), 1);

    floor.cleanup();
    pasted.cleanup();
    middle.cleanup();
    top.cleanup();
}

// ---------------------------------------------------------------------------
// AC-9 — the allowlist
// ---------------------------------------------------------------------------

/// AC-9: `allowed_domains` constrains **model-chosen** destinations only.
///
/// Three legs, one host: refused when the model chose it, allowed when the user
/// pasted it (BR-11's exemption — their explicit act is its own
/// authorization), and allowed on the tier grant alone when no allowlist is
/// configured. The second and third are each other's falsification: an
/// implementation that refused everything, or one that refused nothing, fails a
/// different leg.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_allowlist_constrains_model_chosen_destinations_only() {
    let outside = "https://evil.example.test/page";

    // --- model-chosen, outside the list: refused ---------------------------
    let listed = Setup::at(WebTier::FetchAnyUrl)
        .allowing(&["docs.rs"])
        .build("allow-refuse");
    let refused = listed.fetch(outside);
    assert!(refused.is_error);
    assert!(
        refused.content.contains("allowed_domains"),
        "the refusal must name the allowlist: {}",
        refused.content
    );
    assert!(
        refused.content.contains("docs.rs"),
        "and say what it does permit: {}",
        refused.content
    );
    assert_eq!(listed.transport.calls(), 0, "and send nothing");
    assert_eq!(
        listed.recorder.outcomes(),
        vec![WebLookupOutcome::RefusedDomain]
    );

    // Non-vacuity within the same fixture: a host *on* the list goes out.
    let permitted = listed.fetch(DOCS_URL);
    assert!(!permitted.is_error, "{}", permitted.content);
    assert_eq!(listed.transport.calls(), 1);

    // --- the same host, pasted by the user: exempt -------------------------
    let exempt = Setup::at(WebTier::FetchAnyUrl)
        .allowing(&["docs.rs"])
        .pasted(outside)
        .build("allow-exempt");
    let out = exempt.fetch(outside);
    assert!(
        !out.is_error,
        "BR-11 exempts a URL the user pasted: {}",
        out.content
    );
    assert_eq!(exempt.transport.calls(), 1);

    // --- no allowlist at all: the tier grant governs alone -----------------
    let unrestricted = Setup::at(WebTier::FetchAnyUrl).build("allow-none");
    let out = unrestricted.fetch(outside);
    assert!(
        !out.is_error,
        "BR-11: an absent allowlist is a valid, unrestricted configuration: {}",
        out.content
    );
    assert_eq!(unrestricted.transport.calls(), 1);

    // --- and an allowlist that lists nothing permits nothing ---------------
    let empty = Setup::at(WebTier::FetchAnyUrl)
        .allowing(&[])
        .build("allow-empty");
    let out = empty.fetch(outside);
    assert!(
        out.is_error,
        "an empty allowlist is the most restrictive posture"
    );
    assert_eq!(empty.transport.calls(), 0);

    listed.cleanup();
    exempt.cleanup();
    unrestricted.cleanup();
    empty.cleanup();
}

/// AC-9, the **redirect** half: the allowlist governs a destination the *server*
/// chose, through the production hop closure.
///
/// The test above covers the initial URL, which the tool checks itself. A
/// redirect target is checked somewhere else entirely — by the closure
/// `WebTool::lookup` binds to the allowlist and hands to the seam — and until
/// this test nothing in the suite scripted a 3xx, so replacing that closure with
/// `|_| true` left every assertion green. This is the observation that was
/// missing.
///
/// **Why this host, and why model-composed.** Verify wave 1 put two seam-level
/// checks *ahead* of the closure: the address-class floor (unconditional on a
/// hop) and BR-11's user-pasted-host-family exemption (which short-circuits
/// before it). A loopback or same-family target would therefore be refused with
/// the closure never consulted, and the test would pass against `|_| true`. The
/// target here is a **public, unrelated** host and the fetch is
/// **model-composed**, so the closure is the only thing that can refuse it.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_allowlist_constrains_a_redirect_target_through_the_production_hop_closure() {
    const HOP: &str = "https://elsewhere.example.test/landing";

    // --- refused at the hop ------------------------------------------------
    let refused = Setup::at(WebTier::FetchAnyUrl)
        .allowing(&["docs.rs"])
        .answering(vec![
            Ok((302, Some(HOP.to_owned()), Vec::new())),
            // Scripted, and deliberately: if the hop were taken, THIS is what it
            // would deliver — so a green assertion below is "the second request
            // never happened", not "the fixture had nothing left to say".
            Ok((200, None, b"<html><body>hop landed</body></html>".to_vec())),
        ])
        .build("hop-refused");

    let out = refused.fetch(DOCS_URL);
    assert!(
        out.is_error,
        "a redirect off the allowlist must not deliver: {}",
        out.content
    );
    assert!(
        out.content
            .contains("redirected outside the configured allowlist"),
        "the model must be told what refused it: {}",
        out.content
    );
    assert_eq!(
        refused.transport.calls(),
        1,
        "the hop was taken: a redirect target outside the allowlist must produce \
         NO second request; sent {:?}",
        refused.transport.urls()
    );
    assert!(
        !refused.transport.urls().iter().any(|u| u.contains(HOP)),
        "the refused destination was contacted: {:?}",
        refused.transport.urls()
    );
    assert_eq!(
        refused.recorder.outcomes(),
        vec![WebLookupOutcome::RefusedDomain],
        "one attempt, one row, naming the destination refusal (BR-7)"
    );

    // --- falsification: the SAME chain with the target listed --------------
    //
    // Only the allowlist differs. Without this leg the count above would be
    // satisfied by a fixture that could never follow a redirect at all.
    let followed = Setup::at(WebTier::FetchAnyUrl)
        .allowing(&["docs.rs", "elsewhere.example.test"])
        .answering(vec![
            Ok((302, Some(HOP.to_owned()), Vec::new())),
            Ok((200, None, b"<html><body>hop landed</body></html>".to_vec())),
        ])
        .build("hop-followed");

    let out = followed.fetch(DOCS_URL);
    assert!(!out.is_error, "{}", out.content);
    assert!(
        out.content.contains("hop landed"),
        "the followed hop must deliver the target's bytes: {}",
        out.content
    );
    assert_eq!(
        followed.transport.calls(),
        2,
        "the chain really is two requests; sent {:?}",
        followed.transport.urls()
    );
    assert!(
        followed.transport.urls()[1].contains(HOP),
        "the second request went somewhere else: {:?}",
        followed.transport.urls()
    );
    assert_eq!(
        followed.recorder.outcomes(),
        vec![WebLookupOutcome::Completed]
    );

    refused.cleanup();
    followed.cleanup();
}

// ---------------------------------------------------------------------------
// AC-12 — taint, the notice, and the override
// ---------------------------------------------------------------------------

/// AC-12: after a boundary read the next model-composed lookup is blocked with
/// a notice naming **cause and effect**, a user-pasted URL still works in the
/// same session, and lifting the restriction restores the composed one.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_taint_notice_names_cause_and_effect_and_a_paste_still_works() {
    let composed = "https://docs.rs/tokio/latest/tokio/";
    let pasted = "https://docs.rs/serde/latest/serde/";

    let fx = Setup::at(WebTier::FetchAnyUrl)
        .tainted()
        .pasted(pasted)
        .build("taint");

    // --- the block, and what it says ---------------------------------------
    let blocked = fx.fetch(composed);
    assert!(blocked.is_error);
    let notice = &blocked.content;
    assert!(
        notice.contains("privacy-boundary content"),
        "the notice must name the CAUSE: {notice}"
    );
    assert!(
        notice.contains("model-composed web lookups are restricted"),
        "the notice must name the EFFECT: {notice}"
    );
    assert!(
        notice.contains("a URL the user pasted still works"),
        "and what still works: {notice}"
    );
    assert!(
        notice.contains("only the user can lift the restriction"),
        "and who can lift it: {notice}"
    );
    // BUG-152's taxonomy: a restriction is this capability working, never an
    // error the user is told to debug.
    assert!(!notice.contains("error:"), "{notice}");
    assert_eq!(fx.transport.calls(), 0, "and nothing left the machine");
    assert_eq!(
        fx.recorder.outcomes(),
        vec![WebLookupOutcome::TaintRestricted],
        "the status row reads this event; a silent restriction is invisible"
    );

    // --- the same tainted session: the user's own paste proceeds -----------
    let survives = fx.fetch(pasted);
    assert!(
        !survives.is_error,
        "BR-13: the user authored those bytes: {}",
        survives.content
    );
    assert_eq!(fx.transport.calls(), 1, "exactly one of the two went out");

    // --- falsification: the same composed URL with the restriction lifted --
    let lifted = Setup::at(WebTier::FetchAnyUrl)
        .tainted()
        .overridden()
        .build("taint-lifted");
    let restored = lifted.fetch(composed);
    assert!(
        !restored.is_error,
        "the override restores model-composed lookups: {}",
        restored.content
    );
    assert_eq!(lifted.transport.calls(), 1);

    fx.cleanup();
    lifted.cleanup();
}

/// **BR-13's cache exemption: a stored copy is served in a tainted session, and
/// the same URL uncached is not.**
///
/// The taint gate lives at the choke point, and a cache hit never reaches the
/// choke point — `WebTool::lookup` answers it two gates earlier. That ordering
/// is deliberate (a hit performs no egress, so there is nothing for the taint
/// rule to protect) but it is also invisible: until this test the two facts were
/// only ever observed apart, and an implementation that had moved the cache
/// *behind* the taint gate — or the taint check ahead of it — would have passed
/// the whole suite.
///
/// Three legs on **one URL**, so the only difference between the first two is
/// whether the document is on disk:
///
/// 1. tainted, cached → served, zero packets, and **no consent prompt** (a hit
///    asks for nothing, because nothing is being authorized);
/// 2. tainted, the same URL evicted → `taint_restricted`, zero packets, and the
///    one prompt this test ever sees;
/// 3. untainted, uncached → completes, which is what keeps leg 2 from being
///    "this fixture never fetches".
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_cached_page_is_served_in_a_tainted_session_and_the_same_url_uncached_is_not() {
    let fx = Setup::at(WebTier::FetchAnyUrl)
        .policy(PermissionPolicy::Ask)
        .tainted()
        .build("taint-cache");
    let answerer = Answerer::spawn(&fx.bus, &fx.pending, vec!["allow_once"]);

    // The stored copy, written through the same cache the tool reads.
    fx.cache()
        .put(DOCS_URL, "the stored reduction", false)
        .expect("caching a document must succeed");

    // --- leg 1: the hit -----------------------------------------------------
    let hit = fx.fetch(DOCS_URL);
    assert!(
        !hit.is_error,
        "BR-12/BR-13: a cached document performs no egress, so the restriction \
         on egress has nothing to refuse: {}",
        hit.content
    );
    assert!(
        hit.content.contains("served from the local cache"),
        "the model must be told nothing left the machine: {}",
        hit.content
    );
    assert!(
        hit.content.contains("the stored reduction"),
        "the hit serves the stored bytes: {}",
        hit.content
    );
    assert_eq!(fx.transport.calls(), 0, "a cache hit is not a packet");
    assert_eq!(
        fx.recorder.outcomes(),
        vec![WebLookupOutcome::CacheHit],
        "BR-7: a free lookup is still a row"
    );
    assert_eq!(
        answerer.count(),
        0,
        "a cache hit authorizes nothing, so it must ask nothing: {:?}",
        answerer
            .prompts()
            .iter()
            .map(|p| p.key.clone())
            .collect::<Vec<_>>()
    );

    // --- leg 2: the same URL, no stored copy -------------------------------
    assert!(
        fx.cache().evict(DOCS_URL).expect("eviction must succeed"),
        "non-vacuity: there was a stored copy to drop"
    );
    let refused = fx.fetch(DOCS_URL);
    assert!(refused.is_error, "{}", refused.content);
    assert!(
        refused.content.contains("privacy") || refused.content.contains("restricted"),
        "the refusal must name the restriction: {}",
        refused.content
    );
    assert_eq!(
        fx.transport.calls(),
        0,
        "a taint-restricted lookup must put no packet on the wire"
    );
    assert_eq!(
        fx.recorder.outcomes(),
        vec![
            WebLookupOutcome::CacheHit,
            WebLookupOutcome::TaintRestricted
        ],
        "the same URL, the same session, one gate apart"
    );
    assert_eq!(
        answerer.count(),
        1,
        "exactly one prompt across both legs, and it belongs to the uncached one"
    );

    // --- leg 3: falsification — the same uncached URL, untainted -----------
    let clean = Setup::at(WebTier::FetchAnyUrl).build("taint-cache-clean");
    let out = clean.fetch(DOCS_URL);
    assert!(
        !out.is_error,
        "the uncached fetch above was refused by the taint rule, not by the \
         fixture: {}",
        out.content
    );
    assert_eq!(clean.transport.calls(), 1);

    fx.cleanup();
    clean.cleanup();
}

/// AC-12, the override's channel: the flag the lookup gate reads is flipped by
/// the **client RPC** and by nothing a model can reach.
///
/// `WebTaintOverride::lift` carries no `pub`, so the only way to reach it from
/// outside the daemon module is `DaemonRuntime::web_override` — the `web/override`
/// RPC's handler. That is asserted here by *doing* it: the view the choke point
/// reads reports the session lifted afterwards, and reports a different session
/// unlifted (the restriction is session-scoped and never persists — a fresh
/// session starts restricted-on-taint again).
#[test]
fn only_the_client_rpc_can_lift_the_restriction() {
    let runtime = DaemonRuntime::minimal();
    let events = Arc::new(EventBus::new());
    let session = SessionId::from("lift-me");
    let other = SessionId::from("some-other-session");

    let view = runtime.web_taint_view();
    assert!(
        !view.is_overridden(&session),
        "non-vacuity: nothing is lifted before the RPC"
    );

    // An override of a session that was never restricted is answered honestly
    // and **changes nothing** — the CLI renders it as "web lookups were never
    // disabled" rather than a false confirmation, and, just as importantly, the
    // flag stays down so a boundary read later in the same session still
    // engages BR-13 (REQ-563 verify: the lift used to be pre-armed here).
    let clean = runtime.web_override(
        &WebOverrideParams {
            session_id: session.clone(),
        },
        &events,
    );
    assert!(!clean.was_restricted);
    assert!(clean.tiers_restored.is_empty());
    assert!(
        !view.is_overridden(&session),
        "an override of nothing pre-armed the flag, disarming the restriction \
         that has not arrived yet"
    );

    // Now the restriction exists, and the RPC lifts it.
    runtime.session_taint().mark(&session);
    let result = runtime.web_override(
        &WebOverrideParams {
            session_id: session.clone(),
        },
        &events,
    );
    assert!(result.was_restricted);
    assert!(
        view.is_overridden(&session),
        "the RPC flipped the flag the lookup gate reads"
    );
    assert!(
        !view.is_overridden(&other),
        "and only for that session — a fresh session is restricted on taint again"
    );

    // Session-scoped, never persisted: a new process is a new set.
    let restarted = DaemonRuntime::minimal();
    assert!(
        !restarted.web_taint_view().is_overridden(&session),
        "the override must never outlive the process (BR-13)"
    );
}

/// AC-12, the other half of the RPC-only property: **no tool has these names.**
///
/// The registry is the whole surface a model's tool call can reach. A tool
/// named `web_override` would make "the override is rejected when issued by the
/// model" a runtime check someone has to remember; its absence is what makes
/// the rejection structural. Asserted with the web tool *registered*, because
/// the interesting claim is about the state in which the capability exists.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn no_tool_is_named_for_the_override_or_the_refresh() {
    let fx = Setup::at(WebTier::Search)
        .searching(
            "https://search.example.test/api",
            Arc::new(ForwardingGate) as Arc<dyn RedactionGate>,
        )
        .build("no-such-tool");

    assert!(fx.registered, "non-vacuity: the web capability IS present");
    assert!(
        fx.tools.get(WEB_TOOL_NAME).is_some(),
        "and the model can reach the lookup tool"
    );

    for forbidden in [
        "web_override",
        "web override",
        "override",
        "web/override",
        "web_refresh",
        "web refresh",
        "refresh",
        "web/refresh",
    ] {
        assert!(
            fx.tools.get(forbidden).is_none(),
            "`{forbidden}` is reachable from tool dispatch, so a model could issue it"
        );
    }
    // Belt and braces over the whole namespace: no registered tool mentions
    // either verb, whatever it is spelled.
    for name in fx.tools.names() {
        assert!(
            !name.contains("override") && !name.contains("refresh"),
            "`{name}` is a tool the model can call, and it names a user-only action"
        );
    }

    fx.cleanup();
}

/// **REQ-572 AC-4's model-tool-call leg, by the same argument** (the verify
/// pass's missing-seam fix).
///
/// AC-4 asks that a model tool call attempting the setup RPC be rejected, and
/// the daemon's answer is that there is nothing to attempt: `web/setup_*` are
/// client RPCs, tool dispatch holds a `ToolContext` and never a `DaemonRuntime`,
/// so the call reaches the registry and finds no such tool. `server.rs` pins the
/// `may_drive` gate that backs that up in depth; what was missing is the half
/// this file's precedent above already knows how to state — **the registry does
/// not name it** — and until it is asserted, "structurally impossible" is a
/// paragraph rather than a fact.
///
/// The method names come from [`RpcMethod::METHOD`] rather than being spelled
/// again, so a renamed method renames the thing being swept for instead of
/// leaving this test green over a stale string. Registered at the `search` tier
/// for the reason its neighbour is: the interesting claim is about the state in
/// which the web capability *exists*.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn no_tool_is_named_for_the_setup_flow() {
    let fx = Setup::at(WebTier::Search)
        .searching(
            "https://search.example.test/api",
            Arc::new(ForwardingGate) as Arc<dyn RedactionGate>,
        )
        .build("no-setup-tool");

    assert!(fx.registered, "non-vacuity: the web capability IS present");
    assert!(
        fx.tools.get(WEB_TOOL_NAME).is_some(),
        "and the model can reach the lookup tool"
    );

    // Every spelling a model might emit for the three methods: the wire name,
    // the underscored form a tool name would take, and the bare verb.
    let methods = [
        WebSetupPlanParams::METHOD,
        WebSetupPreviewParams::METHOD,
        WebSetupCommitParams::METHOD,
    ];
    for method in methods {
        for spelling in [
            method.to_owned(),
            method.replace('/', "_"),
            method.replace('/', " "),
        ] {
            assert!(
                fx.tools.get(&spelling).is_none(),
                "`{spelling}` is reachable from tool dispatch, so a model could \
                 configure the capability it is being gated by"
            );
        }
    }
    for bare in ["setup", "web_setup", "setup_commit"] {
        assert!(fx.tools.get(bare).is_none(), "`{bare}` is model-reachable");
    }

    // And over the whole namespace, so a differently-spelled tool cannot slip
    // past the list above.
    for name in fx.tools.names() {
        assert!(
            !name.contains("setup"),
            "`{name}` is a tool the model can call, and it names the user-only \
             enablement flow (AC-4)"
        );
        for method in methods {
            assert_ne!(
                name, method,
                "`{name}` exposes a setup RPC through tool dispatch"
            );
        }
    }

    fx.cleanup();
}

// ---------------------------------------------------------------------------
// AC-13 — the search/redact coupling
// ---------------------------------------------------------------------------

const SEARCH_ENDPOINT: &str = "https://search.example.test/api";

/// AC-13: the search gate's presence is the difference between a query that
/// goes out and one that does not — a guard that cannot run is a **block**, not
/// a skip (LESSON-492).
///
/// This is the observable form of "the gate is installed exactly when the tier
/// is `search`": the daemon installs it on that condition (pinned in
/// `runtime.rs` against `Egress::installed`, which is crate-private and so
/// cannot be read from here), and what an absent gate then *does* is this.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_search_with_no_gate_installed_is_a_block_not_a_skip() {
    let transport = LookupCapture::answering("{\"results\":[]}");
    let recorder = Arc::new(Recorder::default());
    // No `with_search_redaction_gate` — the choke point a non-`search` tier
    // builds.
    let ungated = Egress::new(transport.clone(), Vec::new(), Arc::new(NoopSink))
        .with_lookup_recorder(Arc::clone(&recorder) as Arc<dyn LookupRecorder>);
    let clean = Flags::default();
    let ctx = LookupContext::new("sess-consent", &clean, &allow_any_host)
        .with_search_endpoint(SEARCH_ENDPOINT);

    let refused = ungated
        .lookup(
            &LookupRequest::search("tokio task pinning", Authorship::ModelComposed),
            &ctx,
        )
        .await;

    assert_eq!(refused.outcome(), WebLookupOutcome::BlockedRedact);
    assert_eq!(
        refused.detail(),
        &LookupDetail::Blocked {
            cause: BlockCause::ScanUnavailable
        },
        "a missing guard is the strongest form of a guard that cannot run"
    );
    assert_eq!(transport.calls(), 0, "an unscanned query must not leave");
    assert_eq!(recorder.outcomes(), vec![WebLookupOutcome::BlockedRedact]);

    // --- falsification: the same choke point WITH the gate installed -------
    let gated_transport = LookupCapture::answering("{\"results\":[]}");
    let gated = Egress::new(gated_transport.clone(), Vec::new(), Arc::new(NoopSink))
        .with_search_redaction_gate(Arc::new(ForwardingGate));
    let ctx = LookupContext::new("sess-consent", &clean, &allow_any_host)
        .with_search_endpoint(SEARCH_ENDPOINT);
    let sent = gated
        .lookup(
            &LookupRequest::search("tokio task pinning", Authorship::ModelComposed),
            &ctx,
        )
        .await;
    assert_eq!(sent.outcome(), WebLookupOutcome::Completed);
    assert_eq!(gated_transport.calls(), 1);
    assert!(
        gated_transport.urls()[0].contains("q=tokio"),
        "and it is the query that went: {:?}",
        gated_transport.urls()
    );
}

/// AC-13: a transient scan failure blocks **that query** while the turn
/// completes — the model is told, in one sentence, and continues.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_unavailable_scan_blocks_the_query_and_sends_nothing() {
    let fx = Setup::at(WebTier::Search)
        .searching(
            SEARCH_ENDPOINT,
            Arc::new(UnavailableGate) as Arc<dyn RedactionGate>,
        )
        .build("scan-unavailable");

    let out = fx.search("tokio task pinning");
    assert!(
        out.is_error,
        "a query that could not be scanned must not run"
    );
    assert_eq!(fx.transport.calls(), 0);
    assert_eq!(
        fx.recorder.outcomes(),
        vec![WebLookupOutcome::BlockedRedact]
    );
    // BR-9: still a sentence the model can continue from, never a turn error.
    assert!(
        out.content.contains("State that and continue"),
        "the model must be able to carry on: {}",
        out.content
    );

    // --- falsification: the same query, the same tier, a scan that ran -----
    let ok = Setup::at(WebTier::Search)
        .searching(
            SEARCH_ENDPOINT,
            Arc::new(ForwardingGate) as Arc<dyn RedactionGate>,
        )
        .build("scan-available");
    let out = ok.search("tokio task pinning");
    assert!(!out.is_error, "{}", out.content);
    assert_eq!(ok.transport.calls(), 1);

    fx.cleanup();
    ok.cleanup();
}

/// AC-13's local-tier-absent leg, on the **default (loaderless) build**.
///
/// This build compiles no engine (`llama` is non-default), so `EngineSlot` on a
/// fresh runtime is honestly empty — the machine BR-14 is about. The choke
/// point is built by the daemon's own `web_lookup_egress`, with the daemon's own
/// composite search gate on it, and every search query blocks. That is what
/// "the search tier is not offered there" amounts to in behaviour: the tier
/// cannot perform a single query, and the block is a stated outcome rather than
/// an error.
///
/// Hermetic despite holding a real `HttpTransport`: the query is refused before
/// the wire, and the endpoint is a loopback port nobody listens on, so even a
/// regression that forwarded could reach no network.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn on_a_loaderless_build_the_real_search_gate_refuses_every_query() {
    let runtime = DaemonRuntime::minimal();
    let events = Arc::new(EventBus::new());
    let session = SessionId::from("loaderless");
    let endpoint = format!("http://127.0.0.1:{}/search", closed_port());

    // The config a machine that opted into `search` would be running.
    let mut config = Config::default();
    config.web.tier = WebTier::Search;
    config.web.search_endpoint = Some(endpoint.clone());

    let router = Router::new(CategoryTable::new(), None);
    let egress = runtime
        .web_lookup_egress(&router, &config, &events, &session)
        .expect("the lookup choke point builds");

    let clean = Flags::default();
    let ctx = LookupContext::new(session.clone(), &clean, &allow_any_host)
        .with_search_endpoint(&endpoint);
    let refused = egress
        .lookup(
            &LookupRequest::search("tokio task pinning", Authorship::ModelComposed),
            &ctx,
        )
        .await;

    assert_eq!(
        refused.outcome(),
        WebLookupOutcome::BlockedRedact,
        "with no local tier the scan cannot run, so the query is blocked (LESSON-492)"
    );
    assert_eq!(
        refused.detail(),
        &LookupDetail::Blocked {
            cause: BlockCause::ScanUnavailable
        },
        "and the block says the scan was unavailable — not that anything was found"
    );
    assert_eq!(refused.bytes_in(), 0);

    // Falsification: the *fetch* tiers are unaffected by the coupling (BR-14's
    // last sentence). The same runtime, the same absent engine — a fetch is not
    // refused by the scan, it is refused by the unreachable host, which is a
    // different ending entirely.
    let fetched = egress
        .lookup(
            &LookupRequest::fetch(
                format!("http://127.0.0.1:{}/page", closed_port()),
                Authorship::UserPasted,
            ),
            &ctx,
        )
        .await;
    assert_eq!(
        fetched.outcome(),
        WebLookupOutcome::Offline,
        "a fetch on a loaderless machine reaches the wire and finds nothing there, \
         which is not a scan refusal: {:?}",
        fetched.detail()
    );
}

// ---------------------------------------------------------------------------
// REQ-572 AC-3 / BR-7 — what a setup commit does NOT do (TASK-133)
// ---------------------------------------------------------------------------

/// **A guided setup commit enables a tier and grants no consent** (REQ-572 BR-7,
/// LESSON-495).
///
/// The two writes this daemon can make to `[web]` are easy to confuse and mean
/// opposite things. `enable_permanent` answers a *question the user was asked
/// about one lookup*, and the test above it pins that it therefore appends to
/// `permission_allow` — so the next session stops asking. `/web setup` answers
/// "may this machine reach the web at all", which is a **ceiling**, not an
/// answer: every lookup under it is still the user's to allow or refuse.
///
/// A setup flow that quietly did both would be the LESSON-495 defect in its
/// worst form — a user who walked a setup wizard would have consented, forever
/// and on every future session, to a question nobody put to them.
///
/// The claim is made in the two halves that have to agree, the shape the
/// `enable_permanent` test uses:
///
/// 1. **what the flow writes** — read off the *production* preview, whose bytes
///    are the bytes the commit goes on to write (TASK-129 pins that equality),
///    parsed back through the production loader as a restarted daemon reads it;
/// 2. **what a session built from exactly those bytes does** — it asks.
///
/// Falsified in place: the same fixture, at the same tier, with the tier listed
/// in `permission_allow` — which is precisely what `enable_permanent` writes —
/// does *not* ask. So "it asked" is a fact about the bytes the setup flow wrote
/// and not about a fixture that always prompts.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_setup_commit_enables_the_tier_and_answers_no_consent_question() {
    // (1) What `/web setup` writes, from the daemon's own renderer. `minimal()`
    // has no config path, which is exactly right here: this is a question about
    // the *candidate bytes*, and a preview is the production path that produces
    // them without a write.
    let runtime = DaemonRuntime::minimal();
    let preview = runtime
        .web_setup_preview(&WebSetupPreviewParams {
            session_id: SessionId::from("sess-setup"),
            tier: WireWebTier::FetchAnyUrl,
            search_endpoint: None,
            search_key_ref: None,
            search_auth: None,
        })
        .expect("a fetch tier previews on any machine");

    let written = Config::load(&preview.toml).expect("the previewed table loads and validates");
    assert_eq!(
        written.web.tier,
        WebTier::FetchAnyUrl,
        "non-vacuity: the flow really did raise the ceiling; table:\n{}",
        preview.toml
    );
    assert!(
        written.web.permission_allow.is_empty(),
        "BR-7/LESSON-495: enabling a tier must not answer a consent question. \
         Table as written:\n{}",
        preview.toml
    );

    // (2) A session on a machine configured by that commit, and nothing else.
    // The consent list comes from the written bytes rather than from a literal,
    // so a commit that started fanning out grants would change this fixture
    // rather than slip past it.
    let fx = Setup::at(written.web.tier)
        .policy(PermissionPolicy::Ask)
        .consented(&written.web.permission_allow)
        .build("post-commit");
    let answerer = Answerer::spawn(&fx.bus, &fx.pending, vec!["allow_once"]);

    let out = fx.fetch(DOCS_URL);
    assert!(!out.is_error, "{}", out.content);
    assert_eq!(
        answerer.count(),
        1,
        "the first lookup after a setup commit must still ask"
    );
    assert_eq!(fx.transport.calls(), 1, "and the answer let it through");
    // The key it asked under is the tier's, exactly as it was before the commit
    // — a setup write touches no consent key at all (LESSON-495).
    assert_eq!(
        answerer.prompts()[0].key,
        PERMISSION_KEY_FETCH_ANY_URL,
        "the question is the tier's own, unchanged by the commit"
    );
    // And the grant it bought is still one lookup wide.
    let second = fx.fetch("https://docs.rs/serde/latest/serde/");
    assert!(
        second.is_error,
        "allow-once still means once: {}",
        second.content
    );
    assert_eq!(answerer.count(), 2, "so the next lookup asked again");

    // --- falsification: the bytes `enable_permanent` writes ------------------
    let granted = Setup::at(WebTier::FetchAnyUrl)
        .policy(PermissionPolicy::Ask)
        .consented(&[WebTier::FetchAnyUrl])
        .build("post-commit-granted");
    // An empty script: any prompt at all would be cancelled, so a lookup that
    // succeeds here is one that was never asked about.
    let unasked = Answerer::spawn(&granted.bus, &granted.pending, Vec::new());
    let allowed = granted.fetch(DOCS_URL);
    assert!(
        !allowed.is_error,
        "a listed tier must not ask: {}",
        allowed.content
    );
    assert_eq!(
        unasked.count(),
        0,
        "this is the state a setup commit must NOT produce — if it asked nothing \
         here too, the assertion above measures nothing"
    );

    fx.cleanup();
    granted.cleanup();
}

fn allow_any_host(_host: &str) -> bool {
    true
}

/// A loopback port with nothing listening on it: bound to learn a free number,
/// then released.
fn closed_port() -> u16 {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind to find a free port");
    let port = listener.local_addr().unwrap().port();
    drop(listener);
    port
}
