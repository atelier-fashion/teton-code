//! REQ-587 TASK-222 — the chain, the cap and the whole expansion (AC-7, AC-8).
//!
//! The claims here are about a **sequence** of model-issued `skill` calls inside
//! one prompt turn, and a sequence is only visible from outside the tool: BR-6's
//! cap counts every call the loop dispatched, BR-6b's repeat rule turns on what
//! happened *between* two of them, and BR-7's "carried whole or refused" is a
//! statement about what the provider received. So every test drives
//! [`DaemonRuntime::run_prompt_turn`] over a scripted vendor and reads the wire,
//! rather than calling `SkillTool::invoke` in a loop — which would assert the
//! tool's own bookkeeping and nothing about the loop that spends it.
//!
//! ## Why this is its own binary
//!
//! `skill_turn.rs` owns the *turn* — ordering, the two refusal stages, the
//! consent seam — and its fixtures are built around a `Consent` double that
//! every one of its tests needs. Nothing here asks a human anything: the
//! fixtures are user-level skills with no dynamic context, run at the session
//! default, so the harness below is the turn machinery without the consent half.
//! Integration test binaries share no modules in this workspace
//! (`provenance_egress.rs`, `egress_capture.rs` and `cost_attribution.rs` each
//! carry their own scripted transport for the same reason), so the vendor is a
//! copy — a smaller one, with only the two scripting verbs these claims need.
//!
//! ## Why this binary owns `HOME`
//!
//! Two of the four discovery roots are `~/.claude/skills` and
//! `~/.claude/commands`. Left at the developer's own home, every session here
//! would register whatever skills that machine happens to have — twenty, on the
//! machine this feature was written for — and the cap fixture would be counting
//! them. So the binary points `HOME` at a fixture home once, before any daemon
//! exists, and **every figure below is a property of that fixture** rather than
//! of the machine the suite runs on (LESSON-540).
//!
//! ## What is pinned, and where
//!
//! | Claim | Test |
//! |---|---|
//! | AC-7: the thirteenth call is refused, and the next prompt starts at zero | [`the_thirteenth_call_of_a_turn_is_refused_by_the_cap_and_the_next_prompt_starts_over`] |
//! | AC-7: refusals and listings count against the cap | [`a_run_of_listings_exhausts_the_per_turn_cap`] |
//! | AC-7: a refused call never seeds the repeat rule | [`a_refused_call_never_seeds_the_repeat_rule`] |
//! | AC-7: a body that names itself stops at `repeated`, not at the cap | [`a_body_that_names_itself_stops_at_the_repeat_refusal_not_at_the_cap`] |
//! | AC-8: a 7,222-word expansion enters a 128k route whole | [`a_seven_thousand_word_expansion_enters_a_128k_route_whole_and_unelided`] |
//! | AC-8: the same fixture on an undeclared window is refused, in the spoken bound | [`the_same_fixture_on_an_undeclared_window_is_refused_in_the_bounds_spoken_form`] |
//! | BR-9: a typed refusal over a registered row publishes its own record | [`every_tool_raised_refusal_over_a_registered_skill_publishes_a_record`] |
//! | BR-9: the cap's refusal publishes one too, with the count that refused it | [`the_thirteenth_call_of_a_turn_is_refused_by_the_cap_and_the_next_prompt_starts_over`] |
//! | BR-9: a refusal with no skill file to describe publishes none | [`every_tool_raised_refusal_over_a_registered_skill_publishes_a_record`] (`unknown_skill`), [`a_run_of_listings_exhausts_the_per_turn_cap`] (no name at all) |
//!
//! ## Mutation table
//!
//! | Mutation | Test that fails |
//! |---|---|
//! | `TurnState::admit` counts *after* the cap check | [`the_thirteenth_call_of_a_turn_is_refused_by_the_cap_and_the_next_prompt_starts_over`] |
//! | the cap counted per session instead of per prompt turn | [`the_thirteenth_call_of_a_turn_is_refused_by_the_cap_and_the_next_prompt_starts_over`] |
//! | a listing returns early without counting | [`a_run_of_listings_exhausts_the_per_turn_cap`] |
//! | `note_expansion` moved above the resolution/refusal arms | [`a_refused_call_never_seeds_the_repeat_rule`] |
//! | the repeat rule keyed on the name alone, ignoring the arguments | [`a_body_that_names_itself_stops_at_the_repeat_refusal_not_at_the_cap`] |
//! | the `skill` tool absent, or its expansion never folded | [`a_seven_thousand_word_expansion_enters_a_128k_route_whole_and_unelided`] |
//! | the refusal spelling `BudgetBound::wire_name` instead of `words` | [`the_same_fixture_on_an_undeclared_window_is_refused_in_the_bounds_spoken_form`] |
//! | `SkillTool::refuse` dropping its publish (any arm of it) | [`every_tool_raised_refusal_over_a_registered_skill_publishes_a_record`] |
//! | `refuse` counting the call a second time beside the publish | [`every_tool_raised_refusal_over_a_registered_skill_publishes_a_record`] |
//! | the record's lookup reaching for `resolve_for_model` instead of `registered_row` | [`every_tool_raised_refusal_over_a_registered_skill_publishes_a_record`] |
//! | a refusal published with the *file's* dynamic outcomes on it | [`every_tool_raised_refusal_over_a_registered_skill_publishes_a_record`] |
//! | the cap arm returning before `refuse`, as it did before BR-9's record | [`the_thirteenth_call_of_a_turn_is_refused_by_the_cap_and_the_next_prompt_starts_over`] |
//! | a record invented for a call that named no skill | [`a_run_of_listings_exhausts_the_per_turn_cap`] |
//!
//! ## What is *not* here, and why
//!
//! AC-8's `bound: local engine` leg. Reaching it needs a route whose budget was
//! derived from `BudgetInputs::local()`, which needs a local engine —
//! `DaemonRuntime::minimal()` has none, and an integration test cannot install
//! one (the same wall `runtime::tests::skill_turn_readers` exists on the other
//! side of). That arm is pinned at unit level in `harness::budget`, over the
//! same composer this file reads through a socket.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::Duration;

