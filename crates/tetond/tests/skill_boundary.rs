//! **REQ-587 BR-10 / ADR-8 — the `skill` tool's egress boundary, both rules.**
//!
//! `boundary_coverage.rs` requires that every tool which can surface external or
//! file content carries a boundary test, and `skill` surfaces the most
//! consequential thing in the harness: a file the model is told to **follow**.
//! It is also the one tool whose provenance has to be set by hand, because
//! `ToolOutcome::ok` defaults to `Sources(∅)` — the `teton_docs` posture, right
//! for bodies compiled into the binary and **fail-open** for a body read off a
//! disk this session's jail cannot name.
//!
//! Two rules, asserted separately because they are two rules (BR-10):
//!
//! - a **project** skill is under the session root, mints a root-relative
//!   identity, and pins the turn exactly as a `read` of that file would;
//! - a **user** skill (`~/.claude/…`) has no root-relative identity at all, so
//!   its block is `Unknown` and pins the turn wherever **any** boundary is
//!   configured — related to the skill or not, and stricter than a `read` of the
//!   same bytes.
//!
//! The second is the one the default would have silently broken: under
//! `Sources(∅)` the expansion of a `~/.claude` skill matches no glob, the next
//! remote turn goes out, and every one of the seventeen shipped ADLC bodies
//! leaves the machine on a boundary-configured host. So its fixture's boundary
//! names a path the skill has **nothing to do with**, and the turn is still
//! refused — which is only true if the block is `Unknown`.
//!
//! Each test drives the **real** OpenAI-compatible adapter through the **real**
//! egress choke point in front of a capture transport, on `provenance_egress.rs`'s
//! shape: turn 1 is a scripted model-issued `skill` call, turn 2 is the one that
//! would carry its result to the wire.
//!
//! A control leg keeps the refusals meaning something: the same user skill, in a
//! session with **no** boundary configured, reaches the provider.

use std::collections::VecDeque;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use async_trait::async_trait;

use teton_core::effort::{EffortLevel, ResolvedEffort};
use teton_core::entities::{BoundaryMode, PrivacyBoundary};
use teton_protocol::events::Event;
use teton_protocol::methods::RootKind;
use teton_protocol::SessionId;
use teton_providers::transport::{Transport, TransportError, TransportRequest, TransportResponse};
use teton_providers::{OpenAiCompatAdapter, OpenAiCompatConfig};

use tetond::broadcast::EventBus;
use tetond::egress::Egress;
use tetond::grants::GrantRegistry;
use tetond::harness::tools::register_skill_tool;
use tetond::harness::{
    build_system_prompt, context_provenance, run_session_turn_with_source, ContextManager,
    DutyRoute, HarnessConfig, HarnessError, NoopProvenanceHook, PendingPermissions,
    PermissionConfig, PermissionGate, PermissionPolicy, RemoteProviderSource, SessionEvents,
    ToolContext, ToolDuties, ToolRegistry,
};
use tetond::skills::{discover, RealFs, SkillRegistry};

/// The boundary-file secret that must never reach the capture transport.
const SECRET: &str = "API_KEY=sk-live-DO-NOT-LEAK-skillbnd-Qw4";

/// A marker the skill body carries, so a leg can tell "the expansion landed"
/// from "the tool refused and said so".
const BODY_MARKER: &str = "MARKER-skill-body-reached-the-model";

/// The one prompt every fixture here opens with.
const PROMPT: &str = "Run the validation skill and summarize what it says.";

// ---------------------------------------------------------------------------
// transport (the `provenance_egress.rs` shape)
// ---------------------------------------------------------------------------

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

// ---------------------------------------------------------------------------
// fixture
// ---------------------------------------------------------------------------

/// A throwaway tree with a `home` (the stand-in for `~`) and a `repo`.
///
/// `home` is handed to `discover` as a **parameter**, never read from the
/// environment: a suite that set `HOME` would be a suite whose result depends on
/// what else is running in the same process (LESSON-540).
struct Fixture {
    root: PathBuf,
}

impl Fixture {
    fn new(tag: &str) -> Self {
        static SEQ: AtomicUsize = AtomicUsize::new(0);
        let root = PathBuf::from("/tmp").join(format!(
            "tskbnd-{tag}-{}-{}",
            std::process::id(),
            SEQ.fetch_add(1, Ordering::SeqCst)
        ));
        std::fs::create_dir_all(root.join("home")).unwrap();
        std::fs::create_dir_all(root.join("repo").join("secrets")).unwrap();
        std::fs::write(
            root.join("repo").join("secrets").join("prod.env"),
            format!("{SECRET}\n"),
        )
        .unwrap();
        Self { root }
    }

