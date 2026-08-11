//! REQ-569 acceptance evidence, at the raw RPC surface, against the real
//! daemon binary (TASK-109).
//!
//! Every test here spawns the **shipped `teton-code` binary** and speaks
//! NDJSON JSON-RPC to its socket directly. Nothing goes through the CLI — which
//! is AC-8 in file form: a gate a client enforces is a rendering choice, and
//! BUG-155 is what happens when the test surface is not the enforcement surface
//! (LESSON-484).
//!
//! ## What is asserted here, and what is asserted elsewhere
//!
//! | AC | Pinned by |
//! |----|-----------|
//! | AC-1 | [`a_client_driven_from_a_genuine_daemon_descendant_is_refused_at_every_door`] |
//! | AC-2 / AC-3 | `e2e::conversation_carry::client_bs_prompt_carries_the_conversation_client_a_left_behind`, and [`a_consent_the_requester_granted_itself_is_named_as_such_in_the_daemon_log`]'s second leg (the resume arm, end to end) |
//! | AC-4 | `multi_client::a_monitor_declaration_is_refused_without_a_monitor_scope_grant`, `multi_client::a_monitor_consent_granted_by_an_attached_client_produces_a_working_monitor` |
//! | AC-5 | [`a_session_id_read_from_session_list_is_not_standing_to_attach`] |
//! | AC-6 | `server::tests::a_denied_or_timed_out_consent_leaves_the_grant_registry_empty` (TASK-108) |
//! | AC-7 | [`the_single_client_create_prompt_stream_flow_asks_for_nothing_new`] |
//! | AC-9 | `multi_client::an_unattached_connection_cannot_answer_another_sessions_permission_prompt` (TASK-107) |
//! | AC-10 | `multi_client::session_list_omits_title_and_cwd_from_unattached_connections` (TASK-105) |
//!
//! ## AC-1 is driven from a genuine descendant, not an injected verdict
//!
//! The spec's amended AC-1 asks for a client driven **from a process that is a
//! descendant of the daemon** — the shape a tool child or an MCP subprocess
//! actually has. That is what this file does: the daemon is asked to run a
//! `shell` tool whose command re-executes this very test binary as a probe, and
//! the probe opens its own connections to the daemon socket. The fallback the
//! task allowed (injecting the ancestry verdict at the seam) is **not** used
//! here; it is what `multi_client::a_connection_from_the_daemons_own_process_
//! tree_is_refused_attach_and_monitor` does one layer down, and the two are
//! deliberately different fixtures. The probe records its own parent chain and
//! the test asserts the daemon's pid is in it, so "a descendant" is a checked
//! property rather than a comment.
//!
//! ## How an absence is asserted
//!
//! Never by a timer. Every negative here is bounded by a positive control in
//! the same test, on the same daemon and the same connection: the frame that
//! must not appear is one the test then *makes* appear by legitimate means, so
//! a passing negative cannot be the daemon merely being slow, misconfigured, or
//! unable to publish that frame at all. One connection's frame stream is FIFO,
//! which is what turns "it never arrived" into a decidable fact.

#[path = "e2e/harness.rs"]
mod harness;

use std::io::{BufRead, BufReader, Write};
use std::net::Shutdown;
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::Duration;

use serde_json::{json, Value};

use teton_protocol::jsonrpc::error_code;
use teton_protocol::{PROTOCOL_VERSION_MAX, PROTOCOL_VERSION_MIN};

use crate::harness::{
    openai_turn, remote_provider_block, tier_block, Daemon, DaemonOptions, MockProvider,
    MockResponse, Workspace,
};

const GIB: u64 = 1024 * 1024 * 1024;

/// How long any single read waits before the test fails.
///
/// Generous, because it is never the thing under test: every claim here is
/// settled by which frame arrives, not by how long one takes. It exists so a
/// wedged daemon fails the test instead of hanging the suite.
const READ_DEADLINE: Duration = Duration::from_secs(20);

/// The phrase the daemon logs when a connection approved its own attach
/// consent (REQ-569 BR-6's second arm).
///
/// A literal copy of `server::self_approval_line`'s wording, which has a unit
/// test asserting the same substring. Two copies of a sentence is the price of
/// asserting a log line from outside the process; the pair fails together if it
/// is reworded, which is the point — this line is the only visibility the
/// self-approval residual has.
const SELF_APPROVAL_PHRASE: &str = "approved its own attach consent";

/// A deterministic above-floor probe with a scripted local tier — present only
/// so the first-run model-consent flow stays out of the way, exactly as
/// `e2e::conversation_carry` uses it.
fn probe_with_local(script: PathBuf) -> DaemonOptions {
    DaemonOptions::default()
        .env("TETON_PROBE_RAM_BYTES", (16 * GIB).to_string())
        .env("TETON_PROBE_DISK_BYTES", "500000000000")
        .env("TETON_PROBE_GPU", "apple-silicon")
        .script(script)
}

// ---------------------------------------------------------------------------
// A raw NDJSON client
// ---------------------------------------------------------------------------

