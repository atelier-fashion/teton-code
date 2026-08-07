//! End-to-end smoke tests that spawn the **real** `teton` CLI binary
//! (`CARGO_BIN_EXE_teton`) against a live `teton-code`.
//!
//! This is the client-surface layer the daemon-side acceptance matrix (the `daemon` crate's
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

/// Path to the sibling `teton-code` daemon binary (built alongside `teton` into the
/// same target directory under `--workspace`).
fn daemon_bin() -> PathBuf {
    teton_bin()
        .parent()
        .expect("teton binary has a parent dir")
        .join("teton-code")
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

/// A short-lived `daemon`, spawned into an isolated `XDG_RUNTIME_DIR`, killed on
/// drop. The short `/tmp` base keeps the Unix socket path under `SUN_LEN`.
struct TestDaemon {
    child: Child,
    root: PathBuf,
    runtime_dir: PathBuf,
    socket: PathBuf,
}

impl TestDaemon {
    /// A daemon with no local engine: the first-run consent gate is live, and a
    /// proposal is outstanding when a client attaches.
    fn spawn(daemon: &Path) -> Self {
        Self::spawn_with_script(daemon, None)
    }

    /// A daemon whose local tier is the `TETON_LOCAL_SCRIPT` scripted engine,
    /// replaying `replies` one per turn in order.
    ///
    /// The scripted-session tests need both of the things this buys. A scripted
    /// engine downloads nothing, so it is exempt from the first-run consent gate
    /// (`tetond` E-5): no proposal is outstanding when the CLI attaches, and every
    /// piped stdin line therefore reaches the *entry loop* instead of being eaten
    /// as the answer to a consent question. And the tier can actually serve a
    /// turn, so a prompt produces a real `route_decided` event and a real turn
    /// end — which is what makes AC-4's `/verbose` toggle observable at all.
    fn spawn_scripted(daemon: &Path, replies: &[&str]) -> Self {
        Self::spawn_with_script(daemon, Some(replies))
    }

    fn spawn_with_script(daemon: &Path, replies: Option<&[&str]>) -> Self {
        static SEQ: AtomicUsize = AtomicUsize::new(0);
        let seq = SEQ.fetch_add(1, Ordering::SeqCst);
        let root =
            PathBuf::from("/tmp").join(format!("tc{:x}{:x}", std::process::id() & 0xffff, seq));
        let runtime_dir = root.join("x");
        std::fs::create_dir_all(&runtime_dir).unwrap();

        // A config with one remote provider so `doctor` has something to render,
        // deliberately left in the pre-REQ-557 shape (no `model`) because
        // `provider_list_shows_the_migrated_model` asserts the load-time
        // migration resolves it.
        //
        // Every tier is bound to the **local** tier (REQ-558): this daemon serves
        // turns from a scripted local engine and cannot call DeepSeek, so the
        // binding says so rather than leaving it to a heuristic. Before REQ-558 a
        // turn reached the local model only when the prompt happened to contain
        // one of `AUXILIARY_SIGNALS`' ten words — which is the defect REQ-558
        // deletes, and which made every assertion in the slash-command section
        // silently depend on the wording of its fixture prompts.
        //
        // `[local_model] base_url` points the model download at a port nothing
        // is listening on. Two things follow, both deliberate: no test here can
        // reach huggingface.co, and an accepted proposal fails its *download*
        // fast while still recording the decision — which is the half of the
        // consent round-trip these CLI tests are about. Whether the bytes then
        // arrive is `daemon`'s `consent_matrix`, against a mock host.
        let config_path = root.join("config.toml");
        let tiers: String = ["reflex", "scan", "build", "think"]
            .iter()
            .map(|t| format!("[[tiers]]\ntier = \"{t}\"\nprovider_id = \"local\"\n\n"))
            .collect();
        std::fs::write(
            &config_path,
            format!(
                "[[providers]]\nid = \"deepseek\"\nkind = \"openai-compatible\"\n\
                 endpoint = \"https://api.deepseek.com\"\n\n\
                 [[providers]]\nid = \"local\"\nkind = \"local\"\n\n\
                 {tiers}\
                 [local_model]\nauto_accept = false\nbase_url = \"http://127.0.0.1:{}\"\n",
                closed_port()
            ),
        )
        .unwrap();

        // The canned local-engine replies, when this daemon was asked for a
        // scripted tier. Blocks are separated by a `---` line — the format
        // `ScriptedFileEngine::from_script` parses.
        let script = replies.map(|replies| {
            let path = root.join("local_script.txt");
            std::fs::write(&path, replies.join("\n---\n")).unwrap();
            path
        });

        let log = std::fs::File::create(root.join("tetond.log")).unwrap();
        let mut command = Command::new(daemon);
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
        if let Some(script) = &script {
            command.env("TETON_LOCAL_SCRIPT", script);
        }
        let child = command.spawn().expect("spawn teton-code");

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

    /// The daemon's own stderr so far.
    ///
    /// The other half of the story in any assertion about what the daemon
    /// published: the CLI's stdout says what arrived, and only this says what
    /// was sent. Quoted into the assertions that turn on an event, because a
    /// failure that shows one side is a failure nobody can act on.
    fn log(&self) -> String {
        std::fs::read_to_string(self.root.join("tetond.log")).unwrap_or_default()
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
            "daemon socket never appeared at {}. log:\n{log}",
            self.socket.display()
        );
    }

    /// Run `teton <args...>` pointed at this daemon and return combined
    /// stdout+stderr (the CLI writes its rendered lines to stdout).
    fn run_cli(&self, teton: &Path, args: &[&str]) -> String {
        self.run_cli_with_stdin(teton, args, "")
    }

    /// Run `teton <args...>` and return its **stdout alone**.
    ///
    /// The suite's default capture concatenates stdout and stderr, which is the
    /// right haystack for "did this run say X anywhere". It is the wrong
    /// *needle*: AC-2 compares one surface's rendering against the other's own
    /// bytes, and a needle carrying a diagnostic that only ever goes to stderr
    /// would fail a comparison the rendering passed — or, worse, quietly widen
    /// what counts as a match.
    fn run_cli_stdout(&self, teton: &Path, args: &[&str]) -> String {
        self.run_cli_streams(teton, args, "", CliSeams::Off).0
    }

    /// Run `teton <args...>` with `stdin` piped in, so an *interactive* prompt
    /// can be answered by the test the way a user answers it.
    ///
    /// `stdin` is closed after the given input, which is what ends the session
    /// loop (the CLI treats EOF as "done", never as an answer).
    fn run_cli_with_stdin(&self, teton: &Path, args: &[&str], stdin: &str) -> String {
        self.run_cli_capture(teton, args, stdin).0
    }

    /// As [`Self::run_cli_with_stdin`], but with the CLI told it is under test
    /// control (`TETON_TEST_SEAMS=1`).
    ///
    /// Only the `/model set` tests need it. That command is typed-input-only
    /// (REQ-555 spec Permissions, security review 2026-08-04): on piped stdin it
    /// refuses and points at `teton model set`, unless a **debug** build finds
    /// this switch set — the same posture the daemon takes towards its own seams
    /// (`tetond`'s `test_seams_enabled`). A release binary refuses either way,
    /// so this allowance cannot ship as a bypass.
    fn run_cli_seamed(&self, teton: &Path, args: &[&str], stdin: &str) -> String {
        self.run_cli_capture_seamed(teton, args, stdin, CliSeams::On)
            .0
    }

    /// As [`Self::run_cli_with_stdin`], but also returning the process's exit
    /// status — AC-5 claims an exit code, not only an output, and a claim about
    /// an exit code has to look at one.
    fn run_cli_capture(
        &self,
        teton: &Path,
        args: &[&str],
        stdin: &str,
    ) -> (String, std::process::ExitStatus) {
        self.run_cli_capture_seamed(teton, args, stdin, CliSeams::Off)
    }

    fn run_cli_capture_seamed(
        &self,
        teton: &Path,
        args: &[&str],
        stdin: &str,
        seams: CliSeams,
    ) -> (String, std::process::ExitStatus) {
        let (mut combined, stderr, status) = self.run_cli_streams(teton, args, stdin, seams);
        combined.push_str(&stderr);
        (combined, status)
    }

    /// The one place a CLI process is actually run: stdout, stderr and status,
    /// kept apart. Every other runner here is a view over this.
    fn run_cli_streams(
        &self,
        teton: &Path,
        args: &[&str],
        stdin: &str,
        seams: CliSeams,
    ) -> (String, String, std::process::ExitStatus) {
        let mut command = Command::new(teton);
        command
            .args(args)
            .env("XDG_RUNTIME_DIR", &self.runtime_dir)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        match seams {
            CliSeams::On => command.env("TETON_TEST_SEAMS", "1"),
            // Removed rather than simply not set: a developer who exports the
            // switch in their shell must still run the test CI runs.
            CliSeams::Off => command.env_remove("TETON_TEST_SEAMS"),
        };
        let mut child = command
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
        (
            String::from_utf8_lossy(&output.stdout).into_owned(),
            String::from_utf8_lossy(&output.stderr).into_owned(),
            output.status,
        )
    }
}

/// Whether a CLI process is told it is under test control.
///
/// The suite's default is `Off`, because every command except `/model set` is
/// pipe-friendly (BR-9) and must be tested as it ships.
#[derive(Clone, Copy, PartialEq, Eq)]
enum CliSeams {
    On,
    Off,
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
        // **Keep the evidence when the test is failing.**
        //
        // This used to delete the root unconditionally, panicking or not — and
        // the root holds `tetond.log`, the daemon's whole stderr. So the one run
        // that could say what the daemon actually did was also the run that
        // destroyed the record of it. `an_escaped_line_and_a_plain_line_both_reach_the_model`
        // failed once, on a cold build, with a routing notice missing; the CLI's
        // stdout was quoted in the panic and the daemon's side was already gone,
        // which is why that failure could not be diagnosed after the fact.
        //
        // `panicking()` rather than an env switch: a passing run still cleans up
        // after itself, so nothing accumulates in `/tmp` in the normal case.
        if std::thread::panicking() {
            eprintln!(
                "cli_e2e: test failed — keeping the daemon's working directory at {} \
                 (its stderr is {}/tetond.log)",
                self.root.display(),
                self.root.display()
            );
            return;
        }
        let _ = std::fs::remove_dir_all(&self.root);
    }
}

