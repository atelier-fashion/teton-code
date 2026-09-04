//! REQ-617 TASK-004/TASK-008 — the per-turn repeat refusal, from outside the
//! tool (BR-4, BR-5, BR-6; AC-4, AC-5, AC-6, AC-10).
//!
//! Every claim here is about a **sequence** of calls inside one prompt turn, and
//! a sequence is only visible from outside the tools: how many the loop
//! dispatched, what the model got back for the ones it did not, whether the
//! ledger survived into the next prompt. So each test drives
//! [`DaemonRuntime::run_prompt_turn`] over a scripted vendor and reads the wire,
//! rather than calling `RepeatLedger` in a loop — which would assert the
//! ledger's own bookkeeping and nothing about the loop that consults it.
//!
//! The ledger's *unit* claims — the fingerprint, the two thresholds, the verb
//! table — are in `harness::repeat`'s own test block. This binary is the other
//! half: that the loop asks it, before dispatching, and does the right thing
//! with the answer.
//!
//! ## Why its own binary
//!
//! Integration test binaries share no modules in this workspace, so the vendor
//! and harness below are a copy of `skill_tool_loop.rs`'s, cut down to the verbs
//! these claims need. That is the house pattern (`provenance_egress.rs`,
//! `egress_capture.rs` and `cost_attribution.rs` each carry their own).
//!
//! ## What is pinned, and where
//!
//! | Claim | Test |
//! |---|---|
//! | AC-4: five identical `shell` calls dispatch once, refuse four | [`five_identical_shell_calls_dispatch_once_and_refuse_four_times`] |
//! | AC-4/BR-4: each refusal carries the sentence and the byte count | [`five_identical_shell_calls_dispatch_once_and_refuse_four_times`] |
//! | AC-4: `tool_call_repeated` fires per refusal, carrying no arguments | [`the_repeated_event_carries_no_arguments`] |
//! | AC-5: `edit` dispatches twice, is refused on the third | [`write_capable_tools_get_a_second_chance`] |
//! | AC-5: `read` is refused on the second | [`write_capable_tools_get_a_second_chance`] |
//! | AC-5/BR-6: `ls -la` then `ls -la .` both dispatch | [`identical_means_identical_and_a_new_turn_starts_empty`] |
//! | AC-6: a new prompt turn dispatches what the last one refused | [`identical_means_identical_and_a_new_turn_starts_empty`] |
//! | BR-5: the refusal rides outside the untrusted frame | [`a_repeat_refusal_rides_outside_the_untrusted_frame`] |
//! | AC-10: the recorded 26-call turn replays in at most 9 | [`the_recorded_twenty_six_call_turn_replays_in_nine`] |
//!
//! ## Mutation table
//!
//! Every row below was **run**, and the "fails" column records what actually
//! went red rather than what was expected to (conventions.md: show the test can
//! fail before trusting that it passed).
//!
//! | Mutation | Tests that failed |
//! |---|---|
//! | the gate never consulted (equivalent to placing it after `tools.dispatch`) | all but [`identical_means_identical_and_a_new_turn_starts_empty`], which asserts only that two *different* calls dispatch and is silent when nothing is ever refused — a gap worth knowing about, and the reason it is not the only test here |
//! | both thresholds collapsed to `Twice` | five of six |
//! | both thresholds collapsed to `Once` | [`write_capable_tools_get_a_second_chance`], [`the_recorded_twenty_six_call_turn_replays_in_nine`] |
//! | `record` called on the refusal arm as well as the dispatch arm | [`five_identical_shell_calls_dispatch_once_and_refuse_four_times`] — via the count the refusal sentence carries, **not** via the retry, which was the prediction |
//! | the fingerprint keyed on the tool name alone | [`identical_means_identical_and_a_new_turn_starts_empty`], [`the_recorded_twenty_six_call_turn_replays_in_nine`] |
//!
//! Two rows are still predictions rather than runs, and are marked as such:
//! pushing the refusal through `frame_untrusted_builtin`
//! ([`a_repeat_refusal_rides_outside_the_untrusted_frame`]) and giving
//! `tool_call_repeated` an `arguments` field
//! ([`the_repeated_event_carries_no_arguments`]) both need a code change with no
//! one-line spelling. Each test asserts the property directly, so the claim is
//! not weak — but it has not been demonstrated red, and saying which is which is
//! the difference between evidence and confidence.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use serde_json::{json, Value};
use tokio::time::timeout;

