//! End-to-end smoke tests that spawn the **real** `teton` CLI binary
//! (`CARGO_BIN_EXE_teton`) against a live `tetond`.
//!
//! This is the client-surface layer the daemon-side acceptance matrix (`tetond`'s
//! `tests/e2e`) never exercised: it drove the daemon over the socket directly and
//! never ran the actual `teton` binary, so a regression in the CLI's own wiring —
//! for instance the CLI failing to call the daemon's authoritative `cost/query`
//! RPC (REQ-544 M-7) — was invisible to CI. These tests run the shipped binary and
//! assert on its stdout for `doctor` and `cost`.
//!
//! Everything is mock-backed with no live keys: the provider registered in config
//! is never actually called (neither `doctor` nor `cost` makes a model call), and
//! the CLI holds no network path of its own (BR-1).

use std::io::Write;
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{Duration, Instant};

/// Path to the `teton` binary under test.
fn teton_bin() -> PathBuf {
    PathBuf::from(env!("CARGO_BIN_EXE_teton"))
}

/// Path to the sibling `tetond` daemon binary (built alongside `teton` into the
/// same target directory under `--workspace`).
fn tetond_bin() -> PathBuf {
    teton_bin()
        .parent()
        .expect("teton binary has a parent dir")
        .join("tetond")
}

// ---------------------------------------------------------------------------
// `teton --version` — hermetic (no daemon needed)
// ---------------------------------------------------------------------------

#[test]
fn teton_version_flag_prints_the_version() {
    let output = Command::new(teton_bin())
        .arg("--version")
        .output()
        .expect("run teton --version");
    assert!(output.status.success(), "teton --version exited non-zero");
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        stdout.contains("teton"),
        "teton --version should name the binary; stdout: {stdout:?}"
    );
    assert!(
        stdout.contains(env!("CARGO_PKG_VERSION")),
        "teton --version should print {}; stdout: {stdout:?}",
        env!("CARGO_PKG_VERSION")
    );
}

// ---------------------------------------------------------------------------
// `teton doctor` / `teton cost` against a live daemon
// ---------------------------------------------------------------------------

/// A short-lived `tetond`, spawned into an isolated `XDG_RUNTIME_DIR`, killed on
/// drop. The short `/tmp` base keeps the Unix socket path under `SUN_LEN`.
struct TestDaemon {
    child: Child,
    root: PathBuf,
    runtime_dir: PathBuf,
    socket: PathBuf,
}

impl TestDaemon {
    fn spawn(tetond: &Path) -> Self {
        static SEQ: AtomicUsize = AtomicUsize::new(0);
        let seq = SEQ.fetch_add(1, Ordering::SeqCst);
        let root =
            PathBuf::from("/tmp").join(format!("tc{:x}{:x}", std::process::id() & 0xffff, seq));
        let runtime_dir = root.join("x");
        std::fs::create_dir_all(&runtime_dir).unwrap();

        // A config with one provider so `doctor` has something to render. The
        // provider is never actually called (no session runs here).
        //
        // `[local_model] base_url` points the model download at a port nothing
        // is listening on. Two things follow, both deliberate: no test here can
        // reach huggingface.co, and an accepted proposal fails its *download*
        // fast while still recording the decision — which is the half of the
        // consent round-trip these CLI tests are about. Whether the bytes then
        // arrive is `tetond`'s `consent_matrix`, against a mock host.
        let config_path = root.join("config.toml");
        std::fs::write(
            &config_path,
            format!(
                "[[providers]]\nid = \"deepseek\"\nkind = \"openai-compatible\"\n\
                 endpoint = \"https://api.deepseek.com\"\n\n\
                 [local_model]\nauto_accept = false\nbase_url = \"http://127.0.0.1:{}\"\n",
                closed_port()
            ),
        )
        .unwrap();

        let log = std::fs::File::create(root.join("tetond.log")).unwrap();
        let mut command = Command::new(tetond);
        command
            .env("XDG_RUNTIME_DIR", &runtime_dir)
            .env("TETON_CONFIG", &config_path)
            .env("TETON_REPO_ROOT", &root)
            // A deterministic machine, so the proposal is the same everywhere
            // this suite runs, and a retry ladder that does not hold the daemon
            // for half a minute on an unreachable host.
            .env(
                "TETON_PROBE_RAM_BYTES",
                (16u64 * 1024 * 1024 * 1024).to_string(),
            )
            .env(
                "TETON_PROBE_DISK_BYTES",
                (500u64 * 1024 * 1024 * 1024).to_string(),
            )
            .env("TETON_PROBE_GPU", "apple-silicon")
            // DECISION 3: the retry-delay seam is honoured only in a debug build
            // with this master switch set.
            .env("TETON_TEST_SEAMS", "1")
            .env("TETON_DOWNLOAD_RETRY_BASE_MS", "1")
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::from(log));
        let child = command.spawn().expect("spawn tetond");

