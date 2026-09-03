//! REQ-612 TASK-374 acceptance: the runtime carries a repository's own notes.
//!
//! The claims here are about **when** the file is read, **when** it is not, and
//! what a second client can see at the moment it learns the root moved — so
//! every test drives the daemon's own seams (`session/create` over a real
//! socket, `DaemonRuntime::set_session_cwd`, `DaemonRuntime::run_prompt_turn`)
//! rather than calling the loader and inspecting its answer. A loader test
//! cannot fail when the wiring that calls it is deleted, and the wiring is what
//! this task is (`repo_context/mod.rs`'s own suite owns the loader).
//!
//! ## Why the reader is injected
//!
//! BR-2's claim is that a switched-off session **never opens the file**. That is
//! a statement about calls that did not happen, and no inspection of the answer
//! can settle it: a loader that read the file and then discarded it answers
//! identically. So the runtime's `RepoFileReader` seam carries a recording
//! double ([`CountingFiles`]) that delegates to the real filesystem and counts
//! what was asked of it — the shape `DirLister` has, for its reason.
//!
//! ## AC → test map
//!
//! | rule | test |
//! |---|---|
//! | BR-1, AC-2 | [`create_loads_the_file_and_cd_rebuilds_it_before_session_root_changed`] |
//! | BR-2, AC-10 | [`the_session_switch_and_the_durable_switch_withhold_without_opening_the_file`] |
//! | BR-6, AC-8 | [`an_edit_between_prompts_is_resident_on_the_next_and_a_mid_turn_edit_is_not`] |
//! | OQ-4 | [`a_boundary_configured_mid_session_withholds_the_notes_at_the_next_refresh`] |
//!
//! ## Mutation table
//!
//! | Mutation | Test that fails |
//! |---|---|
//! | the create call site deleted | [`create_loads_the_file_and_cd_rebuilds_it_before_session_root_changed`] |
//! | the `/cd` load moved **after** the `session_root_changed` publish | [`create_loads_the_file_and_cd_rebuilds_it_before_session_root_changed`] |
//! | the `/cd` load deleted (the session keeps the old repository's notes) | [`create_loads_the_file_and_cd_rebuilds_it_before_session_root_changed`] |
//! | a non-`project` root read anyway (the `home` leg keeps a block) | [`create_loads_the_file_and_cd_rebuilds_it_before_session_root_changed`] |
//! | `session/context` reachable without `may_drive` | [`create_loads_the_file_and_cd_rebuilds_it_before_session_root_changed`] |
//! | the session switch consulted but the file opened anyway | [`the_session_switch_and_the_durable_switch_withhold_without_opening_the_file`] |
//! | `[context] repo_file = false` read as a *render* switch rather than a *read* switch | [`the_session_switch_and_the_durable_switch_withhold_without_opening_the_file`] |
//! | `/context on` deferring the re-load to the next turn | [`the_session_switch_and_the_durable_switch_withhold_without_opening_the_file`] |
//! | `/context off` writing the durable default | [`the_session_switch_and_the_durable_switch_withhold_without_opening_the_file`] |
//! | the turn-start refresh deleted (an edit never becomes resident) | [`an_edit_between_prompts_is_resident_on_the_next_and_a_mid_turn_edit_is_not`] |
//! | the refresh moved **inside** the tool loop (a mid-turn edit leaks into iteration 2) | [`an_edit_between_prompts_is_resident_on_the_next_and_a_mid_turn_edit_is_not`] |
//! | the refresh's boundary re-check dropped (OQ-4) | [`a_boundary_configured_mid_session_withholds_the_notes_at_the_next_refresh`] |
//! | `route.harness.repo_context` never stamped | every test that reads the wire |
//!
//! ## What is not here
//!
//! The loader's own gates — the candidate order, the symlinked entry, the
//! non-UTF-8 file, the read ceiling — are `repo_context`'s unit suite, which can
//! plant a fixture the filesystem cannot hold. The block's bytes and its
//! sanitization are `render.rs`'s and `turn_loop.rs`'s. What this file adds is
//! the wiring between them.

use std::io::{Read as _, Write as _};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::Duration;

use serde_json::{json, Value};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::unix::{OwnedReadHalf, OwnedWriteHalf};
use tokio::net::UnixStream;
use tokio::time::timeout;

use teton_protocol::events::Event;
use teton_protocol::jsonrpc::error_code;
use teton_protocol::methods::{
    ConfigUpdate, ContextAction, PrivacyBoundaryConfig, ProviderConfig, RepoContextSource,
    RepoContextStateKind, SessionContextParams, TierBindingConfig,
};
use teton_protocol::{
    Phase as ProtoPhase, PrivacyMode, ProviderId, ProviderKind as ProtoProviderKind, SessionId,
    SessionMode, Tier as ProtoTier, PROTOCOL_VERSION_MAX, PROTOCOL_VERSION_MIN,
};