use teton_protocol::events::Event;
use teton_protocol::methods::TierBindingConfig;
use teton_protocol::methods::{ConfigUpdate, ProviderConfig, SessionPermissionsParams};
use teton_protocol::permissions::PermissionLevel;
use teton_protocol::{
    Phase as ProtoPhase, ProviderId, ProviderKind as ProtoProviderKind, SessionId, SessionMode,
    Tier as ProtoTier,
};

use tetond::broadcast::EventBus;
use tetond::grants::{ConnectionId, GrantRegistry};
use tetond::runtime::{ClientPresence, DaemonRuntime};
use tetond::sessions::SessionRegistry;

// ---------------------------------------------------------------------------
// fixtures
// ---------------------------------------------------------------------------

/// A throwaway directory tree, removed on drop.
struct Tree {
    root: PathBuf,
}

impl Tree {
    fn new(tag: &str) -> Self {
        static SEQ: AtomicUsize = AtomicUsize::new(0);
        let seq = SEQ.fetch_add(1, Ordering::SeqCst);
        let root =
            PathBuf::from("/tmp").join(format!("rpt{tag}{:x}{seq:x}", std::process::id() & 0xffff));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).unwrap();
        std::fs::write(root.join("Cargo.toml"), "[package]\n").unwrap();
        std::fs::write(root.join("README.md"), "hello from the fixture\n").unwrap();
        Self { root }
    }

    fn path(&self) -> &Path {
        &self.root
    }
}

impl Drop for Tree {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.root);
    }
}

// ---------------------------------------------------------------------------
// a scripted vendor
// ---------------------------------------------------------------------------

#[derive(Debug, Clone)]
struct Reply(String);

/// One OpenAI-compatible streaming turn, in the shape the real adapter parses.
fn sse_turn(content: Option<&str>, tool: Option<(&str, &str, &str)>) -> String {
    let mut s = String::new();
    if let Some(text) = content {
        let chunk = json!({ "choices": [{ "delta": { "content": text } }] });
        s.push_str(&format!("data: {chunk}\n\n"));
    }
    if let Some((id, name, args)) = tool {
        let chunk = json!({
            "choices": [{
                "delta": { "tool_calls": [{
                    "index": 0,
                    "id": id,
                    "function": { "name": name, "arguments": args }
                }]}
            }]
        });
        s.push_str(&format!("data: {chunk}\n\n"));
        let finish = json!({ "choices": [{ "delta": {}, "finish_reason": "tool_calls" }] });
        s.push_str(&format!("data: {finish}\n\n"));
    } else {
        let finish = json!({ "choices": [{ "delta": {}, "finish_reason": "stop" }] });
        s.push_str(&format!("data: {finish}\n\n"));
    }
    s.push_str("data: {\"usage\":{\"prompt_tokens\":5,\"completion_tokens\":2}}\n\n");
    s.push_str("data: [DONE]\n\n");
    s
}

struct Vendor {
    endpoint: String,
    bodies: Arc<Mutex<Vec<String>>>,
    script: Arc<Mutex<std::collections::VecDeque<Reply>>>,
    next_call: Arc<AtomicUsize>,
}

impl Vendor {
    fn start() -> Self {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind a mock vendor");
        let addr = listener.local_addr().expect("mock vendor address");
        let bodies = Arc::new(Mutex::new(Vec::new()));
        let script: Arc<Mutex<std::collections::VecDeque<Reply>>> = Arc::default();
        let captured = Arc::clone(&bodies);
        let scripted = Arc::clone(&script);
        std::thread::spawn(move || {
            use std::io::{Read, Write};
            for stream in listener.incoming() {
                let Ok(mut stream) = stream else { break };
                // Read by framing, never by a chunking heuristic (LESSON-540).
                let mut raw = Vec::new();
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
                let body = scripted
                    .lock()
                    .unwrap()
                    .pop_front()
                    .map_or_else(|| sse_turn(Some("done"), None), |Reply(body)| body);
                let _ = stream.write_all(
                    format!(
                        "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\n\
                         Content-Length: {}\r\nConnection: close\r\n\r\n{body}",
                        body.len()
                    )
                    .as_bytes(),
                );
                let _ = stream.flush();
            }
        });
        Self {
            endpoint: format!("http://{addr}/v1/chat/completions"),
            bodies,
            script,
            next_call: Arc::new(AtomicUsize::new(1)),
        }
    }