        let socket = runtime_dir.join("teton").join("tetond.sock");
        let daemon = Self {
            child,
            root,
            runtime_dir,
            socket,
        };
        daemon.wait_for_socket();
        daemon
    }

    fn wait_for_socket(&self) {
        let deadline = Instant::now() + Duration::from_secs(10);
        while Instant::now() < deadline {
            if UnixStream::connect(&self.socket).is_ok() {
                return;
            }
            std::thread::sleep(Duration::from_millis(25));
        }
        let log = std::fs::read_to_string(self.root.join("tetond.log")).unwrap_or_default();
        panic!(
            "tetond socket never appeared at {}. log:\n{log}",
            self.socket.display()
        );
    }

    /// Run `teton <args...>` pointed at this daemon and return combined
    /// stdout+stderr (the CLI writes its rendered lines to stdout).
    fn run_cli(&self, teton: &Path, args: &[&str]) -> String {
        self.run_cli_with_stdin(teton, args, "")
    }

    /// Run `teton <args...>` with `stdin` piped in, so an *interactive* prompt
    /// can be answered by the test the way a user answers it.
    ///
    /// `stdin` is closed after the given input, which is what ends the session
    /// loop (the CLI treats EOF as "done", never as an answer).
    fn run_cli_with_stdin(&self, teton: &Path, args: &[&str], stdin: &str) -> String {
        let mut child = Command::new(teton)
            .args(args)
            .env("XDG_RUNTIME_DIR", &self.runtime_dir)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap_or_else(|e| panic!("spawn teton {args:?}: {e}"));
        child
            .stdin
            .take()
            .expect("piped stdin")
            .write_all(stdin.as_bytes())
            .expect("write teton stdin");
        let output = child
            .wait_with_output()
            .unwrap_or_else(|e| panic!("run teton {args:?}: {e}"));
        let mut combined = String::from_utf8_lossy(&output.stdout).into_owned();
        combined.push_str(&String::from_utf8_lossy(&output.stderr));
        combined
    }
}

/// A TCP port with nothing listening on it: bound to learn a free number, then
/// released.
fn closed_port() -> u16 {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind to find a free port");
    let port = listener.local_addr().unwrap().port();
    drop(listener);
    port
}

impl Drop for TestDaemon {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
        let _ = std::fs::remove_dir_all(&self.root);
    }
}

#[test]
fn teton_doctor_and_cost_report_against_a_live_daemon() {
    let tetond = tetond_bin();
    if !tetond.exists() {
        // `cargo test -p teton` alone does not build the sibling daemon; the
        // workspace test run does. Skip cleanly rather than fail in that case.
        let _ = std::io::stderr()
            .write_all(b"skipping CLI e2e: tetond binary not built (run under --workspace)\n");
        return;
    }

    let daemon = TestDaemon::spawn(&tetond);
    let teton = teton_bin();

    // `teton doctor`: reaches the running daemon and reports it plus the
    // configured provider.
    let doctor = daemon.run_cli(&teton, &["doctor"]);
    assert!(
        doctor.contains("daemon: running"),
        "doctor should report the live daemon; output:\n{doctor}"
    );
    assert!(
        doctor.contains("deepseek"),
        "doctor should render the configured provider; output:\n{doctor}"
    );

    // `teton cost`: renders the daemon's AUTHORITATIVE cost report from the
    // `cost/query` RPC — the baseline model and the estimate methodology come
    // from the daemon, not a client-side stub (REQ-544 M-7). Even with an empty
    // ledger the report names its baseline and labels the figure an estimate; a
    // regression that stopped calling `cost/query` would print neither.
    let cost = daemon.run_cli(&teton, &["cost"]);
    assert!(
        cost.contains("cost summary"),
        "cost should render the daemon's report; output:\n{cost}"
    );
    assert!(
        cost.contains("anthropic/claude-opus-4"),
        "cost should show the daemon's savings baseline model; output:\n{cost}"
    );
    assert!(
        cost.to_lowercase().contains("estimate"),
        "cost should label the savings an estimate; output:\n{cost}"
    );
}