#[test]
fn teton_doctor_and_cost_report_against_a_live_daemon() {
    let daemon = daemon_bin();
    if !daemon.exists() {
        // `cargo test -p teton` alone does not build the sibling daemon; the
        // workspace test run does. Skip cleanly rather than fail in that case.
        let _ = std::io::stderr()
            .write_all(b"skipping CLI e2e: teton-code binary not built (run under --workspace)\n");
        return;
    }

    let daemon = TestDaemon::spawn(&daemon);
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
// `daemon`'s `consent_matrix` drives the daemon over the socket directly. What
// it cannot show is that the *shipped CLI* renders the machine's reasoning,
// reads a human's answer from a terminal, and puts a well-formed `model/confirm`
// on the wire. That is this file's job, and TASK-007 deferred it here on purpose.
//
// The daemon is spawned with no `TETON_LOCAL_SCRIPT`, so the consent gate is
// genuinely live and a proposal is genuinely outstanding when the CLI attaches.

/// Skip cleanly when the sibling daemon has not been built (a bare
/// `cargo test -p teton` does not build it; the workspace run does).
fn daemon_or_skip() -> Option<PathBuf> {
    let daemon = daemon_bin();
    if daemon.exists() {
        return Some(daemon);
    }
    let _ = std::io::stderr().write_all(
        b"skipping CLI consent e2e: teton-code binary not built (run under --workspace)\n",
    );
    None
}

#[test]
fn teton_renders_the_first_run_proposal_and_accepts_it_interactively() {
    let Some(daemon) = daemon_or_skip() else {
        return;
    };
    let daemon = TestDaemon::spawn(&daemon);
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
    let Some(daemon) = daemon_or_skip() else {
        return;
    };
    let daemon = TestDaemon::spawn(&daemon);
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
    let Some(daemon) = daemon_or_skip() else {
        return;
    };
    let daemon = TestDaemon::spawn(&daemon);
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

// ---------------------------------------------------------------------------
// H-1 / E-4 — a refused config reaches the person who has to fix it
// ---------------------------------------------------------------------------

/// A daemon that refuses its config must say so *to the user*, through the CLI
/// that started it.
///
/// H-1 made a present-but-invalid config a refusal instead of a silent fall-open
/// to `Config::default()` — but the refusal was invisible from where it mattered.
/// Two things hid it, and both are fixed here: the daemon bound its socket
/// *before* loading the config, so the CLI's `connect` succeeded into the listen
/// backlog and then died at the handshake with a bare EOF; and the CLI spawned
/// the daemon with `stderr` on `/dev/null`, so the diagnostic went nowhere. That
/// combination is what every existing REQ-544 user with a top-level
/// `pinned_local_model` key would have hit on their first start after this REQ
/// hard-deprecated it: a daemon that will not start and a CLI that cannot say
/// why.
///
/// This test uses that exact key, and never spawns `daemon` itself — the CLI's
/// own autostart path is the thing under test.
#[test]
fn a_refused_config_is_reported_by_the_cli_that_autostarted_the_daemon() {
    let daemon = daemon_bin();
    if !daemon.exists() {
        let _ = std::io::stderr()
            .write_all(b"skipping CLI e2e: teton-code binary not built (run under --workspace)\n");
        return;
    }

    let root = PathBuf::from("/tmp").join(format!("tcbad{:x}", std::process::id()));
    let runtime_dir = root.join("x");
    std::fs::create_dir_all(&runtime_dir).unwrap();
    let config_path = root.join("config.toml");
    // REQ-544's key, hard-deprecated by DECISION 2. Under REQ-544 it meant
    // "override the probe's pick"; it is now a validation error pointing at the
    // replacement.
    std::fs::write(&config_path, "pinned_local_model = \"qwen2.5-coder-3b\"\n").unwrap();

    // `cost` (unlike `doctor`, which only *reports* whether a daemon is up) goes
    // through `ensure_connected`, which is the autostart path under test.
    let output = Command::new(teton_bin())
        .arg("cost")
        .env("XDG_RUNTIME_DIR", &runtime_dir)
        .env("TETON_CONFIG", &config_path)
        .env("TETON_REPO_ROOT", &root)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
        .expect("run teton cost");
    let mut combined = String::from_utf8_lossy(&output.stdout).into_owned();
    combined.push_str(&String::from_utf8_lossy(&output.stderr));

    assert!(
        combined.contains("could not reach the daemon"),
        "the CLI must report the autostart failure; output:\n{combined}"
    );
    assert!(
        combined.contains("The daemon reported:"),
        "the CLI must quote the daemon's own diagnostic rather than guessing; \
         output:\n{combined}"
    );
    assert!(
        combined.contains("configuration is present but invalid"),
        "the quoted diagnostic must be the config refusal itself; output:\n{combined}"
    );
    assert!(
        combined.contains("pinned_local_model"),
        "and it must name the key the user has to change; output:\n{combined}"
    );

    let _ = std::fs::remove_dir_all(&root);
}

// ---------------------------------------------------------------------------
// In-session slash commands, driven through a pipe against a live daemon
// (REQ-555 AC-1 / AC-2 / AC-3b / AC-4 / AC-5 / AC-6 / AC-7 / AC-7b)
// ---------------------------------------------------------------------------
//
// The unit tests in `slash.rs` pin the classifier and the dispatch table; what
// they cannot show is the *shipped* loop intercepting a typed line before it
// becomes a `prompt/turn`, rendering the command through the session's own
// connection, and leaving through the same exit Ctrl-D leaves through. That is
// this section's job, and it needs a daemon that can actually serve a turn —
// hence [`TestDaemon::spawn_scripted`].
//
// Two properties of that fixture shape every test below:
//
//   * The scripted local tier is exempt from the first-run consent gate, so no
//     proposal is outstanding and every stdin line reaches the entry loop. (With
//     the unscripted daemon the first line would answer a consent question, as
//     the REQ-547 tests above rely on.)
//   * Every turn is served locally because the fixture config **binds every tier
//     to the local tier** (REQ-558 BR-1), so the scripted engine answers whatever
//     the prompt says. Before REQ-558 that depended on the prompt containing one
//     of `AUXILIARY_SIGNALS`' ten words, which meant these assertions held for the
//     wording of their fixture prompts rather than for the property under test —
//     the trap BUG-155 named and this REQ removes.

/// The scripted engine's replies, one per turn in order. Each is a distinct
/// marker, so a test can say *which* turn produced a line — and so a slash
/// command that leaked into the prompt path shows up as a reply that should not
/// exist.
const TURN_REPLIES: &[&str] = &[
    "scripted-turn-one complete.",
    "scripted-turn-two complete.",
    "scripted-turn-three complete.",
];

/// Assert that no prompt turn was attempted at all (BR-1).
///
/// Observed from outside the process, "no `prompt/turn` was issued" is the
/// absence of everything a turn produces. Which markers carry that weight
/// depends on the session, and it is worth being exact about it:
///
///   * [`TURN_REPLIES`], `prompt failed` and `model still loading` are the
///     **load-bearing** guards in every session, quiet or verbose. A turn that
///     the scripted engine served prints its reply; a turn the daemon refused
///     prints the failure, or — when the local tier was still coming up
///     (BUG-152) — the waiting notice that replaced it. One of the three
///     happens for any line that reached `prompt/turn`.
///   * `does not execute prompt turns yet` covers a daemon too old to run turns
///     at all — also unconditional.
///   * `route [` and `turn ended` only ever render in a **verbose** session
///     (REQ-555 D-5), so in the quiet sessions this helper is called from today
///     they are vacuously absent. They are kept for a caller that toggles
///     `/verbose` before the line under test, where they become the earliest
///     evidence a turn started; they are not what makes a quiet session's
///     assertion true.
fn assert_no_turn_ran(output: &str, what: &str) {
    for reply in TURN_REPLIES {
        assert!(
            !output.contains(reply),
            "{what} reached the model — the scripted engine answered it; output:\n{output}"
        );
    }
    for marker in [
        "route [",
        "turn ended",
        "prompt failed",
        // The literal of `main`'s `TIER_WARMING_HEADLINE`: a refusal that took
        // the BUG-152 path renders this instead of `prompt failed`, and a guard
        // that only knew the old string would go quiet exactly there.
        "model still loading",
        "does not execute prompt turns yet",
    ] {
        assert!(
            !output.contains(marker),
            "{what} produced `{marker}`, so a turn was attempted; output:\n{output}"
        );
    }
}

/// AC-1 (with AC-6 alongside it): `/help` lists every command and its summary,
/// the escape footer is there, and no turn is attempted for either the unknown
/// command or the known one.
///
/// The unknown command comes first on purpose: the `/help` listing that follows
/// it is the evidence that the entry loop kept accepting input after the hint
/// (AC-6), which a test that only looked at the hint could not tell from a loop
/// that had died.
#[test]
fn slash_help_lists_every_command_and_no_turn_is_attempted() {
    let Some(daemon) = daemon_or_skip() else {
        return;
    };
    let daemon = TestDaemon::spawn_scripted(&daemon, TURN_REPLIES);
    let teton = teton_bin();

    let session = daemon.run_cli_with_stdin(&teton, &[], "/frobnicate\n/help\n");

    // AC-6: one actionable line naming `/help`, and no RPC behind it. The
    // command is quoted as it was typed rather than rebuilt from the parsed
    // name, so the echo cannot invent a spelling the user did not use.
    assert!(
        session.contains("unknown command: `/frobnicate`"),
        "an unknown command must name what was typed; output:\n{session}"
    );
    assert!(
        session.contains("type /help for the commands this session knows."),
        "the hint must point at /help; output:\n{session}"
    );

    // AC-1: all six commands, each with the summary `/help` generates from the
    // dispatch table (BR-7) — asserted as the rendered `/name  summary` pair, so
    // a row that lost its summary fails here.
    for (name, summary) in [
        ("/help", "List the commands this session knows."),
        (
            "/cost",
            "Show the daemon's cost report, exactly as `teton cost` does.",
        ),
        ("/model", "Show the model the local tier is currently on."),
        (
            "/model set",
            "Switch the local tier to a catalog model: /model set <name>.",
        ),
        (
            "/verbose",
            "Toggle the routing and turn-end notices for this session.",
        ),
        ("/quit", "End the session, exactly as Ctrl-D does."),
    ] {
        let rendered = session
            .lines()
            .find(|line| line.contains(name) && line.contains(summary));
        assert!(
            rendered.is_some(),
            "`{name}` is missing from /help with its summary; output:\n{session}"
        );
    }

    // BR-7 covers the alias too: `/exit` dispatches, so `/help` names it — on
    // the `/quit` row, not as a seventh entry of its own.
    let quit_line = session
        .lines()
        .find(|line| line.contains("/quit"))
        .unwrap_or_else(|| panic!("/help listed no /quit row; output:\n{session}"));
    assert!(
        quit_line.contains("/exit"),
        "/help must name /exit on the /quit row; got: {quit_line}"
    );

    // AC-7b's documentation half: the escape hatch is one footer line.
    assert!(
        session.contains("//text sends text as a prompt with one leading slash"),
        "/help must document the // escape; output:\n{session}"
    );

    // AC-1's load-bearing half: neither line spent a model call.
    assert_no_turn_ran(&session, "`/frobnicate` and `/help`");
}

/// The line the cost report opens with, and the anchor AC-2's two surfaces are
/// compared on.
const COST_MARKER: &str = "── cost summary ──";

/// Everything a run printed from its **first** cost-summary marker onward.
///
/// For `teton cost` — read from stdout alone below — that is exactly the report:
/// `run_cost` renders it last. For a session it is the `/cost` command's report
/// followed by the rest of the session, which is why the comparison below is
/// `starts_with` and not `contains`. A session always contains a correct report
/// at its *end* (the session-end summary), so "somewhere in the output" would be
/// satisfied by a `/cost` that rendered nothing of the kind.
fn cost_report_from_first_marker<'a>(output: &'a str, what: &str) -> &'a str {
    let at = output
        .find(COST_MARKER)
        .unwrap_or_else(|| panic!("{what} printed no cost report; output:\n{output}"));
    &output[at..]
}