    /// Answer the next request with one call to `tool`.
    fn will_call(&self, tool: &str, arguments: &Value) {
        let id = format!("call-{}", self.next_call.fetch_add(1, Ordering::SeqCst));
        self.script.lock().unwrap().push_back(Reply(sse_turn(
            None,
            Some((&id, tool, &arguments.to_string())),
        )));
    }

    /// Answer the next request with prose and no call, ending the turn.
    fn will_finish(&self) {
        self.script
            .lock()
            .unwrap()
            .push_back(Reply(sse_turn(Some("done"), None)));
    }
}

// ---------------------------------------------------------------------------
// the harness
// ---------------------------------------------------------------------------

struct Harness {
    runtime: Arc<DaemonRuntime>,
    events: Arc<EventBus>,
    sessions: SessionRegistry,
    vendor: Vendor,
    connection: ConnectionId,
}

impl Harness {
    fn new() -> Self {
        let vendor = Vendor::start();
        // REQ-597: this binary's subject is the loop, not privacy. A `shell`
        // result carries Unknown provenance, which with the shipped boundary set
        // pins every turn below to a local tier this runtime has not got.
        let runtime = Arc::new(DaemonRuntime::minimal().with_default_boundaries_disabled());
        runtime
            .apply_config_update(ConfigUpdate::RegisterProvider(ProviderConfig {
                id: ProviderId::from("mock"),
                kind: ProtoProviderKind::OpenaiCompatible,
                endpoint: Some(vendor.endpoint.clone()),
                model: Some("mock-1".to_owned()),
                auth_ref: None,
                max_context: Some(128_000),
                context_budget_cap: None,
                allow_cleartext: None,
                floored_budget: None,
            }))
            .expect("registering a provider");
        for tier in [ProtoTier::Scan, ProtoTier::Build, ProtoTier::Think] {
            runtime
                .apply_config_update(ConfigUpdate::SetTierBinding(TierBindingConfig {
                    tier,
                    provider_id: ProviderId::from("mock"),
                    fallback_id: None,
                }))
                .expect("binding a tier");
        }
        Self {
            runtime,
            events: Arc::new(EventBus::new()),
            sessions: SessionRegistry::new(),
            vendor,
            connection: GrantRegistry::new().next_connection_id(),
        }
    }

    /// A session at `cwd`, at `full` so no tool call below raises a prompt
    /// nobody is here to answer.
    fn session_at(&self, cwd: &Path) -> SessionId {
        let id = self
            .sessions
            .create(
                SessionMode::Structured,
                Some(ProtoPhase::Implement),
                Some(cwd.to_path_buf()),
            )
            .expect("a structured session takes a phase")
            .session_id;
        let result = self.runtime.session_permissions(
            &SessionPermissionsParams {
                session_id: id.clone(),
                level: Some(PermissionLevel::Full),
            },
            &self.events,
        );
        assert!(
            result.changed && result.level == PermissionLevel::Full,
            "the session did not move to full: {result:?}"
        );
        id
    }

    async fn turn(&self, id: &SessionId, prompt: &str) {
        let _ = self
            .runtime
            .run_prompt_turn(
                &self.events,
                &self.sessions,
                id.clone(),
                SessionMode::Structured,
                Some(ProtoPhase::Implement),
                Some(
                    self.sessions
                        .get(id)
                        .and_then(|s| s.cwd)
                        .expect("the fixture always roots its sessions"),
                ),
                prompt.to_owned(),
                None,
                Some(self.connection),
                ClientPresence::unwatched(),
            )
            .await;
    }
}

async fn drain(sub: &mut tetond::broadcast::Subscription) -> Vec<Event> {
    let mut out = Vec::new();
    while let Ok(Some(env)) = timeout(Duration::from_millis(150), sub.recv()).await {
        out.push(env.event);
    }
    out
}

/// How many tool calls the loop **served** — dispatched or refused.
///
/// Counted from `ToolCallUpdate`, which the loop publishes on every arm that
/// finishes a call, refusals included. Derived this way rather than from the
/// vendor's request count because the loop's exit shape differs by how the turn
/// ended: a turn that stops at `max_turns` makes no final prose request, so
/// `requests - 1` is right for one exit and off by one for the other.
fn served(published: &[Event]) -> usize {
    published
        .iter()
        .filter(|event| match event {
            Event::SessionUpdate(update) => matches!(
                update.update,
                teton_protocol::events::SessionUpdatePayload::ToolCallUpdate { .. }
            ),
            _ => false,
        })
        .count()
}