/// A minimal blocking JSON-RPC client over the daemon socket.
///
/// Deliberately not `e2e::harness::Client`: that one answers consent prompts
/// from a reader thread, and the tests here have to decide *when* a user says
/// yes, say no, and observe a prompt that is never answered. Single-threaded,
/// so every frame this client has seen is a fact about the order the daemon
/// sent them in.
struct RawClient {
    writer: UnixStream,
    reader: BufReader<UnixStream>,
    next_id: i64,
    /// Every event notification this client has read, in arrival order.
    events: Vec<Value>,
    /// Whether to answer a tool `permission_request` with "allow once".
    ///
    /// On for a client driving a turn (the shipped policy asks before `shell`),
    /// off for the descendant probe — which must answer nothing it was not
    /// asked, so that anything it *does* receive is the daemon's doing.
    auto_approve_tools: bool,
}

impl RawClient {
    fn connect(socket: &Path) -> Self {
        let writer = UnixStream::connect(socket).expect("connect daemon socket");
        writer.set_read_timeout(Some(READ_DEADLINE)).unwrap();
        let read_half = writer.try_clone().unwrap();
        read_half.set_read_timeout(Some(READ_DEADLINE)).unwrap();
        Self {
            writer,
            reader: BufReader::new(read_half),
            next_id: 1,
            events: Vec::new(),
            auto_approve_tools: true,
        }
    }

    fn write_frame(&mut self, message: &Value) {
        let mut text = serde_json::to_string(message).unwrap();
        text.push('\n');
        self.writer.write_all(text.as_bytes()).unwrap();
        self.writer.flush().unwrap();
    }

    /// Send a request without waiting for its response, returning its id.
    ///
    /// The pending form matters: an ungranted `session/attach` does not answer
    /// until somebody decides it, so the test has to be able to drive the
    /// deciding connection while this one is still waiting.
    fn send(&mut self, method: &str, params: Value) -> i64 {
        let id = self.next_id;
        self.next_id += 1;
        self.write_frame(&json!({
            "jsonrpc": "2.0",
            "id": id,
            "method": method,
            "params": params,
        }));
        id
    }

    /// One frame off the wire. Event notifications are recorded on the way past.
    fn read_frame(&mut self) -> Value {
        let mut line = String::new();
        let read = self.reader.read_line(&mut line);
        let n = match read {
            Ok(n) => n,
            // The deadline is shorter than the daemon's shipped consent window
            // on purpose, so a request waiting on a decision nobody will make
            // fails here — with this sentence — rather than by eventually
            // succeeding as a timeout thirty seconds later.
            Err(e) => panic!(
                "no frame from the daemon within {READ_DEADLINE:?} ({e}): it is wedged, or \
                 something is awaiting a consent decision nobody is going to make"
            ),
        };
        assert!(n > 0, "the daemon closed the connection");
        let frame: Value = serde_json::from_str(&line).unwrap_or_else(|e| panic!("{e}: {line}"));
        if frame.get("method").and_then(Value::as_str) == Some("event") {
            let event = frame["params"].clone();
            if self.auto_approve_tools && event["event"].as_str() == Some("permission_request") {
                if let Some(request_id) = event["request_id"].as_str().map(str::to_owned) {
                    self.send(
                        "permission/respond",
                        json!({
                            "request_id": request_id,
                            "outcome": { "outcome": "selected", "option_id": "allow_once" },
                        }),
                    );
                }
            }
            self.events.push(event);
        }
        frame
    }

    /// Read frames until the response to `id` arrives.
    fn read_response(&mut self, id: i64) -> Value {
        loop {
            let frame = self.read_frame();
            if frame.get("id").and_then(Value::as_i64) == Some(id) {
                return frame;
            }
        }
    }

    fn call(&mut self, method: &str, params: Value) -> Value {
        let id = self.send(method, params);
        self.read_response(id)
    }

    fn handshake(&mut self, name: &str) -> Value {
        self.handshake_declaring(name, false)
    }

    fn handshake_declaring(&mut self, name: &str, monitor: bool) -> Value {
        self.call(
            "handshake",
            json!({
                "client_kind": "cli",
                "client_name": name,
                "client_version": "0.1.0",
                "protocol_min": PROTOCOL_VERSION_MIN,
                "protocol_max": PROTOCOL_VERSION_MAX,
                "monitor": monitor,
            }),
        )
    }

    /// Read frames until the event named `expect` arrives.
    fn wait_for_event(&mut self, expect: &str) -> Value {
        loop {
            let frame = self.read_frame();
            if frame.get("method").and_then(Value::as_str) != Some("event") {
                continue;
            }
            if frame["params"]["event"].as_str() == Some(expect) {
                return frame["params"].clone();
            }
        }
    }

    /// Read frames until the daemon reports `live_connection_count` live
    /// connections after a client left (REQ-565's `daemon_lifetime` event).
    ///
    /// The ordering marker the resume leg needs. `ClientGuard::drop` publishes
    /// this stage, and `handle_client`'s teardown drops that guard **after** it
    /// has released the connection's consent surface — so a client that has
    /// seen this frame knows the departed connections can no longer be routed a
    /// prompt. A sleep would only be a guess about the same thing.
    fn wait_for_client_count(&mut self, live: u64) -> Value {
        loop {
            let event = self.wait_for_event("daemon_lifetime");
            if event["stage"].as_str() == Some("client_disconnected")
                && event["live_connection_count"].as_u64() == Some(live)
            {
                return event;
            }
        }
    }

