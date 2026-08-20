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
use teton_protocol::events::{Event, WebLookupOutcome};
use teton_protocol::{SessionId, SessionMode};
use teton_providers::transport::{Transport, TransportError, TransportRequest, TransportResponse};
use teton_providers::{OpenAiCompatAdapter, OpenAiCompatConfig};

use tetond::broadcast::EventBus;
use tetond::carry::CarriedTurn;
use tetond::egress::{Authorship, Egress, LookupContext, LookupRequest, NoopSink, TaintView};
use tetond::harness::{
    build_system_prompt, context_provenance, run_session_turn_with_source, ContextManager,
    DutyRoute, HarnessConfig, HarnessError, NoopProvenanceHook, PendingPermissions,
    PermissionConfig, PermissionGate, RemoteProviderSource, SessionEvents, ToolContext, ToolDuties,
    ToolRegistry,
};
use tetond::runtime::{SessionTaint, WebTaintOverride};
use tetond::sessions::SessionRegistry;

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