/// Every `tool_call_repeated` in `published`, in order.
fn repeats(published: &[Event]) -> Vec<teton_protocol::events::ToolCallRepeated> {
    published
        .iter()
        .filter_map(|event| match event {
            Event::ToolCallRepeated(r) => Some(r.clone()),
            _ => None,
        })
        .collect()
}

/// Every message content of the last request the vendor was handed.
fn last_request_messages(vendor: &Vendor) -> Vec<String> {
    vendor
        .bodies
        .lock()
        .unwrap()
        .last()
        .and_then(|raw| {
            let (_, body) = raw.split_once("\r\n\r\n")?;
            serde_json::from_str::<Value>(body).ok()
        })
        .map(|request| {
            request["messages"]
                .as_array()
                .cloned()
                .unwrap_or_default()
                .iter()
                .map(|m| m["content"].as_str().unwrap_or_default().to_owned())
                .collect::<Vec<_>>()
        })
        .unwrap_or_default()
}

/// Every tool result in the conversation the last request carried.
fn tool_results(vendor: &Vendor) -> Vec<String> {
    last_request_messages(vendor)
        .into_iter()
        .filter(|content| content.starts_with("Tool result ("))
        .collect()
}

// ---------------------------------------------------------------------------
// AC-4 — the transcript's own loop
// ---------------------------------------------------------------------------

/// **AC-4: five identical `shell: ls -la`, one dispatch, four refusals.**
///
/// This is the transcript's loop, exactly: `ls -la` five times in one turn, each
/// returning the same bytes. `ls` is a read-only verb, so the threshold is one
/// dispatch, and the four refusals each carry BR-4's sentence with the byte
/// count of what the first call returned.
///
/// The dispatch count is read as **one directory listing among the tool
/// results** rather than by counting requests: the loop makes a request per
/// iteration whether it dispatched or refused, so a request count would be five
/// either way and could not tell the two apart. That is the mutation this test
/// exists to catch — a gate placed *after* `tools.dispatch` refuses nothing and
/// still looks identical from the request side.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn five_identical_shell_calls_dispatch_once_and_refuse_four_times() {
    let repo = Tree::new("five");
    let h = Harness::new();
    let session = h.session_at(repo.path());
    let mut sub = h.events.subscribe(512);

    let call = json!({ "command": "ls -la" });
    for _ in 0..5 {
        h.vendor.will_call("shell", &call);
    }
    h.vendor.will_finish();

    h.turn(&session, "list the files").await;
    let published = drain(&mut sub).await;

    let results = tool_results(&h.vendor);
    let refused: Vec<&String> = results
        .iter()
        .filter(|r| r.contains("repeated: this exact call"))
        .collect();
    let dispatched = results.len() - refused.len();

    assert_eq!(
        dispatched,
        1,
        "one dispatch, four refusals — got {dispatched} dispatched of \
         {} results. A gate placed after `tools.dispatch` produces five \
         dispatches here and is otherwise invisible.\nresults: {results:#?}",
        results.len()
    );
    assert_eq!(
        refused.len(),
        4,
        "four refusals expected; got {}\nresults: {results:#?}",
        refused.len()
    );

    for refusal in &refused {
        assert!(
            refusal.contains("Change the arguments or finish."),
            "a refusal must say what to do instead: {refusal}"
        );
        assert!(
            refusal.contains("the result is above"),
            "a refusal must point at the result the model already holds: {refusal}"
        );
        assert!(
            refusal.contains(" bytes;"),
            "a refusal must name the byte count so the model can find it: {refusal}"
        );
    }

    let events = repeats(&published);
    assert_eq!(
        events.len(),
        4,
        "one `tool_call_repeated` per refusal; got {events:?}"
    );
    for event in &events {
        assert_eq!(event.tool, "shell");
        assert_eq!(
            event.count, 1,
            "`ls` is read-only, so every refusal follows exactly one dispatch"
        );
    }
}