    fn events_named(&self, name: &str) -> Vec<&Value> {
        self.events
            .iter()
            .filter(|e| e["event"].as_str() == Some(name))
            .collect()
    }

    fn create_session(&mut self, mode: &str, phase: Option<&str>) -> String {
        let mut params = json!({ "mode": mode });
        if let Some(phase) = phase {
            params["phase"] = json!(phase);
        }
        let created = self.call("session/create", params);
        created["result"]["session_id"]
            .as_str()
            .unwrap_or_else(|| panic!("session/create failed: {created}"))
            .to_owned()
    }

    /// Answer a consent request, the way a user's client does.
    fn answer_consent(&mut self, request_id: &str, granted: bool) -> Value {
        let outcome = if granted { "granted" } else { "denied" };
        self.call(
            "attach/consent",
            json!({
                "request_id": request_id,
                "outcome": { "outcome": outcome },
            }),
        )
    }

    /// Close the connection the way a departing client does — on the
    /// connection, not on one descriptor, so the daemon sees the EOF at once.
    fn disconnect(&mut self) {
        let _ = self.writer.shutdown(Shutdown::Both);
    }
}

/// The session ids in a `session/list` response.
fn listed_ids(response: &Value) -> Vec<String> {
    response["result"]["sessions"]
        .as_array()
        .unwrap_or_else(|| panic!("session/list failed: {response}"))
        .iter()
        .filter_map(|row| row["session_id"].as_str().map(str::to_owned))
        .collect()
}

/// The `request_id` of a consent prompt.
fn consent_request_id(event: &Value) -> String {
    event["request_id"]
        .as_str()
        .unwrap_or_else(|| panic!("a consent prompt with no request_id: {event}"))
        .to_owned()
}

/// A prompt frame with one text block.
fn text_prompt(session_id: &str, text: &str) -> Value {
    json!({
        "session_id": session_id,
        "prompt": [{ "type": "text", "text": text }],
    })
}

// ---------------------------------------------------------------------------
// AC-1 — a genuine daemon descendant
// ---------------------------------------------------------------------------

/// Where the probe child finds the daemon socket.
const PROBE_SOCKET_ENV: &str = "TETON_ATTACH_PROBE_SOCKET";
/// Where the probe child writes what the daemon answered it.
const PROBE_OUT_ENV: &str = "TETON_ATTACH_PROBE_OUT";
/// The test name the probe child runs — this binary, re-entered.
const PROBE_TEST_NAME: &str = "the_daemon_descendant_probe_body";

