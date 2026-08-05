//! REQ-556: what a real terminal proves that a pipe structurally cannot.
//!
//! The rest of this crate's e2e suite (`cli_e2e.rs`) drives `teton` over pipes.
//! That is the right harness for byte-comparable output, and REQ-556 BR-2 keeps
//! it exactly as it was — no indicator bytes are emitted when stdout is not a
//! terminal. The consequence is that the piped suite is *structurally blind* to
//! this REQ's behaviour, so a second harness is the honest cost of the TTY gate
//! rather than an optional extra.
//!
//! What this file pins is BR-1's claim, which is about **timing at a terminal**:
//! an event that arrives while the user is idle at the entry prompt reaches the
//! screen *then*, with nothing typed. Before REQ-556 the entry loop blocked in
//! `read_line`, so nothing drained the event channel between turns and daemon
//! events queued unseen — visible in the report that opened the REQ, where the
//! benchmark and `ready` lines appeared only after a line was typed.
//!
//! **How the event is provoked, and why this way.** The obvious trigger — the
//! local tier reaching `ready` mid-session — needs the daemon parked in its load
//! window on demand, and no existing seam does that (`TETON_LOCAL_SCRIPT` opens
//! the tier from construction; `TETON_FAKE_ENGINE_LOADER` needs the consent flow
//! and a weights host). Inventing a production-code delay to make a test
//! possible would be the wrong trade. Instead this uses a broadcast the daemon
//! already makes for free: `DaemonClientAttach` is published to clients
//! *already subscribed*, before the newcomer subscribes (`tetond/src/server.rs`).
//! So a second client attaching is a deterministic, fixture-free event arriving
//! while the first client sits idle — which is precisely the condition BR-1 is
//! about.
//!
//! **What that does and does not prove.** It proves the idle-render path: an
//! event lands on screen with nothing typed, which fails against the pre-REQ
//! binary. It does **not** exercise the loading *indicator*, because a scripted
//! daemon's tier is open from the start and the indicator correctly draws
//! nothing (BR-6). AC-1's pty leg — the dots observed advancing at a real
//! terminal — is therefore **not covered here**; see the REQ's verification
//! notes.

use std::io::Read;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use portable_pty::{native_pty_system, CommandBuilder, PtySize};

/// Path to the `teton` binary under test.
fn teton_bin() -> PathBuf {
    PathBuf::from(env!("CARGO_BIN_EXE_teton"))
}

/// Path to the sibling `teton-code` daemon binary.
fn daemon_bin() -> PathBuf {
    let mut p = teton_bin();
    p.pop();
    p.join("teton-code")
}

/// Skip rather than fail when the daemon was not built — same posture as
/// `cli_e2e`'s `daemon_or_skip`, so a `-p teton` run without `--workspace`
/// reports honestly instead of going red.
fn daemon_or_skip() -> Option<PathBuf> {
    let daemon = daemon_bin();
    if daemon.exists() {
        return Some(daemon);
    }
    eprintln!("skipping pty e2e: teton-code binary not built (run under --workspace)");
    None
}

/// How long to wait for a marker before declaring it absent. Generous: this
/// asserts on **state reached**, never on a fixed sleep, so a slow machine costs
/// latency rather than a flake (LESSON-450).
const WINDOW: Duration = Duration::from_secs(20);

/// The pty's output, accumulated by a reader thread.
///
/// A thread rather than a deadline around `read`: a blocking read on the pty
/// master does not return when the session goes quiet, so a loop that checked
/// its deadline only *between* reads would park forever the moment the session
/// became idle — which is the exact state this file is about. The thread may
/// block indefinitely; the assertions never do.
type Transcript = std::sync::Arc<std::sync::Mutex<String>>;

fn spawn_reader(mut reader: Box<dyn Read + Send>) -> Transcript {
    let transcript: Transcript = std::sync::Arc::new(std::sync::Mutex::new(String::new()));
    let sink = std::sync::Arc::clone(&transcript);
    std::thread::spawn(move || {
        let mut chunk = [0u8; 4096];
        while let Ok(n) = reader.read(&mut chunk) {
            if n == 0 {
                break;
            }
            sink.lock()
                .expect("transcript mutex")
                .push_str(&String::from_utf8_lossy(&chunk[..n]));
        }
    });
    transcript
}

/// Wait until `marker` appears in the transcript, or `WINDOW` elapses.
///
/// Polls accumulated state rather than sleeping a fixed interval and hoping —
/// a slow machine costs latency here, never a flake (LESSON-450).
fn wait_for(transcript: &Transcript, marker: &str) -> bool {
    let deadline = Instant::now() + WINDOW;
    while Instant::now() < deadline {
        if transcript
            .lock()
            .expect("transcript mutex")
            .contains(marker)
        {
            return true;
        }
        std::thread::sleep(Duration::from_millis(25));
    }
    false
}

fn snapshot(transcript: &Transcript) -> String {
    transcript.lock().expect("transcript mutex").clone()
}

/// A daemon with its own runtime dir, matching `cli_e2e`'s fixture shape.
struct TestDaemon {
    root: PathBuf,
    runtime_dir: PathBuf,
    child: std::process::Child,
}