use serde_json::{json, Value};
use tokio::time::timeout;

use teton_protocol::events::{Event, SkillInvoked};
use teton_protocol::methods::{ConfigUpdate, ProviderConfig, SkillInvocation, TierBindingConfig};
use teton_protocol::{
    Phase as ProtoPhase, ProviderId, ProviderKind as ProtoProviderKind, SessionId, SessionMode,
    Tier as ProtoTier,
};

use tetond::broadcast::EventBus;
use tetond::grants::{ConnectionId, GrantRegistry};
use tetond::runtime::{ClientPresence, DaemonRuntime};
use tetond::sessions::SessionRegistry;
use tetond::skills::RealFs;

// ---------------------------------------------------------------------------
// fixtures — in-repo and deterministic, never a read of `~/.claude`
// ---------------------------------------------------------------------------

/// The pinned cap, read from the module that owns it rather than spelled again.
const CAP: usize = tetond::harness::tools::skill::PER_TURN_INVOCATION_CAP;

/// The word count AC-8's synthetic fixture is built to, and where it comes
/// from: `/proceed`'s measured length. The real file is third-party content and
/// stays out of this repository, so the *shape* is reproduced and the *bytes*
/// are generated — deterministically, from the constant below, so the figure is
/// a property of this file and not of anybody's `~/.claude`.
const PROCEED_SHAPED_WORDS: usize = 7_222;

/// A throwaway directory tree, removed on drop.
struct Tree {
    root: PathBuf,
}

impl Tree {
    fn new(tag: &str) -> Self {
        static SEQ: AtomicUsize = AtomicUsize::new(0);
        let seq = SEQ.fetch_add(1, Ordering::SeqCst);
        let root =
            PathBuf::from("/tmp").join(format!("stl{tag}{:x}{seq:x}", std::process::id() & 0xffff));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).unwrap();
        std::fs::write(root.join("Cargo.toml"), "[package]\n").unwrap();
        Self { root }
    }

    fn write(&self, rel: &str, contents: &str) {
        let path = self.root.join(rel);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, contents).unwrap();
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

/// `words` whitespace-separated words, deterministically.
fn words(count: usize) -> String {
    let mut out = String::with_capacity(count * 4);
    for n in 0..count {
        if n > 0 {
            out.push(' ');
        }
        out.push_str("abc");
    }
    out
}

