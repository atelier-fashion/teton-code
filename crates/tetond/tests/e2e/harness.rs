//! End-to-end test harness: spawn the real `tetond` binary and drive it as a
//! protocol client against temp fixtures.
//!
//! Everything here is deliberately black-box. A test:
//!
//! 1. lays down a temp repo (the fixture demo repo) and a config file,
//! 2. spawns the **real** `tetond` binary ([`Daemon::spawn`]) with an isolated
//!    socket (`XDG_RUNTIME_DIR`) and the injectable seams the daemon reads
//!    (scripted local engine, provider endpoints, MCP config, hardware probe),
//! 3. connects a JSON-RPC [`Client`] over the Unix socket and drives sessions,
//! 4. asserts on the streamed events and, for every remote call, on the bytes a
//!    [`MockProvider`] captured — which doubles as the egress-capture proxy.
//!
//! ## The suite-wide egress capture (BR-1)
//!
//! Every [`MockProvider`] appends each request body it receives to a
//! process-global buffer ([`global_capture`]). [`assert_no_boundary_bytes`]
//! scans that buffer for the boundary secret sentinel — so a leak in *any* test
//! fails the run, not just AC-5's. A blocked call never reaches a provider, so
//! its bytes never enter the capture; that absence is the guarantee.

#![allow(dead_code)]

use std::collections::VecDeque;
use std::io::{BufRead, BufReader, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError};
use std::sync::{Arc, Mutex, OnceLock};
use std::thread;
use std::time::{Duration, Instant};

use serde_json::{json, Value};

/// The secret that lives in the fixture repo's `secrets/prod.env`. It must never
/// appear in any captured egress payload across the whole suite run (BR-1).
pub const SECRET_SENTINEL: &str = "sk-live-DO-NOT-LEAK-teton-e2e-Zx9qWv";

// ---------------------------------------------------------------------------
// Suite-wide egress capture (BR-1)
// ---------------------------------------------------------------------------

/// The process-global buffer every [`MockProvider`] appends request bodies to.
pub fn global_capture() -> &'static Mutex<Vec<Vec<u8>>> {
    static CAPTURE: OnceLock<Mutex<Vec<Vec<u8>>>> = OnceLock::new();
    CAPTURE.get_or_init(|| Mutex::new(Vec::new()))
}

fn record_egress(body: Vec<u8>) {
    global_capture().lock().unwrap().push(body);
}

/// Assert the boundary secret has not appeared in any captured egress payload.
///
/// Called at the end of every egress-touching test, so a leak anywhere in the
/// suite fails the run (BR-1 across the whole suite, not only AC-5).
pub fn assert_no_boundary_bytes() {
    let capture = global_capture().lock().unwrap();
    // BR-1 is about the *content* of a boundary-protected file, not its path (a
    // user prompt may legitimately name the path). The sentinel and the database
    // URL are the file's content; neither may ever reach the wire.
    for (i, body) in capture.iter().enumerate() {
        assert!(
            !contains(body, SECRET_SENTINEL.as_bytes()),
            "BR-1 VIOLATION: boundary secret leaked into captured egress payload #{i}"
        );
        assert!(
            !contains(body, b"postgres://prod-db.internal"),
            "BR-1 VIOLATION: boundary file content leaked into captured egress payload #{i}"
        );
    }
}

fn contains(haystack: &[u8], needle: &[u8]) -> bool {
    needle.len() <= haystack.len() && haystack.windows(needle.len()).any(|w| w == needle)
}

// ---------------------------------------------------------------------------
// SSE builders (OpenAI-compatible and Anthropic shapes)
// ---------------------------------------------------------------------------

/// One canned mock-provider response: an HTTP status and a body.
#[derive(Clone)]
pub struct MockResponse {
    pub status: u16,
    pub body: String,
}

impl MockResponse {
    pub fn ok(body: impl Into<String>) -> Self {
        Self {
            status: 200,
            body: body.into(),
        }
    }