/// **AC-1.** A client driven from a process that is a genuine descendant of the
/// daemon is refused attach, refused a `monitor` declaration, and refused a
/// prompt against another connection's session — and **no consent prompt is
/// raised for any of them**.
///
/// # The fixture is the claim
///
/// The daemon is given one scripted turn that calls the `shell` tool, and the
/// command it runs re-executes this test binary as [`the_daemon_descendant_probe_body`].
/// So the connections under test are opened by a process the daemon spawned:
/// `tetond` → `sh` → probe. That is the shape of a tool child and of an MCP
/// server subprocess, which is the population BR-4 excludes and the reason this
/// REQ exists. The probe records its own parent chain and this test asserts the
/// daemon's pid is in it, so the fixture's central property is checked rather
/// than assumed.
///
/// # The negative and its control
///
/// The owner connection is attached to the session, so it is the surface every
/// consent prompt in this daemon would be rendered at — for the probe's attach
/// (BR-6's first arm) and for its monitor declaration (the monitor rule's "any
/// attached peer") alike. Its frame stream is read to the end of the turn
/// before the control begins, and the daemon's fence puts every event delivered
/// to that connection on the wire ahead of the response ordering it. So "the
/// owner saw no `attach_consent_requested`" is decided, not waited for.
///
/// Then the control: an ordinary same-UID client — not a descendant — asks for
/// exactly the same thing, and the owner *does* get the prompt, names the
/// asker, and lets it in. Without that leg, an empty prompt list would be
/// equally consistent with a daemon that cannot raise prompts at all.
#[test]
fn a_client_driven_from_a_genuine_daemon_descendant_is_refused_at_every_door() {
    let ws = Workspace::new("ac1-descendant");
    let probe_out = ws.root.join("probe.json");
    let probe_binary = std::env::current_exe().expect("this test binary's path");

    // What the daemon's own child is told to do: connect back to the socket it
    // can trivially find, and try every door. Env is set inside the command
    // rather than on the daemon, so the probe's inputs cannot be confused with
    // the daemon's environment (which is the thing ADR-A deliberately does not
    // key on).
    let command = format!(
        "{PROBE_SOCKET_ENV}='{socket}' {PROBE_OUT_ENV}='{out}' '{binary}' {PROBE_TEST_NAME} --exact",
        socket = ws.runtime_dir.join("teton").join("tetond.sock").display(),
        out = probe_out.display(),
        binary = probe_binary.display(),
    );
    let shell_args = json!({ "command": command }).to_string();

    let provider = MockProvider::start(
        vec![MockResponse::ok(openai_turn(
            "Checking something first.",
            Some(("c1", "shell", &shell_args)),
            120,
            20,
        ))],
        MockResponse::ok(openai_turn("Done.", None, 10, 5)),
    );

    let mut config = String::new();
    config.push_str(&remote_provider_block(
        "turns",
        &provider.openai_endpoint(),
        "deepseek-chat",
    ));
    config.push_str(&tier_block("build", "turns"));
    ws.write_config(&config);
    let script = ws.write_script("Unused by this fixture.");
    let daemon = Daemon::spawn(&ws, probe_with_local(script));

    let mut owner = RawClient::connect(daemon.socket());
    assert!(
        owner.handshake("owner").get("result").is_some(),
        "the owner's handshake must succeed"
    );
    let session = owner.create_session("structured", Some("implement"));

    // The turn runs the tool; the tool is the probe. `session/prompt` returns
    // when the whole turn is over, so the probe has run and exited by here.
    let turn = owner.call(
        "session/prompt",
        text_prompt(&session, "Edit the loader and verify the build."),
    );
    assert!(
        turn.get("result").is_some(),
        "the turn that carries the probe must complete: {turn}\ndaemon log:\n{}",
        daemon.log()
    );

    let report: Value =
        serde_json::from_str(&std::fs::read_to_string(&probe_out).unwrap_or_else(|e| {
            panic!(
                "the daemon's own child never reported ({e}). \
                 provider saw {} request(s); daemon log:\n{}",
                provider.request_count(),
                daemon.log()
            )
        }))
        .expect("probe report is json");

    // (0) The fixture is what it claims to be: the probe's parent chain
    // contains the daemon's pid.
    let ancestors: Vec<u64> = report["ancestors"]
        .as_array()
        .expect("probe reported no ancestor chain")
        .iter()
        .filter_map(Value::as_u64)
        .collect();
    assert!(
        ancestors.contains(&u64::from(daemon.pid())),
        "AC-1 needs a genuine descendant: the probe's parent chain {ancestors:?} \
         does not contain the daemon's pid {}",
        daemon.pid()
    );
    assert_eq!(
        report["target"].as_str(),
        Some(session.as_str()),
        "the probe must have aimed at the owner's session, not one of its own: {report}"
    );

    // (1) Attach: forbidden outright — not merely ungranted. The distinction is
    // the ordering: a descendant holds no grant either, so a daemon that asked
    // the grant question first would refuse it too, and would look correct
    // right up until it started raising consent prompts for tool children.
    assert_eq!(
        report["attach"]["error"]["code"].as_i64(),
        Some(error_code::ATTACH_FORBIDDEN),
        "a daemon descendant's attach: {}",
        report["attach"]
    );

    // (2) The monitor declaration, refused at the handshake for the same
    // reason.
    assert_eq!(
        report["monitor"]["error"]["code"].as_i64(),
        Some(error_code::ATTACH_FORBIDDEN),
        "a daemon descendant's monitor declaration: {}",
        report["monitor"]
    );

    // (3) And a prompt against a session it never attached to.
    assert_eq!(
        report["prompt"]["error"]["code"].as_i64(),
        Some(error_code::NOT_ATTACHED),
        "a daemon descendant driving somebody else's session: {}",
        report["prompt"]
    );

    // The daemon says which gate refused, and says it about the ancestry rather
    // than about a missing grant — the one place the two are told apart.
    let log = daemon.log();
    assert!(
        log.contains(
            "refused session/attach for a connection because it descends from this \
                      daemon's own process tree"
        ),
        "the daemon must record the ancestry refusal of the attach; log:\n{log}"
    );
    assert!(
        log.contains(
            "refused a monitor declaration for a connection because it descends from \
                      this daemon's own process tree"
        ),
        "the daemon must record the ancestry refusal of the monitor declaration; log:\n{log}"
    );

    // (4) The negative that matters: not one of those three raised a question
    // for a user. The owner is the surface all of them would have been rendered
    // at, and it has read its stream to the end of the turn.
    assert!(
        owner.events_named("attach_consent_requested").is_empty(),
        "a daemon descendant must not be able to make a consent prompt appear on a \
         user's screen: {:?}",
        owner.events_named("attach_consent_requested")
    );

    // (5) The control: the same daemon, the same session, the same request —
    // from a process that is not a descendant. The prompt appears, names the
    // asker, and lets it in. This is what makes (4) evidence rather than an
    // empty list.
    let mut control = RawClient::connect(daemon.socket());
    assert!(control
        .handshake("positive-control")
        .get("result")
        .is_some());
    let control_attach = control.send("session/attach", json!({ "session_id": session.clone() }));
    let asked = owner.wait_for_event("attach_consent_requested");
    assert!(
        asked["requester"]
            .as_str()
            .is_some_and(|r| r.contains("positive-control")),
        "the prompt must name who is asking: {asked}"
    );
    owner.answer_consent(&consent_request_id(&asked), true);
    let attached = control.read_response(control_attach);
    assert_eq!(
        attached["result"]["session"]["session_id"].as_str(),
        Some(session.as_str()),
        "an ordinary same-UID client is let in once a user says yes: {attached}"
    );
    assert_eq!(
        owner.events_named("attach_consent_requested").len(),
        1,
        "exactly one prompt was ever raised on this daemon — the control's"
    );
}

