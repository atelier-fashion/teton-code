//! REQ-563 acceptance: what a **capture transport in front of the real choke
//! point** proves about opt-in web lookup (TASK-078).
//!
//! Every claim in this file is a claim about bytes. The web tool owns no HTTP
//! client, so the one place a lookup can become traffic is
//! [`Egress::lookup`](tetond::egress::Egress) — and putting a recording
//! `Transport` behind it makes "no packet left" a *count* rather than a reading
//! of the code. That is the same instrument `egress_capture.rs` uses for
//! provider payloads (its `CaptureTransport`, lines 44-67); the version here
//! adds a script, because a lookup test has to answer as well as record.
//!
//! Two transports are in play at once and they are deliberately separate:
//!
//! * the **lookup** transport — everything a web lookup would send;
//! * the **provider** transport — everything the *model* is sent (the same SSE
//!   capture `provenance_egress.rs` drives the loop with).
//!
//! AC-11 is exactly the relationship between them: a page arrives on the first
//! and its raw bytes must never appear on the second.
//!
//! ## AC → test map
//!
//! | AC | Test |
//! |----|------|
//! | AC-1  | [`tier_off_makes_zero_lookup_traffic_across_a_scripted_session`] |
//! | AC-3 (taint) | [`a_model_composed_lookup_in_a_tainted_session_leaves_no_packet`] |
//! | AC-3 (redact) | [`a_planted_credential_is_blocked_in_the_form_the_encoder_would_send`] |
//! | AC-8  | [`an_unreachable_destination_is_offline_and_the_turn_still_completes`] |
//! | AC-10 | [`a_second_lookup_is_served_from_cache_with_zero_transport_calls`] |
//! | AC-11 | [`no_raw_page_bytes_reach_any_remote_provider_payload`] |
//!
//! The rest of the matrix — consent scopes, tier gradation, the allowlist, the
//! taint override, the search/redact coupling — is `web_consent_matrix.rs`.
//!
//! ## Falsification (LESSON-479)
//!
//! Every blocking assertion here is paired, **in the same test**, with the same
//! scenario run under a permissive configuration, and that leg asserts the
//! traffic genuinely flows. A "zero packets" count that has never been observed
//! to be non-zero measures nothing but a broken fixture. Nothing in production
//! is edited to produce the control: only the config, the gate, or the session
//! flag under test is flipped.

use std::collections::VecDeque;
use std::path::PathBuf;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use teton_core::effort::{EffortLevel, ResolvedEffort};

use async_trait::async_trait;
use serde_json::json;
use tokio::runtime::Handle;

use teton_core::config::{WebConfig, WebTier};
use teton_protocol::events::{BlockCause, WebLookupKind, WebLookupOutcome};
use teton_protocol::SessionId;
use teton_providers::transport::{Transport, TransportError, TransportRequest, TransportResponse};
use teton_providers::{OpenAiCompatAdapter, OpenAiCompatConfig};

use tetond::broadcast::EventBus;
use tetond::egress::redact::pattern_verdict;
use tetond::egress::{
    Authorship, Egress, LookupContext, LookupDetail, LookupRecord, LookupRecorder, LookupRequest,
    NoopSink, RedactionGate, RedactionVerdict, TaintView,
};
use tetond::harness::permissions::{PendingPermissions, PermissionConfig, PermissionGate};
use tetond::harness::tools::web::{register_web_tool, SeamError, WebLookupSeam, WEB_TOOL_NAME};
use tetond::harness::{
    build_system_prompt, run_session_turn_with_source, ContextManager, DutyRoute, HarnessConfig,
    HarnessError, NoopProvenanceHook, RemoteProviderSource, SessionEvents, ToolContext, ToolDuties,
    ToolRegistry, TurnOutcome,
};
use tetond::web::{UserUrls, WebCache};

// ---------------------------------------------------------------------------
// Fixtures
// ---------------------------------------------------------------------------

/// One scripted answer from the lookup transport: a status, an optional
/// `Location`, and a body — or a transport failure.
type ScriptedAnswer = Result<(u16, Option<String>, Vec<u8>), TransportError>;

/// A `Transport` that records every request instead of sending it and answers
/// from a script.
///
/// The **count** is the instrument, not the answer: "zero lookup traffic" is the
/// entire content of AC-1, and a mock that only returned a canned body could not
/// tell a refusal from a permissive round trip.
#[derive(Clone, Default)]
struct LookupCapture {
    sent: Arc<Mutex<Vec<TransportRequest>>>,
    script: Arc<Mutex<VecDeque<ScriptedAnswer>>>,
}

impl LookupCapture {
    fn new(script: Vec<ScriptedAnswer>) -> Self {
        Self {
            sent: Arc::new(Mutex::new(Vec::new())),
            script: Arc::new(Mutex::new(script.into_iter().collect())),
        }
    }

    /// A transport that answers every request with `200 body`.
    fn answering(body: &str) -> Self {
        Self::new(vec![Ok((200, None, body.as_bytes().to_vec()))])
    }

    fn calls(&self) -> usize {
        self.sent.lock().unwrap().len()
    }

