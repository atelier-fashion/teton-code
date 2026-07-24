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
// Mock HTTP servers: accepting a connection
// ---------------------------------------------------------------------------

/// How long a mock server will wait on one accepted connection's I/O.
///
/// Generous — every fixture request and response is a few KiB — but finite, so a
/// client that connects and then says nothing cannot wedge a single-threaded
/// accept loop (and with it the server's `Drop`) forever.
const ACCEPTED_IO_TIMEOUT: Duration = Duration::from_secs(10);

/// Hand back an accepted connection in **blocking** mode, with bounded I/O
/// timeouts.
///
/// Both mock servers call `TcpListener::set_nonblocking(true)` so their accept
/// loop can poll a shutdown flag instead of blocking forever. On macOS and the
/// BSDs, though, the socket returned by `accept(2)` *inherits* the listener's
/// `O_NONBLOCK` — so without this, the very first read of the request returns
/// `WouldBlock` whenever the client's bytes have not landed yet. Both readers
/// treat a failed read as "no request": [`read_hf_request`] returns `None` and
/// drops the stream unanswered, and [`read_http_body`] records an empty body.
///
/// The client did nothing wrong — it just got descheduled between `connect` and
/// `send`, which on a loaded machine is common — and sees a `Connection reset by
/// peer` for a request the server never read. Putting the accepted socket back
/// into blocking mode makes the read wait for the bytes, which is what every one
/// of these handlers already assumes it does.
fn accepted(stream: TcpStream) -> TcpStream {
    stream
        .set_nonblocking(false)
        .expect("accepted socket returns to blocking mode");
    stream
        .set_read_timeout(Some(ACCEPTED_IO_TIMEOUT))
        .expect("accepted socket takes a read timeout");
    stream
        .set_write_timeout(Some(ACCEPTED_IO_TIMEOUT))
        .expect("accepted socket takes a write timeout");
    stream
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
                            handle_http(accepted(stream), &requests, &response);
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

    /// Write a model-catalog TOML and return its path (`TETON_CATALOG`).
    pub fn write_catalog(&self, toml: &str) -> PathBuf {
        let path = self.root.join("catalog.toml");
        std::fs::write(&path, toml).unwrap();
        path
    }

    /// The daemon's state directory: where the decision record and the weights
    /// live, and the reason a restarted daemon remembers anything (D-4).
    pub fn state_dir(&self) -> PathBuf {
        self.runtime_dir.join("teton")
    }

    /// Where installed weights land. Local display only — this path never
    /// crosses the protocol boundary (BR-11), which is why a test that wants it
    /// derives it the same way the CLI does.
    pub fn weights_dir(&self) -> PathBuf {
        self.state_dir().join("models")
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
            // DECISION 3: the acceptance suite drives the daemon through gated
            // test seams (TETON_CATALOG, TETON_DISK_FREE_BYTES,
            // TETON_DOWNLOAD_RETRY_BASE_MS), which a debug build honours only with
            // this master switch set.
            .env("TETON_TEST_SEAMS", "1")
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

    // -- REQ-547 consent helpers --

    /// Pump events until one named `name` arrives (or `window` elapses),
    /// returning the first such event — including one already observed.
    pub fn wait_for_event(&mut self, name: &str, window: Duration) -> Option<Value> {
        if let Some(seen) = self.events_named(name).first() {
            return Some((*seen).clone());
        }
        let deadline = Instant::now() + window;
        loop {
            let remaining = deadline
                .checked_duration_since(Instant::now())
                .unwrap_or_default();
            if remaining.is_zero() {
                return None;
            }
            match self.rx.recv_timeout(remaining) {
                Ok(Incoming::Event(ev)) => {
                    let matched = ev.get("event").and_then(Value::as_str) == Some(name);
                    self.on_event(ev);
                    if matched {
                        return self.events_named(name).last().map(|e| (*e).clone());
                    }
                }
                Ok(Incoming::Response(_)) => {}
                Err(_) => return None,
            }
        }
    }

    /// Pump events until one named `name` **satisfies `pred`** (or `window`
    /// elapses), returning that event.
    ///
    /// [`Self::wait_for_event`] returns on the *first* event of a name; a
    /// lifecycle stream sends many `model_lifecycle` events and a test usually
    /// waits for a *particular* stage. Polling for the stage the assertions
    /// depend on — rather than draining a fixed duration and hoping it arrived —
    /// is what keeps the suite from flaking on a loaded runner where the replayed
    /// lifecycle can land after any guessed window. Events already observed are
    /// considered first, so a stage that arrived before the call is not missed.
    pub fn wait_for_event_where(
        &mut self,
        name: &str,
        mut pred: impl FnMut(&Value) -> bool,
        window: Duration,
    ) -> Option<Value> {
        for ev in &self.events {
            if ev.get("event").and_then(Value::as_str) == Some(name) && pred(ev) {
                return Some(ev.clone());
            }
        }
        let deadline = Instant::now() + window;
        loop {
            let remaining = deadline
                .checked_duration_since(Instant::now())
                .unwrap_or_default();
            if remaining.is_zero() {
                return None;
            }
            match self.rx.recv_timeout(remaining) {
                Ok(Incoming::Event(ev)) => {
                    let matched =
                        ev.get("event").and_then(Value::as_str) == Some(name) && pred(&ev);
                    self.on_event(ev);
                    if matched {
                        return self.events.last().cloned();
                    }
                }
                Ok(Incoming::Response(_)) => {}
                Err(_) => return None,
            }
        }
    }

    /// Await the first-run proposal *as an event*
    /// (`model_selection_proposed`).
    ///
    /// Note the daemon publishes its proposal on a task spawned beside
    /// `server::serve` (D-3), so it may be published before this client is
    /// subscribed and the event may never arrive. Delivery does not depend on
    /// that race: [`Self::await_outstanding_proposal`] retrieves the same payload
    /// whenever the client shows up, and is what every shipped client uses.
    pub fn await_proposal(&mut self, window: Duration) -> Value {
        self.wait_for_event("model_selection_proposed", window)
            .expect("the daemon should have proposed a model")
    }

    /// Poll `model/status` until a proposal is outstanding, returning it **in
    /// full** — the same payload the `model_selection_proposed` event carries.
    ///
    /// This is the path every shipped client takes (`teton`'s
    /// `answer_outstanding_model_proposal`).
    pub fn await_outstanding_proposal(&mut self, window: Duration) -> Value {
        let deadline = Instant::now() + window;
        loop {
            let status = self.model_status();
            if status["pending_proposal"].is_object() {
                return status["pending_proposal"].clone();
            }
            assert!(
                Instant::now() < deadline,
                "no model proposal became outstanding within {window:?}; last status: {status}"
            );
            thread::sleep(Duration::from_millis(25));
        }
    }

    /// Poll `model/status` until a proposal is outstanding, returning the
    /// `request_id` it must be answered with.
    pub fn await_pending_proposal(&mut self, window: Duration) -> String {
        let proposal = self.await_outstanding_proposal(window);
        proposal["request_id"]
            .as_str()
            .unwrap_or_else(|| panic!("an outstanding proposal carries a request_id: {proposal}"))
            .to_owned()
    }

    /// Answer an outstanding proposal (`model/confirm`).
    pub fn confirm_model(&mut self, request_id: &str, outcome: Value) -> Value {
        self.call(
            "model/confirm",
            json!({ "request_id": request_id, "outcome": outcome }),
        )
    }

    /// The catalog, each entry's fit, and the current selection (`model/list`).
    pub fn model_list(&mut self) -> Value {
        self.call("model/list", json!({}))["result"].clone()
    }

    /// The decision, install state, and any open proposal (`model/status`).
    pub fn model_status(&mut self) -> Value {
        self.call("model/status", json!({}))["result"].clone()
    }

    /// Poll `model/status` until the selected weights report `status`, or give up.
    pub fn wait_for_install_status(&mut self, status: &str, window: Duration) -> Value {
        let deadline = Instant::now() + window;
        loop {
            let reported = self.model_status();
            if reported["install"]["status"].as_str() == Some(status) {
                return reported;
            }
            if Instant::now() >= deadline {
                return reported;
            }
            thread::sleep(Duration::from_millis(25));
        }
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

/// The MCP config JSON registering the stdio `demo` server at `script`. This is
/// the `TETON_MCP_CONFIG` **test-override** shape (see [`DaemonOptions::mcp`]); the
/// main-TOML source of truth is [`mcp_stdio_toml`].
pub fn mcp_stdio_config(script: &Path) -> String {
    json!([{
        "id": "demo",
        "transport": { "kind": "stdio", "command": "sh", "args": [script.to_string_lossy()] }
    }])
    .to_string()
}

/// The main config TOML registering the stdio `demo` MCP server at `script` via a
/// `[[mcp_server]]` table — the single source of truth for MCP registration
/// (AC-9). The daemon reads this from `TETON_CONFIG`, no side file.
pub fn mcp_stdio_toml(script: &Path) -> String {
    format!(
        "[[mcp_server]]\nid = \"demo\"\n\n\
         [mcp_server.transport]\nkind = \"stdio\"\ncommand = \"sh\"\nargs = [\"{}\"]\n",
        script.to_string_lossy()
    )
}

/// The local-engine script that calls the `demo` MCP server's `echo` tool (AC-9).
///
/// The final reply quotes the tool's real output via the `{{LAST_TOOL_RESULT}}`
/// placeholder (see `ScriptedFileEngine`), so the scripted continuation genuinely
/// depends on the MCP result reaching context — a plumbing regression that
/// discarded the result would erase the sentinel and fail the AC-9 assertion.
pub fn mcp_call_script() -> String {
    [
        "I'll use the knowledge base.\n{\"tool\": \"mcp__demo__echo\", \"arguments\": {\"text\": \"hello\"}}",
        "Done. The MCP tool returned: {{LAST_TOOL_RESULT}}",
    ]
    .join("\n---\n")
}

fn now_nanos() -> u128 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos()
}

// ---------------------------------------------------------------------------
// Mock HuggingFace host (REQ-547 TASK-008)
// ---------------------------------------------------------------------------
//
// The consent matrix needs a model host it can make behave: serve an artifact,
// hand off to a CDN with a `302`, rate-limit, or hand back bytes that are the
// right length and the wrong content. Mocking that surface — rather than
// fetching from huggingface.co — is what keeps the suite hermetic and fast; the
// *real* HF contract is TASK-006's job, and it is the only network-touching one.
//
// The artifact is small and its `sha256` is genuinely computed from the bytes
// served, so the verify path (BR-6) is exercised rather than asserted: point the
// host at different bytes and the digest check fails for the real reason.

/// What the mock host does with a `…/resolve/…` request.
#[derive(Clone)]
pub enum HfArtifact {
    /// Serve the requested file's real bytes from [`MockHfConfig::files`],
    /// honouring a `Range` request.
    Files,
    /// Serve the requested file's *length* in bytes that are not its bytes: the
    /// right size, the wrong content, so nothing but the digest can catch it
    /// (AC-7).
    CorruptFiles,
    /// Answer `302` with `Location: <base><path>` — the HF → CDN handoff a
    /// credential-free, redirect-following client must complete (BR-14).
    RedirectTo(String),
    /// Answer every fetch with this status, optionally with a `Retry-After`.
    /// A `Retry-After: 0` walks the whole real ladder without spending its
    /// seconds (AC-12).
    Status {
        /// The HTTP status to answer with.
        code: u16,
        /// `Retry-After` header value in seconds, when one should be sent.
        retry_after_secs: Option<u64>,
    },
}

/// One file in a mock `GET /api/models/<repo>/tree/<revision>` response.
#[derive(Clone)]
pub struct HfTreeFile {
    /// Path within the repository.
    pub path: String,
    /// The LFS object id — which *is* the artifact's SHA-256 (architecture D-1).
    pub oid: String,
    /// The LFS object size in bytes.
    pub size: u64,
}

/// How a [`MockHf`] should behave.
pub struct MockHfConfig {
    /// The `resolve` behaviour.
    pub artifact: HfArtifact,
    /// Artifact bytes, keyed by the file's last path segment.
    pub files: std::collections::BTreeMap<String, Vec<u8>>,
    /// Tree listings, keyed `"<repo>@<revision>"`.
    pub tree: std::collections::BTreeMap<String, Vec<HfTreeFile>>,
    /// Resolve requests answered with [`Self::fail_status`] before the artifact
    /// is served — the transient-then-recovers path.
    pub fail_first: usize,
    /// Status for those first `fail_first` requests.
    pub fail_status: u16,
    /// `Retry-After` sent alongside them.
    pub fail_retry_after_secs: Option<u64>,
    /// Accept every connection and close it without answering — a dead proxy, a
    /// half-closed keep-alive, a peer that resets mid-response.
    ///
    /// Distinct from [`HfArtifact::Status`] on purpose: a 503 is upstream
    /// *speaking*, this is the conversation breaking. Neither is evidence about
    /// the catalog, and a client that cannot tell them apart from a digest
    /// disagreement will eventually report an outage as corruption.
    pub drop_connections: bool,
}

impl Default for MockHfConfig {
    fn default() -> Self {
        Self {
            artifact: HfArtifact::Files,
            files: std::collections::BTreeMap::new(),
            tree: std::collections::BTreeMap::new(),
            fail_first: 0,
            fail_status: 429,
            fail_retry_after_secs: Some(0),
            drop_connections: false,
        }
    }
}

struct HfState {
    config: MockHfConfig,
    failures_left: AtomicUsize,
    requests: Mutex<Vec<String>>,
}

/// A localhost stand-in for HuggingFace: the LFS metadata API and the artifact
/// `resolve` endpoint, with every request path recorded.
pub struct MockHf {
    port: u16,
    state: Arc<HfState>,
    running: Arc<AtomicBool>,
    handle: Option<thread::JoinHandle<()>>,
}

impl MockHf {
    /// Start a host behaving as `config` describes.
    pub fn start(config: MockHfConfig) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind mock hf");
        let port = listener.local_addr().unwrap().port();
        listener.set_nonblocking(true).unwrap();

        let state = Arc::new(HfState {
            failures_left: AtomicUsize::new(config.fail_first),
            config,
            requests: Mutex::new(Vec::new()),
        });
        let running = Arc::new(AtomicBool::new(true));

        let handle = {
            let state = Arc::clone(&state);
            let running = Arc::clone(&running);
            thread::spawn(move || {
                while running.load(Ordering::SeqCst) {
                    match listener.accept() {
                        Ok((stream, _)) => serve_hf(accepted(stream), &state),
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
            state,
            running,
            handle: Some(handle),
        }
    }

    /// A host serving every fixture model's real bytes.
    pub fn serving(models: &[TestModel]) -> Self {
        Self::start(MockHfConfig {
            files: file_map(models),
            ..MockHfConfig::default()
        })
    }

    /// A host serving every fixture model at the right length and the wrong
    /// content (AC-7).
    pub fn corrupting(models: &[TestModel]) -> Self {
        Self::start(MockHfConfig {
            artifact: HfArtifact::CorruptFiles,
            files: file_map(models),
            ..MockHfConfig::default()
        })
    }

    /// The base URL to configure as `[local_model] base_url` (BR-16).
    pub fn base_url(&self) -> String {
        format!("http://127.0.0.1:{}", self.port)
    }

    /// Every request path this host received, in order.
    pub fn requests(&self) -> Vec<String> {
        self.state.requests.lock().unwrap().clone()
    }

    /// How many *artifact* requests this host received.
    ///
    /// The number AC-1 is about: metadata reads are not model data, so counting
    /// every request would make the assertion mean something weaker than it says.
    pub fn artifact_request_count(&self) -> usize {
        self.requests()
            .iter()
            .filter(|path| path.contains("/resolve/"))
            .count()
    }
}

impl Drop for MockHf {
    fn drop(&mut self) {
        self.running.store(false, Ordering::SeqCst);
        if let Some(handle) = self.handle.take() {
            let _ = handle.join();
        }
    }
}

/// Serve one mock-HF request.
fn serve_hf(mut stream: TcpStream, state: &Arc<HfState>) {
    if state.config.drop_connections {
        // Closing with the request still unread is what makes the peer see a
        // reset rather than a clean EOF — the real shape of a broken transport.
        return;
    }
    let Some((path, range_from)) = read_hf_request(&mut stream) else {
        return;
    };
    state.requests.lock().unwrap().push(path.clone());

    if let Some(rest) = path.strip_prefix("/api/models/") {
        serve_hf_api(&mut stream, state, rest);
        return;
    }

    // Scripted transient failures come first: they are about the *transport*,
    // so they must precede whatever the artifact behaviour is.
    if state.failures_left.load(Ordering::SeqCst) > 0 {
        state.failures_left.fetch_sub(1, Ordering::SeqCst);
        write_hf_status(
            &mut stream,
            state.config.fail_status,
            state.config.fail_retry_after_secs,
        );
        return;
    }

    let file = path.rsplit('/').next().unwrap_or_default().to_owned();
    match &state.config.artifact {
        HfArtifact::Files => match state.config.files.get(&file) {
            Some(bytes) => write_hf_body(&mut stream, bytes, range_from),
            None => write_hf_status(&mut stream, 404, None),
        },
        HfArtifact::CorruptFiles => match state.config.files.get(&file) {
            // Right length, wrong bytes: only the SHA-256 can tell.
            Some(bytes) => {
                let corrupt: Vec<u8> = bytes.iter().map(|b| b ^ 0xa5).collect();
                write_hf_body(&mut stream, &corrupt, range_from);
            }
            None => write_hf_status(&mut stream, 404, None),
        },
        HfArtifact::RedirectTo(base) => {
            let location = format!("{}{}", base.trim_end_matches('/'), path);
            let head = format!(
                "HTTP/1.1 302 Found\r\nLocation: {location}\r\nContent-Length: 0\r\n\
                 Connection: close\r\n\r\n"
            );
            let _ = stream.write_all(head.as_bytes());
            let _ = stream.flush();
        }
        HfArtifact::Status {
            code,
            retry_after_secs,
        } => write_hf_status(&mut stream, *code, *retry_after_secs),
    }
}

/// Answer the two metadata endpoints `tools/refresh-catalog.py` reads: the repo
/// info (public/ungated flags) and the LFS tree at a revision (architecture D-1).
fn serve_hf_api(stream: &mut TcpStream, state: &Arc<HfState>, rest: &str) {
    let rest = rest.split('?').next().unwrap_or(rest);
    let body = match rest.split_once("/tree/") {
        Some((repo, revision)) => {
            let key = format!("{repo}@{revision}");
            match state.config.tree.get(&key) {
                Some(files) => {
                    let entries: Vec<Value> = files
                        .iter()
                        .map(|f| {
                            json!({
                                "type": "file",
                                "path": f.path,
                                "size": f.size,
                                "lfs": { "oid": f.oid, "size": f.size, "pointerSize": 134 },
                            })
                        })
                        .collect();
                    Value::Array(entries)
                }
                None => {
                    write_hf_status(stream, 404, None);
                    return;
                }
            }
        }
        // Repo info. Public and ungated: the mock stands in for a repo the
        // catalog is *allowed* to name, so the gate's other refusals stay
        // exercised by their own cases rather than by this one failing early.
        None => json!({
            "id": rest,
            "sha": "0".repeat(40),
            "gated": false,
            "private": false,
            "disabled": false,
        }),
    };
    let text = body.to_string();
    let head = format!(
        "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\
         Connection: close\r\n\r\n",
        text.len()
    );
    let _ = stream.write_all(head.as_bytes());
    let _ = stream.write_all(text.as_bytes());
    let _ = stream.flush();
}

/// Write a bare status response, with an optional `Retry-After`.
fn write_hf_status(stream: &mut TcpStream, code: u16, retry_after_secs: Option<u64>) {
    let reason = match code {
        404 => "Not Found",
        429 => "Too Many Requests",
        503 => "Service Unavailable",
        _ => "Error",
    };
    let mut head = format!("HTTP/1.1 {code} {reason}\r\nContent-Length: 0\r\n");
    if let Some(secs) = retry_after_secs {
        head.push_str(&format!("Retry-After: {secs}\r\n"));
    }
    head.push_str("Connection: close\r\n\r\n");
    let _ = stream.write_all(head.as_bytes());
    let _ = stream.flush();
}

/// Write `bytes`, honouring a `Range: bytes=<from>-` with a `206`.
fn write_hf_body(stream: &mut TcpStream, bytes: &[u8], range_from: Option<u64>) {
    let total = bytes.len() as u64;
    let head = match range_from {
        Some(from) if from < total => {
            let slice_len = total - from;
            format!(
                "HTTP/1.1 206 Partial Content\r\nContent-Range: bytes {}-{}/{}\r\n\
                 Content-Length: {slice_len}\r\nConnection: close\r\n\r\n",
                from,
                total - 1,
                total
            )
        }
        _ => format!("HTTP/1.1 200 OK\r\nContent-Length: {total}\r\nConnection: close\r\n\r\n"),
    };
    let from = range_from.filter(|from| *from < total).unwrap_or(0) as usize;
    let _ = stream.write_all(head.as_bytes());
    let _ = stream.write_all(&bytes[from..]);
    let _ = stream.flush();
}

/// Read a request line + headers, returning the path and any `Range` start.
fn read_hf_request(stream: &mut TcpStream) -> Option<(String, Option<u64>)> {
    let mut reader = BufReader::new(stream.try_clone().ok()?);
    let mut request_line = String::new();
    if reader.read_line(&mut request_line).ok()? == 0 {
        return None;
    }
    let path = request_line.split_whitespace().nth(1)?.to_owned();
    let mut range_from = None;
    loop {
        let mut line = String::new();
        if reader.read_line(&mut line).ok()? == 0 {
            break;
        }
        let trimmed = line.trim_end();
        if trimmed.is_empty() {
            break;
        }
        let lower = trimmed.to_ascii_lowercase();
        if let Some(rest) = lower.strip_prefix("range:") {
            range_from = rest
                .trim()
                .strip_prefix("bytes=")
                .and_then(|spec| spec.split('-').next())
                .and_then(|start| start.trim().parse::<u64>().ok());
        }
    }
    Some((path, range_from))
}

// ---------------------------------------------------------------------------
// The fixture catalog (REQ-547 TASK-008)
// ---------------------------------------------------------------------------

/// A fixture catalog entry: a real (tiny) artifact with its real digest.
pub struct TestModel {
    /// Catalog name.
    pub name: &'static str,
    /// HuggingFace-shaped repository.
    pub repo: &'static str,
    /// File within the repository.
    pub file: &'static str,
    /// The pinned 40-hex revision (BR-15).
    pub revision: &'static str,
    /// The band this entry serves.
    pub band: &'static str,
    /// Its RAM floor.
    pub ram_floor_bytes: u64,
    /// The bytes a mock host serves for it.
    pub payload: Vec<u8>,
}

impl TestModel {
    /// The canonical (pre-override) download URL.
    pub fn url(&self) -> String {
        format!(
            "https://huggingface.co/{}/resolve/{}/{}",
            self.repo, self.revision, self.file
        )
    }

    /// The genuine SHA-256 of [`Self::payload`] — computed, never pinned, so the
    /// fixture cannot drift away from the bytes it describes.
    pub fn sha256(&self) -> String {
        teton_inference::sha256_hex(&self.payload)
    }
}

/// Index every fixture model's payload by its file name, the way a host serves it.
pub fn file_map(models: &[TestModel]) -> std::collections::BTreeMap<String, Vec<u8>> {
    models
        .iter()
        .map(|m| (m.file.to_owned(), m.payload.clone()))
        .collect()
}

/// Deterministic pseudo-random bytes: a stand-in artifact small enough to move
/// in a test and incompressible enough to be a real digest input.
pub fn fixture_payload(len: usize, seed: u8) -> Vec<u8> {
    let mut state = u32::from(seed).wrapping_add(0x9e37_79b9);
    (0..len)
        .map(|_| {
            state ^= state << 13;
            state ^= state >> 17;
            state ^= state << 5;
            (state & 0xff) as u8
        })
        .collect()
}

/// The fixture catalog: one entry per band, mirroring the shipped catalog's
/// shape (pinned revisions, real digests) at a size CI can actually fetch.
///
/// A 16 GiB machine lands in the `small` band, so `tiny-small` is what the probe
/// proposes, `tiny-mid` is a legal override, and `tiny-large` is the
/// above-RAM-floor pick BR-3's second confirmation exists for.
pub fn fixture_models() -> Vec<TestModel> {
    vec![
        TestModel {
            name: "tiny-small",
            repo: "teton-fixtures/tiny-gguf",
            file: "tiny-small-q4_k_m.gguf",
            revision: "1111111111111111111111111111111111111111",
            band: "small",
            ram_floor_bytes: 3 * (1024 * 1024 * 1024),
            payload: fixture_payload(64 * 1024, 1),
        },
        TestModel {
            name: "tiny-mid",
            repo: "teton-fixtures/tiny-gguf",
            file: "tiny-mid-q4_k_m.gguf",
            revision: "2222222222222222222222222222222222222222",
            band: "mid",
            ram_floor_bytes: 9 * (1024 * 1024 * 1024),
            payload: fixture_payload(96 * 1024, 2),
        },
        TestModel {
            name: "tiny-large",
            repo: "teton-fixtures/tiny-gguf",
            file: "tiny-large-q4_k_m.gguf",
            revision: "3333333333333333333333333333333333333333",
            band: "large",
            ram_floor_bytes: 21 * (1024 * 1024 * 1024),
            payload: fixture_payload(128 * 1024, 3),
        },
    ]
}

/// Render `models` as a catalog TOML the daemon accepts via `TETON_CATALOG`.
pub fn fixture_catalog_toml(models: &[TestModel]) -> String {
    let mut out = String::from("# fixture catalog (REQ-547 TASK-008)\nversion = 1\n");
    for model in models {
        out.push_str(&format!(
            "\n[[models]]\nname = \"{}\"\nurl = \"{}\"\nrevision = \"{}\"\n\
             sha256 = \"{}\"\nsize_bytes = {}\nram_floor_bytes = {}\nband = \"{}\"\n",
            model.name,
            model.url(),
            model.revision,
            model.sha256(),
            model.payload.len(),
            model.ram_floor_bytes,
            model.band,
        ));
    }
    out
}

/// The `[local_model]` config block redirecting fetches at `base_url` (BR-16).
pub fn local_model_block(base_url: &str, auto_accept: bool) -> String {
    format!("[local_model]\nauto_accept = {auto_accept}\nbase_url = \"{base_url}\"\n\n")
}