/// The AC-1 probe, executed by the daemon's own `shell` tool as a child
/// process. **A fixture, not an assertion.**
///
/// It is a `#[test]` because that is how this binary re-enters itself: the
/// command the daemon runs is this executable plus
/// `--exact the_daemon_descendant_probe_body`. Without
/// [`PROBE_SOCKET_ENV`] set — every ordinary run of the suite — it does nothing
/// and passes, which is the honest outcome for a fixture nobody asked to run.
///
/// It answers nothing it is not asked (`auto_approve_tools` off) and asserts
/// nothing itself: it records what the daemon replied and writes it out. The
/// assertions live in the parent test, where a failure is legible.
#[test]
fn the_daemon_descendant_probe_body() {
    let (Ok(socket), Ok(out)) = (
        std::env::var(PROBE_SOCKET_ENV),
        std::env::var(PROBE_OUT_ENV),
    ) else {
        return;
    };
    let socket = PathBuf::from(socket);

    let mut probe = RawClient::connect(&socket);
    probe.auto_approve_tools = false;
    let handshake = probe.handshake("daemon-descendant-probe");

    // A tool child finds its target the way anything else would: by asking.
    // `session/list` stays open on purpose (BR-8 — ids are names), which is
    // exactly why the refusal below has to come from somewhere else.
    let listed = probe.call("session/list", json!({}));
    let target = listed_ids(&listed)
        .into_iter()
        .next()
        .unwrap_or_else(|| "sess-none-listed".to_owned());

    let attach = probe.call("session/attach", json!({ "session_id": target.clone() }));
    let prompt = probe.call(
        "session/prompt",
        text_prompt(&target, "what has this session been told?"),
    );

    // The monitor declaration is a handshake, so it needs its own connection.
    let mut watcher = RawClient::connect(&socket);
    watcher.auto_approve_tools = false;
    let monitor = watcher.handshake_declaring("daemon-descendant-probe", true);

    let report = json!({
        "pid": std::process::id(),
        "ancestors": ancestor_chain(std::process::id()),
        "handshake": handshake,
        "target": target,
        "attach": attach,
        "prompt": prompt,
        "monitor": monitor,
    });
    std::fs::write(&out, serde_json::to_string(&report).unwrap()).expect("write probe report");
}

/// This process's parent chain, nearest first.
///
/// Shells out to `ps` rather than reading `/proc` or calling `sysctl`, because
/// the fixture has to work identically on macOS and Linux and the daemon's own
/// platform split is the thing under test — a probe that reimplemented it could
/// agree with a broken implementation.
fn ancestor_chain(pid: u32) -> Vec<u32> {
    let mut chain = Vec::new();
    let mut current = pid;
    // Bounded for the reason the daemon's own walk is: a cycle in a pid table
    // must not hang the probe.
    for _ in 0..16 {
        let Ok(output) = Command::new("ps")
            .args(["-o", "ppid=", "-p", &current.to_string()])
            .output()
        else {
            break;
        };
        let Ok(parent) = String::from_utf8_lossy(&output.stdout)
            .trim()
            .parse::<u32>()
        else {
            break;
        };
        if parent == 0 || parent == current {
            break;
        }
        chain.push(parent);
        current = parent;
    }
    chain
}

// ---------------------------------------------------------------------------
// AC-5 — a session id is not a credential
// ---------------------------------------------------------------------------

