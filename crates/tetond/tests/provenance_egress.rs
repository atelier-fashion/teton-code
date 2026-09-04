//! Provenance-from-files-touched egress enforcement (REQ-544 C-1 + M-2).
//!
//! The BR-1 bypass this suite guards against: before REQ-544, a tool result's
//! egress provenance came from a literal `path` argument, so `shell`, `grep`, and
//! `glob` — which surface boundary-file content without a `path` arg — folded
//! into context with EMPTY provenance and could be laundered to a remote provider
//! on the next turn with no `privacy_block`.
//!
//! Each test drives the **real** OpenAI-compatible adapter through the **real**
//! egress choke point in front of a capture transport. The scripted remote model
//! reads a `local-only` file via `shell`/`grep`/`glob`; the loop folds that result
//! with the provenance of the files the tool *actually touched* (or UNKNOWN for
//! `shell`), and the *next* remote turn is blocked before a byte leaves. The
//! tests assert:
//!
//! 1. the turn is blocked (a privacy block, not a silent leak),
//! 2. zero boundary bytes reached the capture transport,
//! 3. exactly one `privacy_block` event fired, and
//! 4. the built-in tool result was framed as untrusted content (M-2).
//!
//! ## REQ-571 AC-8: the same pin, reached through a non-canonical spelling
//!
//! The cases at the bottom of this file add **no** behavior to assert. The
//! local-tier session pin and the model-composed web refusal they exercise were
//! delivered by BUG-156, which is resolved, and REQ-571 changed neither. What
//! REQ-571 changed is *which sessions arrive at them*: a `read` of a boundary
//! file spelled `/abs/root/secrets/prod.env` or `src/../secrets/prod.env` used
//! to be tagged with that string verbatim, and no repo-relative `secrets/**`
//! glob matches it — so the pin was never reached and the web channel never
//! closed. Those tests are reachability regressions, and each says so at its
//! own definition so a future reader does not read them as duplicate coverage
//! of the pin itself.

use std::collections::VecDeque;
use std::path::PathBuf;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use teton_core::effort::{EffortLevel, ResolvedEffort};

use async_trait::async_trait;

use teton_core::entities::{BoundaryMode, PrivacyBoundary};
use teton_protocol::events::{Event, PermissionRequest, WebLookupOutcome};
use teton_protocol::jsonrpc::RpcError;
use teton_protocol::methods::{
    BoundaryOriginConfig, ConfigUpdate, ContextAction, PermissionOutcome, PrivacyBoundaryConfig,
    PromptTurnResult, ProviderConfig, RepoContextSource, RepoContextStateKind,
    SessionContextParams, SkillInvocation, TierBindingConfig,
};
use teton_protocol::{
    Phase as ProtoPhase, PrivacyMode, ProviderId, ProviderKind as ProtoProviderKind, SessionId,
    SessionMode, Tier as ProtoTier,
};
use teton_providers::transport::{Transport, TransportError, TransportRequest, TransportResponse};
use teton_providers::{OpenAiCompatAdapter, OpenAiCompatConfig};

use tetond::broadcast::EventBus;
use tetond::carry::CarriedTurn;
use tetond::egress::{Authorship, Egress, LookupContext, LookupRequest, NoopSink, TaintView};
use tetond::grants::{ConnectionId, GrantRegistry};
use tetond::harness::permissions::AddressedPermissionDelivery;
use tetond::harness::{
    build_system_prompt, context_provenance, run_session_turn_with_source, ContextManager,
    DutyRoute, HarnessConfig, HarnessError, NoopProvenanceHook, PendingPermissions,
    PermissionConfig, PermissionGate, RemoteProviderSource, SessionEvents, ToolContext, ToolDuties,
    ToolRegistry,
};
use tetond::repo_context::{RepoContextBlock, RepoContextState};
use tetond::runtime::{ClientPresence, DaemonRuntime, SessionTaint, WebTaintOverride};
use tetond::sessions::SessionRegistry;
use tetond::skills::RealFs;

/// The boundary-file secret that must never reach the capture transport.
const SECRET: &str = "API_KEY=sk-live-DO-NOT-LEAK-provctl-Zx9";

/// A capturing OpenAI-compatible SSE transport: returns a queue of canned bodies
/// (one per turn) and records every request body it was asked to send.
#[derive(Clone, Default)]
struct CaptureSse {
    bodies: Arc<Mutex<VecDeque<String>>>,
    sent: Arc<Mutex<Vec<Vec<u8>>>>,
    calls: Arc<AtomicUsize>,
}

impl CaptureSse {
    fn with_bodies(bodies: Vec<String>) -> Self {
        Self {
            bodies: Arc::new(Mutex::new(bodies.into_iter().collect())),
            sent: Arc::new(Mutex::new(Vec::new())),
            calls: Arc::new(AtomicUsize::new(0)),
        }
    }
    fn captured(&self) -> Vec<Vec<u8>> {
        self.sent.lock().unwrap().clone()
    }

    /// How many requests this transport was asked to send.
    ///
    /// The instrument for a refusal, and deliberately a count rather than a
    /// reading of the outcome enum: "no packet left" is the whole content of the
    /// claim, and only the transport can settle it.
    fn calls(&self) -> usize {
        self.calls.load(Ordering::SeqCst)
    }
}

#[async_trait]
impl Transport for CaptureSse {
    async fn execute(
        &self,
        request: TransportRequest,
    ) -> Result<TransportResponse, TransportError> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        self.sent.lock().unwrap().push(request.body.clone());
        let body = self
            .bodies
            .lock()
            .unwrap()
            .pop_front()
            .unwrap_or_else(|| "data: [DONE]\n\n".to_owned());
        Ok(TransportResponse {
            location: None,
            status: 200,
            body: Box::pin(futures::stream::once(async move { Ok(body.into_bytes()) })),
        })
    }
}

/// One OpenAI-compatible streaming turn: a text delta, an optional tool call,
/// then usage + `[DONE]`.
fn sse_turn(text: &str, tool: Option<(&str, &str, &str)>) -> String {
    let mut s = String::new();
    let chunk = serde_json::json!({ "choices": [{ "delta": { "content": text } }] });
    s.push_str(&format!("data: {chunk}\n\n"));
    if let Some((id, name, args)) = tool {
        let chunk = serde_json::json!({
            "choices": [{ "delta": { "tool_calls": [{
                "index": 0, "id": id, "function": { "name": name, "arguments": args }
            }]}}]
        });
        s.push_str(&format!("data: {chunk}\n\n"));
        let finish =
            serde_json::json!({ "choices": [{ "delta": {}, "finish_reason": "tool_calls" }] });
        s.push_str(&format!("data: {finish}\n\n"));
    } else {
        let finish = serde_json::json!({ "choices": [{ "delta": {}, "finish_reason": "stop" }] });
        s.push_str(&format!("data: {finish}\n\n"));
    }
    let usage = serde_json::json!({ "usage": { "prompt_tokens": 10, "completion_tokens": 5 } });
    s.push_str(&format!("data: {usage}\n\n"));
    s.push_str("data: [DONE]\n\n");
    s
}

fn temp_repo() -> PathBuf {
    static SEQ: AtomicUsize = AtomicUsize::new(0);
    let root = std::env::temp_dir().join(format!(
        "teton-provctl-{}-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos(),
        SEQ.fetch_add(1, Ordering::SeqCst)
    ));
    std::fs::create_dir_all(root.join("src")).unwrap();
    std::fs::create_dir_all(root.join("secrets")).unwrap();
    std::fs::write(root.join("src/lib.rs"), "pub const A: u32 = 1;\n").unwrap();
    std::fs::write(root.join("secrets/prod.env"), format!("{SECRET}\n")).unwrap();
    root
}

fn boundaries() -> Vec<PrivacyBoundary> {
    vec![PrivacyBoundary {
        path_glob: "secrets/**".to_owned(),
        mode: BoundaryMode::LocalOnly,
        origin: Default::default(),
    }]
}

fn contains_bytes(haystack: &[u8], needle: &str) -> bool {
    haystack
        .windows(needle.len())
        .any(|w| w == needle.as_bytes())
}

/// The one prompt every fixture in this file opens with.
const PROMPT: &str = "Read the production config and summarize it.";

/// The profile every fixture here runs under: full, so the loop does not force
/// a verification nudge before the second (blocked) turn.
fn scripted_config() -> HarnessConfig {
    HarnessConfig::for_strong_model()
}

/// This file's system head, built from the same registry the loop dispatches
/// against.
fn scripted_system() -> String {
    build_system_prompt(&ToolRegistry::with_builtins(), &scripted_config())
}

/// Drive the loop over a **caller-owned** context: turn 1 is the scripted tool
/// call, turn 2 is the one that would carry its result to the wire. Returns the
/// loop result, the captured request bodies, and the `privacy_block` events.
///
/// Every caller but the REQ-571 control leg has turn 1 touch a boundary file, so
/// turn 2 is refused before a byte leaves; the control reads a public file and
/// turn 2 genuinely goes out, which is what keeps the refusals here meaning
/// something.
///
/// The context is a parameter rather than a local so the REQ-571 cases below can
/// hand it a [`CarriedTurn`]'s manager. The local-tier pin is evaluated at the
/// commit seam inside that type, and a fixture that owned its own
/// `ContextManager` could not reach the seam at all.
async fn drive_scripted_turn(
    repo: &std::path::Path,
    session_id: &SessionId,
    tool: (&str, &str, &str),
    ctx: &mut ContextManager,
) -> (
    Result<tetond::harness::TurnOutcome, HarnessError>,
    Vec<Vec<u8>>,
    Vec<teton_protocol::events::PrivacyBlock>,
) {
    // Turn 1: the tool call. Turn 2 carries its result, and on a boundary read
    // is refused before it can.
    let transport = CaptureSse::with_bodies(vec![
        sse_turn("Reading the config.", Some(tool)),
        sse_turn("should never send", None),
    ]);
    let capture = transport.clone();

    let bus = Arc::new(EventBus::new());
    let egress = Egress::new(transport, boundaries(), bus.clone());
    let provider = OpenAiCompatAdapter::new(OpenAiCompatConfig::new(
        "deepseek",
        "https://api.deepseek.com/v1/chat/completions",
    ));

    let mut source = RemoteProviderSource::new(
        &provider,
        &egress,
        "deepseek",
        "deepseek-chat",
        session_id.clone(),
        // REQ-559: an integration fixture states its effort like any other
        // call path — the field is required, so it cannot be forgotten.
        ResolvedEffort::effort(EffortLevel::High),
    );

    let config = scripted_config();
    let mut tools = ToolRegistry::with_builtins();
    // REQ-584 BR-6: the daemon registers `projects` per session, so a harness
    // that stands in for a real turn has to as well — otherwise the tool's own
    // boundary test would be exercising an "unknown tool" refusal.
    tetond::harness::tools::register_projects_tool(
        &mut tools,
        std::sync::Arc::new(tetond::projects::ProjectStore::in_memory()),
        None,
        None,
    );
    let tool_ctx = ToolContext::new(repo);

    let pending = Arc::new(PendingPermissions::new());
    let gate = PermissionGate::new(
        session_id.clone(),
        PermissionConfig::permissive(),
        Arc::clone(&bus),
        Arc::clone(&pending),
    );
    let events = SessionEvents::new(Arc::clone(&bus), session_id.clone());
    let mut hook = NoopProvenanceHook;
    let mut sub = bus.subscribe(256);

    let result = run_session_turn_with_source(
        &mut source,
        &tools,
        &tool_ctx,
        &gate,
        &events,
        ctx,
        &config,
        &mut hook,
        // REQ-558: this loop digests through the `digest` category. These turns
        // stay under the summarization threshold, so nothing is served — and an
        // unresolved route bounds mechanically rather than folding raw.
        &DutyRoute::unresolved("no digest route in this test"),
        &DutyRoute::unresolved("no compact route in this test"),
        // REQ-561: and no tool duty either. `triage` would rank a `grep`
        // result; these turns run no multi-match `grep`, and an unresolved
        // route returns the tool's own result unchanged.
        &ToolDuties {
            triage: &DutyRoute::unresolved("no triage route in this test"),
            shell: &DutyRoute::unresolved("no shell route in this test"),
        },
    )
    .await;

    // Drain privacy_block events.
    let mut blocks = Vec::new();
    while let Ok(Some(env)) =
        tokio::time::timeout(std::time::Duration::from_millis(50), sub.recv()).await
    {
        if let Event::PrivacyBlock(pb) = env.event {
            blocks.push(pb);
        }
    }
    (result, capture.captured(), blocks)
}

