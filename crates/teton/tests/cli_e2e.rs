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

    /// A scripted daemon whose environment carries `extra_env` — REQ-583's
    /// tests hand it a `HOME` under the fixture root, so "the home folder" is a
    /// directory the test made rather than the developer's own, and the CLI it
    /// drives is given the same value (`run_cli_from`).
    fn spawn_scripted_with_env(
        daemon: &Path,
        replies: &[&str],
        extra_env: &[(&str, &str)],
    ) -> Self {
        Self::spawn_with_script_env(daemon, Some(replies), "", extra_env)
    }

    fn spawn_with_script(daemon: &Path, replies: Option<&[&str]>, extra_config: &str) -> Self {
        Self::spawn_with_script_env(daemon, replies, extra_config, &[])
    }

    /// A scripted daemon whose config carries a `[skills]` table naming the
    /// project root this test is about (REQ-589 D-13).
    ///
    /// The config is built from a **closure over the fixture root**, which is the
    /// only shape that works here: `[skills] trusted_project_roots` holds the
    /// *canonical* name of a directory, the fixture's directory lives under the
    /// root this function is about to mint, and the daemon reads its config once
    /// at start. So the row cannot be written before the root exists and cannot
    /// be added after the daemon has read it — it has to be composed in between,
    /// which is exactly where this hook sits.
    fn spawn_scripted_trusting(
        daemon: &Path,
        replies: &[&str],
        extra_env: &[(&str, &str)],
        config_for_root: &dyn Fn(&Path) -> String,
    ) -> Self {
        Self::spawn_for_root(daemon, Some(replies), config_for_root, extra_env)
    }

    fn spawn_with_script_env(
        daemon: &Path,
        replies: Option<&[&str]>,
        extra_config: &str,
        extra_env: &[(&str, &str)],
    ) -> Self {
        Self::spawn_for_root(daemon, replies, &|_| extra_config.to_owned(), extra_env)
    }

    fn spawn_for_root(
        daemon: &Path,
        replies: Option<&[&str]>,
        extra_config: &dyn Fn(&Path) -> String,
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
                 {}",
                closed_port(),
                extra_config(&root)
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
            // REQ-611 TASK-364: `socket_path::resolve_data_dir` falls back to
            // the developer's own `~/Library/Application Support/teton` when
            // this is unset, and every daemon prunes its transcript directory
            // at start — so an unset variable makes this fixture run a deletion
            // pass over the machine it is testing on. Under `root`, which
            // `Drop` removes with everything else.
            .env("XDG_DATA_HOME", root.join("d"))
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
        self.run_cli_process(teton, args, stdin, seams, None, &[])
    }

    /// As [`Self::run_cli_streams`], with the CLI's working directory and extra
    /// environment under the test's control.
    ///
    /// REQ-583 is the first thing here that cares where the CLI *runs from*: a
    /// relative `--cwd` joins onto the process's directory, and `~` — in `--cwd`
    /// and in `/cd` — is `HOME`, which the daemon reads too when it decides a
    /// root is the home folder. Every other runner inherits the test runner's
    /// directory and environment, as it always did.
    fn run_cli_from(
        &self,
        teton: &Path,
        args: &[&str],
        stdin: &str,
        cwd: Option<&Path>,
        env: &[(&str, &Path)],
    ) -> (String, String, std::process::ExitStatus) {
        self.run_cli_process(teton, args, stdin, CliSeams::Off, cwd, env)
    }

    fn run_cli_process(
        &self,
        teton: &Path,
        args: &[&str],
        stdin: &str,
        seams: CliSeams,
        cwd: Option<&Path>,
        env: &[(&str, &Path)],
    ) -> (String, String, std::process::ExitStatus) {
        let mut command = Command::new(teton);
        command
            .args(args)
            .env("XDG_RUNTIME_DIR", &self.runtime_dir)
            // REQ-611 TASK-364: the CLI autostarts a daemon when it cannot
            // reach one, and that child inherits this environment — so the
            // isolation has to travel with the *client* too, or an autostarted
            // daemon prunes the developer's real data directory. Same value the
            // fixture daemon was spawned with, so both see one directory.
            .env("XDG_DATA_HOME", self.root.join("d"))
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        if let Some(cwd) = cwd {
            command.current_dir(cwd);
        }
        for (key, value) in env {
            command.env(key, value);
        }
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
        // A CLI that exits before it reads anything — a `--cwd` refused on the
        // spot (REQ-583 BR-6) — may already have closed its end of the pipe
        // when this write lands; `EPIPE` is then the expected shape of that
        // early exit, not a fixture failure (the ubuntu CI leg hit the race).
        // Every other write error still fails the test, and a process that
        // wrongly read nothing fails its own output assertions.
        if let Err(err) = child
            .stdin
            .take()
            .expect("piped stdin")
            .write_all(stdin.as_bytes())
        {
            assert_eq!(
                err.kind(),
                std::io::ErrorKind::BrokenPipe,
                "write teton stdin {args:?}: {err}"
            );
        }
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
        // REQ-611 TASK-364: this command autostarts a daemon, and that
        // daemon prunes its transcript directory at start — under `root`, not
        // under the developer's home (`resolve_data_dir`'s fallback).
        .env("XDG_DATA_HOME", root.join("d"))
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

/// The distinctive phrases of every line `session_ui::format_pressure` can draw
/// (REQ-586 BR-7).
///
/// The two assertions below originally matched the bare `context: ` label, which
/// was a sound proxy while the pressure lines were the only ungated members of
/// that family. REQ-613 gave the label a second, unrelated one — the generation
/// news, which prints `context: no TETON.md was written …` in a **quiet**
/// session by design (BR-2/BR-9: a file the user might have expected is not
/// there, and each reason sends them somewhere different). Every session in this
/// suite runs on a pipe at a project root, so that line is now ordinary output
/// and the prefix no longer distinguishes "was anything clamped" from "was
/// anything said".
///
/// So the needle is what the assertion was always about: the clamp sentences
/// themselves. Narrower, not weaker — a pressure line that appeared in a quiet
/// segment still reddens this, which is the whole claim.
const CLAMP_PHRASES: &[&str] = &[
    "dropped to fit the",
    "middle-elided by",
    "re-fitted to the",
    "could not be fitted to the",
    "adjusted to fit the",
];

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
    let daemon_bin = daemon_bin();
    let teton = teton_bin();

    // An **empty** fixture HOME, handed to both processes. Without it this test
    // reads whatever `~/.claude` the machine running it happens to have: on the
    // dogfood machine that is seventeen ADLC skills, on CI it is none, and a
    // developer with one malformed skill would see the diagnostic line change
    // under a test that never mentions skills. The assertions below are about
    // the *built-in* listing, so the registry they run against has to be a
    // fact about the fixture rather than about the machine (LESSON-540).
    let empty_home = SkillTree::new("nohome");
    let daemon = TestDaemon::spawn_scripted_with_env(
        &daemon_bin,
        TURN_REPLIES,
        &[("HOME", empty_home.path().to_str().unwrap())],
    );

    let session = daemon
        .run_cli_from(
            &teton,
            &[],
            "/frobnicate\n/help\n",
            None,
            &[("HOME", empty_home.path())],
        )
        .0;

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
        // REQ-611 AC-5: the session-lifetime transcript switch is findable.
        (
            "/transcript",
            "Record this session to a file, or stop: /transcript [on|off]; bare, show the state.",
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
    // REQ-586 BR-7/AC-10, the negative half: a short turn clamps nothing, so
    // there is no pressure line to draw. The positive half — a turn that really
    // does drop blocks draws exactly one — is the daemon's emission to prove
    // and rides its own fixture; what this pins is that the never-gated line is
    // not chatter on every turn.
    assert!(
        !CLAMP_PHRASES.iter().any(|phrase| quiet.contains(phrase)),
        "a turn that clamped nothing must say nothing about context; segment:\n{quiet}"
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
    // REQ-586 BR-9/AC-4: the route notice carries the budget that turn ran
    // under and what bound it. The local tier is the route here, so the bound
    // is the local engine's — the one case where a declared window would not
    // change the answer.
    let route = loud
        .lines()
        .find(|line| line.contains("route ["))
        .unwrap_or_else(|| panic!("the verbose segment has no route line:\n{loud}"));
    assert!(
        route.contains("; budget ") && route.contains(" words / "),
        "the budget rides the route line, in both currencies; line: {route:?}"
    );
    // REQ-616 BR-6: with the engine's window in front of it. No engine is
    // loaded in this fixture, so the window is the no-engine default — which is
    // exactly the compatibility property ADR-616-1 rests on.
    assert!(
        route.contains(" · window 32,768 tokens; budget "),
        "the route line names the engine's window before the budget derived \
         from it; line: {route:?}"
    );
    assert!(
        route.ends_with("(bound: local engine)"),
        "a turn on the local tier names the local engine as its bound; line: {route:?}"
    );

    // Turn three, after the second toggle: quiet again — the toggle flips back,
    // it does not latch.
    assert!(
        !quiet_again.contains("route [") && !quiet_again.contains("turn ended"),
        "a second `/verbose` must hide the notices again; segment:\n{quiet_again}"
    );
    assert!(
        !CLAMP_PHRASES
            .iter()
            .any(|phrase| quiet_again.contains(phrase)),
        "still nothing to clamp, so still nothing to say; segment:\n{quiet_again}"
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

// ---------------------------------------------------------------------------
// REQ-586: the context window on the provider surfaces, and the budget on a turn
// ---------------------------------------------------------------------------

/// A remote provider that declares a 128k window, and one that declares a
/// window with a cap sitting at it — the two shapes the listing and doctor say
/// different things about. Written the way a user writes them, as a
/// `[providers.capabilities]` sub-table.
fn windowed_provider_config() -> String {
    "[[providers]]\nid = \"windowed\"\nkind = \"openai-compatible\"\n\
     endpoint = \"https://api.example.invalid/v1/chat/completions\"\n\
     model = \"windowed-model\"\n[providers.capabilities]\nmax_context = 128000\n\n\
     [[providers]]\nid = \"capped\"\nkind = \"anthropic\"\n\
     endpoint = \"https://api.example.invalid/v1/messages\"\n\
     model = \"capped-model\"\n[providers.capabilities]\n\
     max_context = 200000\ncontext_budget_cap = 200000\n\n"
        .to_owned()
}

/// **AC-4 through the shipped binary, on both surfaces.**
///
/// Every provider row names its context window, and the three answers are three
/// different sentences: a declared window, an undeclared one that says the
/// budget is defaulted and which key fixes it (BR-3 — stated, never silent),
/// and the local tier, whose budget is the local engine's whatever any window
/// says and which therefore has nothing to set.
///
/// Asserted on `/provider list` **and** `teton provider list` because the claim
/// is about the shipped renderer both reach (REQ-582's one-renderer rule); the
/// byte-for-byte half of that is
/// `every_read_row_prints_exactly_what_its_shell_twin_prints`, which stays green
/// for the same reason.
///
/// The fourth state — `window: not reported`, an absent field from a daemon
/// older than it — has no fixture here: the shipped daemon always populates the
/// field, so it is pinned as a `render_config` unit test instead.
#[test]
fn every_provider_row_names_its_window_on_both_surfaces() {
    let daemon_path = daemon_bin();
    // Scripted, because the session half is a **pipe**: a daemon with no engine
    // leaves the first-run proposal outstanding, and the first piped line would
    // be eaten answering it rather than reaching the entry loop.
    let daemon = TestDaemon::spawn_scripted_with_config(
        &daemon_path,
        TURN_REPLIES,
        &windowed_provider_config(),
    );
    let teton = teton_bin();

    let shell = daemon.run_cli(&teton, &["provider", "list"]);
    let session = daemon.run_cli_with_stdin(&teton, &[], "/provider list\n");

    for (what, listing) in [
        ("teton provider list", &shell),
        ("/provider list", &session),
    ] {
        let row = |id: &str| -> &str {
            listing
                .lines()
                .find(|line| line.contains(&format!("  {id} [")))
                .unwrap_or_else(|| panic!("{what} printed no row for `{id}`:\n{listing}"))
        };
        assert!(
            row("windowed").ends_with("window: 128k"),
            "{what}: a declared window must be shown:\n{listing}"
        );
        assert!(
            row("deepseek").ends_with(
                "window: unknown — context budget defaulted (set capabilities.max_context)"
            ),
            "{what}: BR-3 — an unknown window is stated, with the key that fixes it:\n{listing}"
        );
        assert!(
            row("local").ends_with("(local engine)") && !row("local").contains("unknown"),
            "{what}: the local tier's budget is its own, so it has no unknown window to \
             report:\n{listing}"
        );
    }
}

/// **AC-4's advisory half, end to end, and BR-5's inert cap beside it.**
///
/// Doctor is where a user goes with "why is this provider only being sent 4k?",
/// so the two things the listing column can only imply are said outright: an
/// undeclared window means the budget is defaulted and here is the key, and a
/// cap that sits at or above its window never binds. Neither is a fault, and
/// neither may change doctor's exit status — the REQ-578 advisory's posture,
/// one class over.
#[test]
fn doctor_advises_on_an_undeclared_window_and_an_inert_cap_and_stays_green() {
    let daemon_path = daemon_bin();
    let daemon = TestDaemon::spawn_with_config(&daemon_path, &windowed_provider_config());
    let teton = teton_bin();

    let (output, status) = daemon.run_cli_capture(&teton, &["doctor"], "");

    assert!(
        status.success(),
        "an advisory must not change doctor's exit status; output:\n{output}\n\
         daemon log:\n{}",
        daemon.log()
    );
    let advisories: Vec<&str> = output
        .lines()
        .filter(|line| line.contains("context budget") || line.contains("context_budget_cap"))
        .collect();

    assert!(
        advisories
            .iter()
            .any(|line| line.contains("`deepseek`") && line.contains("capabilities.max_context")),
        "a provider with no declared window must be named, with the remedy; \
         advisories:\n{advisories:#?}\nfull output:\n{output}"
    );
    assert!(
        advisories
            .iter()
            .any(|line| line.contains("`capped`") && line.contains("never binds")),
        "a cap at the window is inert and doctor says so; advisories:\n{advisories:#?}\n\
         full output:\n{output}"
    );
    assert!(
        !advisories.iter().any(|line| line.contains("`windowed`")),
        "a provider that declared its window has nothing to act on; \
         advisories:\n{advisories:#?}"
    );
    assert!(
        !advisories.iter().any(|line| line.contains("`local`")),
        "the local tier has no `max_context` worth setting — advising it would send a user to \
         edit a key that changes nothing; advisories:\n{advisories:#?}"
    );
}

/// **AC-5, the CLI half: `--max-context` and `--context-budget-cap` are flags,
/// not hand edits.**
///
/// Both flags in one registration, because the cap has been tested at every
/// seam separately — parse to payload, payload to file, file to bound — and
/// nowhere end to end. A cap that parsed and never reached the record would
/// leave all three legs green.
///
/// Registering a **local** provider because that is the one kind this suite can
/// register end to end: every remote kind reads a credential, and the CLI's
/// keychain is the machine's own (the harness clears `TETON_PROVIDER_KEY` for
/// exactly that reason), so a test that completed a remote registration would
/// write a fake key into the developer's login keychain.
///
/// The evidence is the **daemon's config file**, not the CLI's echo: what BR-3
/// claims is that a window typed on the command line is recorded, and a client
/// sentence about it would be this process agreeing with itself. The listing
/// then shows the local row as `(local engine)`, which is the same claim from
/// the other side — a declared window does not make the local tier's budget
/// anything other than the local engine's.
#[test]
fn provider_add_records_a_declared_window_in_the_daemons_config() {
    let daemon_path = daemon_bin();
    let daemon = TestDaemon::spawn(&daemon_path);
    let teton = teton_bin();

    let (output, status) = daemon.run_cli_capture(
        &teton,
        &[
            "provider",
            "add",
            "on-device",
            "--kind",
            "local",
            "--max-context",
            "128000",
            "--context-budget-cap",
            "40000",
        ],
        "",
    );
    assert!(
        status.success(),
        "a local provider needs no credential, so the flag alone must register; output:\n{output}"
    );

    let written = std::fs::read_to_string(daemon.root.join("config.toml"))
        .expect("the fixture's config is where the daemon writes");
    assert!(
        written.contains("max_context = 128000"),
        "the window typed on the command line must reach the stored record; config:\n{written}"
    );
    assert!(
        written.contains("context_budget_cap = 40000"),
        "the cap typed on the command line must reach the stored record too — it is the \
         cost knob, and a flag that parses without being written is a knob that does \
         nothing; config:\n{written}"
    );

    let listed = daemon.run_cli(&teton, &["provider", "list"]);
    let row = listed
        .lines()
        .find(|line| line.contains("  on-device ["))
        .unwrap_or_else(|| panic!("the new provider is not listed:\n{listed}"));
    assert!(
        row.ends_with("(local engine)"),
        "a local row's budget is the local engine's whatever it declares; row: {row:?}"
    );
}

/// **AC-8 at the CLI: a cap below the window is what `/verbose` names as the
/// bound.**
///
/// The turn is routed to a provider nothing is listening for, and that is fine:
/// the route — and the budget stamped on it — is decided before the call, so
/// the line under test is printed whether or not the provider answers. What
/// matters is that the bound the user reads is `user cap` and not `window`,
/// because those two are the difference between "this is as much as the model
/// takes" and "this is as much as you asked me to send it".
///
/// `[privacy] redact` is off in this fixture, so the redact clamp cannot be
/// what bound the budget — which is the other half of the precedence claim.
#[test]
fn a_cap_below_the_window_is_the_bound_a_verbose_turn_names() {
    let daemon_path = daemon_bin();
    let daemon = TestDaemon::spawn_scripted_with_config(
        &daemon_path,
        TURN_REPLIES,
        &format!(
            "[[providers]]\nid = \"capped\"\nkind = \"openai-compatible\"\n\
             endpoint = \"http://127.0.0.1:{}/v1/chat/completions\"\n\
             model = \"capped-model\"\n[providers.capabilities]\n\
             max_context = 200000\ncontext_budget_cap = 40000\n\n",
            closed_port()
        ),
    );
    let teton = teton_bin();

    let bound = daemon.run_cli(&teton, &["policy", "set-tier", "build", "capped"]);
    assert!(
        bound.contains("capped"),
        "the fixture's tier binding did not move, so the turn below would route \
         elsewhere; output:\n{bound}"
    );

    let session = daemon.run_cli_with_stdin(&teton, &[], "/verbose\nexplain the first thing\n");

    let route = session
        .lines()
        .find(|line| line.contains("route [") && line.contains("capped"))
        .unwrap_or_else(|| {
            panic!(
                "the turn never routed to the capped provider; session:\n{session}\n\
                 daemon log:\n{}",
                daemon.log()
            )
        });
    assert!(
        route.contains("(bound: user cap)"),
        "AC-8: a cap below the window is what bound the budget, and the line must say so \
         rather than naming the window; line: {route:?}"
    );
    assert!(
        route.contains("; budget ") && route.contains(" words / "),
        "BR-9: the budget is printed in both currencies — the byte guard is what binds on a \
         remote route, so the word figure alone would overstate what fits; line: {route:?}"
    );
    // REQ-616 BR-6/AC-2: and the **window** it was derived from comes first, in
    // the provider's own tokens. This route is capped, so the window named is
    // the cap (40,000) rather than the declaration it was cut from — the cap is
    // a window ceiling and the pair is recomputed from it.
    assert!(
        route.contains(" · window 40,000 tokens; budget "),
        "the window this route ran under is named before the budget derived from \
         it, so 25,984 words cannot read as a shrunken window (LESSON-446); \
         line: {route:?}"
    );
}

/// **TASK-194 (OQ-6 as amended) at the CLI: a *local* row's declared window is
/// never stated as a cost, however large.**
///
/// The notice this REQ adds names a per-call budget and a 25-call worst case,
/// and both are facts about a **remote** route. A `kind = "local"` entry runs
/// the engine on this machine under the local pair whatever `max_context` says
/// — which is why `provider list` renders it `(local engine)` and why doctor
/// does not advise it to declare a window. Printing "every call may carry
/// 665,984 words … at worst" for it would be the exact class of untruth this
/// task is closing, and it would name a spend where nothing is spent.
///
/// This suite can only complete a **local** registration end to end — every
/// remote kind reads a credential, and the CLI's keychain is the machine's own
/// (the harness clears `TETON_PROVIDER_KEY` for exactly that reason), so a test
/// that registered a remote provider would write a fake key into the
/// developer's login keychain. So the negative is what is drivable here, and it
/// is the leg worth having: the positive path is asserted against a real daemon
/// over the real socket in `tetond`'s `provider_setup_flow`
/// (`a_recorded_big_window_is_stated_once_and_in_the_same_words_on_both_surfaces`,
/// which also pins the two surfaces byte-identical) and the rendering in
/// `main.rs`'s `a_recorded_big_window_prints_the_daemons_notice_once`.
#[test]
fn provider_add_states_no_per_call_cost_for_a_local_rows_declared_window() {
    let daemon_path = daemon_bin();
    let daemon = TestDaemon::spawn(&daemon_path);
    let teton = teton_bin();

    let (output, status) = daemon.run_cli_capture(
        &teton,
        &[
            "provider",
            "add",
            "wide",
            "--kind",
            "local",
            "--max-context",
            "1000000",
        ],
        "",
    );
    assert!(
        status.success(),
        "a local provider needs no credential; output:\n{output}"
    );
    assert!(
        output.contains("provider `wide` registered"),
        "the registration itself must still be reported:\n{output}"
    );
    assert!(
        !output.contains("context window is recorded"),
        "a local row spends nothing per call, so nothing may be said about what it \
         spends:\n{output}"
    );

    // The window is still *recorded* — the notice's absence is about what is
    // said, not about what is stored (BR-3 is unchanged).
    let written = std::fs::read_to_string(daemon.root.join("config.toml"))
        .expect("the fixture's config is where the daemon writes");
    assert!(
        written.contains("max_context = 1000000"),
        "the window typed on the command line still reaches the record:\n{written}"
    );
    assert!(
        !written.contains("context_budget_cap"),
        "and nothing was capped: the declaration is still the consent:\n{written}"
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
/// REQ-611 AC-20: `teton doctor` prints exactly one `transcript:` posture
/// line, and `teton transcript status` prints the same line. The fixture
/// config names no `[transcript]` table, so the default is off and the
/// directory is the isolated `XDG_DATA_HOME`.
///
/// **Mutation (run 2026-09-03):** removing the `render_transcript_posture`
/// call from `doctor_report_on` reddened the `== 1` count; restored.
#[test]
fn doctor_prints_one_transcript_posture_line() {
    let daemon = daemon_bin();
    let daemon = TestDaemon::spawn_scripted(&daemon, TURN_REPLIES);
    let teton = teton_bin();

    let doctor = daemon.run_cli(&teton, &["doctor"]);
    let posture: Vec<&str> = doctor
        .lines()
        .filter(|line| line.trim_start().starts_with("transcript:"))
        .collect();
    assert_eq!(
        posture.len(),
        1,
        "doctor prints exactly one transcript posture line; output:\n{doctor}"
    );
    assert!(
        posture[0].contains("off by default") && posture[0].contains("kept 30 days"),
        "the line names the default and the retention; got: {}",
        posture[0]
    );
    assert!(
        posture[0].contains("transcripts"),
        "the line names the effective directory; got: {}",
        posture[0]
    );

    let status = daemon.run_cli(&teton, &["transcript", "status"]);
    assert!(
        status.lines().any(|line| line.trim() == posture[0].trim()),
        "`teton transcript status` prints the doctor's line verbatim; output:\n{status}"
    );
}

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
///
/// Two turns, because the two hand-off lines are armed by different replies
/// (verify T14, tightened at the re-verify): the first reply recites the
/// REQ-579 setup recipe (`teton provider add`, `teton policy set-tier`), which
/// at a terminal earns the *setup* line and — since that line goes first and
/// at most one prints — could never have earned the REQ-582 generic line, so
/// asserting the generic prefix absent on that turn alone proved nothing about
/// the generic gate. The second reply recites two mirrored rows that are **not**
/// setup recipes (`teton provider list`, `teton doctor`), which is exactly what
/// arms the generic line at a terminal; a pipe must see it on neither turn.
#[test]
fn a_piped_session_whose_reply_recites_the_cli_gets_no_hand_off_line() {
    let daemon_path = daemon_bin();
    // Turn one: the reply the guide actually produces at the front door — the
    // recital three live rounds recorded (verification.md §1–§24).
    let setup_reply = "register it from a shell: teton provider add kimi --kind \
                       openai-compatible, then teton policy set-tier think kimi.";
    // Turn two: an inspect answer that names two mirrored rows by their shell
    // spelling and neither by its `/` form — the generic line's arming shape.
    let inspect_reply = "run teton provider list to see what is registered, and \
                         teton doctor for the daemon.";
    let daemon = TestDaemon::spawn_scripted(&daemon_path, &[setup_reply, inspect_reply]);
    let teton = teton_bin();

    let (session, status) = daemon.run_cli_capture(
        &teton,
        &[],
        "how do I add kimi?\n\
         what is configured?\n",
    );

    // The precondition: both turns ran and each reply really did recite the
    // CLI. Without this the assertions below would pass on a session that
    // never reached the model at all.
    assert!(
        session.contains("teton provider add kimi"),
        "the setup reply must have reached the transcript, or this test \
         proves nothing; output:\n{session}\ndaemon log:\n{}",
        daemon.log()
    );
    assert!(
        session.contains("teton provider list") && session.contains("teton doctor"),
        "the inspect reply must have reached the transcript, or the generic \
         line's negative proves nothing; output:\n{session}\ndaemon log:\n{}",
        daemon.log()
    );

    // And every hand-off is absent. Asserted on the sentences' distinctive
    // halves rather than the whole lines, so a future rewording cannot make
    // this pass by accident: the REQ-579 setup line's three halves (armed by
    // turn one), and the REQ-582 generic line's prefix (armed by turn two).
    for absent in [
        "does this without leaving it",
        "no key in chat",
        "/provider setup <vendor> [tier]",
        "in this session:",
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

/// **`teton provider --help` typed at the prompt is the family's own help page
/// (verify T6; the entry loop's `Input::CliHelp` arm, re-verify Q1).**
///
/// The classifier's unit tests pin what the line classifies *to*; this is the
/// leg that proves the entry loop renders that outcome — the shipped binary,
/// piped, printing clap's page for the family as information: the `Usage:`
/// clause is there, no line of it is an error, no `→ /…` notice was printed
/// (a help page is not a row that ran), and no turn was spent on it.
#[test]
fn a_typed_family_help_request_prints_the_familys_own_page_and_costs_no_turn() {
    let daemon_path = daemon_bin();
    let daemon = TestDaemon::spawn_scripted(&daemon_path, TURN_REPLIES);
    let teton = teton_bin();

    let session = daemon.run_cli_with_stdin(&teton, &[], "teton provider --help\n");
    let body = session_body(&session, "`teton provider --help`");
    let printed = typed_output(body, "`teton provider --help`");

    assert!(
        printed
            .iter()
            .any(|line| line.starts_with("Usage: teton provider")),
        "the family's page must carry the shell's own Usage clause; printed:\n{}\n\
         output:\n{session}",
        printed.join("\n")
    );
    // The page's lines are the shell's, and the shell lists the family's
    // subcommands under it.
    for sub in ["add", "list", "test"] {
        assert!(
            printed
                .iter()
                .any(|line| line.trim_start().starts_with(sub)),
            "the page must list `{sub}`; printed:\n{}",
            printed.join("\n")
        );
    }
    assert!(
        !printed.iter().any(|line| line.starts_with("error:")),
        "asking for help is not an error; printed:\n{}",
        printed.join("\n")
    );
    assert!(
        !session.contains("→ /"),
        "a help page is not a row that ran, so no notice; output:\n{session}"
    );
    assert!(
        !session.contains("names a family rather than a command"),
        "an explicit --help must not get the bare-family refusal; output:\n{session}"
    );
    assert_no_turn_ran(&session, "`teton provider --help`");
}

/// **A recognized `teton …` line meets the same gates its `/` spelling meets
/// (verify m15 / T8).** On a pipe, `teton policy set-tier build deepseek` is
/// refused with the write row's own typed-input line, and `teton model set
/// <name>` with `/model set`'s — the same sentences the `/` spellings print —
/// and nothing changes: no `config/set`, no `model/set`, no turn.
///
/// The recognized spelling is the one a user pastes out of a shell recipe, so
/// it is the spelling most likely to arrive over a pipe; a gate that only the
/// `/` spelling met would be a gate with a second door.
#[test]
fn a_recognized_write_line_on_a_pipe_is_refused_exactly_as_its_slash_spelling() {
    let daemon_path = daemon_bin();
    let daemon = TestDaemon::spawn_scripted(&daemon_path, TURN_REPLIES);
    let teton = teton_bin();

    let policy_before = daemon.run_cli_stdout(&teton, &["policy", "show"]);
    let status_before = daemon.run_cli_stdout(&teton, &["model", "status"]);

    let session = daemon.run_cli_with_stdin(
        &teton,
        &[],
        "teton policy set-tier build deepseek\n\
         teton model set qwen2.5-coder-3b\n",
    );

    // Both lines were recognized: the notice names the session spelling.
    for note in [
        ">> teton policy set-tier → /policy set-tier",
        ">> teton model set → /model set",
    ] {
        assert!(
            session.contains(note),
            "a recognized line must print its notice ({note:?}); output:\n{session}"
        );
    }
    // And each was refused with the row's own sentence — the write row's, and
    // `/model set`'s richer one that names `--yes`.
    let policy_refusals: Vec<&str> = session
        .lines()
        .filter(|line| line.contains("/policy set-tier is typed-input-only"))
        .collect();
    assert_eq!(
        policy_refusals.len(),
        1,
        "`teton policy set-tier` on a pipe must refuse in exactly one line; \
         output:\n{session}"
    );
    assert!(
        policy_refusals[0].contains("teton policy set-tier"),
        "{}",
        policy_refusals[0]
    );
    let model_refusals: Vec<&str> = session
        .lines()
        .filter(|line| line.contains("/model set is typed-input-only"))
        .collect();
    assert_eq!(
        model_refusals.len(),
        1,
        "`teton model set` on a pipe must refuse in exactly one line; output:\n{session}"
    );
    assert!(
        model_refusals[0].contains("--yes") && model_refusals[0].contains("teton model set"),
        "{}",
        model_refusals[0]
    );
    assert_no_turn_ran(&session, "the recognized write lines");

    // Nothing moved, read back through the twins.
    assert_eq!(
        command_lines(&daemon.run_cli_stdout(&teton, &["policy", "show"])),
        command_lines(&policy_before),
        "a refused recognized `policy set-tier` changed the routing table"
    );
    assert_eq!(
        command_lines(&daemon.run_cli_stdout(&teton, &["model", "status"])),
        command_lines(&status_before),
        "a refused recognized `model set` changed the model selection"
    );
}

/// **A recognized line runs end to end on both sides of the gate (verify T10).**
/// `teton doctor` — a read — answers on a pipe with the same report `/doctor`
/// prints; and under the test seam `teton policy set-tier build deepseek`
/// writes, read back through the shell twin. AC-5's parity test covers one read
/// row; this is the second read row and the one write, so recognition is shown
/// to reach the row's *body* and not only its refusal.
#[test]
fn a_recognized_doctor_reads_and_a_seamed_recognized_set_tier_writes() {
    let daemon_path = daemon_bin();
    let daemon = TestDaemon::spawn_scripted(&daemon_path, TURN_REPLIES);
    let teton = teton_bin();

    // The read: `teton doctor` typed at the prompt is `/doctor`, byte for byte
    // after the notice line.
    let slashed = daemon.run_cli_with_stdin(&teton, &[], "/doctor\n");
    let typed = daemon.run_cli_with_stdin(&teton, &[], "teton doctor\n");
    let note = ">> teton doctor → /doctor";
    assert!(typed.contains(note), "output:\n{typed}");
    let slashed_body = session_body(&slashed, "`/doctor`");
    let typed_body = session_body(&typed, "`teton doctor`");
    assert!(
        slashed_body.contains("(this session's connection)"),
        "`/doctor` did not render its report; output:\n{slashed}"
    );
    assert_eq!(
        typed_body.replacen(&format!("{note}\n"), "", 1),
        slashed_body,
        "`teton doctor` and `/doctor` printed different sessions;\n\
         typed:\n{typed}\nslashed:\n{slashed}"
    );
    assert_no_turn_ran(&typed, "a typed `teton doctor`");

    // The write, under the seam the piped harness needs for a typed-input row.
    let before = daemon.run_cli_stdout(&teton, &["policy", "show"]);
    assert!(
        !before.contains("build    → deepseek"),
        "the fixture must start with `build` elsewhere; output:\n{before}"
    );
    let session = daemon.run_cli_seamed(&teton, &[], "teton policy set-tier build deepseek\n");
    assert!(
        session.contains(">> teton policy set-tier → /policy set-tier"),
        "output:\n{session}"
    );
    assert!(
        session.contains("the 'build' tier now routes to `deepseek`."),
        "the recognized write did not report its write; output:\n{session}\n\
         daemon log:\n{}",
        daemon.log()
    );
    assert!(
        !session.contains("typed-input-only"),
        "a seamed session must not meet the write gate; output:\n{session}"
    );
    let after = daemon.run_cli_stdout(&teton, &["policy", "show"]);
    let tier_row = after
        .lines()
        .find(|line| line.trim_start().starts_with("build "))
        .unwrap_or_else(|| panic!("`teton policy show` printed no build row:\n{after}"));
    assert!(
        tier_row.contains("→ deepseek"),
        "the recognized `policy set-tier` did not reach the daemon's routing \
         table; row: {tier_row:?}\nfull:\n{after}"
    );
    assert_no_turn_ran(&session, "a recognized `teton policy set-tier`");
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
// where it does. The composed flow against a double — confirm, read, store,
// `config/set` by reference, and the undo on a refused registration — is
// `main.rs`'s `provider_add_on` tests over `MockKeychain` (verify M1/M4).

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

/// Where `/doctor`'s **second** documented difference begins (REQ-589 BR-13).
///
/// The skill pre-flight is a question *about a session*: which of that
/// session's registered skills will not fit on the route that session is on.
/// A piped session answers it; `teton doctor` owns no session and says so
/// instead of answering about one it picked. Neither is the other's report, so
/// the block is removed from both sides before the diff and each side's own
/// version is then asserted — the same shape [`DOCTOR_DAEMON_LINE`]'s carve-out
/// takes, and for the same reason.
const DOCTOR_SKILLS_LINE: &str = "skills:";

/// Where that block ends: the first line of [`doctor_trailer`], which every
/// report prints and which the pre-flight sits immediately above.
const DOCTOR_TRAILER_LINE: &str = "model: the local-tier lifecycle";

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
        let session_attach = attach_lines(&session);
        assert!(
            !session_attach.is_empty(),
            "the session printed nothing before its entry prompt, so the \
             attach-replay filter below holds over nothing; session:\n{session}"
        );
        let is_daemon_line = |line: &&str| line.starts_with(DOCTOR_DAEMON_LINE);
        let session_lines: Vec<&str> = typed_output(body, &format!("`/{row}`"))
            .into_iter()
            .filter(|line| !is_daemon_line(line))
            .collect();
        // The shell run's **own** attach lines: each line of its stdout that is
        // one of the session's replay lines, taken out once, in the order the
        // shell printed them (verify T15). What is left is the shell's report.
        let mut replay = session_attach.clone();
        let mut shell_attach: Vec<&str> = Vec::new();
        let shell_lines: Vec<&str> = shell_all
            .iter()
            .copied()
            .filter(|line| !is_daemon_line(line))
            .filter(|line| match replay.iter().position(|seen| seen == line) {
                Some(at) => {
                    replay.remove(at);
                    shell_attach.push(line);
                    false
                }
                None => true,
            })
            .collect();
        // REQ-589 BR-13's carve-out, taken before the diff and asserted after
        // it: the pre-flight block runs from the `skills:` line to the trailer
        // the report always ends with, and is removed whole. A row that is not
        // `doctor` must have had **nothing** removed — otherwise the diff ran
        // over two reports with a line quietly taken out of each and would not
        // have noticed.
        fn split_preflight<'a>(lines: &[&'a str]) -> (Vec<&'a str>, Vec<&'a str>) {
            // Notices carry the plain surface's `>> ` prefix and info lines do
            // not, and the two sides disagree about which this is — the session
            // answers (info), the shell declines (notice) — so the marker is
            // matched past the prefix.
            let bare = |line: &str| line.trim_start().trim_start_matches(">> ").to_owned();
            let Some(start) = lines
                .iter()
                .position(|line| bare(line).starts_with(DOCTOR_SKILLS_LINE))
            else {
                return (lines.to_vec(), Vec::new());
            };
            let end = lines[start..]
                .iter()
                .position(|line| bare(line).starts_with(DOCTOR_TRAILER_LINE))
                .map_or(lines.len(), |at| start + at);
            let mut kept = lines[..start].to_vec();
            kept.extend_from_slice(&lines[end..]);
            (kept, lines[start..end].to_vec())
        }
        let (session_lines, session_preflight) = split_preflight(&session_lines);
        let (shell_lines, shell_preflight) = split_preflight(&shell_lines);
        if *row == "doctor" {
            assert!(
                !session_preflight.is_empty(),
                "BR-13: the session's `/doctor` owes a pre-flight answer; \
                 session:\n{session}"
            );
            assert!(
                !shell_preflight.is_empty(),
                "`teton doctor` owes the reason it cannot answer it; shell:\n{shell}"
            );
            assert!(
                session_preflight
                    .iter()
                    .any(|line| line.contains("no route decided yet")
                        || line.contains("dispatchable skill")),
                "a session either names the skills that will not fit or says no \
                 route is decided (ADR-11): {session_preflight:?}"
            );
            assert!(
                shell_preflight
                    .iter()
                    .any(|line| line.contains("no session here")),
                "the shell twin owns no session and must say so rather than \
                 answer about one it picked: {shell_preflight:?}"
            );
        } else {
            assert!(
                session_preflight.is_empty() && shell_preflight.is_empty(),
                "`/{row}` is not `doctor`, so the pre-flight carve-out must \
                 have removed nothing: {session_preflight:?} / {shell_preflight:?}"
            );
        }

        // Multiset equality, both directions — and only **one** of them is an
        // assertion, because the other holds by construction. Session ⊆ shell
        // is the leftovers being empty (below). Shell ⊆ session needs no
        // `sorted_shell == sorted_session` check: a shell line is only ever
        // moved into `shell_attach` by matching a `replay` line, so
        // `shell_attach` is a sub-multiset of `session_attach` before anything
        // is asserted — an equality on the two after `replay.is_empty()` passed
        // would be a tautology, not a second guard (verify residue). What
        // *does* catch an extra replay line on the shell side (a lifecycle
        // event fired between the two runs) is the report diff below: it has
        // nowhere to hide but the shell's report, where the diff fails on it.
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
            // it, on both sides, and would not have noticed. Checked on **both**
            // sides (verify m12): the session's rendering against its unfiltered
            // self, and the shell's against its own line count less the replay.
            assert_eq!(
                session_lines.len(),
                typed_output(body, &format!("`/{row}`")).len(),
                "`/{row}` printed a `{DOCTOR_DAEMON_LINE}…` line, so the \
                 `/doctor` carve-out silently applied to it; session:\n{session}"
            );
            assert_eq!(
                shell_lines.len() + shell_attach.len(),
                shell_all.len(),
                "`teton {}` printed a `{DOCTOR_DAEMON_LINE}…` line, so the \
                 `/doctor` carve-out silently applied to it; shell:\n{shell}",
                argv.join(" ")
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
    // REQ-597: the fixture starts with the *builtin* set in force, so "no
    // boundaries" is no longer the premise — and was never the one this test
    // needs. What the assertion below requires is that `src/**` is not already
    // there; otherwise it would pass without the write row doing anything.
    assert!(
        !boundary_before.contains("src/** [local-only]"),
        "the fixture must not already carry the boundary this test adds, or the \
         assertion below proves nothing; output:\n{boundary_before}"
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
        // REQ-597: the marker that `/boundary list` *answered*. It used to be
        // the empty-set sentence, because the stock config had no boundaries;
        // the builtin set now makes the populated header the evidence. The
        // assertion is unchanged — this row is still about the read answering
        // on a pipe, not about what the list contains.
        "privacy boundaries:",
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
    let teton = teton_bin();

    // Empty fixture HOME, for the reason the unknown-command test above
    // carries: the family grouping asserted here is a property of `COMMANDS`,
    // and it must not be read through whatever skills the running machine
    // happens to have under `~/.claude`.
    let empty_home = SkillTree::new("nohome2");
    let daemon = TestDaemon::spawn_scripted_with_env(
        &daemon_path,
        TURN_REPLIES,
        &[("HOME", empty_home.path().to_str().unwrap())],
    );

    let session = daemon
        .run_cli_from(&teton, &[], "/help\n", None, &[("HOME", empty_home.path())])
        .0;
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
    //
    // **Bounded to the built-in half** (REQ-585 ADR-12). The skills section
    // hangs below the rows and its rows open with `/` too, but a skill is not a
    // family: `/alpha` groups with nothing, and a *shadowed* skill deliberately
    // repeats a built-in row's name — which this walk would read as a family
    // listed twice and fail on. BR-1's grouping is a claim about the **table**,
    // so the walk stops where the table's rows do.
    let mut seen: Vec<String> = Vec::new();
    let mut current: Option<String> = None;
    for line in listing.iter().take_while(|line| **line != SKILLS_HEADER) {
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

// ---------------------------------------------------------------------------
// REQ-583 — the session root: `--cwd`, `/cd`, and the lines they draw
// ---------------------------------------------------------------------------
//
// The daemon's halves of AC-9 and AC-10 — the derived root on `session/create`,
// the jail moving under `session/set_cwd`, the two events on the wire ahead of
// the answer, a `read` refused in the BR-2 shape, at every permission level —
// are `tetond`'s `tests/e2e/session_root.rs`, on captured request bodies. What
// only the shipped binary can settle is the **user's** side of the same story:
// that `teton --cwd` scopes the session to the directory named, that a refused
// `--cwd` is one line and an error exit with no session behind it, that `/cd`
// draws the clear line and the new root in that order and refuses naming the
// path, that a relative `--cwd` is relative to the shell and `~` is `HOME`, and
// that none of it puts the launch notice on a pipe. Every root here is a
// directory the test made under the daemon's own root, and every "home" is a
// `HOME` the test set — the developer's real home is never the fixture.

/// One tool call: a `read` of `path` (absolute, so the same call is admitted
/// under the root that contains it and refused under one that does not).
fn read_call(path: &Path) -> String {
    format!(
        "{{\"tool\": \"read\", \"arguments\": {{\"path\": \"{}\"}}}}",
        path.display()
    )
}

/// The lines a `session_root_changed` event and a bare `/cd` draw
/// (`session_ui::format_session_root_changed`, `slash::current_root_line`).
const ROOT_MOVED: &str = "session root is now ";
const ROOT_LINE: &str = "session root: ";

/// The head of the launch notice (`banner::root_notice`) — TTY-only bytes.
const NOT_A_PROJECT: &str = "Not inside a project";

/// A project fixture (holds a `Cargo.toml`) and a plain one beside it, under
/// `root`; the project holds `notes.txt` for the read legs.
fn root_fixtures(root: &Path) -> (PathBuf, PathBuf) {
    let project = root.join("proj");
    let plain = root.join("plain");
    std::fs::create_dir_all(&project).unwrap();
    std::fs::create_dir_all(&plain).unwrap();
    std::fs::write(project.join("Cargo.toml"), "[package]\nname = \"proj\"\n").unwrap();
    std::fs::write(project.join("notes.txt"), "alpha\n").unwrap();
    (project, plain)
}

/// **AC-9 / AC-10 through the binary: `--cwd` scopes the session, `/cd` moves
/// it — clear line, then the new root — and a refused `/cd` moves nothing.**
///
/// One session, in order: a bare `/cd` names the `--cwd` root and its kind; a
/// scripted `read` of a file under it runs (`[done]`); `/cd <plain>` draws
/// `context cleared; N …` **then** `session root is now <plain> (not a
/// project)`; a bare `/cd` now names the new root; the *same* absolute `read`
/// is refused (`[failed]`) — the file is still there, the jail moved (the BR-2
/// refusal's bytes are asserted at the daemon, `tests/e2e/session_root.rs`;
/// the CLI shows the status the model saw); `/cd /nope` prints the daemon's
/// refusal naming the path and clears nothing; a bare `/cd` still names the
/// plain root; then the **relative** leg (AC-12's `rel` spelling, end to end):
/// `/cd proj` — a name under the CLI process's own working directory, which is
/// the fixture — resolves against that directory, not the session's root,
/// moves back to the project (clear line, `session root is now <project>
/// (project proj)`), and a last bare `/cd` names it. And no launch notice
/// anywhere: stdout is a pipe.
#[test]
fn cwd_scopes_the_session_and_slash_cd_moves_it_and_reports_each_step() {
    let daemon_bin = daemon_bin();
    // The scripted read names an absolute path, so the fixture has to exist
    // before the daemon's script is written — and the daemon's own root is
    // minted inside `spawn`. The fixture therefore lives beside it under
    // `/tmp`, named from the same pid, and is removed at the end. Spelled
    // canonically (on macOS `/tmp` is a link to `/private/tmp`), because the
    // relative leg below resolves against the CLI's working directory as the
    // OS reports it, and the lines it draws are compared to the fixture's own
    // spelling.
    let fixture = PathBuf::from("/tmp").join(format!("tcroot{:x}", std::process::id() & 0xffff));
    let _ = std::fs::remove_dir_all(&fixture);
    std::fs::create_dir_all(&fixture).unwrap();
    let fixture = fixture.canonicalize().unwrap();
    let (project, plain) = root_fixtures(&fixture);
    let notes = project.join("notes.txt");
    let read_notes = read_call(&notes);
    let daemon = TestDaemon::spawn_scripted(
        &daemon_bin,
        &[
            &read_notes,
            "scripted-turn-one complete.",
            &read_notes,
            "scripted-turn-two complete.",
        ],
    );
    let teton = teton_bin();

    // Run *from* the fixture, so `/cd proj` has a working directory to be
    // relative to that is not the session root (the project) and not the
    // test runner's.
    let (stdout, stderr, _status) = daemon.run_cli_from(
        &teton,
        &["--cwd", project.to_str().unwrap()],
        &format!(
            "/cd\n\
             read the notes\n\
             /cd {plain}\n\
             /cd\n\
             read the notes again\n\
             /cd /nope\n\
             /cd\n\
             /cd proj\n\
             /cd\n",
            plain = plain.display()
        ),
        Some(&fixture),
        &[],
    );
    let session = format!("{stdout}{stderr}");
    let _ = std::fs::remove_dir_all(&fixture);
    let log = daemon.log();

    // Both turns ran — otherwise the `[done]`/`[failed]` pair below is about
    // nothing.
    for reply in ["scripted-turn-one complete.", "scripted-turn-two complete."] {
        assert!(
            session.contains(reply),
            "the turn ending `{reply}` never completed; output:\n{session}\nlog:\n{log}"
        );
    }

    let body = session_body(&session, "the --cwd session");
    let project_line = format!("{ROOT_LINE}{} (project proj)", project.display());
    let plain_line = format!("{ROOT_LINE}{} (not a project)", plain.display());
    let moved_line = format!("{ROOT_MOVED}{} (not a project)", plain.display());
    let moved_back_line = format!("{ROOT_MOVED}{} (project proj)", project.display());
    let read_done = format!("read {} [done]", notes.display());
    let read_failed = format!("read {} [failed]", notes.display());
    let refusal =
        "the session root could not be moved: path `/nope` does not exist or is not a directory";

    let at = |needle: &str| {
        body.find(needle).unwrap_or_else(|| {
            panic!("`{needle}` never printed; session body:\n{body}\nlog:\n{log}")
        })
    };
    // (a) the bare form names the `--cwd` root, kind and all.
    let first_bare = at(&project_line);
    // The control read ran under the old root.
    let done = at(&read_done);
    // (c) the move: the clear line, then the new root, in that order.
    let cleared = at(CLEAR_MARKER);
    let moved = at(&moved_line);
    let second_bare = body[moved..]
        .find(&plain_line)
        .map(|i| moved + i)
        .unwrap_or_else(|| panic!("no bare `/cd` line after the move; session body:\n{body}"));
    // The same read is now refused: the file did not move, the jail did.
    let failed = at(&read_failed);
    // (d) the refused move names the path …
    let refused = at(refusal);
    // … and the root stayed where it was.
    let third_bare = body[refused..]
        .find(&plain_line)
        .map(|i| refused + i)
        .unwrap_or_else(|| {
            panic!("no bare `/cd` line after the refused move; session body:\n{body}")
        });
    // (e) the relative leg: `/cd proj` resolved against the CLI's working
    // directory (the fixture) — not against the session's root, which was the
    // plain directory and holds no `proj` — and moved back to the project …
    let moved_back = body[third_bare..]
        .find(&moved_back_line)
        .map(|i| third_bare + i)
        .unwrap_or_else(|| {
            panic!("`/cd proj` did not move back to the project; session body:\n{body}")
        });
    // … which the last bare `/cd` names.
    let fourth_bare = body[moved_back..]
        .find(&project_line)
        .map(|i| moved_back + i)
        .unwrap_or_else(|| {
            panic!("no bare `/cd` line after the relative move; session body:\n{body}")
        });
    assert!(
        first_bare < done
            && done < cleared
            && cleared < moved
            && moved < second_bare
            && second_bare < failed
            && failed < refused
            && refused < third_bare
            && third_bare < moved_back
            && moved_back < fourth_bare,
        "the lines are out of order; session body:\n{body}"
    );

    // Exactly two clears — one per accepted move — and the first dropped the
    // turn that ran before it: `/cd <plain>` cleared, `/cd /nope` did not
    // (validate before mutate), `/cd proj` cleared again.
    let counts = clear_counts(&session);
    assert_eq!(
        counts.len(),
        2,
        "two accepted moves, two clear lines — a refused move must not clear; \
         output:\n{session}"
    );
    assert!(
        counts[0] > 0,
        "the move cleared nothing, so the turn before it retained nothing and the \
         `context cleared` line is not about a conversation; output:\n{session}"
    );
    assert_eq!(
        body.matches(ROOT_MOVED).count(),
        2,
        "two accepted moves draw two `session root is now` lines; body:\n{body}"
    );
    assert!(
        !body.contains(&format!("{ROOT_LINE}/nope"))
            && !body.contains(&format!("{ROOT_MOVED}/nope")),
        "a refused `/cd` must never be reported as the root; body:\n{body}"
    );
    // The project root was named twice (before the first move, and after the
    // relative move back) — a bare `/cd` in between names the plain root,
    // twice.
    assert_eq!(body.matches(&project_line).count(), 2, "body:\n{body}");
    assert_eq!(body.matches(&plain_line).count(), 2, "body:\n{body}");
    // The read's title carries the same path both times: the file did not
    // change, and the second status is the jail's verdict.
    assert_eq!(body.matches(&read_done).count(), 1, "body:\n{body}");
    assert_eq!(body.matches(&read_failed).count(), 1, "body:\n{body}");

    // BR-5's bytes are TTY-only: a pipe never carries the launch notice, even
    // for a plain root (ADR-5) — the move to one drew its two lines and nothing
    // more.
    assert!(
        !session.contains(NOT_A_PROJECT),
        "the not-a-project notice reached a pipe; output:\n{session}"
    );
    // The commands themselves cost no turn: only the two scripted reads did.
    assert_eq!(
        session.matches("scripted-turn-").count(),
        2,
        "a `/cd` reached the model; output:\n{session}"
    );
}

/// **AC-9 / BR-6: `teton --cwd /nope` is one refusal naming the path, an error
/// exit, and no session output — never a session that starts and then fails on
/// every tool.**
///
/// Since the verify pass (finding V) the CLI refuses this itself, before it
/// connects — the same sentence the daemon's validator would answer with, so a
/// script reads one line either way — which is why the assertion below is on
/// the sentence and not on which process wrote it. The daemon stays the
/// authority for the RPC (`tetond`'s e2e pins its refusal for a `cwd` sent on
/// the wire); a daemon is still spawned here so a CLI that *did* connect
/// would be caught doing so by the no-session-output assertions.
#[test]
fn a_cwd_that_does_not_exist_is_refused_before_any_session_output_and_exits_non_zero() {
    let daemon_bin = daemon_bin();
    let daemon = TestDaemon::spawn_scripted(&daemon_bin, TURN_REPLIES);
    let teton = teton_bin();

    let (stdout, stderr, status) =
        daemon.run_cli_streams(&teton, &["--cwd", "/nope"], "hello\n", CliSeams::Off);

    assert!(
        !status.success(),
        "a refused --cwd must exit non-zero (a script would read 0 as a session that \
         ran); stdout:\n{stdout}\nstderr:\n{stderr}"
    );
    assert_eq!(
        status.code(),
        Some(1),
        "stdout:\n{stdout}\nstderr:\n{stderr}"
    );
    let refusal =
        "teton: could not start a session: path `/nope` does not exist or is not a directory";
    assert_eq!(
        stderr.trim_end().lines().last(),
        Some(refusal),
        "the refusal is the daemon's sentence (spoken by the CLI's fail-fast), naming the \
         path, once, on stderr; stderr:\n{stderr}"
    );
    assert_eq!(
        stderr.matches("/nope").count(),
        1,
        "the path is named once — one line, not a line and a repeat; stderr:\n{stderr}"
    );
    // No session opened, so nothing a session prints follows: no ready line,
    // no reply to the prompt that was piped in, no cost summary.
    for marker in [
        READY_MARKER,
        COST_MARKER,
        ROOT_LINE,
        ROOT_MOVED,
        CLEAR_MARKER,
    ] {
        assert!(
            !stdout.contains(marker) && !stderr.contains(marker),
            "`{marker}` printed after a refused create; stdout:\n{stdout}\nstderr:\n{stderr}"
        );
    }
    assert_no_turn_ran(
        &format!("{stdout}{stderr}"),
        "the piped prompt after a refused --cwd",
    );
}

/// **BR-6 (verify finding V), the CLI half isolated: `teton --cwd /nope` is
/// refused by the CLI itself, with no daemon to reach.**
///
/// The test above would pass on the daemon's validator alone — it speaks the
/// same sentence — so it cannot tell whether the CLI's fail-fast is wired.
/// This one can: there is **no reachable daemon** (an empty `XDG_RUNTIME_DIR`,
/// and a `TETON_CONFIG` an autostart would refuse — the
/// `a_refused_config_is_reported_by_the_cli_that_autostarted_the_daemon`
/// fixture), so a CLI that reached `ensure_connected` would print `could not
/// reach the daemon` and quote the config refusal. It must instead exit 1 on
/// the one refusal line and print none of that, no banner, no ready line.
///
/// And the fail-fast belongs to the commands that open a session: `teton
/// --cwd /nope doctor` under the same conditions opens none, is not refused,
/// and reports the daemon as not running.
#[test]
fn a_missing_cwd_is_refused_by_the_cli_itself_with_no_daemon_to_reach() {
    let daemon = daemon_bin();
    if !daemon.exists() {
        let _ = std::io::stderr()
            .write_all(b"skipping CLI e2e: teton-code binary not built (run under --workspace)\n");
        return;
    }
    let root = PathBuf::from("/tmp").join(format!("tcnod{:x}", std::process::id()));
    let runtime_dir = root.join("x");
    std::fs::create_dir_all(&runtime_dir).unwrap();
    let config_path = root.join("config.toml");
    std::fs::write(&config_path, "pinned_local_model = \"qwen2.5-coder-3b\"\n").unwrap();

    let run = |args: &[&str]| {
        let mut child = Command::new(teton_bin())
            .args(args)
            .env("XDG_RUNTIME_DIR", &runtime_dir)
            // REQ-611 TASK-364: isolation travels with the client, since a
            // CLI that cannot reach a daemon autostarts one that inherits
            // this environment and prunes whatever it resolves to.
            .env("XDG_DATA_HOME", root.join("d"))
            .env("TETON_CONFIG", &config_path)
            .env("TETON_REPO_ROOT", &root)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .expect("spawn teton");
        // A CLI that refuses `--cwd` before it reads anything may already have
        // exited — and closed its end of the pipe — by the time this write
        // lands. `EPIPE` is then the expected shape of "refused before reading
        // stdin", not a failure of the fixture (the ubuntu CI leg hit exactly
        // that race); any other write error still fails the test.
        if let Err(err) = child
            .stdin
            .take()
            .expect("piped stdin")
            .write_all(b"hello\n")
        {
            assert_eq!(
                err.kind(),
                std::io::ErrorKind::BrokenPipe,
                "write teton stdin: {err}"
            );
        }
        let output = child.wait_with_output().expect("run teton");
        (
            String::from_utf8_lossy(&output.stdout).into_owned(),
            String::from_utf8_lossy(&output.stderr).into_owned(),
            output.status,
        )
    };

    let (stdout, stderr, status) = run(&["--cwd", "/nope"]);
    assert_eq!(
        status.code(),
        Some(1),
        "stdout:\n{stdout}\nstderr:\n{stderr}"
    );
    assert_eq!(
        stderr.trim_end(),
        "teton: could not start a session: path `/nope` does not exist or is not a directory",
        "one refusal line, the CLI's own, and nothing else on stderr; stdout:\n{stdout}"
    );
    assert!(stdout.is_empty(), "nothing on stdout; stdout:\n{stdout}");
    let combined = format!("{stdout}{stderr}");
    for reach in [
        "could not reach the daemon",
        "The daemon reported:",
        "configuration is present but invalid",
        "Teton Code v",
        "ready (",
    ] {
        assert!(
            !combined.contains(reach),
            "`{reach}` printed: the CLI reached for a daemon instead of failing fast; \
             output:\n{combined}"
        );
    }

    // `doctor` opens no session: the flag is carried but nothing is refused,
    // and the command does its job against the absent daemon.
    let (stdout, stderr, status) = run(&["--cwd", "/nope", "doctor"]);
    let combined = format!("{stdout}{stderr}");
    assert!(
        status.success(),
        "`teton --cwd /nope doctor` must not be refused; output:\n{combined}"
    );
    assert!(
        combined.contains("daemon: not running"),
        "doctor must report the daemon as not running; output:\n{combined}"
    );
    assert!(
        !combined.contains("could not start a session"),
        "a command that opens no session was refused for its --cwd; output:\n{combined}"
    );

    let _ = std::fs::remove_dir_all(&root);
}

/// **AC-9 / AC-12: a relative `--cwd` is relative to the shell's directory, and
/// `~/x` is `HOME`'s `x` — the grammar `/cd` shares.**
///
/// The CLI is run *from* the fixture root with `--cwd proj`, and again with
/// `--cwd ~/x` under a `HOME` the test set; a bare `/cd` in each session names
/// the root the daemon derived — the resolved directory and its kind — which is
/// the only place a piped session says where it stands. The daemon is given the
/// same `HOME`, so its display for the second root is `~/x`: one spelling, from
/// the daemon's own rule (ADR-1).
#[test]
fn a_relative_cwd_joins_the_shell_directory_and_a_tilde_cwd_expands_home() {
    let daemon_bin = daemon_bin();
    // `HOME` for both processes: a directory under `/tmp` the test made, holding
    // `x/` — named from the pid, like the daemon's own root, and removed after.
    let home = PathBuf::from("/tmp").join(format!("tchome{:x}", std::process::id() & 0xffff));
    let _ = std::fs::remove_dir_all(&home);
    std::fs::create_dir_all(home.join("x")).unwrap();
    let daemon = TestDaemon::spawn_scripted_with_env(
        &daemon_bin,
        TURN_REPLIES,
        &[("HOME", home.to_str().unwrap())],
    );
    let teton = teton_bin();
    let (project, _plain) = root_fixtures(&daemon.root);

    // Relative: `proj` from the daemon's root. The CLI resolves it against its
    // own working directory as the OS reports it — on macOS `/tmp` is a link,
    // so the daemon's spelling of the result may begin `/private/tmp` — which
    // is why the assertion is on the tail of the path, not on the fixture's
    // own spelling of it.
    let (stdout, stderr, status) = daemon.run_cli_from(
        &teton,
        &["--cwd", "proj"],
        "/cd\n",
        Some(&daemon.root),
        &[("HOME", &home)],
    );
    assert!(status.success(), "stdout:\n{stdout}\nstderr:\n{stderr}");
    let root_line = stdout
        .lines()
        .find(|line| line.contains(ROOT_LINE))
        .unwrap_or_else(|| panic!("no bare `/cd` line; stdout:\n{stdout}\nstderr:\n{stderr}"));
    let expected_tail = format!(
        "/{}/proj (project proj)",
        daemon.root.file_name().unwrap().to_str().unwrap()
    );
    assert!(
        root_line.ends_with(&expected_tail),
        "a relative --cwd must resolve against the shell's directory to the project \
         fixture; line: {root_line:?}, expected tail {expected_tail:?}"
    );
    // The fixture is the directory the line names — spelled by the OS, so the
    // tail comparison above is the honest one; the project marker is what
    // made it `project proj`.
    assert!(project.join("Cargo.toml").is_file());

    // `~/x`: `HOME`'s `x`, a marker-less directory — a plain root, spelled `~/x`
    // by the daemon because it reads the same `HOME`.
    let (stdout, stderr, status) = daemon.run_cli_from(
        &teton,
        &["--cwd", "~/x"],
        "/cd\n",
        Some(&daemon.root),
        &[("HOME", &home)],
    );
    let _ = std::fs::remove_dir_all(&home);
    assert!(status.success(), "stdout:\n{stdout}\nstderr:\n{stderr}");
    assert!(
        stdout.contains(&format!("{ROOT_LINE}~/x (not a project)")),
        "`--cwd ~/x` must resolve to HOME's x and be spelled `~/x`; stdout:\n{stdout}\nstderr:\n{stderr}"
    );
    assert!(
        !stdout.contains(NOT_A_PROJECT),
        "the not-a-project notice reached a pipe; stdout:\n{stdout}"
    );
}

/// **AC-11 / BR-8, the piped half: `/cd ~` from a project fires nothing extra
/// on a pipe — byte parity with a move to another project.**
///
/// Two fresh daemons, two sessions from a project fixture; one types `/cd ~`
/// (`HOME` a directory the test made — a `home`-kind root, which at a terminal
/// re-fires the launch notice), the other `/cd <another project>`. Both draw
/// the clear line and a `session root is now …` line; with that one line
/// masked and the session ids masked, the two transcripts are **byte-identical**
/// — the notice's bytes are TTY-only (ADR-5), so a pipe sees the same stream
/// whichever kind of root the session moved to. The terminal half — the notice
/// really firing after `/cd ~` — is `pty_e2e::a_move_to_a_non_project_root_re_fires_the_notice_at_a_terminal`.
#[test]
fn slash_cd_to_home_on_a_pipe_is_byte_identical_to_a_move_to_a_project() {
    let daemon_bin = daemon_bin();
    let teton = teton_bin();
    let home = PathBuf::from("/tmp").join(format!("tchome2{:x}", std::process::id() & 0xffff));
    let _ = std::fs::remove_dir_all(&home);
    std::fs::create_dir_all(&home).unwrap();

    /// The transcript with its one root-dependent line replaced by a sentinel.
    fn mask_root_line(transcript: &str) -> String {
        let mut hits = 0;
        let masked: Vec<String> = transcript
            .lines()
            .map(|line| {
                if let Some(at) = line.find(ROOT_MOVED) {
                    hits += 1;
                    format!("{}{ROOT_MOVED}<root>", &line[..at])
                } else {
                    line.to_owned()
                }
            })
            .collect();
        assert_eq!(
            hits, 1,
            "one move, one root line; transcript:\n{transcript}"
        );
        let mut out = masked.join("\n");
        if transcript.ends_with('\n') {
            out.push('\n');
        }
        out
    }

    let run = |target: &dyn Fn(&Path) -> String| -> (String, String) {
        let daemon = TestDaemon::spawn_scripted_with_env(
            &daemon_bin,
            TURN_REPLIES,
            &[("HOME", home.to_str().unwrap())],
        );
        let (project, _plain) = root_fixtures(&daemon.root);
        let other = daemon.root.join("proj2");
        std::fs::create_dir_all(&other).unwrap();
        std::fs::write(other.join("package.json"), "{}\n").unwrap();
        let (stdout, stderr, status) = daemon.run_cli_from(
            &teton,
            &["--cwd", project.to_str().unwrap()],
            &format!("/cd {}\n", target(&other)),
            None,
            &[("HOME", &home)],
        );
        assert!(status.success(), "stdout:\n{stdout}\nstderr:\n{stderr}");
        (stdout, stderr)
    };
    let (to_home, home_stderr) = run(&|_| "~".to_owned());
    let (to_project, project_stderr) = run(&|other| other.display().to_string());
    let _ = std::fs::remove_dir_all(&home);

    // Non-vacuity: each run moved where it was told, and said so.
    assert!(
        to_home.contains(&format!("{ROOT_MOVED}~ (your home folder)")),
        "`/cd ~` must move to the home folder; stdout:\n{to_home}\nstderr:\n{home_stderr}"
    );
    assert!(
        to_project.contains(ROOT_MOVED) && to_project.contains("(project proj2)"),
        "`/cd <project>` must move to the project; stdout:\n{to_project}\nstderr:\n{project_stderr}"
    );
    assert!(
        to_home.contains(CLEAR_MARKER) && to_project.contains(CLEAR_MARKER),
        "a move clears; home:\n{to_home}\nproject:\n{to_project}"
    );
    // The claim: on a pipe, the move to a non-project root added no bytes.
    assert!(
        !to_home.contains(NOT_A_PROJECT),
        "the not-a-project notice reached a pipe after `/cd ~`; stdout:\n{to_home}"
    );
    assert_eq!(
        mask_root_line(&mask_session_id(&to_home, "the /cd ~ run")),
        mask_root_line(&mask_session_id(&to_project, "the /cd <project> run")),
        "piped output after `/cd ~` differs from a move to a project by more than the \
         root line — the notice's bytes (or something else) reached the pipe.\n\
         /cd ~:\n{to_home}\n/cd <project>:\n{to_project}"
    );
    assert_eq!(
        home_stderr, project_stderr,
        "stderr differs between the two moves"
    );
}

// ---------------------------------------------------------------------------
// REQ-585 — user-defined slash commands discovered from SKILL.md
// ---------------------------------------------------------------------------
//
// Everything below drives the **shipped pair** — the `teton` binary and the
// `teton-code` daemon — over a fixture `HOME`, because that is the only place
// the four globs of BR-1 can be observed at all. A test written against
// `run_cli`/`run_cli_with_stdin` inherits the *runner's* environment: on a
// developer's machine that is a real `~/.claude` with a real skill shelf, and on
// CI it is a home with nothing in it. Neither is the fixture, and a suite that
// leaned on either would be green for a reason unrelated to the code under test
// (LESSON-481's corollary). So every test here hands the same made-up `HOME` to
// **both** processes — `TestDaemon::spawn_scripted_with_env` for the daemon that
// discovers, `run_cli_from`'s env for the client that renders — exactly as
// `slash_cd_to_home_on_a_pipe_is_byte_identical_to_a_move_to_a_project` does.
//
// The fixtures are ordering-independent by construction: names are chosen so
// the registry's own sort (`SkillRegistry::assemble`, by name) is the order
// asserted, and nothing here reads a `read_dir` result. Two REQ-583 tests were
// green on APFS and red on ext4 for exactly that (LESSON-540).

/// The one line `/help`'s skills section opens with (`slash::SKILLS_HEADER`).
///
/// Stated here rather than imported because this suite runs the **shipped
/// binary** and knows nothing of the crate's internals: that the section really
/// carries these bytes is a claim under test, not an assumption (the same reason
/// [`READ_ROWS`] spells its two vocabularies out).
const SKILLS_HEADER: &str = "skills — arguments are passed through as typed:";

/// The head of `/help`'s argument footer — where the built-in half ends on a
/// session that discovered no skills at all.
const ARGUMENT_FOOTER_HEAD: &str = "Command arguments are split on whitespace";

/// The tail every rejected `/` line carries (`slash::HELP_HINT`).
const HELP_HINT: &str = "type /help for the commands this session knows.";

/// A throwaway directory tree under `/tmp`, removed on drop.
///
/// `/tmp` and a short name for the reason [`TestDaemon`]'s own root is there: a
/// fixture path is joined onto by the daemon's socket-adjacent state, and the
/// deep per-user temp dir would blow past `SUN_LEN`. The pid and a counter keep
/// two trees in one run apart.
struct SkillTree {
    root: PathBuf,
}

impl SkillTree {
    fn new(tag: &str) -> Self {
        static SEQ: AtomicUsize = AtomicUsize::new(0);
        let seq = SEQ.fetch_add(1, Ordering::SeqCst);
        let root = PathBuf::from("/tmp").join(format!(
            "tcsk{tag}{:x}-{:x}",
            std::process::id() & 0xffff,
            seq
        ));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).unwrap();
        Self { root }
    }

    fn path(&self) -> &Path {
        &self.root
    }

    /// Write `contents` at `rel`, creating the parents. Returns the file.
    fn write(&self, rel: &str, contents: &str) -> PathBuf {
        let path = self.root.join(rel);
        std::fs::create_dir_all(path.parent().expect("a file has a parent")).unwrap();
        std::fs::write(&path, contents).unwrap();
        path
    }
}

impl Drop for SkillTree {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.root);
    }
}

/// A well-formed skill file: the three keys the daemon reads, then `body`
/// **verbatim**.
///
/// The closing delimiter is followed immediately by the body, so
/// `skill.body.len()` is `body.len()` — which is what BR-12's echo line renders
/// and what the size assertions below compute from.
fn skill_file(description: &str, hint: Option<&str>, body: &str) -> String {
    let mut out = format!("---\ndescription: {description}\n");
    if let Some(hint) = hint {
        out.push_str(&format!("argument-hint: {hint}\n"));
    }
    out.push_str("---\n");
    out.push_str(body);
    out
}

/// [`skill_file`] with extra frontmatter lines between the delimiters — the
/// two REQ-587 BR-3 flags, and any future key a fixture needs to declare.
///
/// A builder rather than four literal files: the flags are `key: value` lines
/// under REQ-585's flat parser, and a fixture that hand-wrote the header would
/// drift from the one `skill_file` writes for every other test here.
fn skill_file_with(description: &str, frontmatter: &[&str], body: &str) -> String {
    let mut out = format!("---\ndescription: {description}\n");
    for line in frontmatter {
        out.push_str(line);
        out.push('\n');
    }
    out.push_str("---\n");
    out.push_str(body);
    out
}

/// The `[skills]` table naming `project` as durably acknowledged (REQ-589
/// D-13) — the one unattended answer to D-10's trust gate.
///
/// The row is the root's **canonical** name, because that is what the daemon
/// matches: a row naming a *path* would let a symlink dropped at that path hand
/// an unacknowledged repository the trust of an acknowledged one. On macOS this
/// is the difference between `/tmp/x` and `/private/tmp/x`, which is exactly the
/// spelling every fixture here has — so a build that stopped canonicalising
/// fails the listed leg on this platform rather than passing by coincidence.
///
/// The rule is spelled out here rather than imported because the minter is
/// `pub(crate)` to `tetond` and this is a black-box client test. Its own suite
/// (`harness::tools::skill::tests::the_durable_name_resolves_the_link_and_names_the_tree`)
/// owns the question of what the name *is*; what this restates is only enough of
/// it to write a row a user could have written by hand.
fn trusting(project: &Path) -> String {
    let canonical = std::fs::canonicalize(project)
        .unwrap_or_else(|err| panic!("{} does not resolve: {err}", project.display()));
    format!(
        "[skills]\ntrusted_project_roots = [{:?}]\n\n",
        canonical.display().to_string()
    )
}

/// A project fixture under `root`: a `Cargo.toml` (so the root classifies as a
/// project) and nothing else.
fn project_at(root: &Path, name: &str) -> PathBuf {
    let project = root.join(name);
    std::fs::create_dir_all(&project).unwrap();
    std::fs::write(
        project.join("Cargo.toml"),
        format!("[package]\nname = \"{name}\"\n"),
    )
    .unwrap();
    project
}

/// The rendered lines of `/help`'s skills section — the rows between the header
/// and the diagnostic line that closes it.
fn skill_rows(listing: &[String], what: &str) -> Vec<String> {
    let at = listing
        .iter()
        .position(|line| line == SKILLS_HEADER)
        .unwrap_or_else(|| panic!("{what} printed no skills section; listing:\n{listing:#?}"));
    listing[at + 1..]
        .iter()
        .take_while(|line| line.starts_with('/'))
        .cloned()
        .collect()
}

/// The line closing the skills section: what registered, from where, and what
/// was found and dropped (BR-3).
fn skills_diagnostic(listing: &[String], what: &str) -> String {
    let rows = skill_rows(listing, what).len();
    let at = listing
        .iter()
        .position(|line| line == SKILLS_HEADER)
        .expect("`skill_rows` already found the header");
    listing
        .get(at + 1 + rows)
        .unwrap_or_else(|| panic!("{what} printed no diagnostic line; listing:\n{listing:#?}"))
        .clone()
}

/// `/help`'s **built-in** half: every line above the skills section (or above
/// the footers, when there is no section), trailing blanks trimmed.
///
/// The comparison AC-1's "byte-identical to the pre-REQ golden" is made with:
/// an empty registry renders no section at all, so a session with no skills
/// prints the pre-REQ listing by construction — and the populated session's
/// built-in half has to equal it line for line.
fn builtin_half(listing: &[String]) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    for line in listing {
        if line == SKILLS_HEADER || line.starts_with(ARGUMENT_FOOTER_HEAD) {
            break;
        }
        out.push(line.clone());
    }
    while out.last().is_some_and(|line| line.trim().is_empty()) {
        out.pop();
    }
    out
}

/// One piped session's `/help` listing, as owned lines.
fn help_listing(session: &str, what: &str) -> Vec<String> {
    typed_output(session_body(session, what), what)
        .into_iter()
        .map(str::to_owned)
        .collect()
}

/// The transcript with its root-naming line replaced by a sentinel, so two runs
/// under two daemons (two fixture roots) can be compared for everything else.
fn mask_root_reported(transcript: &str) -> String {
    transcript
        .lines()
        .map(|line| match line.find(ROOT_LINE) {
            Some(at) => format!("{}{ROOT_LINE}<root>", &line[..at]),
            None => line.to_owned(),
        })
        .collect::<Vec<_>>()
        .join("\n")
}

/// **AC-1: `/help` lists what the four globs found, with sources, hints and the
/// diagnostic — and the built-in half does not move.**
///
/// Two sessions on two daemons, differing only in what their `HOME` and their
/// project root hold. The populated one finds `alpha` (a `skills/` directory)
/// and `beta` (a `commands/` file) under the fixture `HOME`, and `gamma` under
/// the session root's `.claude/skills`; the bare one finds nothing anywhere.
///
/// Three claims, and the third is the one that needs the pair:
///
/// * the rows carry name, hint, description and source, **in name order** — the
///   registry sorts, so `read_dir`'s order cannot reach the surface (LESSON-540);
/// * the diagnostic reports `3 skills (user 2, project 1); 0 skipped`;
/// * the **built-in** half of the listing is byte-identical between the two. An
///   empty registry renders no section at all (ADR-12), so the bare session
///   prints the pre-REQ golden by construction, and the comparison is against a
///   listing produced by this same binary rather than against a copy of one.
///
/// The bare run doubles as the fixture's own isolation check: if `HOME` had
/// leaked from the runner, a developer's real skill shelf would print a section
/// here and this test would say so (LESSON-481's corollary).
#[test]
fn slash_help_lists_the_discovered_skills_with_their_sources_and_the_diagnostic() {
    let daemon_bin = daemon_bin();
    let teton = teton_bin();

    let populated = SkillTree::new("a");
    populated.write(
        ".claude/skills/alpha/SKILL.md",
        &skill_file("the alpha skill", Some("[target]"), "Alpha body.\n"),
    );
    populated.write(
        ".claude/commands/beta.md",
        &skill_file("the beta skill", None, "Beta body.\n"),
    );
    let bare = SkillTree::new("b");

    let listing_from = |home: &SkillTree, with_project: bool| -> Vec<String> {
        let daemon = TestDaemon::spawn_scripted_with_env(
            &daemon_bin,
            TURN_REPLIES,
            &[("HOME", home.path().to_str().unwrap())],
        );
        let project = project_at(&daemon.root, "proj");
        if with_project {
            std::fs::create_dir_all(project.join(".claude/skills/gamma")).unwrap();
            std::fs::write(
                project.join(".claude/skills/gamma/SKILL.md"),
                skill_file("the gamma skill", Some("<REQ-xxx>"), "Gamma body.\n"),
            )
            .unwrap();
        }
        let (stdout, stderr, status) = daemon.run_cli_from(
            &teton,
            &["--cwd", project.to_str().unwrap()],
            "/help\n",
            None,
            &[("HOME", home.path())],
        );
        assert!(status.success(), "stdout:\n{stdout}\nstderr:\n{stderr}");
        assert_no_turn_ran(&stdout, "`/help`");
        help_listing(&stdout, "`/help`")
    };

    let full = listing_from(&populated, true);
    let empty = listing_from(&bare, false);

    // The isolation check, first: a fixture HOME with no `.claude` and a project
    // with none either registers nothing, and an empty registry renders no
    // section. A section here would mean the runner's own home leaked in.
    assert!(
        !empty.iter().any(|line| line == SKILLS_HEADER),
        "a session with no skills must render no skills section — the fixture \
         HOME did not take, so every assertion below would be about the \
         developer's own `~/.claude`; listing:\n{empty:#?}"
    );

    // The rows: every field, in the registry's own order.
    assert_eq!(
        skill_rows(&full, "`/help`"),
        vec![
            "/alpha [target] — the alpha skill (user)".to_owned(),
            "/beta — the beta skill (user)".to_owned(),
            "/gamma <REQ-xxx> — the gamma skill (project)".to_owned(),
        ],
        "the skills section must list every discovered command with its hint, \
         its description and its source, sorted by name; listing:\n{full:#?}"
    );
    assert_eq!(
        skills_diagnostic(&full, "`/help`"),
        "3 skills (user 2, project 1); 0 skipped",
        "listing:\n{full:#?}"
    );

    // The built-in half did not move.
    let builtin = builtin_half(&full);
    assert!(
        builtin.len() > 10,
        "the built-in half is too short to be the command table; \
         listing:\n{full:#?}"
    );
    assert_eq!(
        builtin,
        builtin_half(&empty),
        "the skills section changed the built-in listing — REQ-585 adds one \
         section below the rows and nothing else (ADR-12)"
    );
    // Both footers still close the page, below the new section.
    for footer in [ARGUMENT_FOOTER_HEAD, "//text sends text as a prompt"] {
        assert!(
            full.iter().any(|line| line.starts_with(footer)),
            "`{footer}` vanished from /help; listing:\n{full:#?}"
        );
    }
}

/// **REQ-587 AC-12: `/help` marks the rows the user may not type, and typing
/// one names the flag that made it so.**
///
/// End to end, through a real daemon that reads the two frontmatter flags: the
/// mark is only honest if the flags survive discovery, the wire and the render,
/// and the unit tests upstream of this one can each be green while any of those
/// three drops a key ([`both_invocation_flags_reach_the_client`] is the
/// daemon-side half of the same claim).
///
/// Four rows, because BR-3's states are only distinguishable side by side:
///
/// * `alpha` declares neither flag — the ordinary row, unmarked;
/// * `beta` declares `disable-model-invocation: true` — hidden from the model
///   and **unmarked here**, because that flag says nothing about the user;
/// * `delta` declares `user-invocable: false` — model-only, marked;
/// * `mute` declares both — listed, invocable by nobody, and this is the only
///   surface in the product that renders the combination.
#[test]
fn slash_help_marks_a_model_only_skill_and_typing_it_names_the_flag() {
    let daemon_bin = daemon_bin();
    let teton = teton_bin();

    let home = SkillTree::new("m");
    home.write(
        ".claude/skills/alpha/SKILL.md",
        &skill_file("the alpha skill", Some("[target]"), "Alpha body.\n"),
    );
    home.write(
        ".claude/commands/beta.md",
        &skill_file_with(
            "the beta skill",
            &["disable-model-invocation: true"],
            "Beta body.\n",
        ),
    );
    home.write(
        ".claude/commands/delta.md",
        &skill_file_with(
            "the delta skill",
            &["user-invocable: false"],
            "Delta body.\n",
        ),
    );
    home.write(
        ".claude/commands/mute.md",
        &skill_file_with(
            "the mute skill",
            &["user-invocable: false", "disable-model-invocation: true"],
            "Mute body.\n",
        ),
    );

    let daemon = TestDaemon::spawn_scripted_with_env(
        &daemon_bin,
        TURN_REPLIES,
        &[("HOME", home.path().to_str().unwrap())],
    );
    let project = project_at(&daemon.root, "proj");
    let (stdout, stderr, status) = daemon.run_cli_from(
        &teton,
        &["--cwd", project.to_str().unwrap()],
        "/help\n/delta\n/mute\n/beta\n",
        None,
        &[("HOME", home.path())],
    );
    assert!(status.success(), "stdout:\n{stdout}\nstderr:\n{stderr}");

    let listing = help_listing(&stdout, "`/help`");
    assert_eq!(
        skill_rows(&listing, "`/help`"),
        vec![
            "/alpha [target] — the alpha skill (user)".to_owned(),
            "/beta — the beta skill (user)".to_owned(),
            "/delta — the delta skill (user, model-only)".to_owned(),
            "/mute — the mute skill (user, invocable by nobody)".to_owned(),
        ],
        "a row the user may not type must say which kind of row it is, and a \
         row only the *model* may not reach must not be marked at all; \
         listing:\n{listing:#?}"
    );
    // Registered and counted, never skipped: BR-3's flags are a named state of
    // a file discovery accepted, not a reason to drop it.
    assert_eq!(
        skills_diagnostic(&listing, "`/help`"),
        "4 skills (user 4, project 0); 0 skipped",
        "listing:\n{listing:#?}"
    );

    // Typed: the refusal names the line of the user's own file, because the
    // name is spelled correctly and listed in `/help` four lines above.
    assert!(
        stdout.contains(&format!(
            "`/delta` is a skill whose frontmatter says `user-invocable: false`, so only the \
             model may invoke it — {HELP_HINT}"
        )),
        "output:\n{stdout}"
    );
    assert!(
        stdout.contains(&format!(
            "`/mute` is a skill whose frontmatter says `user-invocable: false`, so nobody may \
             invoke it — its frontmatter also says `disable-model-invocation: true` — {HELP_HINT}"
        )),
        "the two-flag row must not be told that the model may run it; output:\n{stdout}"
    );
    assert!(
        !stdout.contains("unknown command: `/delta`"),
        "a listed name answered as if the session had never heard of it; \
         output:\n{stdout}"
    );
    // …and `disable-model-invocation` costs the user nothing: `/beta` still
    // dispatches, which is what keeps the two flags from being read as one.
    assert!(
        stdout.contains("/beta → skill beta (user,"),
        "a skill hidden from the model must still dispatch for the user; \
         output:\n{stdout}"
    );
}

/// **AC-2: a skill may not take a name the table has claimed.**
///
/// Four fixture skills named for four different *kinds* of claim — a row
/// (`cost`), an alias (`exit`, which is `/quit`), a family word (`provider`,
/// which no row spells but four rows begin with) and REQ-582's `teton` line —
/// are each listed as shadowed and none of them dispatches.
///
/// The "behaves byte-identically to today" half is a **paired run** rather than
/// a phrase search: the same four lines are typed into a session whose `HOME`
/// holds those skills and into one whose `HOME` holds nothing, and the two
/// transcripts are compared whole (session id and root line masked, since the
/// two daemons have their own of each). A shadowed skill that leaked one byte
/// into `/cost`, `/provider list`, `teton provider list` or `/exit` fails here.
///
/// `/exit` is last because it is `/quit`: the session ends on it, which is
/// itself the assertion that the alias still reaches the row.
#[test]
fn a_skill_may_not_take_a_reserved_name_and_the_built_in_still_runs() {
    let daemon_bin = daemon_bin();
    let teton = teton_bin();

    let claimed = SkillTree::new("c");
    for name in ["cost", "exit", "provider", "teton"] {
        claimed.write(
            &format!(".claude/skills/{name}/SKILL.md"),
            &skill_file(
                &format!("the {name} skill"),
                None,
                &format!("Body of {name}.\n"),
            ),
        );
    }
    let bare = SkillTree::new("d");

    // The four surfaces AC-2 names, typed into a session under each HOME.
    const RESERVED_LINES: &str = "/cost\n/provider list\nteton provider list\n/exit\n";

    let run = |home: &SkillTree, stdin: &str| -> String {
        let daemon = TestDaemon::spawn_scripted_with_env(
            &daemon_bin,
            TURN_REPLIES,
            &[("HOME", home.path().to_str().unwrap())],
        );
        let project = project_at(&daemon.root, "proj");
        let (stdout, stderr, status) = daemon.run_cli_from(
            &teton,
            &["--cwd", project.to_str().unwrap()],
            stdin,
            None,
            &[("HOME", home.path())],
        );
        assert!(status.success(), "stdout:\n{stdout}\nstderr:\n{stderr}");
        stdout
    };

    // Leg 1 — `/help` marks each of the four, and says why.
    let helped = run(&claimed, "/help\n");
    let listing = help_listing(&helped, "`/help`");
    assert_eq!(
        skill_rows(&listing, "`/help`"),
        vec![
            "/cost — the cost skill (user, shadowed by the built-in `/cost`)".to_owned(),
            "/exit — the exit skill (user, shadowed by the built-in `/quit`)".to_owned(),
            "/provider — the provider skill (user, shadowed by the `/provider` commands)"
                .to_owned(),
            "/teton — the teton skill (user, shadowed by the `teton` command line)".to_owned(),
        ],
        "each reserved name must be listed and marked with what owns it; \
         listing:\n{listing:#?}"
    );
    assert_eq!(
        skills_diagnostic(&listing, "`/help`"),
        "4 skills (user 4, project 0); 0 skipped",
        "a shadowed skill is registered and listed, never skipped; \
         listing:\n{listing:#?}"
    );
    assert_no_turn_ran(&helped, "`/help` over four shadowed skills");

    // Leg 2 — the four lines, with and without the shadowing files present.
    let with = run(&claimed, RESERVED_LINES);
    let without = run(&bare, RESERVED_LINES);

    // Non-vacuity: each surface really rendered, and the alias really quit.
    assert!(
        with.contains(COST_MARKER),
        "`/cost` did not render the cost report; output:\n{with}"
    );
    assert!(
        with.contains(PROVIDER_LIST_NOTE),
        "`teton provider list` did not hand off to the row; output:\n{with}"
    );
    assert!(
        with.contains("deepseek"),
        "`/provider list` did not render the registered provider; output:\n{with}"
    );
    // Nothing expanded: a shadowed row never becomes an invocation.
    assert!(
        !with.contains("→ skill "),
        "a shadowed skill dispatched; output:\n{with}"
    );
    assert_no_turn_ran(&with, "the four reserved lines");

    assert_eq!(
        mask_root_reported(&mask_session_id(&with, "the shadowed run")),
        mask_root_reported(&mask_session_id(&without, "the control run")),
        "four skills holding reserved names changed what `/cost`, `/provider \
         list`, `teton provider list` and `/exit` print.\n\
         with skills:\n{with}\nwithout:\n{without}"
    );
}

/// The body AC-4 substitutes into: one `$ARGUMENTS`, no dynamic context.
const ALPHA_BODY: &str = "Say hello about $ARGUMENTS.\n";

/// **AC-4 / BR-12, the surface half: one typed `/name …` draws exactly one echo
/// line, before anything else, naming the skill, its source and its size.**
///
/// The line is pinned byte for byte, and the size is computed with the product's
/// own formatter ([`teton_protocol::format_bytes`], so `KiB` and not `KB`) over
/// the body this test wrote — never a figure copied out of the spec's
/// illustration.
///
/// "No model call precedes it" is asserted by *position*: the echo is the first
/// line the session printed after the entry prompt, and the scripted engine's
/// reply comes after it. What the model was actually handed — the substituted
/// body, both interior spaces and both quotes intact — is asserted where it is
/// produced (`tetond`'s `the_engine_is_handed_the_expansion_the_budget_measured`);
/// this suite pins the bytes on the terminal.
#[test]
fn a_skill_invocation_echoes_one_line_naming_the_skill_before_any_model_call() {
    let daemon_bin = daemon_bin();
    let teton = teton_bin();

    let home = SkillTree::new("e");
    home.write(
        ".claude/skills/alpha/SKILL.md",
        &skill_file("the alpha skill", Some("[target]"), ALPHA_BODY),
    );
    let daemon = TestDaemon::spawn_scripted_with_env(
        &daemon_bin,
        TURN_REPLIES,
        &[("HOME", home.path().to_str().unwrap())],
    );
    let project = project_at(&daemon.root, "proj");

    let (stdout, stderr, status) = daemon.run_cli_from(
        &teton,
        &["--cwd", project.to_str().unwrap()],
        "/alpha teton  code \"repo\"\n",
        None,
        &[("HOME", home.path())],
    );
    assert!(status.success(), "stdout:\n{stdout}\nstderr:\n{stderr}");

    let expected = format!(
        ">> /alpha → skill alpha (user, {}, 0 dynamic commands)",
        teton_protocol::format_bytes(ALPHA_BODY.len() as u64)
    );
    let printed = typed_output(session_body(&stdout, "`/alpha`"), "`/alpha`");
    assert_eq!(
        printed.first().copied(),
        Some(expected.as_str()),
        "the invocation's echo line must be the first thing the session prints \
         for it, in BR-12's spelling; output:\n{stdout}"
    );

    // It really became a turn, and the echo came first.
    let reply = TURN_REPLIES[0];
    assert!(
        stdout.contains(reply),
        "the invocation produced no prompt turn; output:\n{stdout}"
    );
    assert!(
        stdout.find(&expected) < stdout.find(reply),
        "the model answered before the invocation was echoed; output:\n{stdout}"
    );
    // The body is in the file, and BR-12 says it stays there.
    assert!(
        !stdout.contains("Say hello about"),
        "the expansion was printed to the surface; output:\n{stdout}"
    );
}

/// A skill whose body carries one dynamic-context command.
fn one_command_skill() -> String {
    skill_file("the dyn skill", None, "Context follows.\n\n!`echo one`\n")
}

/// **AC-9's `plan` leg, through the binary: the level settles it, nobody is
/// asked, and `/verbose` says which door closed.**
#[test]
fn at_plan_a_skills_dynamic_context_is_not_run_and_verbose_names_the_level() {
    let daemon_bin = daemon_bin();
    let teton = teton_bin();

    let home = SkillTree::new("f");
    home.write(".claude/skills/dyn/SKILL.md", &one_command_skill());
    let daemon = TestDaemon::spawn_scripted_with_env(
        &daemon_bin,
        TURN_REPLIES,
        &[("HOME", home.path().to_str().unwrap())],
    );
    let project = project_at(&daemon.root, "proj");

    let (stdout, stderr, status) = daemon.run_cli_from(
        &teton,
        &["--cwd", project.to_str().unwrap()],
        "/verbose\n/permissions plan\n/dyn\n",
        None,
        &[("HOME", home.path())],
    );
    assert!(status.success(), "stdout:\n{stdout}\nstderr:\n{stderr}");

    assert!(
        stdout.contains("/dyn → skill dyn (user, ")
            && stdout.contains(", 1 dynamic command, none run)"),
        "the echo line must report both numbers when they differ (BR-12); \
         output:\n{stdout}"
    );
    assert!(
        stdout
            .contains("  !`echo one` — not run: this session's permission level does not run them"),
        "`/verbose` must name the level as the door that closed; output:\n{stdout}"
    );
    assert!(
        !stdout.contains("permission requested"),
        "at `plan` nothing may be asked; output:\n{stdout}"
    );
    assert!(
        stdout.contains(TURN_REPLIES[0]),
        "the invocation must still produce its turn; output:\n{stdout}"
    );
}

/// **AC-9's `full` leg: an unattended session runs the commands with no prompt
/// at all — the automation posture BR-11 names.**
#[test]
fn at_full_a_skills_dynamic_context_runs_on_a_pipe_with_no_prompt() {
    let daemon_bin = daemon_bin();
    let teton = teton_bin();

    let home = SkillTree::new("g");
    home.write(".claude/skills/dyn/SKILL.md", &one_command_skill());
    let daemon = TestDaemon::spawn_scripted_with_env(
        &daemon_bin,
        TURN_REPLIES,
        &[("HOME", home.path().to_str().unwrap())],
    );
    let project = project_at(&daemon.root, "proj");

    let (stdout, stderr, status) = daemon.run_cli_from(
        &teton,
        &["--cwd", project.to_str().unwrap()],
        "/verbose\n/permissions full\n/dyn\n",
        None,
        &[("HOME", home.path())],
    );
    assert!(status.success(), "stdout:\n{stdout}\nstderr:\n{stderr}");

    assert!(
        stdout.contains("/dyn → skill dyn (user, ") && stdout.contains(", 1 dynamic command)"),
        "when every command ran the echo line carries one count, not two; \
         output:\n{stdout}"
    );
    assert!(
        stdout.contains("  !`echo one` — ran ("),
        "`/verbose` must report the command as run; output:\n{stdout}"
    );
    assert!(
        !stdout.contains("permission requested") && !stdout.contains("was refused without asking"),
        "at `full` a piped session asks nothing and refuses nothing; \
         output:\n{stdout}"
    );
    assert!(
        stdout.contains(TURN_REPLIES[0]),
        "the invocation must still produce its turn; output:\n{stdout}"
    );
}

/// **AC-9's sharpest leg, and BR-11's whole point: on piped stdin at `guarded`
/// the client refuses the consent *without reading a line*, so the `y` that
/// follows is still the next prompt.**
///
/// The claim is a negative one — a line was **not** consumed — so it is asserted
/// as one, the way
/// [`yes_waives_the_in_session_above_floor_confirmation_without_eating_a_line`]
/// asserts its own: the session is fed a second line after the invocation, and
/// that line has to reach the *entry loop*. Two turns ran, so the `y` became a
/// prompt; had it been eaten as a consent answer there would have been one.
///
/// The counts are equalities rather than `>=` for the same reason: a client that
/// answered the consent from stdin *and* then ran the `y` as a prompt would
/// satisfy a `contains`, and fails this.
#[test]
fn on_a_pipe_at_guarded_a_skill_consent_is_refused_without_eating_the_next_line() {
    let daemon_bin = daemon_bin();
    let teton = teton_bin();

    let home = SkillTree::new("i");
    home.write(".claude/skills/dyn/SKILL.md", &one_command_skill());
    let daemon = TestDaemon::spawn_scripted_with_env(
        &daemon_bin,
        TURN_REPLIES,
        &[("HOME", home.path().to_str().unwrap())],
    );
    let project = project_at(&daemon.root, "proj");

    let (stdout, stderr, status) = daemon.run_cli_from(
        &teton,
        // `guarded` is the session default, so nothing is typed to reach it —
        // and the `y` therefore sits immediately behind the invocation, which
        // is the arrangement the negative assertion below needs.
        &["--cwd", project.to_str().unwrap()],
        "/verbose\n/dyn\ny\n",
        None,
        &[("HOME", home.path())],
    );
    assert!(status.success(), "stdout:\n{stdout}\nstderr:\n{stderr}");

    // The question was drawn — with every command of the invocation on it — and
    // then refused, unanswered.
    assert!(
        stdout.contains("skill `dyn` (user) wants to run 1 dynamic-context command:")
            && stdout.contains("    !`echo one`"),
        "the refusal must still show what it refused; output:\n{stdout}"
    );
    assert!(
        stdout.contains(
            "skill `dyn`'s dynamic context was refused without asking: this session's input \
             is not a terminal"
        ),
        "BR-11's refusal line is missing; output:\n{stdout}"
    );
    assert!(
        stdout.contains(
            "send `/permissions full` ahead of it, or set `[permissions] default_level`, to \
             allow it unattended."
        ),
        "a refusal must name its remedy — and the remedy has to be one a piped \
         session can actually take (there is no `--permissions` flag); \
         output:\n{stdout}"
    );
    assert!(
        stdout.contains("  !`echo one` — not run: no human could be asked"),
        "the placeholder must say nobody was asked — not that anyone declined; \
         output:\n{stdout}"
    );
    assert!(
        !stdout.contains("the user declined"),
        "a fail-closed refusal is not a decline; output:\n{stdout}"
    );

    // THE NEGATIVE: the `y` was not consumed. It reached the entry loop and
    // became the second prompt of the session, so the scripted engine served a
    // second reply and the verbose session drew a second turn-end line.
    assert!(
        stdout.contains(TURN_REPLIES[1]),
        "the `y` after the invocation was eaten as a consent answer instead of \
         reaching the entry loop as the next prompt line (BR-11); output:\n{stdout}"
    );
    assert_eq!(
        stdout.matches("turn ended").count(),
        2,
        "exactly two turns must have run — the invocation and the `y` that \
         followed it; output:\n{stdout}"
    );
}

/// **AC-19's `/verbose` half: the invocation line, the file it came from, the
/// frontmatter that was ignored, and what became of each command.**
///
/// The four are one record, so they are asserted from one session. The two
/// commands end differently on purpose — BR-6's endings are distinct facts, and
/// a `/verbose` that collapsed them would be reporting a summary rather than a
/// record.
///
/// The `/cost` half of AC-19 is **not** here: this suite's scripted tier is
/// local, and a local turn is billed nothing (`cost_attribution`'s "none for
/// local-tier inference"), so a `/cost` assertion in this file would pass
/// against an empty ledger and say nothing. It runs in
/// `tetond/tests/cost_attribution.rs`.
#[test]
fn verbose_shows_the_invocation_line_with_its_path_and_per_command_outcomes() {
    let daemon_bin = daemon_bin();
    let teton = teton_bin();

    let home = SkillTree::new("j");
    home.write(
        ".claude/skills/rec/SKILL.md",
        // `model:` is a key Teton does not honor: registered, listed, inert.
        "---\ndescription: the rec skill\nmodel: opus\n---\n\
         !`echo one`\n!`exit 3`\n",
    );
    let daemon = TestDaemon::spawn_scripted_with_env(
        &daemon_bin,
        TURN_REPLIES,
        &[("HOME", home.path().to_str().unwrap())],
    );
    let project = project_at(&daemon.root, "proj");

    let (stdout, stderr, status) = daemon.run_cli_from(
        &teton,
        &["--cwd", project.to_str().unwrap()],
        "/verbose\n/permissions full\n/rec\n",
        None,
        &[("HOME", home.path())],
    );
    assert!(status.success(), "stdout:\n{stdout}\nstderr:\n{stderr}");

    // A command that exited non-zero still *started*, so both count as run and
    // the echo line carries a single number.
    assert!(
        stdout.contains("/rec → skill rec (user, ") && stdout.contains(", 2 dynamic commands)"),
        "the echo line must name the skill, its source, its size and its command \
         count; output:\n{stdout}"
    );
    // The path, home-relative: an absolute one would carry a username into the
    // transcript.
    assert!(
        stdout.contains("  ~/.claude/skills/rec/SKILL.md"),
        "`/verbose` must name the file the body came from, home-relative; \
         output:\n{stdout}"
    );
    assert!(
        stdout.contains("  ignored frontmatter: model"),
        "`/verbose` must list the frontmatter keys this build ignored; \
         output:\n{stdout}"
    );
    assert!(
        stdout.contains("  !`echo one` — ran ("),
        "the first command ran; output:\n{stdout}"
    );
    assert!(
        stdout.contains("  !`exit 3` — failed (exit 3)"),
        "the second command's non-zero exit must be reported as its own ending; \
         output:\n{stdout}"
    );
}

/// **REQ-587 BR-9: the echo line names the swap, and `/verbose` names the
/// flags — end to end, on the path a person can actually type.**
///
/// Both facts are new keys on `skill_invoked`, and each has three places to be
/// dropped between the registry row and the screen: the daemon's publish, the
/// wire, and the renderer. A unit test on the renderer is green while any of
/// the first two loses a key, which is why this leg exists at all.
///
/// It drives the **typed** path deliberately. The model path is instrumented in
/// `tetond/tests/skill_turn.rs`, where a real tool call can be made; here the
/// point is that neither fact is model-only news — `/validate` in a repository
/// that defines its own reaches the repository's file at every permission level
/// with no prompt, so this echo line is the only notice the user gets that the
/// name they typed resolved somewhere else.
///
/// The **turn count** is asserted by its absence, which is the same claim from
/// the other side: a typed invocation spends none of the per-turn cap, and a
/// renderer that printed `0 of 12` here would name a budget the user is not
/// drawing on.
///
/// The third skill (`/gamma`) is BR-3's **typo** case, and it needs the whole
/// product to be honest: the daemon reads a value that is not a boolean, takes
/// the safe reading, names the key as unhonored, and only the client can tell
/// the reader which of the two things happened. A renderer that quoted the
/// canonical literal here would show an author a line their file does not
/// contain — and a unit test on either half alone cannot see that, because each
/// half is doing exactly what it was asked to.
#[test]
fn a_typed_invocation_names_the_swap_and_its_flags_and_counts_no_turn_budget() {
    let daemon_bin = daemon_bin();
    let teton = teton_bin();

    const VALIDATE_BODY: &str = "Validate the repository's way.\n";

    let home = SkillTree::new("n");
    // The user's own `validate`, which the repository's is about to take the
    // name from — without this file there is no swap to name.
    home.write(
        ".claude/skills/validate/SKILL.md",
        &skill_file("the user validate", None, "User body.\n"),
    );
    // Hidden from the model, still the user's to type: the flag `/help` marks
    // not at all, and this line is the only place it is ever named.
    home.write(
        ".claude/commands/beta.md",
        &skill_file_with(
            "the beta skill",
            &["disable-model-invocation: true"],
            "Beta body.\n",
        ),
    );
    // The same flag with a value no parser reads as a boolean. BR-3's safe
    // reading hides this file from the model exactly as beta's `true` does — the
    // *outcome* is identical — while the file itself said something else
    // entirely, and `/verbose` is where the author of the typo finds out which
    // of the two happened.
    home.write(
        ".claude/commands/gamma.md",
        &skill_file_with(
            "the gamma skill",
            &["disable-model-invocation: yes"],
            "Gamma body.\n",
        ),
    );

    // REQ-589 D-13, and the reason this fixture needs a `[skills]` table at all.
    // D-10 put an acknowledgment on the typed path, and `validate` **shadows** a
    // user skill — the one case that is asked even at `full` — so this piped
    // session has no permission level that would let it run the repository's
    // skill. A human's durable row is the only unattended answer there is, and
    // seeding one is what makes the rest of this test about what it is named for.
    // Its unlisted counterpart is
    // `an_unattended_session_at_an_unlisted_root_refuses_and_names_the_row`.
    let daemon = TestDaemon::spawn_scripted_trusting(
        &daemon_bin,
        TURN_REPLIES,
        &[("HOME", home.path().to_str().unwrap())],
        &|root| trusting(&project_at(root, "proj")),
    );
    let project = project_at(&daemon.root, "proj");
    std::fs::create_dir_all(project.join(".claude/skills/validate")).unwrap();
    std::fs::write(
        project.join(".claude/skills/validate/SKILL.md"),
        skill_file("the project validate", None, VALIDATE_BODY),
    )
    .unwrap();

    let (stdout, stderr, status) = daemon.run_cli_from(
        &teton,
        &["--cwd", project.to_str().unwrap()],
        "/verbose\n/validate\n/beta\n/gamma\n",
        None,
        &[("HOME", home.path())],
    );
    assert!(status.success(), "stdout:\n{stdout}\nstderr:\n{stderr}");

    assert!(
        stdout.contains(&format!(
            "/validate → skill validate (project — shadows your user skill, {}, \
             0 dynamic commands)",
            teton_protocol::format_bytes(VALIDATE_BODY.len() as u64)
        )),
        "the echo line must name the swap in the source slot — the user typed a \
         name their own shelf has and the repository answered; output:\n{stdout}"
    );
    assert!(
        stdout.contains("  hidden from the model (`disable-model-invocation: true`)"),
        "`/verbose` must name the flag this build honored, in the file's own \
         spelling; output:\n{stdout}"
    );
    // BR-3's *other* reading of the same key, through the whole product: the
    // daemon parses `yes`, takes the safe value, names the key as unhonored, and
    // the client words the two cases apart. The old line quoted
    // `disable-model-invocation: true` at an author who wrote `yes` — a line
    // their file does not contain — one line above `ignored frontmatter:
    // disable-model-invocation`, which alone reads as "this key did nothing".
    assert!(
        stdout.contains(
            "  hidden from the model (`disable-model-invocation` was not `true` or \
             `false`, so the safe reading hid it)"
        ),
        "a file whose flag value is not a boolean must be told so, and told \
         which reading was taken; output:\n{stdout}"
    );
    assert!(
        stdout.contains("  ignored frontmatter: disable-model-invocation"),
        "the unhonored key is still named, now explained by the line above it \
         rather than contradicted by it; output:\n{stdout}"
    );
    // The flags line reports what the file *wrote*: neither skill declared
    // `user-invocable`, so neither is called model-only anywhere.
    assert!(
        !stdout.contains("model-only") && !stdout.contains("invocable by nobody"),
        "a file that declared no `user-invocable` key was reported as if it \
         had; output:\n{stdout}"
    );
    // Matched as a **line shape**, not as a phrase: `this turn` occurs in the
    // route classifier's own sentence, and a substring search would pass on
    // that instead of on the absence it is asserting.
    assert!(
        !stdout
            .lines()
            .any(|line| line.trim_start().starts_with("invocation ") && line.contains("this turn")),
        "a typed invocation was given a per-turn budget it does not draw on; \
         output:\n{stdout}"
    );
}

/// **REQ-589 D-13 / TASK-262: the unattended trust path, both legs, end to end.**
///
/// This is a **deliberate security widening** and the test is written to hold it
/// to its bound rather than to celebrate it. D-10 put an acknowledgment on the
/// user-typed `/name` path; a piped session has nobody to ask, and `validate`
/// here shadows a user skill, which is the case asked even at `full`. So before
/// D-13 no permission level let an automated run invoke a typed project skill,
/// and after it exactly one thing does: a row a human wrote in
/// `[skills] trusted_project_roots`.
///
/// **The two legs are the whole test, and neither is worth anything alone.**
/// They are the same fixture, the same piped session, the same shadowing skill
/// and the same client refusal — only the row in config differs. So:
///
/// - a build that deleted the consultation fails the **listed** leg;
/// - a build that let any unattended session through fails the **unlisted** leg;
/// - and "it refused" cannot be an accident of a fixture that always refuses,
///   because the leg beside it does not.
///
/// The unlisted leg is the one that matters most. Without it the gate is
/// decorative: "no human is here" would itself be the permission, and every
/// scripted run on every machine would expand every repository's skills.
///
/// It also pins the **remedy**, because a refusal a scripted run meets is a dead
/// end without one — and pins it as the *canonical* row, which on macOS is
/// `/private/tmp/…` where the fixture's own spelling is `/tmp/…`. A user who
/// pasted the other one would have added a line that silently never matches.
#[test]
fn an_unattended_session_at_an_unlisted_root_refuses_and_names_the_row() {
    let daemon_bin = daemon_bin();
    let teton = teton_bin();

    for listed in [true, false] {
        let home = SkillTree::new(if listed { "tl" } else { "tu" });
        // The user's own `validate`, so the repository's shadows it — the case
        // no permission level settles, which is why the row is the only answer.
        home.write(
            ".claude/skills/validate/SKILL.md",
            &skill_file("the user validate", None, "User body.\n"),
        );

        let daemon = TestDaemon::spawn_scripted_trusting(
            &daemon_bin,
            TURN_REPLIES,
            &[("HOME", home.path().to_str().unwrap())],
            &|root| {
                let project = project_at(root, "proj");
                if listed {
                    trusting(&project)
                } else {
                    // A non-empty list naming somewhere else: the refusal must
                    // be about *this* root's absence rather than about a machine
                    // that has never acknowledged anything.
                    trusting(&project_at(root, "other"))
                }
            },
        );
        let project = project_at(&daemon.root, "proj");
        std::fs::create_dir_all(project.join(".claude/skills/validate")).unwrap();
        std::fs::write(
            project.join(".claude/skills/validate/SKILL.md"),
            skill_file(
                "the project validate",
                None,
                "Validate the repository's way.\n",
            ),
        )
        .unwrap();

        let (stdout, stderr, status) = daemon.run_cli_from(
            &teton,
            &["--cwd", project.to_str().unwrap()],
            "/validate\n",
            None,
            &[("HOME", home.path())],
        );
        assert!(status.success(), "stdout:\n{stdout}\nstderr:\n{stderr}");

        // True of both legs, and the reason the listed one is the *list*'s doing
        // rather than a session that was never gated: the daemon asked, and this
        // client answered that there is nobody here to ask.
        assert!(
            stdout.contains("could not be asked here")
                && stdout.contains("[skills] trusted_project_roots"),
            "the client must report that it could not ask, and name where the \
             standing answer lives; listed={listed}, output:\n{stdout}"
        );

        let row = trusting(&project);
        let row = row
            .trim_end()
            .trim_start_matches("[skills]\ntrusted_project_roots = [\"")
            .trim_end_matches("\"]")
            .to_owned();
        if listed {
            assert!(
                stdout.contains("/validate → skill validate (project — shadows your user skill"),
                "a root a human durably acknowledged must run its skills with \
                 nobody at the terminal — that is what D-13 bought; output:\n{stdout}"
            );
            // **BR-10/AC-8, and the assertion with the bite** (REQ-591 D-6).
            // This leg is the one where the client's line and the session's
            // outcome disagree: the client answered `NoTerminal`, the daemon
            // rewrote it to `Allowed` from the row, and the skill echoes two
            // lines below. A client that claimed a refusal here would be
            // contradicted by its own transcript — which is what AC-8 is about,
            // and what the shared assertion above cannot see because "could not
            // be asked" is true of both legs.
            assert!(
                !stdout.contains("was refused without asking"),
                "the client claimed an outcome it cannot know: the row made this \
                 turn go ahead, and the line above says it was refused; \
                 output:\n{stdout}"
            );
            assert!(
                !stdout.contains("has not acknowledged"),
                "the turn both ran and refused; output:\n{stdout}"
            );
        } else {
            assert!(
                stdout.contains("has not acknowledged")
                    && stdout.contains("there was no client to ask"),
                "an unattended session at a root nobody listed must refuse \
                 exactly as it did before D-13 — this is the assertion that \
                 keeps the gate from being decorative; output:\n{stdout}"
            );
            assert!(
                !stdout.contains("/validate → skill validate"),
                "the repository's skill ran anyway; output:\n{stdout}"
            );
            assert!(
                stdout.contains(&row),
                "the refusal must name the canonical row to add, or the remedy \
                 is a guess — expected `{row}` in:\n{stdout}"
            );
        }
    }
}

/// **REQ-591 D-4 — a row names a tree, and a tree does not move when `$HOME`
/// does.**
///
/// The durable name used to be home-relative (`~/proj`), which made a row's
/// meaning a function of `$HOME` **at consult time**: a daemon later launched
/// with a different `HOME` resolved the same row against a different tree. The
/// security argument for changing that is weak on its own — anyone who can
/// rewrite the daemon's environment can rewrite `config.toml` — and it is not
/// the reason. The row is *documented as naming a tree*, and a home-relative
/// string names a tree and an environment variable.
///
/// # Why the project lives inside `HOME` here
///
/// That is the whole fixture, and it is what gives this test its bite.
/// Everywhere else in this file the project sits *beside* the fixture home,
/// where the absolute and home-relative spellings of a tree coincide and
/// neither mint can be told from the other. Under a home that **contains** the
/// project they diverge — `~/proj` against `/private/tmp/…/proj` — so the
/// listed leg below runs the skill today and would **refuse** under the pre-D-4
/// mint, because the row would no longer be a string this build ever produces.
///
/// The unlisted leg is the pairing (LESSON-520): the same daemon, the same
/// home, the same piped invocation, and a well-formed row naming a *sibling*
/// tree. Without it "it ran" would be satisfied by a build that matched
/// everything.
///
/// # Where the old spelling went
///
/// A row left in the pre-D-4 home-relative form cannot appear in a config this
/// daemon will start on: REQ-591 D-5 refuses it at load, by name, with the
/// correct form in the message. That is louder than the fail-closed consult it
/// replaces, and the consult is still underneath it —
/// `skill::tests::the_durable_name_outlives_the_home_it_was_minted_under`
/// drives the gate with such a row directly and it still matches nothing.
///
/// Piped, and shadowing, for the reason
/// [`an_unattended_session_at_an_unlisted_root_refuses_and_names_the_row`] is:
/// no permission level settles this question, so the row is the only answer
/// there is and what the row *says* is the entire subject of the test.
#[test]
fn a_row_written_under_one_home_still_names_its_tree_under_another() {
    let daemon_bin = daemon_bin();
    let teton = teton_bin();

    for (listed, expect_run) in [(true, true), (false, false)] {
        let home = SkillTree::new(if listed { "ha" } else { "hr" });
        // The user's own `validate`, so the repository's shadows it — the case
        // no permission level settles.
        home.write(
            ".claude/skills/validate/SKILL.md",
            &skill_file(
                "the user validate",
                None,
                "User body.
",
            ),
        );
        // **Inside** the home, which is what makes the two spellings differ.
        let project = project_at(home.path(), "proj");
        std::fs::create_dir_all(project.join(".claude/skills/validate")).unwrap();
        std::fs::write(
            project.join(".claude/skills/validate/SKILL.md"),
            skill_file(
                "the project validate",
                None,
                "Validate the repository's way.
",
            ),
        )
        .unwrap();

        // Both rows are well-formed absolute mints — D-5 refuses anything else
        // at load, so a home-relative row cannot be the pairing here. What
        // differs is only *which tree* the row names.
        let row = if listed {
            trusting(&project)
        } else {
            trusting(&project_at(home.path(), "other"))
        };
        let daemon = TestDaemon::spawn_scripted_trusting(
            &daemon_bin,
            TURN_REPLIES,
            &[("HOME", home.path().to_str().unwrap())],
            &|_| row.clone(),
        );

        let (stdout, stderr, status) = daemon.run_cli_from(
            &teton,
            &["--cwd", project.to_str().unwrap()],
            "/validate
",
            None,
            &[("HOME", home.path())],
        );
        assert!(
            status.success(),
            "stdout:
{stdout}
stderr:
{stderr}"
        );

        // True of both legs: the daemon asked, and this client answered that
        // there is nobody here to ask. So whatever happens next is the *row*'s
        // doing.
        assert!(
            stdout.contains("could not be asked here"),
            "listed={listed}: the client must report that it could not ask; \
             output:\n{stdout}"
        );
        assert_eq!(
            stdout.contains("/validate → skill validate (project — shadows your user skill"),
            expect_run,
            "listed={listed}: the row this build mints for a tree **inside** \
             `$HOME` is that tree's absolute path, so it matches here — a \
             home-relative mint would name `~/proj` and this row would match \
             nothing; output:\n{stdout}"
        );
        assert_eq!(
            !stdout.contains("has not acknowledged"),
            expect_run,
            "listed={listed}: and the refusal and the run are exclusive — a \
             build that did both would be BR-10's defect; output:\n{stdout}"
        );
        if expect_run {
            // REQ-591 D-6, as in
            // [`an_unattended_session_at_an_unlisted_root_refuses_and_names_the_row`]:
            // on the leg the row rescues, the client must not have claimed a
            // refusal it does not get to observe.
            assert!(
                !stdout.contains("was refused without asking"),
                "listed={listed}: the client claimed an outcome the daemon then \
                 reversed; output:\n{stdout}"
            );
        }
    }
}

/// **AC-14: `/cd` re-derives the project skills and leaves the user skills
/// alone — and `/help` says so without a restart.**
///
/// One session, two roots, three `/help`s worth of evidence in two: the user
/// skill is in both listings and the two project skills are in exactly one each.
/// A registry that was merged rather than replaced would leave `/one`
/// dispatchable under the second root, which is a row naming a file the session
/// no longer stands on.
#[test]
fn a_cd_re_derives_the_project_skills_and_help_reflects_it_without_a_restart() {
    let daemon_bin = daemon_bin();
    let teton = teton_bin();

    let home = SkillTree::new("k");
    home.write(
        ".claude/skills/usr/SKILL.md",
        &skill_file("the user skill", None, "User body.\n"),
    );
    let daemon = TestDaemon::spawn_scripted_with_env(
        &daemon_bin,
        TURN_REPLIES,
        &[("HOME", home.path().to_str().unwrap())],
    );
    let first = project_at(&daemon.root, "one");
    let second = project_at(&daemon.root, "two");
    for (project, name) in [(&first, "one"), (&second, "two")] {
        std::fs::create_dir_all(project.join(format!(".claude/skills/{name}"))).unwrap();
        std::fs::write(
            project.join(format!(".claude/skills/{name}/SKILL.md")),
            skill_file(&format!("the {name} skill"), None, "Project body.\n"),
        )
        .unwrap();
    }

    let (stdout, stderr, status) = daemon.run_cli_from(
        &teton,
        &["--cwd", first.to_str().unwrap()],
        &format!("/help\n/cd {}\n/help\n", second.display()),
        None,
        &[("HOME", home.path())],
    );
    assert!(status.success(), "stdout:\n{stdout}\nstderr:\n{stderr}");

    let body = session_body(&stdout, "the /cd session");
    let (before, after) = body
        .split_once(ROOT_MOVED)
        .unwrap_or_else(|| panic!("the session never moved; output:\n{stdout}"));

    for (segment, present, absent, what) in [
        (before, "/one — the one skill (project)", "/two", "before"),
        (after, "/two — the two skill (project)", "/one", "after"),
    ] {
        assert!(
            segment.contains(present),
            "the {what} listing is missing `{present}`; segment:\n{segment}"
        );
        assert!(
            !segment.contains(absent),
            "the {what} listing still carries `{absent}` — the registry was \
             merged rather than replaced; segment:\n{segment}"
        );
        assert!(
            segment.contains("/usr — the user skill (user)"),
            "the user half must survive a move; segment:\n{segment}"
        );
        assert!(
            segment.contains("2 skills (user 1, project 1); 0 skipped"),
            "the {what} diagnostic is wrong; segment:\n{segment}"
        );
    }
    assert_no_turn_ran(&stdout, "the /cd session");
}

/// **AC-17 / BR-10: a name discovery *found and dropped* says why; a name it
/// never saw keeps the pre-REQ bytes.**
///
/// The pair is the point. `unknown command` is the honest answer for a name that
/// exists nowhere, and it is exactly the wrong answer for `/analyze` on a
/// machine whose `~/.claude/skills/analyze/SKILL.md` is sitting right there with
/// a broken header — which is the dogfood incident this REQ was opened on.
///
/// The second leg's bytes are asserted against the same literal the pre-REQ
/// unknown-command test uses, on a session whose registry is genuinely
/// non-empty: a control with *no* skills at all would pass whether or not the
/// skipped branch exists.
#[test]
fn a_skipped_skill_says_why_and_a_name_nobody_has_stays_unknown() {
    let daemon_bin = daemon_bin();
    let teton = teton_bin();

    let home = SkillTree::new("l");
    // Found and not registered: an opening delimiter with no closing one.
    home.write(".claude/skills/analyze/SKILL.md", "---\nname: analyze\n");
    // A healthy neighbour, so the registry is not empty and the second leg is
    // about the *absence of an entry* rather than the absence of a registry.
    home.write(
        ".claude/skills/healthy/SKILL.md",
        &skill_file("the healthy skill", None, "Healthy body.\n"),
    );
    let daemon = TestDaemon::spawn_scripted_with_env(
        &daemon_bin,
        TURN_REPLIES,
        &[("HOME", home.path().to_str().unwrap())],
    );
    let project = project_at(&daemon.root, "proj");

    let (stdout, stderr, status) = daemon.run_cli_from(
        &teton,
        &["--cwd", project.to_str().unwrap()],
        "/analyze teton code repo\n/nobodyhasthis\n",
        None,
        &[("HOME", home.path())],
    );
    assert!(status.success(), "stdout:\n{stdout}\nstderr:\n{stderr}");

    assert!(
        stdout.contains(&format!(
            "`/analyze` is a skill that was skipped: malformed frontmatter — {HELP_HINT}"
        )),
        "a name discovery dropped must be answered with the reason, not with \
         `unknown command`; output:\n{stdout}"
    );
    assert!(
        stdout.contains(&format!("unknown command: `/nobodyhasthis` — {HELP_HINT}")),
        "a name with no entry at all must keep the pre-REQ bytes; \
         output:\n{stdout}"
    );
    assert_no_turn_ran(&stdout, "a skipped and an unknown name");
}

// ---------------------------------------------------------------------------
// REQ-587 TASK-222 — the two legs only a model-issued call can drive
// ---------------------------------------------------------------------------
//
// The scripted local engine reads the text tool-call form
// (`{"tool": …, "arguments": …}`), which is the only way a *whole-CLI* test can
// make the model issue a `skill` call. Both legs below need one: an
// acknowledgment nobody types, and a refusal nobody can earn by typing.
//
// The tier is deliberately the **local** one, and both legs turn on that: it is
// what makes the second refusal reachable at all (`LOCAL_BUDGET_BYTES` is 32 KiB
// and the fixture body is larger), and it is why AC-10's *cost* half is not
// here — a local turn produces no billed row, so a `/cost` assertion at this
// layer is vacuous. That half runs against a remote `Vendor` in
// `tetond/tests/skill_turn.rs`, and BUG-183 is what happens when it does not.

/// The scripted engine's replies for a session that invokes a skill twice: once
/// successfully, once past the local route's budget.
const SKILL_CALL_REPLIES: &[&str] = &[
    r#"{"tool": "skill", "arguments": {"name": "small", "args": "REQ-587"}}"#,
    "the small skill landed.",
    r#"{"tool": "skill", "arguments": {"name": "huge", "args": ""}}"#,
    "the huge skill did not.",
];

/// **REQ-587 BR-9 end to end: a model invocation echoes one line, and a model
/// invocation the loop refuses echoes a *different* one.**
///
/// TASK-219 could reach neither. Both lines are raised only by a call the model
/// makes, so no typed line drives them, and the negative half lived at unit
/// level. Once the tool is wired, both are on the path a person actually sees —
/// and the second is where the trap is: a first draft that exercised only
/// `skill_echo_line` stayed **green** under "drop the line entirely", because
/// *a refusal is never silent* is a claim about what reaches the surface, not
/// about what a formatter returns. This drives `render_event`.
///
/// The refusal's shape is the assertion, not merely its presence. A refused
/// record and a command-free skill that ran are the same bytes apart from one
/// field, so a refusal rendered as "the invocation line, plus something" reads
/// at a glance as a skill that worked. It therefore opens with the verdict and
/// carries **no** size and **no** dynamic-command count — both true of the file
/// and false of this turn.
#[test]
fn a_model_invocation_echoes_its_line_and_a_refused_one_says_so_instead() {
    let daemon_bin = daemon_bin();
    let teton = teton_bin();

    const SMALL_BODY: &str = "Do the small thing for $ARGUMENTS.\n";
    let home = SkillTree::new("mi");
    home.write(".claude/skills/small/SKILL.md", &{
        let mut out = String::from("---\ndescription: the small skill\n---\n");
        out.push_str(SMALL_BODY);
        out
    });
    // Larger than the local route's byte budget (63,488 B on the 32,768-token
    // window) with the system prompt beside it, and under discovery's 128 KiB
    // per-file ceiling, so Stage A in the loop refuses it — the one refusal a
    // local tier can actually produce, and the reason this leg is scripted
    // local. (`5_000` repeats — 45 KB — while the byte budget was 32 KiB.)
    let filler = "abcdefgh ".repeat(9_000);
    home.write(
        ".claude/skills/huge/SKILL.md",
        &format!("---\ndescription: the huge skill\n---\nHUGE-BODY-MARKER {filler}\n"),
    );

    let daemon = TestDaemon::spawn_scripted_with_env(
        &daemon_bin,
        SKILL_CALL_REPLIES,
        &[("HOME", home.path().to_str().unwrap())],
    );
    let project = project_at(&daemon.root, "proj");
    let (stdout, stderr, status) = daemon.run_cli_from(
        &teton,
        &["--cwd", project.to_str().unwrap()],
        "/verbose\nrun the small skill\nrun the huge skill\n",
        None,
        &[("HOME", home.path())],
    );
    assert!(status.success(), "stdout:\n{stdout}\nstderr:\n{stderr}");

    // The successful line: no `/name →` prefix, because nobody typed one.
    assert!(
        stdout.contains(&format!(
            "skill small (user, {}, 0 dynamic commands) — invoked by the model",
            teton_protocol::format_bytes(SMALL_BODY.len() as u64)
        )),
        "a model invocation must echo one line saying the model asked; \
         output:\n{stdout}"
    );
    assert!(
        !stdout.contains("/small →"),
        "nobody typed `/small`; a model invocation must not render as the \
         user's own line; output:\n{stdout}"
    );
    // The `12` is `tetond::harness::tools::skill::PER_TURN_INVOCATION_CAP`,
    // spelled rather than read: the `teton` crate cannot depend on `tetond`, and
    // a whole-CLI test reads the daemon's ceiling off the wire like any client.
    // The literal is therefore correct and brittle in the same breath, so the
    // message names the constant that moved rather than leaving a reader to
    // wonder where `12` came from.
    assert!(
        stdout.contains("  invocation 1 of 12 this turn"),
        "`/verbose` shows the turn's count against the cap for a model \
         invocation — the `12` here is `PER_TURN_INVOCATION_CAP` \
         (`tetond::harness::tools::skill`), spelled because this crate cannot \
         depend on that one; if that constant moved, this literal follows it. \
         output:\n{stdout}"
    );
    // AC-10's `tool_call` title: `skill <name>`, so the status line says which
    // skill the model reached for rather than only that *something* did.
    assert!(
        stdout.contains("- skill small [") && stdout.contains("- skill huge ["),
        "each `skill` call's status line must be titled with the skill it \
         named; output:\n{stdout}"
    );

    // The refusal line: the verdict first, the reason named, and none of the
    // figures that would claim an expansion happened.
    assert!(
        stdout.contains(
            "refused: skill huge (user) — the expansion did not fit this turn's context budget"
        ),
        "a refused model invocation must print one line naming the reason; \
         output:\n{stdout}"
    );
    assert!(
        !stdout.contains("skill huge (user,"),
        "the refusal must not be rendered as the invocation line with something \
         added — at a glance that reads as a skill that ran; output:\n{stdout}"
    );
    // Nothing of the refused body entered the session.
    assert!(
        !stdout.contains("HUGE-BODY-MARKER"),
        "a refused expansion reached the surface; output:\n{stdout}"
    );
    // …and the turn went on rather than ending: the refusal is a tool result.
    assert!(
        stdout.contains("the huge skill did not."),
        "a refusal is a tool result the model relays, not a turn-ender; \
         output:\n{stdout}"
    );
}

/// **REQ-587 AC-6's pipe leg, which TASK-219 could not reach.**
///
/// BR-4's acknowledgment is raised only by a *model-issued* call for a
/// **project** skill, so no typed line drives it and the negative pin lived at
/// unit level (`prompter.asked == 0` with the pasted `y` still queued). With the
/// tool wired, the whole rule is observable end to end: on piped stdin at
/// `guarded` the client refuses **without reading a line**, the model is told
/// what the user must do, and the `y` that follows is still the next prompt.
///
/// The negative is asserted as one, the way
/// [`on_a_pipe_at_guarded_a_skill_consent_is_refused_without_eating_the_next_line`]
/// asserts its own: a second line is fed after the request, and it has to reach
/// the *entry loop*. Two turns ran, so the `y` became a prompt; had it been
/// eaten as an answer there would have been one.
#[test]
fn on_a_pipe_at_guarded_a_model_issued_project_skill_is_refused_without_eating_the_next_line() {
    let daemon_bin = daemon_bin();
    let teton = teton_bin();

    const REPLIES: &[&str] = &[
        r#"{"tool": "skill", "arguments": {"name": "scratch", "args": ""}}"#,
        "I could not run the repository's skill.",
        "and this is the second prompt.",
    ];

    // A user skill so the roster is never empty for reasons unrelated to the
    // project one, and the project skill the acknowledgment is about.
    let home = SkillTree::new("pk");
    home.write(
        ".claude/skills/mine/SKILL.md",
        &skill_file("a user skill", None, "User body.\n"),
    );
    let daemon = TestDaemon::spawn_scripted_with_env(
        &daemon_bin,
        REPLIES,
        &[("HOME", home.path().to_str().unwrap())],
    );
    let project = project_at(&daemon.root, "proj");
    std::fs::create_dir_all(project.join(".claude/skills/scratch")).unwrap();
    std::fs::write(
        project.join(".claude/skills/scratch/SKILL.md"),
        skill_file("the repository's own skill", None, "PROJECT-BODY-MARKER\n"),
    )
    .unwrap();

    let (stdout, stderr, status) = daemon.run_cli_from(
        &teton,
        // `guarded` is the session default, so nothing is typed to reach it and
        // the `y` sits immediately behind the prompt that triggers the request.
        &["--cwd", project.to_str().unwrap()],
        "/verbose\nuse the repository's scratch skill\ny\n",
        None,
        &[("HOME", home.path())],
    );
    assert!(status.success(), "stdout:\n{stdout}\nstderr:\n{stderr}");

    // The question was drawn — naming the root and its skills — and then
    // refused, unanswered.
    assert!(
        stdout.contains("running `") && stdout.contains("`'s skills as instructions"),
        "the refusal must still show what it refused; output:\n{stdout}"
    );
    assert!(
        stdout.contains("could not be asked here: this session's input is not a terminal"),
        "BR-11's refusal line is missing; output:\n{stdout}"
    );
    assert!(
        stdout.contains(
            "send `/permissions full` ahead of it, or set `[permissions] default_level`, to \
             allow it unattended."
        ),
        "a refusal names a remedy a piped session can actually take; \
         output:\n{stdout}"
    );
    // What the session *does* show for the call itself: the tool-call status
    // line, titled with the skill the model reached for, ending `failed`.
    assert!(
        stdout.contains("- skill scratch [failed]"),
        "the tool-call line must name the skill and say the call did not \
         succeed; output:\n{stdout}"
    );

    // **And the refusal names itself, in the tool's own words** (BR-9, the
    // Events table). TASK-222 recorded the opposite as a gap: the daemon
    // published a `SkillInvoked` only for the two refusals the **loop** raises
    // (`SkillTool::note_loop_refusal` / `publish_refusal`, both `over_budget`),
    // while every refusal the **tool** raises returned through
    // `Refusal::into_outcome` and published nothing — so this client had a
    // rendered sentence for all seven reasons (`session_ui::
    // refusal_reason_words`) and the daemon reached none of them. It now
    // publishes from `SkillTool::refuse`, the one door out of `invoke`'s refusal
    // arms, and this is that record arriving through a real socket at a real
    // client.
    //
    // Mutation: drop the publish from `refuse` and this line disappears.
    assert!(
        stdout.contains(
            "refused: skill scratch (project) — this repository's skills have not been \
             acknowledged for this session"
        ),
        "a typed refusal must say which skill call it was and why, not leave \
         the user with a failed tool line; output:\n{stdout}"
    );
    // A refusal record is **not** an invocation record: the file's size and its
    // dynamic-command count are true of the file and false of this turn —
    // nothing of it entered the context — so the line drops both.
    assert!(
        !stdout.contains("skill scratch (project, "),
        "the refusal rendered as the invocation line with a flag on it, which \
         at a glance reads as a skill that ran; output:\n{stdout}"
    );
    assert!(
        !stdout.contains("PROJECT-BODY-MARKER"),
        "an unacknowledged repository's body reached the session; output:\n{stdout}"
    );
    assert!(
        !stdout.contains("the user declined"),
        "a fail-closed refusal is not a decline; output:\n{stdout}"
    );

    // THE NEGATIVE: the `y` was not consumed. It reached the entry loop and
    // became the second prompt of the session.
    assert!(
        stdout.contains("and this is the second prompt."),
        "the `y` after the request was eaten as an answer instead of reaching \
         the entry loop as the next prompt line (BR-11); output:\n{stdout}"
    );
    assert_eq!(
        stdout.matches("turn ended").count(),
        2,
        "exactly two turns must have run — the one that raised the request and \
         the `y` that followed it; output:\n{stdout}"
    );
}

// ---------------------------------------------------------------------------
// REQ-592 BR-7: the renderer is inert off a terminal
// ---------------------------------------------------------------------------

/// A reply carrying all three of the constructs the renderer rewrites.
///
/// A table (whose columns it would measure and re-pad), bold text (whose markers
/// it would take out and replace with SGR), and a fenced block (whose delimiters
/// it would swallow, and whose three lines are each written to be misread as
/// something else the moment they are classified: a list item, an emphasis span
/// and a table row). Nothing here survives a rendering pass unchanged, which is
/// what makes "unchanged" an assertion worth making.
///
/// Written with no leading or trailing blank line because the scripted engine
/// `trim()`s each block, and the constant has to be exactly the bytes the daemon
/// will stream.
const MARKDOWN_REPLY: &str = "\
Here is the **summary** you asked for.

| tier | provider |
| --- | --- |
| think | kimi |
| build | local |

```text
- not a list item
**not bold**
| not | a | table |
```

That is the whole of it.";

/// **AC-7 / BR-7: a pipe sees the model's bytes, not a rendering of them.**
///
/// The gate `main.rs` opened in TASK-281 is `IsTerminal on stdout`, and this is
/// the other side of it. A scripted turn streams [`MARKDOWN_REPLY`] one
/// space-separated token at a time; the CLI's stdout is a pipe, so it builds the
/// surface it always built and the concatenated chunks land verbatim.
///
/// The single `contains` is the whole claim — the reply appears as one
/// contiguous run of bytes, so every chunk boundary closed up exactly where the
/// daemon left it and nothing was inserted, dropped or re-laid between them. The
/// three assertions after it do not add to that; they exist so a failure names
/// *which* transform leaked rather than printing a hundred-byte diff.
///
/// Why this cannot be left to the unit tests: `PlainSurface::new` and
/// `PlainSurface::with_color` build a surface with no renderer, so every test
/// that constructs one is inertness-by-assumption. Only a real process,
/// deciding for itself whether it has a terminal, can say the *binary* takes
/// that branch — which is the thing BR-7 promises and the thing an inverted gate
/// would break in exactly the sessions no test watches.
///
/// **This test is the only guard BR-7 has. Measured, not assumed:** with the
/// gate forced open (`if interactive` → `if true`, so the renderer runs on a
/// pipe), all **75** pre-existing `cli_e2e` tests still pass and only this one
/// fails. The reason is that every other fixture reply is a short plain
/// paragraph, and a plain paragraph re-wrapped at the 80-column no-terminal
/// default comes out byte-identical — so the suite's byte comparisons, including
/// the exact occurrence counts, are structurally incapable of seeing an inverted
/// gate. Nothing else in this file covers what this covers. Anyone deleting it as
/// redundant is removing the whole of BR-7's coverage, and should re-run that
/// experiment before believing otherwise.
#[test]
fn a_piped_session_streams_the_reply_unrendered() {
    let daemon_bin = daemon_bin();
    let daemon = TestDaemon::spawn_scripted(&daemon_bin, &[MARKDOWN_REPLY]);
    let teton = teton_bin();

    // Stdout alone: the claim is about the bytes the surface wrote, and folding
    // stderr in would widen what counts as a match.
    let (stdout, _stderr, _status) =
        daemon.run_cli_streams(&teton, &[], "lay out the routing table\n", CliSeams::Off);

    assert_eq!(
        stdout.matches(MARKDOWN_REPLY).count(),
        1,
        "the piped reply is not the daemon's own bytes: the renderer ran off a \
         terminal, or the stream was reassembled (BR-7, AC-7).\n--- expected \
         verbatim ---\n{MARKDOWN_REPLY}\n--- stdout ---\n{stdout}"
    );

    // Which transform leaked, if one did.
    for (what, needle) in [
        ("the fence delimiters were swallowed", "```text"),
        ("the bold markers were replaced", "**summary**"),
        ("the table columns were re-padded", "| think | kimi |"),
    ] {
        assert!(
            stdout.contains(needle),
            "{what} — {needle:?} is missing from a piped session; stdout:\n{stdout}"
        );
    }

    // And nothing painted: no SGR, no cursor motion. A pipe gets text.
    assert!(
        !stdout.contains('\u{1b}'),
        "an escape reached a pipe; stdout:\n{stdout:?}"
    );
}

// ---------------------------------------------------------------------------
// REQ-612 — the repository-notes switch, at the client (TASK-376)
// ---------------------------------------------------------------------------
//
// AC → test map for this section:
//
//   BR-2 + AC-10 (the switch, the state line, and the untouched config)
//       → `context_off_and_on_toggle_the_notes_without_writing_config`
//   BR-7 + AC-11 (the truncation notice with /verbose off, the withheld line,
//   and doctor's two advisories)
//       → `a_truncated_or_withheld_notes_file_is_announced_and_doctor_advises`
//
// What this section deliberately does **not** hold: whether the block actually
// left the system prompt. That is a claim about the bytes a vendor received and
// belongs where the vendor can be inspected — `tetond`'s `repo_context.rs` and
// the egress-capture suite. What is checked here is the client's half of BR-2
// and BR-7: the surfaces a user meets, and the file on disk they must not have
// changed by typing a session command.

/// A project fixture under `root` holding a `TETON.md` of `notes`.
///
/// A `Cargo.toml` beside it because only a `project`-kind root is read (BR-1),
/// which is `root_fixtures`' rule and the reason a bare directory with notes in
/// it would make these tests pass vacuously.
fn notes_fixture(root: &Path, name: &str, notes: &str) -> PathBuf {
    let project = root.join(name);
    std::fs::create_dir_all(&project).unwrap();
    std::fs::write(project.join("Cargo.toml"), "[package]\nname = \"notes\"\n").unwrap();
    std::fs::write(project.join("TETON.md"), notes).unwrap();
    project
}

/// The label `doctor`'s two repository-notes advisories share, cut at rather
/// than matched whole because a piped surface decorates its notice lines.
const ADVISORY: &str = "advisory: repo notes:";

/// A `TETON.md` comfortably over the 8 KiB cap, in whole lines so the
/// truncation has a line boundary to cut at (BR-3).
fn oversized_notes() -> String {
    let line = "This repository is described here, at some length, for the cap to bite.\n";
    line.repeat(200)
}

/// Every `context: …` line a piped session drew, with the surface's own
/// decorations (`\u{203a} `, `>> `) trimmed off the front.
///
/// Trimmed rather than matched whole because a piped session's lines carry the
/// notice marker and, on the line the user typed on, the entry prompt as well —
/// so `starts_with` on the raw line finds nothing while `contains` would match a
/// sentence that merely mentions the word. This cuts at the label.
fn context_lines<'a>(output: &'a str, what: &str) -> Vec<&'a str> {
    let lines: Vec<&str> = output
        .lines()
        .filter_map(|line| line.find("context: ").map(|at| &line[at..]))
        .collect();
    assert!(
        !lines.is_empty(),
        "{what}: the session drew no `context:` line at all; output:\n{output}"
    );
    lines
}

/// **REQ-612 BR-2 / AC-10, the client half.** `/context` reports the state on a
/// pipe; `/context off` stops the notes being carried and `/context on`
/// restores them; `config.toml` is byte-identical throughout.
///
/// The load-bearing observation is the **`/verbose` route line**, not the
/// command's own echo: BR-2 says `off` changes what the next prompt carries, and
/// a test that only read `/context`'s answer back would pass on a daemon that
/// changed nothing but its reply. So the session runs a real turn on each side
/// of the switch and asserts the `· notes N B` clause appears on one and not the
/// other — the resident bytes BR-7 puts on that line, which are the same bytes
/// the block is made of.
///
/// The two switches are typed **before** each turn rather than relying on the
/// state published at session create, because `repo_context_state` is published
/// only when there is news (ADR-3): a client that attached after create would
/// see nothing, and a test written against that ordering would be flaky rather
/// than wrong. Every event this asserts on follows a change the test made.
///
/// **Mutation (run 2026-09-03):** making `slash::handle_context` send
/// `ConfigUpdate::SetRepoContextEnabled` through `config/set` instead of
/// `session/context` reddened the byte-identical-config assertion — which is
/// AC-10's whole point, and the one failure a reader of the command's output
/// alone would never see. Restored.
#[test]
fn context_off_and_on_toggle_the_notes_without_writing_config() {
    let daemon_bin = daemon_bin();
    let daemon = TestDaemon::spawn_scripted(&daemon_bin, TURN_REPLIES);
    let teton = teton_bin();
    let project = notes_fixture(
        &daemon.root,
        "proj",
        "# proj\n\nThe crates live in crates/.\n",
    );
    let config_path = daemon.root.join("config.toml");
    let before = std::fs::read_to_string(&config_path).expect("the fixture config is readable");

    let (stdout, stderr, status) = daemon.run_cli_from(
        &teton,
        &["--cwd", project.to_str().unwrap()],
        "/verbose\n/context\n/context off\n/context\nfirst\n/context on\n/context\nsecond\n",
        Some(&daemon.root),
        &[],
    );
    assert!(status.success(), "stdout:\n{stdout}\nstderr:\n{stderr}");

    // The bare read, on a pipe (BR-2's REQ-560 BR-10 clause): the file, the
    // bytes on disk, what is resident, and the cap — all of them the daemon's
    // figures, none of them derivable here.
    let lines = context_lines(&stdout, "the toggle session");
    let read_back = lines
        .iter()
        .find(|line| line.contains("TETON.md") && line.contains("cap "))
        .unwrap_or_else(|| panic!("no `/context` figures line; stdout:\n{stdout}"));
    for needle in ["TETON.md", "bytes on disk", "resident", "cap "] {
        assert!(
            read_back.contains(needle),
            "AC-10: the state line must name {needle:?}; got: {read_back}"
        );
    }

    // Off is off, and it says why — the switch, not a missing file.
    assert!(
        lines.iter().any(|line| line.contains("the switch is off")),
        "`/context off` must report the switch, not a missing file; lines:\n{lines:#?}"
    );

    // The load-bearing half: the turn after `off` spends no notes bytes, and
    // the turn after `on` does. Route lines only render under `/verbose`, which
    // the session turned on first.
    let routes: Vec<&str> = stdout
        .lines()
        .filter_map(|line| line.find("route [").map(|at| &line[at..]))
        .collect();
    assert_eq!(
        routes.len(),
        2,
        "the session must have run one turn on each side of the switch; \
         stdout:\n{stdout}\ndaemon log:\n{}",
        daemon.log()
    );
    assert!(
        !routes[0].contains("· notes "),
        "the turn after `/context off` still spent notes bytes: {}",
        routes[0]
    );
    assert!(
        routes[1].contains("· notes "),
        "the turn after `/context on` carried no notes: {}\ndaemon log:\n{}",
        routes[1],
        daemon.log()
    );

    // AC-10's other half, read off the disk rather than inferred: a session
    // switch writes nothing.
    let after = std::fs::read_to_string(&config_path).expect("the fixture config is readable");
    assert_eq!(
        before, after,
        "`/context on|off` is session-scoped and must not touch config.toml"
    );

    // BR-2's **other** switch, here because it is the contrast that gives the
    // assertion above its meaning: a durable default exists, it is a shell
    // command, and it does write. Without this leg "the config did not change"
    // would also be satisfied by a build in which nothing can change it.
    let posture = daemon.run_cli(&teton, &["context", "status"]);
    assert!(
        posture.contains("repo notes: on (default)"),
        "`teton context status` reports the durable default; output:\n{posture}"
    );
    let disabled = daemon.run_cli(&teton, &["context", "disable"]);
    assert!(
        disabled.contains("repo notes: off (default)"),
        "`teton context disable` reads the posture back off `config/get` rather than \
         echoing the request; output:\n{disabled}"
    );
    assert_ne!(
        before,
        std::fs::read_to_string(&config_path).expect("the fixture config is readable"),
        "the durable switch is the half that writes"
    );

    // And the one line, shared: `teton doctor` prints exactly what
    // `teton context status` printed (AC-11's no-drift clause, the REQ-611
    // AC-20 rule one feature over).
    let doctor = daemon.run_cli(&teton, &["doctor"]);
    // The *posture* line, not every line the label opens: doctor also says
    // "repo notes: no session here", which is the shell form declining to
    // answer about a session it does not own (`report_skill_preflight`'s rule).
    let posture_lines: Vec<&str> = doctor
        .lines()
        .filter_map(|line| line.find("repo notes: ").map(|at| &line[at..]))
        .filter(|line| line.contains("(default)"))
        .collect();
    assert_eq!(
        posture_lines.len(),
        1,
        "doctor prints exactly one repo-notes posture line; output:\n{doctor}"
    );
    assert!(
        disabled
            .lines()
            .any(|line| line.trim().ends_with(posture_lines[0])),
        "`teton context …` and `teton doctor` must print one line, not two; \
         doctor: {:?}\nteton context disable:\n{disabled}",
        posture_lines[0]
    );
}

/// **REQ-612 BR-7 / AC-11.** A file that is cut to fit says so with `/verbose`
/// **off**, a file a boundary covers says so too, and `/doctor` advises on each
/// while staying green.
///
/// Two daemons, because the two states are two different fixtures: one whose
/// `TETON.md` is over the cap, and one whose config carries a `**/TETON.md`
/// boundary. Both sessions are quiet — no `/verbose` anywhere — which is the
/// claim: BR-3's "nothing is clamped in silence" and BR-5's "a session-long
/// silent pin is what the load-time rule exists to prevent" are not diagnostics
/// a user has to opt into.
///
/// `/doctor` rather than the shell's `teton doctor` for the advisories, and the
/// split is `report_skill_preflight`'s: the posture line is configuration and
/// `teton doctor` prints it (`doctor_prints_one_transcript_posture_line`'s
/// neighbour asserts that), while "is *this session's* file truncated" is a
/// question only a session's root can answer. The shell form says so instead of
/// answering about a session it picked, and that arm is pinned in
/// `cli_rows::a_sessionless_doctor_names_no_session_and_asks_for_no_preflight`.
///
/// **Mutation (run 2026-09-03):** putting the `truncated` arm of
/// `session_ui::format_repo_context` behind the verbose gate reddened the first
/// leg's notice assertion; dropping `advise_on_repo_context` from
/// `doctor_report_on` reddened both advisory assertions. Restored.
#[test]
fn a_truncated_or_withheld_notes_file_is_announced_and_doctor_advises() {
    let daemon_bin = daemon_bin();

    // Leg one: over the cap. The notes are resident, and the user is told that
    // what the model has is the first 8 KiB of them.
    {
        let daemon = TestDaemon::spawn_scripted(&daemon_bin, TURN_REPLIES);
        let teton = teton_bin();
        let project = notes_fixture(&daemon.root, "big", &oversized_notes());

        // `off` then `on` so the announcement follows a change this test made,
        // rather than depending on whether the client was subscribed in time for
        // the state published at session create (ADR-3 publishes only news).
        let (stdout, stderr, status) = daemon.run_cli_from(
            &teton,
            &["--cwd", project.to_str().unwrap()],
            "/context off\n/context on\n/doctor\n",
            Some(&daemon.root),
            &[],
        );
        assert!(status.success(), "stdout:\n{stdout}\nstderr:\n{stderr}");
        assert!(
            !stdout.contains("route ["),
            "this leg must be quiet — a verbose session would prove nothing about \
             BR-3's ungated notice; stdout:\n{stdout}"
        );

        let announced = context_lines(&stdout, "the truncated session");
        assert!(
            announced
                .iter()
                .any(|line| line.contains("are resident") && line.contains("trim the file")),
            "BR-3: a truncated file is announced with /verbose off, with its remedy; \
             lines:\n{announced:#?}\ndaemon log:\n{}",
            daemon.log()
        );

        let advisories: Vec<&str> = stdout
            .lines()
            .filter_map(|line| line.find(ADVISORY).map(|at| &line[at..]))
            .collect();
        assert_eq!(
            advisories.len(),
            1,
            "AC-11: `/doctor` advises exactly once on a truncated file; \
             stdout:\n{stdout}"
        );
        assert!(
            advisories[0].contains("TETON.md") && advisories[0].contains("trim the file"),
            "the advisory names the file and the remedy; got: {}",
            advisories[0]
        );

        // And it is an advisory: REQ-578's posture, unchanged.
        let (_out, _err, doctor_status) =
            daemon.run_cli_from(&teton, &["doctor"], "", Some(&daemon.root), &[]);
        assert!(
            doctor_status.success(),
            "an advisory must not change doctor's exit status"
        );
    }

    // Leg two: a boundary covers the file, so it is never made resident.
    {
        let daemon = TestDaemon::spawn_scripted_with_config(
            &daemon_bin,
            TURN_REPLIES,
            "[[boundaries]]\npath_glob = \"**/TETON.md\"\nmode = \"local-only\"\n\n",
        );
        let teton = teton_bin();
        let project = notes_fixture(&daemon.root, "walled", "# walled\n\nSecrets, apparently.\n");

        let (stdout, stderr, status) = daemon.run_cli_from(
            &teton,
            &["--cwd", project.to_str().unwrap()],
            "/context off\n/context on\n/doctor\n",
            Some(&daemon.root),
            &[],
        );
        assert!(status.success(), "stdout:\n{stdout}\nstderr:\n{stderr}");

        let announced = context_lines(&stdout, "the withheld session");
        assert!(
            announced
                .iter()
                .any(|line| line.contains("local-only boundary")),
            "BR-5: a covered file says so rather than going quiet; \
             lines:\n{announced:#?}\ndaemon log:\n{}",
            daemon.log()
        );

        let advisories: Vec<&str> = stdout
            .lines()
            .filter_map(|line| line.find(ADVISORY).map(|at| &line[at..]))
            .collect();
        assert_eq!(
            advisories.len(),
            1,
            "AC-11: `/doctor` advises exactly once on a withheld file; \
             stdout:\n{stdout}"
        );
        assert!(
            advisories[0].contains("not resident") && advisories[0].contains("boundary"),
            "the advisory names the reason and the remedy; got: {}",
            advisories[0]
        );
    }
}

// ---------------------------------------------------------------------------
// REQ-613 — generating the notes when the repository has none (TASK-387)
// ---------------------------------------------------------------------------
//
// AC → test map for this section:
//
//   BR-2 + BR-10 + AC-3 (the pipe refuses without reading stdin; `always` writes
//   on the same pipe)
//       → `a_piped_session_refuses_the_generation_offer_without_reading_stdin_and_always_writes_instead`
//   BR-8 + AC-10 (a file present refuses naming its size and `--force`; the
//   shell door writes the bytes the session door writes)
//       → `context_init_refuses_without_force_and_the_shell_door_writes_the_same_bytes`
//
// Two properties of the fixture make these legs mean what they say:
//
//   * The scripted engine answers the `draft` duty **off script**, from its own
//     `SCRIPTED_DRAFT` constant, so the drafted body is deterministic and does
//     not consume one of `TURN_REPLIES` — which is what lets a leg count turns
//     and compare bytes at the same time.
//   * `run_cli_process` always gives the CLI a **pipe** for stdin. So every
//     session here is unattended by construction: under `generate = ask` the
//     offer is refused without a line being read, and `generate = always` is the
//     only way a fixture reaches the write. That is not a limitation of the
//     harness — it is AC-3's subject.

/// A project fixture under `root` with **no** notes file: a `Cargo.toml`, a
/// `src/main.rs`, and a README, so the evidence walk has a tree, a manifest and
/// an entry point to draft from.
///
/// A `Cargo.toml` because only a `project`-kind root raises the offer at all
/// (BR-1), and the rest because a walk that found nothing to draft from ends in
/// `Failed { NothingToDraft }` — an outcome a fixture cannot tell from a broken
/// pipeline.
fn bare_project(root: &Path, name: &str) -> PathBuf {
    let project = root.join(name);
    std::fs::create_dir_all(project.join("src")).unwrap();
    std::fs::write(
        project.join("Cargo.toml"),
        "[package]\nname = \"bare\"\nversion = \"0.1.0\"\n",
    )
    .unwrap();
    std::fs::write(
        project.join("README.md"),
        "# bare\n\nA fixture repository.\n",
    )
    .unwrap();
    std::fs::write(project.join("src/main.rs"), "fn main() {}\n").unwrap();
    project
}

/// The `[context]` table a fixture needs to reach the write from a pipe.
const GENERATE_ALWAYS: &str = "[context]\ngenerate = \"always\"\n\n";

/// **REQ-613 BR-2 / BR-10 / AC-3.** At `guarded` on a pipe the client refuses
/// the offer **without reading a line** — the next stdin line is still the next
/// prompt — one line says so, and no file is written. With `generate = always`
/// on the same pipe the file is written instead.
///
/// The load-bearing assertion is the **turn count**, not the refusal text. AC-3
/// is a claim about stdin: a client that drew the question on a pipe would eat
/// the user's second prompt as the answer to it (LESSON-537), and the only way
/// to see that from outside the process is to send two prompts and require two
/// distinct scripted replies. A test that read the refusal line alone would pass
/// on exactly the build the AC exists to forbid.
///
/// The second leg is a different daemon rather than a `config/set` on the first,
/// because `[context] generate` is read when the offer is raised and the point
/// of the leg is the **same pipe, different posture**: same fixture shape, same
/// unattended session, and the only difference is the durable key.
///
/// The shell door rides the first leg rather than the second because it is the
/// same claim about the same posture: the task's own note says `teton context
/// init` "answers the gate on its own TTY through the ordinary prompter, or
/// refuses on a pipe as the session would", and this is the pipe. It is also the
/// only leg that exercises the one-shot context's `answer_permissions: true` —
/// a context that answered nothing would leave the daemon's gate waiting on a
/// client that had already printed "in another session".
///
/// **Mutation (run 2026-09-03):** making `consent_gate` answer `Answerable` for
/// `RepoContextGeneration` reddened the two-replies assertion — the session then
/// consumed `second` as the answer to the prompt and ran one turn. Dropping the
/// `repo_context_generation` arm from `refusal_line` reddened both the session's
/// and the shell door's sentence assertions. Restored both.
#[test]
fn a_piped_session_refuses_the_generation_offer_without_reading_stdin_and_always_writes_instead() {
    let daemon_bin = daemon_bin();
    let teton = teton_bin();

    // Leg one: the shipped posture. Nobody can be asked, so nothing is written
    // and nothing is consumed.
    {
        let daemon = TestDaemon::spawn_scripted(&daemon_bin, TURN_REPLIES);
        let project = bare_project(&daemon.root, "asked");
        let notes = project.join("TETON.md");

        let (stdout, stderr, status) = daemon.run_cli_from(
            &teton,
            &["--cwd", project.to_str().unwrap()],
            "first\nsecond\n",
            Some(&daemon.root),
            &[],
        );
        assert!(status.success(), "stdout:\n{stdout}\nstderr:\n{stderr}");

        // The refusal, in BR-10's one sentence: what was refused, that no input
        // was read for it, and the durable opt-in that settles it.
        assert!(
            stdout.contains("writing `TETON.md`"),
            "the refusal names the question rather than the permission key; \
             stdout:\n{stdout}\ndaemon log:\n{}",
            daemon.log()
        );
        assert!(
            stdout.contains("no line of your input was read"),
            "AC-3: the client must say it consumed nothing; stdout:\n{stdout}"
        );
        assert!(
            stdout.contains("[context] generate = always"),
            "BR-10's unattended posture is one sentence and names the opt-in; \
             stdout:\n{stdout}"
        );

        // AC-3's other half, and the one a reader of the line alone cannot see:
        // **both** prompts ran, so the second stdin line was still a prompt.
        assert!(
            stdout.contains(TURN_REPLIES[0]) && stdout.contains(TURN_REPLIES[1]),
            "the offer ate a prompt: the session must have run one turn per stdin \
             line; stdout:\n{stdout}\ndaemon log:\n{}",
            daemon.log()
        );

        assert!(
            !notes.exists(),
            "a refused offer must leave no file at {}",
            notes.display()
        );

        // The shell door, on the same pipe and the same posture: `teton context
        // init` creates its own session at the directory it was pointed at and
        // then inherits the session's refusal rather than re-implementing it
        // (ADR-7). It is the explicit act, so it is not suppressed by anything —
        // and it is still refused, because explicit is not the same as
        // consented and nobody is at this terminal either.
        let shell = daemon.run_cli(
            &teton,
            &["--cwd", project.to_str().unwrap(), "context", "init"],
        );
        assert!(
            shell.contains("writing `TETON.md`")
                && shell.contains("no line of your input was read"),
            "the shell door answers the gate on its own surface and refuses like the \
             session does; output:\n{shell}\ndaemon log:\n{}",
            daemon.log()
        );
        assert!(
            !notes.exists(),
            "a refused `teton context init` must leave no file either"
        );
    }

    // Leg two: the automation opt-in, on the same shape of pipe.
    {
        let daemon =
            TestDaemon::spawn_scripted_with_config(&daemon_bin, TURN_REPLIES, GENERATE_ALWAYS);
        let project = bare_project(&daemon.root, "always");
        let notes = project.join("TETON.md");

        let (stdout, stderr, status) = daemon.run_cli_from(
            &teton,
            &["--cwd", project.to_str().unwrap()],
            "first\n",
            Some(&daemon.root),
            &[],
        );
        assert!(status.success(), "stdout:\n{stdout}\nstderr:\n{stderr}");

        let written = std::fs::read_to_string(&notes).unwrap_or_else(|err| {
            panic!(
                "BR-10: `generate = always` writes on an unattended session; {err}\n\
                 stdout:\n{stdout}\ndaemon log:\n{}",
                daemon.log()
            )
        });
        // BR-6's header, so a reader of the file knows who wrote it — and the
        // sections the draft prompt asks for, so this is the pipeline's output
        // and not a placeholder.
        assert!(
            written.starts_with("> Generated by Teton on "),
            "the file opens with the header BR-6 requires; got:\n{written}"
        );
        assert!(
            written.contains("## Purpose") && written.contains("## Where to look"),
            "the body is the drafted file; got:\n{written}"
        );

        // And the user was told, in a quiet session, that a file was written
        // without them being asked — the setting's name is the news.
        assert!(
            stdout.contains("without asking") && stdout.contains("[context] generate = always"),
            "a write nobody approved is owed the setting that approved it; \
             stdout:\n{stdout}\ndaemon log:\n{}",
            daemon.log()
        );
        assert!(
            stdout.contains("TETON.md written in"),
            "the terminal stage prints in a quiet session; stdout:\n{stdout}"
        );
    }
}

/// **REQ-613 BR-8 / AC-10.** `/context init` in a project that already has notes
/// refuses, naming the file's size and `--force`; and `teton context init` on
/// the shell drives the daemon to write **the same bytes** the in-session
/// command writes for the same evidence.
///
/// # Why the two doors run against one directory, in turn
///
/// AC-10 says "the same evidence", and the only way to have literally the same
/// evidence is to have literally the same directory: the tree, the manifest, the
/// README and the entry point are all inputs to the draft prompt, and two
/// fixtures that merely *look* alike would make a byte comparison a claim about
/// the fixture builder. So the session door writes, the test records the bytes
/// and removes the file — restoring the tree the first run saw — and the shell
/// door writes again into the same directory.
///
/// The comparison is meaningful because the scripted engine answers a `draft`
/// duty off script: the body is a constant, so what is actually being compared
/// is everything *this* feature composes around it — the header, the tier word,
/// the cut facts, and the bounding — from two entry points.
///
/// `generate = always` for the same reason the leg above needs it: every CLI in
/// this suite runs on a pipe, so it is the only posture under which either door
/// reaches the write at all. It changes nothing about what `init` does; it
/// answers the question the prompt would have asked.
///
/// **Mutation (run 2026-09-03):** dropping the `cwd` from `run_context_init`'s
/// `session/create` reddened the shell-door write — the one-shot session then
/// anchored to the daemon's own directory (launchd gives it `/`), which is not a
/// project, and nothing was written at all. Dropping the daemon's `reason` from
/// the `Failed` line reddened the size-and-flag assertion. Restored both.
#[test]
fn context_init_refuses_without_force_and_the_shell_door_writes_the_same_bytes() {
    let daemon_bin = daemon_bin();
    let teton = teton_bin();
    let daemon = TestDaemon::spawn_scripted_with_config(&daemon_bin, TURN_REPLIES, GENERATE_ALWAYS);

    // --- the no-clobber refusal (AC-10's first clause) ----------------------
    let occupied = bare_project(&daemon.root, "occupied");
    let existing = "# occupied\n\nWritten by a person, not by Teton.\n";
    std::fs::write(occupied.join("TETON.md"), existing).unwrap();

    let (stdout, stderr, status) = daemon.run_cli_from(
        &teton,
        &["--cwd", occupied.to_str().unwrap()],
        "/context init\n",
        Some(&daemon.root),
        &[],
    );
    assert!(status.success(), "stdout:\n{stdout}\nstderr:\n{stderr}");
    assert!(
        stdout.contains(&format!("{} bytes is already there", existing.len()))
            && stdout.contains("`--force`"),
        "AC-10: the refusal names the size and the flag; stdout:\n{stdout}\n\
         daemon log:\n{}",
        daemon.log()
    );
    assert_eq!(
        std::fs::read_to_string(occupied.join("TETON.md")).unwrap(),
        existing,
        "BR-6: a refused `init` must not touch the file it refused to clobber"
    );

    // --- the same bytes out of both doors (AC-10's third clause) ------------
    let project = bare_project(&daemon.root, "both");
    let notes = project.join("TETON.md");

    let (stdout, stderr, status) = daemon.run_cli_from(
        &teton,
        &["--cwd", project.to_str().unwrap()],
        "/context init\n",
        Some(&daemon.root),
        &[],
    );
    assert!(status.success(), "stdout:\n{stdout}\nstderr:\n{stderr}");
    let from_the_session = std::fs::read_to_string(&notes).unwrap_or_else(|err| {
        panic!(
            "`/context init` writes the file; {err}\nstdout:\n{stdout}\ndaemon log:\n{}",
            daemon.log()
        )
    });
    assert!(
        stdout.contains("TETON.md written in"),
        "the session door reports the write; stdout:\n{stdout}"
    );
    // BR-7: the file is loaded the same call, and `/context` names who wrote it.
    assert!(
        stdout.contains("origin: generated"),
        "BR-7: the routed answer says the file is Teton's; stdout:\n{stdout}"
    );

    // Restore the tree the first run walked, so the second run sees the same
    // evidence rather than a directory that now contains its own output.
    std::fs::remove_file(&notes).unwrap();

    let shell = daemon.run_cli(
        &teton,
        &["--cwd", project.to_str().unwrap(), "context", "init"],
    );
    let from_the_shell = std::fs::read_to_string(&notes).unwrap_or_else(|err| {
        panic!(
            "`teton context init` writes the file; {err}\noutput:\n{shell}\n\
             daemon log:\n{}",
            daemon.log()
        )
    });
    assert!(
        shell.contains("TETON.md written in"),
        "the shell door reports the write on its own surface; output:\n{shell}"
    );

    assert_eq!(
        from_the_session, from_the_shell,
        "AC-10: one pipeline behind two doors — the shell form must produce the \
         bytes the session form produces for the same evidence"
    );
}

/// **REQ-613 BR-10 / AC-4.** `teton context generate <mode>` writes the durable
/// key through `config/set` — read back off `config/get`, never echoed — and the
/// two doctor advisories report the two postures a user meant once and may not
/// mean now.
///
/// One daemon, both postures, because the claim is about a key that *moves*: a
/// fixture that only ever saw one value would prove the line renders, not that
/// the write landed. `config.toml` is compared on disk for the same reason the
/// REQ-612 leg compares it — a durable write is a claim about a file, and the
/// only honest way to check one is to read the file.
///
/// The two advisories are drawn from different surfaces, and that split is the
/// design rather than the fixture's convenience: `always` is a standing
/// permission on the **machine**, so the shell's `teton doctor` — which owns no
/// session — must name it; `never` is only worth saying at a root that has no
/// notes, so it is `/doctor` inside a session that does.
///
/// **Mutation (run 2026-09-03):** making the `Generate` arm of `context_on` send
/// no `ConfigUpdate` at all reddened the on-disk comparison — the posture line
/// still rendered, off a key nothing had moved. Moving the `always` advisory
/// into the session-scoped pass reddened the shell `doctor` leg, which is the
/// whole reason it is not there. Inverting the `never` advisory's posture guard
/// reddened the `/doctor` leg. Restored all three.
#[test]
fn teton_context_generate_writes_the_durable_key_and_doctor_reports_both_postures() {
    let daemon_bin = daemon_bin();
    let teton = teton_bin();
    let daemon = TestDaemon::spawn_scripted(&daemon_bin, TURN_REPLIES);
    let project = bare_project(&daemon.root, "posture");
    let config_path = daemon.root.join("config.toml");
    let before = std::fs::read_to_string(&config_path).expect("the fixture config is readable");

    // The shipped posture, before anything is written: the clause is there and
    // it says `ask`.
    let shipped = daemon.run_cli(&teton, &["context", "status"]);
    assert!(
        shipped.contains("repo notes: on (default)") && shipped.contains("generate = ask"),
        "the posture line carries the offer setting; output:\n{shipped}"
    );

    // The write, read back off `config/get` rather than echoed.
    let never = daemon.run_cli(&teton, &["context", "generate", "never"]);
    assert!(
        never.contains("generate = never"),
        "`teton context generate never` reports the posture the daemon holds; \
         output:\n{never}"
    );
    let after = std::fs::read_to_string(&config_path).expect("the fixture config is readable");
    assert_ne!(before, after, "the durable posture is the half that writes");
    assert!(
        after.contains("generate = \"never\""),
        "AC-4: the key lands in config.toml; got:\n{after}"
    );

    // The `never` advisory, from the surface that has a root to judge: this one
    // has no notes, so the only door left is `/context init`.
    let (stdout, stderr, status) = daemon.run_cli_from(
        &teton,
        &["--cwd", project.to_str().unwrap()],
        "/doctor\n",
        Some(&daemon.root),
        &[],
    );
    assert!(status.success(), "stdout:\n{stdout}\nstderr:\n{stderr}");
    assert!(
        stdout.contains(ADVISORY)
            && stdout.contains("generate = never")
            && stdout.contains("/context init"),
        "AC-4: `never` at a root with no notes names the door that is left; \
         stdout:\n{stdout}"
    );

    // And the other posture, on the surface that needs no session at all.
    let always = daemon.run_cli(&teton, &["context", "generate", "always"]);
    assert!(always.contains("generate = always"), "output:\n{always}");
    let doctor = daemon.run_cli(&teton, &["doctor"]);
    assert!(
        doctor.contains(ADVISORY) && doctor.contains("standing permission"),
        "AC-4: `always` is named wherever it is set, session or no session; \
         output:\n{doctor}"
    );
    assert!(
        !doctor.contains("only door left"),
        "the two advisories are two reports, not one: {doctor}"
    );

    // An advisory is still an advisory: REQ-578's posture, unchanged.
    let (_out, _err, doctor_status) =
        daemon.run_cli_from(&teton, &["doctor"], "", Some(&daemon.root), &[]);
    assert!(
        doctor_status.success(),
        "a posture advisory must not change doctor's exit status"
    );
}