/// **AC-5 / AC-8, BR-1, BR-7, BR-8.** Knowing a session id does not enable
/// attaching to it: the peer learns the id the way any same-UID process would —
/// by asking `session/list`, which REQ-569 deliberately keeps open — and is
/// still refused, because the decision belongs to a user rather than to
/// whoever knows the name.
///
/// The refusal here is `CONSENT_DENIED`: a user was asked and said no. That is
/// the shape a real refusal takes now, and it is asserted at the daemon's own
/// RPC surface, never through a client that could be rendering a check of its
/// own.
///
/// **Bounded on both sides.** The denial is followed by the identical call on
/// the identical id, answered yes, which succeeds — so the refusal was the
/// user's decision rather than a broken path. And between them, the refused
/// peer is shown to hold *nothing*: its `session/prompt` and `session/clear`
/// are refused `NOT_ATTACHED`, which is what "a denied request leaves no
/// partial grant state" looks like from outside the daemon (BR-7).
#[test]
fn a_session_id_read_from_session_list_is_not_standing_to_attach() {
    let ws = Workspace::new("ac5-id-is-not-a-credential");
    ws.write_config("");
    let script = ws.write_script("Unused by this fixture.");
    let daemon = Daemon::spawn(&ws, probe_with_local(script));

    let mut owner = RawClient::connect(daemon.socket());
    assert!(owner.handshake("owner").get("result").is_some());
    let session = owner.create_session("freeform", None);

    let mut peer = RawClient::connect(daemon.socket());
    assert!(peer.handshake("curious-peer").get("result").is_some());
    let listed = peer.call("session/list", json!({}));
    assert!(
        listed_ids(&listed).contains(&session),
        "the listing stays open — that is what makes this test mean something: {listed}"
    );

    // --- The user says no. --------------------------------------------------
    let refused_attach = peer.send("session/attach", json!({ "session_id": session.clone() }));
    let asked = owner.wait_for_event("attach_consent_requested");
    assert!(
        asked["requester"]
            .as_str()
            .is_some_and(|r| r.contains("curious-peer")),
        "the prompt must name who is asking: {asked}"
    );
    owner.answer_consent(&consent_request_id(&asked), false);
    let refused = peer.read_response(refused_attach);
    assert_eq!(
        refused["error"]["code"].as_i64(),
        Some(error_code::CONSENT_DENIED),
        "a refused attach must say a decision was made, not that a grant was missing: {refused}"
    );

    // Nothing was left behind for the refused peer to use.
    let driven = peer.call(
        "session/prompt",
        text_prompt(&session, "what has this session been told?"),
    );
    assert_eq!(
        driven["error"]["code"].as_i64(),
        Some(error_code::NOT_ATTACHED),
        "a denied consent must not have attached the peer to anything: {driven}"
    );
    let cleared = peer.call("session/clear", json!({ "session_id": session.clone() }));
    assert_eq!(
        cleared["error"]["code"].as_i64(),
        Some(error_code::NOT_ATTACHED),
        "nor to anything it could clear: {cleared}"
    );

    // --- The control: the same id, the same call, a user who says yes. ------
    let allowed_attach = peer.send("session/attach", json!({ "session_id": session.clone() }));
    let asked = owner.wait_for_event("attach_consent_requested");
    owner.answer_consent(&consent_request_id(&asked), true);
    let attached = peer.read_response(allowed_attach);
    assert_eq!(
        attached["result"]["session"]["session_id"].as_str(),
        Some(session.as_str()),
        "the same request, approved, must succeed — the id was never the problem: {attached}"
    );
    let cleared = peer.call("session/clear", json!({ "session_id": session.clone() }));
    assert!(
        cleared.get("result").is_some(),
        "and the grant is real: an attached peer may drive the session: {cleared}"
    );
}

// ---------------------------------------------------------------------------
// AC-2 — what a granted attach is actually worth
// ---------------------------------------------------------------------------

/// **AC-2's second half.** A client that reached a live session through the
/// grant flow is an attached client for REQ-568's delivery rules too — it
/// receives that session's events from the grant onwards, and not one envelope
/// from before it.
///
/// The first half of AC-2 (a second legitimate client attaches through the
/// grant flow) is pinned by
/// `e2e::conversation_carry::client_bs_prompt_carries_the_conversation_client_
/// a_left_behind` and by the drive-side control in
/// [`a_session_id_read_from_session_list_is_not_standing_to_attach`]. What
/// neither shows is the **receive** side, which is the half REQ-568 owns and
/// the half a grant is supposed to buy.
///
/// **Decided by sequence numbers, not by waiting.** The owner clears the
/// session once before the grant and once after; both envelopes are
/// session-scoped and carry the bus's monotonic `seq`. One connection's stream
/// is FIFO, so if the peer had been given the first clear it would be the first
/// `context_cleared` the peer ever reads. Asserting that the peer's one and
/// only clear carries the *second* seq is therefore a decision, not a timeout:
/// the positive control (the second envelope does arrive) and the negative (the
/// first never did) are the same assertion.
#[test]
fn a_client_that_attached_through_the_grant_flow_receives_that_sessions_events() {
    let ws = Workspace::new("ac2-delivery");
    ws.write_config("");
    let script = ws.write_script("Unused by this fixture.");
    let daemon = Daemon::spawn(&ws, probe_with_local(script));

    let mut owner = RawClient::connect(daemon.socket());
    assert!(owner.handshake("owner").get("result").is_some());
    let session = owner.create_session("freeform", None);

    // Subscribed from here on, and attached to nothing.
    let mut peer = RawClient::connect(daemon.socket());
    assert!(peer.handshake("newcomer").get("result").is_some());

    // (1) An envelope published while the peer holds no grant.
    assert!(owner
        .call("session/clear", json!({ "session_id": session.clone() }))
        .get("result")
        .is_some());
    let before = owner
        .events_named("context_cleared")
        .last()
        .and_then(|e| e["seq"].as_u64())
        .expect("the owner receives its own session's clear");

    // (2) The grant.
    let attach = peer.send("session/attach", json!({ "session_id": session.clone() }));
    let asked = owner.wait_for_event("attach_consent_requested");
    owner.answer_consent(&consent_request_id(&asked), true);
    assert_eq!(
        peer.read_response(attach)["result"]["session"]["session_id"].as_str(),
        Some(session.as_str()),
        "the newcomer must be attached before the delivery claim means anything"
    );

    // (3) An envelope published after it.
    assert!(owner
        .call("session/clear", json!({ "session_id": session.clone() }))
        .get("result")
        .is_some());
    let after = owner
        .events_named("context_cleared")
        .last()
        .and_then(|e| e["seq"].as_u64())
        .expect("the owner receives the second clear too");
    assert!(
        after > before,
        "the bus's seq is monotonic: {before} → {after}"
    );

    let delivered = peer.wait_for_event("context_cleared");
    assert_eq!(
        delivered["session_id"].as_str(),
        Some(session.as_str()),
        "the envelope must be the granted session's: {delivered}"
    );
    assert_eq!(
        delivered["seq"].as_u64(),
        Some(after),
        "the newcomer's first clear must be the one published after its grant — \
         receiving the earlier one would mean the grant bought sight of the past: \
         {delivered}"
    );
    assert_eq!(
        peer.events_named("context_cleared").len(),
        1,
        "exactly one of the two clears was this connection's to see: {:?}",
        peer.events_named("context_cleared")
    );
}