/// The `HOME` every discovery in this binary runs under, and every skill these
/// tests can reach.
///
/// **Every fixture is written here, once.** User-level skills need no
/// project-skill acknowledgment at any level (BR-4), which is what lets this
/// binary run without a consent double at all: nothing below is ever asked a
/// question, so "no prompt was raised" is not a claim any of these tests has to
/// make or could accidentally rely on.
fn fixture_home() -> &'static Path {
    static HOME: OnceLock<Tree> = OnceLock::new();
    HOME.get_or_init(|| {
        let home = Tree::new("home");
        // The chain fixture: a one-line body, so twelve of them in one turn are
        // twelve calls rather than a budget test.
        home.write(
            ".claude/skills/step/SKILL.md",
            "---\ndescription: one step of a chain\n---\n\nStep body.\n",
        );
        // A second name, so a chain can alternate without relying on arguments.
        home.write(
            ".claude/skills/other/SKILL.md",
            "---\ndescription: a second step\n---\n\nOther body.\n",
        );
        // BR-6b's self-naming body: an orchestrator whose instructions tell the
        // model to invoke the orchestrator.
        home.write(
            ".claude/skills/selfnamed/SKILL.md",
            "---\ndescription: a body that names itself\n---\n\n\
             Run `skill { name: \"selfnamed\" }` and then stop.\n",
        );
        // BR-3's hidden row, for BR-9's record: registered, with a real source,
        // path and size, and refused to the model with `not_model_invocable`.
        // It is **absent from the roster**, so adding it changes no figure the
        // listing tests read.
        //
        // It carries a **dynamic command**, which nothing here ever runs: the
        // refusal lands at resolution, above `expand_and_fold`. That is what
        // makes "a refusal carries no outcomes" a claim with something behind
        // it — a build that projected the *file's* slots onto the refusal record
        // would put a `1 dynamic command` figure on a turn in which none ran.
        home.write(
            ".claude/skills/hidden/SKILL.md",
            "---\ndescription: the user's to type\ndisable-model-invocation: true\n---\n\n\
             Hidden body: !`echo never-run`\n",
        );
        // BR-2's third shadowing case: a file whose name a built-in command owns
        // (`RESERVED_SKILL_NAMES`). Registered and listed by `/help` with a
        // mark, never dispatchable, and refused to the model with
        // `reserved_name` — the second row `resolve_for_model` refuses by design
        // and a refusal record must still be able to name.
        home.write(
            ".claude/skills/cost/SKILL.md",
            "---\ndescription: a name a built-in owns\n---\n\nCost body.\n",
        );
        // AC-8's synthetic `/proceed`: the measured word count, with a marker
        // at each end and one in the middle, so "whole" is a claim about the
        // whole file rather than about its first paragraph.
        home.write(
            ".claude/skills/proceedish/SKILL.md",
            &format!(
                "---\ndescription: a synthetic orchestrator of /proceed's measured shape\n---\n\n\
                 PROCEED-HEAD {}\nPROCEED-MIDDLE {}\nPROCEED-TAIL\n",
                words(PROCEED_SHAPED_WORDS / 2),
                words(PROCEED_SHAPED_WORDS / 2),
            ),
        );
        std::env::set_var("HOME", home.path());
        home
    })
    .path()
}

// ---------------------------------------------------------------------------
// a runtime with a route
// ---------------------------------------------------------------------------

/// One scripted answer: a 200 carrying an SSE body.
///
/// No failure arm here — the reroute claims that need one live in
/// `skill_turn.rs`, beside the fallback registration they also need.
#[derive(Debug, Clone)]
struct Reply(String);

/// One OpenAI-compatible streaming turn — `remote_loop.rs`'s `sse_turn`, in the
/// shape the real adapter parses.
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