/// AC-2, e2e leg: a mid-session `/cost` renders the daemon's whole report, and
/// renders it identically to `teton cost` against the same daemon.
///
/// Two claims, and they need different evidence.
///
/// The count is what makes the first an assertion rather than a coincidence: the
/// session-end summary renders the report once on its own, so a session where
/// `/cost` did nothing still contains one. Two means the command rendered its
/// own — through `query_and_render_cost`, the single function behind every cost
/// surface (BR-4).
///
/// The second is the AC's actual words — "the same rendering `teton cost`
/// produces for the same daemon state" — and no single-process test can make it.
/// So the subcommand is run against this same daemon, and the session's **first**
/// report — the `/cost` command's — must open with the subcommand's whole block,
/// byte for byte. The daemon's ledger is empty and unchanged between the two
/// runs, so a difference could only come from the client rendering it twice over.
/// AC-2 says this must not be asserted "by string coincidence", and it is not:
/// the needle is the other surface's own bytes, not a string this test wrote
/// down.
///
/// Anchoring on the first marker is load-bearing. A `contains` would be
/// satisfied by the session-end summary, which renders correctly no matter what
/// `/cost` did — the assertion would be green against a `/cost` that printed its
/// own parallel report. (Checked by mutation, 2026-08-04: a hand-rolled
/// in-session rendering that kept both anchor strings passed `contains` and dies
/// on `starts_with`.)
#[test]
fn slash_cost_renders_the_daemons_report_mid_session() {
    let Some(daemon) = daemon_or_skip() else {
        return;
    };
    let daemon = TestDaemon::spawn_scripted(&daemon, TURN_REPLIES);
    let teton = teton_bin();

    let session = daemon.run_cli_with_stdin(&teton, &[], "/cost\n");

    assert_eq!(
        session.matches(COST_MARKER).count(),
        2,
        "/cost must render the report on top of the session-end one; output:\n{session}"
    );
    assert_eq!(
        session
            .matches("estimated savings vs anthropic/claude-opus-4")
            .count(),
        2,
        "/cost must render the daemon's savings baseline, not a stub; output:\n{session}"
    );

    // The cross-process half: the same daemon, asked the same question by the
    // subcommand. Its **stdout alone** is the needle — the rendering under test
    // is what the subcommand prints, and appending its stderr would put bytes
    // into the comparison that the session's `/cost` was never asked to produce.
    let subcommand = daemon.run_cli_stdout(&teton, &["cost"]);
    let report = cost_report_from_first_marker(&subcommand, "`teton cost`");
    let in_session = cost_report_from_first_marker(&session, "the session");
    assert!(
        in_session.starts_with(report),
        "the in-session report differs from `teton cost`'s.\n\
         --- teton cost ---\n{report}\n--- in session ---\n{in_session}"
    );

    assert_no_turn_ran(&session, "`/cost`");
}