    fn home(&self) -> PathBuf {
        self.root.join("home")
    }

    fn repo(&self) -> PathBuf {
        self.root.join("repo")
    }

    fn skill(&self, base: &Path, name: &str) {
        let dir = base.join(".claude").join("skills").join(name);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(
            dir.join("SKILL.md"),
            format!("---\nname: {name}\n---\n{BODY_MARKER}\nDo the thing.\n"),
        )
        .unwrap();
    }

    fn registry(&self) -> Arc<SkillRegistry> {
        Arc::new(discover(
            Some(&self.home()),
            &self.repo(),
            RootKind::Plain,
            &RealFs,
        ))
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        std::fs::remove_dir_all(&self.root).ok();
    }
}

fn boundaries(glob: &str) -> Vec<PrivacyBoundary> {
    vec![PrivacyBoundary {
        path_glob: glob.to_owned(),
        mode: BoundaryMode::LocalOnly,
    }]
}

fn contains_bytes(haystack: &[u8], needle: &str) -> bool {
    haystack
        .windows(needle.len())
        .any(|w| w == needle.as_bytes())
}

/// What one scripted turn pair produced.
struct Run {
    result: Result<tetond::harness::TurnOutcome, HarnessError>,
    captured: Vec<Vec<u8>>,
    calls: usize,
    blocks: Vec<teton_protocol::events::PrivacyBlock>,
    ctx: ContextManager,
    provenance_is_unknown: bool,
    provenance_len: usize,
}