/// A single-threaded mock OpenAI-compatible vendor on a real socket.
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
                let mut raw = Vec::new();
                let mut buf = [0u8; 65_536];
                while let Ok(read) = stream.read(&mut buf) {
                    if read == 0 {
                        break;
                    }
                    raw.extend_from_slice(&buf[..read]);
                    if raw.windows(4).any(|w| w == b"\r\n\r\n") && read < buf.len() {
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

    /// Answer the next request with one `skill` call.
    ///
    /// `name` is `None` for BR-1's listing — the call with no name, which AC-7
    /// says still counts against the cap.
    fn will_call_skill(&self, name: Option<&str>, args: &str) {
        let id = format!("call-{}", self.next_call.fetch_add(1, Ordering::SeqCst));
        let arguments = match name {
            Some(name) => json!({ "name": name, "args": args }),
            None => json!({}),
        };
        self.script.lock().unwrap().push_back(Reply(sse_turn(
            None,
            Some((&id, "skill", &arguments.to_string())),
        )));
    }

    /// Everything this vendor was asked to send, as one searchable string.
    fn on_the_wire(&self) -> String {
        self.bodies.lock().unwrap().join("\n")
    }
}

/// A daemon runtime, its bus, its sessions and the vendor its one provider
/// points at.
struct Harness {
    runtime: Arc<DaemonRuntime>,
    events: Arc<EventBus>,
    sessions: SessionRegistry,
    vendor: Vendor,
    connection: ConnectionId,
}

impl Harness {
    /// A runtime whose turn-serving tiers are bound to one remote provider
    /// declaring `max_context = window`.
    ///
    /// Installed through `config/set`'s own path, not by reaching into the
    /// runtime: the budget under test is the one `Router::budget_for` derives
    /// from a registered provider, and a hand-built `RouteBudget` would be the
    /// second derivation REQ-586 exists to prevent.
    fn with_window(window: Option<u32>) -> Self {
        fixture_home();
        let vendor = Vendor::start();
        let runtime = Arc::new(DaemonRuntime::minimal());
        runtime
            .apply_config_update(ConfigUpdate::RegisterProvider(ProviderConfig {
                id: ProviderId::from("mock"),
                kind: ProtoProviderKind::OpenaiCompatible,
                endpoint: Some(vendor.endpoint.clone()),
                model: Some("mock-1".to_owned()),
                auth_ref: None,
                max_context: window,
                context_budget_cap: None,
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

    /// A structured session rooted at `cwd`, with its skill registry derived
    /// from that root exactly as `session/create` derives it.
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
        let probed = self.runtime.session_root_for(Some(cwd));
        self.sessions.set_skills(
            &id,
            tetond::skills::discover(
                Some(fixture_home()),
                &probed.path,
                probed.view.kind,
                &RealFs,
            ),
        );
        id
    }

    async fn turn(
        &self,
        id: &SessionId,
        prompt: &str,
        skill: Option<SkillInvocation>,
    ) -> Result<teton_protocol::methods::PromptTurnResult, teton_protocol::jsonrpc::RpcError> {
        self.runtime
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
                skill,
                Some(self.connection),
                ClientPresence::unwatched(),
            )
            .await
    }
}

/// Everything a subscription holds right now.
async fn drain(sub: &mut tetond::broadcast::Subscription) -> Vec<Event> {
    let mut out = Vec::new();
    while let Ok(Some(env)) = timeout(Duration::from_millis(100), sub.recv()).await {
        out.push(env.event);
    }
    out
}

/// Every `skill_invoked` in `published`, in order.
fn invocations(published: &[Event]) -> Vec<SkillInvoked> {
    published
        .iter()
        .filter_map(|event| match event {
            Event::SkillInvoked(invoked) => Some(invoked.clone()),
            _ => None,
        })
        .collect()
}

/// Every **tool result** in the conversation the *last* request carried, in
/// order.
///
/// Two decisions, both load-bearing for a chain:
///
/// * the **last** request, not every request. Each remote call re-sends the
///   whole conversation, so a scan across all of them counts one tool result
///   once per call that followed it — a chain of twelve would read as
///   seventy-eight. The last request is the conversation entire, and each
///   result appears in it exactly once.
/// * tool results only. Every claim here is about what came back from a `skill`
///   call, and including the system prompt would put five kilobytes of tool
///   documentation into the message of every failing assertion, where it hides
///   the one line a reader needs.
fn tool_results(vendor: &Vendor) -> Vec<String> {
    last_request_messages(vendor)
        .into_iter()
        .filter(|content| content.starts_with("Tool result ("))
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

// ---------------------------------------------------------------------------
// AC-7 — the chain, and what bounds it (BR-6)
// ---------------------------------------------------------------------------

/// **AC-7: the call past the cap is refused `per_turn_cap`, and the next prompt
/// starts at zero.**
///
/// Thirteen calls in one prompt turn, each with different arguments so BR-6b's
/// repeat rule is not what refuses any of them. The first twelve expand; the
/// thirteenth is refused by name, and the sentence names the cap so the model
/// can say what it hit rather than guess.
///
/// The reset is the second half and is not a separate mechanism: `build_tools`
/// rebuilds the registry — and therefore the tool's `TurnState` — every prompt,
/// so "per prompt" is a property of the shape. A build that hung the counter on
/// the session would refuse the *second* prompt's first call, which is what the
/// second turn below is for.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_thirteenth_call_of_a_turn_is_refused_by_the_cap_and_the_next_prompt_starts_over() {
    let repo = Tree::new("cap");
    let h = Harness::with_window(Some(128_000));
    let session = h.session_at(repo.path());
    let mut sub = h.events.subscribe(512);

    for n in 0..=CAP {
        h.vendor.will_call_skill(Some("step"), &format!("pass {n}"));
    }
    h.turn(&session, "walk the chain", None)
        .await
        .expect("a refused call is a tool result, and the turn goes on");

    let published = invocations(&drain(&mut sub).await);
    let expanded: Vec<&SkillInvoked> = published
        .iter()
        .filter(|invoked| invoked.refused.is_none())
        .collect();
    assert_eq!(
        expanded.len(),
        CAP,
        "exactly {CAP} calls may expand in one prompt turn: {published:?}"
    );
    assert_eq!(
        expanded
            .last()
            .and_then(|invoked| invoked.turn_invocations)
            .map(|t| (t.count, t.cap)),
        Some((CAP as u32, CAP as u32)),
        "the last admitted call is the {CAP}th, and the record says so: {published:?}"
    );

    // **And the cap publishes its own record** (BR-9). The refusal is raised by
    // `TurnState::admit` *before* the call is parsed, so `SkillTool::refuse`
    // reads the subject back through `call_name` — the one parser — rather than
    // off the `Refusal`: a capped call that named a skill still gets the record,
    // carrying the turn count, which for this reason is the evidence for the
    // refusal itself. It counts nothing: the figures above are unchanged.
    let capped: Vec<&SkillInvoked> = published
        .iter()
        .filter(|invoked| invoked.refused.as_deref() == Some("per_turn_cap"))
        .collect();
    assert_eq!(
        capped.len(),
        1,
        "the thirteenth call publishes exactly one refusal record: {published:?}"
    );
    assert_eq!(
        (
            capped[0].name.as_str(),
            capped[0].turn_invocations.map(|t| (t.count, t.cap))
        ),
        ("step", Some((CAP as u32 + 1, CAP as u32))),
        "the record names the skill the call named, and the count that refused \
         it: {published:?}"
    );
    assert!(
        capped[0].outcomes.is_empty(),
        "nothing ran, so the cap refusal carries no dynamic outcomes: \
         {published:?}"
    );

    let refusal = tool_results(&h.vendor)
        .into_iter()
        .find(|content| content.contains("per_turn_cap"))
        .expect("the thirteenth call must be refused by name");
    assert!(
        refusal.contains(&format!("already made {CAP} `skill` calls")),
        "the refusal names the cap the model hit: {refusal}"
    );
    assert!(
        refusal.contains("the count resets with each prompt"),
        "a refusal names what the model or the user can do next: {refusal}"
    );

    // The reset: a *second* prompt in the same session expands again.
    h.vendor.will_call_skill(Some("step"), "a fresh prompt");
    h.turn(&session, "walk it again", None)
        .await
        .expect("the second prompt runs");
    let second = invocations(&drain(&mut sub).await);
    assert_eq!(
        second
            .iter()
            .filter(|invoked| invoked.refused.is_none())
            .count(),
        1,
        "the cap is per prompt turn, not per session: {second:?}"
    );
    assert_eq!(
        second[0].turn_invocations.map(|t| t.count),
        Some(1),
        "the new prompt's first call is its first call: {second:?}"
    );
}

/// **AC-7: a run of listings exhausts the cap.**
///
/// BR-6a counts *every* call — expansion, listing or typed refusal — because a
/// call that cost nothing would make a loop of them unbounded. A listing is the
/// sharpest case: it is not a refusal and it expands nothing, so a build that
/// counted only expansions would let the model list forever, and no other test
/// here would notice.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_run_of_listings_exhausts_the_per_turn_cap() {
    let repo = Tree::new("list");
    let h = Harness::with_window(Some(128_000));
    let session = h.session_at(repo.path());
    let mut sub = h.events.subscribe(512);

    for _ in 0..=CAP {
        h.vendor.will_call_skill(None, "");
    }
    h.turn(&session, "what can you run", None)
        .await
        .expect("the turn goes on");

    let wire = tool_results(&h.vendor);
    assert_eq!(
        wire.iter()
            .filter(|content| content.contains("skills you may invoke:"))
            .count(),
        CAP,
        "exactly {CAP} listings, and then the cap: {wire:#?}"
    );
    assert!(
        wire.iter().any(|content| content.contains("per_turn_cap")),
        "the call past the cap must be refused even though it asked for nothing \
         but the catalogue: {wire:#?}"
    );

    // **And this one publishes nothing, which is the honest answer** (BR-9). A
    // listing names no skill, so the call past the cap names none either — and
    // the record BR-9 asks for describes a skill *file*. There is none here to
    // describe, and a record with an invented `source`, an empty path and a zero
    // size would read on the session surface like a refusal of something real.
    // The refusal is still not silent: the model reads it and relays it, and the
    // session shows the tool call.
    let published = invocations(&drain(&mut sub).await);
    assert!(
        published.is_empty(),
        "a turn of nameless calls describes no skill file, so it publishes no \
         `skill_invoked` at all: {published:?}"
    );
}

/// **AC-7: a refused call never seeds the repeat rule.**
///
/// BR-6b's repeat rule is about what the model *holds*. A call that was refused
/// left it holding nothing, so re-issuing the same name must earn the same
/// refusal again — never `repeated`, which would tell the model it already has
/// an expansion that was never folded.
///
/// The fixture refuses with `unknown_skill` twice and then expands the real
/// name, which covers the AC's second clause in the same turn: a refusal
/// followed by the *first successful* expansion of a name is not a repeat
/// either.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_refused_call_never_seeds_the_repeat_rule() {
    let repo = Tree::new("noseed");
    let h = Harness::with_window(Some(128_000));
    let session = h.session_at(repo.path());

    h.vendor.will_call_skill(Some("zzz"), "same");
    h.vendor.will_call_skill(Some("zzz"), "same");
    h.vendor.will_call_skill(Some("step"), "same");
    h.turn(&session, "try it", None)
        .await
        .expect("the turn goes on");

    let wire = tool_results(&h.vendor);
    assert_eq!(
        wire.iter()
            .filter(|content| content.contains("unknown_skill"))
            .count(),
        2,
        "an unknown name re-issued is unknown again, never `repeated`: {wire:#?}"
    );
    assert!(
        !wire.iter().any(|content| content.contains("repeated:")),
        "a refused call seeded the repeat rule: {wire:#?}"
    );
    assert!(
        wire.iter().any(|content| content.contains("Step body.")),
        "the first successful expansion after a refusal must land: {wire:#?}"
    );
}

/// **AC-7: a body that names itself stops at `repeated`, not at the cap.**
///
/// Recursion is real now — `/proceed` invokes `/validate`, `/sprint` invokes
/// `/proceed`, and a skill can name itself — and the bound that catches the
/// degenerate case is the *repeat* rule rather than the cap. A model that
/// obeyed this body literally would issue the same `(name, arguments)` pair
/// back to back, and BR-6b refuses the second one: two expansions, not twelve.
///
/// Asserted as a **count**, because "it stopped at the repeat" and "it stopped
/// at the cap" both end the turn and only the numbers tell them apart.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_body_that_names_itself_stops_at_the_repeat_refusal_not_at_the_cap() {
    let repo = Tree::new("selfnm");
    let h = Harness::with_window(Some(128_000));
    let session = h.session_at(repo.path());
    let mut sub = h.events.subscribe(512);

    // A model doing exactly what the body says: the same call, three times.
    // Three rather than thirteen, because the claim is *where it stops*: the
    // repeat rule fires on the second, and a script long enough to also reach
    // the cap would prove only that BR-6a counts refusals (which
    // `a_run_of_listings_exhausts_the_per_turn_cap` is for).
    for _ in 0..3 {
        h.vendor.will_call_skill(Some("selfnamed"), "");
    }
    h.turn(&session, "follow the self-naming skill", None)
        .await
        .expect("the turn goes on");

    let published = invocations(&drain(&mut sub).await);
    assert_eq!(
        published
            .iter()
            .filter(|invoked| invoked.refused.is_none())
            .count(),
        1,
        "one expansion, and then the repeat rule — a chain that reached the cap \
         folded the same body twelve times: {published:?}"
    );
    let wire = tool_results(&h.vendor);
    assert_eq!(
        wire.iter()
            .filter(|content| content.contains("repeated:"))
            .count(),
        2,
        "every identical call after the first must be refused `repeated`: \
         {wire:#?}"
    );
    assert!(
        !wire.iter().any(|content| content.contains("per_turn_cap")),
        "the cap is not what stopped a self-naming body — the repeat rule is, \
         and it fires eleven calls earlier: {wire:#?}"
    );
}