    /// A hard client error (used to simulate a flaky provider for AC-7).
    pub fn bad_request() -> Self {
        Self {
            status: 400,
            body: String::new(),
        }
    }
}

/// An OpenAI-compatible streaming turn: text deltas, an optional tool call, then
/// usage and `[DONE]`.
pub fn openai_turn(
    text: &str,
    tool: Option<(&str, &str, &str)>, // (id, name, arguments-json)
    prompt_tokens: u64,
    completion_tokens: u64,
) -> String {
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
    let usage = json!({ "usage": { "prompt_tokens": prompt_tokens, "completion_tokens": completion_tokens } });
    s.push_str(&format!("data: {usage}\n\n"));
    s.push_str("data: [DONE]\n\n");
    s
}

/// An Anthropic Messages streaming turn: `message_start` (input usage), a text or
/// `tool_use` content block, then `message_delta` (output usage) + `message_stop`.
pub fn anthropic_turn(
    text: &str,
    tool: Option<(&str, &str, &str)>, // (id, name, arguments-json)
    input_tokens: u64,
    output_tokens: u64,
) -> String {
    let mut s = String::new();
    let start = json!({ "type": "message_start", "message": { "usage": { "input_tokens": input_tokens, "output_tokens": 0 } } });
    s.push_str(&format!("data: {start}\n\n"));
    if let Some((id, name, args)) = tool {
        let block = json!({ "type": "content_block_start", "content_block": { "type": "tool_use", "id": id, "name": name } });
        s.push_str(&format!("data: {block}\n\n"));
        let delta = json!({ "type": "content_block_delta", "delta": { "type": "input_json_delta", "partial_json": args } });
        s.push_str(&format!("data: {delta}\n\n"));
        s.push_str(&format!(
            "data: {}\n\n",
            json!({ "type": "content_block_stop" })
        ));
        let mdelta = json!({ "type": "message_delta", "usage": { "output_tokens": output_tokens }, "delta": { "stop_reason": "tool_use" } });
        s.push_str(&format!("data: {mdelta}\n\n"));
    } else {
        s.push_str(&format!(
            "data: {}\n\n",
            json!({ "type": "content_block_start", "content_block": { "type": "text" } })
        ));
        let delta = json!({ "type": "content_block_delta", "delta": { "type": "text_delta", "text": text } });
        s.push_str(&format!("data: {delta}\n\n"));
        s.push_str(&format!(
            "data: {}\n\n",
            json!({ "type": "content_block_stop" })
        ));
        let mdelta = json!({ "type": "message_delta", "usage": { "output_tokens": output_tokens }, "delta": { "stop_reason": "end_turn" } });
        s.push_str(&format!("data: {mdelta}\n\n"));
    }
    s.push_str(&format!("data: {}\n\n", json!({ "type": "message_stop" })));
    s
}

// ---------------------------------------------------------------------------
// Mock provider HTTP server (also the egress-capture proxy)
// ---------------------------------------------------------------------------

/// A localhost HTTP server that answers a provider adapter with canned SSE and
/// captures every request body (per-server and into the suite-wide capture).
pub struct MockProvider {
    port: u16,
    scripted: Arc<Mutex<VecDeque<MockResponse>>>,
    default: MockResponse,
    requests: Arc<Mutex<Vec<Vec<u8>>>>,
    running: Arc<AtomicBool>,
    handle: Option<thread::JoinHandle<()>>,
}

impl MockProvider {
    /// Start a mock provider that serves `scripted` responses in order, then the
    /// `default` for any further request.
    pub fn start(scripted: Vec<MockResponse>, default: MockResponse) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind mock provider");
        let port = listener.local_addr().unwrap().port();
        listener.set_nonblocking(true).unwrap();

        let scripted = Arc::new(Mutex::new(scripted.into_iter().collect::<VecDeque<_>>()));
        let requests = Arc::new(Mutex::new(Vec::new()));
        let running = Arc::new(AtomicBool::new(true));