/// **AC-4's cost half, and BR-4's payload rule.**
///
/// The event carries the tool and a count. It must not carry the command — a
/// `shell` line can hold a path a boundary covers, and an event reaches every
/// attached client and every declared monitor (REQ-611 BR-4, LESSON-513).
///
/// Asserted by planting a distinctive string in the arguments and searching the
/// serialized event for it, rather than by reading the struct's fields: a field
/// list is what a future edit changes, and this is the assertion that would
/// notice.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_repeated_event_carries_no_arguments() {
    let repo = Tree::new("noargs");
    let h = Harness::new();
    let session = h.session_at(repo.path());
    let mut sub = h.events.subscribe(512);

    // The marker rides in the arguments, and the command has to **succeed**:
    // a failing `shell` ends this turn before the second call is served, which
    // would make the assertion below vacuous rather than false.
    const MARKER: &str = "zzmarkerzz";
    std::fs::write(repo.path().join(MARKER), "x\n").unwrap();
    let call = json!({ "command": format!("ls -la {MARKER}") });
    h.vendor.will_call("shell", &call);
    h.vendor.will_call("shell", &call);
    h.vendor.will_finish();

    h.turn(&session, "list the files").await;
    let published = drain(&mut sub).await;

    let names: Vec<&str> = published
        .iter()
        .map(teton_protocol::events::Event::name)
        .collect();
    let results = tool_results(&h.vendor);
    let events = repeats(&published);
    assert_eq!(
        events.len(),
        1,
        "one refusal expected.\nevents: {names:?}\nresults: {results:#?}"
    );
    let serialized = serde_json::to_string(&events[0]).expect("the event serializes");
    assert!(
        !serialized.contains(MARKER),
        "`tool_call_repeated` carried the call's arguments: {serialized}. The \
         ledger hashes them precisely so this cannot happen; a field that put \
         them back is a disclosure surface with no actionable payload."
    );
}

/// **The byte count is the one the model can act on** (BR-4, found in verify).
///
/// The refusal says *"returned N bytes; the result is above"*, so N has to
/// describe the block that is actually above. The ledger was first recorded at
/// `tools.dispatch`, off the tool's raw output — which is not what enters the
/// conversation: the result is framed in the untrusted envelope on its way
/// there, and an oversized one is condensed by the `digest` duty first.
///
/// This asserts the recorded figure against the **framed** result the model
/// holds, by finding the tool result in the conversation and comparing lengths.
/// Recording at the dispatch instead makes the two disagree by the frame's own
/// bytes — several hundred on every `shell` call — and by orders of magnitude
/// on anything digested.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_refusal_counts_the_bytes_the_model_actually_holds() {
    let repo = Tree::new("bytes");
    let h = Harness::new();
    let session = h.session_at(repo.path());

    let call = json!({ "command": "ls -la" });
    h.vendor.will_call("shell", &call);
    h.vendor.will_call("shell", &call);
    h.vendor.will_finish();
    h.turn(&session, "list the files").await;

    let results = tool_results(&h.vendor);
    let dispatched = results
        .iter()
        .find(|r| !r.contains("repeated: this exact call"))
        .expect("one dispatch");
    let refusal = results
        .iter()
        .find(|r| r.contains("repeated: this exact call"))
        .expect("one refusal");

    // The conversation stores the result body; `Tool result (shell):\n` is the
    // renderer's own prefix on the way to the wire, so it is taken back off
    // before the comparison.
    let body = dispatched
        .split_once('\n')
        .map_or(dispatched.as_str(), |(_, rest)| rest);
    let claimed: usize = refusal
        .split(" and returned ")
        .nth(1)
        .and_then(|tail| tail.split(' ').next())
        .and_then(|n| n.parse().ok())
        .unwrap_or_else(|| panic!("no byte count in the refusal: {refusal}"));

    assert_eq!(
        claimed,
        body.len(),
        "the refusal claims {claimed} bytes; the block the model holds is {} \
         bytes. Recording the ledger at `tools.dispatch` rather than at the \
         push produces exactly this gap — the untrusted frame's own bytes on \
         every call, and the whole difference on anything the `digest` duty \
         condensed.\nbody: {body:?}",
        body.len()
    );
    assert!(
        body.len() > 200,
        "non-vacuity: the framed body must be substantially larger than the raw \
         listing, or this test cannot tell the two recording points apart"
    );
}

// ---------------------------------------------------------------------------
// AC-5 — the two thresholds
// ---------------------------------------------------------------------------