/// AC-4: quiet by default, loud after `/verbose`, quiet again after the second
/// toggle — three real turns in one scripted session.
///
/// The output is split on the toggle echoes rather than searched as a whole:
/// "the notices render somewhere" would stay green if they rendered for the
/// wrong turn, which is exactly the drift a session-scoped toggle can suffer.
///
/// What the segments are anchored on matters. The daemon's per-client writer
/// drains two independent producers — request responses and the broadcast
/// event stream — and before the daemon's `EventFence`
/// (`tetond::server`, pinned by `tetond/tests/event_response_ordering.rs`),
/// a turn's trailing streamed text could be queued *after* that turn's own
/// response and render at the head of the next pump, one command later. The
/// fence now orders a turn's events ahead of its response, but this test keeps
/// its anchoring on the two markers whose position never depended on that
/// ordering — the routing notice (`route_decided` is published before the turn
/// runs, and the client's pump is FIFO up to the response it is waiting for)
/// and the turn-end line (the entry loop prints it on the response itself) —
/// so it stays correct about what it is about, the `/verbose` toggle, rather
/// than doubling as a second ordering test. That the turns ran at all is
/// asserted over the whole output, in order.
#[test]
fn slash_verbose_toggles_the_route_notice_around_real_turns() {
    let Some(daemon) = daemon_or_skip() else {
        return;
    };
    let daemon = TestDaemon::spawn_scripted(&daemon, TURN_REPLIES);
    let teton = teton_bin();

    let session = daemon.run_cli_with_stdin(
        &teton,
        &[],
        "explain the first thing\n\
         /verbose\n\
         explain the second thing\n\
         /verbose\n\
         explain the third thing\n",
    );

    let (quiet, rest) = session
        .split_once("verbose on")
        .unwrap_or_else(|| panic!("/verbose never echoed `verbose on`; output:\n{session}"));
    let (loud, quiet_again) = rest
        .split_once("verbose off")
        .unwrap_or_else(|| panic!("/verbose never echoed `verbose off`; output:\n{session}"));

    // All three turns ran, in order — three replies from a script that hands
    // them out one per turn.
    let mut previous = 0;
    for (turn, reply) in TURN_REPLIES.iter().enumerate() {
        let at = session.find(reply).unwrap_or_else(|| {
            panic!(
                "turn {} never reached the model; output:\n{session}",
                turn + 1
            )
        });
        assert!(
            at >= previous,
            "the turns did not run in order; output:\n{session}"
        );
        previous = at;
    }

    // Turn one, quiet: answered with nothing said about routing.
    assert!(
        !quiet.contains("route [") && !quiet.contains("turn ended"),
        "a default session must start quiet; segment:\n{quiet}"
    );

    // Turn two, after the toggle: the routing notice and the turn-end line. The
    // notice's key is the **category and tier** the turn resolved through
    // (REQ-558) — a freeform turn no longer renders as `[freeform]`, because
    // freeform stopped being a routing value at all.
    assert!(
        loud.contains("route [edit/build] → local"),
        "`/verbose` did not surface the routing notice; segment:\n{loud}"
    );
    assert!(
        loud.contains("turn ended"),
        "`/verbose` did not surface the turn-end line; segment:\n{loud}"
    );

    // Turn three, after the second toggle: quiet again — the toggle flips back,
    // it does not latch.
    assert!(
        !quiet_again.contains("route [") && !quiet_again.contains("turn ended"),
        "a second `/verbose` must hide the notices again; segment:\n{quiet_again}"
    );

    // And the counts, which is what makes the three segments an exclusive
    // partition rather than three independent searches: exactly one of the three
    // turns was ever narrated.
    //
    // These two `== 1`s originally rested on empirically-stable ordering; the
    // daemon's per-client writer has since been fixed to order a client's
    // events ahead of the response that follows them (PR #42, spun off from
    // the REQ-555 review), so the ordering is now guaranteed and pinned by
    // tetond's own event_response_ordering suite. Both markers remain
    // FIFO-bound to their own turn. If a line ever moves across a segment
    // boundary again, the regression is in the daemon's writer; do NOT weaken
    // these counts to `>= 1`, which would let a toggle that narrated every
    // turn pass.
    assert_eq!(
        session.matches("route [").count(),
        1,
        "exactly the verbose turn should have been narrated; output:\n{session}"
    );
    assert_eq!(
        session.matches("turn ended").count(),
        1,
        "exactly the verbose turn should have printed a turn-end line; output:\n{session}"
    );
}