        let handle = {
            let scripted = Arc::clone(&scripted);
            let requests = Arc::clone(&requests);
            let running = Arc::clone(&running);
            let default = default.clone();
            thread::spawn(move || {
                while running.load(Ordering::SeqCst) {
                    match listener.accept() {
                        Ok((stream, _)) => {
                            let response = scripted
                                .lock()
                                .unwrap()
                                .pop_front()
                                .unwrap_or_else(|| default.clone());
                            handle_http(stream, &requests, &response);
                        }
                        Err(ref e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                            thread::sleep(Duration::from_millis(5));
                        }
                        Err(_) => break,
                    }
                }
            })
        };

        Self {
            port,
            scripted,
            default,
            requests,
            running,
            handle: Some(handle),
        }
    }

    /// A provider that always returns the same body (e.g. a trivial end-of-turn).
    pub fn always(body: String) -> Self {
        Self::start(Vec::new(), MockResponse::ok(body))
    }

    /// A provider that always fails with `400` (a flaky primary for AC-7).
    pub fn always_bad() -> Self {
        Self::start(Vec::new(), MockResponse::bad_request())
    }

    /// The chat/completions URL for an OpenAI-compatible provider config.
    pub fn openai_endpoint(&self) -> String {
        format!("http://127.0.0.1:{}/v1/chat/completions", self.port)
    }

    /// The Messages URL for an Anthropic provider config.
    pub fn anthropic_endpoint(&self) -> String {
        format!("http://127.0.0.1:{}/v1/messages", self.port)
    }

    /// Every request body this server received.
    pub fn requests(&self) -> Vec<Vec<u8>> {
        self.requests.lock().unwrap().clone()
    }

    /// How many requests this server received.
    pub fn request_count(&self) -> usize {
        self.requests.lock().unwrap().len()
    }
}

impl Drop for MockProvider {
    fn drop(&mut self) {
        self.running.store(false, Ordering::SeqCst);
        if let Some(handle) = self.handle.take() {
            let _ = handle.join();
        }
    }
}

/// Read one HTTP request, capture its body, and write the canned response.
fn handle_http(
    mut stream: TcpStream,
    requests: &Arc<Mutex<Vec<Vec<u8>>>>,
    response: &MockResponse,
) {
    let body = read_http_body(&mut stream);
    record_egress(body.clone());
    requests.lock().unwrap().push(body);

    let status_line = match response.status {
        200 => "HTTP/1.1 200 OK",
        400 => "HTTP/1.1 400 Bad Request",
        500 => "HTTP/1.1 500 Internal Server Error",
        _ => "HTTP/1.1 200 OK",
    };
    let head = format!(
        "{status_line}\r\nContent-Type: text/event-stream\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
        response.body.len()
    );
    let _ = stream.write_all(head.as_bytes());
    let _ = stream.write_all(response.body.as_bytes());
    let _ = stream.flush();
}

/// Read an HTTP request's body using its `Content-Length` header.
fn read_http_body(stream: &mut TcpStream) -> Vec<u8> {
    let mut reader = BufReader::new(stream.try_clone().unwrap());
    let mut content_length = 0usize;
    loop {
        let mut line = String::new();
        if reader.read_line(&mut line).unwrap_or(0) == 0 {
            break;
        }
        let trimmed = line.trim_end();
        if trimmed.is_empty() {
            break; // end of headers
        }
        if let Some(rest) = trimmed.to_ascii_lowercase().strip_prefix("content-length:") {
            content_length = rest.trim().parse().unwrap_or(0);
        }
    }
    let mut body = vec![0u8; content_length];
    if content_length > 0 {
        let _ = reader.read_exact(&mut body);
    }
    body
}

// ---------------------------------------------------------------------------
// The spawned daemon
// ---------------------------------------------------------------------------