// ---------------------------------------------------------------------------
// The first-run consent round-trip, driven by the real `teton` binary
// (REQ-547 AC-1 / AC-3 / AC-5)
// ---------------------------------------------------------------------------
//
// `tetond`'s `consent_matrix` drives the daemon over the socket directly. What
// it cannot show is that the *shipped CLI* renders the machine's reasoning,
// reads a human's answer from a terminal, and puts a well-formed `model/confirm`
// on the wire. That is this file's job, and TASK-007 deferred it here on purpose.
//
// The daemon is spawned with no `TETON_LOCAL_SCRIPT`, so the consent gate is
// genuinely live and a proposal is genuinely outstanding when the CLI attaches.

/// Skip cleanly when the sibling daemon has not been built (a bare
/// `cargo test -p teton` does not build it; the workspace run does).
fn tetond_or_skip() -> Option<PathBuf> {
    let tetond = tetond_bin();
    if tetond.exists() {
        return Some(tetond);
    }
    let _ = std::io::stderr()
        .write_all(b"skipping CLI consent e2e: tetond binary not built (run under --workspace)\n");
    None
}

#[test]
fn teton_renders_the_first_run_proposal_and_accepts_it_interactively() {
    let Some(tetond) = tetond_or_skip() else {
        return;
    };
    let daemon = TestDaemon::spawn(&tetond);
    let teton = teton_bin();

    // `y` accepts the model the CLI just named; the closed stdin that follows
    // ends the session loop.
    let session = daemon.run_cli_with_stdin(&teton, &[], "y\n");

    // BR-2: the hardware reasoning is on screen before the question is asked.
    assert!(
        session.contains("awaiting an answer"),
        "the CLI must surface the outstanding proposal; output:\n{session}"
    );
    assert!(
        session.contains("hardware:") && session.contains("16.0 GiB RAM"),
        "the CLI must render the detected hardware; output:\n{session}"
    );
    assert!(
        session.contains("band:") && session.contains("small"),
        "the CLI must render the band and the reason for it; output:\n{session}"
    );

    // BR-2's load-bearing half, and the REQ's whole premise: the CLI names the
    // *proposed* entry, with its download size and its RAM floor — over a real
    // socket, from a daemon that published the proposal before this process
    // existed. Before TASK-009 the shipped CLI could only offer "the daemon's own
    // pick for the small band", because the delivery path carried a request id
    // and nothing else.
    assert!(
        session.contains("proposed: qwen2.5-coder-3b"),
        "the CLI must name the proposed model, not its band; output:\n{session}"
    );
    assert!(
        session.contains("2.0 GiB download") && session.contains("needs 5.0 GiB RAM"),
        "the proposed model must carry its download size and RAM floor; output:\n{session}"
    );
    assert!(
        session.contains("Download local model qwen2.5-coder-3b"),
        "the question itself must name what it is asking to download; output:\n{session}"
    );
    assert!(
        !session.contains("the daemon's own pick"),
        "the band-only stand-in must be gone; output:\n{session}"
    );
    // The proposal is prompted exactly once, however it was delivered: a client
    // that both receives the event and polls `model/status` de-duplicates on the
    // shared request id.
    assert_eq!(
        session.matches("Download local model").count(),
        1,
        "the proposal must be prompted exactly once; output:\n{session}"
    );
    // BR-3: every selectable entry, with its download size and RAM floor.
    assert!(
        session.contains("qwen2.5-coder-7b") && session.contains("needs"),
        "the CLI must render the selectable catalog entries; output:\n{session}"
    );
    assert!(
        session.to_lowercase().contains("above this machine's ram"),
        "an entry the machine cannot hold must be shown as such, not hidden; output:\n{session}"
    );

    // The honest startup lifecycle (TASK-009): a machine that has not answered
    // has downloaded nothing, benchmarked nothing, and loaded nothing — and the
    // daemon says exactly that instead of replaying a synthetic ready sequence.
    assert!(
        session.contains("awaiting your decision"),
        "an undecided machine must report awaiting-decision; output:\n{session}"
    );
    assert!(
        !session.contains("local model qwen2.5-coder-3b ready"),
        "nothing may claim readiness before the weights exist; output:\n{session}"
    );

    // AC-1/AC-3: the answer reached the daemon and was recorded — asserted from
    // a *separate* process, so nothing here is the CLI believing itself.
    let status = daemon.run_cli(&teton, &["model", "status"]);
    assert!(
        status.contains("selection: qwen2.5-coder-3b"),
        "accepting must record the daemon's own pick; output:\n{status}"
    );
    assert!(
        !status.contains("declined"),
        "accepting must not read as a decline; output:\n{status}"
    );
    assert!(
        !status.contains("awaiting an answer"),
        "an answered proposal must no longer be outstanding; output:\n{status}"
    );
}