/// AC-5: `/quit` ends the session exactly as Ctrl-D does — and so does `/exit`.
///
/// Three fresh daemons run the *same* history and then part ways only at the last
/// line: one session types `/quit`, one types `/exit`, the other closes stdin.
/// `/exit` is an alias of the `quit` row rather than a row of its own, so it
/// cannot leave by a different path — but it is the spelling a user actually
/// typed when they asked to leave (BUG-153), and "leaves like Ctrl-D does" is
/// the claim worth holding at the binary rather than at the table. In piped mode the
/// framed prompter degrades to a plain one and echoes nothing it read, so the two
/// runs are comparable as whole byte streams rather than as extracted summaries —
/// identical banner, identical session id (each daemon is fresh), identical
/// history, identical session-end block. That whole-output equality is a strictly
/// stronger claim than the AC's "identical session-end output", and it is the
/// honest one here: a pipe has no input echo to subtract.
///
/// The shared history is two RPC-bearing commands rather than a model turn, for
/// the reason documented on the `/verbose` test above: a turn's streamed text
/// and its response come from different producers, and although the daemon's
/// `EventFence` now orders a turn's own events ahead of its response, token
/// chunking within the stream is still not a byte-for-byte contract worth
/// leaning a whole-output equality on. Comparing two runs bytewise requires a
/// history whose output is deterministic; what AC-5 is about — that `/quit`
/// leaves through the session-end path Ctrl-D leaves through, rather than a
/// parallel shutdown — is unaffected by which commands preceded it, and the
/// cost summary here is the daemon's real one, over the session's real (empty)
/// ledger.
///
/// (On a TTY the prompter's EOF-vs-Enter cursor chrome legitimately differs;
/// the AC puts that out of scope, and this suite never sees it.)
///
/// If the whole-output equality ever flakes, the cause to look for is the
/// handshake: everything before the session-ready line is replayed startup
/// chatter from two different daemons, and only the two `TestDaemon`s being
/// freshly spawned makes it identical. The fallback that keeps the AC's own
/// claim intact is to compare from the session-ready line onward
/// (`split_once("ready (freeform)")`) — that is the session-end output AC-5
/// actually asks about. Weakening it further (comparing only the cost block)
/// would stop testing that `/quit` and Ctrl-D leave through the same path.
#[test]
fn slash_quit_ends_the_session_exactly_as_ctrl_d_does() {
    let Some(daemon_bin) = daemon_or_skip() else {
        return;
    };
    let teton = teton_bin();
    let history = "/model\n/cost\n";

    let quit_daemon = TestDaemon::spawn_scripted(&daemon_bin, TURN_REPLIES);
    let (quit, quit_status) =
        quit_daemon.run_cli_capture(&teton, &[], &format!("{history}/quit\n"));
    drop(quit_daemon);

    let exit_daemon = TestDaemon::spawn_scripted(&daemon_bin, TURN_REPLIES);
    let (exit, exit_status) =
        exit_daemon.run_cli_capture(&teton, &[], &format!("{history}/exit\n"));
    drop(exit_daemon);

    let eof_daemon = TestDaemon::spawn_scripted(&daemon_bin, TURN_REPLIES);
    let (eof, eof_status) = eof_daemon.run_cli_capture(&teton, &[], history);
    drop(eof_daemon);

    // The reported failure was `/exit` reaching the model, which replied
    // conversationally instead of leaving. Both halves are asserted: nothing
    // answered it, and nothing was said about it on the way out.
    assert_no_turn_ran(&exit, "/exit");
    assert!(
        !exit.contains("unknown command"),
        "/exit must be a command this session knows; output:\n{exit}"
    );

    // The session-end summary is there at all — a `/quit` that skipped it would
    // otherwise be "identical" to a Ctrl-D that skipped it too.
    assert!(
        quit.contains("no model calls were recorded this session."),
        "the session-end call summary is missing; output:\n{quit}"
    );
    assert_eq!(
        quit.matches("── cost summary ──").count(),
        2,
        "the history's /cost and the session-end summary should both render; output:\n{quit}"
    );

    assert_eq!(
        quit, eof,
        "/quit and Ctrl-D must produce the same session output.\n\
         --- /quit ---\n{quit}\n--- ctrl-d ---\n{eof}"
    );
    assert_eq!(
        exit, eof,
        "/exit must produce the same session output as Ctrl-D — no extra line, \
         no reply, just the exit.\n--- /exit ---\n{exit}\n--- ctrl-d ---\n{eof}"
    );
    assert!(
        quit_status.success() && exit_status.success() && eof_status.success(),
        "every path must exit 0; /quit: {quit_status:?}, /exit: {exit_status:?}, \
         ctrl-d: {eof_status:?}"
    );
}

/// AC-7 and AC-7b, e2e legs: a `//`-escaped line and a plain line both reach the
/// model; neither is dispatched as a command.
///
/// The escaped line is `//help …` on purpose — `/help` is a *real* row in the
/// dispatch table, so a classifier that checked the table before the escape
/// would print the command list here. It does not: the line is answered by the
/// model, and the daemon's own routing reason names the signal it matched inside
/// the escaped text ("what does"), which is the prompt text arriving at the
/// daemon and being classified there rather than the CLI reporting on itself.
///
/// What this cannot see is the byte-level shape of that text: the CLI never
/// echoes what it sent, and neither the scripted engine's reply nor any daemon
/// surface quotes the prompt back. That exactly one leading slash survives the
/// collapse is pinned in `slash.rs`
/// (`the_double_slash_escape_collapses_only_the_leading_pair`), the classifier
/// whose output this loop hands straight to `PromptTurnParams`; what is proved
/// here is the half a unit test cannot reach — that the escaped line defeats the
/// dispatch table in the shipped binary and becomes a turn.
#[test]
fn an_escaped_line_and_a_plain_line_both_reach_the_model() {
    let Some(daemon) = daemon_or_skip() else {
        return;
    };
    let daemon = TestDaemon::spawn_scripted(&daemon, TURN_REPLIES);
    let teton = teton_bin();

    let session = daemon.run_cli_with_stdin(
        &teton,
        &[],
        "/verbose\n\
         //help me: what does this repo do?\n\
         explain the plain path\n",
    );

    // Both lines became turns, in order: the escape first, the plain prompt
    // second.
    assert!(
        session.contains(TURN_REPLIES[0]),
        "the //-escaped line never reached the model; output:\n{session}"
    );
    assert!(
        session.contains(TURN_REPLIES[1]),
        "the plain prompt never reached the model; output:\n{session}"
    );
    // REQ-561 TASK-062: a bare count of routing notices stopped meaning "how
    // many turns ran" the moment a harness duty could announce one on the same
    // surface. `title` names the session on its first substantive turn, so the
    // turn routes and the duty route are counted apart — which is a stronger
    // statement than the single count was, not a weaker one.
    assert_eq!(
        session.matches("route [edit/build]").count(),
        2,
        "exactly two turns should have been routed; output:\n{session}"
    );
    // **On the one observed flake here, and what it was not** (REQ-561 verify).
    //
    // This assertion failed once on a cold-build run — `left: 0, right: 1`, the
    // `route [title/reflex]` line absent while both `route [edit/build]` lines
    // were present — and did not reproduce in isolation or under repetition. The
    // obvious suspect was a subscription race, since `title` is the first event
    // of a session's life and `wait_for_socket` only proves the daemon is
    // listening. It is not one: the daemon registers the connection on the event
    // bus *inside* the handshake handler and queues the handshake response
    // afterwards, synchronously and on the same connection, and the CLI blocks on
    // that response before it can send `session/create` or `session/prompt`. The
    // window is closed by construction. Nor was it a lagged subscription: an
    // overflowing subscriber is evicted from the bus outright, which is terminal,
    // so the two later routing notices could not have survived it.
    //
    // No code path was found that drops this event while keeping those, so the
    // cause is recorded as **undiagnosed** rather than guessed at. What is fixed
    // is the reason it could not be diagnosed: the daemon's stderr is now kept
    // when a test panics (see `Drop`) and quoted here, so a recurrence says which
    // side lost the event instead of only that the count was wrong.
    assert_eq!(
        session.matches("route [title/reflex]").count(),
        1,
        "the session is named once, by a duty rather than by a turn.\n\
         --- CLI output ---\n{session}\n--- daemon stderr ---\n{}",
        daemon.log()
    );

    // And neither was treated as a command: no dispatch, no rejection.
    assert!(
        !session.contains("List the commands this session knows."),
        "`//help …` dispatched /help instead of prompting the model; output:\n{session}"
    );
    assert!(
        !session.contains("unknown command"),
        "an escaped or plain line was run through the dispatch table; output:\n{session}"
    );

    // Both turns routed through the category chain, and each notice names the
    // category and tier that decided it (REQ-558 BR-1).
    //
    // Until REQ-558 this pair of assertions read `matched 'what does'` /
    // `matched 'explain'` — the routing reason quoted the `AUXILIARY_SIGNALS`
    // word it found in the prompt, which was the only evidence here that the
    // line's *text* reached the daemon rather than merely a turn. That evidence
    // is gone with the heuristic, and it should be: an assertion that held
    // because of the wording of its fixture prompt is exactly the trap BUG-155
    // named. The byte-level property is pinned where it can be pinned exactly —
    // `slash.rs`'s `the_double_slash_escape_collapses_only_the_leading_pair`,
    // named in this test's doc comment; what remains this test's own is that the
    // escaped line defeated the dispatch table and became a routed turn.
    assert_eq!(
        session.matches("route [edit/build] → local").count(),
        2,
        "both lines must route through the category chain; output:\n{session}"
    );
}