/// A per-test temp workspace: a unique directory the daemon socket, config, and
/// fixture repo live under.
pub struct Workspace {
    pub root: PathBuf,
    pub repo: PathBuf,
    pub runtime_dir: PathBuf,
    pub config_path: PathBuf,
}

impl Workspace {
    /// Create a fresh workspace with the fixture demo repo copied into it.
    ///
    /// The root lives under `/tmp` with a short name: the daemon binds a Unix
    /// domain socket beneath it, and socket paths are capped at ~104 bytes
    /// (`SUN_LEN`), which the deep per-user temp dir would blow past. `tag` is
    /// unused in the path for the same length reason but documents the caller.
    pub fn new(tag: &str) -> Self {
        let _ = tag;
        static SEQ: AtomicUsize = AtomicUsize::new(0);
        let seq = SEQ.fetch_add(1, Ordering::SeqCst);
        let root =
            PathBuf::from("/tmp").join(format!("tt{:x}{:x}", std::process::id() & 0xffff, seq));
        let repo = root.join("repo");
        let runtime_dir = root.join("x");
        std::fs::create_dir_all(&runtime_dir).unwrap();
        write_demo_repo(&repo);
        let config_path = root.join("config.toml");
        Self {
            root,
            repo,
            runtime_dir,
            config_path,
        }
    }

    /// Write the daemon config TOML for this workspace.
    pub fn write_config(&self, toml: &str) {
        std::fs::write(&self.config_path, toml).unwrap();
    }

    /// Write a local-engine script file and return its path.
    pub fn write_script(&self, script: &str) -> PathBuf {
        let path = self.root.join("local_script.txt");
        std::fs::write(&path, script).unwrap();
        path
    }

    /// Write an MCP-config JSON file and return its path.
    pub fn write_mcp_config(&self, json: &str) -> PathBuf {
        let path = self.root.join("mcp.json");
        std::fs::write(&path, json).unwrap();
        path
    }

    /// The contents of a repo file, relative to the repo root.
    pub fn read_repo_file(&self, rel: &str) -> String {
        std::fs::read_to_string(self.repo.join(rel)).unwrap()
    }
}

impl Drop for Workspace {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.root);
    }
}

/// Options for spawning the daemon.
#[derive(Default)]
pub struct DaemonOptions {
    pub local_script: Option<PathBuf>,
    pub mcp_config: Option<PathBuf>,
    pub env: Vec<(String, String)>,
}

impl DaemonOptions {
    pub fn env(mut self, key: &str, value: impl Into<String>) -> Self {
        self.env.push((key.to_owned(), value.into()));
        self
    }

    pub fn script(mut self, path: PathBuf) -> Self {
        self.local_script = Some(path);
        self
    }

    pub fn mcp(mut self, path: PathBuf) -> Self {
        self.mcp_config = Some(path);
        self
    }
}

/// A spawned `tetond` process, killed on drop.
pub struct Daemon {
    child: Child,
    socket: PathBuf,
    log_path: PathBuf,
}

impl Daemon {
    /// Spawn the real daemon binary against `workspace` with `options`.
    pub fn spawn(workspace: &Workspace, options: DaemonOptions) -> Self {
        let socket = workspace.runtime_dir.join("teton").join("tetond.sock");
        let log_path = workspace.root.join("tetond.log");
        let log = std::fs::File::create(&log_path).unwrap();

        let mut cmd = Command::new(env!("CARGO_BIN_EXE_tetond"));
        cmd.env("XDG_RUNTIME_DIR", &workspace.runtime_dir)
            .env("TETON_CONFIG", &workspace.config_path)
            .env("TETON_REPO_ROOT", &workspace.repo)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::from(log));
        if let Some(script) = &options.local_script {
            cmd.env("TETON_LOCAL_SCRIPT", script);
        }
        if let Some(mcp) = &options.mcp_config {
            cmd.env("TETON_MCP_CONFIG", mcp);
        }
        for (k, v) in &options.env {
            cmd.env(k, v);
        }