use tetond::broadcast::EventBus;
use tetond::grants::{ConnectionId, GrantRegistry};
use tetond::repo_context::{FileStat, RealFiles, RepoContextState, RepoFileError, RepoFileReader};
use tetond::runtime::{ClientPresence, DaemonRuntime};
use tetond::server::DaemonProcess;
use tetond::sessions::SessionRegistry;
use tetond::{server, Daemon};

// ---------------------------------------------------------------------------
// trees, notes and the fixture home
// ---------------------------------------------------------------------------

/// A throwaway directory that probes as a `project` root.
struct Tree {
    root: PathBuf,
}

impl Tree {
    /// A fresh tree under `/tmp` with a short name: a daemon socket is bound
    /// beneath one of these and `sun_len` caps the path at ~104 bytes.
    fn new(tag: &str) -> Self {
        static SEQ: AtomicUsize = AtomicUsize::new(0);
        let seq = SEQ.fetch_add(1, Ordering::SeqCst);
        let root =
            PathBuf::from("/tmp").join(format!("rcx{tag}{:x}{seq:x}", std::process::id() & 0xffff));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).unwrap();
        // A project marker, so the root probes as `project`. BR-1 reads the file
        // only at a project root, so without this every test here would be
        // asserting the `absent` path by accident.
        std::fs::write(root.join("Cargo.toml"), "[package]\n").unwrap();
        Self { root }
    }

    /// A tree whose `TETON.md` says `notes`.
    fn with_notes(tag: &str, notes: &str) -> Self {
        let tree = Self::new(tag);
        tree.write_notes(notes);
        tree
    }

    fn write_notes(&self, notes: &str) {
        std::fs::write(self.root.join("TETON.md"), notes).unwrap();
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

/// The `HOME` every probe in this binary runs under.
///
/// Set once, before any daemon is constructed, and never changed — each test
/// calls this first, so the write happens while every other test is still
/// blocked inside the `OnceLock` initializer rather than beside a live read
/// (`skill_turn.rs`'s fixture, for its reason).
///
/// It is a real directory because AC-2's third leg is a `/cd` to a **`home`**
/// root, and `probe` decides that by comparing the path against `$HOME`.
fn fixture_home() -> &'static Path {
    static HOME: OnceLock<Tree> = OnceLock::new();
    HOME.get_or_init(|| {
        let home = Tree::new("home");
        // A `TETON.md` in the home directory too, which is the point of the leg:
        // a `home` root drops the block *because of the root kind*, not because
        // there happened to be no file there.
        home.write_notes("Notes that a home root must never carry.\n");
        std::env::set_var("HOME", home.path());
        home
    })
    .path()
}

// ---------------------------------------------------------------------------
// the recording filesystem
// ---------------------------------------------------------------------------

/// Every call the repository-notes loader made, in order.
///
/// Delegates to [`RealFiles`], so the tests still read real bytes off a real
/// tree — the counter is about *reach*, not about substituting the filesystem.
/// A double that answered from a map would make "the notes the model saw are the
/// notes on disk" a claim about the fixture.
struct CountingFiles {
    inner: RealFiles,
    calls: Mutex<Vec<String>>,
}

impl CountingFiles {
    fn new() -> Arc<Self> {
        Arc::new(Self {
            inner: RealFiles,
            calls: Mutex::new(Vec::new()),
        })
    }

    /// How many times the filesystem was reached, of either kind.
    fn calls(&self) -> usize {
        self.calls.lock().unwrap().len()
    }

    /// How many `read`s — the calls that put a repository's bytes in memory.
    fn reads(&self) -> usize {
        self.calls
            .lock()
            .unwrap()
            .iter()
            .filter(|call| call.starts_with("read "))
            .count()
    }

    fn drain(&self) -> Vec<String> {
        std::mem::take(&mut *self.calls.lock().unwrap())
    }

    fn record(&self, what: &str, path: &Path) {
        self.calls
            .lock()
            .unwrap()
            .push(format!("{what} {}", path.display()));
    }
}

impl RepoFileReader for CountingFiles {
    fn stat(&self, path: &Path) -> Result<FileStat, RepoFileError> {
        self.record("stat", path);
        self.inner.stat(path)
    }

    fn read(&self, path: &Path, ceiling: u64) -> Result<String, RepoFileError> {
        self.record("read", path);
        self.inner.read(path, ceiling)
    }
}

// ---------------------------------------------------------------------------
// a mock vendor, so a turn can be inspected on the wire
// ---------------------------------------------------------------------------

/// One scripted answer, or the plain `done` completion when the script is empty.
#[derive(Debug, Clone)]
struct Reply(String);

/// Something to do to the world while a request is in flight, once.
///
/// A named type because the shape is three wrappers deep — shared with the
/// vendor's thread, taken rather than borrowed, and callable exactly once.
type DuringRequest = Arc<Mutex<Option<Box<dyn FnOnce() + Send>>>>;

