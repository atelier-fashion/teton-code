//! REQ-623 TASK-430 — the `agent` tool's acceptance suite, through the daemon.
//!
//! Every test here but one spawns the **real** `teton-code` binary, drives it
//! over its socket as a protocol client, and answers its remote calls from a
//! matching [`MockProvider`] (REQ-623 ADR-6): a request is answered by **what
//! it says** — a child's own request opens with its `task` as the only user
//! message, the parent's carries the parent's prompt — so concurrent children
//! are addressable however the scheduler interleaves them. A [`Rendezvous`]
//! holds a reply until enough requests have parked on it (AC-5), or until the
//! test opens it after watching the daemon do something ([`Rendezvous::gate`]).
//!
//! Child events interleave nondeterministically (LESSON-591), so nothing here
//! pins an order **across** children: a child's events are read by its
//! `child_id`, and an ordering claim is either within one child or against a
//! parent-anchored event (`agent_call_started`, `agent_call_finished`).
//!
//! The one exception to "through the daemon" is [`statuses::cancelled`]: the
//! daemon has no RPC that cancels a running prompt — a departing client's turn
//! *drains* (REQ-565), and the only abort is `TURN_DRAIN_TIMEOUT`'s, 300 s
//! later — so that test drives the daemon's own `DaemonRuntime` in-process,
//! with the real child runner and a real HTTP vendor, and drops the parent
//! turn's future the way that abort does. Its doc comment says so again.
//!
//! | rule | claim | test |
//! |---|---|---|
//! | AC-5 | three children rendezvous; every start precedes every finish | [`concurrency::three_children_rendezvous`] |
//! | AC-6 | a child's `tool_call` reaches a subscriber while its tool is parked | [`liveness::child_tool_started_arrives_while_parked`] |
//! | BR-4 | a second prompt is refused busy while the call holds the claim | [`liveness::second_prompt_refused_busy_during_call`] |
//! | AC-7 | a guarded child asks, labelled; the sibling reuses the grant | [`consent::guarded_child_asks_and_sibling_reuses_grant`] |
//! | AC-8 | at `plan`, a child's `edit` denies exactly as the parent's | [`consent::plan_child_edit_denies`] |
//! | BR-5 | an unattended answer denies typed; no consent is invented | [`consent::unattended_unlisted_gate_denies_typed`] |
//! | AC-9 | `tier: build` under a Think parent; no binding → category default | [`routing::build_child_under_think_parent`] |
//! | AC-13, BR-8 | shares and release derived from the ledger | [`spend::two_child_split_and_three_child_release`] |
//! | AC-14, BR-10 | one test per terminal status, the parent continuing in each | `statuses::*` |
//! | AC-16, BR-12 | a user skill in a child pins the child and the parent | [`skills::user_skill_in_child_pins_parent`] |
//! | AC-17 | one transcript file holds the parent's and every child's records | [`transcript::one_file_parent_and_children`] |
//!
//! AC-10's egress capture lives beside the other boundary tests, in
//! `provenance_egress.rs`; BR-13's golden sequence in
//! `event_response_ordering.rs`.
//!
//! # Fixture rules
//!
//! - **The leak marker lives only in `secrets/prod.env`** (LESSON-624). Task
//!   strings, prompts and tool arguments here are ordinary prose that reaches
//!   the wire legitimately; [`assert_no_boundary_bytes`] runs at the end of
//!   every test.
//! - **Files a child writes are named for the child** (LESSON-610/611), so two
//!   siblings can never be satisfied by each other's side effect.
//! - **A shell result is asserted by its frame** — `(exit N)` — never by what a
//!   platform's `/bin/sh` printed (conventions: "assert the frame the product
//!   owns").
//! - **A fixture sized by arithmetic asserts the arithmetic** (LESSON-640):
//!   every spend test checks, from the ledger, that the call it relies on
//!   really was past the line it was sized to cross.
//!
//! # Mutation record
//!
//! Recorded on each test's own doc comment (conventions: run the inversion on
//! every test in the batch and count the reds). Each mutation was applied to
//! the daemon source, the binaries rebuilt, this binary plus
//! `provenance_egress` and `event_response_ordering` run (47 tests), and the
//! mutation reverted by a targeted edit. A count is over those 47.
//!
//! Every test here went red under at least one mutation of the behaviour it
//! guards. Two findings the batch produced:
//!
//! - **Two daemon bugs, fixed in their own commits.** A child refused by a
//!   project-skill gate ended `completed` (found by [`statuses::refused`]);
//!   and a child stamped its spend ceiling *at stamping time* rather than its
//!   initial share, so a sibling's earlier release leaked into
//!   `agent_child_started` (found by [`spend::two_child_split_and_three_child_release`]
//!   under the sequential-children mutation).
//! - **BUG-226's own mutation does not redden the parked verifier.**
//!   Dispatching a child's tool inline (no `block_in_place_if_multithread`)
//!   left [`liveness::child_tool_started_arrives_while_parked`] green in 8 of 8
//!   runs: the LIFO-slot starvation that bug recorded does not reproduce inside
//!   a child's own task in this build. Blocking the *parent's* worker on the
//!   call does redden it — see that test.

#[path = "e2e/harness.rs"]
mod harness;

use std::time::{Duration, Instant};

use serde_json::{json, Value};

use harness::{
    agent_table, assert_no_boundary_bytes, cost_table, openai_turn,
    remote_provider_block_with_window, tier_block, Client, Daemon, DaemonOptions, Matcher,
    MockProvider, MockResponse, Rendezvous, Workspace,
};

// ---------------------------------------------------------------------------
// The shared fixture
// ---------------------------------------------------------------------------

/// How long anything here is waited for: generous for a loaded runner, finite
/// so that nothing passes by waiting.
const WINDOW: Duration = Duration::from_secs(15);

/// The remote provider's id in every config here.
const REMOTE: &str = "remote";

/// A model the bundled price table prices, so a spend ceiling can count it
/// (REQ-588 ADR-3 refuses an unpriced call outright under a ceiling).
const MODEL: &str = "deepseek-v4-flash";

/// `SESSION_BUSY` (`teton_protocol::jsonrpc::error_code`), spelled here so the
/// assertion reads the wire number the client receives.
const SESSION_BUSY: i64 = -32008;

/// `SPEND_CEILING_REACHED` — REQ-588's typed outcome on the wire.
const SPEND_CEILING_REACHED: i64 = -32024;

/// The config every daemon here starts from: one remote provider with a
/// declared 128k window (so the parent's budget holds what a test hands its
/// children) bound to `build` — the tier an `implement` session's `edit`
/// category inherits.
fn base_config(provider: &MockProvider) -> String {
    let mut config =
        remote_provider_block_with_window(REMOTE, &provider.openai_endpoint(), MODEL, 128_000);
    config.push_str(&tier_block("build", REMOTE));
    config
}

/// No builtin boundary globs (REQ-597). For the tests whose subject is not
/// privacy and whose children run `shell`: an unproven `shell` result carries
/// `unknown` provenance, which under the shipped globs pins the next call to a
/// local tier these daemons do not have (REQ-614).
const NO_DEFAULT_BOUNDARIES: &str = "[privacy]\ndisable_default_boundaries = true\n\n";

/// One spawned daemon, one client, one `implement` session.
///
/// Field order is drop order: the client disconnects, then the daemon is
/// killed, then the workspace is removed.
struct Rig {
    client: Client,
    _daemon: Daemon,
    ws: Workspace,
    session: String,
}

impl Rig {
    /// A daemon on [`base_config`] plus `extra`.
    fn start(tag: &str, provider: &MockProvider, extra: &str) -> Self {
        let mut config = base_config(provider);
        config.push_str(extra);
        Self::with_config(tag, &config, |_| DaemonOptions::default())
    }

    /// A daemon on exactly `config`, after `plant` has laid anything extra
    /// into the workspace (a project skill, a user skill's home, a config that
    /// names a path inside the workspace) and chosen the daemon's options.
    fn with_config(
        tag: &str,
        config: &str,
        plant: impl FnOnce(&Workspace) -> DaemonOptions,
    ) -> Self {
        let ws = Workspace::new(tag);
        ws.write_config(config);
        let options = plant(&ws);
        let daemon = Daemon::spawn(&ws, options);
        let mut client = daemon.connect();
        let session = client.create_session("structured", Some("implement"));
        Self {
            client,
            _daemon: daemon,
            ws,
            session,
        }
    }

    /// The same, with a client that answers no permission prompt on its own.
    fn without_auto_approve(mut self) -> Self {
        self.client = self.client.without_auto_approve();
        self
    }

    /// Move the session to `level` and check it took.
    fn level(&mut self, level: &str) {
        let set = self.client.call(
            "session/permissions",
            json!({ "session_id": self.session, "level": level }),
        );
        assert_eq!(set["result"]["level"], json!(level), "{set}");
    }

    /// Send the parent's prompt and wait for its response.
    fn prompt(&mut self, text: &str) -> Value {
        let response = self.client.prompt(&self.session, text);
        self.client.drain_events(Duration::from_millis(200));
        response
    }
}

/// Matches a **child's** own request: its only user message opens with its
/// `task`. The parent's follow-up quotes the task too — inside its tool call's
/// escaped arguments — but never as the start of a message's content, so this
/// needle tells the two apart without depending on arrival order.
fn child(task: &str) -> Matcher {
    Matcher::body_contains(format!("\"content\":\"{task}"))
}

/// Matches the parent's requests: every one carries the parent's prompt, and
/// no child's does (BR-1: a child sees nothing of the conversation).
fn parent(marker: &str) -> Matcher {
    Matcher::body_contains(marker)
}

/// A reply that calls `tool` with `args`, billed 100 input / 10 output tokens.
fn calls(tool: &str, args: &Value) -> MockResponse {
    calls_billing(tool, args, 100, 10)
}

/// A reply that calls `tool`, billed as the test says.
fn calls_billing(tool: &str, args: &Value, input: u64, output: u64) -> MockResponse {
    MockResponse::ok(openai_turn(
        "",
        Some(("mock-call", tool, &args.to_string())),
        input,
        output,
    ))
}

/// A reply that ends its turn with `text`, billed 100 / 10.
fn says(text: &str) -> MockResponse {
    says_billing(text, 100, 10)
}

/// A reply that ends its turn with `text`, billed as the test says.
fn says_billing(text: &str, input: u64, output: u64) -> MockResponse {
    MockResponse::ok(openai_turn(text, None, input, output))
}

/// The parent's `agent` call over `tasks`.
fn dispatch(tasks: Value) -> MockResponse {
    calls("agent", &json!({ "tasks": tasks }))
}

/// What a request with no matching entry gets: a plain end of turn that names
/// itself, so a test that fell through to it says so in its failure.
fn unmatched() -> MockResponse {
    says("UNMATCHED-REQUEST")
}

/// Every captured request body, as text.
fn bodies(provider: &MockProvider) -> Vec<String> {
    provider
        .requests()
        .iter()
        .map(|b| String::from_utf8_lossy(b).into_owned())
        .collect()
}

/// The child's own requests (see [`child`]).
fn child_requests(provider: &MockProvider, task: &str) -> Vec<String> {
    let needle = format!("\"content\":\"{task}");
    bodies(provider)
        .into_iter()
        .filter(|b| b.contains(&needle))
        .collect()
}

/// The parent's requests (see [`parent`]).
fn parent_requests(provider: &MockProvider, marker: &str) -> Vec<String> {
    bodies(provider)
        .into_iter()
        .filter(|b| b.contains(marker))
        .collect()
}

/// The last message's content in a captured request — what the model was
/// handed most recently (a tool result, on every follow-up).
fn last_message(body: &str) -> String {
    let parsed: Value = serde_json::from_str(body).expect("a request body is JSON");
    parsed["messages"]
        .as_array()
        .and_then(|m| m.last())
        .and_then(|m| m["content"].as_str())
        .unwrap_or_default()
        .to_owned()
}

