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

        // A config with one provider so `doctor` has something to render. The
        // provider is never actually called (no session runs here).
        //
        // `[local_model] base_url` points the model download at a port nothing
        // is listening on. Two things follow, both deliberate: no test here can
        // reach huggingface.co, and an accepted proposal fails its *download*
        // fast while still recording the decision — which is the half of the
        // consent round-trip these CLI tests are about. Whether the bytes then
        // arrive is `daemon`'s `consent_matrix`, against a mock host.
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

    /// Run `teton <args...>` with `stdin` piped in, so an *interactive* prompt
    /// can be answered by the test the way a user answers it.
    ///
    /// `stdin` is closed after the given input, which is what ends the session
    /// loop (the CLI treats EOF as "done", never as an answer).
    fn run_cli_with_stdin(&self, teton: &Path, args: &[&str], stdin: &str) -> String {
        self.run_cli_capture(teton, args, stdin).0
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
        (combined, output.status)
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
//   * A turn is served locally only when the freeform heuristic classifies the
//     prompt as an *auxiliary* duty — `tetond`'s `AUXILIARY_SIGNALS`. Every
//     prompt line here therefore carries such a signal ("explain", "what does"),
//     so the turn reaches the scripted engine rather than the configured remote
//     default, which this suite deliberately cannot call.

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
/// absence of everything a turn produces: the scripted engine's reply (which a
/// served turn always prints), a routing notice, the turn-end line, and every
/// way a turn can fail. A command that reached the model would trip at least one
/// of these — including on a daemon that refused to serve it.
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

    // AC-6: one actionable line naming `/help`, and no RPC behind it.
    assert!(
        session.contains("unknown command: /frobnicate"),
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

    // AC-7b's documentation half: the escape hatch is one footer line.
    assert!(
        session.contains("//text sends text as a prompt with one leading slash"),
        "/help must document the // escape; output:\n{session}"
    );

    // AC-1's load-bearing half: neither line spent a model call.
    assert_no_turn_ran(&session, "`/frobnicate` and `/help`");
}

/// AC-2, e2e leg: a mid-session `/cost` renders the daemon's whole report.
///
/// The count is what makes this an assertion rather than a coincidence: the
/// session-end summary renders the report once on its own, so a session where
/// `/cost` did nothing still contains one. Two means the command rendered its
/// own — through `query_and_render_cost`, the single function behind every cost
/// surface (BR-4). Which figures appear is pinned against the renderer in
/// `cost_ui`'s unit tests; what this proves is that the in-session command is a
/// call site of it.
#[test]
fn slash_cost_renders_the_daemons_report_mid_session() {
    let Some(daemon) = daemon_or_skip() else {
        return;
    };
    let daemon = TestDaemon::spawn_scripted(&daemon, TURN_REPLIES);
    let teton = teton_bin();

    let session = daemon.run_cli_with_stdin(&teton, &[], "/cost\n");

    assert_eq!(
        session.matches("── cost summary ──").count(),
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
    assert_no_turn_ran(&session, "`/cost`");
}

/// AC-4: quiet by default, loud after `/verbose`, quiet again after the second
/// toggle — three real turns in one scripted session.
///
/// The output is split on the toggle echoes rather than searched as a whole:
/// "the notices render somewhere" would stay green if they rendered for the
/// wrong turn, which is exactly the drift a session-scoped toggle can suffer.
///
/// What the segments are anchored on matters, because one thing here has no
/// fixed position. The daemon's per-client writer drains two independent
/// producers — request responses and the broadcast event stream — so a turn's
/// trailing streamed text can be queued *after* that turn's own response and
/// then render at the head of the next pump, one command later. The routing
/// notice cannot move like that (`route_decided` is published before the turn
/// runs, and the client's pump is FIFO up to the response it is waiting for) and
/// neither can the turn-end line (the entry loop prints it on the response
/// itself). So the per-segment assertions below are made on those two markers
/// only; that the turns ran at all is asserted over the whole output, in order.
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

    // Turn two, after the toggle: the routing notice and the turn-end line.
    assert!(
        loud.contains("route [freeform] → local"),
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

/// AC-5: `/quit` ends the session exactly as Ctrl-D does.
///
/// Two fresh daemons run the *same* history and then part ways only at the last
/// line: one session types `/quit`, the other closes stdin. In piped mode the
/// framed prompter degrades to a plain one and echoes nothing it read, so the two
/// runs are comparable as whole byte streams rather than as extracted summaries —
/// identical banner, identical session id (each daemon is fresh), identical
/// history, identical session-end block. That whole-output equality is a strictly
/// stronger claim than the AC's "identical session-end output", and it is the
/// honest one here: a pipe has no input echo to subtract.
///
/// The shared history is two RPC-bearing commands rather than a model turn, and
/// the reason is the ordering property documented on the `/verbose` test above:
/// a turn's trailing streamed text is queued by a different producer than the
/// turn's response, so its position in the byte stream is not reproducible across
/// two processes. Comparing two runs bytewise requires a history whose output is
/// deterministic; what AC-5 is about — that `/quit` leaves through the session-end
/// path Ctrl-D leaves through, rather than a parallel shutdown — is unaffected by
/// which commands preceded it, and the cost summary here is the daemon's real
/// one, over the session's real (empty) ledger.
///
/// (On a TTY the prompter's EOF-vs-Enter cursor chrome legitimately differs;
/// the AC puts that out of scope, and this suite never sees it.)
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

    let eof_daemon = TestDaemon::spawn_scripted(&daemon_bin, TURN_REPLIES);
    let (eof, eof_status) = eof_daemon.run_cli_capture(&teton, &[], history);
    drop(eof_daemon);

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
    assert!(
        quit_status.success() && eof_status.success(),
        "both paths must exit 0; /quit: {quit_status:?}, ctrl-d: {eof_status:?}"
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
    assert_eq!(
        session.matches("route [").count(),
        2,
        "exactly two turns should have been routed; output:\n{session}"
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

    // The daemon classified the escaped line's own text — the routing reason
    // names the signal it found in it.
    assert!(
        session.contains("matched 'what does'"),
        "the escaped line's text did not reach the daemon's router; output:\n{session}"
    );
    assert!(
        session.contains("matched 'explain'"),
        "the plain line's text did not reach the daemon's router; output:\n{session}"
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
#[test]
fn slash_model_set_runs_the_shared_flow_against_a_live_daemon() {
    let Some(daemon) = daemon_or_skip() else {
        return;
    };
    let daemon = TestDaemon::spawn_scripted(&daemon, TURN_REPLIES);
    let teton = teton_bin();

    let session = daemon.run_cli_with_stdin(
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