/// One OpenAI-compatible streaming turn: `content` deltas, then an optional tool
/// call, then usage + `[DONE]`.
///
/// A copy of the shape `remote_loop.rs` and `skill_turn.rs` each carry, for
/// their stated reason: integration test binaries share nothing, and a shared
/// module would have to live in the lib.
fn sse_turn(content: &str, tool: Option<(&str, &str, &str)>) -> String {
    let mut s = String::new();
    let chunk = json!({ "choices": [{ "delta": { "content": content } }] });
    s.push_str(&format!("data: {chunk}\n\n"));
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
    let usage = json!({ "usage": { "prompt_tokens": 5, "completion_tokens": 2 } });
    s.push_str(&format!("data: {usage}\n\n"));
    s.push_str("data: [DONE]\n\n");
    s
}

/// A single-threaded mock OpenAI-compatible vendor on a real socket.
///
/// Real rather than a `Transport` double because the claim is about the bytes a
/// **turn** put on the wire, and the system prompt is assembled several layers
/// above any seam a double could stand in for.
struct Vendor {
    endpoint: String,
    bodies: Arc<Mutex<Vec<String>>>,
    script: Arc<Mutex<std::collections::VecDeque<Reply>>>,
    /// Something to do to the world **while a request is in flight**, once.
    ///
    /// AC-8's mid-turn leg needs an edit that lands between two iterations of
    /// one turn, and the only moment that is reliably inside a turn is while the
    /// vendor is holding a request. Sleeping instead would pin a position in a
    /// detached schedule, which is LESSON-591's shape.
    during: DuringRequest,
}

impl Vendor {
    fn start() -> Self {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind a mock vendor");
        let addr = listener.local_addr().expect("mock vendor address");
        let bodies = Arc::new(Mutex::new(Vec::new()));
        let script: Arc<Mutex<std::collections::VecDeque<Reply>>> = Arc::default();
        let during: DuringRequest = Arc::new(Mutex::new(None));
        let captured = Arc::clone(&bodies);
        let scripted = Arc::clone(&script);
        let hook = Arc::clone(&during);
        std::thread::spawn(move || {
            for stream in listener.incoming() {
                let Ok(mut stream) = stream else { break };
                // Read the request by its **framing**, never by a heuristic: a
                // short read is legal at any point in a stream, and a loop that
                // stopped at the first one loses the tail of a large body on one
                // platform and not the other (LESSON-540).
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
                if let Some(action) = hook.lock().unwrap().take() {
                    action();
                }
                let body = scripted
                    .lock()
                    .unwrap()
                    .pop_front()
                    .unwrap_or_else(|| Reply(sse_turn("done", None)))
                    .0;
                let response = format!(
                    "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\n\
                     Content-Length: {}\r\nConnection: close\r\n\r\n{body}",
                    body.len()
                );
                let _ = stream.write_all(response.as_bytes());
                let _ = stream.flush();
            }
        });
        Self {
            endpoint: format!("http://{addr}/v1/chat/completions"),
            bodies,
            script,
            during,
        }
    }

    /// Answer the next request with a `read` of `path`, so the turn runs a
    /// second iteration against the same system prompt.
    fn will_call_read(&self, path: &str) {
        self.script.lock().unwrap().push_back(Reply(sse_turn(
            "Looking.",
            Some(("call-1", "read", &json!({ "path": path }).to_string())),
        )));
    }

    /// Run `action` on the vendor's thread while the next request is in flight.
    fn during_next_request(&self, action: impl FnOnce() + Send + 'static) {
        *self.during.lock().unwrap() = Some(Box::new(action));
    }

    fn sent(&self) -> Vec<String> {
        self.bodies.lock().unwrap().clone()
    }
}

/// Every request the vendor was handed, parsed out of the raw HTTP it captured.
fn wire_requests(vendor: &Vendor) -> Vec<Value> {
    vendor
        .sent()
        .iter()
        .filter_map(|raw| {
            let (_, body) = raw.split_once("\r\n\r\n")?;
            serde_json::from_str(body).ok()
        })
        .collect()
}

/// The system prompt of every captured request, in order.
///
/// Parsed as a *value* rather than matched as an escaped substring: the claims
/// below are about the bytes the daemon assembled, and `\n` on the wire is two
/// characters.
fn wire_systems(vendor: &Vendor) -> Vec<String> {
    wire_requests(vendor)
        .iter()
        .filter_map(|request| {
            request["messages"]
                .as_array()?
                .iter()
                .find(|message| message["role"] == json!("system"))
                .map(|message| message["content"].as_str().unwrap_or_default().to_owned())
        })
        .collect()
}

// ---------------------------------------------------------------------------
// a runtime with a route
// ---------------------------------------------------------------------------

/// A daemon runtime with one remote provider, its bus, its sessions, and the
/// recording filesystem its notes are read through.
struct Harness {
    runtime: Arc<DaemonRuntime>,
    events: Arc<EventBus>,
    sessions: SessionRegistry,
    vendor: Vendor,
    files: Arc<CountingFiles>,
    connection: ConnectionId,
}