/// **AC-5: `edit` dispatches twice and is refused on the third; `read` is
/// refused on the second.**
///
/// The two thresholds in one turn, over one session, so a build that collapsed
/// them in either direction fails here rather than passing half of a pair.
///
/// The `edit` retry is the **benign path** and it is the reason the second
/// threshold exists: a failed edit tried again after the model changed something
/// the arguments do not record is a real second attempt, not a loop.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn write_capable_tools_get_a_second_chance() {
    let repo = Tree::new("thresh");
    let h = Harness::new();
    let session = h.session_at(repo.path());

    let edit = json!({
        "path": "README.md",
        "old_string": "hello from the fixture",
        "new_string": "hello from the fixture"
    });
    for _ in 0..3 {
        h.vendor.will_call("edit", &edit);
    }
    let read = json!({ "path": "README.md" });
    for _ in 0..2 {
        h.vendor.will_call("read", &read);
    }
    h.vendor.will_finish();

    h.turn(&session, "edit and read").await;

    let results = tool_results(&h.vendor);
    let refusals: Vec<&String> = results
        .iter()
        .filter(|r| r.contains("repeated: this exact call"))
        .collect();

    assert_eq!(
        refusals.len(),
        2,
        "exactly two refusals: the third `edit` and the second `read`.\n\
         Collapsing both thresholds to `Once` gives three; collapsing both to \
         `Twice` gives one.\nresults: {results:#?}"
    );
    // The `edit` refusal follows two dispatches; the `read` refusal follows one.
    // Asserted on the counts the sentences carry, which is the only place the
    // two thresholds are distinguishable from out here.
    let counts: Vec<bool> = refusals
        .iter()
        .map(|r| r.contains("already ran 2 times"))
        .collect();
    assert_eq!(
        counts,
        vec![true, false],
        "the `edit` refusal must report two prior dispatches and the `read` \
         refusal one — in that order.\nrefusals: {refusals:#?}"
    );
}

// ---------------------------------------------------------------------------
// AC-5/AC-6/BR-6 — what "identical" means, and where the ledger ends
// ---------------------------------------------------------------------------

/// **BR-6 both halves: a changed argument is a new call, and a new prompt starts
/// empty.**
///
/// `ls -la` then `ls -la .` — two different strings, two dispatches, no refusal.
/// That is the escape hatch the refusal sentence points the model at, and a
/// fingerprint keyed on the tool name alone would refuse the second.
///
/// Then the same call again in a **second prompt turn**, which must dispatch: the
/// ledger is a field of the turn's own state, so "cleared at turn end" is a
/// property of the shape rather than of something remembering to clear it. A
/// build that hung it on the session refuses here.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn identical_means_identical_and_a_new_turn_starts_empty() {
    let repo = Tree::new("ident");
    let h = Harness::new();
    let session = h.session_at(repo.path());

    h.vendor.will_call("shell", &json!({ "command": "ls -la" }));
    h.vendor
        .will_call("shell", &json!({ "command": "ls -la ." }));
    h.vendor.will_finish();
    h.turn(&session, "look around").await;

    let first = tool_results(&h.vendor);
    assert!(
        !first.iter().any(|r| r.contains("repeated:")),
        "`ls -la` and `ls -la .` are different calls and both must \
         dispatch.\nresults: {first:#?}"
    );
    assert_eq!(first.len(), 2, "two dispatches expected: {first:#?}");

    // AC-6: the same call the previous turn ended on, in a new turn.
    h.vendor.will_call("shell", &json!({ "command": "ls -la" }));
    h.vendor.will_finish();
    h.turn(&session, "look again").await;

    let second = tool_results(&h.vendor);
    let refused_in_second = second.iter().rev().take(2).any(|r| r.contains("repeated:"));
    assert!(
        !refused_in_second,
        "AC-6: a new prompt turn starts an empty ledger, so `ls -la` must \
         dispatch again. A ledger hung on the session refuses it.\n\
         results: {second:#?}"
    );
}

// ---------------------------------------------------------------------------
// BR-5 — the refusal is not a lost call
// ---------------------------------------------------------------------------