        let child = cmd.spawn().expect("spawn tetond");
        let mut daemon = Self {
            child,
            socket,
            log_path,
        };
        daemon.wait_for_socket();
        daemon
    }

    fn wait_for_socket(&mut self) {
        let deadline = Instant::now() + Duration::from_secs(10);
        while Instant::now() < deadline {
            if UnixStream::connect(&self.socket).is_ok() {
                return;
            }
            if let Ok(Some(status)) = self.child.try_wait() {
                let log = std::fs::read_to_string(&self.log_path).unwrap_or_default();
                panic!("tetond exited early ({status}) before binding. log:\n{log}");
            }
            thread::sleep(Duration::from_millis(25));
        }
        let log = std::fs::read_to_string(&self.log_path).unwrap_or_default();
        panic!(
            "daemon socket never appeared at {}. log:\n{log}",
            self.socket.display()
        );
    }

    /// Open a fresh client connection to this daemon.
    pub fn connect(&self) -> Client {
        Client::connect(&self.socket)
    }
}

impl Drop for Daemon {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

// ---------------------------------------------------------------------------
// The JSON-RPC client
// ---------------------------------------------------------------------------

enum Incoming {
    Response(Value),
    Event(Value),
}

/// A minimal blocking JSON-RPC client over the daemon socket. A reader thread
/// classifies incoming frames; the main thread writes requests, auto-answers
/// permission prompts, and accumulates every event it observed.
pub struct Client {
    writer: UnixStream,
    rx: Receiver<Incoming>,
    next_id: i64,
    events: Vec<Value>,
    auto_approve: bool,
}

impl Client {
    fn connect(socket: &Path) -> Self {
        let writer = UnixStream::connect(socket).expect("connect daemon socket");
        let reader_stream = writer.try_clone().unwrap();
        let (tx, rx) = mpsc::channel();
        thread::spawn(move || {
            let mut reader = BufReader::new(reader_stream);
            let mut line = String::new();
            loop {
                line.clear();
                match reader.read_line(&mut line) {
                    Ok(0) | Err(_) => break,
                    Ok(_) => {}
                }
                let trimmed = line.trim();
                if trimmed.is_empty() {
                    continue;
                }
                let Ok(value) = serde_json::from_str::<Value>(trimmed) else {
                    continue;
                };
                let incoming = match value.get("method").and_then(Value::as_str) {
                    Some("event") => {
                        Incoming::Event(value.get("params").cloned().unwrap_or(Value::Null))
                    }
                    Some(_) => continue, // lag notice etc.
                    None => Incoming::Response(value),
                };
                if tx.send(incoming).is_err() {
                    break;
                }
            }
        });

        let mut client = Self {
            writer,
            rx,
            next_id: 1,
            events: Vec::new(),
            auto_approve: true,
        };
        client.handshake();
        client
    }

    /// Disable the automatic allow-once answer to permission prompts (so a test
    /// can observe an unanswered prompt).
    pub fn without_auto_approve(mut self) -> Self {
        self.auto_approve = false;
        self
    }

    fn handshake(&mut self) {
        let id = self.send(
            "handshake",
            json!({
                "client_kind": "cli",
                "client_name": "e2e",
                "client_version": "0.1.0",
                "protocol_min": 1,
                "protocol_max": 1,
            }),
        );
        let resp = self.await_response(id);
        assert!(resp.get("result").is_some(), "handshake failed: {resp}");
    }

    fn send(&mut self, method: &str, params: Value) -> i64 {
        let id = self.next_id;
        self.next_id += 1;
        let msg = json!({ "jsonrpc": "2.0", "id": id, "method": method, "params": params });
        let mut line = serde_json::to_string(&msg).unwrap();
        line.push('\n');
        self.writer.write_all(line.as_bytes()).unwrap();
        self.writer.flush().unwrap();
        id
    }