impl Harness {
    fn new() -> Self {
        fixture_home();
        let vendor = Vendor::start();
        let files = CountingFiles::new();
        // The shipped boundary set is off: none of its thirteen globs names
        // `TETON.md`, but a `read` tool result under a non-empty set is judged
        // by `context_is_sensitive` on every turn, and a runtime with no local
        // tier answers a pin with a refusal. The one test whose subject *is* a
        // boundary declares its own row (`with_boundary`).
        let runtime = Arc::new(
            DaemonRuntime::minimal()
                .with_default_boundaries_disabled()
                .with_repo_files(Arc::clone(&files) as Arc<dyn RepoFileReader + Send + Sync>),
        );
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
        // `reflex` is deliberately left unbound: `route`, `redact` and `title`
        // all hang off it, and this machine has no local tier — so those duties
        // resolve to nothing and cannot race the turn for a scripted answer
        // (`skill_turn.rs`'s fixture, for its reason).
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
            files,
            connection: GrantRegistry::new().next_connection_id(),
        }
    }

    /// A session rooted at `cwd`, with its repository notes derived exactly as
    /// `session/create` derives them — the same function
    /// [`server::handle_session_create`] calls, so a fixture cannot drift into
    /// an agreeing re-implementation of the create path.
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
        self.runtime
            .store_session_repo_context(&self.sessions, &id, &probed, &self.events);
        id
    }

    /// One `/context` call, through the daemon's own method.
    fn context(
        &self,
        id: &SessionId,
        action: ContextAction,
    ) -> teton_protocol::methods::SessionContextResult {
        self.runtime.session_context(
            &SessionContextParams {
                session_id: id.clone(),
                action,
            },
            &self.sessions,
            &self.events,
        )
    }

    /// Run one turn and require it to complete.
    async fn turn(&self, id: &SessionId, prompt: &str) {
        let cwd = self
            .sessions
            .get(id)
            .and_then(|s| s.cwd)
            .expect("the fixture always roots its sessions");
        let outcome = self
            .runtime
            .run_prompt_turn(
                &self.events,
                &self.sessions,
                id.clone(),
                SessionMode::Structured,
                Some(ProtoPhase::Implement),
                Some(cwd),
                prompt.to_owned(),
                None,
                Some(self.connection),
                ClientPresence::unwatched(),
            )
            .await;
        assert!(outcome.is_ok(), "the turn failed: {outcome:?}");
    }

    /// This session's stored notes, as the next turn would read them.
    fn stored(&self, id: &SessionId) -> Arc<RepoContextState> {
        self.sessions.repo_context(id)
    }
}

/// Every `repo_context_state` event on `sub`, drained.
async fn repo_context_events(
    sub: &mut tetond::broadcast::Subscription,
) -> Vec<teton_protocol::events::RepoContextState> {
    let mut out = Vec::new();
    while let Ok(Some(envelope)) = timeout(Duration::from_millis(50), sub.recv()).await {
        if let Event::RepoContextState(state) = envelope.event {
            out.push(state);
        }
    }
    out
}

/// The opening line of the block, for the file the daemon read.
const BLOCK_OPEN: &str = "<repo-notes file=\"TETON.md\">";

// ---------------------------------------------------------------------------
// BR-1 / AC-2 — the two lifecycle sites, and the order the second client sees
// ---------------------------------------------------------------------------