/// The `ChildResult` array inside an `agent` tool result the model was handed.
fn results_in(message: &str) -> Option<Vec<Value>> {
    if !message.starts_with("Tool result (agent):") {
        return None;
    }
    let open = "trust=\"untrusted\">";
    let start = message.find(open)? + open.len();
    let end = message.find("</tool-result>")?;
    serde_json::from_str::<Vec<Value>>(message[start..end].trim()).ok()
}

/// What the parent's model was handed for its `agent` call: the results in the
/// **first** parent request that carries them — the parent's next model call
/// after the call returned (BR-10: the parent always gets a result).
fn handed(provider: &MockProvider, marker: &str) -> Vec<Value> {
    parent_requests(provider, marker)
        .iter()
        .find_map(|body| results_in(&last_message(body)))
        .unwrap_or_else(|| {
            panic!(
                "no parent request after the call carried the `agent` result: {:#?}",
                parent_requests(provider, marker)
                    .iter()
                    .map(|b| last_message(b).chars().take(200).collect::<String>())
                    .collect::<Vec<_>>()
            )
        })
}

/// The one result named `name`.
fn result_named<'a>(results: &'a [Value], name: &str) -> &'a Value {
    results
        .iter()
        .find(|r| r["name"] == name)
        .unwrap_or_else(|| panic!("no result named {name}: {results:#?}"))
}

/// The id `agent_child_started` gave the child named `name`.
fn child_id_of(client: &Client, name: &str) -> String {
    client
        .events_named("agent_child_started")
        .iter()
        .find(|e| e["name"] == name)
        .and_then(|e| e["child_id"].as_str())
        .unwrap_or_else(|| {
            panic!(
                "no agent_child_started for {name}: {:?}",
                client.event_names()
            )
        })
        .to_owned()
}

/// The status `agent_child_finished` reported for `child_id`.
fn finished_status(client: &Client, child_id: &str) -> String {
    let finished = client
        .events_named("agent_child_finished")
        .into_iter()
        .filter(|e| e["child_id"] == child_id)
        .collect::<Vec<_>>();
    assert_eq!(
        finished.len(),
        1,
        "exactly one finish for {child_id}: {:?}",
        client.event_names()
    );
    finished[0]["status"]
        .as_str()
        .unwrap_or_default()
        .to_owned()
}

/// The index of the first event satisfying `pred`.
fn index_where(client: &Client, pred: impl Fn(&Value) -> bool) -> Option<usize> {
    client.event_index_from(0, pred)
}

/// Whether `event` is child-scoped to `child_id`: a `session_update`,
/// `permission_request` or `context_pressure` stamped with it, or a
/// `cost_recorded` whose record names it.
fn scoped_to(event: &Value, child_id: &str) -> bool {
    event["child_id"] == child_id || event["record"]["child_id"] == child_id
}