    /// Send a request and return the full response object, pumping (and
    /// auto-answering) events until the matching reply arrives.
    pub fn call(&mut self, method: &str, params: Value) -> Value {
        let id = self.send(method, params);
        self.await_response(id)
    }

    fn await_response(&mut self, id: i64) -> Value {
        let deadline = Instant::now() + Duration::from_secs(20);
        loop {
            let remaining = deadline
                .checked_duration_since(Instant::now())
                .unwrap_or_default();
            match self.rx.recv_timeout(remaining) {
                Ok(Incoming::Response(v)) => {
                    if v.get("id").and_then(Value::as_i64) == Some(id) {
                        return v;
                    }
                }
                Ok(Incoming::Event(ev)) => self.on_event(ev),
                Err(RecvTimeoutError::Timeout) => panic!("timed out awaiting response {id}"),
                Err(RecvTimeoutError::Disconnected) => panic!("daemon disconnected awaiting {id}"),
            }
        }
    }

    fn on_event(&mut self, ev: Value) {
        if self.auto_approve
            && ev.get("event").and_then(Value::as_str) == Some("permission_request")
        {
            if let Some(request_id) = ev.get("request_id").and_then(Value::as_str) {
                let request_id = request_id.to_owned();
                self.send(
                    "permission/respond",
                    json!({
                        "request_id": request_id,
                        "outcome": { "outcome": "selected", "option_id": "allow_once" },
                    }),
                );
            }
        }
        self.events.push(ev);
    }

    /// Passively collect events for `window`, auto-answering permission prompts.
    pub fn drain_events(&mut self, window: Duration) {
        let deadline = Instant::now() + window;
        loop {
            let remaining = deadline
                .checked_duration_since(Instant::now())
                .unwrap_or_default();
            if remaining.is_zero() {
                return;
            }
            match self.rx.recv_timeout(remaining) {
                Ok(Incoming::Event(ev)) => self.on_event(ev),
                Ok(Incoming::Response(_)) => {}
                Err(_) => return,
            }
        }
    }

    /// Every event observed so far.
    pub fn events(&self) -> &[Value] {
        &self.events
    }

    /// Every observed event with the given `event` name.
    pub fn events_named(&self, name: &str) -> Vec<&Value> {
        self.events
            .iter()
            .filter(|e| e.get("event").and_then(Value::as_str) == Some(name))
            .collect()
    }

    /// Whether any observed event has the given name.
    pub fn saw_event(&self, name: &str) -> bool {
        !self.events_named(name).is_empty()
    }

    // -- convenience wrappers over common methods --

    /// Create a session and return its id.
    pub fn create_session(&mut self, mode: &str, phase: Option<&str>) -> String {
        let mut params = json!({ "mode": mode });
        if let Some(phase) = phase {
            params["phase"] = json!(phase);
        }
        let resp = self.call("session/create", params);
        resp["result"]["session_id"]
            .as_str()
            .unwrap_or_else(|| panic!("session/create failed: {resp}"))
            .to_owned()
    }

    /// Submit a text prompt turn and return the full response object.
    pub fn prompt(&mut self, session_id: &str, text: &str) -> Value {
        self.call(
            "session/prompt",
            json!({
                "session_id": session_id,
                "prompt": [{ "type": "text", "text": text }],
            }),
        )
    }

    /// The session ids the daemon reports, newest first.
    pub fn session_ids(&mut self) -> Vec<String> {
        let resp = self.call("session/list", json!({}));
        resp["result"]["sessions"]
            .as_array()
            .unwrap()
            .iter()
            .map(|s| s["session_id"].as_str().unwrap().to_owned())
            .collect()
    }

    /// The current config snapshot.
    pub fn config_get(&mut self) -> Value {
        let resp = self.call("config/get", json!({}));
        resp["result"]["snapshot"].clone()
    }