/// **BR-1 / AC-2.** A session created at a project root with a `TETON.md` is
/// carrying it before the create result is answered, a `/cd` into a second
/// repository is carrying *that* one before anybody is told the root moved, and
/// a `/cd` to a `home` root drops the block whatever is lying in the home
/// directory.
///
/// Driven over a **real socket** against the daemon's own `session/create` and
/// `session/set_cwd` handlers, because the claim is about handler wiring: a test
/// that called the loader would stay green with both call sites deleted.
///
/// # The ordering claim, and the instrument that can see it
///
/// REQ-585's rule for the skill registry is that the rebuild lands *before*
/// `session_root_changed` reaches a second attached client. The same rule
/// applies to the notes, and it is asserted twice over:
///
/// 1. **on the bus** — the `/cd`'s three events arrive in the order
///    `repo_context_state`, `context_cleared`, `session_root_changed`, so a
///    client reading its stream in order learns the notes moved before it learns
///    the root did. A build that published the notes after the move fails here
///    outright;
/// 2. **in a second reader** — an observer task woken by `session_root_changed`
///    reads the registry the instant it sees that event, exactly as a second
///    attached client would call `session/context`, and finds the *new*
///    repository's notes.
///
/// The second connection also asks `session/context` without ever attaching, and
/// is refused `NOT_ATTACHED` — the gate `session/transcript` takes, on a method
/// whose answer names a file inside the user's working tree.
///
/// ## Mutation
///
/// | change | result |
/// |---|---|
/// | delete the create call site | the create leg sees no event and an `absent` state |
/// | move the `/cd` load below the publishes | the bus order assertion fails |
/// | delete the `/cd` load | the session keeps Alpha's notes at Beta's root |
/// | read a non-`project` root | the `home` leg keeps a block |
/// | drop the `may_drive` gate | the unattached connection is answered instead of refused |
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn create_loads_the_file_and_cd_rebuilds_it_before_session_root_changed() {
    let home = fixture_home().to_path_buf();
    const ALPHA_NOTES: &str = "Alpha's own description of itself.\n";
    const BETA_NOTES: &str = "Beta's own description of itself.\n";
    let alpha = Tree::with_notes("alpha", ALPHA_NOTES);
    let beta = Tree::with_notes("beta", BETA_NOTES);

    let files = CountingFiles::new();
    let events = Arc::new(EventBus::new());
    let runtime = Arc::new(
        DaemonRuntime::minimal()
            .with_repo_files(Arc::clone(&files) as Arc<dyn RepoFileReader + Send + Sync>),
    );
    let daemon = Arc::new(
        // `Embedded`, like every in-process fixture: this test's client *is* the
        // daemon's own process, which the ancestry gate would otherwise refuse.
        Daemon::with_runtime(Arc::clone(&events), runtime)
            .with_daemon_process(DaemonProcess::Embedded),
    );
    let socket = temp_socket("repo-context-lifecycle");
    let listener = server::bind_listener(&socket).unwrap();
    let server_task = tokio::spawn(server::serve(listener, Arc::clone(&daemon)));

    let mut creation = events.subscribe(64);
    let mut client = TestClient::connect(&socket).await;
    client.handshake().await;
    let session = client.create_session_at(alpha.path()).await;
    let session_id = SessionId::from(session.clone());

    // --- create -----------------------------------------------------------
    let announced = repo_context_events(&mut creation).await;
    assert_eq!(
        announced.len(),
        1,
        "`session/create` announces the notes exactly once: {announced:?}"
    );
    assert_eq!(announced[0].state, RepoContextStateKind::Loaded);
    assert_eq!(announced[0].source, Some(RepoContextSource::TetonMd));
    assert_eq!(
        announced[0].resident_bytes,
        ALPHA_NOTES.len() as u64,
        "the resident figure is the file's bytes, not the block's: {announced:?}"
    );
    assert!(!announced[0].truncated);

    let status = client.context(&session, "status").await;
    assert_eq!(status["result"]["state"], json!("loaded"), "{status}");
    assert_eq!(
        status["result"]["file"],
        json!("TETON.md"),
        "the routed answer names the file, which the event does not: {status}"
    );

    // The gate, on a connection that never attached to this session. Asked
    // before the `/cd` so a refusal cannot be mistaken for a race with it.
    let mut stranger = TestClient::connect(&socket).await;
    stranger.handshake().await;
    let refused = stranger.context(&session, "status").await;
    assert_eq!(
        refused["error"]["code"],
        json!(error_code::NOT_ATTACHED),
        "`session/context` names a file in the user's tree and takes `may_drive`: {refused}"
    );

    // --- /cd into a second repository -------------------------------------
    //
    // The observer stands in for a second attached client: it is woken by the
    // bus and reads the registry the moment it sees `session_root_changed`.
    let mut watching = events.subscribe(64);
    let observed_daemon = Arc::clone(&daemon);
    let observed_session = session_id.clone();
    let observer = tokio::spawn(async move {
        let mut order = Vec::new();
        let mut at_root_changed = None;
        while let Some(envelope) = watching.recv().await {
            let name = envelope.event.name();
            order.push(name.to_owned());
            if name == "session_root_changed" {
                at_root_changed = Some(observed_daemon.sessions.repo_context(&observed_session));
                break;
            }
        }
        (order, at_root_changed)
    });

    let moved = client
        .call(
            "session/set_cwd",
            json!({"session_id": session, "cwd": beta.path()}),
        )
        .await;
    assert!(moved.get("result").is_some(), "the /cd failed: {moved}");

    let (order, at_root_changed) = timeout(Duration::from_secs(10), observer)
        .await
        .expect("the observer never saw `session_root_changed`")
        .expect("the observer task panicked");
    assert_eq!(
        order,
        vec![
            "repo_context_state".to_owned(),
            "context_cleared".to_owned(),
            "session_root_changed".to_owned(),
        ],
        "the notes must be rebuilt — and announced — before the move is: {order:?}"
    );
    let seen = at_root_changed.expect("the observer read the registry");
    assert_eq!(
        seen.file().map(|file| file.text.as_str()),
        Some(BETA_NOTES),
        "a second client reacting to `session_root_changed` read the pre-move notes: {seen:?}"
    );

    // --- /cd to a home root ------------------------------------------------
    let mut dropping = events.subscribe(64);
    let moved_home = client
        .call(
            "session/set_cwd",
            json!({"session_id": session, "cwd": home}),
        )
        .await;
    assert!(
        moved_home.get("result").is_some(),
        "the /cd home failed: {moved_home}"
    );
    let dropped = repo_context_events(&mut dropping).await;
    assert_eq!(
        dropped.iter().map(|e| e.state).collect::<Vec<_>>(),
        vec![RepoContextStateKind::Absent],
        "a `home` root drops the block and says so: {dropped:?}"
    );
    assert_eq!(
        *daemon.sessions.repo_context(&session_id),
        RepoContextState::Absent,
        "BR-1 reads a project root and nothing else"
    );
    assert!(
        !files
            .drain()
            .iter()
            .any(|call| call.contains(fixture_home().to_string_lossy().as_ref())),
        "the home directory's own `TETON.md` was reached for"
    );

    server_task.abort();
    let _ = std::fs::remove_file(&socket);
}