// ---------------------------------------------------------------------------
// AC-7 — the flow every existing user has
// ---------------------------------------------------------------------------

/// **AC-7, the regression bar.** One client creates a session, prompts it, and
/// streams the reply — with **zero** new prompts and zero consent steps.
///
/// This is the flow every existing user has. A consent step leaking into it
/// would be the worst regression this REQ could ship, and it would be an easy
/// one to ship: the creator's attach and a stranger's attach arrive at the same
/// handler, and the only thing between them is `GrantRegistry::may_attach`
/// answering "this connection created it".
///
/// So the assertion is explicit and it is about *absence*: no
/// `attach_consent_requested`, no `attach_refused`, and no `permission_request`
/// either — this turn calls no tool, so a correct daemon asks its user nothing
/// at all between "create" and "here is your answer".
///
/// **The control is in the same test.** A second client then attaches to that
/// same session and the prompt appears immediately on the first client's
/// stream. So the empty list above is not an event this daemon cannot publish,
/// a name this test spelled wrong, or a client that was not listening.
#[test]
fn the_single_client_create_prompt_stream_flow_asks_for_nothing_new() {
    let provider = MockProvider::always(openai_turn(
        "The router allows three attempts.",
        None,
        120,
        20,
    ));

    let mut config = String::new();
    config.push_str(&remote_provider_block(
        "turns",
        &provider.openai_endpoint(),
        "deepseek-chat",
    ));
    config.push_str(&tier_block("build", "turns"));

    let ws = Workspace::new("ac7-regression-bar");
    ws.write_config(&config);
    let script = ws.write_script("Unused by this fixture.");
    let daemon = Daemon::spawn(&ws, probe_with_local(script));

    let mut only = RawClient::connect(daemon.socket());
    assert!(only.handshake("solo").get("result").is_some());
    let session = only.create_session("structured", Some("implement"));
    let turn = only.call(
        "session/prompt",
        text_prompt(&session, "How many retry attempts does the router allow?"),
    );
    assert_eq!(
        turn["result"]["stop_reason"].as_str(),
        Some("end_turn"),
        "the single-client turn must complete: {turn}\ndaemon log:\n{}",
        daemon.log()
    );

    // It genuinely streamed — otherwise "nothing was asked" would be true of a
    // flow that never happened.
    let streamed: String = only
        .events_named("session_update")
        .iter()
        .filter(|e| e["update"]["kind"].as_str() == Some("agent_message_chunk"))
        .filter_map(|e| e["update"]["text"].as_str())
        .collect();
    assert!(
        streamed.contains("three attempts"),
        "the reply must have reached this client as a stream: {:?}",
        only.events_named("session_update")
    );

    // AC-7: nothing new was asked of the user, and nothing old either.
    for quiet in [
        "attach_consent_requested",
        "attach_refused",
        "permission_request",
    ] {
        assert!(
            only.events_named(quiet).is_empty(),
            "the single-client flow must not raise `{quiet}`: {:?}",
            only.events_named(quiet)
        );
    }

    // The control: this daemon can raise that prompt, on this session, to this
    // very connection — it simply had no reason to.
    let mut second = RawClient::connect(daemon.socket());
    assert!(second.handshake("second-client").get("result").is_some());
    let attach = second.send("session/attach", json!({ "session_id": session.clone() }));
    let asked = only.wait_for_event("attach_consent_requested");
    assert!(
        asked["requester"]
            .as_str()
            .is_some_and(|r| r.contains("second-client")),
        "the control prompt must name the second client: {asked}"
    );
    only.answer_consent(&consent_request_id(&asked), false);
    let refused = second.read_response(attach);
    assert_eq!(
        refused["error"]["code"].as_i64(),
        Some(error_code::CONSENT_DENIED),
        "and the control's decision is honoured: {refused}"
    );
}

// ---------------------------------------------------------------------------
// The self-approval residual, made visible
// ---------------------------------------------------------------------------