/// Poll for `path` to exist, up to [`WINDOW`].
fn await_file(path: &std::path::Path) -> bool {
    let deadline = Instant::now() + WINDOW;
    while Instant::now() < deadline {
        if path.exists() {
            return true;
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    false
}

/// A `shell` command that marks itself started, then parks until `release`
/// exists — a tool provably in flight for exactly as long as the test wants.
fn parked_shell(started: &str, release: &str) -> Value {
    json!({
        "command": format!(
            "touch {started} && while [ ! -f {release} ]; do sleep 0.05; done && echo released"
        )
    })
}

// ---------------------------------------------------------------------------
// AC-5 — concurrency, proven by a rendezvous
// ---------------------------------------------------------------------------

mod concurrency {
    use super::*;

    /// **AC-5 (BR-4): three children in one call run concurrently.** Each
    /// child's first request is held by a `Rendezvous` of three: no reply is
    /// written until all three requests are parked on it. A daemon that ran
    /// the children one after another would park the first child forever —
    /// the second never asks — and the prompt would never answer. Concurrency
    /// is proven by the rendezvous releasing, not by a wall-clock bound.
    ///
    /// Then the event order the spec names: all three `agent_child_started`
    /// precede every `agent_child_finished`, and the parent's next request
    /// carries all three reports.
    ///
    /// # Mutation (run 2026-10-07, reverted)
    ///
    /// - **Children run one at a time** (`flight.land().await` moved inside
    ///   `run_children`'s launch loop): 3 red — this test (the first child
    ///   parks on the rendezvous and the prompt never answers), the AC-7
    ///   consent test (the sibling never reaches the gate while the first ask
    ///   is open) and [`statuses::cancelled`] (the second child never parks).
    #[test]
    fn three_children_rendezvous() {
        let rv = Rendezvous::new(3);
        let tasks = json!([
            { "task": "AC5-ALPHA summarise the README", "name": "alpha" },
            { "task": "AC5-BETA describe Cargo.toml", "name": "beta" },
            { "task": "AC5-GAMMA describe src/lib.rs", "name": "gamma" },
        ]);
        let provider = MockProvider::start_matching(
            vec![
                (child("AC5-ALPHA"), rv.hold(says("alpha report"))),
                (child("AC5-BETA"), rv.hold(says("beta report"))),
                (child("AC5-GAMMA"), rv.hold(says("gamma report"))),
                (parent("AC5-PARENT"), dispatch(tasks)),
                (parent("AC5-PARENT"), says("Three reports in.")),
            ],
            unmatched(),
        );
        let mut rig = Rig::start("ac5", &provider, "");

        let response = rig.prompt("AC5-PARENT survey the repository with three children");
        assert_eq!(
            response["result"]["stop_reason"], "end_turn",
            "a call whose children rendezvous completes: {response}"
        );
        assert!(
            rv.is_released() && rv.arrived() == 3,
            "all three children's first requests were parked at once ({} arrived)",
            rv.arrived()
        );

        let client = &rig.client;
        let starts: Vec<usize> = ["alpha", "beta", "gamma"]
            .iter()
            .map(|name| {
                index_where(client, |e| {
                    e["event"] == "agent_child_started" && e["name"] == *name
                })
                .unwrap_or_else(|| panic!("{name} started: {:?}", client.event_names()))
            })
            .collect();
        let finishes: Vec<usize> = client
            .events()
            .iter()
            .enumerate()
            .filter(|(_, e)| e["event"] == "agent_child_finished")
            .map(|(i, _)| i)
            .collect();
        assert_eq!(finishes.len(), 3, "{:?}", client.event_names());
        let last_start = starts.iter().max().copied().unwrap_or_default();
        let first_finish = finishes.iter().min().copied().unwrap_or_default();
        assert!(
            last_start < first_finish,
            "every agent_child_started precedes every agent_child_finished: {:?}",
            client.event_names()
        );

        let results = handed(&provider, "AC5-PARENT");
        for name in ["alpha", "beta", "gamma"] {
            let result = result_named(&results, name);
            assert_eq!(result["status"], "completed", "{result}");
            assert_eq!(result["report"], format!("{name} report"));
        }
        assert_eq!(
            provider.request_count(),
            5,
            "two parent requests, three children"
        );
        assert_no_boundary_bytes();
    }
}

// ---------------------------------------------------------------------------
// AC-6 / BR-4 — the parent's claim and its forwarder while children run
// ---------------------------------------------------------------------------

mod liveness {
    use super::*;

    /// The shared shape: one child whose `shell` call parks until the test
    /// releases it, under `full` (consent is not the subject here) and with no
    /// builtin boundary globs (see [`NO_DEFAULT_BOUNDARIES`]).
    fn parked_rig(tag: &str, marker: &str, task: &str, name: &str) -> (MockProvider, Rig) {
        let started = format!("parked-{name}.started");
        let release = format!("parked-{name}.release");
        let provider = MockProvider::start_matching(
            vec![
                (
                    child(task),
                    calls("shell", &parked_shell(&started, &release)),
                ),
                (child(task), says(&format!("{name} finished"))),
                (
                    parent(marker),
                    dispatch(
                        json!([{ "task": format!("{task} wait for the release"), "name": name }]),
                    ),
                ),
                (parent(marker), says("The child is done.")),
            ],
            unmatched(),
        );
        let mut rig = Rig::start(tag, &provider, NO_DEFAULT_BOUNDARIES);
        rig.level("full");
        (provider, rig)
    }

    /// **AC-6 (BR-4, BR-13; LESSON-518, BUG-226): a child's `tool_call`
    /// reaches a subscribed client while that child's tool is still parked.**
    ///
    /// The parked verifier. The child's `shell` touches a `started` file and
    /// then waits on a `release` file only this test creates; the test sees
    /// `started` (so the tool is provably running), and only then — with the
    /// tool still parked and nothing to release it but the test — waits for
    /// the child-stamped `tool_call` on its own connection. A forwarder that
    /// delivered child events in a burst when the call finished, or a child
    /// tool that sat on the worker the forwarder needs (BUG-226's shape at N×
    /// the duration), cannot deliver it here at all: the call cannot finish
    /// until the event has arrived. The daemon runs a multi-thread runtime,
    /// which is the runtime the BUG-226 starvation needs (LESSON-518).
    ///
    /// # Mutation (run 2026-10-07, reverted)
    ///
    /// - **The parent's worker blocked on the call** (the loop's `as_agent` arm
    ///   `futures::executor::block_on` instead of `.await` — BUG-226's shape at
    ///   N× the duration): red here, among 14 of this binary's 18; the two
    ///   daemon-spawning AC-10 tests went red too, and the in-process
    ///   `provenance_egress` agent test deadlocked on its current-thread
    ///   runtime and was killed.
    /// - **Stamp nothing** (`SessionEvents::for_child` returns the parent's
    ///   emitter): 5 red, this test among them — the event arrives, unstamped.
    /// - **The child's tool dispatched inline** (`run_the_allowed_tool`
    ///   calling `tools.dispatch` without `block_in_place_if_multithread`,
    ///   BUG-226's own fix removed): **0 red, in 8 of 8 runs of this test.** A
    ///   finding, not a pass: the LIFO-slot starvation BUG-226 recorded does
    ///   not reproduce inside a child's own task in this build, so this
    ///   verifier guards the parent-blocking shape and not that one.
    #[test]
    fn child_tool_started_arrives_while_parked() {
        let (_provider, mut rig) = parked_rig("ac6", "AC6-PARENT", "AC6-WATCHED", "watched");
        let started = rig.ws.repo.join("parked-watched.started");
        let release = rig.ws.repo.join("parked-watched.release");
        let id = rig
            .client
            .prompt_no_wait(&rig.session, "AC6-PARENT run one child that waits");

        assert!(
            await_file(&started),
            "the child's shell never started — the fixture never parked"
        );
        let seen = rig.client.wait_for_event_where(
            "session_update",
            |e| e["child_id"].is_string() && e["update"]["kind"] == "tool_call",
            WINDOW,
        );
        // Measured while parked: nothing but this test can create `release`.
        let still_parked = !release.exists() && !rig.client.saw_event("agent_call_finished");
        std::fs::write(&release, "").expect("release the parked tool");
        let seen = seen.unwrap_or_else(|| {
            panic!(
                "the child's tool_call did not reach the subscriber while its tool was \
                 parked: {:?}",
                rig.client.event_names()
            )
        });
        assert!(still_parked, "the event must arrive before the release");
        assert_eq!(seen["parent_turn_id"], "turn-0", "{seen}");
        assert!(
            seen["update"]["title"]
                .as_str()
                .is_some_and(|t| t.starts_with("shell")),
            "{seen}"
        );
        assert!(
            !rig.client
                .events()
                .iter()
                .any(|e| e["event"] == "agent_child_finished"),
            "nothing had finished when the event arrived"
        );

        let response = rig.client.await_response(id);
        assert_eq!(response["result"]["stop_reason"], "end_turn", "{response}");
        let child_id = child_id_of(&rig.client, "watched");
        assert_eq!(seen["child_id"], json!(child_id));
        assert_eq!(finished_status(&rig.client, &child_id), "completed");
        assert_no_boundary_bytes();
    }

    /// **BR-4 (LESSON-539): while an `agent` call runs, the parent's claim is
    /// held, and a second prompt on the session is refused busy** — exactly as
    /// during any tool call.
    ///
    /// The call is held open by a child parked in `shell`; the second prompt is
    /// sent from a second connection attached to nothing — it names the
    /// session, which is all a prompt needs. Benign path: once the call
    /// returns, the claim is released and a third prompt on the same session
    /// runs.
    ///
    /// # Mutation (run 2026-10-07, reverted)
    ///
    /// - **The claim released at once** (`run_prompt_turn` destructuring the
    ///   claim as `_`, so it drops before the attempt): 1 red, this test — the
    ///   second prompt runs.
    #[test]
    fn second_prompt_refused_busy_during_call() {
        let (provider, mut rig) = parked_rig("br4", "BR4-PARENT", "BR4-HOLDER", "holder");
        let started = rig.ws.repo.join("parked-holder.started");
        let release = rig.ws.repo.join("parked-holder.release");
        let id = rig
            .client
            .prompt_no_wait(&rig.session, "BR4-PARENT run one child that waits");
        assert!(await_file(&started), "the child's shell never started");

        let second = rig.client.call(
            "session/prompt",
            json!({
                "session_id": rig.session,
                "prompt": [{ "type": "text", "text": "BR4-SECOND are you free?" }],
            }),
        );
        let refused_while_parked = !release.exists();
        std::fs::write(&release, "").expect("release the parked tool");
        assert_eq!(
            second["error"]["code"].as_i64(),
            Some(SESSION_BUSY),
            "a prompt during the call is refused busy: {second}"
        );
        assert!(refused_while_parked);
        assert!(
            !bodies(&provider).iter().any(|b| b.contains("BR4-SECOND")),
            "the refused prompt sent nothing"
        );

        let first = rig.client.await_response(id);
        assert_eq!(first["result"]["stop_reason"], "end_turn", "{first}");

        // Benign: the claim is released with the call's turn.
        let third = rig.client.prompt(&rig.session, "BR4-THIRD now?");
        assert_eq!(
            third["result"]["stop_reason"], "end_turn",
            "after the call the session is free: {third}"
        );
        assert_no_boundary_bytes();
    }
}

// ---------------------------------------------------------------------------
// AC-7 / AC-8 / BR-5 — one gate, the session's
// ---------------------------------------------------------------------------

mod consent {
    use super::*;

    /// Pump until the call finishes, answering any **further** permission
    /// prompt `reject_once` so that a regression which re-asks fails on this
    /// suite's assertion rather than hanging until the response times out.
    /// Returns the prompts it had to answer.
    ///
    /// Pumps with `wait_for_event_where`, which returns *at* the matching
    /// event: every pump drops the responses it reads past, and the prompt's
    /// own response follows `agent_call_finished` on the wire, so stopping
    /// there leaves it for the caller's `await_response`.
    fn pump_until_call_finished(client: &mut Client, answered: &[String]) -> Vec<Value> {
        let deadline = Instant::now() + WINDOW;
        let mut extra = Vec::new();
        let mut seen: Vec<String> = answered.to_vec();
        loop {
            if client
                .wait_for_event_where("agent_call_finished", |_| true, Duration::from_millis(50))
                .is_some()
            {
                break;
            }
            assert!(
                Instant::now() < deadline,
                "the call never finished: {:?}",
                client.event_names()
            );
            let fresh: Vec<Value> = client
                .events_named("permission_request")
                .into_iter()
                .filter(|e| {
                    !seen
                        .iter()
                        .any(|id| e["request_id"].as_str() == Some(id.as_str()))
                })
                .cloned()
                .collect();
            for request in fresh {
                let id = request["request_id"]
                    .as_str()
                    .unwrap_or_default()
                    .to_owned();
                client.respond_permission(
                    &id,
                    json!({ "outcome": "selected", "option_id": "reject_once" }),
                );
                seen.push(id);
                extra.push(request);
            }
        }
        extra
    }

    /// **AC-7 (BR-5, ADR-5): at `guarded`, a child's `shell` asks through the
    /// session's surface, labelled with the child's name; the grant it is given
    /// is the session's, so the sibling — queued behind the ask — runs without
    /// asking again.**
    ///
    /// Two children each run one `shell`. One reaches the gate first and asks:
    /// `agent_child_consent_requested` names it, and the ordinary
    /// `permission_request` beside it carries its `child_id`. The test waits
    /// until the sibling's own `tool_call` is on the wire — it is at the gate,
    /// queued behind the open ask (`tool_started` is published immediately
    /// before `authorize`) — and only then answers `allow_always`. Benign path:
    /// the sibling's call runs with no second prompt, and both children finish
    /// with their shell's `(exit 0)` in hand.
    ///
    /// # Mutations (run 2026-10-07, each reverted)
    ///
    /// - **No re-read of the grant once a queued child's turn comes**
    ///   (`replay_grant` after `queue_for_consent` removed): 1 red, this test —
    ///   the queued sibling asks a second time.
    /// - **Children skip the loop's gate** (see AC-8's test): 3 red, this test
    ///   among them — nobody is asked at all.
    #[test]
    fn guarded_child_asks_and_sibling_reuses_grant() {
        let provider = MockProvider::start_matching(
            vec![
                (
                    child("AC7-FIRST"),
                    calls("shell", &json!({ "command": "echo first-ran" })),
                ),
                (child("AC7-FIRST"), says("first done")),
                (
                    child("AC7-SECOND"),
                    calls("shell", &json!({ "command": "echo second-ran" })),
                ),
                (child("AC7-SECOND"), says("second done")),
                (
                    parent("AC7-PARENT"),
                    dispatch(json!([
                        { "task": "AC7-FIRST run your echo", "name": "first" },
                        { "task": "AC7-SECOND run your echo", "name": "second" },
                    ])),
                ),
                (parent("AC7-PARENT"), says("Both ran.")),
            ],
            unmatched(),
        );
        let mut rig = Rig::start("ac7", &provider, NO_DEFAULT_BOUNDARIES).without_auto_approve();
        let id = rig.client.prompt_no_wait(
            &rig.session,
            "AC7-PARENT have two children each run an echo",
        );

        let label = rig
            .client
            .wait_for_event("agent_child_consent_requested", WINDOW)
            .unwrap_or_else(|| panic!("no child asked: {:?}", rig.client.event_names()));
        let asker = label["name"].as_str().unwrap_or_default().to_owned();
        let asker_id = label["child_id"].as_str().unwrap_or_default().to_owned();
        assert!(asker == "first" || asker == "second", "{label}");
        assert_eq!(label["tool"], "shell", "{label}");
        let request = rig
            .client
            .wait_for_event_where(
                "permission_request",
                |e| e["child_id"] == asker_id.as_str(),
                WINDOW,
            )
            .unwrap_or_else(|| panic!("no permission_request for {asker_id}"));
        assert_eq!(request["tool_name"], "shell", "{request}");
        assert_eq!(request["parent_turn_id"], "turn-0", "{request}");
        let sibling = if asker == "first" { "second" } else { "first" };
        let sibling_at_gate = rig.client.wait_for_event_where(
            "session_update",
            |e| {
                e["child_id"]
                    .as_str()
                    .is_some_and(|c| c.ends_with(&format!("/{sibling}")))
                    && e["update"]["kind"] == "tool_call"
            },
            WINDOW,
        );
        assert!(
            sibling_at_gate.is_some(),
            "the sibling never reached its shell call: {:?}",
            rig.client.event_names()
        );
        // Let the sibling's task get from `tool_started` into the gate's queue.
        rig.client.drain_events(Duration::from_millis(300));

        let request_id = request["request_id"]
            .as_str()
            .unwrap_or_default()
            .to_owned();
        rig.client.respond_permission(
            &request_id,
            json!({ "outcome": "selected", "option_id": "allow_always" }),
        );
        let re_asked = pump_until_call_finished(&mut rig.client, &[request_id]);
        let response = rig.client.await_response(id);
        rig.client.drain_events(Duration::from_millis(200));

        assert!(
            re_asked.is_empty(),
            "the sibling asked again after a session-scoped grant: {re_asked:#?}"
        );
        assert_eq!(rig.client.events_named("permission_request").len(), 1);
        assert_eq!(
            rig.client
                .events_named("agent_child_consent_requested")
                .len(),
            1
        );
        assert_eq!(response["result"]["stop_reason"], "end_turn", "{response}");
        for (name, task) in [("first", "AC7-FIRST"), ("second", "AC7-SECOND")] {
            let id = child_id_of(&rig.client, name);
            assert_eq!(finished_status(&rig.client, &id), "completed", "{name}");
            let requests = child_requests(&provider, task);
            assert_eq!(requests.len(), 2, "{name}: its shell ran and it answered");
            let result = last_message(&requests[1]);
            assert!(
                result.contains("(exit 0)"),
                "{name}'s shell ran to a zero exit: {result}"
            );
        }
        assert_no_boundary_bytes();
    }

    /// **AC-8 (BR-5): at `plan`, a child's `edit` is denied exactly as the
    /// parent's own is** — the same level, the same gate, the same sentence to
    /// the model, and nothing written.
    ///
    /// The parent tries the edit itself first; then dispatches a child that
    /// tries the same edit and, denied, ends with nothing to say — so it ends
    /// `refused` with `gate_denied:edit` (BR-10). The denial each model was
    /// handed is compared byte for byte. No prompt is raised: `plan` decides.
    ///
    /// # Mutation (run 2026-10-07, reverted)
    ///
    /// - **Children skip the loop's gate** (`self_gated` true inside a child):
    ///   3 red — this test (the edit lands), AC-7's (no consent asked) and
    ///   BR-5's (the command runs).
    #[test]
    fn plan_child_edit_denies() {
        let edit = json!({
            "path": "src/lib.rs",
            "old_string": "pub const ANSWER: u32 = 1;",
            "new_string": "pub const ANSWER: u32 = 2;",
        });
        let provider = MockProvider::start_matching(
            vec![
                (child("AC8-EDITOR"), calls("edit", &edit)),
                (child("AC8-EDITOR"), says("")),
                (parent("AC8-PARENT"), calls("edit", &edit)),
                (
                    parent("AC8-PARENT"),
                    dispatch(
                        json!([{ "task": "AC8-EDITOR change the constant", "name": "editor" }]),
                    ),
                ),
                (parent("AC8-PARENT"), says("Neither of us could edit.")),
            ],
            unmatched(),
        );
        let mut rig = Rig::start("ac8", &provider, "");
        rig.level("plan");
        let before = rig.ws.read_repo_file("src/lib.rs");

        let response = rig.prompt("AC8-PARENT change the constant, or have a child do it");
        assert_eq!(response["result"]["stop_reason"], "end_turn", "{response}");
        assert_eq!(
            rig.ws.read_repo_file("src/lib.rs"),
            before,
            "nothing was written"
        );
        assert!(
            rig.client.events_named("permission_request").is_empty(),
            "plan decides; nobody is asked"
        );

        let parents = parent_requests(&provider, "AC8-PARENT");
        let parent_denial = last_message(&parents[1]);
        let children = child_requests(&provider, "AC8-EDITOR");
        assert_eq!(children.len(), 2, "the child was answered after its edit");
        let child_denial = last_message(&children[1]);
        assert!(
            parent_denial.contains("edit"),
            "fixture: the parent's own edit was denied: {parent_denial}"
        );
        assert_eq!(
            child_denial, parent_denial,
            "a child's edit at plan is denied with exactly the parent's sentence"
        );

        let id = child_id_of(&rig.client, "editor");
        assert_eq!(finished_status(&rig.client, &id), "refused");
        let result = result_named(&handed(&provider, "AC8-PARENT"), "editor").clone();
        assert_eq!(result["status"], "refused", "{result}");
        assert_eq!(result["refusal"], "gate_denied:edit", "{result}");
        assert_no_boundary_bytes();
    }

    /// **BR-5 / AC-7's unattended half: with nobody to ask, the child's
    /// `shell` call fails typed and nothing is invented.**
    ///
    /// The client answers the child's ask the way a client with nobody at its
    /// terminal does — `refused` / `no_terminal` — and the call is denied: the
    /// command never runs (its child-named marker file is never created), the
    /// child's model is told so, and a child left with nothing to say ends
    /// `refused` with `gate_denied:shell`. One prompt, never a second; no
    /// grant remains. Benign path: a sibling that needs no permission
    /// completes in the same call — "the other children keep running".
    ///
    /// # Mutation (run 2026-10-07, reverted)
    ///
    /// - **A refused ask treated as allowed** (`PermissionOutcome::Refused`
    ///   settling `Allowed`): 2 red — this test (the marker file appears) and
    ///   [`statuses::refused`] (the project skill expands).
    #[test]
    fn unattended_unlisted_gate_denies_typed() {
        let provider = MockProvider::start_matching(
            vec![
                (
                    child("BR5-PROBE"),
                    calls("shell", &json!({ "command": "touch unattended-probe.ran" })),
                ),
                (child("BR5-PROBE"), says("")),
                (
                    child("BR5-READER"),
                    calls("read", &json!({ "path": "README.md" })),
                ),
                (child("BR5-READER"), says("the README describes a demo")),
                (
                    parent("BR5-PARENT"),
                    dispatch(json!([
                        { "task": "BR5-PROBE touch your marker", "name": "probe" },
                        { "task": "BR5-READER read the README", "name": "reader" },
                    ])),
                ),
                (parent("BR5-PARENT"), says("One was stopped.")),
            ],
            unmatched(),
        );
        let mut rig = Rig::start("br5", &provider, NO_DEFAULT_BOUNDARIES).without_auto_approve();
        let id = rig
            .client
            .prompt_no_wait(&rig.session, "BR5-PARENT run a probe and a reader");
        let request = rig
            .client
            .wait_for_event("permission_request", WINDOW)
            .unwrap_or_else(|| panic!("the probe never asked: {:?}", rig.client.event_names()));
        let request_id = request["request_id"]
            .as_str()
            .unwrap_or_default()
            .to_owned();
        rig.client.respond_permission(
            &request_id,
            json!({ "outcome": "refused", "reason": "no_terminal" }),
        );
        let extra = pump_until_call_finished(&mut rig.client, &[request_id]);
        let response = rig.client.await_response(id);
        rig.client.drain_events(Duration::from_millis(200));

        assert!(extra.is_empty(), "asked once, never again: {extra:#?}");
        assert_eq!(response["result"]["stop_reason"], "end_turn", "{response}");
        let probe_id = child_id_of(&rig.client, "probe");
        assert_eq!(request["child_id"], json!(probe_id), "{request}");
        assert!(
            !rig.ws.repo.join("unattended-probe.ran").exists(),
            "no consent was invented: the command never ran"
        );
        assert!(
            rig.client.events().iter().any(|e| scoped_to(e, &probe_id)
                && e["update"]["kind"] == "tool_call_update"
                && e["update"]["status"] == "failed"),
            "the child's shell call failed, typed: {:?}",
            rig.client.event_names()
        );
        let probe_requests = child_requests(&provider, "BR5-PROBE");
        assert_eq!(probe_requests.len(), 2);
        assert!(
            last_message(&probe_requests[1]).contains("Permission denied"),
            "the child's model was told: {}",
            last_message(&probe_requests[1])
        );

        let results = handed(&provider, "BR5-PARENT");
        let probe = result_named(&results, "probe");
        assert_eq!(probe["status"], "refused", "{probe}");
        assert_eq!(probe["refusal"], "gate_denied:shell", "{probe}");
        let reader = result_named(&results, "reader");
        assert_eq!(reader["status"], "completed", "{reader}");
        assert_eq!(reader["report"], "the README describes a demo");
        assert_no_boundary_bytes();
    }
}

// ---------------------------------------------------------------------------
// AC-9 — a tier is requested, routed and pinned
// ---------------------------------------------------------------------------

mod routing {
    use super::*;

    /// One leg: a Think parent (`spec` phase → the `design` category → the
    /// `think` tier) dispatching one child that requests `build`. With
    /// `bind_build`, `build` names its own provider.
    fn leg(tag: &str, bind_build: bool) -> (MockProvider, MockProvider, Rig) {
        let thinker = MockProvider::start_matching(
            vec![
                (child("AC9-CHILD"), says("built on the default route")),
                (
                    parent("AC9-PARENT"),
                    dispatch(
                        json!([{ "task": "AC9-CHILD build the thing", "name": "builder", "tier": "build" }]),
                    ),
                ),
                (parent("AC9-PARENT"), says("Back on the think route.")),
            ],
            unmatched(),
        );
        let builder = MockProvider::start_matching(
            vec![(child("AC9-CHILD"), says("built on the build route"))],
            unmatched(),
        );
        let mut config = remote_provider_block_with_window(
            "thinker",
            &thinker.openai_endpoint(),
            MODEL,
            128_000,
        );
        config.push_str(&remote_provider_block_with_window(
            "builder",
            &builder.openai_endpoint(),
            MODEL,
            128_000,
        ));
        config.push_str(&tier_block("think", "thinker"));
        if bind_build {
            config.push_str(&tier_block("build", "builder"));
        }
        let ws = Workspace::new(tag);
        ws.write_config(&config);
        let daemon = Daemon::spawn(&ws, DaemonOptions::default());
        let mut client = daemon.connect();
        let session = client.create_session("structured", Some("spec"));
        let rig = Rig {
            client,
            _daemon: daemon,
            ws,
            session,
        };
        (thinker, builder, rig)
    }

    /// **AC-9 (BR-6): a child's tier is requested, then routed.**
    ///
    /// *Binding present.* A Think-tier parent's child asking for `build` runs
    /// on the Build route: its `agent_child_started` and its result's `route`
    /// both name `builder`/`build`, the `builder` provider receives exactly
    /// the child's request, and the parent's next call still goes to
    /// `thinker` (the parent's route is unaffected by anything a child does).
    ///
    /// *No `build` binding* (benign path). The same request is not a refusal:
    /// the child completes on the default route for its category — `think`,
    /// on `thinker` — and its result names that route; `builder` sees nothing.
    ///
    /// # Mutation (run 2026-10-07, reverted)
    ///
    /// - **The child's tier request dropped** (`dispatch_route` handed `None`
    ///   for a child): 2 red — this test at the bound leg, and
    ///   [`statuses::refused`] (the oversized child's `scan` request is lost,
    ///   it lands on the 128k route and completes).
    #[test]
    fn build_child_under_think_parent() {
        // --- Binding present. ---------------------------------------------
        let (thinker, builder, mut rig) = leg("ac9-bound", true);
        let response = rig.prompt("AC9-PARENT design it, and have a child build it");
        assert_eq!(response["result"]["stop_reason"], "end_turn", "{response}");
        let parent_route = rig
            .client
            .events_named("route_decided")
            .first()
            .map(|e| ((*e)["provider_id"].clone(), (*e)["category"].clone()))
            .expect("the parent's route");
        assert_eq!(
            parent_route,
            (json!("thinker"), json!("design")),
            "fixture: the parent is a Think-tier turn"
        );
        let started = rig.client.events_named("agent_child_started")[0].clone();
        assert_eq!(
            started["route"],
            json!({ "tier": "build", "provider_id": "builder", "model": MODEL }),
            "{started}"
        );
        let result = result_named(&handed(&thinker, "AC9-PARENT"), "builder").clone();
        assert_eq!(result["status"], "completed", "{result}");
        assert_eq!(
            result["route"], started["route"],
            "the result names the route it ran on"
        );
        assert_eq!(result["report"], "built on the build route");
        assert_eq!(
            builder.request_count(),
            1,
            "the build provider served the child"
        );
        assert_eq!(
            parent_requests(&thinker, "AC9-PARENT").len(),
            2,
            "the parent's next call after the result still went to the think route"
        );
        assert!(child_requests(&thinker, "AC9-CHILD").is_empty());
        drop(rig);

        // --- No build binding: the category's default, and no refusal. -----
        let (thinker, builder, mut rig) = leg("ac9-unbound", false);
        let response = rig.prompt("AC9-PARENT design it, and have a child build it");
        assert_eq!(response["result"]["stop_reason"], "end_turn", "{response}");
        let started = rig.client.events_named("agent_child_started")[0].clone();
        assert_eq!(
            started["route"],
            json!({ "tier": "think", "provider_id": "thinker", "model": MODEL }),
            "an unhonoured request takes the category's default: {started}"
        );
        let result = result_named(&handed(&thinker, "AC9-PARENT"), "builder").clone();
        assert_eq!(result["status"], "completed", "not a refusal: {result}");
        assert!(result.get("refusal").is_none(), "{result}");
        assert_eq!(result["route"], started["route"]);
        assert_eq!(result["report"], "built on the default route");
        assert_eq!(
            builder.request_count(),
            0,
            "nothing reached the unbound provider"
        );
        assert_no_boundary_bytes();
    }
}

// ---------------------------------------------------------------------------
// AC-13 / BR-8 — shares of one ceiling, derived from the ledger
// ---------------------------------------------------------------------------

mod spend {
    use super::*;

    /// The ceiling every leg runs under, in dollars as a person types it.
    pub(super) const CEILING_USD: f64 = 1.0;

    /// [`CEILING_USD`] in the unit every spend comparison runs in — through
    /// REQ-588's own one conversion at the config edge, not a re-spelling of
    /// it. This is an *input* to the share arithmetic, not its subject.
    pub(super) fn ceiling_units() -> u64 {
        teton_core::config::CostConfig {
            prompt_ceiling_usd: Some(CEILING_USD),
        }
        .ceiling_micro_cents()
        .expect("a finite, non-negative ceiling converts")
    }

    /// Every `cost_recorded` row for `session` — the ledger's own projection of
    /// each row it wrote — with its position in the client's event stream.
    pub(super) fn cost_rows(client: &Client, session: &str) -> Vec<(usize, Value)> {
        client
            .events()
            .iter()
            .enumerate()
            .filter(|(_, e)| e["event"] == "cost_recorded" && e["session_id"] == session)
            .map(|(i, e)| (i, e["record"].clone()))
            .collect()
    }

    /// A row's spend, in the ledger's unit.
    pub(super) fn units(record: &Value) -> u64 {
        record["usd_micros"]
            .as_u64()
            .unwrap_or_else(|| panic!("a priced row: {record}"))
    }

    /// What the parent prompt had recorded before its call started: the
    /// subtrahend of the call's headroom (ADR-4).
    pub(super) fn parent_spend_before_call(client: &Client, session: &str) -> u64 {
        let call = index_where(client, |e| {
            e["event"] == "agent_call_started" && e["session_id"] == session
        })
        .expect("the call started");
        cost_rows(client, session)
            .iter()
            .filter(|(i, r)| *i < call && r.get("child_id").is_none())
            .map(|(_, r)| units(r))
            .sum()
    }

    /// One child's recorded rows, in order.
    pub(super) fn child_rows(client: &Client, session: &str, child_id: &str) -> Vec<u64> {
        cost_rows(client, session)
            .iter()
            .filter(|(_, r)| r["child_id"] == child_id)
            .map(|(_, r)| units(r))
            .collect()
    }

    /// The child's line under its parent turn in the `/cost` view — the same
    /// ledger, read back through the report a user sees (AC-12).
    pub(super) fn cost_view_child(client: &mut Client, session: &str, child_id: &str) -> u64 {
        let report = client.cost_query();
        report["per_turn"]
            .as_array()
            .into_iter()
            .flatten()
            .filter(|turn| turn["session_id"] == session)
            .flat_map(|turn| turn["children"].as_array().cloned().unwrap_or_default())
            .find(|c| c["child_id"] == child_id)
            .and_then(|c| c["usd_micros"].as_u64())
            .unwrap_or_else(|| panic!("no /cost line for {child_id}: {report}"))
    }

    /// The share `agent_child_started` stamped on `child_id`.
    pub(super) fn stamped(client: &Client, child_id: &str) -> u64 {
        client
            .events_named("agent_child_started")
            .iter()
            .find(|e| e["child_id"] == child_id)
            .and_then(|e| e["bounds"]["spend_ceiling_micro_cents"].as_u64())
            .unwrap_or_else(|| panic!("no stamped share for {child_id}"))
    }

    /// **AC-13 (BR-8; LESSON-552): the session's ceiling is split into equal
    /// shares of the headroom left when the call starts, an early finisher's
    /// unspent share is released to the siblings still running, and both are
    /// derived end to end from the ledger — never from a literal.**
    ///
    /// Every expected figure below is computed from what the ledger recorded
    /// (`cost_recorded`, cross-checked against the `/cost` view) and the
    /// ceiling's one conversion; the token counts the mock bills are sized to
    /// land each call in its band, and each band is asserted before anything
    /// leans on it (LESSON-640).
    ///
    /// **Two children** — `hungry` spends past the whole headroom on its first
    /// call, `frugal` answers cheaply. Each was stamped `floor(headroom / 2)`.
    /// `hungry`'s next call is past its ceiling (whatever `frugal` released to
    /// it) and it ends `spend_exhausted` with an empty report; `frugal`
    /// completes; and the parent's own next call meets the session ceiling the
    /// children consumed and ends on REQ-588's existing spend-exhausted path.
    ///
    /// **Three children** — `early` completes having spent about a third of
    /// its share; `first` and `second` are parked on gates until its release
    /// has happened. `agent_child_share_released` names `early`'s unspent share
    /// split equally between the two, each sibling's
    /// `spend_ceiling_final_micro_cents` is its stamped share plus its part,
    /// and `first` — whose first call alone costs more than its stamped share,
    /// so it would have been `spend_exhausted` without the release — completes
    /// under the raised ceiling. `second` is released only after `first` has
    /// finished, so neither sibling's end moves the other's ceiling again.
    /// Benign path: the parent, under the same ceiling, continues and receives
    /// all three results.
    ///
    /// # Mutations (run 2026-10-07, each reverted)
    ///
    /// - **No release** (`SharePool::release` returning no recipients): 1
    ///   red, this test at the release leg.
    /// - **Shares not split** (each child stamped the whole headroom): 2 red,
    ///   this test and [`statuses::spend_exhausted`].
    /// - **A child's spend not added to the prompt's** (`ChildSpend::add`
    ///   without the parent add): 1 red, this test — the parent's next call is
    ///   not refused.
    /// - **The spend arm collapsed** (`SPEND_CEILING_REACHED` ending a child
    ///   `failed`): 2 red, this test and [`statuses::spend_exhausted`].
    /// - **Children run one at a time**, before the stamped-share fix: red
    ///   here — `first` was stamped after `early` released, and its "initial"
    ///   share already held its part. That is how the stamping bug was found;
    ///   after the fix this test is green under that mutation, and
    ///   `cost::share`'s own test pins the fix.
    #[test]
    fn two_child_split_and_three_child_release() {
        let readme = json!({ "path": "README.md" });
        let first_gate = Rendezvous::gate();
        let second_gate = Rendezvous::gate();
        let provider = MockProvider::start_matching(
            vec![
                // Two-child split.
                (
                    child("SPLIT-HUNGRY"),
                    calls_billing("read", &readme, 250_000, 10),
                ),
                (
                    child("SPLIT-HUNGRY"),
                    says("hungry is never answered twice"),
                ),
                (child("SPLIT-FRUGAL"), says("frugal report")),
                (
                    parent("SPLIT-PARENT"),
                    dispatch(json!([
                        { "task": "SPLIT-HUNGRY read everything", "name": "hungry" },
                        { "task": "SPLIT-FRUGAL answer briefly", "name": "frugal" },
                    ])),
                ),
                (parent("SPLIT-PARENT"), says("the ceiling stops this call")),
                // Three-child release.
                (child("REL-EARLY"), says_billing("early report", 25_237, 0)),
                (
                    child("REL-FIRST"),
                    first_gate.hold(calls_billing("read", &readme, 90_000, 10)),
                ),
                (child("REL-FIRST"), says_billing("first report", 20_000, 10)),
                (child("REL-SECOND"), second_gate.hold(says("second report"))),
                (
                    parent("REL-PARENT"),
                    dispatch(json!([
                        { "task": "REL-EARLY answer at once", "name": "early" },
                        { "task": "REL-FIRST read, then answer", "name": "first" },
                        { "task": "REL-SECOND answer last", "name": "second" },
                    ])),
                ),
                (parent("REL-PARENT"), says("All three are back.")),
            ],
            unmatched(),
        );
        let mut rig = Rig::start("ac13", &provider, &cost_table(CEILING_USD));
        let ceiling = ceiling_units();

        // ================= Two children: the split. =======================
        let split_session = rig.session.clone();
        let response = rig.prompt("SPLIT-PARENT split the work between two children");
        assert_eq!(
            response["error"]["code"].as_i64(),
            Some(SPEND_CEILING_REACHED),
            "the parent's next call met the ceiling its children consumed: {response}"
        );
        let client = &rig.client;
        let hungry = child_id_of(client, "hungry");
        let frugal = child_id_of(client, "frugal");
        let spent_before = parent_spend_before_call(client, &split_session);
        assert!(
            spent_before > 0,
            "fixture: the parent's own first call was billed"
        );
        let share = (ceiling - spent_before) / 2;
        assert_eq!(stamped(client, &hungry), share, "floor(headroom / 2)");
        assert_eq!(stamped(client, &frugal), share, "floor(headroom / 2)");

        let hungry_spent: u64 = child_rows(client, &split_session, &hungry).iter().sum();
        let frugal_spent: u64 = child_rows(client, &split_session, &frugal).iter().sum();
        // `frugal` may have finished first and released to `hungry`; the
        // release names `hungry`'s new ceiling, and it is the only way that
        // ceiling ever rose.
        let hungry_ceiling = client
            .events_named("agent_child_share_released")
            .iter()
            .flat_map(|e| e["recipients"].as_array().cloned().unwrap_or_default())
            .filter(|r| r["child_id"] == hungry.as_str())
            .filter_map(|r| r["new_ceiling_micro_cents"].as_u64())
            .max()
            .unwrap_or(share);
        assert!(
            hungry_spent >= hungry_ceiling,
            "fixture arithmetic: hungry's first call ({hungry_spent}) must reach its ceiling \
             ({hungry_ceiling}) for its next call to be refused"
        );
        assert_eq!(finished_status(client, &hungry), "spend_exhausted");
        let hungry_finish = client
            .events_named("agent_child_finished")
            .into_iter()
            .find(|e| e["child_id"] == hungry.as_str())
            .cloned()
            .expect("hungry finished");
        assert_eq!(
            hungry_finish["report_bytes"], 0,
            "an empty report: {hungry_finish}"
        );
        assert_eq!(finished_status(client, &frugal), "completed");
        assert!(
            spent_before + hungry_spent + frugal_spent >= ceiling,
            "fixture arithmetic: the children consumed the prompt's ceiling"
        );
        assert_eq!(
            parent_requests(&provider, "SPLIT-PARENT").len(),
            1,
            "the parent's next call was refused before it was sent"
        );
        assert_eq!(
            cost_view_child(&mut rig.client, &split_session, &hungry),
            hungry_spent
        );
        assert_eq!(
            cost_view_child(&mut rig.client, &split_session, &frugal),
            frugal_spent
        );

        // ================= Three children: the release. ===================
        let release_session = rig.client.create_session("structured", Some("implement"));
        let id = rig
            .client
            .prompt_no_wait(&release_session, "REL-PARENT three children, one early");
        let released = rig
            .client
            .wait_for_event_where(
                "agent_child_share_released",
                |e| e["session_id"] == release_session.as_str(),
                WINDOW,
            )
            .unwrap_or_else(|| {
                first_gate.open();
                second_gate.open();
                panic!(
                    "early's share was never released: {:?}",
                    rig.client.event_names()
                )
            });
        first_gate.open();
        let first_finished = rig.client.wait_for_event_where(
            "agent_child_finished",
            |e| {
                e["child_id"]
                    .as_str()
                    .is_some_and(|c| c.ends_with("/first"))
            },
            WINDOW,
        );
        second_gate.open();
        assert!(
            first_finished.is_some(),
            "first finished: {:?}",
            rig.client.event_names()
        );
        let response = rig.client.await_response(id);
        rig.client.drain_events(Duration::from_millis(200));
        assert_eq!(response["result"]["stop_reason"], "end_turn", "{response}");

        let client = &rig.client;
        let early = child_id_of(client, "early");
        let first = child_id_of(client, "first");
        let second = child_id_of(client, "second");
        let spent_before = parent_spend_before_call(client, &release_session);
        let share = (ceiling - spent_before) / 3;
        for id in [&early, &first, &second] {
            assert_eq!(stamped(client, id), share, "floor(headroom / 3) for {id}");
        }
        let early_spent: u64 = child_rows(client, &release_session, &early).iter().sum();
        assert!(
            early_spent > 0 && early_spent < share,
            "fixture arithmetic: early finished under its share ({early_spent} of {share})"
        );
        let unspent = share - early_spent;
        let part = unspent / 2;
        assert_eq!(released["child_id"], json!(early), "{released}");
        assert_eq!(
            released["released_micro_cents"],
            json!(unspent),
            "{released}"
        );
        let mut recipients: Vec<(String, u64)> = released["recipients"]
            .as_array()
            .expect("recipients")
            .iter()
            .map(|r| {
                (
                    r["child_id"].as_str().unwrap_or_default().to_owned(),
                    r["new_ceiling_micro_cents"].as_u64().unwrap_or_default(),
                )
            })
            .collect();
        recipients.sort();
        let mut expected = vec![
            (first.clone(), share + part),
            (second.clone(), share + part),
        ];
        expected.sort();
        assert_eq!(
            recipients, expected,
            "two-thirds split between the two running siblings"
        );

        let first_rows = child_rows(client, &release_session, &first);
        assert_eq!(first_rows.len(), 2, "first made two calls: {first_rows:?}");
        assert!(
            first_rows[0] >= share && first_rows[0] < share + part,
            "fixture arithmetic: first's first call ({}) is past its stamped share ({share}) \
             and inside its raised one ({})",
            first_rows[0],
            share + part
        );
        assert!(
            first_rows.iter().sum::<u64>() >= share + part,
            "fixture arithmetic: first ends with nothing unspent, so its end releases nothing"
        );
        assert_eq!(
            client
                .events_named("agent_child_share_released")
                .iter()
                .filter(|e| e["session_id"] == release_session.as_str())
                .count(),
            1,
            "only early's end moved a share"
        );

        let results = handed(&provider, "REL-PARENT");
        let early_result = result_named(&results, "early");
        assert_eq!(early_result["status"], "completed", "{early_result}");
        assert_eq!(
            early_result["spend_ceiling_final_micro_cents"],
            json!(share)
        );
        for name in ["first", "second"] {
            let result = result_named(&results, name);
            assert_eq!(result["status"], "completed", "{name} completed: {result}");
            assert_eq!(result["bounds"]["spend_ceiling_micro_cents"], json!(share));
            assert_eq!(
                result["spend_ceiling_final_micro_cents"],
                json!(share + part),
                "{name}: its stamped share plus its part"
            );
        }
        assert_eq!(
            cost_view_child(&mut rig.client, &release_session, &first),
            first_rows.iter().sum::<u64>()
        );
        assert_no_boundary_bytes();
    }
}

// ---------------------------------------------------------------------------
// AC-14 / BR-10 — eight terminal statuses, the parent continuing in each
// ---------------------------------------------------------------------------

mod statuses {
    use super::*;

    /// Run one prompt whose single `agent` call dispatches `tasks`, and return
    /// the rig and what the parent's next request was handed.
    fn one_call(
        tag: &str,
        marker: &str,
        tasks: Value,
        children: Vec<(Matcher, MockResponse)>,
        extra: &str,
    ) -> (MockProvider, Rig, Vec<Value>) {
        let mut table = children;
        table.push((parent(marker), dispatch(tasks)));
        table.push((parent(marker), says("The parent carried on.")));
        let provider = MockProvider::start_matching(table, unmatched());
        let mut rig = Rig::start(tag, &provider, extra);
        let response = rig.prompt(&format!("{marker} dispatch one call"));
        assert_eq!(
            response["result"]["stop_reason"], "end_turn",
            "the parent turn continues past any child's ending (BR-10): {response}"
        );
        let results = handed(&provider, marker);
        (provider, rig, results)
    }

    /// **`completed` (AC-14).** The model answers; the report is its text,
    /// untouched and unmarked, and the result carries the route and bounds it
    /// ran under. Benign path for the whole matrix.
    ///
    /// Mutation (run 2026-10-07, reverted): **every report emptied** (`finish`
    /// discarding the bounded text): 8 red, this test among them.
    #[test]
    fn completed() {
        let (_provider, rig, results) = one_call(
            "st-completed",
            "ST-COMPLETED",
            json!([{ "task": "ST-DONE answer", "name": "done" }]),
            vec![(
                child("ST-DONE"),
                says("COMPLETED-REPORT the README is short"),
            )],
            "",
        );
        let result = result_named(&results, "done");
        assert_eq!(result["status"], "completed", "{result}");
        assert_eq!(result["report"], "COMPLETED-REPORT the README is short");
        assert!(result.get("refusal").is_none() && result.get("error").is_none());
        assert_eq!(result["turns_used"], 1);
        let started = rig.client.events_named("agent_child_started")[0].clone();
        assert_eq!(
            result["bounds"], started["bounds"],
            "BR-7: the bounds echoed"
        );
        assert_eq!(result["route"], started["route"]);
        assert_no_boundary_bytes();
    }

    /// **`refused` (AC-14), both flavours the spec names, in one call.**
    ///
    /// - `gated` invokes a **project** skill; the repository's acknowledgment
    ///   is asked of the client, which — with nobody at its terminal — answers
    ///   `refused`. With nothing to report, the child ends `refused` with
    ///   `gate_denied:skill`: a project-skill gate is a gate (BR-10).
    /// - `oversized` requests the `scan` tier, bound to a provider with no
    ///   declared window (the 32 KiB default budget), and carries a ~50 KB
    ///   task: admitted whole or refused, it ends `refused` with `over_budget`
    ///   naming size, budget and bound — and nothing is sent anywhere.
    ///
    /// # Mutations (run 2026-10-07, each reverted)
    ///
    /// - **The project-skill gate's refusal counted as a call that ran**
    ///   (`gate_refusal` reading `ran` unadjusted): 1 red here, plus
    ///   `child::tests::a_call_refused_inside_its_own_gate_is_denied_not_ran`
    ///   in the lib. Before this task's fix that was the shipped behaviour:
    ///   `gated` ended `completed` with an empty report.
    /// - **Whole-or-refused dropped** (the `!fit.fits` return made dead): 1
    ///   red, this test — the oversized task is sent.
    /// - The refused-as-allowed and dropped-tier mutations redden it too (see
    ///   BR-5's and AC-9's tests).
    #[test]
    fn refused() {
        let oversized_task = format!("ST-OVERSIZED {}", "word ".repeat(10_000));
        let small = MockProvider::start_matching(Vec::new(), unmatched());
        let provider = MockProvider::start_matching(
            vec![
                (
                    child("ST-GATED"),
                    calls("skill", &json!({ "name": "deploy" })),
                ),
                (child("ST-GATED"), says("")),
                (
                    parent("ST-REFUSED"),
                    dispatch(json!([
                        { "task": "ST-GATED run the deploy skill", "name": "gated" },
                        { "task": oversized_task, "name": "oversized", "tier": "scan" },
                    ])),
                ),
                (parent("ST-REFUSED"), says("Both were refused.")),
            ],
            unmatched(),
        );
        let mut config = base_config(&provider);
        config.push_str(&harness::remote_provider_block(
            "small",
            &small.openai_endpoint(),
            MODEL,
        ));
        config.push_str(&tier_block("scan", "small"));
        let mut rig = Rig::with_config("st-refused", &config, |ws| {
            let dir = ws.repo.join(".claude/skills/deploy");
            std::fs::create_dir_all(&dir).unwrap();
            std::fs::write(
                dir.join("SKILL.md"),
                "---\ndescription: deploy the demo\n---\n\nDeploy the demo.\n",
            )
            .unwrap();
            DaemonOptions::default()
        })
        .without_auto_approve();

        let id = rig.client.prompt_no_wait(
            &rig.session,
            "ST-REFUSED dispatch a gated and an oversized child",
        );
        let ask = rig
            .client
            .wait_for_event("permission_request", WINDOW)
            .unwrap_or_else(|| panic!("no acknowledgment asked: {:?}", rig.client.event_names()));
        assert_eq!(ask["subject"]["kind"], "project_skill_trust", "{ask}");
        rig.client.respond_permission(
            ask["request_id"].as_str().unwrap_or_default(),
            json!({ "outcome": "refused", "reason": "no_terminal" }),
        );
        let response = rig.client.await_response(id);
        rig.client.drain_events(Duration::from_millis(200));
        assert_eq!(response["result"]["stop_reason"], "end_turn", "{response}");

        let results = handed(&provider, "ST-REFUSED");
        let gated = result_named(&results, "gated");
        assert_eq!(gated["status"], "refused", "{gated}");
        assert_eq!(gated["refusal"], "gate_denied:skill", "{gated}");
        assert_eq!(gated["report"], "");
        assert!(
            !bodies(&provider)
                .iter()
                .any(|b| b.contains("Deploy the demo.")),
            "nothing of the skill was expanded"
        );
        let oversized = result_named(&results, "oversized");
        assert_eq!(oversized["status"], "refused", "{oversized}");
        let refusal = oversized["refusal"].as_str().unwrap_or_default();
        assert!(refusal.starts_with("over_budget: "), "{refusal}");
        assert!(
            refusal.contains("context budget is") && refusal.contains("(bound: "),
            "names size, budget and bound: {refusal}"
        );
        assert_eq!(oversized["turns_used"], 0);
        assert_eq!(oversized["route"]["provider_id"], "small", "{oversized}");
        assert!(
            oversized["bounds"]["context_budget_bytes"]
                .as_u64()
                .is_some_and(|budget| (budget as usize) < oversized_task_len()),
            "fixture arithmetic: the task alone is larger than the child's budget: {oversized}"
        );
        assert_eq!(
            small.request_count(),
            0,
            "an over-budget task is never sent"
        );
        assert_no_boundary_bytes();
    }

    /// The size of the `oversized` task, for the arithmetic assertion.
    fn oversized_task_len() -> usize {
        "ST-OVERSIZED ".len() + "word ".len() * 10_000
    }

    /// **`cancelled` (AC-14, BR-10): cancelling the parent turn cancels every
    /// running child, and the cancelled finishes are still published.**
    ///
    /// **Driven in-process, and this is the one test in the file that is.**
    /// The daemon has no RPC that cancels a running prompt: a departing
    /// client's turn *drains* (REQ-565), and the only abort of a started turn
    /// is the server's `task.abort()` after `TURN_DRAIN_TIMEOUT` — 300 s after
    /// the disconnect. So this builds the daemon's own [`DaemonRuntime`] — the
    /// real `agent` tool, the real child runner, a real HTTP vendor — runs the
    /// prompt turn as a task, and aborts it exactly as that teardown does, with
    /// both children parked on their first model call. What it observes is the
    /// session's bus, which is what every client attached to it reads.
    ///
    /// # Mutations (run 2026-10-07, each reverted)
    ///
    /// - **The dropped call publishes nothing** (`Flight::drop` aborting
    ///   without spawning the reaper): 1 red, this test.
    /// - **Children run one at a time**: red here too — the second child never
    ///   parks.
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn cancelled() {
        use teton_protocol::agent::ChildStatus;
        use teton_protocol::events::Event;
        use teton_protocol::methods::{
            ConfigUpdate, ProviderConfig, SessionPermissionsParams, TierBindingConfig,
        };
        use teton_protocol::permissions::PermissionLevel;
        use teton_protocol::{Phase, ProviderId, ProviderKind, SessionMode, Tier};
        use tetond::broadcast::EventBus;
        use tetond::grants::GrantRegistry;
        use tetond::runtime::{ClientPresence, DaemonRuntime};
        use tetond::sessions::SessionRegistry;

        let hold = Rendezvous::gate();
        let provider = MockProvider::start_matching(
            vec![
                (
                    child("ST-CANCEL-ONE"),
                    hold.hold(says("one is never answered")),
                ),
                (
                    child("ST-CANCEL-TWO"),
                    hold.hold(says("two is never answered")),
                ),
                (
                    parent("ST-CANCELLED"),
                    dispatch(json!([
                        { "task": "ST-CANCEL-ONE wait", "name": "one" },
                        { "task": "ST-CANCEL-TWO wait", "name": "two" },
                    ])),
                ),
            ],
            unmatched(),
        );
        let ws = Workspace::new("st-cancelled");
        let runtime = std::sync::Arc::new(DaemonRuntime::minimal());
        runtime
            .apply_config_update(ConfigUpdate::RegisterProvider(ProviderConfig {
                id: ProviderId::from(REMOTE),
                kind: ProviderKind::OpenaiCompatible,
                endpoint: Some(provider.openai_endpoint()),
                model: Some(MODEL.to_owned()),
                auth_ref: None,
                max_context: Some(128_000),
                context_budget_cap: None,
                allow_cleartext: None,
                floored_budget: None,
            }))
            .expect("registering the provider");
        runtime
            .apply_config_update(ConfigUpdate::SetTierBinding(TierBindingConfig {
                tier: Tier::Build,
                provider_id: ProviderId::from(REMOTE),
                fallback_id: None,
            }))
            .expect("binding build");
        let bus = std::sync::Arc::new(EventBus::new());
        let sessions = SessionRegistry::new();
        let session = sessions
            .create(
                SessionMode::Structured,
                Some(Phase::Implement),
                Some(ws.repo.clone()),
            )
            .expect("a structured session")
            .session_id;
        let set = runtime.session_permissions(
            &SessionPermissionsParams {
                session_id: session.clone(),
                level: Some(PermissionLevel::Full),
            },
            &bus,
        );
        assert_eq!(set.level, PermissionLevel::Full);
        let mut sub = bus.subscribe(4096);

        let turn = {
            let runtime = std::sync::Arc::clone(&runtime);
            let bus = std::sync::Arc::clone(&bus);
            let sessions = sessions.clone();
            let session = session.clone();
            let cwd = ws.repo.clone();
            tokio::spawn(async move {
                runtime
                    .run_prompt_turn(
                        &bus,
                        &sessions,
                        session,
                        SessionMode::Structured,
                        Some(Phase::Implement),
                        Some(cwd),
                        "ST-CANCELLED dispatch two children that wait".to_owned(),
                        None,
                        Some(GrantRegistry::new().next_connection_id()),
                        ClientPresence::unwatched(),
                    )
                    .await
            })
        };

        // Both children are parked on their first model call.
        let parked = tokio::time::timeout(WINDOW, async {
            while hold.arrived() < 2 {
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await;
        assert!(parked.is_ok(), "both children reached the provider");

        // The server's abort of a turn nobody is left to answer.
        turn.abort();
        let aborted = turn.await;
        assert!(
            aborted.as_ref().is_err_and(|e| e.is_cancelled()),
            "the parent turn was cancelled, not finished"
        );

        let mut finished: Vec<(String, ChildStatus)> = Vec::new();
        let mut call_finished = None;
        let deadline = tokio::time::Instant::now() + WINDOW;
        while call_finished.is_none() && tokio::time::Instant::now() < deadline {
            match tokio::time::timeout(Duration::from_millis(200), sub.recv()).await {
                Ok(Some(envelope)) => match envelope.event {
                    Event::AgentChildFinished(f) => {
                        finished.push((f.child_id.to_string(), f.status));
                    }
                    Event::AgentCallFinished(f) => call_finished = Some(f),
                    _ => {}
                },
                Ok(None) => break,
                Err(_) => {}
            }
        }
        hold.open();
        let call_finished =
            call_finished.expect("agent_call_finished was published after the cancel");
        finished.sort_by(|a, b| a.0.cmp(&b.0));
        assert_eq!(
            finished
                .iter()
                .map(|(id, status)| (
                    id.rsplit('/').next().unwrap_or_default().to_owned(),
                    *status
                ))
                .collect::<Vec<_>>(),
            vec![
                ("one".to_owned(), ChildStatus::Cancelled),
                ("two".to_owned(), ChildStatus::Cancelled),
            ],
            "every running child is cancelled, and each finish is published"
        );
        assert!(
            call_finished
                .children
                .iter()
                .all(|c| c.status == ChildStatus::Cancelled)
                && call_finished.children.len() == 2,
            "{call_finished:?}"
        );
        assert_no_boundary_bytes();
    }

    /// **`turns_exhausted` (AC-14).** `child_max_turns = 1` and a child that
    /// calls a tool on its only turn: the report is the last text it wrote,
    /// marked as unfinished, and `bounds.max_turns` says what stopped it.
    ///
    /// Mutation (run 2026-10-07, reverted): **the cap's arm ends `completed`
    /// with the raw text**: 1 red, this test.
    #[test]
    fn turns_exhausted() {
        let (_provider, _rig, results) = one_call(
            "st-turns",
            "ST-TURNS",
            json!([{ "task": "ST-LOOPER keep reading", "name": "looper" }]),
            vec![(
                child("ST-LOOPER"),
                MockResponse::ok(openai_turn(
                    "Reading the README first.",
                    Some(("mock-call", "read", r#"{"path":"README.md"}"#)),
                    100,
                    10,
                )),
            )],
            &agent_table(&[("child_max_turns", "1")]),
        );
        let result = result_named(&results, "looper");
        assert_eq!(result["status"], "turns_exhausted", "{result}");
        assert_eq!(result["bounds"]["max_turns"], 1, "{result}");
        let report = result["report"].as_str().unwrap_or_default();
        assert!(
            report.starts_with("[turns_exhausted:"),
            "the report is marked: {report}"
        );
        assert!(
            report.ends_with("Reading the README first."),
            "and carries the last text the child wrote: {report}"
        );
        assert_no_boundary_bytes();
    }

    /// **`budget_exhausted` (AC-14).** The child's provider refuses its request
    /// as too large for its window (REQ-586's typed outcome): the child ends
    /// `budget_exhausted` with an empty report, and nothing else is tried.
    ///
    /// Mutation (run 2026-10-07, reverted): **the window arm ends `failed`**
    /// (`ended_by` mapping `CONTEXT_LENGTH_EXCEEDED` to `failed`): 1 red, this
    /// test.
    #[test]
    fn budget_exhausted() {
        let (provider, _rig, results) = one_call(
            "st-budget",
            "ST-BUDGET",
            json!([{ "task": "ST-WIDE read it all", "name": "wide" }]),
            vec![(
                child("ST-WIDE"),
                MockResponse::status_with_body(
                    400,
                    r#"{"error":{"code":"context_length_exceeded","message":"This model's maximum context length is 8192 tokens"}}"#,
                ),
            )],
            "",
        );
        let result = result_named(&results, "wide");
        assert_eq!(result["status"], "budget_exhausted", "{result}");
        assert_eq!(result["report"], "");
        assert_eq!(child_requests(&provider, "ST-WIDE").len(), 1);
        assert_no_boundary_bytes();
    }

    /// **`spend_exhausted` (AC-14).** Two children under a prompt ceiling:
    /// `spender`'s first call costs more than its stamped share, so its next
    /// call is refused at its own choke point and it ends `spend_exhausted`
    /// with an empty report. `waiter` is held until `spender` has finished, so
    /// no release reaches `spender` first; and the call's total is left under
    /// the prompt's ceiling, so the parent continues and receives both results.
    ///
    /// # Mutations (run 2026-10-07, each reverted)
    ///
    /// - **The spend arm ends `failed`**: 2 red, this test and AC-13's.
    /// - **Shares not split**: 2 red, the same two — `spender`'s first call is
    ///   inside a share of the whole headroom, so it is answered again.
    #[test]
    fn spend_exhausted() {
        let gate = Rendezvous::gate();
        let provider = MockProvider::start_matching(
            vec![
                (
                    child("ST-SPENDER"),
                    calls_billing("read", &json!({ "path": "README.md" }), 150_000, 10),
                ),
                (child("ST-SPENDER"), says("spender is never answered twice")),
                (child("ST-WAITER"), gate.hold(says("waiter report"))),
                (
                    parent("ST-SPEND"),
                    dispatch(json!([
                        { "task": "ST-SPENDER read a lot", "name": "spender" },
                        { "task": "ST-WAITER answer after", "name": "waiter" },
                    ])),
                ),
                (parent("ST-SPEND"), says("The parent carried on.")),
            ],
            unmatched(),
        );
        let mut rig = Rig::start("st-spend", &provider, &cost_table(spend::CEILING_USD));
        let id = rig
            .client
            .prompt_no_wait(&rig.session, "ST-SPEND dispatch two");
        let spender_done = rig.client.wait_for_event_where(
            "agent_child_finished",
            |e| {
                e["child_id"]
                    .as_str()
                    .is_some_and(|c| c.ends_with("/spender"))
            },
            WINDOW,
        );
        gate.open();
        assert!(spender_done.is_some(), "{:?}", rig.client.event_names());
        let response = rig.client.await_response(id);
        rig.client.drain_events(Duration::from_millis(200));
        assert_eq!(response["result"]["stop_reason"], "end_turn", "{response}");

        let spender = child_id_of(&rig.client, "spender");
        let share = spend::stamped(&rig.client, &spender);
        let spent: u64 = spend::child_rows(&rig.client, &rig.session, &spender)
            .iter()
            .sum();
        assert!(
            spent >= share,
            "fixture arithmetic: spender's call ({spent}) reached its share ({share})"
        );
        let results = handed(&provider, "ST-SPEND");
        let result = result_named(&results, "spender");
        assert_eq!(result["status"], "spend_exhausted", "{result}");
        assert_eq!(result["report"], "");
        assert_eq!(result["spend_ceiling_final_micro_cents"], json!(share));
        assert_eq!(child_requests(&provider, "ST-SPENDER").len(), 1);
        assert_eq!(result_named(&results, "waiter")["status"], "completed");
        assert_no_boundary_bytes();
    }

    /// **`timed_out` (AC-14): past its deadline with a tool call in flight.**
    ///
    /// `child_deadline_secs = 1`, and the child's `shell` parks until the test
    /// releases it — which the test does only after the child has been
    /// reported `timed_out`, so the deadline provably fired while the tool was
    /// in flight. The parent is not made to wait for the tool: it continues,
    /// and receives `timed_out` with an empty report and the deadline it ran
    /// under. The tool's output never reaches a model — the child is not
    /// answered again.
    ///
    /// Mutation (run 2026-10-07, reverted): **the deadline never fires** (the
    /// runner's `deadline.expired()` arm made pending): 1 red, this test — no
    /// finish arrives while the tool is parked.
    #[test]
    fn timed_out() {
        let provider = MockProvider::start_matching(
            vec![
                (
                    child("ST-SLOW"),
                    calls(
                        "shell",
                        &parked_shell("timed-slow.started", "timed-slow.release"),
                    ),
                ),
                (child("ST-SLOW"), says("slow is never answered twice")),
                (
                    parent("ST-TIMED"),
                    dispatch(json!([{ "task": "ST-SLOW wait for the release", "name": "slow" }])),
                ),
                (parent("ST-TIMED"), says("The parent carried on.")),
            ],
            unmatched(),
        );
        let mut extra = agent_table(&[("child_deadline_secs", "1")]);
        extra.push_str(NO_DEFAULT_BOUNDARIES);
        let mut rig = Rig::start("st-timed", &provider, &extra);
        rig.level("full");
        let release = rig.ws.repo.join("timed-slow.release");
        let id = rig
            .client
            .prompt_no_wait(&rig.session, "ST-TIMED dispatch one");
        assert!(await_file(&rig.ws.repo.join("timed-slow.started")));
        let finished = rig.client.wait_for_event("agent_child_finished", WINDOW);
        let in_flight = !release.exists();
        std::fs::write(&release, "").expect("release the parked tool");
        let finished = finished.unwrap_or_else(|| panic!("{:?}", rig.client.event_names()));
        assert!(in_flight);
        assert_eq!(finished["status"], "timed_out", "{finished}");
        let response = rig.client.await_response(id);
        assert_eq!(response["result"]["stop_reason"], "end_turn", "{response}");

        let result = result_named(&handed(&provider, "ST-TIMED"), "slow").clone();
        assert_eq!(result["status"], "timed_out", "{result}");
        assert_eq!(result["report"], "");
        assert_eq!(result["bounds"]["deadline_secs"], 1);
        assert_eq!(
            child_requests(&provider, "ST-SLOW").len(),
            1,
            "the cancelled call's output was never handed to a model"
        );
        assert_no_boundary_bytes();
    }

    /// **`failed` (AC-14): a terminal provider error after the child's own
    /// retry and reroute path**, carrying the error code. The provider answers
    /// `401` — settled, never retried — and this daemon has no fallback and no
    /// local tier to reroute onto.
    ///
    /// Mutation (run 2026-10-07, reverted): **a terminal error ends
    /// `completed`** (`ended_by`'s fall-through arm): 1 red, this test.
    #[test]
    fn failed() {
        let (_provider, _rig, results) = one_call(
            "st-failed",
            "ST-FAILED",
            json!([{ "task": "ST-BROKEN try", "name": "broken" }]),
            vec![(child("ST-BROKEN"), MockResponse::status(401))],
            "",
        );
        let result = result_named(&results, "broken");
        assert_eq!(result["status"], "failed", "{result}");
        assert_eq!(result["report"], "");
        let error = result["error"].as_str().unwrap_or_default();
        let code = error.split(':').next().unwrap_or_default();
        assert!(
            code.parse::<i64>().is_ok_and(|c| c < 0),
            "the error leads with its code: {error}"
        );
        assert_no_boundary_bytes();
    }
}

// ---------------------------------------------------------------------------
// AC-16 / BR-12 — a skill in a child
// ---------------------------------------------------------------------------

mod skills {
    use super::*;

    const GIB: u64 = 1024 * 1024 * 1024;

    /// The skill body's marker: ordinary prose, and *supposed* to reach the
    /// wire wherever the expansion legitimately does (LESSON-624 — the leak
    /// marker stays in `secrets/prod.env`).
    const BODY: &str = "USER-SKILL-BODY-AC16";

    /// One leg: a child that invokes the `~/.claude` user skill `probe`, in a
    /// repo-rooted session with the shipped boundary globs in force and a
    /// scripted local tier to be rerouted onto — and, when `skills_boundary`,
    /// a `local-only` glob over the skills directory, which is what makes a
    /// user skill's expansion boundary content since REQ-619 gave it a
    /// `~`-scoped identity (its AC-7).
    fn leg(tag: &str, marker: &str, skills_boundary: bool) -> (MockProvider, Rig) {
        let provider = MockProvider::start_matching(
            vec![
                (
                    child("AC16-SKILLED"),
                    calls("skill", &json!({ "name": "probe" })),
                ),
                (child("AC16-SKILLED"), says("the child answered remotely")),
                (
                    parent(marker),
                    dispatch(
                        json!([{ "task": "AC16-SKILLED use the probe skill", "name": "skilled" }]),
                    ),
                ),
                (parent(marker), says("the parent answered remotely")),
            ],
            unmatched(),
        );
        let mut config = base_config(&provider);
        if skills_boundary {
            config.push_str(
                "[[boundaries]]\npath_glob = \"**/.claude/skills/**\"\nmode = \"local-only\"\n\n",
            );
        }
        let rig = Rig::with_config(tag, &config, |ws| {
            let home = ws.user_skill(
                "probe",
                &format!("{BODY} Describe the repository in one line.\n"),
            );
            let script = ws.write_script(&["Answered locally."; 6].join("\n---\n"));
            DaemonOptions::default()
                .env("TETON_PROBE_RAM_BYTES", (16 * GIB).to_string())
                .env("TETON_PROBE_DISK_BYTES", "500000000000")
                .env("TETON_PROBE_GPU", "apple-silicon")
                .env("HOME", home.display().to_string())
                .script(script)
        });
        (provider, rig)
    }

    /// **AC-16 (BR-12, BR-9): a child may call `skill`; the expansion lands in
    /// the child's context, not the parent's; and a user skill whose
    /// expansion a boundary covers pins the child local and, through the
    /// result, the parent.**
    ///
    /// *Benign leg — no boundary over the skills directory.* The child's next
    /// request leaves carrying the expansion (it is in the child's context);
    /// the parent's next request leaves carrying the child's report and **not**
    /// the expansion (it never entered the parent's); nothing is blocked.
    ///
    /// *Pinned leg — `**/.claude/skills/**` is `local-only`.* The child made
    /// one remote request (the one that called the skill) and none after; the
    /// parent made one remote request (the one that dispatched) and none after
    /// the result; a `privacy_block` naming the skill file fires inside the
    /// call (the child's) and another after it (the parent's); both turns end
    /// on the local tier, and the expansion never reaches the wire.
    ///
    /// # Mutation (run 2026-10-07, reverted)
    ///
    /// - **The result block sheds the children's provenance** (`result_of`
    ///   without the union): red here at the pinned leg (the parent's next
    ///   call leaves), among 4 — the others are `provenance_egress`'s two
    ///   AC-10 tests and its in-process agent test.
    /// - **Every result block pins** (its boundary bit forced on): red here at
    ///   the benign leg, among 16.
    #[test]
    fn user_skill_in_child_pins_parent() {
        // --- Benign: the expansion stays in the child. ----------------------
        let (provider, mut rig) = leg("ac16-free", "AC16-FREE", false);
        let response = rig.prompt("AC16-FREE have a child use the probe skill");
        assert_eq!(response["result"]["stop_reason"], "end_turn", "{response}");
        assert!(
            rig.client.saw_event("skill_invoked"),
            "{:?}",
            rig.client.event_names()
        );
        let children = child_requests(&provider, "AC16-SKILLED");
        assert_eq!(children.len(), 2, "the child's follow-up left the machine");
        assert!(
            children[1].contains(BODY),
            "the expansion is in the child's context"
        );
        let parents = parent_requests(&provider, "AC16-FREE");
        assert_eq!(parents.len(), 2, "the parent's follow-up left the machine");
        assert!(
            parents[1].contains("the child answered remotely"),
            "fixture: the parent's follow-up carries the child's report"
        );
        assert!(
            !parents.iter().any(|b| b.contains(BODY)),
            "the expansion never entered the parent's context"
        );
        assert!(rig.client.events_named("privacy_block").is_empty());
        drop(rig);

        // --- Pinned: the skill file is boundary content. --------------------
        let (provider, mut rig) = leg("ac16-pinned", "AC16-PINNED", true);
        let response = rig.prompt("AC16-PINNED have a child use the probe skill");
        assert_eq!(response["result"]["stop_reason"], "end_turn", "{response}");
        assert!(
            rig.client.saw_event("skill_invoked"),
            "{:?}",
            rig.client.event_names()
        );
        assert_eq!(
            child_requests(&provider, "AC16-SKILLED").len(),
            1,
            "after the skill, the child made no remote request"
        );
        assert_eq!(
            parent_requests(&provider, "AC16-PINNED").len(),
            1,
            "after the result, the parent made no remote request"
        );
        assert!(
            !bodies(&provider).iter().any(|b| b.contains(BODY)),
            "the expansion never reached the wire"
        );
        let call_end = index_where(&rig.client, |e| e["event"] == "agent_call_finished")
            .expect("the call finished");
        let blocks: Vec<(usize, String)> = rig
            .client
            .events()
            .iter()
            .enumerate()
            .filter(|(_, e)| e["event"] == "privacy_block")
            .map(|(i, e)| (i, e["path"].as_str().unwrap_or_default().to_owned()))
            .collect();
        assert!(
            blocks
                .iter()
                .any(|(i, path)| *i < call_end && path.contains(".claude/skills/probe")),
            "the child was blocked on the skill file: {blocks:?}"
        );
        assert!(
            blocks
                .iter()
                .any(|(i, path)| *i > call_end && path.contains(".claude/skills/probe")),
            "the parent was blocked on the skill file, through the result: {blocks:?}"
        );
        let id = child_id_of(&rig.client, "skilled");
        assert_eq!(finished_status(&rig.client, &id), "completed");
        assert_no_boundary_bytes();
    }
}

// ---------------------------------------------------------------------------
// AC-17 — one transcript file, through the daemon
// ---------------------------------------------------------------------------

mod transcript {
    use super::*;

    /// The records of `session`'s transcript once it holds an
    /// `agent_call_finished` and the parent's closing text, read with a stock
    /// JSON parser — every file in the directory, to prove there is one.
    fn await_records(
        dir: &std::path::Path,
        session: &str,
    ) -> (Vec<std::path::PathBuf>, Vec<Value>) {
        let deadline = Instant::now() + WINDOW;
        loop {
            let files: Vec<std::path::PathBuf> = std::fs::read_dir(dir)
                .map(|listing| {
                    listing
                        .flatten()
                        .map(|e| e.path())
                        .filter(|p| p.extension().is_some_and(|x| x == "jsonl"))
                        .collect()
                })
                .unwrap_or_default();
            let mine: Vec<std::path::PathBuf> = files
                .iter()
                .filter(|p| p.to_string_lossy().contains(session))
                .cloned()
                .collect();
            if let [path] = mine.as_slice() {
                let records: Vec<Value> = std::fs::read_to_string(path)
                    .unwrap_or_default()
                    .lines()
                    .filter_map(|line| serde_json::from_str(line).ok())
                    .collect();
                // The parent's `agent` result and each child's `read` result.
                let done = records.iter().any(|r| r["kind"] == "agent_call_finished")
                    && records
                        .iter()
                        .filter(|r| r["kind"] == "tool_result")
                        .count()
                        >= 3;
                if done {
                    return (mine, records);
                }
            }
            assert!(
                Instant::now() < deadline,
                "{session}'s transcript never held the call: {files:?}"
            );
            std::thread::sleep(Duration::from_millis(25));
        }
    }

    /// **AC-17 (BR-13): the transcript file for a session with one `agent`
    /// call holds the parent turn's records and every child's, each child
    /// record carrying `child_id` and `parent_turn_id` — in one file.**
    ///
    /// The in-process half (`transcript.rs`) drives the emitters by hand; this
    /// is the same claim through the shipped binary, where the only way a
    /// child's records can be tagged is the runner handing each child its
    /// `for_child` emitter. Two children each `read` a file (named for the
    /// child, LESSON-610) and answer; the parent then answers. The file is read
    /// as a stranger would, and the tree reconstructed from it: every child
    /// tool record names a child the file's own `agent_child_started` records
    /// announce, under the turn its `prompt_submitted` opened. Benign half: the
    /// parent's own tool records carry neither id.
    ///
    /// Mutation (run 2026-10-07, reverted): **`for_child` stamps nothing**
    /// (each child records through an emitter indistinguishable from the
    /// parent's): 5 red, this test among them.
    #[test]
    fn one_file_parent_and_children() {
        let provider = MockProvider::start_matching(
            vec![
                (
                    child("AC17-LEFT"),
                    calls("read", &json!({ "path": "left.txt" })),
                ),
                (child("AC17-LEFT"), says("left read its file")),
                (
                    child("AC17-RIGHT"),
                    calls("read", &json!({ "path": "right.txt" })),
                ),
                (child("AC17-RIGHT"), says("right read its file")),
                (
                    parent("AC17-PARENT"),
                    dispatch(json!([
                        { "task": "AC17-LEFT read left.txt", "name": "left" },
                        { "task": "AC17-RIGHT read right.txt", "name": "right" },
                    ])),
                ),
                (parent("AC17-PARENT"), says("Both files read.")),
            ],
            unmatched(),
        );
        let ws_dir = std::cell::RefCell::new(std::path::PathBuf::new());
        let mut config = base_config(&provider);
        // The directory is the workspace's own, so it is only known once the
        // workspace is; the closure below writes the table before the spawn.
        let rig = Rig::with_config("ac17", "", |ws| {
            let dir = ws.root.join("transcripts");
            config.push_str(&format!(
                "[transcript]\nenabled = true\ndir = \"{}\"\nretain_days = 0\n\n",
                dir.display()
            ));
            ws.write_config(&config);
            std::fs::write(ws.repo.join("left.txt"), "left contents\n").unwrap();
            std::fs::write(ws.repo.join("right.txt"), "right contents\n").unwrap();
            *ws_dir.borrow_mut() = dir;
            DaemonOptions::default()
        });
        let mut rig = rig;
        let response = rig.prompt("AC17-PARENT read both files with two children");
        assert_eq!(response["result"]["stop_reason"], "end_turn", "{response}");
        let dir = ws_dir.borrow().clone();
        let (files, records) = await_records(&dir, &rig.session);
        assert_eq!(files.len(), 1, "one session, one file: {files:?}");

        let kinds: Vec<&str> = records.iter().filter_map(|r| r["kind"].as_str()).collect();
        let prompt = records
            .iter()
            .find(|r| r["kind"] == "prompt_submitted")
            .unwrap_or_else(|| panic!("the parent's prompt is recorded: {kinds:?}"));
        let turn = prompt["turn_id"].clone();
        let announced: Vec<(Value, Value)> = records
            .iter()
            .filter(|r| r["kind"] == "agent_child_started")
            .map(|r| (r["child_id"].clone(), r["name"].clone()))
            .collect();
        assert_eq!(
            announced.len(),
            2,
            "both children announced in the file: {kinds:?}"
        );
        for record in records
            .iter()
            .filter(|r| r["kind"] == "agent_child_started")
        {
            assert_eq!(record["parent_turn_id"], turn, "{record}");
        }

        let tool_records: Vec<&Value> = records
            .iter()
            .filter(|r| r["kind"] == "tool_call_input" || r["kind"] == "tool_result")
            .collect();
        let parents: Vec<&&Value> = tool_records
            .iter()
            .filter(|r| r.get("child_id").is_none())
            .collect();
        assert_eq!(
            parents.len(),
            2,
            "the parent's agent call: its input and result"
        );
        assert!(parents
            .iter()
            .all(|r| r.get("parent_turn_id").is_none() && r["tool"] != "read"));
        for (child_id, name) in &announced {
            let mine: Vec<&&Value> = tool_records
                .iter()
                .filter(|r| r["child_id"] == *child_id)
                .collect();
            assert_eq!(
                mine.iter()
                    .map(|r| r["kind"].as_str().unwrap_or_default())
                    .collect::<Vec<_>>(),
                vec!["tool_call_input", "tool_result"],
                "{name}'s read, in its own order"
            );
            for record in &mine {
                assert_eq!(record["parent_turn_id"], turn, "{record}");
            }
            let file = format!("{}.txt", name.as_str().unwrap_or_default());
            assert_eq!(
                mine[0]["input"]["path"],
                json!(file),
                "{name} read its own file"
            );
            assert!(
                mine[1]["output"].as_str().is_some_and(
                    |o| o.contains(&format!("{} contents", name.as_str().unwrap_or_default()))
                ),
                "{}",
                mine[1]
            );
        }
        for kind in [
            "agent_call_started",
            "agent_child_finished",
            "agent_call_finished",
        ] {
            assert!(kinds.contains(&kind), "{kind} is in the file: {kinds:?}");
        }
        drop(rig);
        assert_no_boundary_bytes();
    }
}