// ---------------------------------------------------------------------------
// BR-2 / AC-10 — two switches, and off means unopened
// ---------------------------------------------------------------------------

/// **BR-2 / AC-10.** `[context] repo_file = false` and `/context off` both mean
/// the file is *never opened* — not read-then-withheld — and `/context on`
/// re-loads at once rather than waiting for the next turn. The session switch
/// writes nothing durable.
///
/// The instrument for "never opened" is the injected reader's counter: a build
/// that read the file and then declined to render it is indistinguishable by its
/// answer and fails immediately here.
///
/// "Writes nothing to `config.toml`" is asserted **behaviourally** rather than
/// by inspecting a file: a fresh session created on the same runtime after
/// `/context off` still loads its notes, which is only true if the durable
/// default is untouched. A runtime with no `config_path` cannot write at all, so
/// asserting the absence of that write would be asserting the absence of a
/// mechanism (LESSON-519).
///
/// ## Mutation
///
/// | change | result |
/// |---|---|
/// | consult the switch after the `stat` | the counter is non-zero on both off legs |
/// | treat `repo_file` as a render switch | the counter is non-zero, and the state is `loaded` |
/// | defer `/context on`'s re-load to the next turn | the `on` answer is `withheld_off` |
/// | persist the session switch | the second session is `withheld_off` |
/// | leave the block stamped after `off` | the wire carries `<repo-notes` on the last turn |
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_session_switch_and_the_durable_switch_withhold_without_opening_the_file() {
    let repo = Tree::with_notes(
        "switch",
        "The notes a switched-off session must not read.\n",
    );
    let h = Harness::new();

    // --- the durable switch, off ------------------------------------------
    h.runtime
        .apply_config_update(ConfigUpdate::SetRepoContextEnabled { enabled: false })
        .expect("the durable switch is a config update");
    let dark = h.session_at(repo.path());
    assert_eq!(
        *h.stored(&dark),
        RepoContextState::WithheldOff,
        "`repo_file = false` withholds"
    );
    assert_eq!(
        h.files.calls(),
        0,
        "off means unopened — the loader reached the filesystem: {:?}",
        h.files.drain()
    );
    h.turn(&dark, "anything at all").await;
    assert_eq!(
        h.files.calls(),
        0,
        "the turn-start refresh opened a file the switch had turned off: {:?}",
        h.files.drain()
    );
    assert!(
        wire_systems(&h.vendor)
            .iter()
            .all(|system| !system.contains("<repo-notes")),
        "a withheld session put the notes in its prompt anyway"
    );

    // --- the durable switch, on -------------------------------------------
    h.runtime
        .apply_config_update(ConfigUpdate::SetRepoContextEnabled { enabled: true })
        .expect("the durable switch is a config update");
    let lit = h.session_at(repo.path());
    assert!(
        matches!(*h.stored(&lit), RepoContextState::Loaded(_)),
        "a session created with the switch on carries the notes: {:?}",
        h.stored(&lit)
    );
    assert!(
        h.files.reads() >= 1,
        "the file was never read: {:?}",
        h.files.drain()
    );
    h.turn(&lit, "what is this repository?").await;
    let with_notes = wire_systems(&h.vendor)
        .last()
        .cloned()
        .expect("the turn reached the vendor");
    assert!(
        with_notes.contains(BLOCK_OPEN)
            && with_notes.contains("The notes a switched-off session must not read."),
        "the notes are not in the prompt the model was handed: {with_notes}"
    );

    // --- the session switch, off ------------------------------------------
    h.files.drain();
    let off = h.context(&lit, ContextAction::Off);
    assert_eq!(off.state, RepoContextStateKind::WithheldOff);
    assert_eq!(off.resident_bytes, 0);
    assert_eq!(
        off.source, None,
        "`off` never opened a file, so the daemon must not name one"
    );
    assert_eq!(
        h.files.calls(),
        0,
        "`/context off` opened the file it was switching away from: {:?}",
        h.files.drain()
    );
    h.turn(&lit, "and now?").await;
    let after_off = wire_systems(&h.vendor)
        .last()
        .cloned()
        .expect("the turn reached the vendor");
    assert!(
        !after_off.contains("<repo-notes"),
        "the block survived `/context off`: {after_off}"
    );
    assert_eq!(
        h.files.calls(),
        0,
        "the turn under a switched-off session still reached the filesystem: {:?}",
        h.files.drain()
    );

    // --- the session switch, on: at once ----------------------------------
    let on = h.context(&lit, ContextAction::On);
    assert_eq!(
        on.state,
        RepoContextStateKind::Loaded,
        "`/context on` reports what is resident now, not what the next turn will make resident"
    );
    assert_eq!(on.source, Some(RepoContextSource::TetonMd));
    assert_eq!(on.file.as_deref(), Some("TETON.md"));
    assert!(on.resident_bytes > 0 && !on.truncated);
    assert!(
        h.files.reads() >= 1,
        "`on` answered `loaded` without reading anything: {:?}",
        h.files.drain()
    );

    // --- and nothing durable was written ----------------------------------
    let fresh = h.session_at(repo.path());
    assert!(
        matches!(*h.stored(&fresh), RepoContextState::Loaded(_)),
        "a `/context off` reached the machine's durable default: {:?}",
        h.stored(&fresh)
    );
}