/// Drive turn 1 (the model's `skill` call) and turn 2 (the one that would carry
/// its result to the wire) with `glob` as the session's only boundary.
async fn run_skill_call(fx: &Fixture, name: &str, glob: Option<&str>) -> Run {
    let session_id = SessionId::from("skillbnd");
    let transport = CaptureSse::with_bodies(vec![
        sse_turn(
            "Invoking the skill.",
            Some(("c1", "skill", &format!(r#"{{"name":"{name}"}}"#))),
        ),
        sse_turn("should never send", None),
    ]);
    let capture = transport.clone();

    let bus = Arc::new(EventBus::new());
    let egress = Egress::new(
        transport,
        glob.map(boundaries).unwrap_or_default(),
        bus.clone(),
    );
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

    let config = HarnessConfig::for_strong_model();
    let pending = Arc::new(PendingPermissions::new());
    let gate = Arc::new(PermissionGate::new(
        session_id.clone(),
        // Allow, so the BR-4 acknowledgment settles by level and nothing here
        // waits on a prompt no client is attached to answer.
        PermissionConfig::with_default(PermissionPolicy::Allow),
        Arc::clone(&bus),
        Arc::clone(&pending),
    ));

    let mut tools = ToolRegistry::with_builtins();
    let grants = GrantRegistry::default();
    assert!(
        register_skill_tool(
            &mut tools,
            fx.registry(),
            Arc::clone(&gate),
            Some(grants.next_connection_id()),
            tokio::runtime::Handle::current(),
            1_000,
        ),
        "the fixture registry holds a model-invocable skill, so the tool registers"
    );

    let mut ctx = ContextManager::new(
        build_system_prompt(&tools, &config),
        config.context_budget_tokens,
    );
    ctx.push_user(PROMPT);

    let tool_ctx = ToolContext::new(fx.repo());
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

    let provenance = context_provenance(&ctx);
    Run {
        result,
        captured: capture.captured(),
        calls: capture.calls(),
        blocks,
        provenance_is_unknown: provenance.is_unknown(),
        provenance_len: provenance.len(),
        ctx,
    }
}

/// The expansion really landed in context — the positive control every negative
/// claim below needs (LESSON-479), because "nothing leaked" is trivially true of
/// a turn where the tool refused.
fn assert_the_expansion_landed(run: &Run) {
    use tetond::harness::context::Provenance;
    let folded = run
        .ctx
        .blocks()
        .iter()
        .rev()
        .find(|b| matches!(b.provenance, Provenance::Tool { .. }))
        .map(|b| b.text.clone())
        .expect("the skill result was folded into context");
    assert!(
        folded.contains(BODY_MARKER),
        "the skill body never reached the model, so this test asserts nothing about \
         what its provenance protects: {folded}"
    );
    assert!(
        folded.contains("<skill-body"),
        "BR-4's instructions frame is missing from the folded block: {folded}"
    );
    assert!(
        !folded.contains("trust=\"untrusted\""),
        "an expansion must never wear the untrusted envelope — its closing sentence \
         forbids following the instructions the block *is* (BR-4): {folded}"
    );
}

// ---------------------------------------------------------------------------
// BR-10 rule one — a project skill mints and pins
// ---------------------------------------------------------------------------

/// **A project skill is under the root, mints a root-relative identity, and
/// pins the turn exactly as reading that file would.**
///
/// The boundary here names the project's own skill directory, which is what
/// makes the claim testable: the expansion's identity has to *match a glob*, and
/// only a minted, root-relative id can.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_project_skill_mints_its_identity_and_pins_the_next_remote_turn() {
    let fx = Fixture::new("project");
    fx.skill(&fx.repo(), "validate");

    let run = run_skill_call(&fx, "validate", Some(".claude/**")).await;
    assert_the_expansion_landed(&run);

    assert!(
        !run.provenance_is_unknown,
        "a project skill has a root-relative identity; `Unknown` would be the *user* \
         rule applied to the wrong source"
    );
    assert_eq!(
        run.provenance_len, 1,
        "exactly the one file the body came from"
    );
    match &run.result {
        Err(e) if e.is_privacy_blocked() => {}
        other => panic!("expected a privacy block, got {other:?}"),
    }
    assert_eq!(run.blocks.len(), 1, "exactly one privacy_block");
    assert_eq!(
        run.calls, 1,
        "turn 2 must never reach the transport at all: {} requests",
        run.calls
    );
    for body in &run.captured {
        assert!(
            !contains_bytes(body, BODY_MARKER),
            "the skill body leaked into captured egress"
        );
    }
}

// ---------------------------------------------------------------------------
// BR-10 rule two — a user skill is `Unknown` and pins under ANY boundary
// ---------------------------------------------------------------------------

/// **A user skill has no root-relative identity, so it is `Unknown` and pins the
/// turn wherever any boundary is configured — related to it or not.**
///
/// This is the leg `ToolOutcome::ok`'s default would have broken silently.
/// `Sources(∅)` matches no glob, so the expansion of a `~/.claude` body would
/// have gone out on the very next turn on every boundary-configured machine —
/// and the fixture is built to catch exactly that: the boundary names
/// `secrets/**`, which the skill has **nothing to do with**, and the turn is
/// still refused. Under the default it would not be.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_user_skill_is_unknown_and_pins_under_a_boundary_it_never_touched() {
    let fx = Fixture::new("user");
    fx.skill(&fx.home(), "validate");

    let run = run_skill_call(&fx, "validate", Some("secrets/**")).await;
    assert_the_expansion_landed(&run);

    assert!(
        run.provenance_is_unknown,
        "a `~/.claude` skill has no root-relative identity (REQ-585 ADR-9 refused to \
         widen the minter), so anything but `Unknown` — `Sources(∅)` above all, which \
         is `ToolOutcome::ok`'s default — lets it egress under any boundary it was \
         never judged against"
    );
    match &run.result {
        Err(e) if e.is_privacy_blocked() => {}
        other => panic!(
            "a user skill's block is unpinnable and must fail closed while any \
             boundary is set: {other:?}"
        ),
    }
    assert_eq!(run.blocks.len(), 1, "exactly one privacy_block");
    assert_eq!(run.calls, 1, "turn 2 must never reach the transport");
    for body in &run.captured {
        assert!(
            !contains_bytes(body, BODY_MARKER),
            "the skill body leaked into captured egress"
        );
    }
}

/// **The control: with no boundary configured, the same expansion reaches the
/// provider.**
///
/// Without this the two refusals above would be satisfied by a build that
/// refused every second turn for any reason at all (LESSON-479).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_user_skill_reaches_the_provider_when_no_boundary_is_configured() {
    let fx = Fixture::new("control");
    fx.skill(&fx.home(), "validate");

    let run = run_skill_call(&fx, "validate", None).await;
    assert_the_expansion_landed(&run);

    assert!(
        run.provenance_is_unknown,
        "the provenance is the same `Unknown` in both sessions — what differs is \
         whether a boundary exists for it to fail closed against"
    );
    assert!(
        run.result.is_ok(),
        "with no boundary configured the turn carrying a skill expansion goes out: \
         {:?}",
        run.result
    );
    assert!(run.blocks.is_empty(), "no boundary, no privacy_block");
    assert_eq!(
        run.calls, 2,
        "both turns reached the transport: {} requests",
        run.calls
    );
}