/// AC-3b, e2e leg: `/model set` runs the shared validate → confirm → set flow
/// against a live daemon.
///
/// TASK-036 pinned that flow's decision (`decide_model_set`) as a pure function,
/// which is where the consent gate belongs. What is left to prove — and what
/// only a live daemon can — is that the in-session command is wired to it: that
/// the catalog it validates against is the daemon's own `model/list`, that an
/// accepted name reaches `model/set` and changes what the *next* `/model` reads
/// back, and that a declined above-floor pick leaves the daemon's selection
/// where it was.
///
/// The `n` on its own line is the second confirmation's answer. If a regression
/// stopped asking, that line would fall through to the entry loop as a prompt —
/// which `assert_no_turn_ran` catches at the end.
///
/// This is one of the two tests in the file that run the CLI *seamed*
/// ([`TestDaemon::run_cli_seamed`]) — the other is the `--yes` waiver below, and
/// both are seamed for the same reason. `/model set` is refused on non-terminal
/// stdin and would otherwise decline a piped session outright; the refusal
/// itself is the next test's subject, and the gate's own decision is a unit test
/// in `slash.rs`.
#[test]
fn slash_model_set_runs_the_shared_flow_against_a_live_daemon() {
    let Some(daemon) = daemon_or_skip() else {
        return;
    };
    let daemon = TestDaemon::spawn_scripted(&daemon, TURN_REPLIES);
    let teton = teton_bin();

    let session = daemon.run_cli_seamed(
        &teton,
        &[],
        "/model set definitely-not-a-model\n\
         /model set qwen2.5-coder-1.5b\n\
         /model\n\
         /model set qwen3-coder-30b-a3b\n\
         n\n\
         /model\n",
    );

    // Leg one — an unknown name: nothing is sent, and the remedy is the catalog
    // itself (which is why in-session `/model list` is out of scope).
    assert!(
        session.contains("no catalog entry named `definitely-not-a-model`"),
        "an unknown name must be refused by name; output:\n{session}"
    );
    for name in [
        "qwen2.5-coder-1.5b",
        "qwen2.5-coder-3b",
        "qwen2.5-coder-7b",
        "qwen3-coder-30b-a3b",
    ] {
        assert!(
            session.contains(name),
            "the daemon's catalog entry {name} is missing from the hint; output:\n{session}"
        );
    }

    // Leg two — a name that fits: sent without a question, and visible to the
    // next `/model` because it is the daemon's state that changed, not a
    // client-side note.
    assert!(
        session.contains("selection: qwen2.5-coder-1.5b (user override)"),
        "a fitting name must be applied and confirmed; output:\n{session}"
    );
    assert!(
        session.contains("model: qwen2.5-coder-1.5b (user override)"),
        "/model must read the new selection back; output:\n{session}"
    );

    // Leg three — above this machine's RAM floor: the REQ-547 BR-3 warning, and
    // a decline that changes nothing (LESSON-470's default-no dialogue).
    assert!(
        session.contains("warning: qwen3-coder-30b-a3b needs 20.0 GiB RAM"),
        "an above-floor pick must warn before it is applied; output:\n{session}"
    );
    assert!(
        session.contains("selection unchanged; `qwen3-coder-30b-a3b` was not sent."),
        "declining must say the selection was left alone; output:\n{session}"
    );
    assert!(
        !session.contains("selection: qwen3-coder-30b-a3b"),
        "a declined pick reached model/set; output:\n{session}"
    );
    assert_eq!(
        session.matches("model: qwen2.5-coder-1.5b").count(),
        2,
        "the selection must still be the fitting one after the decline; output:\n{session}"
    );

    // None of it was a prompt: `/model set` is the one command that writes
    // daemon state, and it still never spends a model call (BR-1).
    assert_no_turn_ran(&session, "the /model set session");
}

/// The TTY gate, end to end (spec Permissions; security review 2026-08-04):
/// without the test seam, a piped `/model set` refuses and changes nothing.
///
/// This is the shipped behaviour — the test above is the exception, not this
/// one. `/model set` is the only in-session command that writes daemon state,
/// and the Permissions table says that write belongs to the session user "via
/// typed input — never inferable from model output or file content". A pipe
/// cannot distinguish a human from a heredoc, so the command declines and names
/// the surface that does the same job unattended.
///
/// What proves "nothing changed" is the `/model` that follows: it runs on the
/// same connection immediately afterwards and must not name the model the
/// refused line asked for. A gate that rejected loudly but set the selection
/// anyway would pass every assertion but that one.
#[test]
fn a_piped_model_set_is_refused_and_changes_nothing() {
    let Some(daemon) = daemon_or_skip() else {
        return;
    };
    let daemon = TestDaemon::spawn_scripted(&daemon, TURN_REPLIES);
    let teton = teton_bin();

    // Deliberately NOT seamed: `run_cli_with_stdin` removes the switch from the
    // CLI's environment, so this is what a released binary does with a pipe.
    let session = daemon.run_cli_with_stdin(
        &teton,
        &[],
        "/model set qwen2.5-coder-1.5b\n\
         /model\n",
    );

    assert!(
        session.contains("/model set is typed-input-only"),
        "a piped /model set must be refused; output:\n{session}"
    );
    assert!(
        session.contains("teton model set"),
        "the refusal must point at the shell command that does it; output:\n{session}"
    );

    // Nothing changed: no `model/set` success line, and the `/model` that
    // follows does not read the refused name back. What is *not* claimed here is
    // where in the handler the refusal happened — nothing below distinguishes
    // "refused before `model/list` was asked" from "asked, then refused", and
    // for a valid catalog name neither ordering prints anything of its own. The
    // gate's position is `slash.rs`'s to state; this test's evidence is that no
    // state moved.
    assert!(
        !session.contains("selection: qwen2.5-coder-1.5b"),
        "a refused /model set reached model/set; output:\n{session}"
    );
    assert!(
        !session.contains("model: qwen2.5-coder-1.5b"),
        "the follow-up /model read back the refused selection; output:\n{session}"
    );
    // And the session carried on: the `/model` after it answered.
    assert!(
        session.contains("model: "),
        "the entry loop must keep accepting input after the refusal; output:\n{session}"
    );

    assert_no_turn_ran(&session, "a refused /model set");
}