/// [`drive_scripted_turn`] over a context this fixture owns, for the REQ-544
/// cases that only inspect what the turn assembled.
async fn run_touching_tool(
    repo: &std::path::Path,
    tool: (&str, &str, &str),
) -> (
    Result<tetond::harness::TurnOutcome, HarnessError>,
    Vec<Vec<u8>>,
    Vec<teton_protocol::events::PrivacyBlock>,
    ContextManager,
) {
    let mut ctx = ContextManager::new(scripted_system(), scripted_config().context_budget_tokens);
    ctx.push_user(PROMPT);
    let (result, captured, blocks) =
        drive_scripted_turn(repo, &SessionId::from("provctl"), tool, &mut ctx).await;
    (result, captured, blocks, ctx)
}

/// Assert the shared BR-1 guarantee for a boundary-touching built-in tool.
fn assert_blocked_and_clean(
    result: &Result<tetond::harness::TurnOutcome, HarnessError>,
    captured: &[Vec<u8>],
    blocks: &[teton_protocol::events::PrivacyBlock],
) {
    // (1) The turn was blocked as a privacy block — not a leak, not a generic
    // transport retry.
    match result {
        Err(e) if e.is_privacy_blocked() => {}
        other => panic!("expected a privacy block, got {other:?}"),
    }
    // (2) Zero boundary bytes reached the wire in ANY captured request.
    for body in captured {
        assert!(
            !contains_bytes(body, SECRET),
            "boundary content leaked into captured egress"
        );
    }
    // (3) Exactly one privacy_block event (REQ-544 M-1 — no duplicate blocks).
    assert_eq!(blocks.len(), 1, "exactly one privacy_block");
}

/// The last tool-result block folded into `ctx`, framed as untrusted (M-2).
fn assert_last_tool_result_is_framed(ctx: &ContextManager) {
    use tetond::harness::context::Provenance;
    let framed = ctx
        .blocks()
        .iter()
        .rev()
        .find(|b| matches!(b.provenance, Provenance::Tool { .. }))
        .map(|b| b.text.clone())
        .expect("a tool result was folded into context");
    assert!(
        framed.contains("trust=\"untrusted\""),
        "built-in tool result must be framed as untrusted (M-2): {framed}"
    );
}

// ---------------------------------------------------------------------------
// shell — UNKNOWN provenance, fail-closed
// ---------------------------------------------------------------------------