// ---------------------------------------------------------------------------
// BR-9 — a typed refusal is never silent on the session surface
// ---------------------------------------------------------------------------

/// **BR-9: a typed refusal the *tool* raises publishes its own record, and the
/// one that names no registered skill publishes none.**
///
/// The Events table says a model invocation refused *for any typed reason* gets
/// a record, and the client has a rendered sentence for every one of the seven
/// (`session_ui::refusal_reason_words`). Only the loop's two `over_budget`
/// refusals ever reached it: everything through `Refusal::into_outcome` was
/// silent, so a user watching a session saw a `skill <name> [failed]` tool line
/// and was never told which call it was or why. TASK-222 recorded that gap;
/// this is the fix, driven from the loop rather than from `SkillTool::invoke`,
/// because what is under test is that the *published* record reaches the bus.
///
/// Five calls, four of them refused, chosen so each exercises a different way
/// `registered_row` has to find the file a refusal is about:
///
/// * `step` twice — `repeated`, over a row the model's own resolver returns;
/// * `zzz` — `unknown_skill`, over a name **no** row carries, which is the one
///   reason with a skill named in it and no skill to name. `SkillInvoked`
///   requires a `source`, a `path_display` and a `body_bytes`, and
///   `SkillSource` is a closed two-variant enum, so a record here would have to
///   choose a root the file was never found under. It publishes none;
/// * `hidden` — `not_model_invocable`, a registered row `resolve_for_model`
///   refuses **by design**, which is why the record's lookup is not that
///   resolver;
/// * `cost` — `reserved_name`, a registered row that is not dispatchable at
///   all, which is `registered_row`'s other branch.
///
/// The negatives are as load-bearing as the positives: a refusal record must
/// carry **no** outcomes and must say it was refused, or it is byte-identical to
/// a command-free skill that ran and the session prints a refusal as a success
/// (that is what the `refused` field is for, and what TASK-219's renderer keys
/// on). The `hidden` fixture carries a `` !`echo` `` slot nothing here ever
/// runs, so "no outcomes" is a claim a build that projected the *file's* slots
/// onto the record would fail rather than one no mutation can reach.
///
/// The other two reasons are elsewhere, because they need a different script:
/// `per_turn_cap` in
/// [`the_thirteenth_call_of_a_turn_is_refused_by_the_cap_and_the_next_prompt_starts_over`]
/// (it publishes, naming the skill the capped call named) and
/// [`a_run_of_listings_exhausts_the_per_turn_cap`] (a capped call that named
/// nothing publishes nothing). `invalid_arguments` needs a malformed call the
/// scripting verb here cannot compose; it takes the same door and the same
/// `call_name` lookup, which for a failed parse yields `None`.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn every_tool_raised_refusal_over_a_registered_skill_publishes_a_record() {
    let repo = Tree::new("refrec");
    let h = Harness::with_window(Some(128_000));
    let session = h.session_at(repo.path());
    let mut sub = h.events.subscribe(512);

    h.vendor.will_call_skill(Some("step"), "once");
    h.vendor.will_call_skill(Some("step"), "once");
    h.vendor.will_call_skill(Some("zzz"), "");
    h.vendor.will_call_skill(Some("hidden"), "");
    h.vendor.will_call_skill(Some("cost"), "");
    h.turn(&session, "try every door", None)
        .await
        .expect("a refused call is a tool result, and the turn goes on");

    let published = invocations(&drain(&mut sub).await);
    let refusals: Vec<&SkillInvoked> = published
        .iter()
        .filter(|invoked| invoked.refused.is_some())
        .collect();
    let reasons: Vec<(&str, &str)> = refusals
        .iter()
        .map(|invoked| {
            (
                invoked.name.as_str(),
                invoked.refused.as_deref().unwrap_or_default(),
            )
        })
        .collect();
    assert_eq!(
        reasons,
        vec![
            ("step", "repeated"),
            ("hidden", "not_model_invocable"),
            ("cost", "reserved_name"),
        ],
        "each typed refusal over a registered row publishes one record, in the \
         order the calls were made: {published:?}"
    );

    // The one with nothing to name publishes nothing — not a hollow record with
    // an invented source.
    assert!(
        published.iter().all(|invoked| invoked.name != "zzz"),
        "`unknown_skill` names a skill this session does not have, so there is \
         no file to describe and no record to publish: {published:?}"
    );

    // A refusal record is not an invocation record.
    for invoked in &refusals {
        assert!(
            invoked.outcomes.is_empty(),
            "nothing ran, so a refusal carries no dynamic outcomes: {invoked:?}"
        );
        assert_eq!(
            invoked.invoked_by,
            teton_protocol::events::InvokedBy::Model,
            "every refusal here is the model's call: {invoked:?}"
        );
    }

    // BR-6a's count is untouched: `admit` counted every one of these five calls
    // on the way in, and publishing a record counts nothing a second time.
    assert_eq!(
        published
            .last()
            .and_then(|invoked| invoked.turn_invocations)
            .map(|t| t.count),
        Some(5),
        "the five calls of this turn are five, not ten: publishing a refusal \
         must not count it again: {published:?}"
    );

    // Non-vacuity: the one call that was *not* refused expanded, so this is a
    // turn in which the tool worked rather than one in which it was never
    // reached.
    assert_eq!(
        published
            .iter()
            .filter(|invoked| invoked.refused.is_none())
            .count(),
        1,
        "exactly one of the five calls expands: {published:?}"
    );
    let wire = tool_results(&h.vendor);
    for reason in [
        "repeated",
        "unknown_skill",
        "not_model_invocable",
        "reserved_name",
    ] {
        let head = format!("ERROR: {reason}:");
        assert!(
            wire.iter().any(|content| content.contains(&head)),
            "the model's half of the refusal is unchanged — `{reason}` is \
             missing from the wire: {wire:#?}"
        );
    }
}