/// `--yes` waives the in-session above-RAM-floor confirmation, and consumes no
/// input line doing it (REQ-547 BR-3 / REQ-555 BR-4b; user-approved 2026-08-04).
///
/// The session inherits the flag because `/model set` runs the *same*
/// `apply_model_set` the subcommand runs — the Permissions table names `--yes`
/// the explicit unattended stand-in for the second confirmation, and one flow
/// means the session cannot answer that question differently from the shell.
///
/// The load-bearing half is the last line. With `--yes` the flow asks nothing,
/// so the `/model` that follows must reach the *entry loop* and read the new
/// selection back. If the confirmation were still asked, `/model` would be eaten
/// as its answer and the read-back would never appear — which is exactly how a
/// waiver that only *looks* applied would show up.
#[test]
fn yes_waives_the_in_session_above_floor_confirmation_without_eating_a_line() {
    let Some(daemon) = daemon_or_skip() else {
        return;
    };
    let daemon = TestDaemon::spawn_scripted(&daemon, TURN_REPLIES);
    let teton = teton_bin();

    // 20.0 GiB RAM floor against the fixture's 16 GiB probe: above the floor,
    // so the BR-3 gate is live and something has to answer it.
    let session = daemon.run_cli_seamed(
        &teton,
        &["-y"],
        "/model set qwen3-coder-30b-a3b\n\
         /model\n",
    );

    assert!(
        session.contains("--yes supplies the second confirmation (BR-3)"),
        "--yes must say it answered the RAM-floor question; output:\n{session}"
    );
    assert!(
        session.contains("selection: qwen3-coder-30b-a3b (user override)"),
        "--yes must let the above-floor pick through; output:\n{session}"
    );
    assert!(
        session.contains("model: qwen3-coder-30b-a3b (user override)"),
        "the following /model must reach the entry loop and read the new \
         selection back — no question consumed it; output:\n{session}"
    );

    assert_no_turn_ran(&session, "the --yes /model set session");
}

// ---------------------------------------------------------------------------
// REQ-557 — `provider add` requires the model it will call
// ---------------------------------------------------------------------------

/// AC-2: `teton provider add <id> --kind anthropic` with no `--model` exits
/// non-zero naming the flag, registers nothing, **and never asks for a
/// credential**.
///
/// The "never asks" half is the one worth a test. `run_provider_add` used to
/// call `read_secret` before it built the registration, so a missing argument
/// would have been discovered only *after* the user typed an API key into a
/// command that was always going to fail. Ordering a validation before an input
/// prompt is invisible to a test that only checks the exit code, which is why
/// this asserts on the prompt text too.
#[test]
fn provider_add_without_a_model_refuses_before_asking_for_a_credential() {
    let Some(daemon) = daemon_or_skip() else {
        return;
    };
    let daemon = TestDaemon::spawn(&daemon);
    let teton = teton_bin();

    let (output, status) = daemon.run_cli_capture(
        &teton,
        &["provider", "add", "unmodeled", "--kind", "anthropic"],
        "",
    );

    assert!(
        !status.success(),
        "a remote provider with no --model must exit non-zero; output:\n{output}"
    );
    assert!(
        output.contains("--model"),
        "the failure must name the flag that is missing; output:\n{output}"
    );
    assert!(
        !output.contains("API key for"),
        "the argument check must precede the credential prompt — a user must \
         never type a key into a command that cannot succeed; output:\n{output}"
    );

    // Registers nothing: the provider list is unchanged.
    let listed = daemon.run_cli(&teton, &["provider", "list"]);
    assert!(
        !listed.contains("unmodeled"),
        "a refused registration must not appear in `provider list`; output:\n{listed}"
    );
}

/// A **local** provider still registers without `--model`: the local model is
/// owned by the REQ-547 consent flow and is read there, never set here. The
/// requirement is on remote kinds only, and a parser-level `required` would have
/// broken this path.
#[test]
fn a_local_provider_still_registers_without_a_model() {
    let Some(daemon) = daemon_or_skip() else {
        return;
    };
    let daemon = TestDaemon::spawn(&daemon);
    let teton = teton_bin();

    let (output, status) = daemon.run_cli_capture(
        &teton,
        &["provider", "add", "on-device", "--kind", "local"],
        "",
    );

    assert!(
        status.success(),
        "a local provider needs no --model and no credential; output:\n{output}"
    );
    assert!(
        !output.contains("API key for"),
        "a local provider has no credential to read; output:\n{output}"
    );

    let listed = daemon.run_cli(&teton, &["provider", "list"]);
    assert!(
        listed.contains("on-device"),
        "the local provider should be registered; output:\n{listed}"
    );
}

/// `provider list` renders the model each provider calls, end to end against a
/// live daemon — including a provider that reached this state through the
/// load-time migration rather than through `provider add`.
///
/// (The rendering of a provider with *no* model is pinned as a unit test on
/// `render_config`, where both branches can be exercised without a daemon whose
/// fixture config would have been migrated already.)
#[test]
fn provider_list_renders_the_declared_model() {
    let Some(daemon) = daemon_or_skip() else {
        return;
    };
    let daemon = TestDaemon::spawn(&daemon);
    let teton = teton_bin();

    // The daemon's fixture config declares `deepseek` in the pre-REQ shape, so
    // the load-time migration resolves it through the legacy price lookup.
    let listed = daemon.run_cli(&teton, &["provider", "list"]);
    assert!(
        listed.contains("deepseek-chat"),
        "the listing must show the model the provider calls, not only its id; \
         output:\n{listed}"
    );
}

/// BUG-155 / REQ-557 AC-1: "registering a third with id `opus` fails."
///
/// It did not. The daemon's `RegisterProvider` is replace-or-insert, so a second
/// `provider add` on an existing id silently OVERWROTE the entry — the exact
/// command BR-3's headline ("Opus for design, Sonnet for build") invites people
/// to run twice, quietly collapsing two providers into one and re-pointing every
/// route the user believed went to the first.
///
/// The refusal must also come before the credential prompt, for the same reason
/// the `--model` check does.
#[test]
fn provider_add_refuses_an_id_that_is_already_registered() {
    let Some(daemon) = daemon_or_skip() else {
        return;
    };
    let daemon = TestDaemon::spawn(&daemon);
    let teton = teton_bin();

    // The fixture config already registers `deepseek`.
    let (output, status) = daemon.run_cli_capture(
        &teton,
        &[
            "provider",
            "add",
            "deepseek",
            "--kind",
            "openai-compatible",
            "--model",
            "deepseek-reasoner",
        ],
        "",
    );

    assert!(
        !status.success(),
        "re-adding an existing id must fail rather than overwrite; output:\n{output}"
    );
    assert!(
        output.contains("already registered"),
        "the failure must say why; output:\n{output}"
    );
    assert!(
        !output.contains("API key for"),
        "and must refuse before asking for a credential; output:\n{output}"
    );

    // The original registration is intact — not replaced by the refused one.
    let listed = daemon.run_cli(&teton, &["provider", "list"]);
    assert!(
        listed.contains("deepseek-chat"),
        "the existing provider must keep its model; output:\n{listed}"
    );
    assert!(
        !listed.contains("deepseek-reasoner"),
        "the refused registration must not have landed; output:\n{listed}"
    );
}