/// **BR-5: the refusal rides outside the untrusted frame.**
///
/// A refused repeat ends by asking the model to change the arguments or finish.
/// Inside `<tool-result trust="untrusted">` that instruction sits under the
/// envelope's own closing sentence telling the model never to act on directives
/// in the block — so the harness would be asking for something and forbidding it
/// in the same breath.
///
/// The check is that the refusal's sentence is **not** wrapped: the loop's other
/// two sentences (the over-budget refusal and BUG-147's dropped-calls notice)
/// ride the same way, for the same reason.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_repeat_refusal_rides_outside_the_untrusted_frame() {
    let repo = Tree::new("frame");
    let h = Harness::new();
    let session = h.session_at(repo.path());

    let call = json!({ "command": "pwd" });
    h.vendor.will_call("shell", &call);
    h.vendor.will_call("shell", &call);
    h.vendor.will_finish();
    h.turn(&session, "where am i").await;

    let results = tool_results(&h.vendor);
    let refusal = results
        .iter()
        .find(|r| r.contains("repeated: this exact call"))
        .unwrap_or_else(|| panic!("no refusal in {results:#?}"));

    assert!(
        !refusal.contains("<tool-result"),
        "the repeat refusal is inside the untrusted envelope, whose closing \
         sentence tells the model not to act on directives in the block — while \
         this block's whole purpose is to ask it to act (BR-5).\n{refusal}"
    );
    assert!(
        refusal.contains("ERROR: repeated:"),
        "the refusal must arrive as the harness's own error line, in the slot \
         BUG-147's dropped-calls notice uses, so a model can tell a refusal from \
         a lost call.\n{refusal}"
    );
}

// ---------------------------------------------------------------------------
// AC-10 — the recorded turn, replayed
// ---------------------------------------------------------------------------