impl TestDaemon {
    fn spawn(daemon: &Path) -> Self {
        let root = PathBuf::from("/tmp").join(format!("tcpty{:x}", std::process::id()));
        let runtime_dir = root.join("x");
        std::fs::create_dir_all(&runtime_dir).unwrap();
        let config_path = root.join("config.toml");
        // A closed port for the model host: nothing here may reach the network,
        // and no weights are needed for what this file asserts.
        std::fs::write(
            &config_path,
            "[[providers]]\nid = \"deepseek\"\nkind = \"openai-compatible\"\n\
             endpoint = \"https://api.deepseek.com\"\n\n\
             [local_model]\nauto_accept = false\nbase_url = \"http://127.0.0.1:9\"\n",
        )
        .unwrap();
        // A scripted tier: the engine is present from construction, so no
        // consent prompt stands between the session and the entry prompt.
        let script = root.join("local_script.txt");
        std::fs::write(&script, "scripted reply").unwrap();

        let log = std::fs::File::create(root.join("tetond.log")).unwrap();
        let child = std::process::Command::new(daemon)
            .env("XDG_RUNTIME_DIR", &runtime_dir)
            .env("TETON_CONFIG", &config_path)
            .env("TETON_REPO_ROOT", &root)
            .env("TETON_LOCAL_SCRIPT", &script)
            // Load-bearing: without it the probe seams below are *ignored* and
            // the daemon probes the real machine — which then picks the real
            // model and spends tens of seconds loading real weights, so the
            // test times out against a daemon that is behaving correctly.
            .env("TETON_TEST_SEAMS", "1")
            .env("TETON_PROBE_RAM_BYTES", (16u64 << 30).to_string())
            .env("TETON_PROBE_DISK_BYTES", (500u64 << 30).to_string())
            .env("TETON_PROBE_GPU", "apple-silicon")
            .stdout(std::process::Stdio::from(log.try_clone().unwrap()))
            .stderr(std::process::Stdio::from(log))
            .spawn()
            .expect("spawn daemon");
        Self {
            root,
            runtime_dir,
            child,
        }
    }
}

impl Drop for TestDaemon {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
        let _ = std::fs::remove_dir_all(&self.root);
    }
}

/// REQ-556 BR-1 / AC-2's substance, at a real terminal.
///
/// A session sitting idle at the entry prompt renders an event **when it
/// arrives**, with nothing typed. Against the pre-REQ binary the entry loop is
/// parked in `read_line`, nothing drains the channel, and the line does not
/// appear until a turn runs — which is the defect this REQ exists to fix.
#[test]
fn an_idle_session_renders_an_event_with_nothing_typed() {
    let Some(daemon_path) = daemon_or_skip() else {
        return;
    };
    let daemon = TestDaemon::spawn(&daemon_path);

    let pty = native_pty_system()
        .openpty(PtySize {
            rows: 40,
            cols: 100,
            pixel_width: 0,
            pixel_height: 0,
        })
        .expect("openpty");

    let mut cmd = CommandBuilder::new(teton_bin());
    cmd.env("XDG_RUNTIME_DIR", &daemon.runtime_dir);
    cmd.env("TETON_CONFIG", daemon.root.join("config.toml"));
    cmd.env("TETON_REPO_ROOT", &daemon.root);
    let mut session = pty.slave.spawn_command(cmd).expect("spawn teton under pty");
    // Drop the slave handle so the master sees EOF once the child exits;
    // holding it open would keep the reader thread alive forever.
    drop(pty.slave);
    let transcript = spawn_reader(pty.master.try_clone_reader().expect("pty reader"));

    // Wait until the session is genuinely idle at the entry prompt — a
    // state-derived sync point, not a sleep (LESSON-450).
    assert!(
        wait_for(&transcript, "ready (freeform)"),
        "the session never reached the entry prompt; transcript:\n{}",
        snapshot(&transcript)
    );

    // Isolation guard, and it earned its place: an early version of this test
    // silently attached to the developer's *real* daemon, so it passed for the
    // wrong reason and then failed for an unrelated one. `16.0 GiB` is the
    // fixture's pinned probe (`TETON_PROBE_RAM_BYTES`), so seeing it proves the
    // pty session is talking to the daemon this test started and not to
    // whatever else is listening on the machine.
    let attached = snapshot(&transcript);
    assert!(
        attached.contains("16.0 GiB"),
        "this session is not attached to the test daemon — the probe line does \
         not match the fixture's pinned hardware. Transcript:\n{attached}"
    );

    // Nothing is typed into this pty from here on. A second client attaches;
    // the daemon broadcasts that to the clients already subscribed — which is
    // the idle session above.
    let doctor = std::process::Command::new(teton_bin())
        .arg("doctor")
        .env("XDG_RUNTIME_DIR", &daemon.runtime_dir)
        .env("TETON_CONFIG", daemon.root.join("config.toml"))
        .env("TETON_REPO_ROOT", &daemon.root)
        .output()
        .expect("run a second client");
    assert!(
        doctor.status.success(),
        "the second client failed to attach: {}",
        String::from_utf8_lossy(&doctor.stderr)
    );

    let landed = wait_for(&transcript, "client attached");
    let final_transcript = snapshot(&transcript);
    // Kill the session rather than relying on EOF. The reader thread owns a
    // cloned master fd and never drops it, so closing our own master handle
    // does not hang up the slave — the child would sit at its prompt forever
    // and `wait()` would never return. Teardown is not what this test asserts,
    // so it takes the blunt route.
    let _ = session.kill();
    let _ = session.wait();

    assert!(
        landed,
        "an event that arrived while the session was idle never reached the \
         screen — the entry loop is not draining events between turns (BR-1). \
         Nothing was typed into the pty. Transcript:\n{final_transcript}"
    );
}