#[tokio::test]
async fn shell_cat_of_a_boundary_file_blocks_the_next_remote_turn() {
    let repo = temp_repo();
    let (result, captured, blocks, ctx) = run_touching_tool(
        &repo,
        ("c1", "shell", r#"{"command":"cat secrets/prod.env"}"#),
    )
    .await;

    // A shell result cannot be attributed to a file set, so it is UNKNOWN — and
    // the context is therefore unknown-provenance, which egress fail-closes.
    assert!(
        context_provenance(&ctx).is_unknown(),
        "a shell result must taint the context as unknown provenance"
    );
    assert_blocked_and_clean(&result, &captured, &blocks);
    assert_last_tool_result_is_framed(&ctx);
    std::fs::remove_dir_all(&repo).ok();
}

// ---------------------------------------------------------------------------
// grep — matched-files provenance
// ---------------------------------------------------------------------------

#[tokio::test]
async fn grep_matching_a_boundary_file_blocks_the_next_remote_turn() {
    let repo = temp_repo();
    let (result, captured, blocks, ctx) =
        run_touching_tool(&repo, ("c1", "grep", r#"{"pattern":"sk-live"}"#)).await;

    // grep tagged the result with the matched boundary file.
    assert!(
        context_provenance(&ctx).contains("secrets/prod.env"),
        "grep must tag the result with the matched boundary file"
    );
    assert_blocked_and_clean(&result, &captured, &blocks);
    assert_eq!(blocks[0].path, "secrets/prod.env");
    assert_last_tool_result_is_framed(&ctx);
    std::fs::remove_dir_all(&repo).ok();
}

// ---------------------------------------------------------------------------
// glob — enumerated-files provenance
// ---------------------------------------------------------------------------

#[tokio::test]
async fn glob_enumerating_a_boundary_file_blocks_the_next_remote_turn() {
    let repo = temp_repo();
    let (result, captured, blocks, ctx) =
        run_touching_tool(&repo, ("c1", "glob", r#"{"pattern":"secrets/**"}"#)).await;

    assert!(
        context_provenance(&ctx).contains("secrets/prod.env"),
        "glob must tag the result with the enumerated boundary file"
    );
    assert_blocked_and_clean(&result, &captured, &blocks);
    assert_eq!(blocks[0].path, "secrets/prod.env");
    assert_last_tool_result_is_framed(&ctx);
    std::fs::remove_dir_all(&repo).ok();
}

/// **REQ-583 OQ-7, the adopted decision, pinned.** A `glob` that lists the
/// *directory* `secrets/` surfaces a name, not content: the outcome is tagged
/// with the bare identity `secrets`, and the boundary `secrets/**` — which
/// covers the files under `secrets/`, not the name itself — does not block the
/// next remote turn. The sibling case above (`secrets/**`, enumerating the
/// files) still blocks. A matcher change that made a bare directory identity
/// taint would fail this test by name, which is the point: the decision is
/// privacy-adjacent and was made deliberately (architecture ADR-3), so it is
/// pinned rather than left to whatever the matcher happens to do.
#[tokio::test]
async fn glob_listing_the_boundary_directory_by_name_tags_it_but_does_not_block() {
    let repo = temp_repo();
    let (result, captured, blocks, ctx) =
        run_touching_tool(&repo, ("c1", "glob", r#"{"pattern":"**/secrets"}"#)).await;

    // The positive half: the directory really was listed and tagged under
    // its bare identity — the listed name and the tagged identity are one
    // value (BR-9), and it is `secrets`, not a file under it.
    let framed = ctx
        .blocks()
        .iter()
        .rev()
        .find(|b| {
            matches!(
                b.provenance,
                tetond::harness::context::Provenance::Tool { .. }
            )
        })
        .map(|b| b.text.clone())
        .expect("the glob result was folded into context");
    assert!(
        framed.contains("secrets/"),
        "the directory must have been listed, marked as one: {framed}"
    );
    let provenance = context_provenance(&ctx);
    assert!(
        provenance.contains("secrets"),
        "the bare directory identity must be tagged: {:?}",
        provenance.sources().collect::<Vec<_>>()
    );
    assert!(
        !provenance.contains("secrets/prod.env"),
        "listing the directory by name enumerates no file under it: {:?}",
        provenance.sources().collect::<Vec<_>>()
    );

    // The decision: a name is not content, so the boundary over the content
    // does not fire, and the second turn goes out.
    assert!(
        result.is_ok(),
        "OQ-7 (adopted): a listed directory name does not taint — the next remote \
         turn must not be blocked: {result:?}"
    );
    assert!(
        blocks.is_empty(),
        "no privacy block was warranted: {blocks:?}"
    );
    assert_eq!(
        captured.len(),
        2,
        "the fixture must really have put a second request on the wire"
    );
    // And no boundary *content* left the machine either way.
    for body in &captured {
        assert!(!contains_bytes(body, SECRET), "boundary content leaked");
    }
    std::fs::remove_dir_all(&repo).ok();
}

// ---------------------------------------------------------------------------
// teton_docs — no repo file, and therefore no taint (REQ-577 BR-6)
// ---------------------------------------------------------------------------

/// **REQ-584 BR-5: the locator surfaces no repository content, so the session
/// it runs in is not pinned.**
///
/// The same negative claim `teton_docs` makes, over a different mechanism: this
/// tool *does* touch the filesystem — it reads directory names — and the
/// distinction the boundary cares about is that it never opens a file. The repo
/// here holds the boundary file every other test in this suite leaks through,
/// and the second remote turn still goes out.
///
/// Paired with its positive half in the same run (LESSON-520), so "no
/// provenance" is a statement about a served result rather than an empty one.
#[tokio::test]
async fn projects_touches_no_repo_file_and_leaves_the_next_remote_turn_free() {
    let repo = temp_repo();
    let (result, captured, blocks, ctx) =
        run_touching_tool(&repo, ("c1", "projects", r#"{}"#)).await;

    // The positive half: the tool answered and its answer was folded in.
    let framed = ctx
        .blocks()
        .iter()
        .rev()
        .find(|b| {
            matches!(
                b.provenance,
                tetond::harness::context::Provenance::Tool { .. }
            )
        })
        .map(|b| b.text.clone())
        .expect("the projects result was folded into context");
    assert!(
        framed.contains("no known projects") || framed.contains("/cd "),
        "the locator did not answer: {framed}"
    );
    assert_last_tool_result_is_framed(&ctx);

    // The negative half, which is the boundary claim (REQ-584 BR-5). The
    // locator reads **directory names**, never file bodies — so like
    // `teton_docs` it has knowable, empty provenance rather than `Unknown`.
    // `Unknown` here would fail-close egress over a list of folder names.
    let provenance = context_provenance(&ctx);
    assert!(
        !provenance.is_unknown(),
        "a directory listing has knowable provenance — `Unknown` would \
         fail-close egress over the machine's own project names"
    );
    assert_eq!(
        provenance.len(),
        0,
        "`projects` opened no path, so there is no identity to carry: {:?}",
        provenance.sources().collect::<Vec<_>>()
    );

    // And therefore the turn carrying the result is not refused — the leg that
    // makes the assertions above mean something (LESSON-479).
    assert!(
        result.is_ok(),
        "listing projects must not block the next remote turn: {result:?}"
    );
    assert!(
        blocks.is_empty(),
        "no privacy block was warranted: {blocks:?}"
    );
    assert_eq!(
        captured.len(),
        2,
        "the fixture must really have put a second request on the wire, or \
         'not blocked' is an observation about a turn that never happened"
    );
    for body in &captured {
        assert!(
            !contains_bytes(body, SECRET),
            "boundary content reached egress through a tool that reads no files"
        );
    }
}

/// **REQ-577 BR-6: the bundled-docs tool surfaces nothing from the repository,
/// so the session it runs in is not pinned.**
///
/// The boundary claim for `teton_docs` is a negative one, and a negative claim
/// needs the fixture that could falsify it: the repo here *has* the boundary
/// file every other test in this suite leaks through, and the second remote turn
/// nonetheless goes out. A tool that had quietly read a path — or tagged its
/// result `Unknown` the way `shell` must — would fail-close that turn, which is
/// exactly what the three tests above assert happening.
///
/// Paired with the positive half in the same run (LESSON-520): the topic body
/// really is in the context, framed as untrusted like every other built-in
/// result. Without that, "no provenance" would be a statement about an empty
/// result rather than about a served one.
#[tokio::test]
async fn teton_docs_touches_no_repo_file_and_leaves_the_next_remote_turn_free() {
    let repo = temp_repo();
    let (result, captured, blocks, ctx) =
        run_touching_tool(&repo, ("c1", "teton_docs", r#"{"topic":"providers"}"#)).await;

    // The positive half: the bundled topic was served and folded into context.
    let framed = ctx
        .blocks()
        .iter()
        .rev()
        .find(|b| {
            matches!(
                b.provenance,
                tetond::harness::context::Provenance::Tool { .. }
            )
        })
        .map(|b| b.text.clone())
        .expect("the docs result was folded into context");
    assert!(
        framed.contains("teton provider add"),
        "the providers topic was not served: {framed}"
    );
    assert_last_tool_result_is_framed(&ctx);

    // The negative half, which is the boundary claim.
    let provenance = context_provenance(&ctx);
    assert!(
        !provenance.is_unknown(),
        "a bundled body has knowable provenance — `Unknown` would fail-close egress over \
         the daemon's own documentation"
    );
    assert_eq!(
        provenance.len(),
        0,
        "`teton_docs` opened no path, so there is no identity to carry: {:?}",
        provenance.sources().collect::<Vec<_>>()
    );

    // And therefore the turn that carries the result is not refused — the leg
    // that makes the assertions above mean something (LESSON-479).
    assert!(
        result.is_ok(),
        "reading bundled docs must not block the next remote turn: {result:?}"
    );
    assert!(
        blocks.is_empty(),
        "no privacy block was warranted: {blocks:?}"
    );
    assert_eq!(
        captured.len(),
        2,
        "the fixture must really have put a second request on the wire, or 'not blocked' \
         is an observation about a turn that never happened"
    );
    for body in &captured {
        assert!(
            !contains_bytes(body, SECRET),
            "boundary content reached egress through a tool that reads no files"
        );
    }
    std::fs::remove_dir_all(&repo).ok();
}

// ---------------------------------------------------------------------------
// REQ-571 AC-8 — the taint pin and the web channel, reached by a non-canonical
// spelling
// ---------------------------------------------------------------------------

/// The one identity every spelling of the boundary file must mint (REQ-571 BR-2)
/// and the one value a repo-relative `secrets/**` glob can match.
const CANONICAL_ID: &str = "secrets/prod.env";

/// Distinctive bytes of the **public** file `temp_repo` writes, for the
/// falsification leg: a "nothing leaked" count over a fixture that forwards
/// nothing measures nothing (LESSON-479).
const PUBLIC_CONTENT: &str = "pub const A: u32 = 1;";

/// The lookup destination. A globally-classed host, so the only gate with an
/// opinion about it is the taint gate under test.
const LOOKUP_URL: &str = "https://docs.rs/tokio/latest/tokio/";

fn allow_any_host(_host: &str) -> bool {
    true
}

/// The lookup seam's read of the two session flags.
///
/// Composed here from the same two public types the daemon composes its own
/// `SessionTaintView` from, because that type's fields are private. Note what it
/// cannot do: neither flag can be *written* through this handle, so a pin
/// observed on the far side of it can only have come from the commit seam — and
/// the override is never lifted, which is the un-overridden default a session
/// starts in.
struct TaintFlags {
    taint: Arc<SessionTaint>,
    overridden: Arc<WebTaintOverride>,
}

impl TaintView for TaintFlags {
    fn is_tainted(&self, session: &SessionId) -> bool {
        self.taint.is_tainted(session)
    }

    fn is_overridden(&self, session: &SessionId) -> bool {
        self.overridden.is_lifted(session)
    }
}

/// Everything one session settles after a turn that read one file — as values,
/// so two spellings of that file can be compared rather than described.
#[derive(Debug, PartialEq, Eq)]
struct SessionProbe {
    /// The provenance ids the folded tool result carries.
    sources: Vec<String>,
    /// Or the fail-closed `Unknown` a `shell` result would carry instead.
    unknown: bool,
    /// The remote turn was refused as a privacy block, and how many
    /// `privacy_block` events said so.
    turn_blocked: bool,
    privacy_blocks: usize,
    /// The commit pinned the session to the local tier (REQ-544 C-2, evaluated
    /// by `runtime::context_is_sensitive` inside [`CarriedTurn`]).
    pinned: bool,
    /// A later **model-composed** `web_fetch` in the same session, and the
    /// lookup transport's call count once it has been answered.
    composed: WebLookupOutcome,
    packets_after_composed: usize,
    /// The user's own paste of the same URL in the same session, and the count
    /// after that. BR-13's asymmetry, and the leg that proves this fixture can
    /// put a packet on a wire at all.
    pasted: WebLookupOutcome,
    packets_after_pasted: usize,
}

/// Drive one session end to end: a scripted turn whose `read` names `path_arg`,
/// the commit that evaluates the pin, then the two authorships of a `web_fetch`
/// in the session that commit left behind.
///
/// Returns the probe and the provider request bodies the turn produced.
async fn probe_spelling(repo: &std::path::Path, path_arg: &str) -> (SessionProbe, Vec<Vec<u8>>) {
    let sessions = SessionRegistry::new();
    let session_id = sessions
        .create(SessionMode::Freeform, None, None)
        .expect("a freeform session needs no phase")
        .session_id;
    let taint = Arc::new(SessionTaint::new());

    // The real commit protocol. `runtime::context_is_sensitive` is crate-private
    // and `CarriedTurn` is the only path to it an integration test has — which
    // is also the only path production has.
    let mut turn = CarriedTurn::begin(
        &sessions,
        &session_id,
        scripted_system(),
        &scripted_config(),
        Arc::clone(&taint),
        boundaries(),
        PROMPT,
        std::collections::BTreeSet::new(),
        false,
        // No notes in this fixture, so a reroute has nothing to re-render.
        None,
    );

    let args = serde_json::json!({ "path": path_arg }).to_string();
    let (result, captured, blocks) =
        drive_scripted_turn(repo, &session_id, ("c1", "read", &args), turn.ctx_mut()).await;

    let provenance = context_provenance(turn.ctx());
    let sources: Vec<String> = provenance.sources().map(str::to_owned).collect();
    let unknown = provenance.is_unknown();

    assert!(
        !taint.is_tainted(&session_id),
        "the session was already pinned before the commit that is supposed to pin it, \
         so `pinned` below would say nothing about the commit seam"
    );
    // Production reaches this commit on the blocked leg too: a privacy-blocked
    // turn is not abandoned, it is rerouted to the local tier and re-run there,
    // and *that* attempt commits (`DaemonRuntime::run_prompt_turn`). This
    // fixture has no local engine to re-run against, so it commits the manager
    // the blocked attempt left — the same blocks, through the same seam.
    turn.commit();
    let pinned = taint.is_tainted(&session_id);

    // The second hop: what this session's web tool may do now that the turn has
    // committed. A fresh transport, so its call count is a count of *lookups*,
    // answering any lookup that does reach it with a page.
    let wire = CaptureSse::with_bodies(vec!["<html/>".to_owned(), "<html/>".to_owned()]);
    let web = Egress::new(wire.clone(), Vec::new(), Arc::new(NoopSink));
    let flags = TaintFlags {
        taint: Arc::clone(&taint),
        overridden: Arc::new(WebTaintOverride::new()),
    };
    let lookup_ctx = LookupContext::new(session_id.clone(), &flags, &allow_any_host);

    let composed = web
        .lookup(
            &LookupRequest::fetch(LOOKUP_URL, Authorship::ModelComposed),
            &lookup_ctx,
        )
        .await;
    let packets_after_composed = wire.calls();
    let pasted = web
        .lookup(
            &LookupRequest::fetch(LOOKUP_URL, Authorship::UserPasted),
            &lookup_ctx,
        )
        .await;
    let packets_after_pasted = wire.calls();

    (
        SessionProbe {
            sources,
            unknown,
            turn_blocked: result
                .as_ref()
                .err()
                .is_some_and(|e| e.is_privacy_blocked()),
            privacy_blocks: blocks.len(),
            pinned,
            composed: composed.outcome(),
            packets_after_composed,
            pasted: pasted.outcome(),
            packets_after_pasted,
        },
        captured,
    )
}

/// What a session that read the boundary file settles at, whatever the model
/// spelled: blocked at the wire, pinned at the commit, and closed to the model's
/// own web lookups afterwards — while the user's paste still goes out.
fn tainted_probe() -> SessionProbe {
    SessionProbe {
        sources: vec![CANONICAL_ID.to_owned()],
        unknown: false,
        turn_blocked: true,
        privacy_blocks: 1,
        pinned: true,
        composed: WebLookupOutcome::TaintRestricted,
        packets_after_composed: 0,
        pasted: WebLookupOutcome::Completed,
        packets_after_pasted: 1,
    }
}

/// The same session having read a file no boundary names: nothing blocked,
/// nothing pinned, and both authorships reach the wire.
fn untainted_probe() -> SessionProbe {
    SessionProbe {
        sources: vec!["src/lib.rs".to_owned()],
        unknown: false,
        turn_blocked: false,
        privacy_blocks: 0,
        pinned: false,
        composed: WebLookupOutcome::Completed,
        packets_after_composed: 1,
        pasted: WebLookupOutcome::Completed,
        packets_after_pasted: 2,
    }
}

/// The absolute-inside-root spelling of the boundary file, as a model would put
/// it in a tool argument.
fn absolute_spelling(repo: &std::path::Path) -> String {
    repo.canonicalize()
        .unwrap()
        .join(CANONICAL_ID)
        .to_string_lossy()
        .into_owned()
}

fn assert_no_boundary_bytes(captured: &[Vec<u8>]) {
    for body in captured {
        assert!(
            !contains_bytes(body, SECRET),
            "boundary content leaked into captured egress"
        );
    }
}

/// **REQ-571 AC-8 — the absolute-inside-root spelling.**
///
/// This test verifies that *existing* behavior is now **reached**; it adds no
/// new behavior. The local-tier pin and the model-composed web refusal it
/// asserts were both delivered by **BUG-156** (resolved), and REQ-571 does not
/// touch either: `context_is_sensitive` and the lookup taint gate are unchanged.
/// What changed is that this spelling gets there. Before TASK-119 the `read`
/// tagged the result with `/abs/root/secrets/prod.env` verbatim, `secrets/**`
/// matched nothing, and every assertion below was false for this spelling while
/// being true for the canonical one — which is the divergence BR-9 forbids.
#[tokio::test]
async fn a_session_tainted_by_an_absolute_spelling_reaches_the_pin_and_closes_the_web() {
    let repo = temp_repo();

    let (probe, captured) = probe_spelling(&repo, &absolute_spelling(&repo)).await;
    assert_eq!(
        probe,
        tainted_probe(),
        "the absolute-inside-root spelling did not settle where the canonical one does"
    );
    assert_no_boundary_bytes(&captured);

    // The falsification pair, in this test and through the same driver: a
    // session whose `read` named a file no boundary covers pins nothing, and
    // the model's own lookup leaves as a packet. Without it, "refused" and
    // "pinned" could both be what this fixture says about everything.
    let (clean, clean_captured) = probe_spelling(&repo, "src/lib.rs").await;
    assert_eq!(
        clean,
        untainted_probe(),
        "the control leg did not behave as an unpinned session"
    );
    assert!(
        clean_captured
            .iter()
            .any(|body| contains_bytes(body, PUBLIC_CONTENT)),
        "the control never forwarded the file it read, so the zero-leak claim above is vacuous"
    );

    std::fs::remove_dir_all(&repo).ok();
}

/// **REQ-571 AC-8 — the `..`-traversing spelling.**
///
/// As above, and for the same reason: existing BUG-156 behavior, newly reached.
/// This spelling is the interesting member of the BR-3 set — `teton-core`
/// refuses it un-canonicalized by design, so it is `ToolContext::resolve`'s
/// canonicalization that makes `src/../secrets/prod.env` agree with
/// `secrets/prod.env`, and therefore the tool layer where the pin becomes
/// reachable.
#[tokio::test]
async fn a_session_tainted_by_a_traversing_spelling_reaches_the_pin_and_closes_the_web() {
    let repo = temp_repo();

    let (probe, captured) = probe_spelling(&repo, &format!("src/../{CANONICAL_ID}")).await;
    assert_eq!(
        probe,
        tainted_probe(),
        "the `..`-traversing spelling did not settle where the canonical one does"
    );
    assert_no_boundary_bytes(&captured);

    // The same falsification pair, minted for this test rather than shared with
    // the one above: neither spelling's cover may ride on the other's
    // (LESSON-502).
    let (clean, clean_captured) = probe_spelling(&repo, "src/lib.rs").await;
    assert_eq!(
        clean,
        untainted_probe(),
        "the control leg did not behave as an unpinned session"
    );
    assert!(
        clean_captured
            .iter()
            .any(|body| contains_bytes(body, PUBLIC_CONTENT)),
        "the control never forwarded the file it read, so the zero-leak claim above is vacuous"
    );

    std::fs::remove_dir_all(&repo).ok();
}

/// **REQ-571 AC-8, the control: one path, not two.**
///
/// The two tests above say each non-canonical spelling reaches BUG-156's pin.
/// This one says the canonical spelling has not quietly become a *different*
/// path in the process — the three settle at byte-identical probes, so there is
/// no spelling-shaped fork in which one leg could later regress alone. It is
/// still cover for reachability of existing behavior: nothing here is new, and
/// `tainted_probe()` is asserted of the canonical leg first so the comparison
/// cannot be satisfied by three sessions that all fail to pin.
#[tokio::test]
async fn every_spelling_settles_identically_to_the_canonical_one() {
    let repo = temp_repo();

    let canonical = probe_spelling(&repo, CANONICAL_ID).await.0;
    let absolute = probe_spelling(&repo, &absolute_spelling(&repo)).await.0;
    let traversing = probe_spelling(&repo, &format!("src/../{CANONICAL_ID}"))
        .await
        .0;

    assert_eq!(
        canonical,
        tainted_probe(),
        "the canonical spelling itself stopped reaching the pin"
    );
    assert_eq!(
        absolute, canonical,
        "the absolute-inside-root spelling and the canonical one diverged"
    );
    assert_eq!(
        traversing, canonical,
        "the `..`-traversing spelling and the canonical one diverged"
    );

    std::fs::remove_dir_all(&repo).ok();
}

// ---------------------------------------------------------------------------
// REQ-585 AC-11(b) / AC-11(c) — a skill that ran a command, at the choke point
// ---------------------------------------------------------------------------
//
// BR-7's second half. A dynamic-context command's output is `shell` output: the
// daemon cannot attribute it to a file set, so it enters the expansion with
// nothing that can be pinned. On a boundary-configured machine that fails
// closed, so an invocation that ran **any** command pins its turn to the local
// tier — exactly as [`shell_cat_of_a_boundary_file_blocks_the_next_remote_turn`]
// above pins a `shell` result's, and for the same reason.
//
// The two cases are a pair on purpose (LESSON-520): (b) is a refusal and (c) is
// the send that makes it mean something. Without (c), "nothing left the machine"
// would also be true of a harness that could not send at all; without (b), (c)
// would be a turn nobody claimed should have been stopped.
//
// The expansion is built by the real expander over a real `SKILL.md`, and the
// commands are run by the real runner, so what reaches the wire in (c) is the
// product's own bytes rather than a string this file composed.

/// The skill AC-11(b)/(c) are written against: a body that names its argument
/// and inlines one command's output.
const CTX_SKILL: &str = "---\ndescription: the context skill\n---\n\
                         About $ARGUMENTS:\n\n!`echo DYNAMIC-OUTPUT-MARKER`\n";

/// A sibling with the same shape and **no** dynamic context — the control that
/// isolates "a command ran" as the cause of (b)'s refusal.
const STATIC_SKILL: &str = "---\ndescription: the static skill\n---\n\
                            About $ARGUMENTS: STATIC-BODY-MARKER\n";

/// The expansion of one fixture skill, with its dynamic commands actually run,
/// plus whether the result can be pinned — the two values
/// `DaemonRuntime::run_prompt_turn` carries into the seed.
///
/// `unknown` is `sources-could-not-mint  ||  any command **spawned**`, which is
/// the daemon's own line (`skill.unknown |= outcomes.iter().any(spawned)`). A
/// project skill under the root mints, so in these fixtures the second disjunct
/// is the only one that can be true — which is what makes (b) a claim about the
/// *command* rather than about where the file lives (that is AC-11(a)'s, in
/// `egress_capture.rs`).
///
/// **`spawned`, not `did_run`, and this helper was written the other way.**
/// REQ-587 TASK-222 corrected it. `did_run` is the `Ran` arm alone, so a command
/// that started and exited non-zero — or was killed at the deadline — read as
/// "no command ran" here while the daemon marked the block unpinnable. The
/// helper agreed with the daemon on the fixtures it happened to carry (both of
/// which exit zero) and disagreed with it on the one predicate AC-11(b) exists
/// to pin: an exit status is a value the command *chose*, and a mirror of the
/// daemon's rule that drops the failing arms is exactly the side channel
/// REQ-585's verify closed (LESSON-528's shape — the mirrored predicate that is
/// identical until one side is edited).
fn ran_expansion(repo: &std::path::Path, name: &str, arguments: &str) -> (String, bool) {
    let registry = tetond::skills::discover(
        None,
        repo,
        teton_protocol::methods::RootKind::Project,
        &tetond::skills::RealFs,
    );
    let skill = registry
        .dispatchable_by_user(name)
        .unwrap_or_else(|| panic!("the fixture must register `{name}`"));
    let expansion = tetond::skills::expand(skill, arguments, &format!(".claude/…/{name}"));
    let outcomes = tetond::skills::run_all(repo, expansion.commands(), 10_000);
    let ran = outcomes.iter().any(tetond::skills::DynamicOutcome::spawned);
    // The user path's frame, which is the one `accept_invocation` supplies
    // (REQ-587 ADR-6) — the daemon's own line, not a paraphrase of it.
    let frame = expansion.user_frame();
    (expansion.fold(&frame, &outcomes), ran)
}

/// A repo holding the two fixture skills beside the boundary file.
fn skill_repo() -> PathBuf {
    let repo = temp_repo();
    for (name, body) in [("ctx", CTX_SKILL), ("static", STATIC_SKILL)] {
        let dir = repo.join(".claude/skills").join(name);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("SKILL.md"), body).unwrap();
    }
    repo
}

/// Drive one remote turn over a caller-seeded context and a caller-chosen
/// boundary set, and report what the transport saw.
///
/// A single scripted reply with no tool call: the whole claim is about the
/// **first** request, which is the one carrying the skill expansion. (The
/// fixtures above need two turns because their provenance arrives on a tool
/// result; a skill's arrives with the seed.)
async fn drive_seeded_turn(
    repo: &std::path::Path,
    session_id: &SessionId,
    boundary_set: Vec<PrivacyBoundary>,
    ctx: &mut ContextManager,
) -> (
    Result<tetond::harness::TurnOutcome, HarnessError>,
    Vec<Vec<u8>>,
    usize,
    Vec<teton_protocol::events::PrivacyBlock>,
) {
    let transport = CaptureSse::with_bodies(vec![sse_turn("Understood.", None)]);
    let capture = transport.clone();

    let bus = Arc::new(EventBus::new());
    let egress = Egress::new(transport, boundary_set, bus.clone());
    let provider = OpenAiCompatAdapter::new(OpenAiCompatConfig::new(
        "deepseek",
        "https://api.deepseek.com/v1/chat/completions",
    ));
    let mut source = RemoteProviderSource::new(
        &provider,
        &egress,
        "deepseek",
        "deepseek-chat",
        session_id.clone(),
        ResolvedEffort::effort(EffortLevel::High),
    );

    let config = scripted_config();
    let tools = ToolRegistry::with_builtins();
    let tool_ctx = ToolContext::new(repo);
    let pending = Arc::new(PendingPermissions::new());
    let gate = PermissionGate::new(
        session_id.clone(),
        PermissionConfig::permissive(),
        Arc::clone(&bus),
        Arc::clone(&pending),
    );
    let events = SessionEvents::new(Arc::clone(&bus), session_id.clone());
    let mut hook = NoopProvenanceHook;
    let mut sub = bus.subscribe(256);

    let result = run_session_turn_with_source(
        &mut source,
        &tools,
        &tool_ctx,
        &gate,
        &events,
        ctx,
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

    let mut blocks = Vec::new();
    while let Ok(Some(env)) =
        tokio::time::timeout(std::time::Duration::from_millis(50), sub.recv()).await
    {
        if let Event::PrivacyBlock(pb) = env.event {
            blocks.push(pb);
        }
    }
    (result, capture.captured(), capture.calls(), blocks)
}

/// **AC-11(b).** A skill that ran **any** dynamic command pins its turn local
/// wherever **any** boundary is configured — the boundary need have nothing to
/// do with the command, because the output has no identity for a glob to be
/// compared against in the first place.
///
/// The control runs first and on the same boundary set: the sibling skill with
/// no dynamic context genuinely reaches the wire. So the refusal below is "a
/// command ran", not "this repo has a boundary".
#[tokio::test]
async fn a_skill_that_ran_a_command_blocks_the_remote_turn_under_any_boundary() {
    let repo = skill_repo();

    // The control: same repo, same boundary, no dynamic context.
    let (still, ran) = ran_expansion(&repo, "static", "the control");
    assert!(!ran, "fixture: the control skill runs no command");
    let mut ctx = ContextManager::new(scripted_system(), scripted_config().context_budget_tokens);
    ctx.push_user_from(still, std::collections::BTreeSet::new(), false);
    let (result, captured, calls, blocks) = drive_seeded_turn(
        &repo,
        &SessionId::from("skill-static"),
        boundaries(),
        &mut ctx,
    )
    .await;
    assert!(
        result.is_ok() && calls == 1 && blocks.is_empty(),
        "a skill with no dynamic context must still reach the provider under a \
         boundary it does not touch: {result:?}, {calls} call(s), {blocks:?}"
    );
    assert!(
        contains_bytes(&captured[0], "STATIC-BODY-MARKER"),
        "the control never put the expansion on the wire, so the refusal below \
         would say nothing"
    );

    // The claim.
    let (expansion, ran) = ran_expansion(&repo, "ctx", "the real one");
    assert!(
        ran && expansion.contains("DYNAMIC-OUTPUT-MARKER"),
        "fixture: the command must actually have run and been folded in"
    );
    let mut ctx = ContextManager::new(scripted_system(), scripted_config().context_budget_tokens);
    // The seed the daemon builds: a project skill mints, and the ran command is
    // what marks the block unpinnable.
    ctx.push_user_from(expansion, std::collections::BTreeSet::new(), ran);
    assert!(
        context_provenance(&ctx).is_unknown(),
        "an invocation that ran a command must taint the context as unknown \
         provenance, exactly as a `shell` result does"
    );
    let (result, captured, calls, blocks) =
        drive_seeded_turn(&repo, &SessionId::from("skill-ctx"), boundaries(), &mut ctx).await;

    match &result {
        Err(e) if e.is_privacy_blocked() => {}
        other => panic!("expected a privacy block, got {other:?}"),
    }
    assert_eq!(calls, 0, "not one packet may leave: {calls} call(s)");
    assert!(
        captured.is_empty(),
        "the transport was asked to send something"
    );
    assert_eq!(blocks.len(), 1, "exactly one privacy_block: {blocks:?}");
    std::fs::remove_dir_all(&repo).ok();
}

/// **AC-11(c).** With no boundary configured, the choke point does not inspect —
/// so a skill that ran a command reaches the remote provider, and the payload
/// the provider received **is the expansion**.
///
/// Not "a request went out": the captured body is searched for the three things
/// the expander actually produced — the preamble naming the file, the argument
/// string substituted into the body, and the command's stdout inside the
/// untrusted envelope the fold splices it into. A turn that reached the wire
/// carrying something else would satisfy a call count and fails this.
#[tokio::test]
async fn with_no_boundary_a_skills_expansion_reaches_the_provider_as_the_payload() {
    let repo = skill_repo();
    let (expansion, ran) = ran_expansion(&repo, "ctx", "teton  code \"repo\"");
    assert!(ran, "fixture: the command must actually have run");

    let mut ctx = ContextManager::new(scripted_system(), scripted_config().context_budget_tokens);
    ctx.push_user_from(expansion.clone(), std::collections::BTreeSet::new(), ran);
    let (result, captured, calls, blocks) = drive_seeded_turn(
        &repo,
        &SessionId::from("skill-open"),
        // The whole difference from the test above.
        Vec::new(),
        &mut ctx,
    )
    .await;

    assert!(result.is_ok(), "no boundary, no refusal: {result:?}");
    assert_eq!(calls, 1, "exactly one request went out");
    assert!(blocks.is_empty(), "nothing to block: {blocks:?}");

    let body = &captured[0];
    for fragment in [
        // The preamble the expander writes, naming the file the body came from.
        ".claude/…/ctx",
        // BR-4: the arguments reach `$ARGUMENTS` with both interior spaces and
        // both quotes intact — now inside BR-4's argument sub-frame, which
        // BUG-190 draws around a splice as well as around the trailer.
        // (JSON-escaped on the wire, hence the escapes.)
        "About <skill-arguments>teton  code \\\"repo\\\"</skill-arguments>:",
        // The command's stdout, folded into the body.
        "DYNAMIC-OUTPUT-MARKER",
        // …inside the envelope every built-in result gets.
        "trust=\\\"untrusted\\\"",
    ] {
        assert!(
            contains_bytes(body, fragment),
            "the payload is not the expansion — `{fragment}` is missing from \
             what the provider received:\n{}",
            String::from_utf8_lossy(body)
        );
    }
    std::fs::remove_dir_all(&repo).ok();
}

// ---------------------------------------------------------------------------
// REQ-587 AC-11 — the same four facts, for a **model**-issued invocation
// ---------------------------------------------------------------------------
//
// The legs above seed the expansion the way `run_prompt_turn` seeds a typed
// `/name`. A model invocation reaches context by a different route entirely: the
// `skill` tool decides the provenance (`expand_and_fold`'s
// `match (skill.source, spawned)`), the loop folds the result with it, and only
// then does the *next* remote call meet the choke point. Nothing above exercises
// that decision, so a build that returned `Sources(∅)` from the tool — the
// `ToolOutcome::ok` default, and fail-open for a skill body — would leave every
// test in this file green while a user skill's bytes left a
// boundary-configured machine.
//
// So these four drive the real tool through the real loop in front of the real
// `Egress`, and read what the transport saw. They are **four legs and not one**
// because REQ-585 ADR-9 made them four different facts:
//
//   (a)  a project skill under a `local-only` boundary pins, against the file's
//        own root-relative identity — the same verdict a `read` earns;
//   (a2) a user skill has **no** root-relative identity, so it pins under *any*
//        boundary, related or not — stricter than (a), and asserted apart from
//        it so neither can carry the other;
//   (b)  a command that **spawned** pins, whatever it exited with;
//   (c)  with no boundary, the expansion reaches the provider.

/// The two skills the model-path legs invoke, plus a home to put a user one in.
///
/// Deterministic and in-repo: every byte here is written by this function, and
/// nothing reads `~/.claude` (LESSON-540).
fn model_skill_trees() -> (PathBuf, PathBuf) {
    let repo = temp_repo();
    let home = repo.join("home-outside");
    let write = |path: PathBuf, body: &str| {
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, body).unwrap();
    };
    // A project skill whose body *is* the boundary file's bytes.
    write(
        repo.join(".claude/skills/guarded/SKILL.md"),
        &format!("---\ndescription: the guarded skill\n---\n\n{SECRET}\n"),
    );
    // A project skill the boundary does not cover, with no dynamic context.
    write(
        repo.join(".claude/skills/open/SKILL.md"),
        "---\ndescription: the public skill\n---\n\nPUBLIC-SKILL-BODY\n",
    );
    // A project skill whose one command **fails**: `spawned` is true and
    // `did_run` is false, which is the difference AC-11(b) turns on.
    write(
        repo.join(".claude/skills/failing/SKILL.md"),
        "---\ndescription: a skill whose command exits non-zero\n---\n\n\
         FAILING-BODY-MARKER: !`echo SPAWNED-MARKER; exit 3`\n",
    );
    // A project skill whose one command **succeeds**, for the leg that asserts
    // what actually reaches the provider: a failed command leaves a `not run`
    // placeholder rather than output, so it cannot carry the envelope claim.
    write(
        repo.join(".claude/skills/ran/SKILL.md"),
        "---\ndescription: a skill whose command runs\n---\n\n\
         RAN-BODY-MARKER: !`echo RAN-OUTPUT-MARKER`\n",
    );
    // A user skill, outside the root by construction.
    write(
        home.join(".claude/skills/usr/SKILL.md"),
        "---\ndescription: the user skill\n---\n\nUSER-SKILL-BODY\n",
    );
    (repo, home)
}

/// Drive one **model-issued** `skill` call through the loop in front of a real
/// `Egress` carrying `boundary_set`, and report what the transport saw.
///
/// Two scripted remote turns: the first issues the call, the second is the one
/// that would carry the expansion to the wire — and is refused before a byte
/// leaves whenever the fold's provenance pins the turn.
///
/// The system prompt is built from the **same** registry the loop dispatches
/// against, which is what makes the `skill` tool's presence a property of one
/// value rather than of two that happen to agree.
async fn drive_model_skill_call(
    repo: &std::path::Path,
    home: Option<&std::path::Path>,
    session_id: &SessionId,
    skill: &str,
    boundary_set: Vec<PrivacyBoundary>,
) -> (
    Result<tetond::harness::TurnOutcome, HarnessError>,
    Vec<Vec<u8>>,
    usize,
    Vec<teton_protocol::events::PrivacyBlock>,
) {
    let transport = CaptureSse::with_bodies(vec![
        sse_turn(
            "Fetching the skill.",
            Some((
                "skill-1",
                "skill",
                &serde_json::json!({ "name": skill }).to_string(),
            )),
        ),
        sse_turn("Understood.", None),
    ]);
    let capture = transport.clone();

    let bus = Arc::new(EventBus::new());
    let egress = Egress::new(transport, boundary_set, bus.clone());
    let provider = OpenAiCompatAdapter::new(OpenAiCompatConfig::new(
        "deepseek",
        "https://api.deepseek.com/v1/chat/completions",
    ));
    let mut source = RemoteProviderSource::new(
        &provider,
        &egress,
        "deepseek",
        "deepseek-chat",
        session_id.clone(),
        ResolvedEffort::effort(EffortLevel::High),
    );

    let pending = Arc::new(PendingPermissions::new());
    let gate = Arc::new(PermissionGate::new(
        session_id.clone(),
        PermissionConfig::permissive(),
        Arc::clone(&bus),
        Arc::clone(&pending),
    ));

    let registry = Arc::new(tetond::skills::discover(
        home,
        repo,
        teton_protocol::methods::RootKind::Project,
        &tetond::skills::RealFs,
    ));
    let mut tools = ToolRegistry::with_builtins();
    assert!(
        tetond::harness::tools::register_skill_tool(
            &mut tools,
            Arc::clone(&registry),
            Arc::clone(&gate),
            Some(tetond::grants::GrantRegistry::new().next_connection_id()),
            tokio::runtime::Handle::current(),
            10_000,
        ),
        "the fixture must register at least one model-invocable skill"
    );

    let config = scripted_config();
    let mut ctx = ContextManager::new(
        build_system_prompt(&tools, &config),
        config.context_budget_tokens,
    );
    ctx.push_user(PROMPT);
    let tool_ctx = ToolContext::new(repo);
    let events = SessionEvents::new(Arc::clone(&bus), session_id.clone());
    let mut hook = NoopProvenanceHook;
    let mut sub = bus.subscribe(256);

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

    let mut blocks = Vec::new();
    while let Ok(Some(env)) =
        tokio::time::timeout(std::time::Duration::from_millis(50), sub.recv()).await
    {
        if let Event::PrivacyBlock(pb) = env.event {
            blocks.push(pb);
        }
    }
    (result, capture.captured(), capture.calls(), blocks)
}

/// **AC-11(a).** A model-invoked **project** skill under a `local-only`
/// boundary pins the turn local and nothing leaves — against the file's own
/// root-relative identity, which `ProvenanceId::from_resolved` mints exactly as
/// it does for a typed `/name`.
///
/// The control runs first on the same boundary set: a project skill the
/// boundary does not cover genuinely reaches the wire carrying its own body, so
/// the refusal is "this file is guarded" rather than "an egress that refuses
/// everything" (LESSON-479).
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_model_invoked_project_skill_under_a_boundary_pins_the_turn_and_nothing_leaves() {
    let (repo, home) = model_skill_trees();
    let guarded = vec![PrivacyBoundary {
        path_glob: ".claude/skills/guarded/**".to_owned(),
        mode: BoundaryMode::LocalOnly,
        origin: Default::default(),
    }];

    // The control: a project skill outside the boundary.
    let (result, captured, calls, blocks) = drive_model_skill_call(
        &repo,
        Some(&home),
        &SessionId::from("model-skill-open"),
        "open",
        guarded.clone(),
    )
    .await;
    assert!(
        result.is_ok() && calls == 2 && blocks.is_empty(),
        "a project skill outside every boundary must still reach the wire: \
         {result:?}, {calls} call(s), {blocks:?}"
    );
    assert!(
        contains_bytes(&captured[1], "PUBLIC-SKILL-BODY"),
        "the control never put the expansion on the wire, so the refusal below \
         would say nothing"
    );

    // The claim.
    let (result, captured, calls, blocks) = drive_model_skill_call(
        &repo,
        Some(&home),
        &SessionId::from("model-skill-guarded"),
        "guarded",
        guarded,
    )
    .await;
    match &result {
        Err(e) if e.is_privacy_blocked() => {}
        other => panic!("a model-invoked skill under a boundary must pin: {other:?}"),
    }
    assert_eq!(
        calls, 1,
        "only the call that asked for the skill may go out; the one carrying it \
         must be refused before a byte leaves"
    );
    assert_eq!(blocks.len(), 1, "exactly one privacy_block: {blocks:?}");
    assert_eq!(
        blocks[0].path, ".claude/skills/guarded/SKILL.md",
        "the block must name the skill file, exactly as a `read` of it would — \
         a divergent identity is a block that happened to fire"
    );
    for request in &captured {
        assert!(
            !contains_bytes(request, SECRET),
            "boundary bytes reached the wire from a model-invoked expansion"
        );
    }
    std::fs::remove_dir_all(&repo).ok();
}

/// **AC-11(a2).** A model-invoked **user** skill has no root-relative identity
/// at all — `from_resolved` refuses rather than inventing one — so its block is
/// `Unknown` and the turn pins wherever **any** boundary is configured, whether
/// or not that boundary has anything to do with the file.
///
/// Stricter than (a), and stricter than what a `read` of the same bytes would
/// earn, which is why it is asserted apart: the asymmetry *is* the claim. On the
/// same egress, under the same unrelated boundary, the project skill above goes
/// out and this one does not.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_model_invoked_user_skill_pins_the_turn_wherever_any_boundary_exists() {
    let (repo, home) = model_skill_trees();

    // The contrast: the same unrelated boundary, a project skill, and the wire.
    let (result, captured, calls, blocks) = drive_model_skill_call(
        &repo,
        Some(&home),
        &SessionId::from("model-skill-project-unrelated"),
        "open",
        boundaries(),
    )
    .await;
    assert!(
        result.is_ok() && calls == 2 && blocks.is_empty(),
        "a project skill under an unrelated boundary must still reach the wire — \
         without this the refusal below would say nothing about *user* skills: \
         {result:?}, {calls} call(s)"
    );
    assert!(contains_bytes(&captured[1], "PUBLIC-SKILL-BODY"));

    // The claim: a user skill cannot be pinned, so it is.
    let (result, captured, calls, blocks) = drive_model_skill_call(
        &repo,
        Some(&home),
        &SessionId::from("model-skill-user"),
        "usr",
        // Names nothing this test touches: the point is that it does not have to.
        boundaries(),
    )
    .await;
    match &result {
        Err(e) if e.is_privacy_blocked() => {}
        other => panic!("an unpinnable expansion must fail closed: {other:?}"),
    }
    assert_eq!(calls, 1, "the turn carrying the expansion may not go out");
    assert_eq!(blocks.len(), 1, "exactly one privacy_block: {blocks:?}");
    assert_eq!(
        blocks[0].path,
        tetond::egress::provenance::UNKNOWN_PROVENANCE_PATH,
        "an unpinnable block is refused against the content-free sentinel, \
         never against a path it does not have"
    );
    for request in &captured {
        assert!(
            !contains_bytes(request, "USER-SKILL-BODY"),
            "the unpinnable expansion leaked past a configured boundary"
        );
    }
    std::fs::remove_dir_all(&repo).ok();
}

/// **AC-11(b).** A model invocation whose dynamic command **spawned** pins the
/// turn local under any boundary — and the fixture's command exits **3**.
///
/// The predicate is `DynamicOutcome::spawned`, not `did_run`, and the
/// difference is the whole test: an exit status is a value the command chose,
/// so a rule that only marked the `Ran` arm would let a body write
/// `!\`cat secrets/prod.env; exit 1\`` and have the output enter context
/// pinnable. REQ-585's verify closed that side channel; a test written to "ran"
/// exercises only the arm that was never the problem and passes on a build
/// where the predicate regressed.
///
/// The control is the project skill with no dynamic context, on the same
/// boundary: it reaches the wire, so what is being asserted below is "a command
/// spawned" and not "this repo has a boundary".
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_model_invocation_whose_command_failed_still_pins_the_turn() {
    let (repo, home) = model_skill_trees();

    let (result, _, calls, blocks) = drive_model_skill_call(
        &repo,
        Some(&home),
        &SessionId::from("model-skill-static"),
        "open",
        boundaries(),
    )
    .await;
    assert!(
        result.is_ok() && calls == 2 && blocks.is_empty(),
        "the control must reach the provider: {result:?}, {calls} call(s)"
    );

    let (result, captured, calls, blocks) = drive_model_skill_call(
        &repo,
        Some(&home),
        &SessionId::from("model-skill-failing"),
        "failing",
        boundaries(),
    )
    .await;
    match &result {
        Err(e) if e.is_privacy_blocked() => {}
        other => panic!(
            "a command that spawned and exited non-zero must still pin the \
             turn — `spawned`, not `did_run`: {other:?}"
        ),
    }
    assert_eq!(calls, 1, "not one packet may leave: {calls} call(s)");
    assert_eq!(blocks.len(), 1, "exactly one privacy_block: {blocks:?}");
    assert_eq!(
        blocks[0].path,
        tetond::egress::provenance::UNKNOWN_PROVENANCE_PATH,
        "command output has no identity for a glob to be compared against"
    );
    for request in &captured {
        assert!(
            !contains_bytes(request, "FAILING-BODY-MARKER"),
            "the pinned expansion reached the wire"
        );
    }
    std::fs::remove_dir_all(&repo).ok();
}

/// **AC-11(c).** With no boundary configured the choke point does not inspect,
/// so a model-invoked expansion reaches the provider — and the payload it
/// received **is the expansion**.
///
/// Not "a request went out": the captured body is searched for the frame the
/// tool wrote, the body the file holds, and the failed command's stdout inside
/// REQ-585's untrusted envelope. A turn that reached the wire carrying
/// something else satisfies a call count and fails this.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn with_no_boundary_a_model_invoked_expansion_reaches_the_provider() {
    let (repo, home) = model_skill_trees();
    let (result, captured, calls, blocks) = drive_model_skill_call(
        &repo,
        Some(&home),
        &SessionId::from("model-skill-open-wire"),
        "ran",
        // The whole difference from the test above.
        Vec::new(),
    )
    .await;

    assert!(result.is_ok(), "no boundary, no refusal: {result:?}");
    assert_eq!(calls, 2, "both calls went out");
    assert!(blocks.is_empty(), "nothing to block: {blocks:?}");

    let body = &captured[1];
    for fragment in [
        // BR-4's frame, which is what tells the model the block is instructions.
        "<skill-body skill=\\\"ran\\\"",
        // The file's own prose…
        "RAN-BODY-MARKER",
        // …the command's stdout, folded into the body…
        "RAN-OUTPUT-MARKER",
        // …inside the envelope every dynamic-context result gets.
        "trust=\\\"untrusted\\\"",
    ] {
        assert!(
            contains_bytes(body, fragment),
            "the payload is not the expansion — `{fragment}` is missing from \
             what the provider received:\n{}",
            String::from_utf8_lossy(body)
        );
    }
    std::fs::remove_dir_all(&repo).ok();
}

// ---------------------------------------------------------------------------
// REQ-587 verify — the naming duty is inside the boundary, not beside it
//
// Everything above drives the harness loop directly, because that is where a
// tool result's provenance is decided. The claim below is about a **duty** that
// the loop never sees: `title` is started by `DaemonRuntime::run_prompt_turn`,
// on its own task, and it sends its own request through its own route. So this
// section drives the daemon, and it asserts on the one instrument that can
// settle "nothing left the machine" — the title route's own transport.
//
// This is deliberately not a check on the `Provenance` value the call site
// computes. REQ-585 shipped this same defect once already and a call-site
// assertion is what it would have passed.
// ---------------------------------------------------------------------------

/// A mock OpenAI-compatible vendor on a real socket, counting what it served.
///
/// Real, rather than a `Transport` double, because a `DutyRoute`'s transport is
/// built by the daemon from a registered provider (`build_remote_transport`) and
/// there is no seam to inject one — and because only a socket can settle "no
/// packet left".
struct TitleVendor {
    endpoint: String,
    hits: Arc<AtomicUsize>,
    bodies: Arc<Mutex<Vec<String>>>,
}

impl TitleVendor {
    fn start(reply: String) -> Self {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind a mock vendor");
        let addr = listener.local_addr().expect("mock vendor address");
        let hits = Arc::new(AtomicUsize::new(0));
        let bodies = Arc::new(Mutex::new(Vec::new()));
        let served = Arc::clone(&hits);
        let captured = Arc::clone(&bodies);
        std::thread::spawn(move || {
            use std::io::{Read, Write};
            for stream in listener.incoming() {
                let Ok(mut stream) = stream else { break };
                let mut raw = Vec::new();
                // Read by the request's **framing**, never by a guess about
                // socket chunking (REQ-587 verify). A short read is legal at
                // any point in a stream, so the old `saw \r\n\r\n && read <
                // buf.len()` break truncated a body larger than the buffer on
                // Linux and not on macOS. That is worse here than anywhere
                // else in the suite: these legs assert what a boundary keeps
                // **off** the wire, and a truncated capture makes an absence
                // assertion pass for the wrong reason.
                let mut buf = [0u8; 65_536];
                let mut want: Option<usize> = None;
                while let Ok(read) = stream.read(&mut buf) {
                    if read == 0 {
                        break;
                    }
                    raw.extend_from_slice(&buf[..read]);
                    if want.is_none() {
                        if let Some(end) = raw.windows(4).position(|w| w == b"\r\n\r\n") {
                            let head = String::from_utf8_lossy(&raw[..end]).to_ascii_lowercase();
                            let len = head
                                .lines()
                                .find_map(|line| line.strip_prefix("content-length:"))
                                .and_then(|value| value.trim().parse::<usize>().ok())
                                .unwrap_or(0);
                            want = Some(end + 4 + len);
                        }
                    }
                    if want.is_some_and(|total| raw.len() >= total) {
                        break;
                    }
                }
                captured
                    .lock()
                    .unwrap()
                    .push(String::from_utf8_lossy(&raw).into_owned());
                // Counted **after** the body has been read, so a hit means a
                // request arrived whole rather than that a socket was opened.
                served.fetch_add(1, Ordering::SeqCst);
                let raw = format!(
                    "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\n\
                     Content-Length: {}\r\nConnection: close\r\n\r\n{reply}",
                    reply.len()
                );
                let _ = stream.write_all(raw.as_bytes());
                let _ = stream.flush();
            }
        });
        Self {
            endpoint: format!("http://{addr}/v1/chat/completions"),
            hits,
            bodies,
        }
    }

    fn hits(&self) -> usize {
        self.hits.load(Ordering::SeqCst)
    }

    fn bodies(&self) -> Vec<String> {
        self.bodies.lock().unwrap().clone()
    }
}

/// A distinctive fragment of the skill body, so a captured payload can be
/// recognized as *the expansion* rather than merely as a request.
const NOTES_BODY_MARKER: &str = "NOTES-SKILL-BODY-MARKER";

/// A repo whose one project skill sits under a `local-only` tree.
///
/// The skill is a **project** skill on purpose. It mints a real repo-relative id
/// (`.claude/skills/notes/SKILL.md`), so the boundary below has something exact
/// to match and the refusal is a boundary decision rather than the fail-closed
/// `unknown` arm — which a user skill would take under *any* boundary and which
/// would therefore prove less about where the value came from.
fn notes_skill_repo() -> PathBuf {
    let repo = temp_repo();
    let dir = repo.join(".claude/skills/notes");
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(
        dir.join("SKILL.md"),
        format!(
            "---\ndescription: the notes skill\n---\n\n\
             {NOTES_BODY_MARKER}: summarize the working notes for this repository \
             and say what is left to do.\n"
        ),
    )
    .unwrap();
    repo
}

/// The boundary that covers the skill file itself — a repository that keeps its
/// own `.claude/` tree on the machine.
fn claude_tree_is_local_only() -> Vec<PrivacyBoundaryConfig> {
    vec![PrivacyBoundaryConfig {
        path_glob: ".claude/**".to_owned(),
        mode: PrivacyMode::LocalOnly,
        origin: Default::default(),
    }]
}

/// Run one `/notes` turn on a daemon whose **`reflex`** tier — and therefore the
/// `title` duty — is bound to its own remote vendor, and report what that
/// vendor saw.
///
/// Two vendors, on two sockets, because one counter cannot tell a turn's request
/// from a duty's. The turn-serving tiers point at the first; `reflex` points at
/// the second, and in this fixture nothing else can reach it: `route` makes no
/// model call for a **structured** session (`dispatch_route`'s `Structured` arm
/// resolves from the phase), and `redact` — the third `reflex` category — is off
/// unless `[privacy] redact` is set, which `DaemonRuntime::minimal` leaves at its
/// default. So a hit on the second vendor is the naming duty and nothing else.
/// A client that acknowledges this repository's skills and answers nothing else.
///
/// REQ-589 ADR-10: a typed **project** skill is asked for once, before it
/// expands, and a turn with no addressable connection cannot be asked at all —
/// so without this the fixture below refuses before the naming duty it is about
/// ever starts.
struct Acknowledges(Arc<PendingPermissions>);

impl AddressedPermissionDelivery for Acknowledges {
    fn deliver(
        &self,
        connection: ConnectionId,
        _session_id: &SessionId,
        request: PermissionRequest,
    ) -> bool {
        self.0.resolve_from(
            &request.request_id,
            PermissionOutcome::Selected {
                option_id: "allow_always".to_owned(),
            },
            connection,
        )
    }
}

async fn drive_named_skill_turn(
    boundary_set: Vec<PrivacyBoundaryConfig>,
) -> (TitleVendor, Result<PromptTurnResult, RpcError>) {
    let repo = notes_skill_repo();
    let turns = TitleVendor::start(sse_turn("Understood.", None));
    let titles = TitleVendor::start(sse_turn("Working notes review", None));

    let runtime = Arc::new(DaemonRuntime::minimal());
    for (id, endpoint) in [("mock", &turns.endpoint), ("titler", &titles.endpoint)] {
        runtime
            .apply_config_update(ConfigUpdate::RegisterProvider(ProviderConfig {
                id: ProviderId::from(id),
                kind: ProtoProviderKind::OpenaiCompatible,
                endpoint: Some(endpoint.clone()),
                model: Some(format!("{id}-1")),
                auth_ref: None,
                max_context: Some(128_000),
                context_budget_cap: None,
                allow_cleartext: None,
                floored_budget: None,
            }))
            .expect("registering a provider");
    }
    for (tier, provider) in [
        (ProtoTier::Scan, "mock"),
        (ProtoTier::Build, "mock"),
        (ProtoTier::Think, "mock"),
        // The whole point of the fixture: a user who bound `reflex` remotely on
        // purpose gets what they asked for, and BR-10 still applies to it.
        (ProtoTier::Reflex, "titler"),
    ] {
        runtime
            .apply_config_update(ConfigUpdate::SetTierBinding(TierBindingConfig {
                tier,
                provider_id: ProviderId::from(provider),
                fallback_id: None,
            }))
            .expect("binding a tier");
    }
    for boundary in boundary_set {
        runtime
            .apply_config_update(ConfigUpdate::SetPrivacyBoundary(boundary))
            .expect("setting a boundary");
    }

    let acknowledges: Arc<dyn AddressedPermissionDelivery> =
        Arc::new(Acknowledges(Arc::clone(runtime.pending())));
    runtime.install_addressed_delivery(acknowledges);

    let sessions = SessionRegistry::new();
    let session_id = sessions
        .create(
            SessionMode::Structured,
            Some(ProtoPhase::Implement),
            Some(repo.clone()),
        )
        .expect("a structured session takes a phase")
        .session_id;
    // The registry exactly as `session/create` derives it, with **no** home:
    // this binary must not register whatever skills the developer's machine
    // happens to have (LESSON-540).
    let probed = runtime.session_root_for(Some(&repo));
    sessions.set_skills(
        &session_id,
        tetond::skills::discover(None, &probed.path, probed.view.kind, &RealFs),
    );

    let events = Arc::new(EventBus::new());
    let result = runtime
        .run_prompt_turn(
            &events,
            &sessions,
            session_id,
            SessionMode::Structured,
            Some(ProtoPhase::Implement),
            Some(repo.clone()),
            String::new(),
            Some(SkillInvocation {
                name: "notes".to_owned(),
                raw_arguments: String::new(),
            }),
            // The connection the acknowledgment is addressed to (REQ-585 ADR-7,
            // REQ-589 ADR-10). `None` here is a caller nobody can be asked, and
            // a project skill is not expanded for one.
            Some(GrantRegistry::new().next_connection_id()),
            ClientPresence::unwatched(),
        )
        .await;

    // The naming runs on a task the turn does not wait for (BR-3), so the count
    // is read after a bounded settle rather than immediately. The *positive*
    // leg polls until the hit lands and the negative leg waits the same ceiling
    // before reading zero, so "nothing was sent" and "nothing had time to be
    // sent" are not the same measurement.
    for _ in 0..100 {
        if titles.hits() > 0 {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    }

    std::fs::remove_dir_all(&repo).ok();
    (titles, result)
}

/// **REQ-587 verify, C2.** A skill turn does not hand the expansion to the
/// naming duty for delivery to a remote provider.
///
/// `run_prompt_turn` starts the `title` duty on the session's first substantive
/// prompt, and for a skill turn the text it is named after is
/// `SkillTurn::text` — the skill file's bytes. `title_route` resolves that duty
/// **remotely** unless the session is already tainted, which on the first prompt
/// it is not, and `harness::title` puts up to 2 KiB of its input in the request.
/// So the provenance handed to `Egress::send` is the only thing standing between
/// a `local-only` file and a provider — and `Egress::send` short-circuits on an
/// empty provenance *before* it looks at a boundary.
///
/// This is REQ-585's Critical, in the same function: that REQ moved the naming
/// later so a refused expansion would not spend it, and left the hard-coded
/// `Provenance::empty()` where it was.
///
/// **The instrument is the transport, not the call site.** A test that read the
/// `Provenance` value `spawn_title_session` was handed would have passed on
/// REQ-585's build too, for a duty that was sending the file anyway.
///
/// **Mutation:** restore `Provenance::empty()` at the skill call site and this
/// fails — the vendor is asked to serve a request carrying the body.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_skill_turns_expansion_never_reaches_the_naming_dutys_provider() {
    // The control leg first, and it is the load-bearing half: with no boundary
    // configured the naming duty genuinely fires, genuinely reaches this
    // vendor, and the request it sends genuinely carries the skill file's
    // prose. Without it, "zero requests" below would be satisfied by a fixture
    // that cannot send at all (LESSON-479).
    let (open, _) = drive_named_skill_turn(Vec::new()).await;
    assert_eq!(
        open.hits(),
        1,
        "with no boundary the naming duty must reach its provider, or the \
         refusal asserted below is about a fixture rather than about a guard"
    );
    let sent = open.bodies().join("\n");
    assert!(
        sent.contains(NOTES_BODY_MARKER),
        "non-vacuity: what the naming duty sends is the *expansion*, which is \
         why its provenance has to be the expansion's:\n{sent}"
    );

    // The same turn, the same skill, on a machine whose `.claude/` tree is
    // `local-only`.
    let (guarded, _) = drive_named_skill_turn(claude_tree_is_local_only()).await;
    assert_eq!(
        guarded.hits(),
        0,
        "the naming duty put the skill body on the wire: {:?}",
        guarded.bodies()
    );
}

// ===========================================================================
// REQ-612 BR-5 / AC-7 — a resident repository-notes block is a file on the
// wire, and the boundary matcher judges it as one
// ===========================================================================
//
// The bypass this section guards is stated in BR-5 in as many words: before
// this REQ the system prompt carried no file provenance at all, so a `TETON.md`
// under a `local-only` glob placed in the system string would have egressed to
// every remote provider on every turn of the session with no boundary verdict
// ever taken — a session-long leak rather than a single tool result's, and the
// one path around the charter's BR-1.
//
// Two legs, and the second is what makes the first mean anything.

/// A string that exists **only** inside the fixture's `TETON.md`.
///
/// LESSON-624: the marker is never a prompt, never a tool argument, never a
/// grep pattern and never a path — a marker any of those carried would reach a
/// request body through a daemon that leaked nothing, and the assertion below
/// would fire against a correct build.
const NOTES_ONLY_MARKER: &str = "quartzite-heliotrope-4417";

/// The notes the fixture repository describes itself with.
///
/// Ordinary prose: this file's subject is the boundary verdict, not the
/// sanitizer, so nothing here is hostile and nothing here is framed.
const NOTES_BODY: &str = "# fixture\n\nThe crates live under crates/. Build with cargo.\n\
                          The line below is this repository's own name for itself:\n\n\
                          quartzite-heliotrope-4417\n";

/// The prompt the notes turns run under.
///
/// Deliberately says nothing about the notes and names no file: the block is
/// resident because the *session* carries it, and a prompt that asked about it
/// would put the fixture's own words in the request body.
const NOTES_PROMPT: &str = "Summarize what you already know.";

/// A `local-only` set covering the notes file and nothing else.
fn notes_boundaries() -> Vec<PrivacyBoundary> {
    vec![PrivacyBoundary {
        path_glob: "**/TETON.md".to_owned(),
        mode: BoundaryMode::LocalOnly,
        origin: Default::default(),
    }]
}

/// A `project`-kind root whose `TETON.md` carries [`NOTES_ONLY_MARKER`].
///
/// The `Cargo.toml` is load-bearing: BR-1 reads a `project` root and nothing
/// else, so without a marker file both legs would be asserting the `absent`
/// path by accident.
fn notes_repo() -> PathBuf {
    static SEQ: AtomicUsize = AtomicUsize::new(0);
    let root = std::env::temp_dir().join(format!(
        "teton-notes-egress-{}-{}",
        std::process::id(),
        SEQ.fetch_add(1, Ordering::SeqCst)
    ));
    let _ = std::fs::remove_dir_all(&root);
    std::fs::create_dir_all(&root).unwrap();
    std::fs::write(root.join("Cargo.toml"), "[package]\nname = \"notes\"\n").unwrap();
    std::fs::write(root.join("TETON.md"), NOTES_BODY).unwrap();
    root
}

/// One session whose repository notes were loaded through the daemon's own
/// `session/create` derivation, and the `repo_context_state` line that load
/// published.
struct NotesSession {
    runtime: Arc<DaemonRuntime>,
    sessions: SessionRegistry,
    events: Arc<EventBus>,
    session_id: SessionId,
    announced: Vec<teton_protocol::events::RepoContextState>,
}

impl NotesSession {
    fn state(&self) -> Arc<RepoContextState> {
        self.sessions.repo_context(&self.session_id)
    }

    /// This session's `/context` answer, through the daemon's own method.
    ///
    /// `async` since REQ-613 gave the method a fourth action that runs the
    /// generation pipeline; `status` runs none of it and awaits nothing.
    async fn status(&self) -> teton_protocol::methods::SessionContextResult {
        self.runtime
            .session_context(
                &SessionContextParams {
                    session_id: self.session_id.clone(),
                    action: ContextAction::Status,
                },
                &self.sessions,
                &self.events,
                // No stamped route: this fixture asserts on egress rather than
                // on the cap, and a session no turn has routed reports the
                // ceiling.
                None,
                // No connection: nothing on this path raises a prompt.
                None,
            )
            .await
            .expect("`status` takes no turn claim, so it is never refused")
    }
}

/// Create a session at `repo` with `boundary` configured, loading its notes
/// through [`DaemonRuntime::store_session_repo_context`] — the same function
/// `session/create` calls, so this fixture cannot drift into an agreeing
/// re-implementation of the load path (LESSON-451).
async fn notes_session(repo: &std::path::Path, boundary: Option<&str>) -> NotesSession {
    let events = Arc::new(EventBus::new());
    // The shipped set is off so the only glob in play is this fixture's own: a
    // second row that happened to match would make the `withheld` leg pass for
    // a reason the test did not choose.
    let runtime = Arc::new(DaemonRuntime::minimal().with_default_boundaries_disabled());
    if let Some(path_glob) = boundary {
        runtime
            .apply_config_update(ConfigUpdate::SetPrivacyBoundary(PrivacyBoundaryConfig {
                path_glob: path_glob.to_owned(),
                mode: PrivacyMode::LocalOnly,
                origin: BoundaryOriginConfig::User,
            }))
            .expect("a boundary is a config update");
    }
    let sessions = SessionRegistry::new();
    let session_id = sessions
        .create(SessionMode::Freeform, None, Some(repo.to_path_buf()))
        .expect("a freeform session needs no phase")
        .session_id;

    let mut sub = events.subscribe(64);
    let probed = runtime.session_root_for(Some(repo));
    runtime.store_session_repo_context(&sessions, &session_id, &probed, &events);

    let mut announced = Vec::new();
    while let Ok(Some(envelope)) =
        tokio::time::timeout(std::time::Duration::from_millis(50), sub.recv()).await
    {
        if let Event::RepoContextState(state) = envelope.event {
            announced.push(state);
        }
    }

    NotesSession {
        runtime,
        sessions,
        events,
        session_id,
        announced,
    }
}

/// `scripted_config()` with `block` as this route's resident notes, at the cap
/// the route derived.
///
/// The one line production spells at `runtime::turn` — `state.file().map(|file|
/// RepoContextBlock::render(file, cap))` — asked of the *real* renderer over the
/// *real* loaded file, never a `RepoContextBlock { text: … }` literal
/// (LESSON-544): what AC-7 is about is the bytes the producer makes.
fn notes_config(state: &RepoContextState) -> HarnessConfig {
    let base = scripted_config();
    let cap = base.budget.repo_context_cap;
    HarnessConfig {
        repo_context: state.file().map(|file| RepoContextBlock::render(file, cap)),
        ..base
    }
}

/// Drive one remote turn over `ctx` and return the loop result, every request
/// body the transport was asked to send, and the ordered event names the turn
/// published.
///
/// No tool call is scripted: the claim is about what the *system prompt* puts
/// on the wire, and a turn that read the file would put its bytes there
/// legitimately.
async fn drive_notes_turn(
    repo: &std::path::Path,
    session_id: &SessionId,
    config: &HarnessConfig,
    ctx: &mut ContextManager,
    boundaries: Vec<PrivacyBoundary>,
) -> (
    Result<tetond::harness::TurnOutcome, HarnessError>,
    Vec<Vec<u8>>,
    Vec<String>,
) {
    let transport = CaptureSse::with_bodies(vec![sse_turn("Understood.", None)]);
    let capture = transport.clone();

    let bus = Arc::new(EventBus::new());
    let egress = Egress::new(transport, boundaries, bus.clone());
    let provider = OpenAiCompatAdapter::new(OpenAiCompatConfig::new(
        "deepseek",
        "https://api.deepseek.com/v1/chat/completions",
    ));
    let mut source = RemoteProviderSource::new(
        &provider,
        &egress,
        "deepseek",
        "deepseek-chat",
        session_id.clone(),
        ResolvedEffort::effort(EffortLevel::High),
    );

    let tools = ToolRegistry::with_builtins();
    let tool_ctx = ToolContext::new(repo);
    let gate = PermissionGate::new(
        session_id.clone(),
        PermissionConfig::permissive(),
        Arc::clone(&bus),
        Arc::new(PendingPermissions::new()),
    );
    let events = SessionEvents::new(Arc::clone(&bus), session_id.clone());
    let mut hook = NoopProvenanceHook;
    let mut sub = bus.subscribe(256);

    let result = run_session_turn_with_source(
        &mut source,
        &tools,
        &tool_ctx,
        &gate,
        &events,
        ctx,
        config,
        &mut hook,
        &DutyRoute::unresolved("no digest route in this test"),
        &DutyRoute::unresolved("no compact route in this test"),
        &ToolDuties {
            triage: &DutyRoute::unresolved("no triage route in this test"),
            shell: &DutyRoute::unresolved("no shell route in this test"),
        },
    )
    .await;

    let mut names = Vec::new();
    while let Ok(Some(envelope)) =
        tokio::time::timeout(std::time::Duration::from_millis(50), sub.recv()).await
    {
        names.push(envelope.event.name().to_owned());
    }
    (result, capture.captured(), names)
}

/// **REQ-612 BR-5 / AC-7, both legs.** A `TETON.md` covered by a `local-only`
/// glob is never made resident and **no byte of it reaches any remote request
/// body**; the same file with no glob over it is resident, its bytes do reach
/// the wire, and its identity is in the turn's egress provenance union — so the
/// boundary matcher has something to judge on every later turn.
///
/// ## Why the two legs are one test
///
/// The claim is a comparison, not an absence. "No request carried the marker"
/// is satisfied by a fixture that cannot put a system prompt on a wire at all,
/// by a loader that read no file, and by a repository with no notes in it — so
/// leg 2 asserts the marker **does** reach a captured body under the identical
/// fixture with the glob removed. Only the pair says the boundary did the work
/// (LESSON-479).
///
/// ## The marker
///
/// [`NOTES_ONLY_MARKER`] lives in the file's bytes and nowhere else — not in
/// [`NOTES_PROMPT`], not in a tool argument, not in the fixture's paths. A
/// marker any of those carried would ride to the vendor through a daemon that
/// leaked nothing, and this test would fail against a correct build (LESSON-624).
///
/// ## The instruments, and why each is the daemon's own
///
/// - the state is read back through `session/context` **and** off the published
///   `repo_context_state` line, so BR-5's "one line says so" is asserted rather
///   than assumed;
/// - the block is produced by the real loader and the real renderer over a real
///   file, never by a `RepoContextBlock { text: … }` literal (LESSON-544);
/// - the union is read by the production `context_provenance` off the manager a
///   real turn ran against, seeded through `CarriedTurn::begin` — the one
///   seeding path the daemon has.
///
/// ## Mutation (run 2026-09-03)
///
/// | change | result |
/// |---|---|
/// | `context_provenance` skips the `system_sources` union | leg 2's provenance assertion fails |
/// | `RepoContext::load`'s boundary gate deleted | leg 1's state, its line, and the marker assertion all fail |
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_boundary_covered_notes_file_never_leaves_and_an_uncovered_one_is_in_the_union() {
    let repo = notes_repo();

    // --- Leg 1: the glob covers the file --------------------------------
    let covered = notes_session(&repo, Some("**/TETON.md")).await;
    assert_eq!(
        covered.state().kind(),
        RepoContextStateKind::WithheldBoundary,
        "a covered file must not be made resident: {:?}",
        covered.state()
    );

    // BR-5's "one line says so", on both surfaces that carry it.
    assert_eq!(
        covered
            .announced
            .iter()
            .map(|line| line.state)
            .collect::<Vec<_>>(),
        vec![RepoContextStateKind::WithheldBoundary],
        "the withholding was silent: {:?}",
        covered.announced
    );
    assert_eq!(
        covered.announced[0].source,
        Some(RepoContextSource::TetonMd)
    );
    assert_eq!(
        covered.announced[0].resident_bytes, 0,
        "a withheld file is resident in no bytes at all"
    );
    let status = covered.status().await;
    assert_eq!(status.state, RepoContextStateKind::WithheldBoundary);
    assert_eq!(status.file.as_deref(), Some("TETON.md"));
    assert_eq!(status.resident_bytes, 0);

    let config = notes_config(&covered.state());
    assert!(
        config.repo_context.is_none(),
        "a withheld state renders no block"
    );
    let system = build_system_prompt(&ToolRegistry::with_builtins(), &config);
    assert!(
        !system.contains(NOTES_ONLY_MARKER),
        "the covered file reached the system prompt:\n{system}"
    );

    let mut turn = CarriedTurn::begin(
        &covered.sessions,
        &covered.session_id,
        system,
        &config,
        Arc::new(SessionTaint::new()),
        notes_boundaries(),
        NOTES_PROMPT,
        std::collections::BTreeSet::new(),
        false,
        // No notes in this fixture, so a reroute has nothing to re-render.
        None,
    );
    let (result, captured, names) = drive_notes_turn(
        &repo,
        &covered.session_id,
        &config,
        turn.ctx_mut(),
        notes_boundaries(),
    )
    .await;
    assert!(
        result.is_ok(),
        "the covered leg's turn must run — a refused turn sends nothing and \
         proves nothing: {result:?}"
    );
    assert!(
        !captured.is_empty(),
        "no request reached the transport at all, so the assertion below is \
         about a fixture rather than about a boundary"
    );
    // The conventions' rule for an egress assertion: name the requests, beside
    // the ordered event names, before anybody goes looking at the choke point.
    let carrying: Vec<usize> = captured
        .iter()
        .enumerate()
        .filter(|(_, body)| contains_bytes(body, NOTES_ONLY_MARKER))
        .map(|(index, _)| index)
        .collect();
    assert!(
        carrying.is_empty(),
        "the covered file's bytes left the machine: requests {carrying:?} of \
         {} carried the marker; the turn published {names:?}",
        captured.len()
    );
    assert!(
        !context_provenance(turn.ctx()).contains("TETON.md"),
        "a file that was never read must contribute no identity: {:?}",
        context_provenance(turn.ctx()).sources().collect::<Vec<_>>()
    );

    // --- Leg 2: the same file, no glob over it ---------------------------
    let open = notes_session(&repo, None).await;
    let state = open.state();
    assert_eq!(
        state.kind(),
        RepoContextStateKind::Loaded,
        "with no boundary the same file loads: {state:?}"
    );
    let identity = state
        .file()
        .expect("a loaded state carries its file")
        .provenance
        .clone();

    let config = notes_config(&state);
    let block = config
        .repo_context
        .as_ref()
        .expect("a loaded state renders a block");
    assert!(
        block.text.contains(NOTES_ONLY_MARKER),
        "the block the route stamped does not carry the file's own bytes:\n{}",
        block.text
    );
    let system = build_system_prompt(&ToolRegistry::with_builtins(), &config);

    let mut turn = CarriedTurn::begin(
        &open.sessions,
        &open.session_id,
        system,
        &config,
        Arc::new(SessionTaint::new()),
        Vec::new(),
        NOTES_PROMPT,
        std::collections::BTreeSet::new(),
        false,
        // No notes in this fixture, so a reroute has nothing to re-render.
        None,
    );
    let (result, captured, names) =
        drive_notes_turn(&repo, &open.session_id, &config, turn.ctx_mut(), Vec::new()).await;
    assert!(result.is_ok(), "the uncovered turn failed: {result:?}");
    assert!(
        captured
            .iter()
            .any(|body| contains_bytes(body, NOTES_ONLY_MARKER)),
        "non-vacuity: with no boundary the resident notes must genuinely reach \
         the vendor, or leg 1's silence is about a fixture that cannot send \
         them; the turn published {names:?}"
    );

    // The union: the identity the *head* carries, read by the production
    // function off the manager the turn actually ran against.
    let provenance = context_provenance(turn.ctx());
    assert!(
        provenance.contains(identity.as_str()),
        "the resident block egressed with no identity for the boundary matcher \
         to judge — BR-5's bypass, re-opened: {:?}",
        provenance.sources().collect::<Vec<_>>()
    );
    assert!(
        !provenance.is_unknown(),
        "the notes are attributable bytes, not a `shell` result"
    );

    std::fs::remove_dir_all(&repo).ok();
}