// ---------------------------------------------------------------------------
// BR-6 / AC-8 — fresh at the start of a prompt, fixed inside one
// ---------------------------------------------------------------------------

/// **BR-6 / AC-8.** An edit landing between two prompts is resident on the next
/// one, with one event; an edit landing *inside* a turn is not resident until
/// the turn after it, because the system prompt is fixed for the turn.
///
/// The mid-turn leg drives a two-iteration tool loop and rewrites the file while
/// the **first** model call is in flight — the one moment that is reliably
/// inside a turn — so iteration two's prompt is the discriminator: a refresh
/// that ran per model call rather than per turn would carry the new text there.
///
/// A quiet turn is also measured: with nothing edited, the check costs one
/// `stat` and no `read`, which is ADR-3's budget for the answer every turn of
/// every session gets.
///
/// ## Mutation
///
/// | change | result |
/// |---|---|
/// | delete the refresh | the second prompt still carries the first text |
/// | refresh per model call | iteration two carries the mid-turn text |
/// | compare content instead of the `stat` key | the quiet turn reads the file |
/// | publish on every turn rather than on change | the quiet turn announces |
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn an_edit_between_prompts_is_resident_on_the_next_and_a_mid_turn_edit_is_not() {
    let repo = Tree::with_notes("edit", "First notes: the project is a widget.\n");
    // Something for the scripted tool call to read, inside the jail.
    std::fs::write(repo.path().join("src.txt"), "a file to read\n").unwrap();
    let h = Harness::new();
    let session = h.session_at(repo.path());
    let mut bus = h.events.subscribe(64);

    // --- turn one: the file as it stood at create -------------------------
    h.turn(&session, "first").await;
    assert!(
        wire_systems(&h.vendor)[0].contains("First notes: the project is a widget."),
        "the created session's notes never reached the model"
    );
    let _ = repo_context_events(&mut bus).await;

    // --- a quiet turn: one `stat`, no read, no event ----------------------
    h.files.drain();
    h.turn(&session, "second").await;
    let quiet = h.files.drain();
    assert_eq!(
        quiet.len(),
        1,
        "the quiet turn's staleness check is one `stat`: {quiet:?}"
    );
    assert!(quiet[0].starts_with("stat "), "{quiet:?}");
    assert!(
        repo_context_events(&mut bus).await.is_empty(),
        "a turn that changed nothing announced a change"
    );

    // --- an edit between prompts ------------------------------------------
    repo.write_notes("Second notes: the project is a widget factory, actually.\n");
    h.turn(&session, "third").await;
    let after_edit = wire_systems(&h.vendor)
        .last()
        .cloned()
        .expect("the turn reached the vendor");
    assert!(
        after_edit.contains("Second notes: the project is a widget factory, actually."),
        "the edit is not resident on the next prompt: {after_edit}"
    );
    assert!(
        !after_edit.contains("First notes:"),
        "the stale text survived the re-read: {after_edit}"
    );
    let announced = repo_context_events(&mut bus).await;
    assert_eq!(
        announced.len(),
        1,
        "an edit is one event, not none and not two: {announced:?}"
    );
    assert_eq!(announced[0].state, RepoContextStateKind::Loaded);

    // --- an edit inside a turn --------------------------------------------
    let mid_turn = repo.path().to_path_buf();
    h.vendor.will_call_read("src.txt");
    h.vendor.during_next_request(move || {
        std::fs::write(
            mid_turn.join("TETON.md"),
            "Third notes: written while the turn was running.\n",
        )
        .unwrap();
    });
    let before = wire_systems(&h.vendor).len();
    h.turn(&session, "fourth").await;
    let systems = wire_systems(&h.vendor);
    assert!(
        systems.len() >= before + 2,
        "the turn did not run two iterations: {} prompts",
        systems.len() - before
    );
    for (index, system) in systems[before..].iter().enumerate() {
        assert!(
            system.contains("Second notes:"),
            "iteration {index} of one turn changed its system prompt: {system}"
        );
        assert!(
            !system.contains("Third notes:"),
            "a mid-turn edit reached iteration {index} of the same turn: {system}"
        );
    }

    // --- and the turn after it carries the mid-turn edit -------------------
    h.turn(&session, "fifth").await;
    assert!(
        wire_systems(&h.vendor)
            .last()
            .expect("the turn reached the vendor")
            .contains("Third notes: written while the turn was running."),
        "the mid-turn edit never became resident"
    );
}

// ---------------------------------------------------------------------------
// OQ-4 — a boundary that comes to cover the file
// ---------------------------------------------------------------------------