// ---------------------------------------------------------------------------
// AC-8 — carried whole, or refused (BR-7)
// ---------------------------------------------------------------------------

/// **AC-8: a 7,222-word expansion of `/proceed`'s measured shape enters a
/// declared 128k route whole.**
///
/// Whole means all three markers: the head, the middle and the tail. A middle
/// elision takes the marker in the centre and leaves the two ends, so the
/// middle is the assertion that separates "arrived" from "arrived intact" — the
/// one a mechanical truncation passes without.
///
/// No `context_pressure` of any kind fired, which is the other half: an
/// expansion this size on a window this large is ordinary, and a route that
/// announced pressure here would be announcing it for every `/proceed`.
///
/// **This is not the `digest`-bypass test, and saying so is the point.** On a
/// declared 128k window the `digest` threshold scales with the budget
/// (`budget::digest_thresholds`) to well past anything a fixture can write, so
/// `summarize_if_large` is never reached here and a build with the bypass
/// deleted passes this — verified, not assumed. AC-8 puts that claim on the
/// **default budget** route, where the threshold sits at its 1,500-word default
/// and the fold would bite, and that is where it lives:
/// `skill_turn.rs::an_expansion_past_the_digest_threshold_is_folded_whole_where_an_ordinary_result_is_not`.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_seven_thousand_word_expansion_enters_a_128k_route_whole_and_unelided() {
    let repo = Tree::new("whole");
    let h = Harness::with_window(Some(128_000));
    let session = h.session_at(repo.path());
    let mut sub = h.events.subscribe(512);

    h.vendor.will_call_skill(Some("proceedish"), "REQ-587");
    h.turn(&session, "run the orchestrator", None)
        .await
        .expect("a 7,222-word expansion fits a 128k window");

    let wire = h.vendor.on_the_wire();
    for marker in ["PROCEED-HEAD", "PROCEED-MIDDLE", "PROCEED-TAIL"] {
        assert!(
            wire.contains(marker),
            "`{marker}` is missing from what the provider received, so the \
             expansion was condensed or elided rather than carried whole"
        );
    }
    let published = drain(&mut sub).await;
    assert!(
        !published
            .iter()
            .any(|event| matches!(event, Event::ContextPressure(_))),
        "nothing about a 7,222-word body on a 128k window is pressure: \
         {published:?}"
    );
    // Non-vacuity: the fixture really is the size it claims to be.
    let body = std::fs::read_to_string(fixture_home().join(".claude/skills/proceedish/SKILL.md"))
        .expect("the fixture is on disk");
    assert!(
        body.split_whitespace().count() >= PROCEED_SHAPED_WORDS,
        "the fixture must actually be {PROCEED_SHAPED_WORDS} words, or this \
         test is about a short string"
    );
}