/// REQ-558 AC-11 / ADR-A / ADR-H: `teton policy show` renders the **daemon's**
/// resolved table through the real CLI binary.
///
/// The daemon-side half of AC-11 — that the projection `policy show` renders is
/// the resolver's own answer, byte for byte, and agrees with `route_decided`
/// and with the turn-failure sentence — is pinned in
/// `tetond/tests/e2e/routing_categories.rs`. What that cannot show is that the
/// shipped CLI reaches `config/get` at all and renders what comes back: a
/// `render_policy` unit test formats a struct nobody proved the binary fetched.
/// This closes that last mile, which is the same seam TASK-007 deferred here
/// for the consent flow.
///
/// A **scripted** tier, because a resolved row and an unresolved one take
/// different branches of the renderer and only the resolved one carries ADR-A's
/// call-site marker — a daemon with no engine renders eleven `unresolved` rows
/// and would leave the marker untested.
#[test]
fn policy_show_renders_the_daemons_resolved_table() {
    let Some(daemon) = daemon_or_skip() else {
        return;
    };
    let daemon = TestDaemon::spawn_scripted(&daemon, &["Done."]);
    let teton = teton_bin();

    let shown = daemon.run_cli(&teton, &["policy", "show"]);

    // Both halves of ADR-H's table are rendered.
    assert!(
        shown.contains("tiers —"),
        "the tier table is the primary surface and must be rendered; output:\n{shown}"
    );
    assert!(
        shown.contains("categories:"),
        "and the per-category rows below it; output:\n{shown}"
    );

    /// The rendered row whose first word is `name`, or `None`.
    fn row<'a>(shown: &'a str, name: &str) -> Option<&'a str> {
        shown.lines().find(|l| {
            l.trim_start_matches(['>', ' '])
                .starts_with(&format!("{name} "))
        })
    }

    // All four tiers, each showing this fixture's binding to the local tier.
    for tier in ["reflex", "scan", "build", "think"] {
        let line =
            row(&shown, tier).unwrap_or_else(|| panic!("no `{tier}` tier row; output:\n{shown}"));
        assert!(
            line.contains("→ local"),
            "tier `{tier}` must render its binding; row:\n{line}"
        );
    }

    // All eleven categories have a row, each naming its tier and its provider.
    // Matched on the row prefix rather than by bare substring, so `route` is
    // not satisfied by the word "Routing" inside somebody else's reason.
    for (category, tier) in [
        ("route", "reflex"),
        ("redact", "reflex"),
        ("title", "reflex"),
        ("digest", "scan"),
        ("compact", "scan"),
        ("triage", "scan"),
        ("edit", "build"),
        ("shell", "build"),
        ("design", "think"),
        ("debug", "think"),
        ("review", "think"),
    ] {
        let line = row(&shown, category)
            .unwrap_or_else(|| panic!("no `{category}` category row; output:\n{shown}"));
        assert!(
            line.contains(tier) && line.contains("→ local"),
            "category `{category}` must render its `{tier}` tier and its \
             provider; row:\n{line}"
        );
    }

    // ADR-A: a category with no call site says so every time it is printed, and
    // one that has a call site does not. The marker is derived from the
    // daemon's own call sites, so this also shows the CLI is rendering the
    // daemon's answer rather than a table of its own.
    //
    // The marked example is `redact`, the one category REQ-561 leaves unwired
    // (it is REQ-562's, because a model call inside the egress choke point needs
    // its own spec). It replaced `compact` here when TASK-063 gave `compact` a
    // call site — moved rather than dropped, because a test that only checks the
    // *unmarked* side would pass just as well against a renderer that had
    // forgotten how to print the marker at all.
    let unreached = row(&shown, "redact").expect("a `redact` row");
    assert!(
        unreached.contains("declared, no call site yet"),
        "an unreached category must be marked; row:\n{unreached}"
    );
    let reached = row(&shown, "edit").expect("an `edit` row");
    assert!(
        !reached.contains("no call site"),
        "`edit` has a call site and must not carry the marker; row:\n{reached}"
    );
    // REQ-561 AC-1, the half this REQ has answered so far: `triage` was on the
    // marked side of this very assertion until `GrepTool::refine` gave it a call
    // site. The marker is derived, so the row follows the code — and this is
    // where a regression that unwired the duty would surface.
    let triaged = row(&shown, "triage").expect("a `triage` row");
    assert!(
        !triaged.contains("no call site"),
        "`triage` is wired (REQ-561 TASK-060) and must not carry the marker; row:\n{triaged}"
    );
    // And the same for `shell`, which `ShellTool::refine` gave a call site in
    // TASK-061. REQ-558's ADR-I had deferred it as unroutable; BR-4b answered
    // that by dispatching on *interpreting* the output rather than on deciding
    // to run the command, which happens after the command has already run.
    let shell = row(&shown, "shell").expect("a `shell` row");
    assert!(
        !shell.contains("no call site"),
        "`shell` is wired (REQ-561 TASK-061) and must not carry the marker; row:\n{shell}"
    );
    // And `title`, which `DaemonRuntime::title_session` gave a call site in
    // TASK-062 — the one of the five that belongs to no tool. The field it
    // populates, `SessionSummary.title`, had been on the wire since the
    // skeleton and was simply never written to.
    let title = row(&shown, "title").expect("a `title` row");
    assert!(
        !title.contains("no call site"),
        "`title` is wired (REQ-561 TASK-062) and must not carry the marker; row:\n{title}"
    );
    // And `compact`, which `ContextManager::compact_if_pressured` gave a call
    // site in TASK-063 — the category this very assertion used to hold up as the
    // marked example. It runs at a soft fraction of the context budget, ahead of
    // the unconditional `truncate_to_budget` that still enforces it (ADR-4).
    let compact = row(&shown, "compact").expect("a `compact` row");
    assert!(
        !compact.contains("no call site"),
        "`compact` is wired (REQ-561 TASK-063) and must not carry the marker; row:\n{compact}"
    );

    // REQ-561 AC-16 / BR-11: every row names the content class it transmits,
    // through the shipped binary against a live daemon.
    //
    // What this adds over the `main.rs` unit test is that the daemon populates
    // the field at all and that a real `config/get` carried it. What it does
    // *not* show is that the CLI read the wire rather than recomputing the class
    // from the category — the two agree by construction, so this stays green
    // either way. `policy_show_prints_the_daemons_content_class_rather_than_
    // recomputing_it` is the test for that, and it exists because a mutation
    // proved this one blind to it.
    for (category, disclosed) in [
        ("route", "your prompt"),
        ("redact", "outbound payloads"),
        ("title", "your prompt"),
        ("digest", "tool output"),
        ("compact", "conversation history"),
        ("triage", "file content and your request"),
        ("edit", "the whole turn"),
        ("shell", "the command and its output"),
        ("design", "the whole turn"),
        ("debug", "the whole turn"),
        ("review", "the whole turn"),
    ] {
        let line = row(&shown, category).expect("a row for every category");
        assert!(
            line.contains(disclosed),
            "category `{category}` must disclose that it sends `{disclosed}`; row:\n{line}"
        );
    }

    // OQ-4's resolution, on the two rows a user would actually compare: one
    // `scan` binding, two different kinds of content leaving the machine. Binding
    // `scan` remotely for cheap long-context work also moves conversation history
    // off it, and re-splitting the binding is out of scope — so this line pair is
    // the whole mitigation, and it is worth nothing if both rows read alike.
    let triage_row = row(&shown, "triage").expect("a `triage` row");
    let compact_row = row(&shown, "compact").expect("a `compact` row");
    assert!(
        !triage_row.contains("conversation history") && !compact_row.contains("file content"),
        "the `scan` tier's two categories must not read as one disclosure;\
         \ntriage:\n{triage_row}\ncompact:\n{compact_row}"
    );

    // AC-16's other half: `redact` transmits nothing today, and its row says so
    // in one phrase rather than leaving a reader to join a content class at one
    // end of the line to a marker at the other. A class printed alone reads as a
    // live egress path.
    assert!(
        unreached.contains("would send outbound payloads; declared, no call site yet"),
        "`redact`'s class and its call-site marker must render adjacently; row:\n{unreached}"
    );

    // AC-12: the BR-9 declared default is configuration-visible, and the CLI
    // says so rather than leaving it compiled in silently.
    assert!(
        shown.contains("judgment_default"),
        "the declared default must be reported; output:\n{shown}"
    );
}
