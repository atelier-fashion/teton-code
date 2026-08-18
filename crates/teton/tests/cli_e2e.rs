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

mod common;
use common::{daemon_bin, teton_bin};

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
        Self::spawn_with_script(daemon, None, "")
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
        Self::spawn_with_script(daemon, Some(replies), "")
    }

    /// A scripted daemon whose config carries `extra` — one more TOML table
    /// appended to the fixture config.
    ///
    /// REQ-563's `[web]` table is the first thing any test here has needed to
    /// vary: web lookup is off by default (BR-1), so a session that exercises it
    /// has to be told to, and the switch is config rather than a flag or an
    /// environment seam. Appended rather than templated in, so every existing
    /// fixture's bytes are unchanged.
    fn spawn_scripted_with_config(daemon: &Path, replies: &[&str], extra: &str) -> Self {
        Self::spawn_with_script(daemon, Some(replies), extra)
    }

    /// A daemon with no local engine whose config carries `extra` — the
    /// [`Self::spawn`] fixture plus one more appended block.
    ///
    /// REQ-578's doctor advisory is about configs somebody **wrote by hand**,
    /// which is the one shape `provider add` can never produce: the registration
    /// seam composes a base URL into the request URL before it stores anything,
    /// so the endpoint the advisory exists for cannot be created through the
    /// CLI. Appending providers to the fixture config is how a test gets one.
    fn spawn_with_config(daemon: &Path, extra: &str) -> Self {
        Self::spawn_with_script(daemon, None, extra)
    }

    /// A scripted daemon whose presence verifier is the REQ-575 seam's — `"fail"`
    /// for a present-but-refusing mechanism, `"1"` for an accepting one.
    ///
    /// The seam rides `TETON_TEST_SEAMS`, which this fixture already sets and
    /// which a release build refuses to start under, so it cannot exist in a
    /// shipped binary (`tetond`'s `seam_verifier`). It is also **not** gated on
    /// the `presence` cargo feature: the seam installs a verifier in place of the
    /// build's own, so a default build driven through it takes exactly the
    /// `config/set` path a `presence` build takes with a real mechanism. That is
    /// what lets REQ-582 AC-11 be a plain e2e rather than a feature-gated one —
    /// see the two tests that use it.
    fn spawn_scripted_with_presence(daemon: &Path, replies: &[&str], mode: &str) -> Self {
        Self::spawn_with_script_env(
            daemon,
            Some(replies),
            "",
            &[("TETON_PRESENCE_ACCEPT", mode)],
        )
    }

    fn spawn_with_script(daemon: &Path, replies: Option<&[&str]>, extra_config: &str) -> Self {
        Self::spawn_with_script_env(daemon, replies, extra_config, &[])
    }

    fn spawn_with_script_env(
        daemon: &Path,
        replies: Option<&[&str]>,
        extra_config: &str,
        extra_env: &[(&str, &str)],
    ) -> Self {
        static SEQ: AtomicUsize = AtomicUsize::new(0);
        let seq = SEQ.fetch_add(1, Ordering::SeqCst);
        // The `-` is load-bearing: without it `tc{pid}{seq}` is ambiguous — pid
        // `0x123`/seq `0x45` and pid `0x1234`/seq `0x5` both render `tc12345`, and
        // two colliding daemons would share a runtime dir, a socket, and the
        // single-instance flock, with each `drop` deleting the other's root. That
        // cannot happen under plain `cargo test` (one process, unique `seq`) but it
        // can under `cargo nextest`, which runs test binaries in parallel. The name
        // stays short because `root` becomes an `XDG_RUNTIME_DIR` and the socket
        // under it has to fit in `SUN_LEN`.
        let root =
            PathBuf::from("/tmp").join(format!("tc{:x}-{:x}", std::process::id() & 0xffff, seq));
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
                 [local_model]\nauto_accept = false\nbase_url = \"http://127.0.0.1:{}\"\n\n\
                 {extra_config}",
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
            // REQ-565: this daemon is a *fixture* whose lifetime the test owns —
            // it is spawned here and killed in `Drop`. Without the `never`
            // policy it would exit as soon as the first CLI command
            // disconnected, and every subsequent command in the same test would
            // autostart a replacement that never saw `TETON_CONFIG` (the CLI
            // does not forward it), so the config this fixture was built around
            // would silently vanish mid-test.
            //
            // Pinning the fixture is not hiding the new behaviour: these tests
            // are about providers, models and rendering across commands, and
            // the lifetime itself has its own suite
            // (`tetond/tests/daemon_lifetime.rs`) that spawns real daemons under
            // the real default.
            .args(["--shutdown-policy", "never"])
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
        for (key, value) in extra_env {
            command.env(key, value);
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
        // Removed rather than simply not set, for the same reason as
        // `TETON_TEST_SEAMS` below: a developer who exports a provider key in
        // their shell must still run the test CI runs. `read_secret` takes this
        // variable ahead of the prompt, so an exported value would flip every
        // REQ-578 registration test off the "stdin is closed, so the flow stops
        // at the credential step" path they are all written against — and onto
        // one that writes to the **real OS keychain**, which has no test seam.
        command.env_remove("TETON_PROVIDER_KEY");
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
        cost.contains("anthropic/claude-fable-5"),
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

#[test]
fn teton_renders_the_first_run_proposal_and_accepts_it_interactively() {
    let daemon = daemon_bin();
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
    let daemon = daemon_bin();
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
    let daemon = daemon_bin();
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
///   * [`TURN_REPLIES`], `prompt failed`, `model still loading` and
///     `message queued` are the **load-bearing** guards in every session,
///     quiet or verbose. A turn that the scripted engine served prints its
///     reply; a turn the daemon refused prints the failure, or — when the
///     local tier was still coming up (BUG-152) — the waiting notice that
///     replaced it; a turn the daemon *held* for a warming tier (REQ-580)
///     prints the queued notice before anything else. One of the four happens
///     for any line that reached `prompt/turn`.
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
        // The head of `session_ui`'s `turn_queued` notice (REQ-580): a turn the
        // daemon held for a warming tier prints this before it prints anything
        // else, and it may print nothing else for a while.
        "message queued",
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
    let daemon = daemon_bin();
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

    // AC-1: every command, each with the summary `/help` generates from the
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
        // REQ-567 BR-8: the only way a user drops a conversation the daemon now
        // carries across prompts, so it has to be findable (BUG-153).
        (
            "/clear",
            "Drop this session's retained conversation; the next prompt starts fresh.",
        ),
        (
            "/verbose",
            "Toggle the routing and turn-end notices for this session.",
        ),
        (
            "/effort",
            "Show or set the global reasoning effort: /effort [low|medium|high|xhigh|max].",
        ),
        (
            "/permissions",
            "Show or set this session's permission level: /permissions [level].",
        ),
        // REQ-572's enablement walkthrough, and REQ-563's two web controls. A
        // command a user cannot find in `/help` is a command they do not have
        // (BUG-153), and `/web allow` is the only way out of a taint
        // restriction — so its absence here would be a dead end, not just a
        // discoverability gap.
        (
            "/web setup",
            "Set up web lookup: pick a tier, name a backend, confirm before anything is written.",
        ),
        (
            "/web allow",
            "Lift this session's web taint restriction (grants no new tier).",
        ),
        (
            "/web refresh",
            "Drop a URL's cached copy so the next lookup re-fetches: /web refresh <url>.",
        ),
        // REQ-579 AC-14: `/help` lists `/provider setup` from the same table
        // that dispatches it. The unit tests pin that the row is generated
        // rather than hand-written; this pins that the **shipped binary** prints
        // it, which is the half a user meets.
        (
            "/provider setup",
            "Register a provider and route a tier to it: /provider setup [vendor] [tier] — \
             confirm before anything is written.",
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
    let daemon = daemon_bin();
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
            .matches("estimated savings vs anthropic/claude-fable-5")
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
    let daemon = daemon_bin();
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

/// The opening of the one line a clear draws (`session_ui::format_context_cleared`).
const CLEAR_MARKER: &str = "context cleared;";

/// How many blocks each `context_cleared` notice in `output` reported, in order.
///
/// Parsed from the rendered line rather than from a debug hook, because the
/// rendered line is what this suite is about: a clear the user cannot read the
/// result of is a clear they have to take on faith. The zero case has its own
/// sentence ("there was nothing retained to drop"), which is why it is matched
/// as a word and not as a number.
fn clear_counts(output: &str) -> Vec<u64> {
    output
        .lines()
        .filter_map(|line| line.split_once(CLEAR_MARKER))
        .map(|(_, tail)| {
            let tail = tail.trim();
            if tail.starts_with("there was nothing") {
                return 0;
            }
            tail.split_whitespace()
                .next()
                .and_then(|n| n.parse::<u64>().ok())
                .unwrap_or_else(|| panic!("a clear notice named no count: `{tail}`"))
        })
        .collect()
}

/// The session-ready banner with its minted id replaced by a fixed sentinel, so
/// two runs' transcripts can be compared byte for byte (REQ-569 BR-8).
///
/// Session ids stopped being `sess-0` — every daemon mints 128 random bits — so
/// two runs against two freshly spawned daemons *always* differ in the banner
/// line. That is the only difference by design, and normalizing it is what lets
/// the caller keep asserting whole-output equality rather than retreating to a
/// comparison that would stop noticing an extra line.
///
/// It panics when the banner is absent instead of returning the transcript
/// unchanged. A mask that quietly did nothing would make two runs that *both*
/// stopped printing the session line compare equal — the vacuous pass is the
/// failure mode a normalizing helper invites, so the non-vacuity is checked here
/// rather than left to each caller to remember.
fn mask_session_id(transcript: &str, what: &str) -> String {
    const BANNER: &str = "session sess-";
    const SENTINEL: &str = "sess-<minted>";

    let mut masked = String::with_capacity(transcript.len());
    let mut rest = transcript;
    let mut hits = 0;
    while let Some(at) = rest.find(BANNER) {
        let (head, tail) = rest.split_at(at + "session ".len());
        masked.push_str(head);
        masked.push_str(SENTINEL);
        // The id runs to the next space — `session <id> ready (freeform).`
        rest = &tail[tail.find(' ').unwrap_or(tail.len())..];
        hits += 1;
    }
    masked.push_str(rest);

    assert!(
        hits > 0,
        "{what}: no session-ready banner to mask — the transcript is not the \
         session output this compares:\n{transcript}"
    );
    masked
}

/// **REQ-567 AC-6, the UX half: `/clear` spends no model call, and says so once.**
///
/// The command is dispatched before a prompt is ever built (BR-1), so the
/// session-wide evidence that no turn ran is the same absence
/// [`assert_no_turn_ran`] checks for every other command — and it is checked on
/// a session whose *only* input is `/clear`, so nothing else could have produced
/// a reply to hide behind.
///
/// The count is asserted as **exactly one line**, which is the whole render
/// decision (TASK-095): a clear publishes `context_cleared` to every attached
/// client *and* answers the issuing client's RPC with the same number, and only
/// the event is rendered. A handler that also printed its answer would leave two
/// lines here saying one thing, and the person who typed the command would be the
/// only one who saw the duplicate.
///
/// A fresh session has nothing to drop, and that is deliberately the case under
/// test here: `0` is a real answer, not a degenerate one, and the sentence it
/// gets is the one that reads as an answer to a command rather than as an
/// arithmetic result.
#[test]
fn slash_clear_runs_no_turn_and_says_when_there_was_nothing_to_drop() {
    let daemon = daemon_bin();
    let daemon = TestDaemon::spawn_scripted(&daemon, TURN_REPLIES);
    let teton = teton_bin();

    let session = daemon.run_cli_with_stdin(&teton, &[], "/clear\n");

    assert_eq!(
        clear_counts(&session),
        vec![0],
        "one clear must draw exactly one notice, naming what it dropped; output:\n{session}\n\
         daemon log:\n{}",
        daemon.log()
    );
    assert!(
        session.contains("context cleared; there was nothing retained to drop."),
        "a clear with nothing to drop must read as an answer, not as `0 blocks`; \
         output:\n{session}"
    );
    // Wider than the count above, and deliberately so: a second rendering that
    // did not reuse the event's wording would slip past a marker-shaped count.
    // One clear, one line that talks about clearing or dropping at all — a
    // second line saying the same thing in its own words fails here even though
    // it carries no marker. (Checked by mutation, 2026-08-10: a handler that
    // also rendered its RPC answer as "dropped N blocks." passes the marker
    // count and dies on this.)
    //
    // Scanned from the session-ready line onward, which is where the entry loop
    // starts and therefore the only region a command can render into: the
    // startup chatter above it legitimately says "clears" about this machine's
    // RAM floor, and a guard that counted that would be about the banner.
    let entry_loop = session
        .split_once("ready (freeform)")
        .map(|(_, tail)| tail)
        .unwrap_or_else(|| panic!("the session never became ready; output:\n{session}"));
    let about_the_clear: Vec<&str> = entry_loop
        .lines()
        .filter(|line| {
            let line = line.to_ascii_lowercase();
            line.contains("clear") || line.contains("drop")
        })
        .collect();
    assert_eq!(
        about_the_clear.len(),
        1,
        "the issuing client drew a second line about the clear — the RPC answer and the \
         broadcast event both reached it and both rendered: {about_the_clear:?}\noutput:\n{session}"
    );
    assert_no_turn_ran(&session, "`/clear`");
}

/// **REQ-567 AC-6's flow, through the user's own surface.**
///
/// Prompt, `/clear`, prompt about the first exchange, `/clear` again. The second
/// clear's count is the assertion: it can only cover the second exchange if the
/// first clear really emptied the conversation, and it would be roughly double
/// if the daemon had gone on carrying the first exchange past a clear that told
/// the user it was gone. That is the worst available failure — the user was
/// already told it had worked — so it is worth pinning from outside the daemon.
///
/// **Why the count and not the context.** The claim AC-6 states about the
/// *assembled context* ("the next prompt's assembled context contains no prior
/// conversation") cannot be made from here: the scripted daemon answers from a
/// file and never reports what prompt it was handed, so a second prompt's reply
/// is the same marker whether or not the first exchange came with it. That leg
/// lives where the evidence is — `runtime.rs`'s `conversation_carry` module
/// drives `run_prompt_turn` against a recording engine and asserts on the
/// context that engine received, including the prompt-clear-prompt flow this
/// test drives (TASK-094's
/// `a_clear_empties_the_conversation_and_announces_what_it_dropped`). What is
/// only assertable *here* is that the real `/clear` command, typed into a real
/// session, reaches that path at all — and the second count is the externally
/// visible consequence of it having done so.
#[test]
fn slash_clear_drops_the_conversation_the_next_prompt_would_have_carried() {
    let daemon = daemon_bin();
    let daemon = TestDaemon::spawn_scripted(&daemon, TURN_REPLIES);
    let teton = teton_bin();

    let session = daemon.run_cli_with_stdin(
        &teton,
        &[],
        "how many attempts does the router allow?\n\
         /clear\n\
         what did we just establish?\n\
         /clear\n",
    );

    // Non-vacuity: both turns really ran, so both clears had an exchange to
    // drop. Without this the counts below could both be zero and agree.
    for (nth, reply) in TURN_REPLIES.iter().take(2).enumerate() {
        assert!(
            session.contains(reply),
            "turn {} never ran, so the clear counts below are about nothing; output:\n{session}",
            nth + 1
        );
    }

    let counts = clear_counts(&session);
    assert_eq!(
        counts.len(),
        2,
        "two clears, two notices — no more and no fewer; output:\n{session}"
    );
    assert!(
        counts[0] > 0,
        "the first clear dropped nothing, so the turn before it retained nothing \
         and this test is about neither; output:\n{session}\ndaemon log:\n{}",
        daemon.log()
    );
    assert_eq!(
        counts[1], counts[0],
        "the second clear dropped {} blocks where the first dropped {} — the first clear \
         reported success and left the conversation behind, so the prompt after it carried \
         history the user was told had gone; output:\n{session}",
        counts[1], counts[0]
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
///
/// One difference between two runs is intended and permanent: session ids are
/// 128 random bits (REQ-569 BR-8), so three freshly spawned daemons name three
/// different sessions in their ready banners. [`mask_session_id`] normalizes
/// exactly that, which keeps the assertion a **whole-output** equality — the
/// thing that would catch `/quit` printing an extra line or skipping the
/// summary — instead of demoting it to the weaker suffix comparison the
/// paragraph above holds in reserve.
#[test]
fn slash_quit_ends_the_session_exactly_as_ctrl_d_does() {
    let daemon_bin = daemon_bin();
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

    // The three runs' session ids are three independent 128-bit values, and
    // that is the one difference between the transcripts that is not evidence
    // of anything. Masked here, once, before any comparison below.
    let (quit, exit, eof) = (
        mask_session_id(&quit, "/quit"),
        mask_session_id(&exit, "/exit"),
        mask_session_id(&eof, "ctrl-d"),
    );

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
    let daemon = daemon_bin();
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
    let daemon = daemon_bin();
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
    let daemon = daemon_bin();
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
    let daemon = daemon_bin();
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
    let daemon = daemon_bin();
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
    let daemon = daemon_bin();
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
    let daemon = daemon_bin();
    let daemon = TestDaemon::spawn(&daemon);
    let teton = teton_bin();

    // The daemon's fixture config declares `deepseek` in the pre-REQ shape, so
    // the load-time migration resolves it through the legacy price lookup.
    let listed = daemon.run_cli(&teton, &["provider", "list"]);
    assert!(
        listed.contains("deepseek-v4-pro"),
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
    let daemon = daemon_bin();
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
            "deepseek-v4-flash",
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
        listed.contains("deepseek-v4-pro"),
        "the existing provider must keep its model; output:\n{listed}"
    );
    assert!(
        !listed.contains("deepseek-v4-flash"),
        "the refused registration must not have landed; output:\n{listed}"
    );
}

// ---------------------------------------------------------------------------
// REQ-578 — a pasted base URL becomes the request URL, and the CLI says so
// ---------------------------------------------------------------------------
//
// AC-1..AC-4 are claims about the **shipped binary's** registration flow: what
// it decides to store when a user pastes what their vendor documents, and when
// it says so. Each runs the real `teton` against a real daemon, with the
// endpoint decision reached through the real argv and the real
// `settle_endpoint` — the seam a unit test can only drive directly.
//
// **Every run here stops one step short of the keychain, and that is a
// constraint rather than a preference.** The shipped CLI writes credentials to
// the real OS keychain (`keychain::default_keychain`) with no test seam in
// front of it — the same rule the `/web setup` section below states, and the
// reason none of these tests supplies a key. What that costs is the last hop:
// stdin is closed, so `read_secret` gets EOF, the command exits non-zero
// naming the missing key, and no registration reaches `config/set`. What it
// does not cost is any part of AC-1..AC-4, because the whole endpoint decision
// — compose, echo, refuse — happens *before* the credential is read. That
// ordering is BR-5, and the fact that these tests can make their assertions at
// all with the key step unreached is itself evidence for it.
//
// The other half — that a composed endpoint is a registration the daemon's own
// `config/set` accepts and persists byte-identically (BR-8) — is
// `tetond/tests/composed_endpoint_registration.rs`, which drives the RPC
// directly and needs no credential at all.

/// **AC-1: the base URL Moonshot documents is stored as the URL Teton POSTs,
/// and the user is told the full form before a key is asked for.**
///
/// The command a user actually types, run as they type it. `https://api.moonshot.ai/v1`
/// is what Moonshot's quickstart hands an OpenAI-compatible SDK, and before
/// this REQ it registered cleanly and 404'd on the first turn — one step
/// removed from its cause (BUG-170, LESSON-523).
#[test]
fn provider_add_composes_a_pasted_base_url_and_says_so_before_asking_for_a_key() {
    let daemon = daemon_bin();
    let daemon = TestDaemon::spawn(&daemon);
    let teton = teton_bin();

    // **Stdout alone**, not the combined capture. An ordering claim compared
    // across two concatenated streams is not an ordering claim: everything on
    // stdout precedes everything on stderr in that string no matter what order
    // the user saw, so a prompt that moved to stderr would keep passing. Both
    // lines under test are stdout's (the surface writes there, and so does the
    // hiding prompter), which is exactly why the comparison must be made there.
    let (output, stderr, status) = daemon.run_cli_streams(
        &teton,
        &[
            "provider",
            "add",
            "kimi",
            "--kind",
            "openai-compatible",
            "--endpoint",
            "https://api.moonshot.ai/v1",
            "--model",
            "kimi-k3",
        ],
        "",
        CliSeams::Off,
    );

    let composed = output
        .find("https://api.moonshot.ai/v1/chat/completions")
        .unwrap_or_else(|| {
            panic!(
                "AC-1: the base URL must be completed to the request URL and echoed in full; \
                 stdout:\n{output}\nstderr:\n{stderr}"
            )
        });
    assert!(
        output.contains("endpoint stored as"),
        "AC-1/BR-4: the echo must say the URL is what was *stored*, not merely mention it; \
         stdout:\n{output}"
    );

    // BR-5, end to end: the credential prompt is downstream of the decision.
    let asked = output.find("API key for").unwrap_or_else(|| {
        panic!(
            "the flow must reach the credential step (and fail there on a closed stdin), or \
             this test is asserting about a command that stopped for some other reason; \
             stdout:\n{output}\nstderr:\n{stderr}"
        )
    });
    assert!(
        composed < asked,
        "BR-5: the stored endpoint must be on screen BEFORE the key is asked for — a user \
         decides whether to type a credential by reading what will be called; stdout:\n{output}"
    );

    // The key step is where this run ends, so nothing was registered and no
    // keychain entry was created (see the section header).
    assert!(
        !status.success(),
        "with no key on a closed stdin the command must fail; output:\n{output}"
    );
    let listed = daemon.run_cli(&teton, &["provider", "list"]);
    assert!(
        !listed.contains("kimi"),
        "a registration that never supplied a key must not appear; output:\n{listed}"
    );
}

/// **AC-2: the full request URL is stored byte-identically, with no echo.**
///
/// Idempotence for everyone who followed the documentation (BR-7). The
/// documented form is still the canonical one — composition is forgiveness, not
/// the new convention — so the command that was correct before this REQ must
/// read *exactly* as it did before, silence included. An echo here would tell a
/// user who did it right that something was changed.
#[test]
fn provider_add_stores_a_full_request_url_without_a_word_about_it() {
    let daemon = daemon_bin();
    let daemon = TestDaemon::spawn(&daemon);
    let teton = teton_bin();

    let (output, _status) = daemon.run_cli_capture(
        &teton,
        &[
            "provider",
            "add",
            "kimi",
            "--kind",
            "openai-compatible",
            "--endpoint",
            "https://api.moonshot.ai/v1/chat/completions",
            "--model",
            "kimi-k3",
        ],
        "",
    );

    assert!(
        !output.contains("endpoint stored as"),
        "AC-2: an endpoint stored exactly as typed must produce no echo at all; output:\n{output}"
    );
    // Non-vacuity: the flow really did run the endpoint step, it just had
    // nothing to say — it reached the credential prompt beyond it.
    assert!(
        output.contains("API key for"),
        "the command must have reached the credential step, or the silence above is the \
         silence of a command that never got this far; output:\n{output}"
    );
}

/// **AC-3: `--kind anthropic` with no `--endpoint` registers the official
/// Messages URL, echoes it, and does so before the key prompt.**
///
/// The BUG-170 sequence, inverted. Before this REQ the missing endpoint was
/// refused by the daemon's validator — *after* the user's key had been read and
/// stored — and there was no way to spell the command that worked without
/// knowing a URL the product never printed. Now the default is written
/// explicitly into config (BR-3) and shown (BR-4), so the address is the user's
/// to check rather than a runtime secret.
#[test]
fn provider_add_anthropic_defaults_its_endpoint_and_shows_it_first() {
    let daemon = daemon_bin();
    let daemon = TestDaemon::spawn(&daemon);
    let teton = teton_bin();

    // Stdout alone — see AC-1 above for why an ordering claim may not be made
    // over the concatenation of two streams.
    let (output, stderr, _status) = daemon.run_cli_streams(
        &teton,
        &[
            "provider",
            "add",
            "claude",
            "--kind",
            "anthropic",
            "--model",
            "claude-opus-5",
        ],
        "",
        CliSeams::Off,
    );

    let defaulted = output
        .find("https://api.anthropic.com/v1/messages")
        .unwrap_or_else(|| {
            panic!(
                "AC-3: the Anthropic default must be echoed in full; stdout:\n{output}\n\
                 stderr:\n{stderr}"
            )
        });
    assert!(
        output.contains("endpoint stored as"),
        "AC-3/BR-3: the default is *stored*, not applied at call time, and the echo is how a \
         user learns that; stdout:\n{output}"
    );
    let asked = output
        .find("API key for")
        .unwrap_or_else(|| panic!("the flow must reach the credential step; stdout:\n{output}"));
    assert!(
        defaulted < asked,
        "AC-3: the endpoint must be determined and shown before the key prompt; stdout:\n{output}"
    );
}

/// **AC-4: an explicit gateway path is stored verbatim, with no composition and
/// no warning.**
///
/// The class this rule is most easily got wrong. A self-hosted gateway serves
/// chat completions wherever its operator put them, so `/llm/proxy` is a
/// deliberate address and not a mistake to correct — a normalizer that appended
/// `/chat/completions` here would break the deployments Teton exists to
/// support, and one that merely *warned* would teach its users to ignore it.
#[test]
fn provider_add_stores_a_custom_gateway_path_verbatim_and_silently() {
    let daemon = daemon_bin();
    let daemon = TestDaemon::spawn(&daemon);
    let teton = teton_bin();

    for kind in ["openai-compatible", "anthropic"] {
        let (output, _status) = daemon.run_cli_capture(
            &teton,
            &[
                "provider",
                "add",
                "gw",
                "--kind",
                kind,
                "--endpoint",
                "https://gw.example.com/llm/proxy",
                "--model",
                "gateway-model",
            ],
            "",
        );

        assert!(
            !output.contains("endpoint stored as"),
            "AC-4 ({kind}): an explicit path must be stored as typed, with nothing said about \
             it; output:\n{output}"
        );
        assert!(
            !output.contains("/llm/proxy/chat/completions") && !output.contains("/llm/proxy/v1"),
            "AC-4 ({kind}): nothing may be appended to a custom path; output:\n{output}"
        );
        assert!(
            output.contains("API key for"),
            "({kind}) the command must have reached the credential step; output:\n{output}"
        );
    }
}

/// **AC-5: `teton doctor` flags a hand-edited base-URL endpoint with the exact
/// full form, does not flag a custom path, and its exit status is unchanged.**
///
/// The advisory exists for the config `provider add` can no longer produce: one
/// somebody wrote by hand, or wrote before the composition existed. Since
/// REQ-577 the stored endpoint is POSTed verbatim, so such a config is valid,
/// starts the daemon, lists cleanly — and 404s on the first turn with nothing
/// naming the cause. Doctor is where a user goes with exactly that symptom.
///
/// All three claims are load-bearing together. Flagging without the full form
/// would leave the user where they were; flagging the gateway would make the
/// notice noise; and failing the exit status would turn a valid config into a
/// broken-looking one and change what every script wrapping `teton doctor`
/// sees (BR-6: no new fatal class).
#[test]
fn doctor_flags_a_hand_edited_base_url_endpoint_and_stays_green() {
    let daemon_path = daemon_bin();
    // Hand-written, exactly as a user following a vendor quickstart would write
    // it: `kimi` carries the bare `/v1` base URL, `gw` a deliberate gateway
    // path. Neither could have been produced by `provider add` after this REQ.
    let daemon = TestDaemon::spawn_with_config(
        &daemon_path,
        "[[providers]]\nid = \"kimi\"\nkind = \"openai-compatible\"\n\
         endpoint = \"https://api.moonshot.ai/v1\"\nmodel = \"kimi-k3\"\n\n\
         [[providers]]\nid = \"gw\"\nkind = \"openai-compatible\"\n\
         endpoint = \"https://gw.example.com/llm/proxy\"\nmodel = \"gateway-model\"\n\n",
    );
    let teton = teton_bin();

    let (output, status) = daemon.run_cli_capture(&teton, &["doctor"], "");

    assert!(
        status.success(),
        "AC-5: the advisory must not change doctor's exit status; output:\n{output}\n\
         daemon log:\n{}",
        daemon.log()
    );
    // The advisory lines, picked out of doctor's report by the phrase that
    // makes one an advisory. Per line, because both providers are *listed*
    // either way: what is at issue is which of them is advised about.
    let advisories: Vec<&str> = output
        .lines()
        .filter(|line| line.contains("looks like a vendor base URL"))
        .collect();

    assert!(
        advisories.iter().any(|line| line.contains("`kimi`")
            && line.contains("https://api.moonshot.ai/v1/chat/completions")),
        "AC-5: the hand-edited base URL must be flagged, by name, with the exact full form to \
         use — an advisory that only says something is off leaves the user where they were; \
         advisories:\n{advisories:#?}\nfull output:\n{output}"
    );
    assert!(
        !advisories.iter().any(|line| line.contains("`gw`")),
        "AC-5: a custom gateway path must not be advised on — it is a first-class deployment, \
         and an advisory that fires on it is noise a user learns to skip; \
         advisories:\n{advisories:#?}"
    );
    assert!(
        output.contains("https://gw.example.com/llm/proxy")
            && !output.contains("https://gw.example.com/llm/proxy/chat"),
        "the gateway must still be listed, and listed as it was written; output:\n{output}"
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
    let daemon = daemon_bin();
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
    // **The marked set is now empty.** `redact` was the last one standing —
    // REQ-561 left it unwired because a model call inside the egress choke point
    // needed its own spec, and REQ-562 TASK-070 wired it — so this end of the
    // assertion is now "nothing is marked", derived from the same place.
    //
    // That does cost this test something, and it is worth naming: with no
    // unreached category left, a renderer that had simply forgotten how to print
    // the marker would satisfy every `!contains("no call site")` here. That half
    // is not dropped, it is re-homed to the layer where an unreached row can
    // still be *constructed* —
    // `main::tests::policy_show_marks_the_unreached_categories_and_the_judgment_default`
    // renders a synthetic snapshot with `reached: false` and asserts the marker
    // appears. A twelfth category that arrives unwired brings the e2e half back.
    let redact_row = row(&shown, "redact").expect("a `redact` row");
    for line in shown.lines() {
        assert!(
            !line.contains("no call site"),
            "every declared category is dispatched on, so no row may carry the \
             `declared, no call site yet` marker; row:\n{line}"
        );
    }
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

    // AC-16's other half, now on the other side of the line REQ-562 crossed.
    // `redact` used to transmit nothing, and its row read "would send outbound
    // payloads; declared, no call site yet" — the conditional verb and the
    // marker in one phrase, so a class printed alone could not read as a live
    // egress path. TASK-070 gave it a call site, so the marker is gone (asserted
    // for every row above).
    //
    // **The verb stays conditional on this daemon, and that is the assertion
    // rather than a regression** (REQ-562 report honesty; user decision,
    // 2026-08-08). `[privacy] redact` is off by default (BR-10/OQ-3) and this
    // fixture never sets it, so no gate is installed, nothing is scanned, and
    // the present-tense "sends outbound payloads" this test used to require was
    // a claim about work the daemon was not doing — the exact untruth AC-13
    // forbids on the other surfaces. The row is now conditional *and* says
    // which state it is in, so the two readings a user could otherwise not tell
    // apart — "the switch is off" and "the binding is missing" — are distinct.
    //
    // The enabled leg is unit-covered rather than added here: flipping the
    // switch means a second daemon boot with a different config, and it would
    // put a real scan in front of every remote call in this suite. Both states
    // render from one fixture in
    // `main::tests::policy_show_reports_whether_the_redaction_scan_runs`.
    assert!(
        redact_row.contains("would send outbound payloads"),
        "with the scan off, `redact`'s row must not claim the present tense; \
         row:\n{redact_row}"
    );
    assert!(
        redact_row
            .contains("content scan: disabled (default — enable with `[privacy] redact = true`)"),
        "the row must say the scan is off and name the key that turns it on; \
         row:\n{redact_row}"
    );

    // AC-12: the BR-9 declared default is configuration-visible, and the CLI
    // says so rather than leaving it compiled in silently.
    assert!(
        shown.contains("judgment_default"),
        "the declared default must be reported; output:\n{shown}"
    );
}

// ---------------------------------------------------------------------------
// REQ-563 — the client surfaces of opt-in web lookup (TASK-078)
// ---------------------------------------------------------------------------
//
// AC → test map for this section:
//
//   AC-10 (`/web refresh`) + AC-12 (`/web allow`)
//       → `the_two_web_commands_reach_the_daemon_and_render_its_answer`
//   AC-2 (the consent prompt at Ask) + AC-6 (`/cost` counts every lookup) +
//   AC-8 (offline is a notice, the turn completes)
//       → `a_web_lookup_is_consented_reported_and_counted_in_the_cost_report`
//
// AC-6's `/help` half is asserted in `slash_help_lists_every_command_and_no_turn_is_attempted`
// above, which lists both `/web` rows with their summaries.
//
// The status **row** (`web: fetch`) is not assertable from this file: it is
// drawn by `main::paint_status` above the framed entry prompt, which exists only
// at a TTY (REQ-556 BR-2 keeps a piped run byte-identical to what it was). Its
// pty leg lives in `pty_e2e.rs`.

/// AC-12 and AC-10's client half: both `/web` commands are real round trips to
/// the daemon, and each renders the daemon's own answer.
///
/// They are user-only actions on purpose — the model reaches the tool registry,
/// which has no tool by either name (asserted in `tetond`'s
/// `web_consent_matrix`) — so the client command *is* the surface, and a command
/// that silently did nothing would look exactly like one that worked.
#[test]
fn the_two_web_commands_reach_the_daemon_and_render_its_answer() {
    let daemon = daemon_bin();
    let daemon = TestDaemon::spawn_scripted(&daemon, TURN_REPLIES);
    let teton = teton_bin();

    let session = daemon.run_cli_with_stdin(
        &teton,
        &[],
        "/web allow\n/web refresh https://docs.rs/tokio/latest/tokio/\n/web refresh\n",
    );

    // `/web allow` on a session that never read boundary content: the honest
    // answer is that nothing was restricted, not a false confirmation that
    // something was lifted (BR-13).
    assert!(
        session.contains("this session has not read privacy-boundary content"),
        "`/web allow` must render the daemon's `was_restricted: false` answer; \
         output:\n{session}"
    );

    // `/web refresh <url>` on a URL with nothing stored: a fact, not a failure.
    assert!(
        session.contains("web cache: nothing was stored for that URL"),
        "`/web refresh` must distinguish absent from evicted; output:\n{session}"
    );

    // A bare `/web refresh` is an argument error the dispatch table catches
    // before any RPC — it names the usage rather than guessing a URL.
    assert!(
        session.contains("/web refresh <url>"),
        "a bare `/web refresh` must name its usage; output:\n{session}"
    );

    // Neither command spent a model call (BR-7's "commands are not prompts").
    assert_no_turn_ran(&session, "the two /web commands");
}

/// AC-2 + AC-6 + AC-8, end to end through the shipped binaries: a scripted turn
/// asks for a page, the user is asked in concrete terms, the destination is
/// unreachable, the turn finishes anyway, and `/cost` counts the lookup.
///
/// Hermetic: the fetch target is a loopback port nothing listens on, so the
/// lookup ends `offline` without touching a network. That is the whole point of
/// choosing it — an offline ending is a *lookup that happened*, so it exercises
/// consent, the choke point, the ledger row and every rendering surface, while
/// reaching nothing.
///
/// **The user pastes the URL, and that is load-bearing.** The seam's SSRF floor
/// refuses a loopback destination the *model* composed, which is correct and is
/// tested where it lives; a URL the user typed is exempt, because a person
/// pointing this daemon at `127.0.0.1` is pointing it at their own machine on
/// purpose. Without the paste this fixture would never reach the wire and would
/// be measuring the address gate rather than the offline ending it is named for.
#[test]
fn a_web_lookup_is_consented_reported_and_counted_in_the_cost_report() {
    let daemon = daemon_bin();
    // A port nobody is listening on: the fetch connects to nothing.
    let url = format!("http://127.0.0.1:{}/tokio", closed_port());
    let tool_call = format!("{{\"tool\": \"web\", \"arguments\": {{\"url\": \"{url}\"}}}}");
    let daemon = TestDaemon::spawn_scripted_with_config(
        &daemon,
        &[
            &tool_call,
            "I could not reach that page, so here is what I know.",
        ],
        "[web]\ntier = \"fetch_any_url\"\n",
    );
    let teton = teton_bin();

    // Line 1 is the prompt — carrying the URL verbatim, which is what makes the
    // fetch user-authored; line 2 answers the permission question the lookup
    // raises; line 3 asks for the cost report the lookup should be in.
    let session = daemon.run_cli_with_stdin(
        &teton,
        &[],
        &format!("what does {url} say about task pinning?\ny\n/cost\n"),
    );
    let log = daemon.log();

    // AC-2 / BR-4: the question was concrete — the verbatim URL and the host —
    // and it offered the persistent choice, which only the web keys get.
    assert!(
        session.contains("permission requested: web_fetch_user_url"),
        "the lookup must be authorized under its per-tier key — and this URL was \
         pasted by the user, so it is the user-URL tier's key and not the \
         any-URL one's; output:\n{session}\nlog:\n{log}"
    );
    assert!(
        session.contains(&url),
        "the prompt must show the verbatim URL; output:\n{session}"
    );
    assert!(
        session.contains("host 127.0.0.1"),
        "the prompt must name the destination host; output:\n{session}"
    );
    assert!(
        session.contains("[p]ermanently"),
        "a web prompt offers enabling permanently (BR-4); output:\n{session}"
    );

    // AC-8 / BR-9: an unreachable destination is a transient-shaped notice, and
    // the turn finished — the scripted engine's closing reply is on screen.
    assert!(
        session.contains("web fetch 127.0.0.1 — unavailable: offline"),
        "an unreachable host must render as the offline notice; output:\n{session}\nlog:\n{log}"
    );
    assert!(
        session.contains("I could not reach that page"),
        "the turn must complete despite the failed lookup; output:\n{session}\nlog:\n{log}"
    );
    assert!(
        !session.contains("prompt failed"),
        "a lookup failure is never a turn error (BR-9); output:\n{session}"
    );

    // AC-6 / BR-7: every lookup lands in the ledger, including the free ones,
    // and `/cost` shows it. The section is silent when empty, so its presence is
    // itself the assertion.
    let report = cost_report_from_first_marker(&session, "`/cost` after a web lookup");
    assert!(
        report.contains("web lookups:"),
        "`/cost` must render the web-lookup roll-up; report:\n{report}"
    );
    assert!(
        report.contains("1 lookup(s)"),
        "one lookup was attempted, so one must be counted; report:\n{report}"
    );

    // Non-vacuity for the section's presence: a session that performed no
    // lookup renders no such section at all (the default state, BR-1).
    let quiet_daemon = TestDaemon::spawn_scripted(&daemon_bin(), TURN_REPLIES);
    let quiet = quiet_daemon.run_cli_with_stdin(&teton, &[], "/cost\n");
    assert!(
        !quiet.contains("web lookups:"),
        "a machine that never looked anything up must not grow a web section; \
         output:\n{quiet}"
    );
}

// ---------------------------------------------------------------------------
// REQ-560 — named permission levels, over a pipe
//
// Everything below is **added**; not one existing test above is touched. That
// is AC-8's claim and it is meant to be checked by diffing this file: the status
// row is TTY-gated (BR-9), so a piped session's bytes are what they always were,
// and a test edited to accommodate status-line bytes would be a violation rather
// than an accommodation.
//
// The prompt round-trip is what makes these tests possible on a pipe: a
// permission question is asked through the `Prompter` seam, which on piped stdin
// reads the next line. So a scripted stdin can answer one.
// ---------------------------------------------------------------------------

/// A scripted reply that asks to run a shell command that **succeeds**.
///
/// Success matters: a tool renders `[done]` when it ran and `[failed]` when it
/// did not, so a command that cannot fail on its own makes the status line a
/// clean read of the *permission* decision rather than of the tool's luck. An
/// `edit` was the obvious choice and is the wrong one — a denied edit and an
/// edit whose `old_string` was not in the file both render `[failed]`, so the
/// evidence would not distinguish "the level refused" from "the fixture was
/// wrong".
const SHELL_CALL: &str = r#"{"tool": "shell", "arguments": {"command": "true"}}"#;

/// The same shape under the `edit` name, for the legs that need the tool `edits`
/// treats differently from `shell`.
///
/// Only ever used for **whether a prompt happened**, never for the tool's
/// outcome. A tool jail is the *session's* cwd (BUG-147), which for a CLI
/// spawned by this harness is the test runner's directory rather than the
/// fixture root — so a file tool here always fails, and a `[failed]` beside an
/// `edit` says nothing about permissions. `shell: true` carries every outcome
/// claim instead, for the reason above it.
const EDIT_CALL: &str = r#"{"tool": "edit", "arguments": {"path": "notes.txt", "old_string": "alpha", "new_string": "beta"}}"#;

/// AC-2: the three legs, in one session — `guarded` asks about an edit;
/// `edits` runs it unprompted and still asks about a shell; `plan` denies both.
///
/// Two independent signals, and both are needed. The **prompt count** says
/// whether the level asked; the **`shell: true` outcome** says whether the call
/// ran, and it is unambiguous because `true` cannot fail on its own — a
/// `[failed]` beside it is a refusal and nothing else. The scripted closing line
/// of each turn is the non-vacuity anchor: it proves the turn actually ran, so a
/// count that stayed at one cannot be a session that quietly stopped.
#[test]
fn permission_levels_change_what_a_session_asks_about() {
    let daemon = daemon_bin();
    let replies = [
        EDIT_CALL,
        "guarded edit turn done.",
        EDIT_CALL,
        "edits edit turn done.",
        SHELL_CALL,
        "edits shell turn done.",
        SHELL_CALL,
        "plan shell turn done.",
    ];
    let daemon = TestDaemon::spawn_scripted(&daemon, &replies);
    let teton = teton_bin();

    let session = daemon.run_cli_with_stdin(
        &teton,
        &[],
        "edit something\ny\n\
         /permissions edits\n\
         edit something else\n\
         run something\ny\n\
         /permissions plan\n\
         run again\n",
    );

    // Every turn ran — without this, the counts below could be satisfied by a
    // session that died after the first one.
    for marker in [
        "guarded edit turn done.",
        "edits edit turn done.",
        "edits shell turn done.",
        "plan shell turn done.",
    ] {
        assert!(
            session.contains(marker),
            "the turn ending `{marker}` never completed; output:\n{session}"
        );
    }

    // Leg 1 — guarded: an edit is a question.
    assert!(
        session.contains("permission requested: edit"),
        "at guarded an edit must ask; output:\n{session}"
    );

    // Leg 2 — edits: exactly one edit question in the whole session, so the
    // second edit ran without asking. A count is the assertion because the mere
    // presence of a prompt cannot distinguish "asked twice" from "asked once".
    assert!(
        session.contains("permission level: edits"),
        "the level change must be confirmed; output:\n{session}"
    );
    assert_eq!(
        session.matches("permission requested: edit").count(),
        1,
        "only the guarded edit should have asked; output:\n{session}"
    );

    // Leg 2b — a shell still asks at `edits`, which is the whole reason this
    // level exists separately from `full`; allowed, it ran.
    assert!(
        session.contains("permission requested: shell"),
        "at edits a shell must still ask; output:\n{session}"
    );
    assert!(
        session.contains("shell: true [done]"),
        "the allowed shell must have run; output:\n{session}"
    );

    // Leg 3 — plan: refused, and refused **without asking**. The shell count is
    // unchanged from leg 2b and `true` failed, which together can only mean the
    // level decided it rather than the user.
    assert!(
        session.contains("permission level: plan"),
        "the level change must be confirmed; output:\n{session}"
    );
    assert_eq!(
        session.matches("permission requested: shell").count(),
        1,
        "plan must deny without asking; output:\n{session}"
    );
    assert!(
        session.contains("shell: true [failed]"),
        "`true` cannot fail on its own, so plan must have refused it; \
         output:\n{session}"
    );
}

/// AC-3 / BR-5: a grant is an answer to a question the level decides whether to
/// ask, so a tightened level outranks it — and loosening restores it, because
/// the grant was never discarded.
///
/// Three `shell` calls, one prompt. The middle one is refused by `plan` and the
/// third runs on the grant made before it, which is the whole claim: `[done]`,
/// `[failed]`, `[done]` with a single question at the front.
#[test]
fn a_tightened_level_outranks_a_session_grant_over_a_pipe() {
    let daemon = daemon_bin();
    let replies = [
        SHELL_CALL,
        "granted.",
        SHELL_CALL,
        "denied by plan.",
        SHELL_CALL,
        "grant applies again.",
    ];
    let daemon = TestDaemon::spawn_scripted(&daemon, &replies);
    let teton = teton_bin();

    let session = daemon.run_cli_with_stdin(
        &teton,
        &[],
        // `a` is allow-always at the prompt.
        "run once\na\n\
         /permissions plan\n\
         run again\n\
         /permissions guarded\n\
         run a third time\n",
    );

    // One question in the whole session: the first. The third call was answered
    // by the remembered grant and the second by the level — two different
    // reasons for silence, and neither is a prompt.
    assert_eq!(
        session.matches("permission requested: shell").count(),
        1,
        "the grant must be remembered and plan must not ask; output:\n{session}"
    );
    assert!(
        session.contains("permission level: plan") && session.contains("permission level: guarded"),
        "both level changes must be confirmed; output:\n{session}"
    );

    // The outcomes, in order: allowed, refused by the tightened level, allowed
    // again on the restored grant. Sequence is the assertion — a count alone
    // could not tell this from "denied, denied, denied".
    // Scanned over the whole transcript rather than line by line: a tool's
    // terminal status can share a line with the prompt that preceded it, so a
    // line filter would silently miss the first outcome — and a missing outcome
    // would make this assertion pass for the wrong reason.
    let mut outcomes: Vec<(usize, &str)> = Vec::new();
    for (at, _) in session.match_indices("shell: true [done]") {
        outcomes.push((at, "done"));
    }
    for (at, _) in session.match_indices("shell: true [failed]") {
        outcomes.push((at, "failed"));
    }
    outcomes.sort_unstable();
    let outcomes: Vec<&str> = outcomes.into_iter().map(|(_, what)| what).collect();
    assert_eq!(
        outcomes,
        vec!["done", "failed", "done"],
        "a tightened level must refuse the granted tool and loosening must restore \
         it; output:\n{session}"
    );
}

/// AC-9 / BR-10: bare `/permissions` prints the current level **on a pipe**.
///
/// This is the criterion that keeps the feature usable for the users BR-9 hides
/// the status row from. It is also AC-11's e2e half: `/help` lists the command
/// from the same table that dispatches it.
#[test]
fn bare_permissions_reads_the_level_on_a_pipe_and_help_lists_it() {
    let daemon = daemon_bin();
    let daemon = TestDaemon::spawn_scripted(&daemon, TURN_REPLIES);
    let teton = teton_bin();

    let session = daemon.run_cli_with_stdin(
        &teton,
        &[],
        "/permissions\n/effort\n/help\n/permissions full\n/permissions\n",
    );

    // The read, with no argument, on a pipe.
    assert!(
        session.contains("permission level: guarded"),
        "bare /permissions must print the level; output:\n{session}"
    );
    // AC-9's second half, now that REQ-559 has landed: the spec deferred this
    // leg with "when REQ-559 has landed, the same test covers bare `/effort`".
    // It has, so it does — bare `/effort` reads on a pipe too, which is the
    // point of BR-10: every value the TTY-only status row shows has a
    // non-visual read path.
    assert!(
        session.contains("Reasoning effort:"),
        "bare /effort must print the setting on a pipe (REQ-559 BR-9); output:\n{session}"
    );
    // AC-11: both rows listed in `/help`, from the dispatch table `/help` is
    // generated from — so neither can exist without appearing there.
    assert!(
        session.lines().any(|line| line.contains("/permissions")
            && line.contains("Show or set this session's permission level")),
        "/help must list /permissions with its summary; output:\n{session}"
    );
    assert!(
        session.lines().any(|line| line.contains("/effort")
            && line.contains("Show or set the global reasoning effort")),
        "/help must list /effort with its summary; output:\n{session}"
    );
    // A set, then a read that reflects it.
    assert!(
        session.contains("permission level: full"),
        "the set must be confirmed; output:\n{session}"
    );
    assert!(
        session.contains("permission level: full (unchanged)"),
        "the second read must report the level without claiming a change; output:\n{session}"
    );
    // BR-14: `/effort` appears **once** — one row, REQ-559's. Counted rather
    // than asserted absent, because absence stopped being the invariant the
    // moment that REQ landed; what BR-14 forbids is a second row.
    assert_eq!(
        session
            .lines()
            .filter(|line| line.contains("/effort") && line.contains("reasoning effort"))
            .count(),
        1,
        "/help must list /effort exactly once (BR-14); output:\n{session}"
    );
    assert_no_turn_ran(&session, "`/permissions`, `/effort` and `/help`");
}

/// AC-6 / BR-6: the level is session-scoped. A fresh session against the **same
/// daemon** starts at the configured default, and nothing was written to disk.
///
/// The full-restart leg is the daemon spawn itself: this test's second CLI run
/// is a new session, and the config file is asserted byte-identical, so a level
/// that had persisted through either route would fail here.
#[test]
fn a_permission_level_does_not_survive_the_session() {
    let daemon = daemon_bin();
    let daemon = TestDaemon::spawn_scripted(&daemon, TURN_REPLIES);
    let teton = teton_bin();
    let config_path = daemon.root.join("config.toml");
    let before = std::fs::read_to_string(&config_path).expect("the fixture config is readable");

    let first = daemon.run_cli_with_stdin(&teton, &[], "/permissions full\n");
    assert!(
        first.contains("permission level: full"),
        "the level must have been set in the first session; output:\n{first}"
    );

    let second = daemon.run_cli_with_stdin(&teton, &[], "/permissions\n");
    assert!(
        second.contains("permission level: guarded"),
        "a new session must start at the configured default, not inherit the last \
         one's level; output:\n{second}"
    );

    let after = std::fs::read_to_string(&config_path).expect("the fixture config is readable");
    assert_eq!(
        before, after,
        "a session-scoped level was written to the config file"
    );
}

/// AC-15 / BR-7, the piped half: a `/permissions` line typed while a permission
/// prompt is open is consumed as the **prompt's answer**, never dispatched as a
/// level change behind the user's back.
///
/// Scope, stated rather than implied. The CLI is a single reader of stdin by
/// construction (REQ-556 ADR-556-1): while a prompt is open, the prompter is
/// what reads the next line. So over a pipe a level change *cannot* be delivered
/// mid-prompt at all, and what this test pins is that discipline — the line goes
/// to the question that is waiting, and the session's posture is not quietly
/// changed by text the user typed while being asked something else.
///
/// The concurrent case BR-7 is really about — a level change arriving from a
/// **second attached client** while the first has a prompt open — cannot be
/// staged through one piped CLI. It is pinned at the gate instead, by
/// `a_level_change_leaves_an_in_flight_prompt_pending` in
/// `harness::permissions`, which drives the two concurrently and asserts against
/// `PendingPermissions` state. Naming that here so the split is a decision
/// rather than a gap someone later mistakes for full coverage.
#[test]
fn a_level_line_typed_at_an_open_prompt_answers_the_prompt_and_changes_nothing() {
    let daemon_path = daemon_bin();

    for arriving in ["full", "plan"] {
        let replies = [
            SHELL_CALL,
            "first turn done.",
            SHELL_CALL,
            "second turn done.",
        ];
        let daemon = TestDaemon::spawn_scripted(&daemon_path, &replies);
        let teton = teton_bin();

        let session = daemon.run_cli_with_stdin(
            &teton,
            &[],
            &format!("run something\n/permissions {arriving}\nrun again\n"),
        );

        // The prompt was asked, and asked of the user.
        assert!(
            session.contains("permission requested: shell"),
            "{arriving}: the shell call must have asked; output:\n{session}"
        );
        // The turn completed — without this the assertions below could be
        // satisfied by a session that died at the prompt.
        assert!(
            session.contains("first turn done."),
            "{arriving}: the first turn never finished; output:\n{session}"
        );
        // The load-bearing one: the line went to the prompt, so no level change
        // was dispatched. A confirmation here would mean the command ran while a
        // question was open — which is the shape BR-7 forbids.
        assert!(
            !session.contains("permission level: "),
            "{arriving}: a line typed while a prompt was open was dispatched as a \
             level change; it must be the prompt's answer instead; output:\n{session}"
        );
        // And the call was decided as a refusal, because `/permissions full` is
        // not a valid answer — `true` cannot fail on its own, so this is the
        // decline and not the command.
        assert!(
            session.contains("shell: true [failed]"),
            "{arriving}: an unrecognised answer must decline the call; \
             output:\n{session}"
        );
    }
}

// ---------------------------------------------------------------------------
// REQ-572 — the `/web setup` walkthrough, at the client (TASK-133)
// ---------------------------------------------------------------------------
//
// AC → test map for this section:
//
//   AC-10 (non-interactive degradation) + BR-12
//       → `a_piped_web_setup_prints_the_instructions_and_asks_nothing`
//   AC-3 (the walk, client half) + BR-7 (preview then confirm) +
//   BR-14 (the completion is announced, not merely returned)
//       → `the_walkthrough_collects_every_answer_and_the_daemon_announces_the_write`
//
// What this section deliberately does **not** hold:
//
//   * the **echo-off key step and the secret sweep (AC-5)** — a pipe cannot
//     observe echo, which is the whole of that claim. It is `pty_e2e.rs`'s.
//   * the daemon-side flow (AC-1/AC-3/AC-6/AC-7) — `tetond`'s
//     `web_setup_flow.rs` drives it against a spawned daemon that owns a config
//     file, which is where the write and the live pickup can be observed.
//   * AC-4's user-only gate — `tetond`'s `multi_client.rs` and `server.rs`.
//
// **Every walk here is keyless, and that is a constraint rather than a
// preference.** The shipped CLI writes credentials to the real OS keychain
// (`keychain::default_keychain`), and a test that entered a key would create —
// and on a failed commit delete — a `teton/web-search` entry in the developer's
// own login keychain, clobbering a real one if it existed. No test may do that.
// The keyless SearxNG branch exists in the flow precisely because that backend
// needs no credential (AC-8), so it is a real user path and not a test-only
// one; the store-then-commit ordering and the delete-on-failure are pinned
// against a fake keychain in `web_setup_ui`'s own suite (TASK-132).

/// **AC-10 / BR-12: on a pipe the command prints the instructions, asks
/// nothing, and the session carries on.**
///
/// The degradation is the requirement, not a fallback: a walkthrough that drew
/// a prompt at a pipe would read the next line of the *session's* input as a
/// tier answer (LESSON-470's is-terminal rule). So the assertions are in three
/// parts — what was printed, what was **not** asked, and that the line after
/// the command still reached the model.
#[test]
fn a_piped_web_setup_prints_the_instructions_and_asks_nothing() {
    let daemon_path = daemon_bin();
    let daemon = TestDaemon::spawn_scripted(&daemon_path, TURN_REPLIES);
    let teton = teton_bin();
    let config_path = daemon.root.join("config.toml");
    let before = std::fs::read(&config_path).expect("the fixture config exists");

    // No `run_cli_seamed`: this is the shipped posture, with the test seam off.
    let session = daemon.run_cli_with_stdin(&teton, &[], "/web setup\nhello there\n");

    // (1) The daemon's plan was rendered — the command ran, it did not refuse.
    assert!(
        session.contains("web lookup is available on this machine and currently off"),
        "`/web setup` must report the capability state even on a pipe; \
         output:\n{session}"
    );
    // (2) …and then it degraded to instructions, saying why.
    assert!(
        session.contains("which needs a terminal"),
        "AC-10: the piped branch must say why it is not asking; output:\n{session}"
    );
    assert!(
        session.contains("keychain reference"),
        "and it must still name the keychain rule, which is the part a user \
         writing the table by hand most needs; output:\n{session}"
    );
    // (3) It asked nothing. A drawn prompt is the defect this branch exists to
    // avoid, and its bytes are unmistakable.
    assert!(
        !session.contains("tier [1-3"),
        "AC-10: no prompt may be drawn on a pipe; output:\n{session}"
    );
    assert!(
        !session.contains("write this to your config?"),
        "and certainly no confirm; output:\n{session}"
    );
    // (4) Nothing was written.
    assert_eq!(
        std::fs::read(&config_path).ok().as_deref(),
        Some(before.as_slice()),
        "AC-10: the degraded path must leave the config untouched"
    );
    // (5) BR-12: "the session continues normally" — the line after the command
    // reached the model, so no session input was eaten by a prompt that was not
    // drawn.
    assert!(
        session.contains(TURN_REPLIES[0]),
        "the next typed line must still reach the model; output:\n{session}\n\
         daemon log:\n{}",
        daemon.log()
    );
}

/// **AC-3's client half: the walk collects every answer, previews the exact
/// bytes, and the write is announced by the daemon rather than by the client.**
///
/// Driven over pipes through the debug-build test seam, which the flow's gate
/// honours with the polarity `slash::test_seams_allowed` documents as the safe
/// one: the seam can only make the walkthrough *reachable*, so a release binary
/// ignoring it can only fall back to printing instructions. That is what lets
/// the whole walk — a tier answer, an endpoint, the keyless branch, the preview,
/// the default-no confirm — be driven without a terminal.
///
/// The tier is `search` on purpose. It is the only branch that asks the endpoint
/// and key-needed questions, and the endpoint is the keyless SearxNG shape the
/// flow itself suggests (AC-8) — so this test also proves that suggestion is
/// walkable end to end and not just parseable.
#[test]
fn the_walkthrough_collects_every_answer_and_the_daemon_announces_the_write() {
    let daemon_path = daemon_bin();
    let daemon = TestDaemon::spawn_scripted(&daemon_path, TURN_REPLIES);
    let teton = teton_bin();
    let config_path = daemon.root.join("config.toml");

    // Non-vacuity: this machine has no `[web]` table before the walk.
    let before = std::fs::read_to_string(&config_path).expect("the fixture config exists");
    assert!(
        !before.contains("[web]"),
        "the fixture must start with no `[web]` table:\n{before}"
    );

    const ENDPOINT: &str = "http://localhost:8888/search?format=json";
    let session = daemon.run_cli_seamed(
        &teton,
        &[],
        // tier → endpoint → "no key" → confirm.
        &format!("/web setup\n3\n{ENDPOINT}\nn\ny\n"),
    );

    // The menu was drawn, with the search row offered (this daemon has a
    // scripted local tier, so the search leg can serve — AC-7's other half).
    assert!(
        session.contains("3) search"),
        "the tier menu must be drawn; output:\n{session}"
    );
    assert!(
        !session.contains("(unavailable:"),
        "a machine with a local tier must not mark the search row unavailable; \
         output:\n{session}"
    );
    // THE CROSS-SEAM PIN (REQ-573). Every other assertion about these rows lives
    // on one side of the seam or the other: the daemon's golden test pins the
    // catalog it builds, and the client's `shipped_catalog()` fixture pins what
    // the renderer does with a catalog it was handed. Both can stay green while
    // disagreeing with each other, because the client's fixture is a hand
    // transcription of the daemon's list and nothing compares the two.
    //
    // This is the one place in CI where the real daemon's catalog reaches the
    // real renderer, so the rows are pinned **verbatim** here rather than by
    // substring. A label reworded on one side only, a header template that
    // drifted, a column that stopped lining up — each of them fails here, which
    // is the whole fixture-drift class.
    for row in [
        "  self-hosted SearxNG  http://localhost:8888/search?format=json  (no key)",
        "  Brave Search API     https://api.search.brave.com/res/v1/web/search  \
         (header `X-Subscription-Token: {key}`)",
        "  Kagi Search API      https://kagi.com/api/v0/search  \
         (header `Authorization: Bot {key}`)",
    ] {
        assert!(
            session.contains(row),
            "the endpoint question must carry the daemon's suggestions, byte for \
             byte — this row is not in the output:\n{row}\noutput:\n{session}"
        );
    }

    // BR-7: the exact bytes were shown before the confirm, and the host came
    // from the daemon's parse rather than from anything the client re-derived.
    assert!(
        session.contains("this is what would be written to your config:"),
        "the preview must be rendered; output:\n{session}"
    );
    assert!(
        session.contains("tier = \"search\""),
        "and it must carry the candidate table verbatim; output:\n{session}"
    );
    assert!(
        session.contains("searches would go to: localhost"),
        "the confirm step must name the host a query would reach; output:\n{session}"
    );
    assert!(
        session.contains("write this to your config? [y/N]"),
        "the one confirmation is default-no (LESSON-470); output:\n{session}"
    );

    // BR-14: the completion is the **daemon's** event reaching this client, not
    // a line the handler composed — which is why it names the config path the
    // daemon wrote.
    assert!(
        session.contains("web lookup enabled (`search`)"),
        "AC-3: the completion notice must render; output:\n{session}\n\
         daemon log:\n{}",
        daemon.log()
    );
    assert!(
        session.contains("Nothing has been looked up yet"),
        "and it must say that enabling looked nothing up (BR-13/OQ-2); \
         output:\n{session}"
    );

    // The write landed, in the file this daemon was given.
    let written = std::fs::read_to_string(&config_path).expect("read the config back");
    assert!(
        written.contains("[web]") && written.contains("tier = \"search\""),
        "the walk must have written the table:\n{written}"
    );
    assert!(
        written.contains(ENDPOINT),
        "including the endpoint the user typed:\n{written}"
    );
    assert!(
        !written.contains("search_key_ref"),
        "a keyless walk must reference no key:\n{written}"
    );

    // And none of it spent a model call — a command is not a prompt (BR-7 of
    // REQ-555, and the reason the walk is a client command at all).
    assert_no_turn_ran(&session, "the /web setup walkthrough");
}

// ---------------------------------------------------------------------------
// REQ-579 — the `/provider setup` walkthrough, at the client (TASK-157)
// ---------------------------------------------------------------------------
//
// AC → test map for this section:
//
//   AC-9 (piped stdin prints the recipe, exits 0, consumes no further stdin)
//   + BR-11
//       → `a_piped_provider_setup_prints_the_recipe_and_asks_nothing`
//
// What this section deliberately does **not** hold:
//
//   * the walk itself (AC-2/AC-6/AC-7/AC-8/AC-12) — every branch of it reaches
//     the credential step, and the shipped CLI writes credentials to the **real
//     OS keychain** (`keychain::default_keychain`) with no test seam to redirect
//     it, so a walk driven here would create — and on a refused commit delete —
//     a `teton/kimi` entry in whoever's login keychain ran the suite. Unlike
//     `/web setup`, this flow has no keyless branch to walk instead: a provider
//     registration without a credential reference is refused by construction
//     (the protocol type carries no `Default` for exactly that reason). Those
//     legs are `provider_setup_ui`'s own suite, against a fake keychain.
//   * the daemon-side flow (AC-2/AC-4/AC-7/AC-10/AC-11/AC-12) — `tetond`'s
//     `provider_setup_flow.rs` drives it against a spawned daemon that owns a
//     config file, which is where the write and the live routing can be seen.
//   * the echo-off sweep (AC-4's transcript half) — a pipe cannot observe echo.
//     It is `pty_e2e.rs`'s, and for `/provider setup` it is not added: the pty
//     harness has no fake-keychain seam (see its own note above
//     `the_setup_walk_stops_before_the_keychain_write`).

/// **AC-9 / BR-11: on a pipe the command prints the exact CLI recipe, asks
/// nothing, exits 0, and the line after it still reaches the model.**
///
/// The degradation is the requirement, not a fallback. A walkthrough that drew
/// a prompt at a pipe would read the next line of the *session's* input as a
/// model name — and the line after that as an API key, which is how a secret
/// ends up in a transcript. So the assertions are in four parts: what was
/// printed, what was **not** asked, that nothing was written, and that the
/// following line reached the model instead of being eaten.
///
/// The recipe lines are asserted **verbatim**, because AC-9's claim is that a
/// user can copy them into a shell. `instructions_are_commands_the_cli_itself_parses`
/// pins that they parse; this pins that the shipped binary actually emits them,
/// composed from the daemon's own catalog rather than from a CLI-side list
/// (BR-4) — this session's `kimi` endpoint and `--model kimi-k3` came over the
/// socket from `provider/setup_plan`.
#[test]
fn a_piped_provider_setup_prints_the_recipe_and_asks_nothing() {
    let daemon_path = daemon_bin();
    let daemon = TestDaemon::spawn_scripted(&daemon_path, TURN_REPLIES);
    let teton = teton_bin();
    let config_path = daemon.root.join("config.toml");
    let before = std::fs::read(&config_path).expect("the fixture config exists");

    // No `run_cli_seamed`: this is the shipped posture, with the test seam off.
    let (session, status) =
        daemon.run_cli_capture(&teton, &[], "/provider setup kimi\nhello there\n");

    // (1) It degraded to instructions, and said why.
    assert!(
        session.contains("which needs a terminal"),
        "AC-9: the piped branch must say why it is not asking; output:\n{session}"
    );

    // (2) The recipe, verbatim — the two commands the user can paste.
    assert!(
        session.contains(
            "teton provider add kimi --kind openai-compatible --endpoint \
             https://api.moonshot.ai/v1/chat/completions --model kimi-k3"
        ),
        "AC-9: the registration command must be printed exactly as the shell \
         takes it; output:\n{session}\ndaemon log:\n{}",
        daemon.log()
    );
    assert!(
        session.contains("teton policy set-tier think kimi"),
        "AC-9: and the routing command, defaulting to `think` (ADR-6); \
         output:\n{session}"
    );
    assert!(
        session.contains("keychain"),
        "and it must still name where the key goes, which is the part a user \
         running these by hand most needs; output:\n{session}"
    );

    // (3) It asked nothing. A drawn prompt is the defect this branch exists to
    // avoid, and each prompt's bytes are unmistakable.
    for prompt in [
        "vendor [number or name",
        "provider id [",
        "model [Enter for",
        "API key (not shown",
        "route [Enter for",
        "write this to your config?",
    ] {
        assert!(
            !session.contains(prompt),
            "AC-9: no prompt may be drawn on a pipe ({prompt:?}); output:\n{session}"
        );
    }

    // (4) Nothing was written, and nothing was previewed into the file.
    assert_eq!(
        std::fs::read(&config_path).ok().as_deref(),
        Some(before.as_slice()),
        "AC-9: the degraded path must leave the config untouched"
    );

    // (5) AC-9 claims an exit code, so this looks at one.
    assert!(
        status.success(),
        "AC-9: the piped session must exit 0; status {status:?}; output:\n{session}"
    );

    // (6) BR-11's "without consuming stdin": the line after the command reached
    // the model, so no session input was eaten by a prompt that was not drawn.
    assert!(
        session.contains(TURN_REPLIES[0]),
        "the next typed line must still reach the model; output:\n{session}\n\
         daemon log:\n{}",
        daemon.log()
    );
}

// ---------------------------------------------------------------------------
// REQ-581 — `teton provider test <id>`, the shell surface (BR-2 / BR-7, ADR-5)
// ---------------------------------------------------------------------------
//
// What this section holds is the **wiring** of the subcommand, which is the one
// thing no unit test can see: `run_provider_test` opens a session of its own
// (ADR-5 — the method is session-gated and the cost row needs a session), hands
// the flow that session and the process's `--yes`, and renders through the same
// module `/provider test` does. `provider_test_ui`'s own suite proves what the
// flow decides; this proves the shipped binary reaches it, both ways.
//
// The provider under test points at a **closed loopback port**, so the whole
// path — session, gate, consent, `provider/test`, adapter, transport, egress —
// runs to the end against a socket that answers `ECONNREFUSED` immediately. No
// mock server, no key, no vendor: `unreachable` is a real outcome produced by a
// real dial, and it is the only one that can be asserted hermetically.
//
// Not held here: the outcome table (401/404/429/5xx), which is `tetond`'s
// `provider_test_flow.rs` against its `MockProvider`, and the preview/decline
// legs, which are unit assertions on a call counter rather than proofs of a
// negative over a socket.

/// The fixture provider `teton provider test` is pointed at: a remote id whose
/// endpoint is a port nobody is listening on.
///
/// It carries a `model`, because a remote provider without one is refused before
/// anything is dialled (BUG-155), and **no** `auth_ref`, because a credential
/// reference would have to resolve against the real OS keychain — which this
/// suite must never touch (see the `/provider setup` section's note).
fn unreachable_provider_config(port: u16) -> String {
    format!(
        "[[providers]]\nid = \"probe\"\nkind = \"openai-compatible\"\n\
         endpoint = \"http://127.0.0.1:{port}/v1/chat/completions\"\n\
         model = \"probe-model\"\n\n"
    )
}

/// **BR-2 / AC-4 at the shell: a pipe without `--yes` sends nothing and says
/// what would have let it.**
///
/// The subcommand's gate is the *flow's* gate, reached through a session this
/// command opened — so a piped run has to answer at the gate and stop, without
/// the session it just created having spent anything. The negative is what the
/// leg is for: the report line the `--yes` run below prints must be absent here,
/// which is the observable form of "nothing left the machine".
#[test]
fn a_piped_provider_test_without_yes_sends_nothing_and_names_the_flag() {
    let daemon_path = daemon_bin();
    let daemon = TestDaemon::spawn_scripted_with_config(
        &daemon_path,
        TURN_REPLIES,
        &unreachable_provider_config(closed_port()),
    );
    let teton = teton_bin();

    let (output, status) = daemon.run_cli_capture(&teton, &["provider", "test", "probe"], "");

    assert!(
        output.contains("asks before it sends"),
        "the refusal must say why it did not ask; output:\n{output}\ndaemon log:\n{}",
        daemon.log()
    );
    assert!(
        output.contains("--yes"),
        "and name the flag that consents in advance; output:\n{output}"
    );
    assert!(
        output.contains("Nothing was sent"),
        "and say that nothing left the machine; output:\n{output}"
    );

    // The observable half of "nothing was sent": no dial happened, so no report
    // line exists. Asserted on the fragments the renderers own — the verdict
    // word and the trailing clause every failed outcome carries.
    assert!(
        !output.contains("unreachable") && !output.contains("Nothing else was sent"),
        "a refused run must produce no report at all; output:\n{output}"
    );
    assert!(
        !output.contains("proceed?"),
        "no question may be drawn at a pipe; output:\n{output}"
    );
    assert!(
        status.success(),
        "a refusal ends the command, not the process's exit code; status {status:?}; \
         output:\n{output}"
    );
}

/// **BR-7 / ADR-5: `teton -y provider test <id>` runs the whole flow and reports
/// what came back.**
///
/// End to end through the shipped binary: the subcommand opens a session, the
/// `--yes` stands in for the confirm, `provider/test` dials the closed port, and
/// the typed `unreachable` outcome comes back and is rendered by the same
/// `provider_test_ui` the slash command uses. That chain is exactly what a unit
/// test cannot reach — the session-opening half of ADR-5 lives in `main`.
///
/// Asserted on the fixed fragments the two renderers own (the preview's shape,
/// the verdict word, the health clause), never on the daemon's `reason`: that
/// sentence is composed from the dial host and belongs to `tetond`'s tests.
#[test]
fn a_consented_provider_test_runs_end_to_end_and_reports_unreachable() {
    let daemon_path = daemon_bin();
    let port = closed_port();
    let daemon = TestDaemon::spawn_scripted_with_config(
        &daemon_path,
        TURN_REPLIES,
        &unreachable_provider_config(port),
    );
    let teton = teton_bin();

    let (output, status) = daemon.run_cli_capture(&teton, &["-y", "provider", "test", "probe"], "");
    let log = daemon.log();

    // (1) The preview ran, naming the provider and the endpoint it will dial —
    // the line BR-2 requires before anything leaves the machine, printed even
    // when the consent came from the flag.
    assert!(
        output.contains("provider:  probe (openai-compatible, probe-model)"),
        "the preview must name the provider, its kind and its model; output:\n{output}\n\
         daemon log:\n{log}"
    );
    assert!(
        output.contains(&format!("http://127.0.0.1:{port}/v1/chat/completions")),
        "and the endpoint as stored; output:\n{output}"
    );

    // (2) `--yes` is the consent, so no question was drawn.
    assert!(
        !output.contains("proceed?"),
        "--yes must consume no prompt; output:\n{output}"
    );

    // (3) The report: the session was opened, the method was served, and the
    // typed outcome came back. `unreachable` is the honest ending for a closed
    // port, and it proves the dial actually happened.
    assert!(
        output.contains("probe probe-model: unreachable —"),
        "the report must name the provider, the model and the verdict; output:\n{output}\n\
         daemon log:\n{log}"
    );
    assert!(
        output.contains("Nothing else was sent"),
        "a failed outcome says the call stopped there; output:\n{output}"
    );
    assert!(
        output.contains("provider health:"),
        "BR-4: the report says what the next turn will do; output:\n{output}"
    );

    // (4) Nothing about the session it opened leaked into the report, and the
    // command exited cleanly: a provider that cannot be reached is an answer,
    // not a CLI failure.
    assert!(
        !output.contains("could not start a session"),
        "the subcommand must have opened its own session (ADR-5); output:\n{output}\n\
         daemon log:\n{log}"
    );
    assert!(
        status.success(),
        "an unreachable provider is a report, not an error exit; status {status:?}; \
         output:\n{output}"
    );
}

/// **ADR-9 / AC-1's non-TTY half: a script's bytes do not move.**
///
/// The hand-off nudge is a terminal affordance — it names an in-session command
/// nobody at a pipe can type. So the gate is `typed_input`, and this is the leg
/// that proves the gate is real rather than asserted: the scripted engine
/// answers with a reply that recites `teton provider add`, which is exactly the
/// condition that arms the line at a terminal, and a piped session must still
/// see nothing added.
///
/// The negative is worth an e2e rather than only a unit test because the unit
/// test can only pin the function's own gate; what a script actually receives
/// depends on the flag `main` passes it, and that wiring is unobservable from
/// inside the module. BR-11 already gives a script the shell recipe, which is
/// the copy-pasteable answer at a pipe; a second line naming a command it
/// cannot run would be noise in whatever is parsing the output.
#[test]
fn a_piped_session_whose_reply_recites_the_cli_gets_no_hand_off_line() {
    let daemon_path = daemon_bin();
    // The reply the guide actually produces at the front door — the recital
    // three live rounds recorded (verification.md §1–§24).
    let reply = "register it from a shell: teton provider add kimi --kind \
                 openai-compatible, then teton policy set-tier think kimi.";
    let daemon = TestDaemon::spawn_scripted(&daemon_path, &[reply]);
    let teton = teton_bin();

    let (session, status) = daemon.run_cli_capture(&teton, &[], "how do I add kimi?\n");

    // The precondition: the turn ran and the reply really did recite the CLI.
    // Without this the assertion below would pass on a session that never
    // reached the model at all.
    assert!(
        session.contains("teton provider add kimi"),
        "the scripted reply must have reached the transcript, or this test \
         proves nothing; output:\n{session}\ndaemon log:\n{}",
        daemon.log()
    );

    // And the nudge is absent. Asserted on the sentence's distinctive halves
    // rather than the whole line, so a future rewording of it cannot make this
    // pass by accident.
    for absent in [
        "does this without leaving it",
        "no key in chat",
        "/provider setup <vendor> [tier]",
    ] {
        assert!(
            !session.contains(absent),
            "ADR-9: a pipe must see no hand-off ({absent:?}); output:\n{session}"
        );
    }
    assert!(
        status.success(),
        "the piped session must still exit 0; status {status:?}; output:\n{session}"
    );
}

// ---------------------------------------------------------------------------
// REQ-582 — a `teton …` line typed at the session prompt (BR-4, ADR-1)
// ---------------------------------------------------------------------------

/// The tail of the line every session announces itself with.
const READY_MARKER: &str = "ready (freeform)";

/// Everything a piped session printed between its ready line and its
/// session-end cost summary — the part the typed lines are responsible for.
///
/// Anchored at both ends rather than sliced from the start because the head of a
/// session (the attach, the lifecycle replay) and its tail (the cost report,
/// which quotes figures) carry text that is not about the command under test,
/// and the session id in the ready line differs between two runs by
/// construction.
fn session_body<'a>(output: &'a str, what: &str) -> &'a str {
    let start = output
        .find(READY_MARKER)
        .and_then(|at| output[at..].find('\n').map(|nl| at + nl + 1))
        .unwrap_or_else(|| panic!("{what} never opened a session; output:\n{output}"));
    let end = output[start..]
        .find(COST_MARKER)
        .unwrap_or_else(|| panic!("{what} printed no session-end summary; output:\n{output}"));
    &output[start..start + end]
}

/// The notice a recognized CLI line prints, as the surface draws it.
const PROVIDER_LIST_NOTE: &str = ">> teton provider list → /provider list";

/// **AC-5: typing `teton provider list` runs the row, and costs no turn.**
///
/// Two claims, and they need different evidence.
///
/// *The same command ran.* Both spellings are driven against one daemon and the
/// bodies of the two sessions are diffed — everything between the ready line and
/// the session-end summary, byte for byte, with the notice line removed from the
/// typed one. A `contains` of some phrase from the listing would pass on a
/// recognition that reached a *different* row, or one that rendered half the
/// report; the diff is the only statement that says "the same command, the same
/// renderer" (LESSON-517).
///
/// *No model call was made.* Asserted by the scripted engine's reply queue
/// rather than by a timer or by the absence of a word: the daemon replays one
/// canned reply per turn, so a turn taken by the typed line would put
/// `TURN_REPLIES[0]` in the transcript. [`assert_no_turn_ran`] checks that and
/// every other marker a turn leaves behind, so the claim is deterministic —
/// there is nothing to wait for and nothing to race.
#[test]
fn a_typed_teton_line_runs_the_row_it_names_and_costs_no_turn() {
    let daemon_path = daemon_bin();
    let daemon = TestDaemon::spawn_scripted(&daemon_path, TURN_REPLIES);
    let teton = teton_bin();

    let slashed = daemon.run_cli_with_stdin(&teton, &[], "/provider list\n");
    let typed = daemon.run_cli_with_stdin(&teton, &[], "teton provider list\n");

    // The one line the recognized spelling adds: what was typed, and the
    // spelling this session uses for it.
    assert!(
        typed.contains(PROVIDER_LIST_NOTE),
        "a recognized line must name the session spelling; output:\n{typed}"
    );
    assert!(
        !slashed.contains(PROVIDER_LIST_NOTE),
        "the `/` spelling has nothing to translate; output:\n{slashed}"
    );

    let slashed_body = session_body(&slashed, "`/provider list`");
    let typed_body = session_body(&typed, "`teton provider list`");
    // The precondition: the listing really rendered, so the diff below is
    // between two reports rather than between two empty strings.
    assert!(
        slashed_body.contains("deepseek"),
        "`/provider list` printed no providers; output:\n{slashed}"
    );
    assert_eq!(
        typed_body.replacen(&format!("{PROVIDER_LIST_NOTE}\n"), "", 1),
        slashed_body,
        "`teton provider list` and `/provider list` printed different sessions;\n\
         typed:\n{typed}\nslashed:\n{slashed}"
    );

    // AC-5's load-bearing half, on both spellings.
    assert_no_turn_ran(&typed, "a typed `teton provider list`");
    assert_no_turn_ran(&slashed, "`/provider list`");
}

/// **AC-6: what a `teton …` line gets when it is not a command this session can
/// run — and what it still gets when it is not a command at all.**
///
/// Four lines through one session, because the four outcomes are one decision
/// table and a session that answered three of them correctly while dropping the
/// fourth would be the bug (BR-4's totality):
///
/// * `teton uninstall` — a real command with no session form. One refusing line
///   naming the shell, and no turn.
/// * `teton provider list please` — recognized (its path is a row), answered by
///   the CLI parser's **own** `unexpected argument`, and no turn (ADR-1's
///   amendment to AC-6).
/// * `//teton provider list` — the escape hatch still outranks recognition
///   (REQ-555 BR-1b), so this one *is* a turn and the model sees the line with
///   its leading pair collapsed.
/// * `teton is slow today` — a question about the product, unchanged: a turn,
///   and the model's answer.
///
/// The reply queue is the arithmetic that ties them together: three of the four
/// lines must spend nothing, so the two turns take the first two scripted
/// replies. A regression that sent a refused line to the model would shift
/// them.
#[test]
fn a_teton_line_with_no_session_form_is_refused_and_a_question_still_reaches_the_model() {
    let daemon_path = daemon_bin();
    let daemon = TestDaemon::spawn_scripted(&daemon_path, TURN_REPLIES);
    let teton = teton_bin();

    let session = daemon.run_cli_with_stdin(
        &teton,
        &[],
        "teton uninstall\n\
         teton provider list please\n\
         //teton provider list\n\
         teton is slow today\n",
    );

    // The refusal names the command, the reason, and the shell.
    assert!(
        session.contains("`teton uninstall` is shell-only"),
        "`teton uninstall` must be refused by name; output:\n{session}"
    );
    assert!(
        session.contains("from a shell instead"),
        "the refusal must point at the shell; output:\n{session}"
    );

    // The parser's own error for the stray word — the same text the shell
    // prints for that argv (BR-3), and the row's clap parse rather than a
    // second reading of the line.
    assert!(
        session.contains("unexpected argument 'please'"),
        "a recognized line's stray word is a parser error; output:\n{session}"
    );

    // Neither refused line reached a listing: no provider table was rendered by
    // either of them, so the only thing they cost was a line of text.
    assert!(
        !session.contains("deepseek-v4-pro"),
        "a refused or rejected line still ran the command; output:\n{session}"
    );

    // Exactly two turns ran, and they are the escaped line and the question —
    // in that order, from the head of the reply queue.
    assert!(
        session.contains(TURN_REPLIES[0]),
        "`//teton provider list` never reached the model; output:\n{session}\n\
         daemon log:\n{}",
        daemon.log()
    );
    assert!(
        session.contains(TURN_REPLIES[1]),
        "`teton is slow today` never reached the model; output:\n{session}"
    );
    assert!(
        !session.contains(TURN_REPLIES[2]),
        "a refused line spent a turn: the reply queue moved three times; \
         output:\n{session}"
    );
}

// ---------------------------------------------------------------------------
// REQ-582 — the mirrored rows against their shell twins (AC-1, AC-2, AC-4,
// AC-8, AC-11)
// ---------------------------------------------------------------------------
//
// AC → test map for this section:
//
//   AC-1 (a read row prints what its twin prints)
//       → `every_read_row_prints_exactly_what_its_shell_twin_prints`
//   AC-2 (the write rows change the config the twins read back)
//       → `the_write_rows_change_the_config_their_shell_twins_read_back`
//   AC-4 (a write row on a pipe refuses; a read row does not)
//       → `on_a_pipe_every_write_row_names_its_shell_twin_and_changes_nothing`
//   AC-8 (`/help` lists every new row, grouped, with both footers)
//       → `slash_help_lists_every_mirrored_row_grouped_with_both_footers`
//   AC-11 (presence: a refused write leaves config.toml byte-identical)
//       → `a_presence_refused_session_set_tier_leaves_the_config_untouched`
//       + `an_attested_session_set_tier_writes` (the non-vacuity anchor)
//
// AC-3 (`/provider add`'s echo-off key) is not here and cannot be: a pipe has no
// echo bit. Its terminal half is `pty_e2e.rs`'s
// `a_session_provider_add_asks_for_its_key_echo_off_and_stores_nothing_untyped`,
// and the seam question that test's doc comment answers — there is no keychain
// double the shipped binary can be pointed at — is why the walk there stops
// where it does.

/// A rendering's lines with leading and trailing blank lines removed.
///
/// The interior is untouched, so a dropped line, a reordered one or a reworded
/// one all fail; what is discarded is only the padding around the two capture
/// windows, which differ by construction. A shell command's stdout ends at its
/// last line; a session body is a slice between two anchors and carries whatever
/// blank line the session put between the command's last line and its
/// end-of-session summary. Comparing those two verbatim would be comparing the
/// harnesses rather than the renderings.
fn command_lines(text: &str) -> Vec<&str> {
    let mut lines: Vec<&str> = text.lines().collect();
    while lines.first().is_some_and(|line| line.trim().is_empty()) {
        lines.remove(0);
    }
    while lines.last().is_some_and(|line| line.trim().is_empty()) {
        lines.pop();
    }
    lines
}

/// The entry prompt a piped session draws before every line it reads
/// (`main`'s `entry_prompt`, non-interactive form).
///
/// It is written without a newline, so the first line of whatever the session
/// prints next is *prefixed* with it — and the next prompt opens the line after
/// the command's last. That makes it exactly the frame [`typed_output`] needs.
const ENTRY_PROMPT: &str = "› ";

/// What one typed line made a piped session print, taken out of its frame.
///
/// A session body opens at an entry prompt, carries the command's rendering, and
/// then draws the next prompt — on whose line the end-of-session summary lands
/// when stdin is closed. So the command's own output is everything between the
/// first prompt and the second, with the first prompt's two characters removed
/// from the head of the line it shares.
///
/// Extracted rather than tolerated, because the alternative is comparing a
/// shell's output against a slice that carries a prompt on one end and a cost
/// summary's preamble on the other — a comparison that would have to be loosened
/// until it stopped being a diff.
fn typed_output<'a>(body: &'a str, what: &str) -> Vec<&'a str> {
    let mut lines = body.lines();
    let first = lines
        .next()
        .and_then(|line| line.strip_prefix(ENTRY_PROMPT))
        .unwrap_or_else(|| panic!("{what} did not open at an entry prompt; body:\n{body}"));
    let mut out = vec![first];
    for line in lines {
        if line.starts_with(ENTRY_PROMPT) {
            break;
        }
        out.push(line);
    }
    while out.last().is_some_and(|line| line.trim().is_empty()) {
        out.pop();
    }
    out
}

/// The complete lines a session printed before its entry prompt — the daemon's
/// replay of the model lifecycle to the connection that just attached
/// (BUG-177's routing), which every client run against the same daemon receives.
///
/// The final segment is dropped because it is not a complete line: the session
/// announces itself with `session <id> ready (freeform)…`, and the anchor this
/// splits on sits in the middle of it.
fn attach_lines(session: &str) -> Vec<&str> {
    let head = session
        .split(READY_MARKER)
        .next()
        .expect("split always yields a first segment");
    let mut lines: Vec<&str> = head.lines().collect();
    lines.pop();
    lines.retain(|line| !line.trim().is_empty());
    lines
}

/// The six read rows: the session spelling, and the argv its shell twin takes.
///
/// Written out rather than derived from the table, deliberately: this suite runs
/// the **shipped binary** and knows nothing of the crate's internals, so the two
/// spellings being the same words is a claim under test here rather than an
/// assumption. (`cli_rows.rs`'s unit tests pin the derivation itself.)
const READ_ROWS: &[(&str, &[&str])] = &[
    ("provider list", &["provider", "list"]),
    ("boundary list", &["boundary", "list"]),
    ("policy show", &["policy", "show"]),
    ("model list", &["model", "list"]),
    ("model status", &["model", "status"]),
    ("doctor", &["doctor"]),
];

/// The one line `/doctor` and `teton doctor` are allowed to disagree about
/// (BR-7, ADR-5): the shell handshakes and can name the protocol version, the
/// session reports the connection it already has.
const DOCTOR_DAEMON_LINE: &str = "daemon: running — ";

/// **AC-1: every read row prints exactly what its shell twin prints.**
///
/// One daemon, twelve client runs: each row is driven from a shell (`teton
/// provider list`) and from a piped session (`/provider list`), and the two
/// renderings are diffed line by line. Not a `contains` of a phrase from each
/// report — a `contains` passes on half a report, on a report rendered by a
/// second copy of the renderer that has since drifted, and on a row that reached
/// a different command with overlapping output. The diff is the only statement
/// that says "one grammar, one renderer, one daemon method" (BR-2; LESSON-517 —
/// the seam is the ground truth for parity, so pin the crossing bytes).
///
/// Both surfaces are a `PlainSurface` over a pipe with colour off, so the line
/// classes draw the same prefixes on both (`>> ` for a notice, nothing for
/// info) and the comparison is on whole lines rather than on text after a
/// prefix. That is worth stating because it is a property of `render.rs` this
/// test depends on rather than one it asserts: if the two surfaces ever
/// diverge, this test is where it shows up.
///
/// `/doctor` carries the single documented exception (BR-7): the line naming the
/// daemon is dropped from both sides before the diff, and then each side's own
/// version of it is asserted — the shell's from a fresh handshake, the
/// session's from the connection it is already on.
#[test]
fn every_read_row_prints_exactly_what_its_shell_twin_prints() {
    let daemon_path = daemon_bin();
    let daemon = TestDaemon::spawn_scripted(&daemon_path, TURN_REPLIES);
    let teton = teton_bin();

    for (row, argv) in READ_ROWS {
        let shell = daemon.run_cli_stdout(&teton, argv);
        let session = daemon.run_cli_with_stdin(&teton, &[], &format!("/{row}\n"));
        let body = session_body(&session, &format!("`/{row}`"));

        // The precondition for the whole comparison: the shell twin actually
        // rendered a report. Without it a daemon that answered nothing would
        // make two empty strings equal and this test would pass on silence.
        let shell_all = command_lines(&shell);
        assert!(
            !shell_all.is_empty(),
            "`teton {}` printed nothing at all; stdout:\n{shell}",
            argv.join(" ")
        );

        // **The shell run attaches too**, and since BUG-177 the daemon replays
        // the model lifecycle to whichever connection is attaching — so a shell
        // command's stdout also carries the `>> probe: …` / `>> local model …
        // ready` lines the session printed before its own entry prompt. They are
        // part of neither rendering, and they do not sit in one place: `doctor`
        // prints its header before the `config/get` those frames are drained by,
        // so they land *inside* its report while `provider list` sees them ahead
        // of one.
        //
        // So they are removed by identity rather than by position or by shape.
        // The lines are taken from the session's own attach — a shape filter
        // (`>> `) would also swallow a report's own notices, and `/doctor`'s
        // trailer is two of them — and each is removed exactly once, with the
        // leftovers asserted empty afterwards so an over- or under-match is a
        // failure rather than a quietly widened comparison.
        let mut replay = attach_lines(&session);
        assert!(
            !replay.is_empty(),
            "the session printed nothing before its entry prompt, so the \
             attach-replay filter below holds over nothing; session:\n{session}"
        );
        let is_daemon_line = |line: &&str| line.starts_with(DOCTOR_DAEMON_LINE);
        let session_lines: Vec<&str> = typed_output(body, &format!("`/{row}`"))
            .into_iter()
            .filter(|line| !is_daemon_line(line))
            .collect();
        let shell_lines: Vec<&str> = shell_all
            .iter()
            .copied()
            .filter(|line| !is_daemon_line(line))
            .filter(|line| match replay.iter().position(|seen| seen == line) {
                Some(at) => {
                    replay.remove(at);
                    false
                }
                None => true,
            })
            .collect();
        assert!(
            replay.is_empty(),
            "`teton {}` did not replay every line the session's attach did \
             ({replay:?}), so the two runs are not in the same daemon state and \
             the diff below would be about that; session:\n{session}\n\
             shell:\n{shell}",
            argv.join(" ")
        );
        assert_eq!(
            session_lines,
            shell_lines,
            "`/{row}` and `teton {}` printed different reports.\n\
             session:\n{session}\nshell:\n{shell}",
            argv.join(" ")
        );

        if *row != "doctor" {
            // Only `/doctor` prints the line the filter above removes, so for
            // every other row the filter must have removed nothing — otherwise
            // the diff was run over a report with a line quietly taken out of
            // it, on both sides, and would not have noticed.
            assert_eq!(
                session_lines.len(),
                typed_output(body, &format!("`/{row}`")).len(),
                "`/{row}` printed a `{DOCTOR_DAEMON_LINE}…` line, so the \
                 `/doctor` carve-out silently applied to it; session:\n{session}"
            );
            continue;
        }

        // BR-7: the carve-out itself. The session names the attach it already
        // has; the shell names the protocol it just negotiated. Each is asserted
        // on its own side, so a build that printed the session's wording from
        // the shell (or the reverse) fails here rather than passing the diff.
        let line_of = |text: &str, what: &str| -> String {
            text.lines()
                .find(|line| line.starts_with(DOCTOR_DAEMON_LINE))
                .unwrap_or_else(|| panic!("{what} printed no daemon line; output:\n{text}"))
                .to_owned()
        };
        let session_daemon = line_of(body, "`/doctor`");
        let shell_daemon = line_of(&shell, "`teton doctor`");
        assert!(
            session_daemon.ends_with("(this session's connection)"),
            "BR-7: `/doctor` must report the connection this session already \
             has, not a fresh handshake; line: {session_daemon:?}"
        );
        assert!(
            shell_daemon.contains("(protocol "),
            "`teton doctor` still handshakes, so it still names the protocol; \
             line: {shell_daemon:?}"
        );
        // And they agree about everything a handshake does not decide — the
        // daemon's name and version — which is what makes the carve-out one
        // clause rather than one line of a different report.
        let named = shell_daemon
            .trim_start_matches(DOCTOR_DAEMON_LINE)
            .split(" (protocol ")
            .next()
            .expect("the shell line names a daemon")
            .to_owned();
        assert!(
            session_daemon.contains(&named),
            "the two doctor reports name different daemons: {session_daemon:?} \
             vs {shell_daemon:?}"
        );
    }
}

/// **AC-2: the write rows change the config their shell twins read back.**
///
/// Driven under `TETON_TEST_SEAMS=1` (`run_cli_seamed`) because the rows are
/// typed-input-only and this harness is a pipe (ADR-4) — the refusal that gate
/// produces without the seam is AC-4's subject, one test below.
///
/// The evidence is deliberately **the twin's own reading**, not the writing
/// row's echo: `/policy set-tier` saying "the 'build' tier now routes to
/// `deepseek`" is a sentence this client composed, while `teton policy show`
/// re-deriving that binding from the daemon's resolver is the daemon agreeing.
/// The before/after pair makes each assertion non-vacuous — the fixture binds
/// every tier to `local`, so a row that did nothing would leave `build` there.
#[test]
fn the_write_rows_change_the_config_their_shell_twins_read_back() {
    let daemon_path = daemon_bin();
    let daemon = TestDaemon::spawn_scripted(&daemon_path, TURN_REPLIES);
    let teton = teton_bin();

    // Before: the fixture's own bindings, read through the twins.
    let policy_before = daemon.run_cli_stdout(&teton, &["policy", "show"]);
    let boundary_before = daemon.run_cli_stdout(&teton, &["boundary", "list"]);
    assert!(
        policy_before.contains("build") && !policy_before.contains("build    → deepseek"),
        "the fixture must start with `build` bound somewhere other than \
         deepseek, or the assertion below proves nothing; output:\n{policy_before}"
    );
    assert!(
        boundary_before.contains("no privacy boundaries configured"),
        "the fixture must start with no boundaries; output:\n{boundary_before}"
    );

    let session = daemon.run_cli_seamed(
        &teton,
        &[],
        "/policy set-tier build deepseek\n\
         /policy set-category edit deepseek --fallback local\n\
         /boundary add src/** --mode local-only\n",
    );

    // What each row said it did — the client's half, and the precondition for
    // reading anything into the config below.
    for said in [
        "the 'build' tier now routes to `deepseek`.",
        "the 'edit' category now routes to `deepseek` (fallback local).",
        "boundary added: src/** [local-only]",
    ] {
        assert!(
            session.contains(said),
            "a write row did not report its write ({said:?}); output:\n{session}\n\
             daemon log:\n{}",
            daemon.log()
        );
    }
    // And no row met the typed-input gate: the seam is what this test is driven
    // through, so a refusal here would mean the seam stopped working and every
    // assertion below would be about a config nothing wrote.
    assert!(
        !session.contains("typed-input-only"),
        "a seamed session must not meet the write gate; output:\n{session}"
    );

    // After: the twins read the daemon's own resolution back.
    let policy_after = daemon.run_cli_stdout(&teton, &["policy", "show"]);
    let boundary_after = daemon.run_cli_stdout(&teton, &["boundary", "list"]);
    let tier_row = policy_after
        .lines()
        .find(|line| line.trim_start().starts_with("build "))
        .unwrap_or_else(|| panic!("`teton policy show` printed no build row:\n{policy_after}"));
    assert!(
        tier_row.contains("→ deepseek"),
        "`/policy set-tier build deepseek` did not reach the daemon's routing \
         table; row: {tier_row:?}\nfull:\n{policy_after}"
    );
    let category_row = policy_after
        .lines()
        .find(|line| line.trim_start().starts_with("edit "))
        .unwrap_or_else(|| panic!("`teton policy show` printed no edit row:\n{policy_after}"));
    assert!(
        category_row.contains("→ deepseek") && category_row.contains("(fallback local)"),
        "`/policy set-category edit deepseek --fallback local` did not reach the \
         routing table with its fallback; row: {category_row:?}\nfull:\n{policy_after}"
    );
    assert!(
        boundary_after.contains("  src/** [local-only]"),
        "`/boundary add src/** --mode local-only` did not reach the privacy \
         boundaries; output:\n{boundary_after}"
    );
}

/// **AC-4: on a pipe every write row refuses by naming its shell twin, and every
/// read row still answers.**
///
/// The same session drives both halves, because the pairing is the rule (BR-11):
/// a gate that also stopped the reads would make the piped e2e suites — all of
/// them — blind to the rows this REQ added, and a gate that stopped nothing would
/// let a CI step reconfigure the machine it runs on.
///
/// "Changed nothing" is read back through the shell twins afterwards rather than
/// inferred from the refusal, for LESSON-519's reason: the error is what the
/// client said, and the config is what happened.
#[test]
fn on_a_pipe_every_write_row_names_its_shell_twin_and_changes_nothing() {
    let daemon_path = daemon_bin();
    let daemon = TestDaemon::spawn_scripted(&daemon_path, TURN_REPLIES);
    let teton = teton_bin();

    let policy_before = daemon.run_cli_stdout(&teton, &["policy", "show"]);
    let boundary_before = daemon.run_cli_stdout(&teton, &["boundary", "list"]);
    let providers_before = daemon.run_cli_stdout(&teton, &["provider", "list"]);

    let session = daemon.run_cli_with_stdin(
        &teton,
        &[],
        "/provider add kimi2 --kind openai-compatible \
         --endpoint http://127.0.0.1:1/v1/chat/completions --model kimi-k3\n\
         /boundary add src/** --mode local-only\n\
         /policy set-tier build deepseek\n\
         /policy set-category edit deepseek --fallback local\n\
         /provider list\n\
         /policy show\n\
         /boundary list\n",
    );

    // Each write row: exactly one line, naming the row, what was checked, and
    // the shell command that does the same thing unattended.
    for (row, twin) in [
        ("provider add", "teton provider add"),
        ("boundary add", "teton boundary add"),
        ("policy set-tier", "teton policy set-tier"),
        ("policy set-category", "teton policy set-category"),
    ] {
        let refusals: Vec<&str> = session
            .lines()
            .filter(|line| line.contains(&format!("/{row} is typed-input-only")))
            .collect();
        assert_eq!(
            refusals.len(),
            1,
            "`/{row}` on a pipe must refuse in exactly one line; output:\n{session}"
        );
        assert!(
            refusals[0].contains(twin),
            "`/{row}`'s refusal must name `{twin}`; line: {:?}",
            refusals[0]
        );
        assert!(
            refusals[0].contains("not a terminal"),
            "`/{row}`'s refusal must say what was actually checked; line: {:?}",
            refusals[0]
        );
    }
    // BR-11: the reads on the same pipe answered.
    for answered in [
        "providers:",
        "tiers — the primary surface",
        "no privacy boundaries configured",
    ] {
        assert!(
            session.contains(answered),
            "a read row was refused on the same pipe ({answered:?}); \
             output:\n{session}"
        );
    }
    // A refused row costs no turn either: it never reaches the model.
    assert_no_turn_ran(&session, "the piped write rows");

    // And nothing moved. Read back through the twins, byte for byte.
    assert_eq!(
        command_lines(&daemon.run_cli_stdout(&teton, &["policy", "show"])),
        command_lines(&policy_before),
        "a refused write row changed the routing table"
    );
    assert_eq!(
        command_lines(&daemon.run_cli_stdout(&teton, &["boundary", "list"])),
        command_lines(&boundary_before),
        "a refused write row changed the privacy boundaries"
    );
    assert_eq!(
        command_lines(&daemon.run_cli_stdout(&teton, &["provider", "list"])),
        command_lines(&providers_before),
        "a refused `/provider add` registered a provider"
    );
}

/// **AC-8: `/help` lists every mirrored row, grouped, with both footers.**
///
/// The unit test in `slash.rs` pins that the listing is *generated* from the
/// table and that the grouping rule holds over it; this pins that the **shipped
/// binary** prints the ten rows a user is meant to discover, with the summaries
/// that tell them what each one takes. A row that dispatches and cannot be found
/// in `/help` is a row the user does not have (BUG-153) — which for this REQ
/// would mean shipping the parity and hiding it.
#[test]
fn slash_help_lists_every_mirrored_row_grouped_with_both_footers() {
    let daemon_path = daemon_bin();
    let daemon = TestDaemon::spawn_scripted(&daemon_path, TURN_REPLIES);
    let teton = teton_bin();

    let session = daemon.run_cli_with_stdin(&teton, &[], "/help\n");
    // The listing on its own: the first row line shares its row with the entry
    // prompt the session drew before reading `/help`, and every check below is
    // about what the row line says rather than about that frame.
    let listing = typed_output(session_body(&session, "`/help`"), "`/help`");

    // The ten new rows, each with the head of the summary `/help` generates for
    // it. Asserted as a `/name … summary` pair on one rendered line, so a row
    // that lost its summary — or gained a second, hand-written listing — fails.
    for (name, summary) in [
        (
            "/provider list",
            "List the providers registered on this machine",
        ),
        (
            "/provider add",
            "Register a provider by hand: /provider add <id>",
        ),
        ("/boundary list", "List the privacy boundaries"),
        ("/boundary add", "Add a privacy boundary over a path glob"),
        ("/policy show", "Show the effective routing table"),
        (
            "/policy set-tier",
            "Route a tier to a provider: /policy set-tier",
        ),
        (
            "/policy set-category",
            "Route one category ahead of its tier",
        ),
        ("/model list", "Show the model catalog"),
        ("/model status", "Report the recorded model decision"),
        (
            "/doctor",
            "Diagnose the daemon, socket, model state, and providers",
        ),
    ] {
        assert!(
            listing
                .iter()
                .any(|line| line.contains(name) && line.contains(summary)),
            "`{name}` is missing from /help with its summary; output:\n{session}"
        );
    }
    // The rows that were already there are still there — a regrouped listing
    // that dropped one would be a worse `/help` than the one this REQ found.
    for name in [
        "/help",
        "/cost",
        "/effort",
        "/model",
        "/model set",
        "/clear",
        "/verbose",
        "/permissions",
        "/web setup",
        "/web allow",
        "/web refresh",
        "/provider setup",
        "/provider test",
        "/quit",
    ] {
        assert!(
            listing
                .iter()
                .any(|line| line.starts_with(&format!("{name} "))),
            "`{name}` vanished from /help; output:\n{session}"
        );
    }

    // The grouping (BR-1): a family's rows are one contiguous run, and a blank
    // line separates it from the next family. Read off the rendered listing by
    // its first words rather than assumed from the table.
    assert!(
        listing.iter().any(|line| line.trim().is_empty()),
        "the listing has no blank line, so nothing is grouped; \
         listing:\n{listing:#?}"
    );
    let mut seen: Vec<String> = Vec::new();
    let mut current: Option<String> = None;
    for line in &listing {
        let Some(name) = line.strip_prefix('/') else {
            current = None;
            continue;
        };
        let family = name
            .split_whitespace()
            .next()
            .expect("a row line names a row")
            .to_owned();
        if current.as_deref() != Some(family.as_str()) {
            assert!(
                !seen.contains(&family),
                "the `/{family}` rows are not contiguous in /help — a family \
                 listed twice is a listing a reader has to scan twice; \
                 listing:\n{listing:#?}"
            );
            seen.push(family.clone());
            current = Some(family);
        }
    }

    // Both footers, and the argument one is where OQ-5's limitation is
    // documented for the user rather than only for the spec.
    assert!(
        session
            .contains("Command arguments are split on whitespace and quotes are not interpreted"),
        "/help must document how a mirrored row reads its arguments; \
         output:\n{session}"
    );
    assert!(
        session.contains("//text sends text as a prompt with one leading slash"),
        "/help must still document the // escape; output:\n{session}"
    );
    assert_no_turn_ran(&session, "`/help`");
}

// ---------------------------------------------------------------------------
// REQ-582 AC-11 — a write row meets the daemon's presence gate exactly as its
// shell twin does
// ---------------------------------------------------------------------------
//
// BR-6's claim is that the write rows send the *same* `config/set` params their
// twins send, so every daemon-side gate applies to them unchanged. The gate
// worth proving that against is REQ-576's presence attestation, because it is
// the one that refuses a payload the client has already accepted — and because
// "nothing was written" is a claim about a file, which only a real daemon with a
// real config on disk can settle (LESSON-519: inspect, do not infer).
//
// The pair is deliberate (LESSON-520): the refusing test's byte-identical
// assertion is only worth something because the accepting test proves the very
// same line does write when presence is satisfied.
//
// **Not feature-gated, and why.** `TETON_PRESENCE_ACCEPT` installs a verifier in
// place of whatever the build has (`tetond`'s `seam_verifier`), so a default
// build driven through it takes the same `config/set` path a `--features
// presence` build takes with a real mechanism — which is exactly how
// `tetond/tests/config_set_attestation.rs` drives the same gate. The seam rides
// `TETON_TEST_SEAMS`, and a release build refuses to start when that is set, so
// none of this exists in a shipped binary.

/// The daemon's own sentence for a presence-gated `config/set` it refused,
/// passed through by the client rather than paraphrased (LESSON-456 — the daemon
/// classifies, the client renders). The client's own half of the line (`the
/// 'build' tier was not bound: …`) is asserted beside it.
const ATTESTATION_REFUSED: &str = "the presence check did not pass";

/// **AC-11 (refused): a presence-refused `/policy set-tier` leaves `config.toml`
/// byte-identical, and says what the shell twin says.**
#[test]
fn a_presence_refused_session_set_tier_leaves_the_config_untouched() {
    let daemon_path = daemon_bin();
    let daemon = TestDaemon::spawn_scripted_with_presence(&daemon_path, TURN_REPLIES, "fail");
    let teton = teton_bin();
    let config_path = daemon.root.join("config.toml");

    // Read the baseline only after a client round-trip has completed: a starting
    // daemon normalises its own config once (the REQ-557 model migration), so
    // bytes read before that would report a write this test never made.
    let _ = daemon.run_cli_stdout(&teton, &["policy", "show"]);
    let before = std::fs::read(&config_path).expect("the fixture config exists");

    let session = daemon.run_cli_seamed(&teton, &[], "/policy set-tier build deepseek\n");
    let shell = daemon.run_cli(&teton, &["policy", "set-tier", "build", "deepseek"]);

    assert!(
        session.contains("the 'build' tier was not bound"),
        "a refused `/policy set-tier` must report the refusal in the client's \
         own sentence; output:\n{session}\ndaemon log:\n{}",
        daemon.log()
    );
    assert!(
        session.contains(ATTESTATION_REFUSED),
        "the refusal must be the daemon's attestation refusal, passed through \
         rather than paraphrased; output:\n{session}"
    );
    // The same gate, the same words, from the shell — which is BR-6's whole
    // claim: the row sends what the twin sends, so it meets what the twin meets.
    assert!(
        shell.contains("the 'build' tier was not bound") && shell.contains(ATTESTATION_REFUSED),
        "`teton policy set-tier` must meet the same gate; output:\n{shell}"
    );

    // AC-11's load-bearing half: the file, not the error.
    assert_eq!(
        std::fs::read(&config_path).ok().as_deref(),
        Some(before.as_slice()),
        "a presence-refused `/policy set-tier` must leave config.toml \
         byte-identical"
    );
    // And the running config agrees with the file.
    let policy = daemon.run_cli_stdout(&teton, &["policy", "show"]);
    let build_row = policy
        .lines()
        .find(|line| line.trim_start().starts_with("build "))
        .unwrap_or_else(|| panic!("no build row:\n{policy}"));
    assert!(
        !build_row.contains("→ deepseek"),
        "the refused binding reached the running config; row: {build_row:?}"
    );
}

/// **AC-11 (attested): the same line writes when presence is satisfied.**
///
/// The non-vacuity anchor for the test above — a payload that could never apply
/// would leave `config.toml` untouched whatever the gate did.
#[test]
fn an_attested_session_set_tier_writes() {
    let daemon_path = daemon_bin();
    let daemon = TestDaemon::spawn_scripted_with_presence(&daemon_path, TURN_REPLIES, "1");
    let teton = teton_bin();
    let config_path = daemon.root.join("config.toml");

    let _ = daemon.run_cli_stdout(&teton, &["policy", "show"]);
    let before = std::fs::read(&config_path).expect("the fixture config exists");

    let session = daemon.run_cli_seamed(&teton, &[], "/policy set-tier build deepseek\n");

    assert!(
        session.contains("the 'build' tier now routes to `deepseek`."),
        "an attested `/policy set-tier` must apply; output:\n{session}\n\
         daemon log:\n{}",
        daemon.log()
    );
    assert_ne!(
        std::fs::read(&config_path).ok().as_deref(),
        Some(before.as_slice()),
        "the attested `/policy set-tier` must actually change config.toml (so \
         the refused test's byte-identical assertion means something)"
    );
    let policy = daemon.run_cli_stdout(&teton, &["policy", "show"]);
    let build_row = policy
        .lines()
        .find(|line| line.trim_start().starts_with("build "))
        .unwrap_or_else(|| panic!("no build row:\n{policy}"));
    assert!(
        build_row.contains("→ deepseek"),
        "the attested binding never reached the running config; \
         row: {build_row:?}"
    );
}
