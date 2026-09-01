//! REQ-607 AC-4 and AC-5 — the advisory travels in-band, and names nothing it
//! read from the live environment.
//!
//! ## Why these two claims need a whole turn
//!
//! Everything else in this REQ is provable at unit level, on the composed map or
//! on `ToolOutcome::content`. These two are not:
//!
//! - **AC-4 (BR-3)** is a claim about what the session *published*. It is only
//!   answerable from outside the tool, by subscribing to the bus a real turn
//!   emits on and draining it. Calling `ShellTool::run` in a loop would assert
//!   the tool's own bookkeeping and nothing about the event stream.
//! - **AC-5 (BR-4)** needs a configured `auth_ref = "env:<NAME>"` credential to
//!   reach the spawn path through the live policy provider, which only exists
//!   once a daemon-shaped turn is running.
//!
//! ## The structural fact that makes AC-4 satisfiable, stated so a change to it
//! ## is noticed here
//!
//! `SessionUpdatePayload::ToolCall` carries a `tool_call_id`, a `title` and a
//! `status`; `ToolCallUpdate` carries an id and a status. **No event carries
//! tool result content.** That is why an advisory on the result can be
//! guaranteed absent from the stream at all. If a payload ever grows a `content`
//! or `output` field, this test is the thing that should go red.
//!
//! ## What the assertion is over
//!
//! Each drained envelope is **serialized to JSON** and searched. AC-4 rules out
//! matching on a type name — `Event::ShellEnvWithheld` or similar — because a
//! rename would silently defeat it while leaving the disclosure in place. What
//! is forbidden is the *content*: the variable name, and any count-shaped
//! description of the withheld set.
//!
//! ## Mutations run
//!
//! Both were applied to the production path, run, and reverted:
//!
//! | Mutation | Result |
//! |---|---|
//! | `withheld_advisory` returns `None` unconditionally — the advisory is silenced | **both** tests red, on their non-vacuity contrasts ("the tool result carries no advisory", "the advisory named nothing"). So a vanished advisory cannot leave these passing. |
//! | `turn_loop`'s `tool_started` title gains `[1 withheld: SSH_AUTH_SOCK]` — the disclosure BR-3 forbids, in the smallest form that could actually happen | AC-4 red, naming the offending envelope. AC-5 stays green, correctly: it is a claim about the tool result, not the bus. |
//!
//! ## Why this is its own binary
//!
//! Integration test binaries share no modules in this workspace, so the scripted
//! vendor below is a copy of `remote_loop.rs`'s — a smaller one, with only the
//! verbs these two claims need. It also owns process-global environment state
//! (`SSH_AUTH_SOCK`, and the `child_env` policy provider), which is the second
//! reason not to fold it into an existing binary.

use std::collections::VecDeque;
use std::path::PathBuf;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;

use teton_core::effort::{EffortLevel, ResolvedEffort};
use teton_protocol::methods::StopReason;
use teton_protocol::SessionId;
use teton_providers::transport::{Transport, TransportError, TransportRequest, TransportResponse};
use teton_providers::{OpenAiCompatAdapter, OpenAiCompatConfig};

use tetond::broadcast::EventBus;
use tetond::cost::{CostLedger, NoopCostSink, PriceTable};
use tetond::egress::Egress;
use tetond::harness::{
    build_system_prompt, run_session_turn_with_source, ContextManager, DutyRoute, HarnessConfig,
    NoopProvenanceHook, PendingPermissions, PermissionConfig, PermissionGate, RemoteProviderSource,
    SessionEvents, ToolContext, ToolDuties, ToolRegistry,
};

/// Serializes the two tests below.
///
/// Both mutate this process's environment and then spawn children that read it,
/// and `cargo test` runs them on separate threads by default. Without this they
/// would be racing over `SSH_AUTH_SOCK` and the credential sentinel — the class
/// of flake that passes locally and fails once on a loaded CI runner
/// (conventions.md: a fixture must not depend on scheduling).
///
/// `tokio::sync::Mutex`, not `std::sync::Mutex`: the guard is held across the
/// `.await` on a whole prompt turn, and a std guard held across an await point
/// is a blocking hazard clippy refuses (`await_holding_lock`) — correctly, since
/// it parks a runtime worker for the length of a spawn.
static ENV_MUTATION: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