#[test]
fn teton_yes_answers_the_first_run_proposal_with_no_input() {
    let Some(tetond) = tetond_or_skip() else {
        return;
    };
    let daemon = TestDaemon::spawn(&tetond);
    let teton = teton_bin();

    // AC-5: no input at all — stdin is empty and closed immediately.
    let session = daemon.run_cli_with_stdin(&teton, &["--yes"], "");
    assert!(
        session.contains("auto-accept"),
        "`--yes` must say it answered without asking; output:\n{session}"
    );

    let status = daemon.run_cli(&teton, &["model", "status"]);
    assert!(
        status.contains("selection: qwen2.5-coder-3b"),
        "`--yes` must record the proposed model; output:\n{status}"
    );
    assert!(
        !status.contains("awaiting an answer"),
        "`--yes` must leave no prompt outstanding; output:\n{status}"
    );
}

#[test]
fn teton_model_list_renders_the_catalog_and_each_entry_fit() {
    let Some(tetond) = tetond_or_skip() else {
        return;
    };
    let daemon = TestDaemon::spawn(&tetond);
    let teton = teton_bin();

    // AC-9, cross-process: the catalog, the machine, and each entry's verdict.
    let list = daemon.run_cli(&teton, &["model", "list"]);
    for name in [
        "qwen2.5-coder-1.5b",
        "qwen2.5-coder-3b",
        "qwen2.5-coder-7b",
        "qwen3-coder-30b-a3b",
    ] {
        assert!(list.contains(name), "{name} missing from output:\n{list}");
    }
    assert!(
        list.contains("hardware:") && list.contains("16.0 GiB RAM"),
        "model list must describe the machine the fits were computed for; output:\n{list}"
    );
    assert!(
        list.contains("fits") && list.contains("above this machine's RAM"),
        "model list must render each entry's fit, both verdicts; output:\n{list}"
    );

    // `model set` changes the selection post-first-run, and refuses an
    // above-RAM-floor pick without the second confirmation (BR-3).
    let set = daemon.run_cli(&teton, &["model", "set", "qwen2.5-coder-1.5b"]);
    assert!(
        set.contains("qwen2.5-coder-1.5b"),
        "model set must confirm the new selection; output:\n{set}"
    );
    let status = daemon.run_cli(&teton, &["model", "status"]);
    assert!(
        status.contains("selection: qwen2.5-coder-1.5b"),
        "the change must be visible to the next invocation; output:\n{status}"
    );

    let refused =
        daemon.run_cli_with_stdin(&teton, &["model", "set", "qwen3-coder-30b-a3b"], "n\n");
    assert!(
        refused.to_lowercase().contains("warning"),
        "an above-RAM-floor pick must warn before it is applied; output:\n{refused}"
    );
    let after = daemon.run_cli(&teton, &["model", "status"]);
    assert!(
        after.contains("selection: qwen2.5-coder-1.5b"),
        "declining the warning must leave the selection alone; output:\n{after}"
    );
}