    /// Query the authoritative cost report.
    pub fn cost_query(&mut self) -> Value {
        let resp = self.call("cost/query", json!({}));
        resp["result"]["report"].clone()
    }
}

// ---------------------------------------------------------------------------
// Fixtures + config builders
// ---------------------------------------------------------------------------

/// Write the fixture demo repo into `repo`.
pub fn write_demo_repo(repo: &Path) {
    let files: &[(&str, &str)] = &[
        ("README.md", include_str!("fixtures/demo_repo/README.md")),
        ("Cargo.toml", include_str!("fixtures/demo_repo/Cargo.toml")),
        ("src/lib.rs", include_str!("fixtures/demo_repo/src/lib.rs")),
        (
            "src/main.rs",
            include_str!("fixtures/demo_repo/src/main.rs"),
        ),
        (
            "secrets/prod.env",
            include_str!("fixtures/demo_repo/secrets/prod.env"),
        ),
    ];
    for (rel, contents) in files {
        let path = repo.join(rel);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, contents).unwrap();
    }
}

/// The local-engine script driving a read → edit → verify → done flow (AC-1).
pub fn edit_answer_script() -> String {
    [
        "I'll read the file first.\n{\"tool\": \"read\", \"arguments\": {\"path\": \"src/lib.rs\"}}",
        "Now change the constant.\n{\"tool\": \"edit\", \"arguments\": {\"path\": \"src/lib.rs\", \"old_string\": \"pub const ANSWER: u32 = 1;\", \"new_string\": \"pub const ANSWER: u32 = 2;\"}}",
        "Verify the change landed.\n{\"tool\": \"shell\", \"arguments\": {\"command\": \"grep -q 'ANSWER: u32 = 2' src/lib.rs && echo VERIFIED\"}}",
        "Done. src/lib.rs now defines ANSWER = 2 and the change is verified.",
    ]
    .join("\n---\n")
}

/// A tiny mock MCP server as a shell script (stdio transport, ADR-003). It reads
/// one line per incoming message and answers `initialize`, `tools/list` (an
/// `echo` tool), and `tools/call` in lockstep — the same shape the daemon's MCP
/// client speaks.
const MOCK_MCP_STDIO_SERVER: &str = r#"#!/bin/sh
read _initialize
printf '%s\n' '{"jsonrpc":"2.0","id":1,"result":{"protocolVersion":"2024-11-05","serverInfo":{"name":"demo","version":"0.1"},"capabilities":{"tools":{}}}}'
read _initialized
read _list
printf '%s\n' '{"jsonrpc":"2.0","id":2,"result":{"tools":[{"name":"echo","description":"echoes text back","inputSchema":{"type":"object","properties":{"text":{"type":"string"}}}}]}}'
read _call
printf '%s\n' '{"jsonrpc":"2.0","id":3,"result":{"content":[{"type":"text","text":"echoed from the demo MCP server"}],"isError":false}}'
"#;

/// Write the mock MCP stdio server script and return its path.
pub fn write_mcp_stdio_server(root: &Path) -> PathBuf {
    let path = root.join("mcp_server.sh");
    std::fs::write(&path, MOCK_MCP_STDIO_SERVER).unwrap();
    path
}

/// The MCP config JSON registering the stdio `demo` server at `script`.
pub fn mcp_stdio_config(script: &Path) -> String {
    json!([{
        "id": "demo",
        "transport": { "kind": "stdio", "command": "sh", "args": [script.to_string_lossy()] }
    }])
    .to_string()
}

/// The local-engine script that calls the `demo` MCP server's `echo` tool (AC-9).
pub fn mcp_call_script() -> String {
    [
        "I'll use the knowledge base.\n{\"tool\": \"mcp__demo__echo\", \"arguments\": {\"text\": \"hello\"}}",
        "Done. The MCP tool returned its result.",
    ]
    .join("\n---\n")
}

fn now_nanos() -> u128 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos()
}