/// An obviously synthetic agent socket (AC-11, LESSON-497). Nothing will ever
/// connect to it; it exists so the daemon *has* the variable, which is half of
/// what makes it "withheld".
fn agent_sentinel() -> String {
    format!("/tmp/SENTINEL-agent-{}.sock", std::process::id())
}

/// The credential variable AC-5 is about. Named from the REQ verbatim.
const CRED_SENTINEL: &str = "MY_LLM_CRED_SENTINEL";

// --------------------------------------------------------------------------
// A scripted OpenAI-compatible transport (a trimmed copy of remote_loop.rs's).
// --------------------------------------------------------------------------

#[derive(Clone, Default)]
struct ScriptedSseTransport {
    bodies: Arc<Mutex<VecDeque<String>>>,
}

impl ScriptedSseTransport {
    fn with_bodies(bodies: Vec<String>) -> Self {
        Self {
            bodies: Arc::new(Mutex::new(bodies.into_iter().collect())),
        }
    }
}

#[async_trait]
impl Transport for ScriptedSseTransport {
    async fn execute(&self, _request: TransportRequest) -> Result<TransportResponse, TransportError> {
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

/// One streaming turn: text deltas, an optional tool call, then usage + `[DONE]`.
fn sse_turn(text: &str, tool: Option<(&str, &str, &str)>) -> String {
    let mut s = String::new();
    let chunk = serde_json::json!({ "choices": [{ "delta": { "content": text } }] });
    s.push_str(&format!("data: {chunk}\n\n"));
    if let Some((id, name, args)) = tool {
        let chunk = serde_json::json!({
            "choices": [{
                "delta": { "tool_calls": [{
                    "index": 0,
                    "id": id,
                    "function": { "name": name, "arguments": args }
                }]}
            }]
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

// --------------------------------------------------------------------------
// Fixtures
// --------------------------------------------------------------------------

/// A git repository with no remote, so `git push` fails locally and promptly.
/// No network, no ssh server: the advisory's exit-code predicate only needs a
/// non-zero status.
fn temp_repo() -> PathBuf {
    static SEQ: AtomicUsize = AtomicUsize::new(0);
    let root = std::env::temp_dir().join(format!(
        "teton-advisory-{}-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos(),
        SEQ.fetch_add(1, Ordering::SeqCst)
    ));
    std::fs::create_dir_all(&root).unwrap();
    let _ = std::process::Command::new("git")
        .args(["init", "-q"])
        .current_dir(&root)
        .output();
    root
}

fn ledger() -> Arc<CostLedger> {
    Arc::new(
        CostLedger::open_in_memory(PriceTable::bundled(), Arc::new(NoopCostSink))
            .expect("open in-memory ledger"),
    )
}

/// Drain every event currently buffered on `sub`.
async fn collect_events(
    sub: &mut tetond::broadcast::Subscription,
) -> Vec<teton_protocol::events::EventEnvelope> {
    let mut out = Vec::new();
    while let Ok(Some(env)) = tokio::time::timeout(Duration::from_millis(50), sub.recv()).await {
        out.push(env);
    }
    out
}

/// Drive one prompt turn whose scripted model issues a single failing `shell`
/// call, and hand back everything the turn published plus the rendered result.
///
/// Returns `(events, transcript)` where `transcript` is the assembled context
/// after the turn — the tool result the model was actually shown.
async fn run_failing_shell_turn(session: &str) -> (Vec<teton_protocol::events::EventEnvelope>, String) {
    let repo = temp_repo();

    let bodies = vec![
        sse_turn(
            "Pushing.",
            Some(("call_1", "shell", r#"{"command":"git push origin main"}"#)),
        ),
        sse_turn("That did not work.", None),
    ];

    let transport = ScriptedSseTransport::with_bodies(bodies);
    let cost = ledger();
    let egress = Egress::new(transport, Vec::new(), Arc::new(tetond::egress::NoopSink))
        .with_cost_meter(cost);

    let provider = OpenAiCompatAdapter::new(OpenAiCompatConfig::new(
        "deepseek",
        "https://api.deepseek.com/v1/chat/completions",
    ));
    let session_id = SessionId::from(session);
    let mut source = RemoteProviderSource::new(
        &provider,
        &egress,
        "deepseek",
        "deepseek-chat",
        session_id.clone(),
        ResolvedEffort::effort(EffortLevel::High),
    );

    let config = HarnessConfig::default();
    let tools = ToolRegistry::with_builtins();
    let tool_ctx = ToolContext::new(&repo);
    let system = build_system_prompt(&tools, &config);
    let mut ctx = ContextManager::new(system, config.context_budget_tokens);
    ctx.push_user("Push the branch.");

    let bus = Arc::new(EventBus::new());
    let pending = Arc::new(PendingPermissions::new());
    let gate = PermissionGate::new(
        session_id.clone(),
        PermissionConfig::permissive(),
        Arc::clone(&bus),
        Arc::clone(&pending),
    );
    let events = SessionEvents::new(Arc::clone(&bus), session_id);
    let mut hook = NoopProvenanceHook;
    let mut sub = bus.subscribe(256);

    let outcome = run_session_turn_with_source(
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
    .await
    .expect("the turn completes");
    assert_eq!(outcome.stop_reason, StopReason::EndTurn);

    let drained = collect_events(&mut sub).await;
    // The tool result as the model was shown it — the rendered artifact AC-1
    // and AC-5 are about (LESSON-519), read back off the assembled context
    // rather than reconstructed.
    let transcript: String = ctx
        .blocks()
        .iter()
        .map(|b| b.text.as_str())
        .collect::<Vec<_>>()
        .join("\n");
    std::fs::remove_dir_all(&repo).ok();
    (drained, transcript)
}

// --------------------------------------------------------------------------
// AC-4 (BR-3) — in-band on the tool result, and nowhere else.
// --------------------------------------------------------------------------

/// REQ-607 AC-4 / BR-3 — the advisory rides the tool result, and **no published
/// event describes the withheld set**.
///
/// REQ-596 OQ-1 settled that no `shell_env_withheld` event is emitted, on the
/// grounds that a bus event carrying a bare count is not actionable and the
/// payload that would make it actionable is the one BR-5 forbids. BR-3 adds a
/// targeted sentence without reopening that, and this is what keeps the two
/// mechanisms distinct **in fact** rather than in prose.
///
/// # Non-vacuous on both halves
///
/// An "assert nothing is there" test is trivially satisfiable by nothing having
/// happened, so both contrasts are asserted:
///
/// 1. The drain is **non-empty** — the turn really did publish events.
/// 2. The transcript **carries** the advisory — there really was a withheld set
///    to describe, so its absence from the stream is a contrast rather than an
///    artefact.
///
/// # And not by grepping for a type name
///
/// Each envelope is serialized and its *content* searched. Matching on
/// `Event::ShellEnvWithheld` would pass forever after a rename, with the
/// disclosure still in the payload.
#[tokio::test]
async fn the_advisory_rides_the_tool_result_and_no_event_describes_the_withheld_set() {
    let _serialized = ENV_MUTATION.lock().await;
    let sentinel = agent_sentinel();
    // SAFETY: this binary owns its own process; the variable is set once before
    // any spawn and never removed, and nothing here reads it concurrently.
    unsafe { std::env::set_var("SSH_AUTH_SOCK", &sentinel) };

    let (events, transcript) = run_failing_shell_turn("advisory-events").await;

    // (2) There was something to describe: the advisory really is on the result.
    assert!(
        transcript.contains("SSH_AUTH_SOCK") && transcript.contains("allow_ssh_agent"),
        "the tool result carries no advisory, so this test's real assertion below \
         would pass for the wrong reason:\n{transcript}"
    );

    // (1) The turn really published events.
    assert!(
        !events.is_empty(),
        "the drain is empty, so 'no event describes the withheld set' is vacuous"
    );

    // The assertion itself, over serialized payloads.
    for envelope in &events {
        let json = serde_json::to_string(&envelope.event).expect("an event serializes");
        assert!(
            !json.contains("SSH_AUTH_SOCK"),
            "an event named a withheld variable. BR-3 puts the advisory in-band on the \
             failing call precisely so the bus does not become a disclosure surface \
             (REQ-596 OQ-1): {json}"
        );
        assert!(
            !json.contains(sentinel.as_str()),
            "an event carried the agent socket path — a fact about this machine, which \
             REQ-596 BR-5's value half forbids unconditionally: {json}"
        );
        for shape in ["withheld", "env_withheld"] {
            assert!(
                !json.contains(shape),
                "an event carries a `{shape}`-shaped payload describing what the child was \
                 denied. A count is the shape REQ-596 OQ-1 refused: {json}"
            );
        }
    }
}

// --------------------------------------------------------------------------
// AC-5 (BR-4) — nothing read from the live environment is named.
// --------------------------------------------------------------------------

/// REQ-607 AC-5 / BR-4 — the advisory names no variable it learned from the
/// daemon's environment or from a configured credential reference.
///
/// BR-4 narrows REQ-596 BR-5 by exactly one step: a name from the **statically
/// documented** rejection table may be printed, because it is public in the
/// source and identical on every install. A name the daemon *discovered* — from
/// `std::env::vars()`, or from `auth_ref = "env:<NAME>"` — may not, because that
/// discloses this machine.
///
/// So the assertion is two-sided: `SSH_AUTH_SOCK` **is** present (the licensed
/// case, which is BR-4's benign path), and `MY_LLM_CRED_SENTINEL` is **not**,
/// along with its value.
#[tokio::test]
async fn the_advisory_names_no_variable_read_from_the_live_environment() {
    let _serialized = ENV_MUTATION.lock().await;
    let sentinel = agent_sentinel();
    let cred_value = format!("SENTINEL-cred-value-{}", std::process::id());
    // SAFETY: set once, before any spawn in this binary.
    unsafe {
        std::env::set_var("SSH_AUTH_SOCK", &sentinel);
        std::env::set_var(CRED_SENTINEL, &cred_value);
    }

    // The credential reference the daemon was told about, installed through the
    // real policy plumbing rather than a hand-built set.
    let config = teton_core::config::Config::from_toml(&format!(
        r#"
[[providers]]
id = "deepseek"
kind = "openai-compatible"
endpoint = "https://deepseek.invalid/v1"
model = "m"
auth_ref = "env:{CRED_SENTINEL}"
"#
    ))
    .expect("the fixture config parses");
    // The name is taken from the config rather than restated, so a fixture whose
    // `auth_ref` stopped naming this variable could not silently keep passing.
    let configured = config
        .providers
        .iter()
        .find_map(|p| p.auth_ref.as_deref())
        .and_then(|r| r.strip_prefix("env:"))
        .expect("the fixture's auth_ref names an env variable");
    assert_eq!(configured, CRED_SENTINEL);

    // The set is handed over directly. `credential_env_names_of` — the
    // `auth_ref` → name derivation — is crate-private and already pinned at unit
    // level by REQ-596's own tests, and widening its visibility for a test would
    // spend the visibility ratchet on nothing: AC-5's claim is about what the
    // *advisory* does with a credential set, not about how the set is derived.
    let names: std::collections::BTreeSet<String> =
        std::iter::once(configured.to_owned()).collect();
    tetond::child_env::set_child_env_policy_provider(move || tetond::child_env::ChildEnvPolicy {
        credential_env_names: names.clone(),
        allow_ssh_agent: false,
    });

    let (_events, transcript) = run_failing_shell_turn("advisory-names").await;

    // The licensed case — BR-4's benign path. Without this, an implementation
    // that named nothing at all would satisfy every assertion below.
    assert!(
        transcript.contains("SSH_AUTH_SOCK"),
        "the advisory named nothing, so the prohibitions below are vacuous:\n{transcript}"
    );

    assert!(
        !transcript.contains(CRED_SENTINEL),
        "the advisory named a variable resolved from a configured auth_ref — a name the \
         daemon learned about this machine, which BR-4 leaves unnameable:\n{transcript}"
    );
    assert!(
        !transcript.contains(cred_value.as_str()),
        "a credential value reached tool output. That half of REQ-596 BR-5 is untouched \
         and unconditional:\n{transcript}"
    );
    assert!(
        !transcript.contains(sentinel.as_str()),
        "the advisory disclosed the agent socket path:\n{transcript}"
    );
}