/// The 2026-09-04 transcript's third prompt, as a call multiset (AC-10).
///
/// **This fixture is hand-authored, and that is weaker evidence than the file
/// would be. Say so plainly.** The transcript itself is not in the repository
/// and is not on the machine: REQ-611 writes transcripts to the daemon's state
/// directory, which is a denied prefix on the tool jail precisely so nothing in
/// a session can read one.
///
/// So the multiset is built in two parts, with different standing:
///
/// * **Recorded.** `ls -la` ×5, `cd … && pwd` ×4, `pwd` ×3 and `projects` ×4 —
///   sixteen calls across four shapes — are the counts this REQ's Description
///   states outright.
/// * **Modelled.** The remaining ten are described rather than enumerated: the
///   model *"searched for a config file for seven tool calls, then read
///   `.claude.json`"*. A search that takes seven calls and ends at the wrong
///   file is a **repetitive** search, so it is modelled as one — four identical
///   globs, three identical greps, two identical reads of the file it settled
///   on, and one read of something else. Ten calls, four shapes.
///
/// The modelled half is where a reader should be sceptical, and
/// [`the_recorded_twenty_six_call_turn_replays_in_nine`] is written so that the
/// claim it actually proves does not depend on it — see that test's second
/// assertion, which is the load-bearing one.
fn recorded_calls() -> Vec<(&'static str, Value)> {
    let mut calls: Vec<(&'static str, Value)> = Vec::new();
    // Recorded: the four shapes the Description counts.
    for _ in 0..5 {
        calls.push(("shell", json!({ "command": "ls -la" })));
    }
    for _ in 0..4 {
        calls.push(("shell", json!({ "command": "cd /tmp && pwd" })));
    }
    for _ in 0..3 {
        calls.push(("shell", json!({ "command": "pwd" })));
    }
    for _ in 0..4 {
        calls.push(("projects", json!({})));
    }
    // Modelled: the config-file search, as a search that did not find anything.
    for _ in 0..4 {
        calls.push(("glob", json!({ "pattern": "*.json" })));
    }
    for _ in 0..3 {
        calls.push(("grep", json!({ "pattern": "transcript" })));
    }
    for _ in 0..2 {
        calls.push(("read", json!({ "path": ".claude.json" })));
    }
    calls.push(("read", json!({ "path": "README.md" })));
    calls
}

/// How many distinct `(tool, arguments)` shapes [`recorded_calls`] holds.
fn distinct_shapes(calls: &[(&'static str, Value)]) -> usize {
    let mut seen = std::collections::BTreeSet::new();
    for (tool, arguments) in calls {
        seen.insert(format!("{tool}{arguments}"));
    }
    seen.len()
}

/// **AC-10: 26 recorded calls, at most 9 dispatched.**
///
/// Three assertions, and the middle one is the load-bearing claim.
///
/// 1. **The baseline is 26.** Asserted before the replay so it cannot drift from
///    the number AC-10 names — a fixture that quietly became 24 calls would make
///    the reduction look better and nothing would say so.
///
/// 2. **The reduction is exactly the repeats.** `dispatched` equals the number of
///    distinct call shapes plus one for each write-capable shape's admitted
///    retry. This is the property the ledger actually guarantees, it is
///    falsifiable, and — the point — it does **not** depend on the modelled half
///    of the fixture being a faithful reconstruction. Whatever the real
///    transcript's other ten calls were, the loop dispatches one of each shape
///    and refuses the rest, which is the whole of what this REQ claims.
///
/// 3. **`dispatched <= 9`**, AC-10's stated ceiling. This one *does* depend on
///    the modelled half: nine is reachable because a seven-call search that
///    found nothing repeats itself. If the real search was seven *distinct*
///    calls, the real turn reduces to twelve rather than nine and AC-10's figure
///    is optimistic — which is a fact about the number in the requirement, not
///    about the mechanism, and assertion 2 is why this test is still worth
///    having either way.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_recorded_twenty_six_call_turn_replays_in_nine() {
    let calls = recorded_calls();
    assert_eq!(
        calls.len(),
        26,
        "the recorded baseline is 26 calls (REQ-617 Description). A fixture \
         that drifted from that number makes the reduction unreadable."
    );

    let repo = Tree::new("replay");
    // The modelled search settles on `.claude.json`, and it has to *find* it:
    // a failing tool call ends the turn early, which would leave the count
    // below measuring where the turn stopped rather than what the ledger
    // refused.
    std::fs::write(repo.path().join(".claude.json"), "{}\n").unwrap();
    let h = Harness::new();
    let session = h.session_at(repo.path());
    let mut sub = h.events.subscribe(1024);

    for (tool, arguments) in &calls {
        h.vendor.will_call(tool, arguments);
    }
    h.vendor.will_finish();
    h.turn(&session, "analyze this project").await;
    let published = drain(&mut sub).await;

    // **Counted from the bus and the wire, not from the conversation.**
    //
    // The obvious way to count dispatches — filter the last request's tool
    // results for the ones that are not refusals — is wrong here and was wrong
    // silently. The conversation grows with every result, so by call twenty-six
    // the context gate has compacted the oldest blocks away, and a *dispatched*
    // result that has been compacted is indistinguishable from one that never
    // happened. It undercounted by one.
    //
    // Events are not compacted. Every served call — dispatched or refused —
    // publishes one `ToolCallUpdate`, and every refusal publishes one
    // `tool_call_repeated`, so the difference is the dispatches.
    let refusals = repeats(&published).len();
    let served = served(&published);
    let dispatched = served - refusals;

    assert!(
        served >= 20,
        "only {served} of the 26 calls reached the loop, so the turn ended early \
         and the counts below measure where it stopped rather than what the \
         ledger refused"
    );

    // **The turn does not reach the 26th call, and that is part of the claim.**
    // `max_turns` ends it at 25 served, so the recorded turn's tail — the one
    // `read` of a different file — never happens. Nothing is wrong with that;
    // it is the second bound the loop has always had, and this REQ's refusals
    // are what let 25 iterations cover 25 of the model's calls instead of
    // stalling on four spellings of `ls -la`.
    //
    // So the expectation is computed over the calls that were **served**, not
    // over all 26. Spelling `9` here instead would be a figure that silently
    // stops meaning anything the moment `max_turns` moves.
    //
    // The load-bearing assertion: one dispatch per distinct call shape served,
    // plus the admitted retry for the single write-capable shape among them
    // (`cd … && pwd` chains, so it is write-capable).
    //
    // `distinct_shapes` derives the shape count rather than spelling it. The
    // `+ 1` is spelled, because it is a claim about the *fixture* — exactly one
    // of its shapes is write-capable — and deriving it from `allowance_for`
    // would be the test computing its expected value from the code under test
    // (conventions.md: never let the subject compute the oracle).
    let expected = distinct_shapes(&calls[..served]) + 1;
    assert_eq!(
        dispatched,
        expected,
        "the reduction must be exactly the repeats: {} distinct shapes among \
         the {served} calls served, plus one write-capable retry, is {expected} \
         dispatches — got {dispatched} ({refusals} refused).\nThis is the \
         claim that does not depend on the modelled half of the fixture.",
        distinct_shapes(&calls[..served])
    );

    assert!(
        dispatched <= 9,
        "AC-10: at most 9 dispatched tool calls; got {dispatched} of {served} \
         served"
    );
}