/// **AC-8: the same fixture on a route that declared no window is refused in
/// the bound's spoken form, with the remedy.**
///
/// `max_context` unset takes `budget::derive`'s `DefaultUnknown` arm, whose
/// pair is the default 4,096 words / 32 KiB — smaller than this body — so the
/// same file that sailed through a declared window is refused here. What the
/// refusal must say is BR-7's: `bound: unknown window`, which is
/// `BudgetBound::words()`, and **not** `default_unknown`, which is
/// `wire_name()` and is a token nobody reads aloud. It must also name the
/// provider whose `capabilities.max_context` is the line a new user would go
/// and write.
///
/// The refusal is a **tool result**, not a turn-ender: the model is handed a
/// sentence it can relay, and the turn goes on.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_same_fixture_on_an_undeclared_window_is_refused_in_the_bounds_spoken_form() {
    let repo = Tree::new("nowin");
    let h = Harness::with_window(None);
    let session = h.session_at(repo.path());

    h.vendor.will_call_skill(Some("proceedish"), "REQ-587");
    h.turn(&session, "run the orchestrator", None)
        .await
        .expect("a refused expansion is a tool result, and the turn goes on");

    let refusal = tool_results(&h.vendor)
        .into_iter()
        .find(|content| content.contains("does not fit this route's context budget"))
        .expect("the expansion must be refused on the default pair");
    assert!(
        refusal.contains("bound: unknown window"),
        "the bound is spoken, never spelled in its wire token: {refusal}"
    );
    assert!(
        !refusal.contains("default_unknown"),
        "`wire_name()` reached a sentence a person reads: {refusal}"
    );
    assert!(
        refusal.contains("set `capabilities.max_context` for `mock`"),
        "the `unknown window` arm carries the remedy and names the provider: \
         {refusal}"
    );
    assert!(
        refusal.contains("`proceedish`"),
        "the refusal names the skill, its size and the budget: {refusal}"
    );
    assert!(
        !h.vendor.on_the_wire().contains("PROCEED-MIDDLE"),
        "a refused expansion enters nothing — never a shortened version of \
         itself"
    );
}