/// **OQ-4, as the architecture resolved it.** A privacy boundary configured
/// *mid-session* that covers the notes drops the block at the next turn's
/// refresh, with the event that says why — rather than leaving a session pinning
/// itself local on every turn in silence.
///
/// ## Mutation
///
/// | change | result |
/// |---|---|
/// | drop the refresh's boundary re-check | the block survives the new row |
/// | fold `withheld_boundary` into `absent` | the state assertion fails |
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_boundary_configured_mid_session_withholds_the_notes_at_the_next_refresh() {
    let repo = Tree::with_notes("boundary", "Notes a boundary will come to cover.\n");
    let h = Harness::new();
    let session = h.session_at(repo.path());
    let mut bus = h.events.subscribe(64);

    h.turn(&session, "before").await;
    assert!(
        wire_systems(&h.vendor)[0].contains("Notes a boundary will come to cover."),
        "the notes were not resident before the boundary was configured"
    );
    let _ = repo_context_events(&mut bus).await;

    h.runtime
        .apply_config_update(ConfigUpdate::SetPrivacyBoundary(PrivacyBoundaryConfig {
            path_glob: "**/TETON.md".to_owned(),
            mode: PrivacyMode::LocalOnly,
            origin: teton_protocol::methods::BoundaryOriginConfig::User,
        }))
        .expect("a boundary is a config update");

    h.turn(&session, "after").await;
    assert_eq!(
        h.stored(&session).kind(),
        RepoContextStateKind::WithheldBoundary,
        "a boundary that came to cover the file left it resident: {:?}",
        h.stored(&session)
    );
    let announced = repo_context_events(&mut bus).await;
    assert_eq!(
        announced.iter().map(|e| e.state).collect::<Vec<_>>(),
        vec![RepoContextStateKind::WithheldBoundary],
        "the drop was silent: {announced:?}"
    );
    let after = wire_systems(&h.vendor)
        .last()
        .cloned()
        .expect("the turn reached the vendor");
    assert!(
        !after.contains("<repo-notes"),
        "the covered file stayed in the prompt: {after}"
    );
}

// ---------------------------------------------------------------------------
// a minimal JSON-RPC client
// ---------------------------------------------------------------------------

/// The counter, not the timestamp, guarantees uniqueness: `SystemTime::now()`
/// can return the same value for two calls within one clock tick.
fn temp_socket(tag: &str) -> PathBuf {
    static NEXT: AtomicUsize = AtomicUsize::new(0);
    std::env::temp_dir().join(format!(
        "teton-{tag}-{}-{}.sock",
        std::process::id(),
        NEXT.fetch_add(1, Ordering::Relaxed),
    ))
}

/// A newline-delimited JSON-RPC client over the daemon socket.
struct TestClient {
    reader: BufReader<OwnedReadHalf>,
    writer: OwnedWriteHalf,
    next_id: i64,
}

impl TestClient {
    async fn connect(path: &Path) -> Self {
        let stream = UnixStream::connect(path).await.unwrap();
        let (read_half, write_half) = stream.into_split();
        Self {
            reader: BufReader::new(read_half),
            writer: write_half,
            next_id: 1,
        }
    }

    async fn send(&mut self, method: &str, params: Value) -> i64 {
        let id = self.next_id;
        self.next_id += 1;
        let mut text = serde_json::to_string(&json!({
            "jsonrpc": "2.0",
            "id": id,
            "method": method,
            "params": params,
        }))
        .unwrap();
        text.push('\n');
        self.writer.write_all(text.as_bytes()).await.unwrap();
        self.writer.flush().await.unwrap();
        id
    }

    async fn frame(&mut self) -> Value {
        let mut line = String::new();
        let read = timeout(Duration::from_secs(10), self.reader.read_line(&mut line))
            .await
            .expect("timed out waiting for a frame")
            .unwrap();
        assert!(read > 0, "connection closed unexpectedly");
        serde_json::from_str(&line).unwrap()
    }

    async fn call(&mut self, method: &str, params: Value) -> Value {
        let id = self.send(method, params).await;
        loop {
            let frame = self.frame().await;
            if frame.get("id").and_then(Value::as_i64) == Some(id) {
                return frame;
            }
        }
    }

    async fn handshake(&mut self) {
        let answer = self
            .call(
                "handshake",
                json!({
                    "client_kind": "cli",
                    "client_name": "repo-context-test-client",
                    "client_version": "0.1.0",
                    "protocol_min": PROTOCOL_VERSION_MIN,
                    "protocol_max": PROTOCOL_VERSION_MAX,
                    "monitor": false,
                }),
            )
            .await;
        assert!(answer.get("result").is_some(), "handshake failed: {answer}");
    }

    async fn create_session_at(&mut self, cwd: &Path) -> String {
        let created = self
            .call("session/create", json!({"mode": "freeform", "cwd": cwd}))
            .await;
        created["result"]["session_id"]
            .as_str()
            .unwrap_or_else(|| panic!("session/create failed: {created}"))
            .to_owned()
    }

    async fn context(&mut self, session: &str, action: &str) -> Value {
        self.call(
            "session/context",
            json!({"session_id": session, "action": action}),
        )
        .await
    }
}