    fn captured(&self) -> Vec<TransportRequest> {
        self.sent.lock().unwrap().clone()
    }

    fn urls(&self) -> Vec<String> {
        self.captured().into_iter().map(|r| r.url).collect()
    }
}

#[async_trait]
impl Transport for LookupCapture {
    async fn execute(
        &self,
        request: TransportRequest,
    ) -> Result<TransportResponse, TransportError> {
        self.sent.lock().unwrap().push(request);
        // An exhausted script keeps answering `200` with an empty body: a test
        // that scripted one answer is asserting about the first call, and a
        // panic on the second would report "the fixture ran out" where the
        // interesting fact is "there WAS a second call".
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

/// A capturing OpenAI-compatible SSE transport — the *provider* side. Same
/// shape `provenance_egress.rs` drives the loop with.
#[derive(Clone, Default)]
struct CaptureSse {
    bodies: Arc<Mutex<VecDeque<String>>>,
    sent: Arc<Mutex<Vec<Vec<u8>>>>,
}

impl CaptureSse {
    fn with_bodies(bodies: Vec<String>) -> Self {
        Self {
            bodies: Arc::new(Mutex::new(bodies.into_iter().collect())),
            sent: Arc::new(Mutex::new(Vec::new())),
        }
    }

    fn captured(&self) -> Vec<Vec<u8>> {
        self.sent.lock().unwrap().clone()
    }
}

#[async_trait]
impl Transport for CaptureSse {
    async fn execute(
        &self,
        request: TransportRequest,
    ) -> Result<TransportResponse, TransportError> {
        self.sent.lock().unwrap().push(request.body.clone());
        let body = self
            .bodies
            .lock()
            .unwrap()
            .pop_front()
            .unwrap_or_else(|| "data: [DONE]\n\n".to_owned());
        Ok(TransportResponse {
            status: 200,
            location: None,
            body: Box::pin(futures::stream::once(async move { Ok(body.into_bytes()) })),
        })
    }
}

/// One OpenAI-compatible streaming turn: a text delta, an optional tool call,
/// then usage + `[DONE]`.
fn sse_turn(text: &str, tool: Option<(&str, &str, &str)>) -> String {
    let mut s = String::new();
    let chunk = json!({ "choices": [{ "delta": { "content": text } }] });
    s.push_str(&format!("data: {chunk}\n\n"));
    if let Some((id, name, args)) = tool {
        let chunk = json!({
            "choices": [{ "delta": { "tool_calls": [{
                "index": 0, "id": id, "function": { "name": name, "arguments": args }
            }]}}]
        });
        s.push_str(&format!("data: {chunk}\n\n"));
        let finish = json!({ "choices": [{ "delta": {}, "finish_reason": "tool_calls" }] });
        s.push_str(&format!("data: {finish}\n\n"));
    } else {
        let finish = json!({ "choices": [{ "delta": {}, "finish_reason": "stop" }] });
        s.push_str(&format!("data: {finish}\n\n"));
    }
    s.push_str("data: {\"usage\":{\"prompt_tokens\":10,\"completion_tokens\":5}}\n\n");
    s.push_str("data: [DONE]\n\n");
    s
}

/// The two session flags the taint gate reads, as plain booleans.
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

/// Every `LookupRecord` the choke point (or a local refusal) produced.
#[derive(Default)]
struct Recorder {
    records: Mutex<Vec<LookupRecord>>,
}

impl Recorder {
    fn records(&self) -> Vec<LookupRecord> {
        self.records.lock().unwrap().clone()
    }

    fn outcomes(&self) -> Vec<WebLookupOutcome> {
        self.records().into_iter().map(|r| r.outcome).collect()
    }
}

impl LookupRecorder for Recorder {
    fn web_lookup(&self, _session_id: &SessionId, record: &LookupRecord) {
        self.records.lock().unwrap().push(record.clone());
    }
}

/// The harness-facing seam, implemented over the **real** choke point.
///
/// This is the piece `harness/tools/web.rs`'s own unit tests could not build:
/// [`LookupOutcome`](tetond::egress::LookupOutcome) has no public constructor,
/// so a delivered lookup exists only when a transport genuinely delivered one.
/// That is why end-to-end delivery is proven here and not there.
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

/// A redaction gate that answers with the **production** pattern scanner.
///
/// Not a canned verdict: LESSON-490's rule is that a guard which runs on an
/// encoded form is tested against the encoder's output, and the corollary is
/// that the guard under test has to be the real one. `pattern_verdict` is the
/// exact function the daemon's composite scanner's blocking pass calls.
struct ProductionPatternGate {
    seen: Mutex<Vec<String>>,
}

impl ProductionPatternGate {
    fn new() -> Arc<Self> {
        Arc::new(Self {
            seen: Mutex::new(Vec::new()),
        })
    }

    fn calls(&self) -> usize {
        self.seen.lock().unwrap().len()
    }
}

#[async_trait]
impl RedactionGate for ProductionPatternGate {
    async fn scan(&self, payload: &str) -> RedactionVerdict {
        self.seen.lock().unwrap().push(payload.to_owned());
        pattern_verdict(payload)
    }
}

/// A gate that forwards everything — the permissive control for the scan.
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

fn contains_bytes(haystack: &[u8], needle: &str) -> bool {
    needle.len() <= haystack.len()
        && haystack
            .windows(needle.len())
            .any(|w| w == needle.as_bytes())
}

fn scratch(tag: &str) -> PathBuf {
    static SEQ: AtomicUsize = AtomicUsize::new(0);
    let dir = std::env::temp_dir().join(format!(
        "teton-weblookup-{tag}-{}-{}-{}",
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

fn web_config(tier: WebTier) -> WebConfig {
    WebConfig {
        tier,
        ..WebConfig::default()
    }
}

// ---------------------------------------------------------------------------
// The scripted session
// ---------------------------------------------------------------------------

/// Everything one scripted session produced.
struct SessionRun {
    result: Result<TurnOutcome, HarnessError>,
    /// Every request body the **provider** was sent.
    provider_payloads: Vec<Vec<u8>>,
    /// Every request the **lookup** transport was asked to send.
    lookup_requests: Vec<TransportRequest>,
    /// Every lookup the choke point (or a local refusal) recorded.
    records: Vec<LookupRecord>,
    system_prompt: String,
    web_registered: bool,
    dir: PathBuf,
}

impl SessionRun {
    fn lookup_calls(&self) -> usize {
        self.lookup_requests.len()
    }

    /// The turn ended normally — AC-8's "the turn completes", and the negative
    /// of "a lookup failure fails the turn" (BR-9).
    fn completed(&self) -> bool {
        self.result.is_ok()
    }

    fn cleanup(self) {
        std::fs::remove_dir_all(&self.dir).ok();
    }
}

/// Drive one whole scripted session: a remote model that calls `web` and then
/// finishes, over a real turn loop, a real registry, a real permission gate,
/// and a real choke point in front of `LookupCapture`.
///
/// `web` is the `[web]` table the daemon would have read; `lookup_script` is
/// what the network answers. Nothing here is web-specific except those two —
/// which is the point: the same session shape runs at every tier, so the
/// difference an assertion sees is the difference the config made.
async fn scripted_session(
    tag: &str,
    web: WebConfig,
    tool_args: &str,
    pasted: &[&str],
    lookup_script: Vec<ScriptedAnswer>,
) -> SessionRun {
    let dir = scratch(tag);

    // --- the provider side -------------------------------------------------
    let sse = CaptureSse::with_bodies(vec![
        sse_turn("Looking that up.", Some(("c1", WEB_TOOL_NAME, tool_args))),
        sse_turn("Here is what I found.", None),
    ]);
    let provider_egress = Egress::new(sse.clone(), Vec::new(), Arc::new(NoopSink));
    let provider = OpenAiCompatAdapter::new(OpenAiCompatConfig::new(
        "deepseek",
        "https://api.deepseek.com/v1/chat/completions",
    ));
    let session_id = SessionId::from("web-acceptance");
    let mut source = RemoteProviderSource::new(
        &provider,
        &provider_egress,
        "deepseek",
        "deepseek-chat",
        session_id.clone(),
        // REQ-559: an integration fixture states its effort like any other
        // call path — the field is required, so it cannot be forgotten.
        ResolvedEffort::effort(EffortLevel::High),
    );

    // --- the lookup side ---------------------------------------------------
    let lookup = LookupCapture::new(lookup_script);
    let recorder = Arc::new(Recorder::default());
    let lookup_egress = Egress::new(lookup.clone(), Vec::new(), Arc::new(NoopSink))
        .with_lookup_recorder(Arc::clone(&recorder) as Arc<dyn LookupRecorder>);
    let seam = Arc::new(CaptureSeam {
        egress: lookup_egress,
        taint: Flags::default(),
        session: session_id.clone(),
        endpoint: web.search_endpoint.clone(),
        recorder: Arc::clone(&recorder),
    });

    // --- the harness -------------------------------------------------------
    let bus = Arc::new(EventBus::new());
    let pending = Arc::new(PendingPermissions::new());
    // Permissive **and** every tier listed in `[web] permission_allow`: consent is
    // `web_consent_matrix.rs`'s subject, and a prompt here would make every
    // assertion in this file depend on answering it. The two are separate calls
    // because `permissive()` deliberately leaves the web keys asking — it means
    // "the local, jailed tool set is pre-approved", which a web lookup is not —
    // so the unprompted posture this fixture wants has to be stated.
    let mut permissions = PermissionConfig::permissive();
    permissions.apply_web_permission(&[
        WebTier::FetchUserUrl,
        WebTier::FetchAnyUrl,
        WebTier::Search,
    ]);
    let gate = Arc::new(PermissionGate::new(
        session_id.clone(),
        permissions,
        Arc::clone(&bus),
        Arc::clone(&pending),
    ));

    let mut urls = UserUrls::new();
    for url in pasted {
        urls.insert(url);
    }

    let mut tools = ToolRegistry::with_builtins();
    let web_registered = register_web_tool(
        &mut tools,
        &web,
        WebCache::from_config(&dir, &web),
        Arc::new(Mutex::new(urls)),
        Arc::clone(&gate),
        Arc::clone(&seam) as Arc<dyn WebLookupSeam>,
        Handle::current(),
    );

    // `for_strong_model`: no `max_tools` cap, so the web tool — registered
    // last, and therefore first to be cut (D-1) — is actually exposed, and no
    // verification nudge lands between the lookup and the closing turn.
    let config = HarnessConfig::for_strong_model();
    let system_prompt = build_system_prompt(&tools, &config);
    let mut ctx = ContextManager::new(system_prompt.clone(), config.context_budget_tokens);
    ctx.push_user("What does the tokio docs page say about task pinning?");

    let events = SessionEvents::new(Arc::clone(&bus), session_id);
    let tool_ctx = ToolContext::new(&dir);
    let mut hook = NoopProvenanceHook;

    let result = run_session_turn_with_source(
        &mut source,
        &tools,
        &tool_ctx,
        &gate,
        &events,
        &mut ctx,
        &config,
        &mut hook,
        &DutyRoute::unresolved("no digest route in this test"),
        &DutyRoute::unresolved("no compact route in this test"),
        &ToolDuties {
            triage: &DutyRoute::unresolved("no triage route in this test"),
            shell: &DutyRoute::unresolved("no shell route in this test"),
        },
    )
    .await;

    SessionRun {
        result,
        provider_payloads: sse.captured(),
        lookup_requests: lookup.captured(),
        records: recorder.records(),
        system_prompt,
        web_registered,
        dir,
    }
}

// ---------------------------------------------------------------------------
// AC-1 — the fresh-install default
// ---------------------------------------------------------------------------

/// AC-1: with no opt-in, a whole session that *tries* to look something up
/// makes **zero** lookup transport calls, and the system prompt names the
/// opt-in instead of leaving the model to hunt the repository.
///
/// The falsification leg is the second half of the same test: the identical
/// session at `fetch_any_url` reaches the wire. Without it, "zero calls" would
/// be satisfied by a fixture that could never call anything.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn tier_off_makes_zero_lookup_traffic_across_a_scripted_session() {
    let args = r#"{"url":"https://docs.rs/tokio/latest/tokio/"}"#;

    // --- the default state -------------------------------------------------
    let off = scripted_session("ac1-off", web_config(WebTier::Off), args, &[], vec![]).await;

    assert!(
        !off.web_registered,
        "BR-1/D-1: at `tier = off` the capability is absent by construction"
    );
    assert_eq!(
        off.lookup_calls(),
        0,
        "a session with no opt-in put bytes on the lookup transport"
    );
    assert!(
        off.records.is_empty(),
        "and nothing was recorded, because nothing was attempted"
    );
    assert!(off.completed(), "the session still finished normally");

    // BR-6/LESSON-482: the prompt describes the ending this state has, and
    // names the key the user would have to change (LESSON-493).
    assert!(
        off.system_prompt
            .contains("Web lookup is off on this machine"),
        "the prompt must name the state; prompt:\n{}",
        off.system_prompt
    );
    assert!(
        off.system_prompt.contains("[web] tier"),
        "the prompt must name the opt-in key; prompt:\n{}",
        off.system_prompt
    );
    // ...and it must not send the model looking through the repo for it.
    assert!(
        off.system_prompt
            .contains("instead of searching the repository for the answer"),
        "prompt:\n{}",
        off.system_prompt
    );

    // --- the falsification leg: flip ONLY the tier ------------------------
    let on = scripted_session(
        "ac1-on",
        web_config(WebTier::FetchAnyUrl),
        args,
        &[],
        vec![Ok((
            200,
            None,
            b"<html><body>pinning</body></html>".to_vec(),
        ))],
    )
    .await;

    assert!(
        on.web_registered,
        "the control must actually register a tool"
    );
    assert_eq!(
        on.lookup_calls(),
        1,
        "the control leg has to reach the wire, or the zero above measures nothing"
    );
    assert_eq!(on.records.len(), 1, "one attempt, one row (BR-7)");
    assert_eq!(on.records[0].outcome, WebLookupOutcome::Completed);
    assert!(
        !on.system_prompt
            .contains("Web lookup is off on this machine"),
        "an opted-in machine must not be told it is opted out"
    );

    off.cleanup();
    on.cleanup();
}

// ---------------------------------------------------------------------------
// AC-3 — privacy: taint, and the redact scan on a search query
// ---------------------------------------------------------------------------

/// AC-3, first half: a model-composed lookup in a session that has touched
/// boundary content is refused with `taint_restricted` and **no packet**.
///
/// Driven at the choke point rather than through the tool, because the gate
/// under test lives there and the flag it reads is the session's, not the
/// tool's. Three legs, one session's worth of flags between them: refused,
/// then the user's own paste in the *same* tainted session, then the same
/// model-composed lookup with the restriction lifted.
#[tokio::test]
async fn a_model_composed_lookup_in_a_tainted_session_leaves_no_packet() {
    let url = "https://docs.rs/tokio/latest/tokio/";

    // --- tainted, model-composed: the block ---
    let transport = LookupCapture::answering("<html/>");
    let recorder = Arc::new(Recorder::default());
    let egress = Egress::new(transport.clone(), Vec::new(), Arc::new(NoopSink))
        .with_lookup_recorder(Arc::clone(&recorder) as Arc<dyn LookupRecorder>);
    let tainted = Flags {
        tainted: true,
        overridden: false,
    };
    let ctx = LookupContext::new("sess-taint", &tainted, &allow_any_host);

    let blocked = egress
        .lookup(&LookupRequest::fetch(url, Authorship::ModelComposed), &ctx)
        .await;

    assert_eq!(blocked.outcome(), WebLookupOutcome::TaintRestricted);
    assert_eq!(blocked.detail(), &LookupDetail::TaintRestricted);
    assert_eq!(
        transport.calls(),
        0,
        "a refusal that reached the transport is not a refusal"
    );
    assert_eq!(
        blocked.host(),
        "docs.rs",
        "the ledger row still names where it was headed (BR-7)"
    );
    assert!(blocked.body().is_empty(), "and brought nothing back");

    // --- falsification leg A: BR-13's asymmetry, same session, same flag ---
    // The user pasted this URL, so the bytes leaving are theirs. If this did
    // not go out, the block above would be "this fixture never sends".
    let allowed = egress
        .lookup(&LookupRequest::fetch(url, Authorship::UserPasted), &ctx)
        .await;
    assert_eq!(allowed.outcome(), WebLookupOutcome::Completed);
    assert_eq!(transport.calls(), 1, "exactly one of the two went out");

    // --- falsification leg B: the override restores the composed one -------
    let lifted = Flags {
        tainted: true,
        overridden: true,
    };
    let ctx = LookupContext::new("sess-taint", &lifted, &allow_any_host);
    let restored = egress
        .lookup(&LookupRequest::fetch(url, Authorship::ModelComposed), &ctx)
        .await;
    assert_eq!(restored.outcome(), WebLookupOutcome::Completed);
    assert_eq!(transport.calls(), 2);

    // One row per attempt, whatever the ending (BR-7).
    assert_eq!(
        recorder.outcomes(),
        vec![
            WebLookupOutcome::TaintRestricted,
            WebLookupOutcome::Completed,
            WebLookupOutcome::Completed,
        ]
    );
    // And no row carries anything finer than a host — the type has no field
    // that could hold the URL, which is the guarantee (BR-7).
    for record in recorder.records() {
        assert_eq!(record.host, "docs.rs");
    }
}

/// The planted value. An obviously synthetic sentinel, never a real provider's
/// key shape: a fixture spelled like a live credential trips forge-side secret
/// scanning on push, and the shape under test here is the `KEY=value`
/// assignment, not the value. House convention — see `AKIASENTINEL562ABCDE` in
/// `redact_egress.rs` and `sk-ZZQUUXSENTINELCREDENTIAL0123` in `runtime.rs`.
const PLANTED_VALUE: &str = "SENTINELNOTAREALCREDENTIAL01234";

/// A search query with a planted credential, spelled as a `KEY=value`
/// assignment — the layout LESSON-490 found the scanner had gone blind to.
const PLANTED: &str =
    "why does STRIPE_API_KEY=SENTINELNOTAREALCREDENTIAL01234 not work with the v2 endpoint";

/// A query with no credential in it — the non-vacuity control for the scan.
const CLEAN_QUERY: &str = "tokio task pinning best practices";

/// AC-3, second half (with AC-13's planted-secret leg): a search query carrying
/// a credential is blocked before the wire, and the bytes it is blocked from
/// sending are the ones the **production encoder** would have produced.
///
/// LESSON-490 in method as well as subject: the fixture is not a hand-written
/// "what the URL would look like". The permissive leg runs first, the request
/// the production `search_request` built is *read off the capture transport*,
/// and the blocked leg then asserts those exact bytes never appear. The
/// encoding is not incidental — `append_pair` turns the `=` of the assignment
/// into `%3D`, so a fixture written by hand in raw form would have been testing
/// a representation the wire never carries.
#[tokio::test]
async fn a_planted_credential_is_blocked_in_the_form_the_encoder_would_send() {
    let endpoint = "https://search.example.test/api?count=5";

    // --- leg 1: the fixture, built by the production encoder ---------------
    //
    // A forwarding gate is installed, so the ONLY difference from leg 2 is the
    // scan's verdict. What leaves is what the daemon's own request builder
    // produced.
    let permissive_transport = LookupCapture::answering("{\"results\":[]}");
    let permissive = Egress::new(permissive_transport.clone(), Vec::new(), Arc::new(NoopSink))
        .with_search_redaction_gate(Arc::new(ForwardingGate));
    let clean = Flags::default();
    let ctx =
        LookupContext::new("sess-redact", &clean, &allow_any_host).with_search_endpoint(endpoint);

    let sent = permissive
        .lookup(
            &LookupRequest::search(PLANTED, Authorship::ModelComposed),
            &ctx,
        )
        .await;
    assert_eq!(
        sent.outcome(),
        WebLookupOutcome::Completed,
        "the permissive leg must actually send, or there is no fixture"
    );
    assert_eq!(permissive_transport.calls(), 1);

    let wire = permissive_transport.urls().remove(0);
    // The encoder transformed it: this is the whole of LESSON-490's point.
    assert!(
        wire.contains("STRIPE_API_KEY%3D"),
        "the production encoder percent-encodes the assignment; wire form: {wire}"
    );
    assert!(
        !wire.contains(&format!("STRIPE_API_KEY={PLANTED_VALUE}")),
        "the raw spelling is NOT what travels — a hand-written fixture would \
         have asserted against bytes the wire never carries; wire form: {wire}"
    );
    // The exact encoded secret, lifted from the encoder's output.
    let encoded_secret = wire
        .split("q=")
        .nth(1)
        .expect("the encoder put the query in `q`")
        .to_owned();
    assert!(
        PLANTED.contains(PLANTED_VALUE),
        "the fixture and its value must not drift apart"
    );
    assert!(encoded_secret.contains(PLANTED_VALUE));

    // --- leg 2: the same query, the production scanner ---------------------
    //
    // The scanner really does see this shape — asserted against the production
    // function directly, so a block below cannot be a gate that refuses
    // everything.
    let verdict = pattern_verdict(PLANTED);
    assert!(
        verdict.findings().iter().any(teton_findings_is_high),
        "the production pattern pass must find the planted credential: {verdict:?}"
    );

    let blocked_transport = LookupCapture::answering("{\"results\":[]}");
    let recorder = Arc::new(Recorder::default());
    let gate = ProductionPatternGate::new();
    let guarded = Egress::new(blocked_transport.clone(), Vec::new(), Arc::new(NoopSink))
        .with_search_redaction_gate(Arc::clone(&gate) as Arc<dyn RedactionGate>)
        .with_lookup_recorder(Arc::clone(&recorder) as Arc<dyn LookupRecorder>);
    let ctx =
        LookupContext::new("sess-redact", &clean, &allow_any_host).with_search_endpoint(endpoint);

    let refused = guarded
        .lookup(
            &LookupRequest::search(PLANTED, Authorship::ModelComposed),
            &ctx,
        )
        .await;

    assert_eq!(refused.outcome(), WebLookupOutcome::BlockedRedact);
    assert!(
        matches!(
            refused.detail(),
            LookupDetail::Blocked {
                cause: BlockCause::Redaction { .. }
            }
        ),
        "the block must name the inspection that refused it: {:?}",
        refused.detail()
    );
    assert_eq!(gate.calls(), 1, "the scan ran");
    assert_eq!(
        blocked_transport.calls(),
        0,
        "a blocked query must reach no transport at all"
    );
    // Byte-level: the encoder's own output appears in nothing that was sent.
    for request in blocked_transport.captured() {
        assert!(
            !contains_bytes(request.url.as_bytes(), &encoded_secret),
            "the encoded credential reached the wire"
        );
    }

    // --- leg 3: the same production gate forwards a clean query ------------
    let clean_transport = LookupCapture::answering("{\"results\":[]}");
    let clean_gate = ProductionPatternGate::new();
    let same_guard = Egress::new(clean_transport.clone(), Vec::new(), Arc::new(NoopSink))
        .with_search_redaction_gate(Arc::clone(&clean_gate) as Arc<dyn RedactionGate>);
    let ctx =
        LookupContext::new("sess-redact", &clean, &allow_any_host).with_search_endpoint(endpoint);
    let forwarded = same_guard
        .lookup(
            &LookupRequest::search(CLEAN_QUERY, Authorship::ModelComposed),
            &ctx,
        )
        .await;
    assert_eq!(
        forwarded.outcome(),
        WebLookupOutcome::Completed,
        "the production gate blocks the credential, not every query"
    );
    assert_eq!(clean_transport.calls(), 1);

    // BR-7: the refusal is a row, and the row carries the host and nothing else
    // — no query text anywhere in it.
    assert_eq!(recorder.outcomes(), vec![WebLookupOutcome::BlockedRedact]);
    let record = recorder.records().remove(0);
    assert_eq!(record.kind, WebLookupKind::Search);
    assert_eq!(record.host, "search.example.test");
    assert_eq!(record.bytes_in, 0);
    let rendered = format!("{record:?}");
    assert!(
        !rendered.contains("STRIPE_API_KEY") && !rendered.contains(PLANTED_VALUE),
        "a ledger row must not be able to carry query text: {rendered}"
    );
}

/// `Finding::is_high` behind a function so the closure above stays legible.
fn teton_findings_is_high(finding: &tetond::egress::redact::Finding) -> bool {
    finding.is_high() && finding.kind() == tetond::egress::redact::FindingKind::Credential
}

// ---------------------------------------------------------------------------
// AC-8 — offline
// ---------------------------------------------------------------------------

/// AC-8: with the destination unreachable, the lookup ends as `offline`, the
/// turn **completes**, and no error status is reported for the session.
///
/// The falsification leg is the same scripted session against a transport that
/// answers: it completes too, but with `completed` rather than `offline` — so
/// the taxonomy is being observed, not just the happy path.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_unreachable_destination_is_offline_and_the_turn_still_completes() {
    let args = r#"{"url":"https://docs.rs/tokio/latest/tokio/"}"#;

    let offline = scripted_session(
        "ac8-offline",
        web_config(WebTier::FetchAnyUrl),
        args,
        &[],
        vec![Err(TransportError::Connect)],
    )
    .await;

    assert!(
        offline.completed(),
        "BR-9: a lookup failure is never a turn error; got {:?}",
        offline.result
    );
    let outcome = offline.result.as_ref().expect("the turn completed");
    assert!(
        !outcome.final_text.is_empty(),
        "the model still finished its turn"
    );
    assert_eq!(offline.lookup_calls(), 1, "it genuinely tried");
    assert_eq!(
        offline
            .records
            .iter()
            .map(|r| r.outcome)
            .collect::<Vec<_>>(),
        vec![WebLookupOutcome::Offline],
        "an unreachable destination is `offline`, not a generic failure (BUG-152)"
    );

    // The model was told, in the transient-shaped words BR-9 asks for, and the
    // sentence travelled to the provider in the next turn's payload.
    let second = offline
        .provider_payloads
        .get(1)
        .expect("the turn made a second provider call");
    assert!(
        contains_bytes(second, "web lookup unavailable: offline"),
        "the model must be told the lookup did not happen, in transient-shaped words"
    );

    // --- falsification: the same session against a reachable host ----------
    let online = scripted_session(
        "ac8-online",
        web_config(WebTier::FetchAnyUrl),
        args,
        &[],
        vec![Ok((
            200,
            None,
            b"<html><body>pinning</body></html>".to_vec(),
        ))],
    )
    .await;
    assert!(online.completed());
    assert_eq!(
        online.records.iter().map(|r| r.outcome).collect::<Vec<_>>(),
        vec![WebLookupOutcome::Completed],
        "the offline ending above is a reading of the transport, not a constant"
    );

    offline.cleanup();
    online.cleanup();
}

// ---------------------------------------------------------------------------
// AC-10 — the cache
// ---------------------------------------------------------------------------

/// AC-10: a second granted lookup of the same URL within TTL is served from the
/// local cache with **zero** transport calls and a `cache_hit` row; an explicit
/// refresh re-fetches.
///
/// The refresh leg is this test's falsification: it is the same tool, the same
/// URL, and the same cache, differing only in that the user asked for the
/// stored copy to be dropped — so "the second call sent nothing" cannot be "this
/// tool sends nothing".
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_second_lookup_is_served_from_cache_with_zero_transport_calls() {
    let dir = scratch("ac10-cache");
    let url = "https://docs.rs/tokio/latest/tokio/";
    let config = web_config(WebTier::FetchAnyUrl);

    let transport = LookupCapture::new(vec![
        Ok((200, None, b"<html><body>first body</body></html>".to_vec())),
        Ok((200, None, b"<html><body>second body</body></html>".to_vec())),
    ]);
    let recorder = Arc::new(Recorder::default());
    let egress = Egress::new(transport.clone(), Vec::new(), Arc::new(NoopSink))
        .with_lookup_recorder(Arc::clone(&recorder) as Arc<dyn LookupRecorder>);
    let session_id = SessionId::from("sess-cache");
    let seam = Arc::new(CaptureSeam {
        egress,
        taint: Flags::default(),
        session: session_id.clone(),
        endpoint: None,
        recorder: Arc::clone(&recorder),
    });

    let bus = Arc::new(EventBus::new());
    // As above: `permissive()` leaves the web keys asking, so the fixture that
    // wants no prompt says so explicitly.
    let mut permissions = PermissionConfig::permissive();
    permissions.apply_web_permission(&[
        WebTier::FetchUserUrl,
        WebTier::FetchAnyUrl,
        WebTier::Search,
    ]);
    let gate = Arc::new(PermissionGate::new(
        session_id,
        permissions,
        Arc::clone(&bus),
        Arc::new(PendingPermissions::new()),
    ));
    let mut tools = ToolRegistry::with_builtins();
    assert!(register_web_tool(
        &mut tools,
        &config,
        WebCache::from_config(&dir, &config),
        Arc::new(Mutex::new(UserUrls::new())),
        gate,
        Arc::clone(&seam) as Arc<dyn WebLookupSeam>,
        Handle::current(),
    ));
    let tool = tools.get(WEB_TOOL_NAME).expect("the web tool registered");
    let tool_ctx = ToolContext::new(&dir);
    let args = json!({ "url": url });

    // --- lookup 1: the fetch -----------------------------------------------
    let first = tool.run(&tool_ctx, &args);
    assert!(!first.is_error, "{}", first.content);
    assert!(first.content.contains("first body"));
    assert_eq!(transport.calls(), 1);

    // --- lookup 2: the cache hit -------------------------------------------
    let second = tool.run(&tool_ctx, &args);
    assert!(!second.is_error, "{}", second.content);
    assert_eq!(
        transport.calls(),
        1,
        "BR-12: a fresh cache hit performs no egress at all"
    );
    assert!(
        second.content.contains("served from the local cache"),
        "the model is told nothing left the machine: {}",
        second.content
    );
    assert!(
        second.content.contains("first body"),
        "the hit serves the stored reduction: {}",
        second.content
    );
    assert_eq!(
        recorder.outcomes(),
        vec![WebLookupOutcome::Completed, WebLookupOutcome::CacheHit],
        "BR-7: a free lookup is still a lookup, and still a row"
    );
    let hit = recorder.records().remove(1);
    assert_eq!(hit.bytes_in, 0, "nothing came off the wire");
    assert_eq!(hit.host, "docs.rs", "and the row still names the host");

    // --- falsification: the explicit refresh re-fetches ---------------------
    //
    // `WebCache::evict` is exactly what the `web/refresh` RPC performs; the
    // cache is a path and a TTL, so a second handle on the same directory is
    // the same cache.
    let evicted = WebCache::from_config(&dir, &config)
        .evict(url)
        .expect("evicting a cached document must succeed");
    assert!(evicted, "there was a stored copy to drop");

    let third = tool.run(&tool_ctx, &args);
    assert!(!third.is_error, "{}", third.content);
    assert_eq!(
        transport.calls(),
        2,
        "an explicit refresh must re-fetch, or the cache-hit count above is meaningless"
    );
    assert!(
        third.content.contains("second body"),
        "and the re-fetch brought the new bytes: {}",
        third.content
    );

    std::fs::remove_dir_all(&dir).ok();
}

// ---------------------------------------------------------------------------
// AC-11 — local reduction
// ---------------------------------------------------------------------------

/// Bytes that exist only in the raw page: a script body, a style body, an HTML
/// comment, and a tag with an attribute. None may ever reach a remote provider.
const RAW_SCRIPT: &str = "RAW-ONLY-SCRIPT-9c1f";
const RAW_STYLE: &str = "RAW-ONLY-STYLE-4d2e";
const RAW_COMMENT: &str = "RAW-ONLY-COMMENT-77ab";
const RAW_ATTRIBUTE: &str = "RAW-ONLY-ATTR-b0c3";
/// Bytes that survive the reduction — the positive control that keeps the
/// absences above from being vacuous.
const REDUCED_TEXT: &str = "REDUCED-VISIBLE-3a7b";

fn hostile_page() -> Vec<u8> {
    format!(
        "<!doctype html>\n\
         <html><head>\n\
         <style>.x {{ content: \"{RAW_STYLE}\"; }}</style>\n\
         <script>var token = \"{RAW_SCRIPT}\";</script>\n\
         </head>\n\
         <body class=\"{RAW_ATTRIBUTE}\">\n\
         <!-- {RAW_COMMENT} -->\n\
         <p>{REDUCED_TEXT} tokio pins a task to its worker.</p>\n\
         </body></html>\n"
    )
    .into_bytes()
}

/// AC-11: after a fetch, **no raw page byte** appears in any remote-provider
/// payload for that turn — only the locally-produced reduction does.
///
/// The reduction is deterministic daemon code (BR-10, D-3), so this is a byte
/// assertion over the provider capture rather than a claim about intent. The
/// positive control — the reduced text *is* in a payload — is what makes the
/// four absences meaningful: a session where nothing reached the provider would
/// pass every negative on its own.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn no_raw_page_bytes_reach_any_remote_provider_payload() {
    let run = scripted_session(
        "ac11-reduce",
        web_config(WebTier::FetchAnyUrl),
        r#"{"url":"https://docs.rs/tokio/latest/tokio/"}"#,
        &[],
        vec![Ok((200, None, hostile_page()))],
    )
    .await;

    assert!(run.completed(), "{:?}", run.result);
    assert_eq!(run.lookup_calls(), 1, "the page really was fetched");
    assert_eq!(
        run.records.iter().map(|r| r.outcome).collect::<Vec<_>>(),
        vec![WebLookupOutcome::Completed]
    );

    // The positive control first: if this fails, every absence below is empty.
    assert!(
        run.provider_payloads
            .iter()
            .any(|body| contains_bytes(body, REDUCED_TEXT)),
        "the reduction never reached the provider, so the negatives below prove nothing"
    );

    for (i, body) in run.provider_payloads.iter().enumerate() {
        for raw in [RAW_SCRIPT, RAW_STYLE, RAW_COMMENT, RAW_ATTRIBUTE] {
            assert!(
                !contains_bytes(body, raw),
                "raw page bytes ({raw}) reached remote-provider payload #{i}"
            );
        }
        // Not one marker at a time: the reduction strips markup, so no tag from
        // the fetched document may survive into a payload either.
        assert!(
            !contains_bytes(body, "<script>"),
            "a fetched <script> tag reached remote-provider payload #{i}"
        );
    }

    run.cleanup();
}