/// **BR-6's second arm, named out loud in the daemon log (TASK-109).**
///
/// When no connection is attached to the target session, the requesting
/// connection renders its own consent prompt. That arm is what makes the resume
/// flow work — the person reopening their session is the person being asked —
/// and for a headless same-UID process that is not a daemon descendant it means
/// approving itself with no human anywhere in the loop. An accepted, documented
/// residual of ADR-A's perimeter; **not** an invisible one.
///
/// Two legs, and the order is the assertion:
///
/// 1. An attached peer approves a newcomer's attach (BR-6's first arm). A grant
///    is minted — the newcomer attaches, which is the control proving a consent
///    genuinely completed — and the log says nothing about self-approval,
///    because nothing self-approved.
/// 2. Every attached client leaves, a fresh client attaches to the same
///    session, and it renders and answers its own prompt. Now the line appears.
///
/// Leg 2 is what bounds leg 1's negative: the same grep, the same log file, the
/// same daemon. A wording change, a missing call site, or a marker that fires
/// for every grant fails one leg or the other.
///
/// Leg 2 is also this file's end-to-end evidence for AC-3's harder half: the
/// resume flow through a daemon where **nobody is attached**, which
/// `conversation_carry`'s AC-9 test cannot show (its client A is still attached
/// when B asks).
///
/// # No sleeps
///
/// The transition from leg 1 to leg 2 needs the daemon to have released the
/// departed connections' consent surfaces, or the prompt would be routed to
/// clients that are gone. That is observed, not waited out: a third connection
/// watches for the `daemon_lifetime` frame reporting one live connection, which
/// `ClientGuard::drop` publishes *after* `handle_client` releases the surface.
#[test]
fn a_consent_the_requester_granted_itself_is_named_as_such_in_the_daemon_log() {
    let ws = Workspace::new("self-approval");
    ws.write_config("");
    let script = ws.write_script("Unused by this fixture.");
    let daemon = Daemon::spawn(&ws, probe_with_local(script));

    // The connection that watches the daemon's client count. Connected first,
    // so it observes every departure below.
    let mut watcher = RawClient::connect(daemon.socket());
    assert!(watcher.handshake("watcher").get("result").is_some());

    let mut owner = RawClient::connect(daemon.socket());
    assert!(owner.handshake("owner").get("result").is_some());
    let session = owner.create_session("freeform", None);

    // --- Leg 1: an attached peer approves. Not a self-approval. -------------
    let mut peer = RawClient::connect(daemon.socket());
    assert!(peer.handshake("peer").get("result").is_some());
    let peer_attach = peer.send("session/attach", json!({ "session_id": session.clone() }));
    let asked = owner.wait_for_event("attach_consent_requested");
    owner.answer_consent(&consent_request_id(&asked), true);
    let attached = peer.read_response(peer_attach);
    assert_eq!(
        attached["result"]["session"]["session_id"].as_str(),
        Some(session.as_str()),
        "the peer-approved attach must succeed — this is the grant that must NOT \
         be reported as a self-approval: {attached}"
    );
    assert!(
        !daemon.log().contains(SELF_APPROVAL_PHRASE),
        "a consent an attached peer approved is a two-party decision and must not be \
         logged as a self-approval; log:\n{}",
        daemon.log()
    );

    // --- Leg 2: everyone attached leaves; a fresh client resumes. -----------
    owner.disconnect();
    peer.disconnect();
    // The ordering marker: only the watcher is left, and the daemon says so
    // after it has stopped being able to route a prompt to either departed
    // connection.
    watcher.wait_for_client_count(1);

    let mut resumer = RawClient::connect(daemon.socket());
    assert!(resumer.handshake("resume-client").get("result").is_some());
    let resume_attach = resumer.send("session/attach", json!({ "session_id": session.clone() }));
    // Nobody is attached, so the prompt comes back to the connection that asked
    // — the arm under test. Receiving it *is* the proof of which arm ran.
    let own_prompt = resumer.wait_for_event("attach_consent_requested");
    assert!(
        own_prompt["requester"]
            .as_str()
            .is_some_and(|r| r.contains("resume-client")),
        "the requester renders a prompt about itself: {own_prompt}"
    );
    resumer.answer_consent(&consent_request_id(&own_prompt), true);
    let resumed = resumer.read_response(resume_attach);
    assert_eq!(
        resumed["result"]["session"]["session_id"].as_str(),
        Some(session.as_str()),
        "the resume flow must still work — that is what BR-6's second arm is for: {resumed}"
    );
    // AC-3's budget, spent exactly once: one prompt, one answer, attached.
    assert_eq!(
        resumer.events_named("attach_consent_requested").len(),
        1,
        "a resume costs at most one visible consent step: {:?}",
        resumer.events_named("attach_consent_requested")
    );

    // And the daemon now says, in as many words, that nobody but the requester
    // was involved.
    let log = daemon.log();
    let said = log.matches(SELF_APPROVAL_PHRASE).count();
    assert_eq!(
        said, 1,
        "exactly one self-approval happened on this daemon and the log must name it \
         exactly once; log:\n{log}"
    );
    let line = log
        .lines()
        .find(|line| line.contains(SELF_APPROVAL_PHRASE))
        .expect("the line the count above just found");
    assert!(
        line.contains("resume-client"),
        "the line must name which connection approved itself: {line}"
    );
    assert!(
        !watcher
            .events
            .iter()
            .any(|e| e["event"] == "attach_consent_requested"),
        "the self-rendered prompt is offered to the requester and to nobody else: {:?}",
        watcher.events_named("attach_consent_requested")
    );
}
