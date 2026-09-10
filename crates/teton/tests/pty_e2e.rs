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

use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{Duration, Instant};

use portable_pty::{native_pty_system, CommandBuilder, PtySize};

mod common;
use common::{daemon_bin, teton_bin};

/// How long to wait for a marker before declaring it absent. Generous: this
/// asserts on **state reached**, never on a fixed sleep, so a slow machine costs
/// latency rather than a flake (LESSON-450) — and the cost lands only on a run
/// that is already failing, because a passing wait returns the moment its
/// marker arrives.
///
/// 60s rather than 20s (BUG-173): a degraded ubuntu runner spent a full 20s
/// window getting a correctly-behaving session to its entry prompt (CI run
/// 31853759487, PR #151, a docs-only change), so 20s was a ceiling real startup
/// could reach rather than a bound only a hang could. The entry-prompt wait
/// covers a client process spawn, two RPC round-trips, and a session create;
/// the ceiling has to be one that machine slowness cannot touch.
const WINDOW: Duration = Duration::from_secs(60);

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

/// Wait until `ready` accepts the accumulated transcript, or `WINDOW` elapses.
///
/// Polls accumulated state rather than sleeping a fixed interval and hoping —
/// a slow machine costs latency here, never a flake (LESSON-450).
///
/// The general form of [`wait_for`], which is a `contains` over this same loop.
/// REQ-592's tail leg needs a **positional** condition — one row landing before
/// a frame drawn later — and a substring test cannot express one: both halves
/// are present in the transcript from the moment the first arrives, so a pair of
/// `wait_for` calls would return on a state that has not happened yet.
fn wait_until(transcript: &Transcript, ready: impl Fn(&str) -> bool) -> bool {
    let deadline = Instant::now() + WINDOW;
    while Instant::now() < deadline {
        if ready(transcript.lock().expect("transcript mutex").as_str()) {
            return true;
        }
        std::thread::sleep(Duration::from_millis(25));
    }
    false
}

/// Wait until `marker` appears in the transcript, or `WINDOW` elapses.
fn wait_for(transcript: &Transcript, marker: &str) -> bool {
    wait_until(transcript, |seen| seen.contains(marker))
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
        Self::spawn_with(daemon, "", &["scripted reply"])
    }

    /// A daemon whose config carries `extra` (one more TOML table) and whose
    /// scripted tier replays `replies`, one per model call.
    ///
    /// REQ-563 needs both: `[web] tier` is config rather than a flag (web lookup
    /// is off by default, BR-1), and a lookup only happens if the scripted tier
    /// emits the tool call that asks for one.
    fn spawn_with(daemon: &Path, extra: &str, replies: &[&str]) -> Self {
        Self::spawn_with_env(daemon, extra, replies, &[])
    }

    /// As [`Self::spawn_with`], with `extra_env` on the daemon's environment.
    ///
    /// REQ-583's terminal test hands it a `HOME` under the fixture root — the
    /// daemon decides a root is "the home folder" by reading `HOME`, and the
    /// pty session it drives is given the same value — so the home a `/cd ~`
    /// moves to is a directory the test made, never the developer's own.
    fn spawn_with_env(
        daemon: &Path,
        extra: &str,
        replies: &[&str],
        extra_env: &[(&str, &std::ffi::OsStr)],
    ) -> Self {
        static SEQ: AtomicUsize = AtomicUsize::new(0);
        // Per-daemon, so two tests in this file never share a root, a socket, or
        // the single-instance flock — and so neither `drop` deletes the other's
        // working directory. Short, because the root becomes an
        // `XDG_RUNTIME_DIR` and the socket under it has to fit in `SUN_LEN`.
        let root = PathBuf::from("/tmp").join(format!(
            "tcpty{:x}-{:x}",
            std::process::id() & 0xffff,
            SEQ.fetch_add(1, Ordering::SeqCst)
        ));
        let runtime_dir = root.join("x");
        std::fs::create_dir_all(&runtime_dir).unwrap();
        let config_path = root.join("config.toml");
        // A closed port for the model host: nothing here may reach the network,
        // and no weights are needed for what this file asserts.
        std::fs::write(
            &config_path,
            format!(
                "[[providers]]\nid = \"deepseek\"\nkind = \"openai-compatible\"\n\
                 endpoint = \"https://api.deepseek.com\"\n\n\
                 [local_model]\nauto_accept = false\nbase_url = \"http://127.0.0.1:9\"\n\n\
                 {extra}"
            ),
        )
        .unwrap();
        // A scripted tier: the engine is present from construction, so no
        // consent prompt stands between the session and the entry prompt.
        let script = root.join("local_script.txt");
        std::fs::write(&script, replies.join("\n---\n")).unwrap();

        let log = std::fs::File::create(root.join("tetond.log")).unwrap();
        let mut command = std::process::Command::new(daemon);
        command
            // REQ-565: a fixture daemon whose lifetime this test owns (killed in
            // `Drop`). Without `never` it would exit when the PTY session
            // disconnects, and a later command in the same test would autostart
            // a replacement that never saw `TETON_CONFIG`. The lifetime itself
            // is covered by `tetond/tests/daemon_lifetime.rs`.
            .args(["--shutdown-policy", "never"])
            .env("XDG_RUNTIME_DIR", &runtime_dir)
            // REQ-611 TASK-364: `resolve_data_dir` falls back to the
            // developer's own home when this is unset, and every daemon prunes
            // its transcript directory at start — so an unset variable would
            // have this fixture run a deletion pass over the machine it is
            // testing on. Under `root`, which `Drop` removes.
            .env("XDG_DATA_HOME", root.join("d"))
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
            .stderr(std::process::Stdio::from(log));
        for (key, value) in extra_env {
            command.env(key, value);
        }
        let child = command.spawn().expect("spawn daemon");
        let daemon = Self {
            root,
            runtime_dir,
            child,
        };
        // Constructed before the wait so a panic below still runs `Drop` —
        // the child is killed and the root removed, not leaked.
        daemon.wait_for_socket();
        daemon
    }

    /// Block until this daemon accepts a connection, or panic with its log
    /// (BUG-173).
    ///
    /// The same barrier `cli_e2e`'s fixture has always had, and the piece this
    /// copy of its shape dropped. The daemon binds its socket only after
    /// `DaemonRuntime::from_env` finishes (`tetond/src/main.rs`, H-1: the order
    /// is load-bearing) and serves the accept loop immediately after, so a
    /// successful connect means startup is over — everything a test's
    /// `wait_for` window still has to cover is client-side. Without the
    /// barrier, the entry-prompt window absorbed daemon startup as well, and on
    /// a degraded CI runner the sum crossed the window with every process
    /// behaving correctly.
    ///
    /// The barrier also closes a race meaner than slowness. A pty client that
    /// reached a not-yet-bound socket walked `teton`'s autostart path and
    /// spawned a *second* daemon from beside its own binary — one that
    /// inherited none of this fixture's seams (`TETON_LOCAL_SCRIPT`,
    /// `TETON_TEST_SEAMS`, the probe pins) and raced the fixture for the
    /// single-instance flock; on a win it served the session with the real
    /// machine's probe and no scripted tier. BUG-164's resolution records that
    /// exact signature (the CLI "autostarted its own, hitting the
    /// model-consent prompt and timing out"). A fixture that is known-ready
    /// before any client exists makes the autostart path unreachable.
    fn wait_for_socket(&self) {
        let socket = self.runtime_dir.join("teton").join("tetond.sock");
        let deadline = Instant::now() + WINDOW;
        while Instant::now() < deadline {
            // A connect, not an existence check: the path appears at `bind`,
            // but only a successful connect proves the accept loop is serving.
            // The probe connection is dropped unhandshaken, which the daemon
            // treats as any other departing client (`cli_e2e` has done exactly
            // this before every test since its fixture existed).
            if std::os::unix::net::UnixStream::connect(&socket).is_ok() {
                return;
            }
            std::thread::sleep(Duration::from_millis(25));
        }
        let log = std::fs::read_to_string(self.root.join("tetond.log")).unwrap_or_default();
        panic!(
            "daemon socket never appeared at {}. log:\n{log}",
            socket.display()
        );
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
    let daemon_path = daemon_bin();
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
    // REQ-611 TASK-364: the same data directory the fixture daemon got, so a
    // CLI that autostarts one lands in `root` rather than the developer's home.
    cmd.env("XDG_DATA_HOME", daemon.root.join("d"));
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
        // REQ-611 TASK-364: the same data directory the fixture daemon got.
        .env("XDG_DATA_HOME", daemon.root.join("d"))
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

// ---------------------------------------------------------------------------
// REQ-563 AC-6: the web capability's status row (TASK-078)
// ---------------------------------------------------------------------------

/// AC-6's status-line clause, at a real terminal.
///
/// `main::paint_status` draws the web row above the framed entry prompt, and the
/// framed prompter exists only at a TTY — REQ-556 BR-2 keeps a piped run
/// byte-identical to what it was, so `cli_e2e.rs` is *structurally* blind to
/// this row and this file is the only place it can be observed.
///
/// The row is deliberately absent until the capability is engaged (BR-1: a
/// machine that never opted in must see the layout it always saw), so the test
/// has to engage it: a scripted turn asks for a page, the user answers the
/// consent prompt, and the `web_consent_decided` that comes back is what raises
/// the field. The fetch target is a loopback port nothing listens on, so
/// nothing reaches a network.
#[test]
fn the_status_row_shows_the_session_s_web_capability() {
    let daemon_path = daemon_bin();
    let url = format!("http://127.0.0.1:{}/tokio", closed_port());
    let tool_call = format!("{{\"tool\": \"web\", \"arguments\": {{\"url\": \"{url}\"}}}}");
    // Every tier bound to the local (scripted) tier, so the turn is served here
    // rather than resolving to the unreachable remote provider — the same
    // binding `cli_e2e`'s fixture makes, for the same REQ-558 reason.
    let tiers: String = ["reflex", "scan", "build", "think"]
        .iter()
        .map(|t| format!("[[tiers]]\ntier = \"{t}\"\nprovider_id = \"local\"\n\n"))
        .collect();
    let config = format!(
        "[[providers]]\nid = \"local\"\nkind = \"local\"\n\n\
         {tiers}\
         [web]\ntier = \"fetch_any_url\"\n"
    );
    let daemon = TestDaemon::spawn_with(
        &daemon_path,
        &config,
        &[&tool_call, "I could not reach that page."],
    );

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
    // REQ-611 TASK-364: the same data directory the fixture daemon got, so a
    // CLI that autostarts one lands in `root` rather than the developer's home.
    cmd.env("XDG_DATA_HOME", daemon.root.join("d"));
    cmd.env("TETON_CONFIG", daemon.root.join("config.toml"));
    cmd.env("TETON_REPO_ROOT", &daemon.root);
    let mut session = pty.slave.spawn_command(cmd).expect("spawn teton under pty");
    drop(pty.slave);
    let transcript = spawn_reader(pty.master.try_clone_reader().expect("pty reader"));
    let mut writer = pty.master.take_writer().expect("pty writer");

    assert!(
        wait_for(&transcript, "ready (freeform)"),
        "the session never reached the entry prompt; transcript:\n{}",
        snapshot(&transcript)
    );
    // Non-vacuity: nothing has engaged the capability yet, so no row is drawn.
    assert!(
        !snapshot(&transcript).contains("web:"),
        "the status row must be absent until the capability is engaged (BR-1); \
         transcript:\n{}",
        snapshot(&transcript)
    );

    writer
        .write_all(b"what does the tokio page say about task pinning?\r")
        .expect("type the prompt");
    writer.flush().ok();

    assert!(
        // The model composed this URL (the typed prompt names no link), so the
        // per-tier key is the any-URL one — asserted in full, so a regression
        // that collapsed the three keys back to one would be visible here.
        wait_for(&transcript, "permission requested: web_fetch_any_url"),
        "the lookup never asked for consent; transcript:\n{}",
        snapshot(&transcript)
    );
    writer.write_all(b"y\r").expect("answer the prompt");
    writer.flush().ok();

    let shown = wait_for(&transcript, "web: fetch");
    let final_transcript = snapshot(&transcript);
    let _ = session.kill();
    let _ = session.wait();

    assert!(
        shown,
        "the status row never reported this session's web capability (AC-6); \
         transcript:\n{final_transcript}\ndaemon log:\n{}",
        std::fs::read_to_string(daemon.root.join("tetond.log")).unwrap_or_default()
    );
}

/// A loopback port with nothing listening on it.
fn closed_port() -> u16 {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind to find a free port");
    let port = listener.local_addr().unwrap().port();
    drop(listener);
    port
}

/// **REQ-560 AC-10 / BR-11: the status row renders below the bottom rule, and a
/// redraw strands no row in either direction.**
///
/// This is the criterion BR-11 exists for and it cannot be reached on a pipe:
/// the frame renders only at a TTY (BR-9), so the arithmetic that places the row
/// — and the arithmetic that takes it back — is unobservable everywhere else.
/// The unit tests in `prompt.rs` pin the *bytes* `draw` and `read_line` would
/// write; this pins what a real terminal does with them.
///
/// Three claims, in the order they can be established:
///
/// 1. the row is drawn, and drawn **below** the bottom rule;
/// 2. a typed line is accepted intact with the frame uncorrupted — i.e. the
///    cursor really did land in the input row and not on the status row;
/// 3. after a redraw (an event arriving while the frame is open, which is what
///    REQ-556's indicator does) neither the row above nor the row below is left
///    stranded — the transcript ends with exactly one of each.
#[test]
fn the_status_row_renders_below_the_frame_and_survives_a_redraw() {
    let daemon_path = daemon_bin();
    // A scripted tier so a typed prompt actually produces a turn — which is what
    // forces the frame down and redraws it, the redraw this test is about. Every
    // tier is bound to the local (scripted) provider, the same binding the web
    // test above makes for the same REQ-558 reason: otherwise the turn resolves
    // to the unreachable remote provider and fails before it can redraw
    // anything.
    let tiers: String = ["reflex", "scan", "build", "think"]
        .iter()
        .map(|t| format!("[[tiers]]\ntier = \"{t}\"\nprovider_id = \"local\"\n\n"))
        .collect();
    let config = format!("[[providers]]\nid = \"local\"\nkind = \"local\"\n\n{tiers}");
    let daemon = TestDaemon::spawn_with(&daemon_path, &config, &["a scripted reply."]);

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
    // REQ-611 TASK-364: the same data directory the fixture daemon got, so a
    // CLI that autostarts one lands in `root` rather than the developer's home.
    cmd.env("XDG_DATA_HOME", daemon.root.join("d"));
    cmd.env("TETON_CONFIG", daemon.root.join("config.toml"));
    cmd.env("TETON_REPO_ROOT", &daemon.root);
    let mut session = pty.slave.spawn_command(cmd).expect("spawn teton under pty");
    drop(pty.slave);
    let transcript = spawn_reader(pty.master.try_clone_reader().expect("pty reader"));
    let mut writer = pty.master.take_writer().expect("pty writer");

    assert!(
        wait_for(&transcript, "ready (freeform)"),
        "the session never reached the entry prompt; transcript:\n{}",
        snapshot(&transcript)
    );

    // (1) The row is there, at a real terminal, naming the session's level.
    assert!(
        wait_for(&transcript, "permissions: guarded"),
        "the status row never rendered at a tty (AC-10); transcript:\n{}",
        snapshot(&transcript)
    );
    // …and it carries **both** values it is specified to carry, now that
    // REQ-559 has landed. The permission half alone would satisfy the assertion
    // above while the effort seam sat unwired, so the second field is asserted
    // separately. The *value* deliberately is not: what it resolves to depends
    // on which providers the fixture registers, and the claim here is that the
    // field reaches the terminal, not what the resolver decided.
    let framed_once = snapshot(&transcript);
    let row = framed_once
        .lines()
        .find(|line| line.contains("permissions: guarded"))
        .expect("just asserted present");
    assert!(
        row.contains("effort: "),
        "the status row must carry the effort field beside the permission one \
         (REQ-560 status line + REQ-559 value); row: {row:?}"
    );

    let framed = snapshot(&transcript);
    // …and it is BELOW the bottom rule, which is the placement BR-11 specifies.
    // The rule is a run of box-drawing characters; the last one before the
    // status row is the bottom rule, so the row must come after it.
    let row_at = framed
        .find("permissions: guarded")
        .expect("just asserted present");
    let rule_before = framed[..row_at]
        .rfind('\u{2500}')
        .expect("the frame draws a rule above the status row");
    assert!(
        rule_before < row_at,
        "the status row must sit below the bottom rule; transcript:\n{framed}"
    );
    // The cursor-up escape that puts the caret back in the input row must be the
    // four-row form. `\x1b[2A` would mean the row was never counted, and the
    // caret would land on the bottom rule.
    assert!(
        framed.contains("\x1b[3A"),
        "with a status row the caret must rise three rows, not two; \
         transcript:\n{framed}"
    );

    // (2) A typed line is accepted intact with the frame uncorrupted.
    writer
        .write_all(b"hello from the entry row\r")
        .expect("type the prompt");
    writer.flush().ok();
    assert!(
        wait_for(&transcript, "a scripted reply."),
        "the typed line never produced a turn, so the caret was not in the input \
         row; transcript:\n{}",
        snapshot(&transcript)
    );

    // (3) The frame was torn down and redrawn around that turn. Neither
    // direction may be left stranded: the transcript's tail must hold exactly
    // one status row, below exactly one intact frame.
    assert!(
        wait_for(&transcript, "permissions: guarded"),
        "the status row did not come back after the redraw; transcript:\n{}",
        snapshot(&transcript)
    );
    let after = snapshot(&transcript);
    let _ = session.kill();
    let _ = session.wait();

    // A stranded row shows up as a status row with no frame above it in the
    // tail — the shape a redraw that erased three rows but drew four would
    // leave. Counting the whole transcript would be meaningless (every redraw
    // legitimately writes another), so the assertion is about the tail: the last
    // status row must still have a rule above it.
    let last_row = after
        .rfind("permissions: guarded")
        .expect("just asserted present");
    assert!(
        after[..last_row].contains('\u{2500}'),
        "after a redraw the status row must still sit under a frame, not alone; \
         transcript:\n{after}"
    );
    // And the redraw really did happen — otherwise (3) is asserting nothing.
    assert!(
        after.matches("permissions: guarded").count() > 1,
        "the frame was never redrawn, so this leg proves nothing; transcript:\n{after}"
    );
}

// ---------------------------------------------------------------------------
// REQ-572 AC-5 — secret hygiene, at a real terminal (TASK-133)
// ---------------------------------------------------------------------------
//
// AC → test map for this section:
//
//   AC-5 (input echo is off at the key step) + AC-5 (the planted key appears in
//   no transcript, no config file, no daemon log)
//       → `the_key_step_does_not_echo_and_the_key_reaches_nothing`
//
// AC-5's remaining surfaces, and where each is asserted:
//
//   * **no event payload, no RPC frame** — the daemon is never sent the value:
//     only `search_key_ref` crosses the wire. Pinned at the client's own seam by
//     `web_setup_ui`'s `a_full_walk_stores_the_key_and_sends_only_its_reference`
//     (the planted key swept out of the serialized preview *and* commit frames
//     and out of every rendered line), and by this file's daemon-log sweep.
//   * **"egress-capture shows zero packets attributable to the flow itself"** —
//     `tetond`'s `web_setup_flow.rs`, where a live HTTP server watches the whole
//     plan → preview → commit and receives nothing.
//
// The walk here stops at the confirm rather than completing, and the reason is
// not convenience: see the test's own doc comment. The shipped CLI writes to the
// real OS keychain, and a completed walk would put a credential in — and then
// possibly take one out of — whoever's login keychain ran the suite.

/// The key this test types. Distinctive enough that finding it anywhere is
/// unambiguous, and it is a plausible credential rather than a marker word so
/// nothing downstream can be excused for treating it as test noise.
const PLANTED_KEY: &str = "sk-web-PLANTED-DO-NOT-ECHO-4Kq2vZ";

/// Typed at the prompt immediately before the key prompt, where the client
/// renders it nowhere. It is the control for [`PLANTED_KEY`]'s absence: the
/// same terminal, the same reader thread, one question apart.
const ECHO_WITNESS: &str = "yes-echo-witness-7Qx";

/// **AC-5: the key step does not echo, and the key appears nowhere afterwards.**
///
/// This is the one claim in REQ-572 that a pipe is structurally blind to.
/// `cli_e2e.rs` can drive the whole walkthrough — it does — but a pipe has no
/// `ECHO` bit to clear, so "input echo is off at the key step" is unobservable
/// there, and the transcript it captures is the CLI's own output rather than
/// what a terminal put on screen. Only a pty carries both.
///
/// ## The non-vacuity leg, and why it is the important half
///
/// A transcript that recorded no typed input at all would pass a "the key is
/// absent" assertion while proving nothing. So the walk types the **endpoint**
/// at an ordinary `ask` prompt first and asserts it *does* appear: the tty is
/// echoing, the reader thread is capturing it, and the key's absence a few
/// lines later is therefore the `EchoOff` guard doing its job and not a blind
/// instrument.
///
/// ## Why the walk stops at the confirm
///
/// The shipped `teton` writes credentials to the **real OS keychain**
/// (`keychain::default_keychain`); there is no test seam that redirects it, and
/// adding one would mean shipping a debug build that can be talked into writing
/// a plaintext secret somewhere else. A confirmed walk would therefore create —
/// and, on a refused commit, delete — a `teton/web-search` entry in whoever's
/// login keychain ran the suite, destroying a real credential if one was there.
/// No test may do that.
///
/// The step this stops short of is `Keychain::store` → `web/setup_commit`, and
/// it is pinned against a fake keychain in `web_setup_ui`'s own suite
/// (`a_full_walk_stores_the_key_and_sends_only_its_reference`, which sweeps the
/// planted key out of the serialized preview and commit frames and out of every
/// rendered line). What this test owns is everything a fake keychain cannot
/// reach: the terminal's echo bit, and the bytes a screen actually showed.
#[test]
fn the_key_step_does_not_echo_and_the_key_reaches_nothing() {
    let daemon_path = daemon_bin();
    // Every tier bound to the scripted local tier, so the session is servable
    // and — the part this test needs — the daemon reports a local model, which
    // is what makes the `search` tier (the only branch that asks for a key)
    // offerable at all.
    let tiers: String = ["reflex", "scan", "build", "think"]
        .iter()
        .map(|t| format!("[[tiers]]\ntier = \"{t}\"\nprovider_id = \"local\"\n\n"))
        .collect();
    let config = format!("[[providers]]\nid = \"local\"\nkind = \"local\"\n\n{tiers}");
    let daemon = TestDaemon::spawn_with(&daemon_path, &config, &["a scripted reply."]);
    let config_path = daemon.root.join("config.toml");

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
    // REQ-611 TASK-364: the same data directory the fixture daemon got, so a
    // CLI that autostarts one lands in `root` rather than the developer's home.
    cmd.env("XDG_DATA_HOME", daemon.root.join("d"));
    cmd.env("TETON_CONFIG", &config_path);
    cmd.env("TETON_REPO_ROOT", &daemon.root);
    let mut session = pty.slave.spawn_command(cmd).expect("spawn teton under pty");
    drop(pty.slave);
    let transcript = spawn_reader(pty.master.try_clone_reader().expect("pty reader"));
    let mut writer = pty.master.take_writer().expect("pty writer");

    assert!(
        wait_for(&transcript, "ready (freeform)"),
        "the session never reached the entry prompt; transcript:\n{}",
        snapshot(&transcript)
    );

    // Read the baseline **here**, not at spawn: a starting daemon rewrites its
    // own config once (the REQ-557 model migration normalises the document), so
    // bytes read before the socket was up would compare a pre-migration file
    // against a post-migration one and report a write this walk never made.
    // Reaching the entry prompt is the state that says startup is over.
    let before = std::fs::read(&config_path).expect("the fixture config exists");

    // Every step is a state-derived sync point: type, then wait for the next
    // question to appear. Never a sleep (LESSON-450).
    let step = |writer: &mut Box<dyn Write + Send>, text: &str, until: &str| {
        writer
            .write_all(text.as_bytes())
            .expect("type into the pty");
        writer.flush().ok();
        assert!(
            wait_for(&transcript, until),
            "the walk never reached {until:?}; transcript:\n{}",
            snapshot(&transcript)
        );
    };

    step(&mut writer, "/web setup\r", "tier [1-3");
    // `3` is `search` — the only tier that asks for an endpoint and a key.
    step(&mut writer, "3\r", "search endpoint");
    const ENDPOINT: &str = "https://api.search.brave.com/res/v1/web/search";
    step(
        &mut writer,
        &format!("{ENDPOINT}\r"),
        "does this backend need an API key?",
    );
    // THE ECHO WITNESS. `is_yes_by_default` reads anything that is not `n`/`no`
    // as yes, so this answer means "yes, it needs a key" — and nothing in the
    // client ever renders it back. Its presence in the transcript can therefore
    // only be the **terminal echoing what was typed**, which is the property the
    // key assertion below depends on. (The endpoint would not do: the preview
    // prints it, so its appearance proves rendering, not echo.)
    step(
        &mut writer,
        &format!("{ECHO_WITNESS}\r"),
        "auth header template",
    );
    // Empty: take whatever the prompt offered. The endpoint above is Brave's,
    // so what is offered — and what an empty answer therefore sends — is
    // Brave's own `X-Subscription-Token: {key}` and not the generic Bearer.
    step(&mut writer, "\r", "API key (not shown");

    // THE STEP THIS TEST IS ABOUT.
    step(
        &mut writer,
        &format!("{PLANTED_KEY}\r"),
        "write this to your config?",
    );

    // Decline: the walk stops one step before the keychain write (see the doc
    // comment). Everything a screen could have shown has now been shown.
    writer.write_all(b"n\r").expect("decline the confirm");
    writer.flush().ok();
    let declined = wait_for(&transcript, "no key was stored");

    let final_transcript = snapshot(&transcript);
    let _ = session.kill();
    let _ = session.wait();

    assert!(
        declined,
        "the confirm step never resolved; transcript:\n{final_transcript}"
    );

    // (1) NON-VACUITY. The witness was typed at the prompt immediately before
    // the key prompt, is echoed by the tty and rendered by nothing, and it is
    // here. So this transcript records typed input, the reader thread is
    // capturing it, and the key's absence below is echo suppression rather than
    // a blind instrument.
    assert!(
        final_transcript.contains(ECHO_WITNESS),
        "the pty transcript records nothing the user typed at an ordinary \
         prompt, so the sweep below would prove nothing; \
         transcript:\n{final_transcript}"
    );
    // And the walk really did get as far as the endpoint (which the preview
    // renders, so this is a progress check and not a second echo check).
    assert!(
        final_transcript.contains(ENDPOINT),
        "the walk never previewed the endpoint; transcript:\n{final_transcript}"
    );
    // The empty answer above took what the prompt offered, and the endpoint
    // typed was Brave's — so what the offer *said* is now an assertion rather
    // than a comment about one (REQ-573). Pinned in the **prompt's own wording**,
    // not by bare containment: the preview below carries the same template, so a
    // containment check would pass on a prompt that had offered the generic
    // Bearer default and a walk that had gone on to send it.
    assert!(
        final_transcript.contains("auth header template [Enter for `X-Subscription-Token: {key}`]"),
        "the auth prompt must offer Brave's own header for a Brave endpoint — \
         the generic Bearer default in its place is a config Brave answers 401 \
         to; transcript:\n{final_transcript}"
    );
    // …and the empty answer really did take it: the previewed table carries the
    // offered template, which is the half a prompt's wording cannot prove.
    assert!(
        final_transcript.contains("search_auth = \"X-Subscription-Token: {key}\""),
        "Enter at the auth prompt must send what was offered; \
         transcript:\n{final_transcript}"
    );

    // (2) AC-5: the key was typed into the same terminal and never appeared.
    assert!(
        !final_transcript.contains(PLANTED_KEY),
        "AC-5 VIOLATION: the API key was echoed to the terminal. The key step \
         must clear ECHO for the duration of the read; transcript:\n{final_transcript}"
    );
    // Not even a fragment: a partial echo is a leaked credential too.
    assert!(
        !final_transcript.contains("PLANTED-DO-NOT-ECHO"),
        "AC-5 VIOLATION: part of the API key reached the terminal; \
         transcript:\n{final_transcript}"
    );

    // (3) The preview the user *did* read carried the reference and not the
    // value — which is what makes the confirm step honest (BR-6/BR-7).
    assert!(
        final_transcript.contains("keychain://teton/web-search"),
        "the preview must show the keychain reference the commit would write; \
         transcript:\n{final_transcript}"
    );

    // (4) Nothing was written, and the config the daemon holds is untouched.
    assert_eq!(
        std::fs::read(&config_path).ok().as_deref(),
        Some(before.as_slice()),
        "a declined confirm must leave the config byte-identical"
    );

    // (5) The daemon never saw the key at all — only a reference crossed the
    // wire, so nothing it logs can contain the value.
    let log = std::fs::read_to_string(daemon.root.join("tetond.log")).unwrap_or_default();
    assert!(
        !log.contains(PLANTED_KEY),
        "AC-5 VIOLATION: the API key reached the daemon's log:\n{log}"
    );
}

/// **REQ-579 ADR-9 / AC-1's deterministic half, at a real terminal.**
///
/// Three live rounds proved the shipped local model will not volunteer
/// `/provider setup` from the guide (verification.md §1–§24): 0/9 replies named
/// it, while the endpoint and model transferred every time. ADR-9 moves the
/// guarantee off the prompt and onto the surface — when the reply reaches for
/// the shell recipe, the harness says the session has a command for it.
///
/// This belongs in the pty suite and nowhere else. The nudge is gated on
/// `typed_input`, so `cli_e2e` is *structurally blind* to the positive case the
/// same way it is blind to the loading indicator; the piped suite can only hold
/// the negative (`a_piped_session_whose_reply_recites_the_cli_gets_no_hand_off_line`).
/// And the unit tests can only pin the function's own gate — that `main` passes
/// it the session's real terminal flag, at the one place a typed turn ends, is
/// visible only from outside the process.
///
/// The reply is scripted rather than solicited, which is the point: the trigger
/// is a deterministic match on text the model already emitted, so a fixture that
/// emits that text is the honest test of it. What the *live* model says is the
/// recorded result in verification.md, not something a test can pin.
#[test]
fn a_reply_reciting_the_cli_earns_the_hand_off_line_at_a_terminal() {
    let daemon_path = daemon_bin();
    // Every tier bound to the scripted local tier, for the REQ-558 reason the
    // other typed-turn tests here bind them: otherwise the turn resolves to an
    // unreachable remote provider and fails before it can produce a reply.
    let tiers: String = ["reflex", "scan", "build", "think"]
        .iter()
        .map(|t| format!("[[tiers]]\ntier = \"{t}\"\nprovider_id = \"local\"\n\n"))
        .collect();
    let config = format!("[[providers]]\nid = \"local\"\nkind = \"local\"\n\n{tiers}");
    // The answer the guide actually produces at the front door: the shell
    // recipe, with no mention of the in-session command.
    let reply = "register it from a shell: teton provider add kimi --kind \
                 openai-compatible.";
    // REQ-592 AC-10's terminal leg rides this fixture, so the shape it needs is
    // pinned here rather than left to luck: **the reply's last line carries no
    // trailing newline**, which is the common case for a model reply and the one
    // a streaming renderer holds. Assertions (1) and (5) below are then the two
    // halves of AC-10 at a real terminal — the tail is on screen at all, and it
    // is on screen *before* the `hand_off_after_turn` line and the entry frame
    // that follows it. If this fixture ever grows a trailing newline, that
    // coverage evaporates silently, which is what this line exists to prevent.
    assert!(
        !reply.ends_with('\n'),
        "REQ-592 AC-10 needs a reply whose final chunk has no trailing newline"
    );
    let daemon = TestDaemon::spawn_with(&daemon_path, &config, &[reply]);

    // Wide enough that the sentence under test occupies one row.
    //
    // **Since REQ-592 the wrap that could break it is the CLI's, not the
    // terminal's** (ADR-8). Before that REQ a hard wrap was a display artifact:
    // the pty master received whatever bytes `teton` wrote, so an over-wide
    // assistant line still reached this transcript contiguous and a `cols` that
    // was too small cost nothing here. `BR-3` now puts **real `\n` bytes**
    // between the rows, so a marker longer than `cols` would arrive genuinely
    // split and an assertion about wording would fail for a reason that is not
    // about wording.
    //
    // 200 against a 75-column reply, so the margin is wide. TASK-283 re-ran this
    // rather than assuming it: the reply is one rendered row, `teton provider
    // add kimi` is inside it, and the hand-off below is a `line()` kind, which
    // OQ-5 leaves unwrapped at any width.
    let pty = native_pty_system()
        .openpty(PtySize {
            rows: 40,
            cols: 200,
            pixel_width: 0,
            pixel_height: 0,
        })
        .expect("openpty");

    let mut cmd = CommandBuilder::new(teton_bin());
    cmd.env("XDG_RUNTIME_DIR", &daemon.runtime_dir);
    // REQ-611 TASK-364: the same data directory the fixture daemon got, so a
    // CLI that autostarts one lands in `root` rather than the developer's home.
    cmd.env("XDG_DATA_HOME", daemon.root.join("d"));
    cmd.env("TETON_CONFIG", daemon.root.join("config.toml"));
    cmd.env("TETON_REPO_ROOT", &daemon.root);
    let mut session = pty.slave.spawn_command(cmd).expect("spawn teton under pty");
    drop(pty.slave);
    let transcript = spawn_reader(pty.master.try_clone_reader().expect("pty reader"));
    let mut writer = pty.master.take_writer().expect("pty writer");

    assert!(
        wait_for(&transcript, "ready (freeform)"),
        "the session never reached the entry prompt; transcript:\n{}",
        snapshot(&transcript)
    );

    writer
        .write_all(b"how do I add kimi?\r")
        .expect("type the prompt");
    writer.flush().ok();

    // (1) The precondition: the turn ran and the reply really did recite the
    // CLI. Without this the assertion below could pass on a session that never
    // reached the model.
    assert!(
        wait_for(&transcript, "teton provider add kimi"),
        "the scripted reply never arrived, so nothing armed the hand-off; \
         transcript:\n{}",
        snapshot(&transcript)
    );

    // (2) The hand-off followed it.
    const HAND_OFF: &str =
        "in this session, /provider setup <vendor> [tier] does this without leaving it; \
         no key in chat.";
    assert!(
        wait_for(&transcript, HAND_OFF),
        "ADR-9: a recital at a terminal must be followed by the hand-off; \
         transcript:\n{}",
        snapshot(&transcript)
    );

    let seen = snapshot(&transcript);
    let _ = session.kill();
    let _ = session.wait();

    // (3) Once, not once per matched command and not once per redraw.
    assert_eq!(
        seen.matches(HAND_OFF).count(),
        1,
        "the hand-off is once per turn; transcript:\n{seen}"
    );

    // (4) In the harness's voice, not the model's — it is a `>>` notice, which
    // is the whole reason a user can tell it apart from the answer it follows.
    let line = seen
        .lines()
        .find(|line| line.contains(HAND_OFF))
        .expect("just asserted present");
    assert!(
        line.contains(">>"),
        "the hand-off must render as a harness notice; line: {line:?}"
    );

    // (5) After the reply, not before it: it is a hand-off from an answer the
    // user has already read.
    //
    // **REQ-592 AC-10 at a real terminal.** The fixture's reply ends without a
    // newline (pinned above), so its last row is exactly the tail a streaming
    // renderer holds — and this is the assertion that it is not still being
    // held: `end_block()` runs at the end of `Connection::call`, which is before
    // `main.rs` reaches `hand_off_after_turn` and before the entry frame is
    // redrawn. A tail released too late would land on the wrong side of this
    // comparison; a tail never released would fail step (1) above.
    let reply_at = seen
        .find("teton provider add kimi")
        .expect("asserted present");
    let hand_off_at = seen.find(HAND_OFF).expect("asserted present");
    assert!(
        reply_at < hand_off_at,
        "the hand-off must follow the reply it is about; transcript:\n{seen}"
    );
}

// ---------------------------------------------------------------------------
// REQ-582 AC-3 — `/provider add` at a real terminal (TASK-173)
// ---------------------------------------------------------------------------
//
// ## What this covers, and the one thing no test in this repository can
//
// AC-3 asks for a `/provider add` on a TTY that reads the key echo-off, stores
// it in the keychain, registers the provider, and leaks the key nowhere. Three
// of those four are here. The keychain store is not, and the reason is the same
// one `the_key_step_does_not_echo_and_the_key_reaches_nothing` records for
// `/web setup` one section above: **the shipped `teton` writes credentials to
// the real OS keychain** (`keychain::default_keychain()`, which the session's
// dispatcher hands to `provider_add_on`), and there is no seam that redirects it
// in the binary. So any test that types a key at this pty would create — and,
// on a rejected registration, delete — an entry in whoever's login keychain ran
// the suite. No test may do that, and adding a redirect seam would mean shipping
// a build that can be talked into writing a plaintext secret somewhere else.
//
// So the key is never typed here, and the echo claim is made **fail-closed**
// instead of by sweeping a transcript for a value:
//
//   * `StdinPrompter::ask_secret` clears `ECHO` before it reads and refuses to
//     read at all if it cannot (`EchoState::Failed` → `ECHO_UNAVAILABLE`,
//     nothing read, nothing stored).
//   * Under a pty, stdin *is* a terminal, so `EchoState::NoTerminal` — the one
//     state that reads unhidden — is unreachable by construction.
//   * Therefore a run that reached the read (the prompt was answered and the
//     flow moved on) and did **not** print `ECHO_UNAVAILABLE` is a run in which
//     echo was actually off for the read.
//
// The bytes-on-a-screen half of the same claim — a real credential typed at a
// real terminal appearing nowhere — is `the_key_step_does_not_echo_and_the_key_
// reaches_nothing`, over the *same* `Prompter::ask_secret` seam this row reads
// through (`read_secret(id, prompter)`, ADR-3). What REQ-582 changed is which
// prompter that seam is handed, not what it does.
//
// The rest of AC-3 — the flow completing against a keychain double, the key
// reaching that double under the id, the `config/set` crossing the wire with a
// `keychain://` reference and never the key, and a refused registration taking
// the key back out — is pinned in-process by `main.rs`'s `provider_add_on`
// tests over `MockKeychain` (REQ-582 verify M4), since the keychain is now a
// parameter of the body rather than a value it builds for itself.
//
// Since the verify pass (M1) the session also **confirms before it reads**: a
// default-no question naming the settled registration comes first, so a
// multi-line paste's second line answers "no" instead of becoming the key. The
// walk below answers it, which is what makes the credential prompt reachable
// at all.
//
// This test types no credential by design (see above): the shipped binary would
// put it in the real OS keychain. It answers the confirmation and then presses
// return at the key prompt, and nothing else.

/// **AC-3 (terminal half): `/provider add` runs at a TTY, confirms, asks for
/// its key through the hiding prompt, and stores nothing when none is typed —
/// while the one kind that needs no key registers end to end.**
///
/// Two rows through one session, because the pair is what makes each half mean
/// something:
///
/// * `--kind openai-compatible` reaches the credential step. The write gate let
///   it through (this is typed input, ADR-4), clap parsed the four flags, the
///   duplicate probe and the endpoint settlement passed, the session asked its
///   default-no confirmation and got a `y`, and the session's own prompter asked
///   for the key. An empty answer refuses; nothing is registered and
///   `config.toml` is byte-identical.
/// * `--kind local` needs no credential, so it goes all the way — and asks no
///   confirmation, since there is no key read to guard: the daemon applies the
///   registration and the config on disk gains the provider. Without it, "the
///   config did not change" above would be indistinguishable from a row that
///   cannot write at all.
#[test]
fn a_session_provider_add_asks_for_its_key_echo_off_and_stores_nothing_untyped() {
    let daemon_path = daemon_bin();
    // Every tier on the scripted local tier, for the REQ-558 reason the other
    // typed-turn tests here bind them: a session that cannot serve a turn cannot
    // reach its entry prompt in the state these steps assume.
    let tiers: String = ["reflex", "scan", "build", "think"]
        .iter()
        .map(|t| format!("[[tiers]]\ntier = \"{t}\"\nprovider_id = \"local\"\n\n"))
        .collect();
    let config = format!("[[providers]]\nid = \"local\"\nkind = \"local\"\n\n{tiers}");
    let daemon = TestDaemon::spawn_with(&daemon_path, &config, &["a scripted reply."]);
    let config_path = daemon.root.join("config.toml");

    let pty = native_pty_system()
        .openpty(PtySize {
            rows: 40,
            // Wide enough that the command lines below are not hard-wrapped: a
            // wrapped line is still correct output, but it would split a marker
            // and fail an assertion about wording rather than about behaviour.
            cols: 200,
            pixel_width: 0,
            pixel_height: 0,
        })
        .expect("openpty");

    let mut cmd = CommandBuilder::new(teton_bin());
    cmd.env("XDG_RUNTIME_DIR", &daemon.runtime_dir);
    // REQ-611 TASK-364: the same data directory the fixture daemon got, so a
    // CLI that autostarts one lands in `root` rather than the developer's home.
    cmd.env("XDG_DATA_HOME", daemon.root.join("d"));
    cmd.env("TETON_CONFIG", &config_path);
    cmd.env("TETON_REPO_ROOT", &daemon.root);
    // `read_secret` takes this variable ahead of the prompt, so an exported
    // value in a developer's shell would skip the credential step entirely —
    // and then store that value in the **real OS keychain**, which is the one
    // thing this test exists not to do. Removed rather than merely unset.
    cmd.env_remove("TETON_PROVIDER_KEY");
    let mut session = pty.slave.spawn_command(cmd).expect("spawn teton under pty");
    drop(pty.slave);
    let transcript = spawn_reader(pty.master.try_clone_reader().expect("pty reader"));
    let mut writer = pty.master.take_writer().expect("pty writer");

    assert!(
        wait_for(&transcript, "ready (freeform)"),
        "the session never reached the entry prompt; transcript:\n{}",
        snapshot(&transcript)
    );

    // Read the baseline here, not at spawn: a starting daemon normalises its own
    // config once (the REQ-557 model migration), so bytes read earlier would
    // report a write this session never made.
    let before = std::fs::read(&config_path).expect("the fixture config exists");

    let step = |writer: &mut Box<dyn Write + Send>, text: &str, until: &str| {
        writer
            .write_all(text.as_bytes())
            .expect("type into the pty");
        writer.flush().ok();
        assert!(
            wait_for(&transcript, until),
            "the walk never reached {until:?}; transcript:\n{}",
            snapshot(&transcript)
        );
    };

    // (1) A remote registration reaches the session's confirmation — through
    // the write gate, through clap's parse of all four flags, through the
    // duplicate probe and the endpoint settlement. The question is default-no
    // and names what a `y` consents to (REQ-582 verify M1).
    step(
        &mut writer,
        "/provider add kimi2 --kind openai-compatible \
         --endpoint http://127.0.0.1:1/v1/chat/completions --model kimi-k3\r",
        "register `kimi2` (openai-compatible, kimi-k3) at http://127.0.0.1:1/v1/chat/completions? \
         the key is read next, echo-off, into the keychain [y/N]",
    );
    // A `y`, and only then the credential step. The prompt's own wording is the
    // assertion: it names the provider and promises the value will not be shown.
    step(
        &mut writer,
        "y\r",
        "API key for `kimi2` (not shown; stored only in the keychain):",
    );

    // (2) An empty answer. `prompt_for_secret` treats it as "nothing was typed"
    // and the row renders `ProviderAddRefusal::NoKey` — one line, and the
    // session carries on (ADR-3).
    step(&mut writer, "\r", "no API key provided");

    // (3) The fail-closed echo proof. Reaching (2) means `ask_secret` performed
    // its read; under a pty `EchoState::NoTerminal` is unreachable, and
    // `EchoState::Failed` would have printed this sentence *instead of* reading.
    // So its absence, together with the refusal above, is the statement that
    // echo was off while the terminal was waiting for a credential.
    let after_key = snapshot(&transcript);
    assert!(
        !after_key.contains("this terminal would not turn echo off"),
        "the key prompt could not clear ECHO, so the read was refused rather \
         than hidden and this test proves nothing about echo; \
         transcript:\n{after_key}"
    );

    // (4) Nothing was registered, and the file says so.
    assert_eq!(
        std::fs::read(&config_path).ok().as_deref(),
        Some(before.as_slice()),
        "a `/provider add` that never got a key must leave config.toml \
         byte-identical; transcript:\n{after_key}"
    );

    // (5) The non-vacuity half: a registration that needs **no** credential goes
    // all the way from this session's prompt to the daemon's config. Without it
    // (4) would also pass on a row that cannot write at all.
    step(
        &mut writer,
        "/provider add localy --kind local\r",
        "provider `localy` registered",
    );

    let final_transcript = snapshot(&transcript);
    let _ = session.kill();
    let _ = session.wait();

    let after = std::fs::read_to_string(&config_path).expect("the config still exists");
    assert!(
        after.contains("localy"),
        "the local registration never reached config.toml:\n{after}"
    );
    assert!(
        !after.contains("kimi2"),
        "the refused registration reached config.toml after all:\n{after}"
    );
    // A local provider has no credential, so it must never have been asked for
    // one — the prompt appears exactly once in this whole session, at step (1) —
    // and, having no key read to guard, no confirmation either: the question
    // was drawn exactly once, for the remote registration.
    assert_eq!(
        final_transcript.matches("API key for `").count(),
        1,
        "the key prompt was drawn for a kind that has no key, or drawn twice; \
         transcript:\n{final_transcript}"
    );
    assert_eq!(
        final_transcript.matches("the key is read next").count(),
        1,
        "the confirmation was drawn for a kind that reads no key, or drawn twice; \
         transcript:\n{final_transcript}"
    );
    // (The former `NEVER_TYPED_KEY` sweep — asserting a value this test never
    // types is absent from a transcript — was tautological and is gone (verify
    // m13); the rule it stood for is the section comment above, and the walk's
    // own steps are what enforce it: nothing here writes anything at the key
    // prompt but a bare return.)
}

// ---------------------------------------------------------------------------
// REQ-583 — the not-a-project notice, at a real terminal
// ---------------------------------------------------------------------------

/// The head of the notice (`banner::root_notice`); its bytes are TTY-only.
const NOT_A_PROJECT: &str = "Not inside a project";

/// **REQ-583 AC-8 / AC-11 / BR-5 / BR-8, the terminal half.** The piped suite
/// pins the notice's *absence* on a pipe (`cli_e2e`); this is where it is
/// allowed to appear, and must.
///
/// Two sessions against one daemon, each at a pty with a `HOME` the test made:
///
/// 1. `teton --cwd <plain>` draws the banner with the session root on its
///    `cwd:` line, then the notice, then the ready line — in that order (BR-5:
///    under the banner, once, before the session proceeds).
/// 2. `teton --cwd <project>` draws no notice at launch; typing `/cd ~` draws
///    the clear line, `session root is now ~ (your home folder)`, and the
///    notice again — the same one line launch would have printed (BR-8).
#[test]
fn a_move_to_a_non_project_root_re_fires_the_notice_at_a_terminal() {
    let daemon_path = daemon_bin();
    // The daemon's root is minted inside `spawn`, so the home lives beside it,
    // named the same way; the project and plain fixtures go under the root
    // once it exists.
    let home = PathBuf::from("/tmp").join(format!("tcptyhome{:x}", std::process::id() & 0xffff));
    let _ = std::fs::remove_dir_all(&home);
    std::fs::create_dir_all(&home).unwrap();
    let daemon = TestDaemon::spawn_with_env(
        &daemon_path,
        "",
        &["scripted reply"],
        &[("HOME", home.as_os_str())],
    );
    let project = daemon.root.join("proj");
    let plain = daemon.root.join("plain");
    std::fs::create_dir_all(&project).unwrap();
    std::fs::create_dir_all(&plain).unwrap();
    std::fs::write(project.join("Cargo.toml"), "[package]\nname = \"proj\"\n").unwrap();

    let open = |cwd: &Path| {
        let pty = native_pty_system()
            .openpty(PtySize {
                rows: 40,
                cols: 100,
                pixel_width: 0,
                pixel_height: 0,
            })
            .expect("openpty");
        let mut cmd = CommandBuilder::new(teton_bin());
        cmd.args(["--cwd", cwd.to_str().unwrap()]);
        cmd.env("XDG_RUNTIME_DIR", &daemon.runtime_dir);
        // REQ-611 TASK-364: the same data directory the fixture daemon got, so a
        // CLI that autostarts one lands in `root` rather than the developer's home.
        cmd.env("XDG_DATA_HOME", daemon.root.join("d"));
        cmd.env("TETON_CONFIG", daemon.root.join("config.toml"));
        cmd.env("TETON_REPO_ROOT", &daemon.root);
        cmd.env("HOME", &home);
        let session = pty.slave.spawn_command(cmd).expect("spawn teton under pty");
        drop(pty.slave);
        let transcript = spawn_reader(pty.master.try_clone_reader().expect("pty reader"));
        let writer = pty.master.take_writer().expect("pty writer");
        (session, transcript, writer)
    };

    // 1. A plain root: banner, notice, ready — in that order.
    let (mut session, transcript, _writer) = open(&plain);
    let ready = wait_for(&transcript, "ready (freeform)");
    let launch = snapshot(&transcript);
    let _ = session.kill();
    let _ = session.wait();
    assert!(
        ready,
        "the plain-root session never reached the entry prompt; transcript:\n{launch}"
    );
    let cwd_line = format!("cwd: {}", plain.display());
    let at = |needle: &str| {
        launch.find(needle).unwrap_or_else(|| {
            panic!("`{needle}` never reached the terminal; transcript:\n{launch}")
        })
    };
    let banner_at = at(&cwd_line);
    let notice_at = at(NOT_A_PROJECT);
    let ready_at = at("ready (freeform)");
    assert!(
        banner_at < notice_at && notice_at < ready_at,
        "the notice belongs under the banner and before the ready line; transcript:\n{launch}"
    );
    assert!(
        launch.contains(&format!(
            "the session root is {} (not a project); tools are scoped to it",
            plain.display()
        )),
        "the notice names the session root and its kind; transcript:\n{launch}"
    );
    assert_eq!(
        launch.matches(NOT_A_PROJECT).count(),
        1,
        "once, at launch; transcript:\n{launch}"
    );

    // 2. A project root: no notice at launch; `/cd ~` re-fires it.
    let (mut session, transcript, mut writer) = open(&project);
    assert!(
        wait_for(&transcript, "ready (freeform)"),
        "the project session never reached the entry prompt; transcript:\n{}",
        snapshot(&transcript)
    );
    let before = snapshot(&transcript);
    assert!(
        !before.contains(NOT_A_PROJECT),
        "a project root earns no notice at launch; transcript:\n{before}"
    );
    assert!(
        before.contains(&format!("cwd: {}", project.display())),
        "the banner's cwd line is the --cwd root; transcript:\n{before}"
    );

    writer.write_all(b"/cd ~\r").expect("type /cd ~");
    writer.flush().ok();
    let moved = wait_for(&transcript, "session root is now ~ (your home folder)");
    let refired = wait_for(&transcript, NOT_A_PROJECT);
    let after = snapshot(&transcript);
    let _ = session.kill();
    let _ = session.wait();
    let _ = std::fs::remove_dir_all(&home);

    assert!(
        moved,
        "`/cd ~` never reported the new root; transcript:\n{after}"
    );
    assert!(
        refired,
        "the notice did not re-fire after `/cd ~` at a terminal (BR-8); transcript:\n{after}"
    );
    let cleared_at = after
        .find("context cleared;")
        .unwrap_or_else(|| panic!("a move clears, and says so; transcript:\n{after}"));
    let moved_at = after
        .find("session root is now ~ (your home folder)")
        .unwrap();
    let notice_at = after.find(NOT_A_PROJECT).unwrap();
    assert!(
        cleared_at < moved_at && moved_at < notice_at,
        "clear line, root line, notice — in that order; transcript:\n{after}"
    );
    assert!(
        after.contains("the session root is ~ (your home folder); tools are scoped to it"),
        "the re-fired notice names the home root; transcript:\n{after}"
    );
}

// ---------------------------------------------------------------------------
// REQ-585 AC-8 — the consent prompt's bytes, at a terminal
// ---------------------------------------------------------------------------
//
// This is the other half of the LESSON-481 trade this file exists for. On a
// pipe a skill's dynamic-context consent is **refused without being asked**
// (BR-11) — `cli_e2e`'s
// `on_a_pipe_at_guarded_a_skill_consent_is_refused_without_eating_the_next_line`
// pins that, and it means the piped suite can never see the question itself.
// The question is the thing BR-6 makes promises about: one prompt per
// invocation, every command shown verbatim, asked under the skill's own key.
// So it is asked here, where there is a terminal to ask at.
//
// Only the prompt's bytes. What each answer *does* — the placeholders, the
// remembered grant, the neutralized output — is the daemon's, and lives in
// `tetond/tests/skill_consent_matrix.rs` and `skill_turn.rs`.

/// A skill whose body carries three dynamic-context commands, in an order the
/// prompt must preserve.
const THREE_COMMAND_SKILL: &str = "---\ndescription: three commands\n---\n\
                                   !`echo one`\n!`echo two`\n!`echo three`\n";

/// **AC-8's prompt, at a real terminal: one question for the whole invocation,
/// every command listed verbatim in document order, under the skill's own
/// permission key.**
///
/// Three claims, and each fails a different plausible implementation:
///
/// * **one** `permission requested` for three commands — a gate that asked per
///   command would train a user to hold `y` down, which is the failure BR-6's
///   "once per invocation" exists to prevent;
/// * the three commands appear **verbatim, one per line, in document order** —
///   `Surface::line` destroys newlines, so a joined list would arrive as one
///   run-on line and the ordering assertion below would be about nothing;
/// * the key is `skill:user:three`, **not** `shell` — a skill consent asked
///   under the shell tool's name is a grant remembered against the wrong
///   question, and a remembered `shell` allow would silently answer it.
#[test]
fn a_skill_consent_asks_once_at_a_terminal_and_lists_every_command_verbatim() {
    let daemon_path = daemon_bin();
    let home = PathBuf::from("/tmp").join(format!("tcptyskill{:x}", std::process::id() & 0xffff));
    let _ = std::fs::remove_dir_all(&home);
    std::fs::create_dir_all(home.join(".claude/skills/three")).unwrap();
    std::fs::write(
        home.join(".claude/skills/three/SKILL.md"),
        THREE_COMMAND_SKILL,
    )
    .unwrap();

    // Every tier bound to the scripted local tier, for the REQ-558 reason every
    // typed-turn test here binds them: otherwise the turn resolves to an
    // unreachable remote provider and fails before it can produce a reply — and
    // this test's last assertion is that the invocation still finished.
    let tiers: String = ["reflex", "scan", "build", "think"]
        .iter()
        .map(|t| format!("[[tiers]]\ntier = \"{t}\"\nprovider_id = \"local\"\n\n"))
        .collect();
    let config = format!("[[providers]]\nid = \"local\"\nkind = \"local\"\n\n{tiers}");
    let daemon = TestDaemon::spawn_with_env(
        &daemon_path,
        &config,
        &["scripted reply"],
        &[("HOME", home.as_os_str())],
    );
    let project = daemon.root.join("proj");
    std::fs::create_dir_all(&project).unwrap();
    std::fs::write(project.join("Cargo.toml"), "[package]\nname = \"proj\"\n").unwrap();

    // Wide enough that the terminal cannot hard-wrap a command line under test:
    // a wrapped line is still correct output, but it would split the marker and
    // fail an assertion about wording rather than about behaviour.
    let pty = native_pty_system()
        .openpty(PtySize {
            rows: 40,
            cols: 200,
            pixel_width: 0,
            pixel_height: 0,
        })
        .expect("openpty");
    let mut cmd = CommandBuilder::new(teton_bin());
    cmd.args(["--cwd", project.to_str().unwrap()]);
    cmd.env("XDG_RUNTIME_DIR", &daemon.runtime_dir);
    // REQ-611 TASK-364: the same data directory the fixture daemon got, so a
    // CLI that autostarts one lands in `root` rather than the developer's home.
    cmd.env("XDG_DATA_HOME", daemon.root.join("d"));
    cmd.env("TETON_CONFIG", daemon.root.join("config.toml"));
    cmd.env("TETON_REPO_ROOT", &daemon.root);
    cmd.env("HOME", &home);
    let mut session = pty.slave.spawn_command(cmd).expect("spawn teton under pty");
    drop(pty.slave);
    let transcript = spawn_reader(pty.master.try_clone_reader().expect("pty reader"));
    let mut writer = pty.master.take_writer().expect("pty writer");

    assert!(
        wait_for(&transcript, "ready (freeform)"),
        "the session never reached the entry prompt; transcript:\n{}",
        snapshot(&transcript)
    );
    writer.write_all(b"/three\r").expect("type /three");
    writer.flush().ok();

    // The question itself is the marker: a prompt that never came would time out
    // here rather than be inferred from a partial transcript.
    let asked = wait_for(&transcript, "allow skill:user:three?");
    let asking = snapshot(&transcript);
    assert!(
        asked,
        "a skill with dynamic context must ask at a terminal; transcript:\n{asking}"
    );

    // Answer, so the session finishes rather than being killed mid-question.
    writer.write_all(b"n\r").expect("decline");
    writer.flush().ok();
    let done = wait_for(&transcript, "scripted reply");
    let after = snapshot(&transcript);
    let _ = session.kill();
    let _ = session.wait();
    let _ = std::fs::remove_dir_all(&home);

    // One question for the whole invocation.
    assert_eq!(
        after
            .matches("permission requested: skill:user:three")
            .count(),
        1,
        "one consent per invocation, never one per command; transcript:\n{after}"
    );
    // Under the skill's own key, and not the shell tool's.
    assert!(
        !after.contains("permission requested: shell"),
        "a skill's dynamic context must never ask under `shell`; transcript:\n{after}"
    );
    // The subject block: who is asking, and how many.
    assert!(
        asking.contains("skill `three` (user) wants to run 3 dynamic-context commands:"),
        "the prompt must name the skill, its source and the count; transcript:\n{asking}"
    );
    // Every command, verbatim, one per line, in document order.
    let at = |needle: &str| {
        asking.find(needle).unwrap_or_else(|| {
            panic!("`{needle}` was never shown at the prompt; transcript:\n{asking}")
        })
    };
    let one = at("    !`echo one`");
    let two = at("    !`echo two`");
    let three = at("    !`echo three`");
    assert!(
        one < two && two < three,
        "the commands must be listed in the order the body runs them; \
         transcript:\n{asking}"
    );
    assert!(
        done,
        "the turn must still complete after the answer; transcript:\n{after}"
    );
}

/// **BUG-191 / REQ-587 AC-6, AC-14: BR-4's acknowledgment prompt, at a real
/// terminal.**
///
/// AC-6's evidence clause reads "(daemon unit + **pty for the prompt bytes** +
/// `cli_e2e` for the pipe)" and AC-14 says the pty suite covers "only the
/// acknowledgment prompt bytes". It did not: TASK-222 named this file and never
/// touched it, so the prompt bytes were pinned at renderer-unit level in
/// `session_ui.rs` and the only e2e leg asserted the refusal **without** a
/// terminal — the opposite claim.
///
/// The acknowledgment is raised from `SkillTool::invoke`, which is the *model's*
/// path, so no typed line can drive it. The scripted local engine's text
/// tool-call form is the only way a whole-CLI test can make the model issue one.
///
/// 22 project skills against `MAX_LISTED_PROJECT_SKILLS` (20), one of them
/// shadowing a user skill of the same name — so this draws the bounded list,
/// the shadowing mark, and the `+2 more` tail in one prompt. Shadowing entries
/// sort first, which is why the marked one is listed at all.
#[test]
fn the_acknowledgment_prompt_names_the_root_its_skills_and_what_it_left_out() {
    let daemon_path = daemon_bin();
    let home = PathBuf::from("/tmp").join(format!("tcptyack{:x}", std::process::id() & 0xffff));
    let _ = std::fs::remove_dir_all(&home);
    // A *user* skill named `validate`, for the project one of the same name to
    // shadow. Without it the entry renders bare and the mark is untested.
    std::fs::create_dir_all(home.join(".claude/skills/validate")).unwrap();
    std::fs::write(
        home.join(".claude/skills/validate/SKILL.md"),
        "---\ndescription: the user's validate\n---\nUser body.\n",
    )
    .unwrap();

    let tiers: String = ["reflex", "scan", "build", "think"]
        .iter()
        .map(|t| format!("[[tiers]]\ntier = \"{t}\"\nprovider_id = \"local\"\n\n"))
        .collect();
    let config = format!("[[providers]]\nid = \"local\"\nkind = \"local\"\n\n{tiers}");
    let daemon = TestDaemon::spawn_with_env(
        &daemon_path,
        &config,
        &[
            r#"{"tool": "skill", "arguments": {"name": "validate", "args": ""}}"#,
            "the project skill landed.",
        ],
        &[("HOME", home.as_os_str())],
    );

    let project = daemon.root.join("proj");
    std::fs::create_dir_all(&project).unwrap();
    std::fs::write(project.join("Cargo.toml"), "[package]\nname = \"proj\"\n").unwrap();
    // REQ-613 BR-1: an **empty** `TETON.md`, which is the documented way to say
    // "no notes here" — the loader counts it present, so no block is resident
    // and nothing about this session changes, and the generation offer is not
    // raised. Without it the first prompt of this fixture draws *two* permission
    // questions at one terminal and the expect script answers the wrong one.
    // This test is about the acknowledgment, so it says "not here" rather than
    // arranging to answer an offer it is not testing.
    std::fs::write(project.join("TETON.md"), "").unwrap();
    // The shadowing one, plus 21 others: 22 against a bound of 20 leaves 2.
    for name in std::iter::once("validate".to_owned()).chain((1..=21).map(|n| format!("s{n:02}"))) {
        let dir = project.join(".claude/skills").join(&name);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(
            dir.join("SKILL.md"),
            format!("---\ndescription: project {name}\n---\nProject body for {name}.\n"),
        )
        .unwrap();
    }

    // Tall enough to hold a 20-entry prompt without scrolling it off, wide
    // enough that no line under test can hard-wrap.
    let pty = native_pty_system()
        .openpty(PtySize {
            rows: 60,
            cols: 200,
            pixel_width: 0,
            pixel_height: 0,
        })
        .expect("openpty");
    let mut cmd = CommandBuilder::new(teton_bin());
    cmd.args(["--cwd", project.to_str().unwrap()]);
    cmd.env("XDG_RUNTIME_DIR", &daemon.runtime_dir);
    // REQ-611 TASK-364: the same data directory the fixture daemon got, so a
    // CLI that autostarts one lands in `root` rather than the developer's home.
    cmd.env("XDG_DATA_HOME", daemon.root.join("d"));
    cmd.env("TETON_CONFIG", daemon.root.join("config.toml"));
    cmd.env("TETON_REPO_ROOT", &daemon.root);
    cmd.env("HOME", &home);
    let mut session = pty.slave.spawn_command(cmd).expect("spawn teton under pty");
    drop(pty.slave);
    let transcript = spawn_reader(pty.master.try_clone_reader().expect("pty reader"));
    let mut writer = pty.master.take_writer().expect("pty writer");

    assert!(
        wait_for(&transcript, "ready (freeform)"),
        "the session never reached the entry prompt; transcript:\n{}",
        snapshot(&transcript)
    );
    // A typed prompt the model answers with a `skill` call — the acknowledgment
    // is raised by the call, never by anything a person can type.
    writer.write_all(b"go\r").expect("type a prompt");
    writer.flush().ok();

    let asked = wait_for(
        &transcript,
        "the model wants to run this repository's skills as instructions",
    );
    let asking = snapshot(&transcript);
    assert!(
        asked,
        "BR-4's acknowledgment must be drawn at a terminal; transcript:\n{asking}"
    );

    // Answer, so the session finishes rather than being killed mid-question.
    writer.write_all(b"n\r").expect("decline");
    writer.flush().ok();
    let done = wait_for(&transcript, "the project skill landed.");
    let after = snapshot(&transcript);
    let _ = session.kill();
    let _ = session.wait();
    let _ = std::fs::remove_dir_all(&home);

    // The shadowing entry, with its mark — the one entry whose source is worth
    // saying, because it is taking a name from the user's own skill.
    assert!(
        asking.contains("    validate (project — shadows your user skill)"),
        "the shadowing entry must carry its mark; transcript:\n{asking}"
    );
    // An ordinary entry is its bare name: every entry here is a project skill,
    // so `(project)` on each would be the same word twenty times over.
    assert!(
        asking.contains("\n    s01\n") || asking.contains("    s01\r"),
        "an ordinary entry is listed by bare name, one per line; \
         transcript:\n{asking}"
    );
    // The tail is the daemon's *count* of what it left out, never a re-count of
    // a list this side bounded — 22 skills against a bound of 20.
    assert!(
        asking.contains("    +2 more"),
        "the prompt must say how many it left out; transcript:\n{asking}"
    );
    assert!(
        done,
        "the turn must still complete after the answer; transcript:\n{after}"
    );
}

// ---------------------------------------------------------------------------
// REQ-591 D-1 — the durable acknowledgment, on both presence configurations
// ---------------------------------------------------------------------------

/// **D-1 — `p` writes a machine-wide row, so on a build that can ask a human it
/// asks one; and the answer's session half is untouched either way.**
///
/// The two seams and the two gate doors are pinned in-process against doubles.
/// What only a spawned daemon can say is that the check is **wired** in a real
/// process, over the verifier that build actually loaded, with a real
/// `config.toml` on disk to inspect afterwards — LESSON-519's "inspect the
/// artifact" and the reason `config_set_attestation.rs` spawns one for
/// `config/set`.
///
/// The two legs are the same fixture, the same repository, the same typed
/// invocation and the same `p`; only `TETON_PRESENCE_ACCEPT` changes, which
/// selects `AcceptingVerifier` (`1`) or `AlwaysFailsVerifier` (`fail`) through
/// `default_verifier`. Both ride the `TETON_TEST_SEAMS` master switch a release
/// build refuses to start under, so neither exists in the shipped binary.
///
/// **The pairing is the test** (LESSON-520). A daemon that never wrote the row
/// fails the accepting leg; one that stopped consulting the seam fails the
/// refusing leg. Neither can be green because the fixture was mis-built — and
/// the file is re-parsed by `Config::load`, not merely grepped, because a
/// document that reads right and does not load right is not a durable answer
/// (BR-9).
///
/// The **third** assertion is D-1's shape: on both legs the skill still runs.
/// The human at this terminal answered a question about this session and their
/// answer to it is not in doubt; only the part that outlives the session is a
/// commitment about the machine.
#[test]
fn a_permanent_acknowledgment_writes_its_row_only_where_presence_is_satisfied() {
    for (presence, expect_row) in [("1", true), ("fail", false)] {
        let daemon_path = daemon_bin();
        let home = PathBuf::from("/tmp").join(format!(
            "tcptyd1{presence}{:x}",
            std::process::id() & 0xffff
        ));
        let _ = std::fs::remove_dir_all(&home);
        std::fs::create_dir_all(home.join(".claude/skills")).unwrap();

        let tiers: String = ["reflex", "scan", "build", "think"]
            .iter()
            .map(|t| format!("[[tiers]]\ntier = \"{t}\"\nprovider_id = \"local\"\n\n"))
            .collect();
        let config = format!("[[providers]]\nid = \"local\"\nkind = \"local\"\n\n{tiers}");
        let daemon = TestDaemon::spawn_with_env(
            &daemon_path,
            &config,
            &["the project skill landed."],
            &[
                ("HOME", home.as_os_str()),
                // The REQ-575 seam, the same one `config_set_attestation.rs`
                // drives `config/set` under. `TETON_TEST_SEAMS` is already on
                // this fixture's daemon.
                ("TETON_PRESENCE_ACCEPT", std::ffi::OsStr::new(presence)),
            ],
        );

        let project = daemon.root.join("proj");
        std::fs::create_dir_all(project.join(".claude/skills/deploy")).unwrap();
        std::fs::write(project.join("Cargo.toml"), "[package]\nname = \"proj\"\n").unwrap();
        std::fs::write(
            project.join(".claude/skills/deploy/SKILL.md"),
            "---\ndescription: the project deploy\n---\nDeploy body.\n",
        )
        .unwrap();

        let pty = native_pty_system()
            .openpty(PtySize {
                rows: 60,
                cols: 300,
                pixel_width: 0,
                pixel_height: 0,
            })
            .expect("openpty");
        let mut cmd = CommandBuilder::new(teton_bin());
        cmd.args(["--cwd", project.to_str().unwrap()]);
        cmd.env("XDG_RUNTIME_DIR", &daemon.runtime_dir);
        // REQ-611 TASK-364: the same data directory the fixture daemon got, so a
        // CLI that autostarts one lands in `root` rather than the developer's home.
        cmd.env("XDG_DATA_HOME", daemon.root.join("d"));
        cmd.env("TETON_CONFIG", daemon.root.join("config.toml"));
        cmd.env("TETON_REPO_ROOT", &daemon.root);
        cmd.env("HOME", &home);
        let mut session = pty.slave.spawn_command(cmd).expect("spawn teton under pty");
        drop(pty.slave);
        let transcript = spawn_reader(pty.master.try_clone_reader().expect("pty reader"));
        let mut writer = pty.master.take_writer().expect("pty writer");

        assert!(
            wait_for(&transcript, "ready (freeform)"),
            "{presence}: the session never reached the entry prompt; transcript:\n{}",
            snapshot(&transcript)
        );
        writer.write_all(b"/deploy\r").expect("type the invocation");
        writer.flush().ok();

        assert!(
            wait_for(&transcript, "permission requested: project_skill_trust:"),
            "{presence}: a typed project skill must be acknowledged; transcript:\n{}",
            snapshot(&transcript)
        );
        let asking = snapshot(&transcript);
        assert!(
            asking.contains("[p]ermanently"),
            "{presence}: the durable option must be on the prompt, or `p` below \
             answers something else; transcript:\n{asking}"
        );
        writer.write_all(b"p\r").expect("answer permanently");
        writer.flush().ok();

        let ran = wait_for(&transcript, "the project skill landed.");
        let after = snapshot(&transcript);
        let _ = session.kill();
        let _ = session.wait();

        // The session half, on both legs: a human said yes to this session and
        // no presence check governs that.
        assert!(
            ran,
            "{presence}: the acknowledged skill must still run — the presence \
             check is on the durable half only; transcript:\n{after}"
        );

        // The durable half. Read off the file, then through the production
        // loader (BR-9, LESSON-519).
        let document = std::fs::read_to_string(daemon.root.join("config.toml"))
            .expect("the daemon's config file");
        let listed = teton_core::config::Config::load(&document)
            .expect("the document still parses whichever way the gate answered")
            .skills
            .trusted_project_roots;
        assert_eq!(
            !listed.is_empty(),
            expect_row,
            "{presence}: `[skills] trusted_project_roots` is a machine-wide \
             commitment, and this is the list it did or did not join: {listed:?}\n\
             document:\n{document}\ndaemon log:\n{}",
            std::fs::read_to_string(daemon.root.join("tetond.log")).unwrap_or_default()
        );

        drop(daemon);
        let _ = std::fs::remove_dir_all(&home);
    }
}

// ---------------------------------------------------------------------------
// REQ-589 AC-14 / BUG-191 — the over-budget offer's bytes, at a terminal
// ---------------------------------------------------------------------------
//
// BR-3 replaces a refusal with a question, and everything that makes the
// question worth asking is *wording*: which figures it quotes, what it says the
// window will do with a send this size, and which concrete write each answer
// performs. None of that is a structure — ADR-16 puts the composed sentence on
// `PermissionSubject::SkillOverBudget` as a field precisely because a client
// that re-worded from the structure would be the second composer BR-5 forbids.
//
// So a renderer-unit test asserting `render_consent_subject`'s output is
// asserting what a structure *says it would* print. That is the gap BUG-191 was
// filed for on REQ-587's acknowledgment prompt, where the prompt bytes were
// pinned in `session_ui.rs` and the only e2e leg asserted the refusal **without**
// a terminal — the opposite claim. This is the leg that reads the terminal.
//
// The fixture is the shape of the reported failure that opened this REQ: a
// typed `/analyze` from a repository's own `.claude/skills`, measured at Stage
// A against the local route's pair, on the local engine's own window —
// `bound: local engine`, verdict `ExceedsWindow` (BUG-222: the engine's window
// is a fact, and this body is past it), remedy `BindTierRemote`. It is the
// cell of the reachability table the report actually landed in.
//
// The *figures* are no longer the report's own: REQ-590 gave the local tier a
// derived pair (`BUDGET_PAIR`, 21,162 words / 63 KB on the 32,768-token
// window) and the reported
// measurement no longer exceeds either half. That the report's own numbers
// now serve is REQ-590 AC-12, witnessed in `skill_over_budget_offer.rs`; what
// this file keeps is the offer's *rendering*, exercised at whatever boundary
// the route actually has.
//
// What is *not* asserted here: the config file the remedy writes. Whether the
// two remedy-bearing answers reach a durable write is asserted, because without
// it options 1 and 2 (and 4 and 3) are indistinguishable at this surface and
// two of the four ids would be pinned vacuously. What the write *contains* is
// the daemon's, and lives beside the code that makes it.

/// Discovery's per-file ceiling for one `SKILL.md`.
///
/// A literal here because this crate is the thin client and cannot read
/// `tetond::skills::SKILL_MAX_BYTES` — but it is not a number this crate is
/// guessing at either: the client prints it, in the `over 128 KiB (N B)`
/// diagnostic a skipped skill earns.
///
/// It matters to a *budget* fixture because the two ceilings pull in opposite
/// directions. A file past this one is **named and skipped**, never measured, so
/// a body grown to clear the budget can grow straight past discovery and every
/// test below then fails with "`/analyze` is a skill that was skipped" rather
/// than with anything about an offer.
const SKILL_FILE_CEILING_BYTES: usize = 128 * 1024;

/// The word half of [`BUDGET_PAIR`], parsed rather than restated.
fn budget_words() -> usize {
    BUDGET_PAIR
        .split_once(" words / ")
        .expect("BUDGET_PAIR is a `N words / N KB` pair")
        .0
        .replace(',', "")
        .parse()
        .expect("BUDGET_PAIR's word half is a grouped count")
}

/// A skill body large enough to blow the local route's budget on its own, in
/// **both** currencies, while staying inside [`SKILL_FILE_CEILING_BYTES`].
///
/// A quarter past [`budget_words`] at ~4 bytes a word, which lands ~67% past
/// the byte half as well and ~19% short of the file ceiling. Deliberately clear
/// of the budget boundary rather than one word over it as the report was: Stage
/// A measures the body **with the system prompt**, whose size is not this test's
/// to fix, so a fixture tuned to land one word over would be measuring the
/// harness's prompt and not the skill. All three bounds are asserted below,
/// because the room between them is narrower than it looks — 21,162 words at
/// more than 6.2 B/word does not fit in 128 KiB at all.
///
/// **It was 500 lines of prose until REQ-590**, sized against the 4,096-word
/// budget the local tier ran under then. That budget went to 10,240 and the old
/// body no longer reached it — it stayed over the *byte* half and so still drew
/// an offer, which is precisely the way a resized fixture goes on passing while
/// testing something other than what it says. Prose at ~5.4 B/word cannot be
/// grown to clear the word budget without passing the file ceiling, which is
/// why the filler is short words now. (The ceiling itself rose 64 → 128 KiB
/// with the 32,768-token window, in step with the byte half.)
fn over_budget_skill_body() -> String {
    let words = budget_words() + budget_words() / 4;
    let mut body = String::from(
        "---\ndescription: audit this repository end to end\n---\naudit every file, step by step:\n",
    );
    // 16 whitespace words and ~66 bytes a line: two of instruction, fourteen of
    // filler.
    for step in 0..(words / 16) {
        body.push_str(&format!("step {step}: "));
        body.push_str(&"abc ".repeat(14));
        body.push('\n');
    }
    let counted = body.split_whitespace().count();
    assert!(
        counted > budget_words(),
        "fixture: {counted} words does not clear the {}-word budget",
        budget_words()
    );
    assert!(
        body.len() < SKILL_FILE_CEILING_BYTES,
        "fixture: a {} B body is past discovery's {SKILL_FILE_CEILING_BYTES} B \
         ceiling and would be skipped rather than measured",
        body.len()
    );
    body
}

/// The budget pair the local route derives, spelled as the terminal spells it.
///
/// Since REQ-590 the local tier's pair derives from the engine's own window
/// like any declared one: `LOCAL_ENGINE_N_CTX_DEFAULT` (32,768) less the generation
/// reservation (1,024) is 31,744 usable → 31,744 × 2/3 = **21,162 words**, and
/// 31,744 × 2 = **63,488 B**, which `bytes_figure` (`(bytes + 500) / 1000`)
/// rounds to `63 KB`.
///
/// On the 16,384-token window the byte half was the `LOCAL_BUDGET_BYTES`
/// constant instead (32,768 B, `33 KB`) — D-4 derived it (30,720, `31 KB`) and
/// ADR-9 reversed that because the derived figure was the smaller one there —
/// so the pair a terminal spelled was asymmetric. At 32,768 both halves are the
/// window's.
///
/// A literal, because this crate is the thin client and cannot read `tetond`'s
/// constants — the same reason [`REMEDY_WRITE`] carries a vendor window as a
/// literal. It is the **one** home of the pair in this file:
/// [`offer_sentence`], [`decline_refusal`] and [`measured_pair`]'s
/// non-vacuity threshold all read it rather than restating it.
const BUDGET_PAIR: &str = "21,162 words / 63 KB";

/// BR-3's `ExceedsWindow` clause on the local engine (BUG-222). The fixture
/// body is a quarter past the word budget — max(26,452 × 3/2, bytes / 2) is
/// past the engine's 32,768 — so the daemon says the send will blow the window
/// the engine allocated, and names the backstop ADR-3 built for exactly this
/// tier rather than promising the send will serve.
const EXCEEDS_ENGINE_WINDOW_CLAUSE: &str = "This will blow the context window the engine \
                                            allocated: proceeding will very likely be rejected \
                                            by the engine, and the turn ends with a \
                                            context-length error rather than quietly losing \
                                            anything.";

/// The remedy as ADR-1 binds it: the **concrete write**, never "raise the
/// limit".
///
/// `build` is the tier this fixture's classifier routes a `/analyze` turn to,
/// and it is in the string on purpose — BR-9's write is "bind *this* tier",
/// and a label that named no tier would be the vague promise ADR-1's precedent
/// (`enable_permanent`, which once promised a write that was silently a no-op)
/// exists to forbid.
///
/// **TASK-260.** `deepseek` and its window are here for the same reason the tier
/// is. This fixture registers exactly one remote provider, which is ADR-12's
/// *propose by name* count, so both halves of BR-9's pair are concrete: the
/// provider by id and its window by figure, with the date it was read. Until
/// TASK-260 this constant read "a remote provider … that provider's
/// `capabilities.max_context`", which is the vagueness ADR-18 item 2 recorded
/// and the same promise ADR-1's precedent forbids.
///
/// The window figure and its date are literals here because this crate is the
/// thin client and cannot read `tetond`'s recipe catalog. They are the one
/// place in this crate that copies a vendor window, and `recipe_window_one_home.rs`
/// — which sweeps the daemon's `src/`, not this crate's tests — cannot see them.
const REMEDY_WRITE: &str = "bind the `build` tier to `deepseek` and declare its \
                            `capabilities.max_context = 1000000` in the same change (DeepSeek's \
                            own published window, read 2026-08-19)";

/// BR-9's cost, which [`REMEDY_WRITE`] cannot be rendered without (AC-7a):
/// `RemedyClause::render` is the only way out of that type and it concatenates
/// the two unconditionally. Asserting them as one string here is what would
/// fail if a second rendering path ever shed the risk.
const REMEDY_RISK: &str = "either half alone leaves a provider with no declared window, which \
                           derives this same budget again, and rebinding moves every turn that \
                           tier serves to that provider, at that provider's prices";

/// The write and its cost, joined the one way they are ever joined.
fn remedy_clause() -> String {
    format!("{REMEDY_WRITE} — {REMEDY_RISK}")
}

/// The bound as the daemon speaks it for the local route (REQ-590 AC-16).
///
/// Every other bound names something a user can go and read; this one names an
/// engine, so since REQ-590 it accounts for its own number instead. The
/// arithmetic is checkable from the sentence alone: `32,768 − 1,024 = 31,744`,
/// `31,744 × 2/3` is [`BUDGET_PAIR`]'s word half and `31,744 × 2` its byte
/// half. (On the 16,384-token window the clause ended `the byte half is
/// fixed`, because it was — ADR-9.)
///
/// A literal, for [`BUDGET_PAIR`]'s reason: this crate is the thin client and
/// cannot read `tetond`'s constants. Its one home in this file, read by
/// [`offer_sentence`] and [`decline_refusal`] alike — AC-3's claim is that
/// those two share a head byte for byte, and two copies of this clause is the
/// one edit that would make that claim vacuous.
const LOCAL_BOUND: &str = "bound: local engine — both halves come from the engine's \
                           32,768-token window, less the 1,024 reserved for the reply";

/// The daemon's finished question, with the one figure this fixture cannot fix
/// left as a parameter.
///
/// Everything else is byte-exact, and that is the ADR-16 assertion: the client
/// renders this **verbatim**. If it re-worded so much as a comma this would not
/// be found in the transcript.
fn offer_sentence(measured: &str) -> String {
    format!(
        "`/analyze` (this repository's skill) does not fit this route's context budget: the body \
         alone, with the system prompt, comes to about {measured}, and the budget is \
         {BUDGET_PAIR} ({LOCAL_BOUND}). {EXCEEDS_ENGINE_WINDOW_CLAUSE} The durable fix is to {}. \
         Send it whole this once, take the durable fix, both, or neither?",
        remedy_clause()
    )
}

/// AC-3's refusal: today's `-32023`, in every byte, from the same measurement.
///
/// Two differences from [`offer_sentence`] and they are the whole of AC-3. The
/// head is **identical** — same stage clause, same measured pair, same budget
/// pair, same spoken bound — and the tail is `SkillCaller::consequence`'s, with
/// no verdict, no remedy and no ASSUME-018 source marker, because today's
/// refusal carries none of them. `` `/analyze` `` here, not
/// `` `/analyze` (this repository's skill) ``.
fn decline_refusal(measured: &str) -> String {
    format!(
        "`/analyze` does not fit this route's context budget: the body alone, with the system \
         prompt, comes to about {measured}, and the budget is {BUDGET_PAIR} ({LOCAL_BOUND}). \
         Nothing was sent and no provider saw this turn — a skill expansion is carried whole or \
         refused, never shortened into something you did not invoke."
    )
}

/// The four rows as the terminal draws them, numbered, in the daemon's order.
///
/// The order is the daemon's and this test asserts the numbers, because BR-3's
/// "leads with the remedy" **is** the order and nothing else (ADR-14): an
/// `ExceedsWindow` verdict leads with it — the daemon expects the send to fail,
/// so the durable fix is row 1 and the one-time override row 2 (BUG-222 moved
/// this fixture into that cell; it was `WindowUnknown`, override first). A client that sorted these rows would silently undo the rule, and the
/// numbers are what a person types.
///
/// Rows 2 and 3 carry [`remedy_clause`] whole — the same rendering the sentence
/// above them carries. Rows 1 and 4 say, in so many words, that they write
/// nothing: ADR-1 requires a label to name its write, and "writes nothing" is
/// the honest form of that for the two answers that make none.
fn option_rows() -> [String; 4] {
    [
        format!("  1) Send it whole this once, and {}", remedy_clause()),
        "  2) Send it whole this once, over budget — writes nothing, and nothing is remembered, \
         so the next invocation asks again"
            .to_owned(),
        format!("  3) Do not send it, but {}", remedy_clause()),
        "  4) Do not send it — refuse the turn exactly as this route does today, and write nothing"
            .to_owned(),
    ]
}

/// The last line of the prompt block, and therefore the marker every leg waits
/// on: everything this file asserts is drawn above it.
const CHOOSE_LINE: &str = "  choose 1-4 (empty refuses the turn): ";

/// The clause `format_over_budget_accepted` opens with. BR-1's record is drawn
/// **unconditionally** — a declined offer prints a refusal, so the accepted one
/// has to print its counterpart or one question has one visible outcome and one
/// silent one.
const ACCEPTED_NOTICE_HEAD: &str = "over budget: skill `analyze` (project) was sent whole at your \
                                    request — ";

/// The clause `format_over_budget_remedy_applied` opens with — a file on disk
/// changed, and this is the line that says so.
const REMEDY_APPLIED_HEAD: &str = "over budget: wrote the going-forward fix (";

/// What the scripted local engine answers a turn that was actually sent. Its
/// presence is the proof an answer proceeded; its absence, with a refusal
/// beside it, is the proof one did not.
const SENT_MARKER: &str = "the oversized expansion was sent.";

/// A pty session parked at REQ-589's over-budget offer.
///
/// The pty master is held rather than dropped: the reader thread and the writer
/// were taken from it, and dropping it closes the terminal out from under a
/// session this fixture still means to type at.
struct OfferSession {
    _master: Box<dyn portable_pty::MasterPty + Send>,
    session: Box<dyn portable_pty::Child + Send + Sync>,
    writer: Box<dyn Write + Send>,
    transcript: Transcript,
    home: PathBuf,
    /// Killed and cleaned up by its own `Drop`, after this type's has run.
    _daemon: TestDaemon,
}

impl OfferSession {
    /// Type one line at the terminal.
    fn type_line(&mut self, line: &str) {
        self.writer
            .write_all(format!("{line}\r").as_bytes())
            .expect("type at the pty");
        self.writer.flush().ok();
    }

    fn wait_for(&self, marker: &str) -> bool {
        wait_for(&self.transcript, marker)
    }

    fn snapshot(&self) -> String {
        snapshot(&self.transcript)
    }
}

impl Drop for OfferSession {
    fn drop(&mut self) {
        let _ = self.session.kill();
        let _ = self.session.wait();
        let _ = std::fs::remove_dir_all(&self.home);
    }
}

/// Drive a fresh session to the over-budget offer and leave it parked at the
/// question, with the whole prompt block already on screen.
///
/// A daemon per leg, not a session per leg against one daemon: two of the four
/// answers **write the config file** this daemon was started with, and a leg
/// that inherited another leg's write would be answering a question about a
/// different route.
///
/// The acknowledgment BR-6 puts first is answered on the way past. It is a real
/// step of the reported path — a typed project skill is acknowledged before it
/// expands, so before the route and before either budget stage — and answering
/// it here keeps the leg below about the offer.
fn park_at_the_over_budget_offer(tag: &str) -> OfferSession {
    let daemon_path = daemon_bin();
    let home =
        PathBuf::from("/tmp").join(format!("tcptyob{:x}-{tag}", std::process::id() & 0xffff));
    let _ = std::fs::remove_dir_all(&home);
    std::fs::create_dir_all(&home).unwrap();

    // Every tier on the scripted local tier, for the reason every typed-turn
    // test in this file binds them — and here it is also the fixture itself:
    // `BudgetBound::LocalEngine` is what the reported failure was bound by.
    let tiers: String = ["reflex", "scan", "build", "think"]
        .iter()
        .map(|t| format!("[[tiers]]\ntier = \"{t}\"\nprovider_id = \"local\"\n\n"))
        .collect();
    let config = format!("[[providers]]\nid = \"local\"\nkind = \"local\"\n\n{tiers}");
    let daemon = TestDaemon::spawn_with_env(
        &daemon_path,
        &config,
        &[SENT_MARKER],
        &[("HOME", home.as_os_str())],
    );

    let project = daemon.root.join("proj");
    std::fs::create_dir_all(project.join(".claude/skills/analyze")).unwrap();
    std::fs::write(project.join("Cargo.toml"), "[package]\nname = \"proj\"\n").unwrap();
    std::fs::write(
        project.join(".claude/skills/analyze/SKILL.md"),
        over_budget_skill_body(),
    )
    .unwrap();

    // Wide enough that no line under test can hard-wrap. The client wraps
    // nothing itself, so a terminal narrower than the sentence would soft-wrap
    // it for display without putting bytes in the stream — but the width costs
    // nothing and removes the question.
    let pty = native_pty_system()
        .openpty(PtySize {
            rows: 60,
            cols: 400,
            pixel_width: 0,
            pixel_height: 0,
        })
        .expect("openpty");
    let mut cmd = CommandBuilder::new(teton_bin());
    cmd.args(["--cwd", project.to_str().unwrap()]);
    cmd.env("XDG_RUNTIME_DIR", &daemon.runtime_dir);
    // REQ-611 TASK-364: the same data directory the fixture daemon got, so a
    // CLI that autostarts one lands in `root` rather than the developer's home.
    cmd.env("XDG_DATA_HOME", daemon.root.join("d"));
    cmd.env("TETON_CONFIG", daemon.root.join("config.toml"));
    cmd.env("TETON_REPO_ROOT", &daemon.root);
    cmd.env("HOME", &home);
    let session = pty.slave.spawn_command(cmd).expect("spawn teton under pty");
    drop(pty.slave);
    let transcript = spawn_reader(pty.master.try_clone_reader().expect("pty reader"));
    let writer = pty.master.take_writer().expect("pty writer");
    let mut parked = OfferSession {
        _master: pty.master,
        session,
        writer,
        transcript,
        home,
        _daemon: daemon,
    };

    assert!(
        parked.wait_for("ready (freeform)"),
        "the session never reached the entry prompt; transcript:\n{}",
        parked.snapshot()
    );
    parked.type_line("/analyze");

    // BR-6's acknowledgment, which a typed project skill now raises before it
    // expands (ADR-10). Answered `y`, because this leg is about what comes
    // after it.
    assert!(
        parked.wait_for("permission requested: project_skill_trust:"),
        "a typed project skill must be acknowledged before it expands; transcript:\n{}",
        parked.snapshot()
    );
    parked.type_line("y");

    // The **last** line of the offer block, not the first: waiting on the first
    // would let a snapshot be taken between two writes and assert about a
    // prompt that was still arriving.
    assert!(
        parked.wait_for(CHOOSE_LINE),
        "BR-3's offer must be drawn at a terminal; transcript:\n{}",
        parked.snapshot()
    );
    parked
}

/// The measured pair **exactly as the terminal drew it**, read back out of the
/// sentence it was drawn in.
///
/// The one figure this fixture cannot fix: Stage A measures the skill body
/// *with the system prompt*, and pinning a literal would pin the harness's
/// prompt size. Extracting it is not a weaker assertion than a literal would
/// be — every other byte of the sentence is literal around it, the pair is
/// checked for shape and for being over the budget below, and each leg then
/// asserts that the *other* sentences of the same measurement quote this same
/// string back (AC-2: one measurement, one set of figures).
fn measured_pair(transcript: &str) -> String {
    const OPENS: &str = "comes to about ";
    const CLOSES: &str = ", and the budget is";
    let from = transcript
        .find(OPENS)
        .unwrap_or_else(|| panic!("no measured pair was drawn; transcript:\n{transcript}"))
        + OPENS.len();
    let rest = &transcript[from..];
    let to = rest
        .find(CLOSES)
        .unwrap_or_else(|| panic!("the measured pair never closed; transcript:\n{transcript}"));
    let pair = rest[..to].to_owned();

    // Shape, and that it is over: an extraction that silently captured the
    // empty string would make every assertion below pass vacuously.
    let (words, bytes) = pair
        .split_once(" words / ")
        .unwrap_or_else(|| panic!("`{pair}` is not a `N words / N KB` pair"));
    let counted: usize = words
        .replace(',', "")
        .parse()
        .unwrap_or_else(|_| panic!("`{words}` is not a grouped word count"));
    // The threshold is read out of `BUDGET_PAIR` rather than written again.
    // This assertion held a literal `4_096` through REQ-590, which raised the
    // local word budget (to 10,240 then; 21,162 on the 32,768-token window)
    // and left it comparing the fixture against a
    // budget no route runs under — passing, and saying nothing.
    assert!(
        counted > budget_words(),
        "the fixture must measure over the {}-word budget, not `{pair}`",
        budget_words()
    );
    assert!(
        bytes.ends_with(" KB") || bytes.ends_with(" MB") || bytes.ends_with(" B"),
        "`{bytes}` is not a byte figure"
    );
    pair
}

/// **AC-14 / BUG-191: BR-3's offer, at a real terminal, in the daemon's own
/// words.**
///
/// Four claims, and each fails a different plausible implementation:
///
/// * the daemon's composed sentence reaches the screen **verbatim** — the whole
///   of it, in one contiguous run. ADR-16 puts it on the subject as a field
///   precisely so the client re-words nothing; a client that re-worded, or that
///   re-composed the same facts from the structure beside it, would be BR-5's
///   forbidden second composer and would not match this string;
/// * the figures are drawn **once**. The client's own lead line carries no
///   numbers at all, because `stage`, both pairs, the bound and the provider are
///   already in the sentence — and a second spelling of one number is
///   LESSON-456's shape in its most innocuous form;
/// * every option label names its **concrete write** (ADR-1), and the two that
///   write cannot shed BR-7a's risk (AC-7a);
/// * a stray `y` — the reflex answer at a consent prompt, and the answer this
///   one *must not* read — re-asks instead of sending an oversized turn.
///
/// The `WindowVerdict::Unknown` hedge is asserted **absent**, and so is
/// `WindowUnknown` ("this route declares no window"): the local engine's window
/// is a fact (BUG-222), and ADR-13 exists because quietly relabelling a verdict
/// would tell a user their route declares no window on the strength of a parse
/// failure.
#[test]
fn the_over_budget_offer_is_drawn_at_a_terminal_in_the_daemons_own_words() {
    let mut parked = park_at_the_over_budget_offer("words");
    let asking = parked.snapshot();

    // One question for the whole invocation, under the skill's own key — never
    // under `skill`, the tool, whose posture is read-only at every level.
    assert_eq!(
        asking
            .matches("permission requested: skill:project:analyze")
            .count(),
        1,
        "one over-budget offer per invocation, under the skill's own key; \
         transcript:\n{asking}"
    );

    // The client's one line: the marking, in the vocabulary it already names a
    // source in (ASSUME-018) — and not one figure of its own.
    assert!(
        asking.contains("  skill `analyze` (project) is over this route's budget:"),
        "the client must mark the skill and its source; transcript:\n{asking}"
    );

    // The daemon's sentence, whole and verbatim (ADR-16).
    let measured = measured_pair(&asking);
    let sentence = offer_sentence(&measured);
    assert!(
        asking.contains(&sentence),
        "the daemon's question must be rendered verbatim.\nexpected:\n{sentence}\n\
         transcript:\n{asking}"
    );

    // Both pairs, once each. The client quotes neither on its own line, so a
    // second occurrence would mean a second speller of one measurement.
    assert_eq!(
        asking.matches(BUDGET_PAIR).count(),
        1,
        "the budget pair is spelled once, by the sentence; transcript:\n{asking}"
    );
    assert_eq!(
        asking.matches(&measured).count(),
        1,
        "the measured pair is spelled once, by the sentence; transcript:\n{asking}"
    );

    // The four rows, verbatim, numbered, in the daemon's order — and
    // `ExceedsWindow` leads with the remedy, so the durable fix is row 1.
    let rows = option_rows();
    let at = |needle: &str| {
        asking
            .find(needle)
            .unwrap_or_else(|| panic!("this row was never drawn:\n{needle}\ntranscript:\n{asking}"))
    };
    let (one, two, three, four) = (at(&rows[0]), at(&rows[1]), at(&rows[2]), at(&rows[3]));
    assert!(
        one < two && two < three && three < four,
        "the rows must be drawn in the daemon's order; transcript:\n{asking}"
    );
    assert!(
        asking.contains(CHOOSE_LINE),
        "the prompt must say what range it reads and what silence does; \
         transcript:\n{asking}"
    );

    // ADR-13: this verdict is readable, so the hedge has nothing to say here.
    assert!(
        !asking.contains("this build cannot read the window verdict this daemon sent"),
        "a readable verdict must not draw the unreadable-verdict hedge; \
         transcript:\n{asking}"
    );

    // The reflex answer, refused as an answer. `y` at a consent prompt means
    // yes; at this one it means nothing, and the retry line says why rather
    // than re-drawing the question.
    parked.type_line("y");
    assert!(
        parked.wait_for(
            "  please answer with one of the numbers above, 1-4 — this prompt reads no letters, \
             so a stray `y` is not an answer to it"
        ),
        "a stray `y` must re-ask, never send; transcript:\n{}",
        parked.snapshot()
    );
    let after_y = parked.snapshot();
    assert!(
        !after_y.contains(ACCEPTED_NOTICE_HEAD) && !after_y.contains(SENT_MARKER),
        "a stray `y` must not have sent an oversized turn; transcript:\n{after_y}"
    );

    // Answer for real, so the session finishes rather than being killed
    // mid-question — and so BR-1's record is on screen to compare against.
    parked.type_line("1");
    assert!(
        parked.wait_for(SENT_MARKER),
        "the turn must complete after the answer; transcript:\n{}",
        parked.snapshot()
    );
    let after = parked.snapshot();

    // AC-2 at the surface: the record of the send quotes the pair the question
    // quoted, character for character, and the budget it was measured against.
    assert!(
        after.contains(&format!(
            "{ACCEPTED_NOTICE_HEAD}{measured} against a budget of {BUDGET_PAIR}, past the \
             route's context window; measured from its body, before any dynamic-context command \
             ran. Nothing was shortened."
        )),
        "the accepted record must quote the offer's own figures; transcript:\n{after}"
    );
}

/// **AC-14's second half: each of the four ids, answered at a terminal, settles
/// the turn the way its own label said it would.**
///
/// The labels are promises about two independent things — whether the turn is
/// sent, and whether a file on disk changes — and ADR-1 spells the four
/// combinations as four ids because `PermissionOutcome::Selected` cannot carry
/// two booleans. So the matrix is asserted as two booleans: a row that sent
/// without writing and a row that sent *and* wrote must be told apart here, or
/// two of the four ids are pinned by a test that cannot see the difference
/// between them.
///
/// The fifth row is not an option id. `(empty refuses the turn)` is a promise
/// the prompt makes in the bytes above, and BR-4's rule is that silence is never
/// consent — so an empty line has to land on the refusal, not on a re-ask and
/// never on a send.
///
/// What each row asserts is the **outcome**, not the config file: the write's
/// contents belong to the code that makes it. `REMEDY_APPLIED_HEAD` is the
/// client's own line for "a file on disk changed", and its presence or absence
/// is the boolean this test is entitled to read.
#[test]
fn each_over_budget_answer_settles_the_turn_the_way_its_label_said() {
    // (typed answer, tag, sends, writes)
    let rows: &[(&str, &str, bool, bool)] = &[
        ("1", "both", true, true),
        ("2", "once", true, false),
        ("3", "fix", false, true),
        ("4", "no", false, false),
        // Not an id — the prompt's own parenthetical, kept.
        ("", "empty", false, false),
    ];

    for (answer, tag, sends, writes) in rows {
        let mut parked = park_at_the_over_budget_offer(tag);
        let measured = measured_pair(&parked.snapshot());
        parked.type_line(answer);

        // The settle point, waited on rather than slept past: the send's own
        // reply, or the refusal that stands in for it. The remedy line, when
        // there is one, is drawn *before* either — so by the time this returns,
        // its absence below is a fact and not a race.
        let settled = if *sends {
            parked.wait_for(SENT_MARKER)
        } else {
            parked.wait_for(&decline_refusal(&measured))
        };
        let after = parked.snapshot();
        assert!(
            settled,
            "answering `{answer}` must {} the turn; transcript:\n{after}",
            if *sends { "send" } else { "refuse" }
        );

        // BR-1's record is drawn on exactly the answers that sent, and AC-3's
        // refusal on exactly the ones that did not. One question, two outcomes,
        // neither of them silent.
        assert_eq!(
            after.contains(ACCEPTED_NOTICE_HEAD),
            *sends,
            "answering `{answer}` must {} BR-1's accepted record; transcript:\n{after}",
            if *sends { "draw" } else { "not draw" }
        );
        assert_eq!(
            after.contains(&decline_refusal(&measured)),
            !*sends,
            "answering `{answer}` must {} AC-3's refusal; transcript:\n{after}",
            if *sends { "not draw" } else { "draw" }
        );

        // The other boolean, and the one the option ids exist to carry: whether
        // a durable write happened. Without this, `1` and `2` are the same
        // transcript and so are `3` and `4`.
        assert_eq!(
            after.contains(REMEDY_APPLIED_HEAD),
            *writes,
            "answering `{answer}` must {} the going-forward fix; transcript:\n{after}",
            if *writes { "write" } else { "not write" }
        );
    }
}

// ---------------------------------------------------------------------------
// REQ-592 OQ-4 — a resized terminal, at a real terminal (TASK-281)
// ---------------------------------------------------------------------------

/// **OQ-4's wiring, and the only test that can see it.**
///
/// ADR-9 decided against a `SIGWINCH` handler on the grounds that the width is
/// re-read as the session goes, so a resize takes effect on the next block and
/// already-printed rows keep their breaks. `PlainSurface` holds the width as a
/// field, so that decision is only true if something in `main.rs` keeps telling
/// it — and nothing outside a real terminal can tell whether anything does.
/// `render.rs`'s unit tests call `set_width` themselves, which proves the verb
/// works and says nothing about whether it is called; `cli_e2e` has no terminal
/// to resize.
///
/// So: one session, two turns, and a window dragged narrower in between. The
/// first reply fits the wide window and is emitted as a single row. The second
/// is emitted as three, and the assertion is that its sentence can no longer be
/// found in one piece.
///
/// **A pty transcript is the right instrument for this** and a screen scrape
/// would be the wrong one. Hard-wrapping is a display artefact — the terminal
/// folds a too-long row across lines without putting a byte anywhere — so the
/// transcript holds exactly the bytes `teton` wrote. A CLI still laying out at
/// the old width therefore writes the sentence contiguously, and this test fails
/// on the presence of that contiguous run rather than on how it looked.
///
/// Mutation: delete the `ctx.surface.set_width(...)` from
/// `next_interactive_line`'s `stdin_ready` arm and the second reply arrives in
/// one piece.
#[test]
fn a_resized_window_lays_the_next_turn_out_at_the_new_width() {
    // Wide enough for either sentence to be one row, then narrow enough that the
    // second cannot be. Both are chosen so the greedy wrap has to break the
    // sentence rather than merely re-space it.
    const WIDE_COLS: u16 = 120;
    const NARROW_COLS: u16 = 32;
    const FIRST: &str = "the first answer is delivered while the terminal is still wide open here";
    const SECOND: &str =
        "the second answer arrives after the window has been dragged much narrower";

    // The precondition the whole test rests on: at the wide width neither
    // sentence wraps, so a CLI that never re-read the width would write the
    // second one contiguously — which is the failure this detects.
    assert!(
        FIRST.len() < WIDE_COLS as usize && SECOND.len() < WIDE_COLS as usize,
        "both fixtures must fit the wide window, or the test proves nothing"
    );
    assert!(
        SECOND.len() > NARROW_COLS as usize,
        "the second fixture must not fit the narrow window"
    );

    let daemon_path = daemon_bin();
    // Every tier bound to the scripted local tier, for the REQ-558 reason the
    // other typed-turn tests here bind them.
    let tiers: String = ["reflex", "scan", "build", "think"]
        .iter()
        .map(|t| format!("[[tiers]]\ntier = \"{t}\"\nprovider_id = \"local\"\n\n"))
        .collect();
    let config = format!("[[providers]]\nid = \"local\"\nkind = \"local\"\n\n{tiers}");
    let daemon = TestDaemon::spawn_with(&daemon_path, &config, &[FIRST, SECOND]);

    let pty = native_pty_system()
        .openpty(PtySize {
            rows: 40,
            cols: WIDE_COLS,
            pixel_width: 0,
            pixel_height: 0,
        })
        .expect("openpty");

    let mut cmd = CommandBuilder::new(teton_bin());
    cmd.env("XDG_RUNTIME_DIR", &daemon.runtime_dir);
    // REQ-611 TASK-364: the same data directory the fixture daemon got, so a
    // CLI that autostarts one lands in `root` rather than the developer's home.
    cmd.env("XDG_DATA_HOME", daemon.root.join("d"));
    cmd.env("TETON_CONFIG", daemon.root.join("config.toml"));
    cmd.env("TETON_REPO_ROOT", &daemon.root);
    let mut session = pty.slave.spawn_command(cmd).expect("spawn teton under pty");
    drop(pty.slave);
    let transcript = spawn_reader(pty.master.try_clone_reader().expect("pty reader"));
    let mut writer = pty.master.take_writer().expect("pty writer");

    assert!(
        wait_for(&transcript, "ready (freeform)"),
        "the session never reached the entry prompt; transcript:\n{}",
        snapshot(&transcript)
    );

    writer.write_all(b"first question\r").expect("type");
    writer.flush().ok();

    // Turn one, at the wide width: the sentence is on screen in one piece,
    // because 72 columns of prose fit a 120-column window without a break.
    assert!(
        wait_for(&transcript, FIRST),
        "the first turn never produced its reply; transcript:\n{}",
        snapshot(&transcript)
    );

    // **The drag.** `TIOCGWINSZ` on the slave reports the master's size, so this
    // is the same fact a user's window manager would deliver.
    pty.master
        .resize(PtySize {
            rows: 40,
            cols: NARROW_COLS,
            pixel_width: 0,
            pixel_height: 0,
        })
        .expect("resize the pty");

    // Typed only after the resize, which is what makes this the resize-then-type
    // case: the entry frame was drawn — and its width read — *before* the drag,
    // so a session that only measured the terminal when it drew the frame would
    // still be laying out at 120 here.
    writer.write_all(b"second question\r").expect("type");
    writer.flush().ok();

    // The head of the second sentence fits the narrow window and survives as one
    // run, which is how we know the turn ran at all.
    assert!(
        wait_for(&transcript, "the second answer arrives after"),
        "the second turn never produced its reply; transcript:\n{}",
        snapshot(&transcript)
    );

    let seen = snapshot(&transcript);
    let _ = session.kill();
    let _ = session.wait();

    assert!(
        !seen.contains(SECOND),
        "the reply after the resize was written as one unbroken row, so the \
         session is still laying out at {WIDE_COLS} columns in a {NARROW_COLS}-\
         column window (OQ-4). The terminal then hard-wraps it mid-word, which is \
         the defect REQ-592 exists to remove; transcript:\n{seen}"
    );
    // And the other half of OQ-4: what was already printed keeps its breaks. The
    // first reply is still in the transcript exactly as it was emitted — nothing
    // re-flowed it when the window changed.
    assert!(
        seen.contains(FIRST),
        "the pre-resize reply was re-flowed after the fact; transcript:\n{seen}"
    );
}

// ---------------------------------------------------------------------------
// REQ-592 — the rendered bytes at a real terminal (TASK-283)
// ---------------------------------------------------------------------------
//
// ## Why every leg below is here, and can be nowhere else
//
// `PlainSurface` gains its renderer only through `with_markdown`, and `main.rs`
// reaches for that constructor only when stdout is a terminal (ADR-1, BR-7). A
// piped run builds the surface it always built, so `cli_e2e` is not merely
// uninterested in this feature — it is **structurally blind** to it, the same
// way it is blind to the loading indicator and the hand-off nudge. `render.rs`'s
// own unit tests do see the renderer, but they see it over a `Vec<u8>` at a
// width they hand in themselves. That a real session hands it the *real*
// terminal's width, on the real stream, at the real turn boundary, is a fact
// only a process observed from outside can produce.
//
// ## Why the turns are scripted
//
// AC-12 is a claim about the renderer. TASK-282 added a clause to the system
// prompt that changes what a live model writes, so a fixture that *solicited*
// its own markdown would be asserting on the model's obedience and calling the
// result layout. What a live model writes under the new clause is a
// verification-notes observation, not something a test can pin ([[LESSON-481]]:
// pay for the harness the gate demands, and say where a gap remains).
//
// ## What moved in the existing tests, and what did not
//
// ADR-8 predicted that assertions matching a contiguous run of assistant text
// longer than their `cols` would begin failing. Re-verified here at TASK-283:
// **none did.** Only two tests in this file assert on `fragment()`-kind text at
// all — `a_reply_reciting_the_cli_earns_the_hand_off_line_at_a_terminal` (a
// 75-column reply at 200 columns) and `a_resized_window_lays_the_next_turn_out_
// at_the_new_width` (this REQ's own, which asserts *on* the break) — and the two
// `wait_for`s on the default fixture's reply are matching 14 and 17 columns
// against 100 and 200. Everything else in this file asserts on `line()`-kind
// output or on the `Prompter`'s own bytes, neither of which BR-3 touches (OQ-5).
// The exposure was real; the blast radius was empty. The comment at the
// hand-off test records the reason its margin is now load-bearing.

/// A pty width chosen to be **narrow enough to force every decision** the legs
/// below assert on: a sentence of ordinary prose does not fit it, and a
/// two-column table with one long cell cannot possibly be aligned in it.
///
/// It is also the number `prompt::terminal_width()` reports and hands to
/// `with_markdown` unchanged, so the width the renderer lays out at *is* this
/// constant — the pty's size is this feature's input, not a stage size.
const RENDERED_COLS: u16 = 60;

/// The audit paragraph as the **reader** sees it: markers consumed, words
/// otherwise untouched. Kept beside its marked-up spelling below, with a
/// fixture-integrity assertion tying the two together, so neither can drift into
/// asserting about a sentence the other does not contain.
const AUDIT_PARAGRAPH: &str = "The audit run by cargo audit produced a paragraph of prose \
                               that is wider than sixty columns and must be broken across rows.";

/// The same paragraph as the model wrote it, with one strong run and one code
/// span in it. Both are early in the sentence and neither straddles a break at
/// [`RENDERED_COLS`], which is what lets the SGR assertions name exact bytes.
const AUDIT_SOURCE: &str = "The **audit** run by `cargo audit` produced a paragraph of prose \
                            that is wider than sixty columns and must be broken across rows.";

/// The table's one data value. With `unsafe deserialization` beside it the
/// aligned layout needs 122 columns, so at 60 the transposition is forced rather
/// than merely available (BR-4).
const AUDIT_DETAIL: &str = "the parser reconstructs arbitrary objects from a pickled payload \
                            that arrives on an untrusted queue";

/// `text` with every CSI escape removed and every carriage return dropped —
/// what a reader sees, rather than what the stream carries.
///
/// Both halves are needed and for different reasons. The renderer's rows are
/// interleaved with the entry frame's own cursor motion (`\x1b[2A`, `\x1b[J`),
/// and the pty's `ONLCR` puts a `\r` before every `\n` it forwards. A row
/// measured or compared without stripping the two is a row measured against
/// bytes that occupy no column of the screen.
fn without_escapes(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut chars = text.chars().peekable();
    while let Some(c) = chars.next() {
        match c {
            '\r' => {}
            // CSI: `[`, parameter and intermediate bytes, then one final byte in
            // `0x40..=0x7e`. Anything else after ESC is a two-character escape,
            // and its second character has already been consumed by the `next`.
            '\x1b' if chars.peek() == Some(&'[') => {
                chars.next();
                for c in chars.by_ref() {
                    if ('\u{40}'..='\u{7e}').contains(&c) {
                        break;
                    }
                }
            }
            '\x1b' => {}
            _ => out.push(c),
        }
    }
    out
}

/// Every **SGR** escape in `text` — the `\x1b[…m` sequences that set a colour or
/// an attribute — rendered readably for a failure message.
///
/// Deliberately not "every escape", which is why AC-8's pty leg can make a
/// whole-transcript claim at all. The entry frame writes `\x1b[2A` and `\x1b[J`
/// on every redraw whatever the colour setting is, because that is **geometry**:
/// it is how the frame stays put while output scrolls above it, and `NO_COLOR`
/// has nothing to say about it. Styling is exactly the escapes whose final byte
/// is `m`, and styling is what AC-8 is about.
fn sgr_escapes(text: &str) -> Vec<String> {
    let bytes = text.as_bytes();
    let mut found: Vec<String> = Vec::new();
    let mut at = 0;
    while at + 1 < bytes.len() {
        if bytes[at] != 0x1b || bytes[at + 1] != b'[' {
            at += 1;
            continue;
        }
        let mut end = at + 2;
        while end < bytes.len() && !(0x40..=0x7e).contains(&bytes[end]) {
            end += 1;
        }
        if end < bytes.len() && bytes[end] == b'm' {
            found.push(format!(
                "ESC{}",
                String::from_utf8_lossy(&bytes[at + 1..=end])
            ));
        }
        at = end + 1;
    }
    found
}

/// The transcript's rows as a reader sees them.
fn display_rows(transcript: &str) -> Vec<String> {
    without_escapes(transcript)
        .lines()
        .map(str::to_owned)
        .collect()
}

/// The consecutive non-empty rows beginning at the first row that opens with
/// `head` — one rendered block, as it stands on screen.
///
/// This is how a wrap is asserted here rather than by pinning break columns. The
/// claim BR-3 makes is that a block was broken into rows that fit *and that no
/// word was lost or duplicated doing it*, which a rejoin states directly.
/// Pinning the columns would restate `wrap_ranges`' own unit tests through a
/// pty, and would move every time a fixture's wording did.
fn rendered_block(rows: &[String], head: &str) -> Vec<String> {
    let at = rows
        .iter()
        .position(|row| row.starts_with(head))
        .unwrap_or_else(|| panic!("no rendered row opens with {head:?}; rows:\n{rows:#?}"));
    rows[at..]
        .iter()
        .take_while(|row| !row.trim().is_empty())
        .cloned()
        .collect()
}

/// A `teton` session at a pty of a **known** width, driven by a scripted tier.
///
/// The three legs below need the same five facts and differ only in `cols`, the
/// script, and the client's environment, so the shape is stated once rather than
/// a fourth, fifth and sixth time.
struct RenderedSession {
    session: Box<dyn portable_pty::Child + Send + Sync>,
    transcript: Transcript,
    writer: Box<dyn Write + Send>,
    /// Held open so the writer and the reader thread keep a live master; never
    /// read directly.
    _master: Box<dyn portable_pty::MasterPty + Send>,
    /// Killed and its root removed by its own `Drop`; held so it outlives the
    /// session it is serving.
    _daemon: TestDaemon,
}

/// The daemon config every typed-turn leg here shares, plus `extra`.
///
/// One `kind = "local"` provider with **every** tier bound to it, for the
/// REQ-558 reason the other typed-turn tests in this file bind them: otherwise
/// the turn resolves to the unreachable remote provider and fails before it can
/// reply.
fn local_tier_config(extra: &str) -> String {
    let tiers: String = ["reflex", "scan", "build", "think"]
        .iter()
        .map(|t| format!("[[tiers]]\ntier = \"{t}\"\nprovider_id = \"local\"\n\n"))
        .collect();
    format!("[[providers]]\nid = \"local\"\nkind = \"local\"\n\n{tiers}{extra}")
}

impl RenderedSession {
    /// Open a session at a pty exactly `cols` wide whose scripted tier answers
    /// one typed turn per entry of `replies`, with `client_env` on the
    /// **client's** environment.
    ///
    /// `client_env` is the client's and not the daemon's because the decision
    /// AC-8 is about is the client's: `banner::color_enabled()` reads `NO_COLOR`
    /// and `TERM` in the process that owns the terminal, and the daemon has no
    /// say in it.
    ///
    /// Two environment facts are **pinned rather than inherited**, because a
    /// developer's shell or a CI runner sets both and either would silently
    /// invert a leg: `NO_COLOR` is removed (a shell that exports it would turn
    /// AC-12's SGR assertions into a test of the developer's preferences) and
    /// `TERM` is set to a non-`dumb` value (`color_enabled` refuses `dumb`, and
    /// a runner with no `TERM` at all is a different case again). AC-8 then puts
    /// `NO_COLOR` back deliberately, which is the whole of what it varies.
    fn open(cols: u16, replies: &[&str], client_env: &[(&str, &str)]) -> Self {
        Self::open_with_config(cols, replies, client_env, &local_tier_config(""))
    }

    /// [`Self::open`] with the daemon's whole config handed in rather than
    /// composed (REQ-621 TASK-413).
    ///
    /// Two of REQ-621's legs need a config the shared one cannot express: the
    /// running-tool leg needs `[permissions] default_level = "full"` so a
    /// `shell` call runs without a question standing between the turn and the
    /// row, and the RPC-error leg needs a tier bound to a provider that cannot
    /// answer. Appending would not do for the second — a tier names exactly one
    /// provider and the daemon refuses to start on a duplicate binding — so the
    /// whole document is the parameter, and [`local_tier_config`] is what every
    /// other leg passes.
    fn open_with_config(
        cols: u16,
        replies: &[&str],
        client_env: &[(&str, &str)],
        config: &str,
    ) -> Self {
        let daemon_path = daemon_bin();
        let daemon = TestDaemon::spawn_with(&daemon_path, config, replies);

        let pty = native_pty_system()
            .openpty(PtySize {
                rows: 40,
                cols,
                pixel_width: 0,
                pixel_height: 0,
            })
            .expect("openpty");

        let mut cmd = CommandBuilder::new(teton_bin());
        cmd.env("XDG_RUNTIME_DIR", &daemon.runtime_dir);
        // REQ-611 TASK-364: the same data directory the fixture daemon got, so a
        // CLI that autostarts one lands in `root` rather than the developer's home.
        cmd.env("XDG_DATA_HOME", daemon.root.join("d"));
        cmd.env("TETON_CONFIG", daemon.root.join("config.toml"));
        cmd.env("TETON_REPO_ROOT", &daemon.root);
        cmd.env_remove("NO_COLOR");
        cmd.env("TERM", "xterm-256color");
        for (key, value) in client_env {
            cmd.env(key, value);
        }
        let session = pty.slave.spawn_command(cmd).expect("spawn teton under pty");
        drop(pty.slave);
        let transcript = spawn_reader(pty.master.try_clone_reader().expect("pty reader"));
        let writer = pty.master.take_writer().expect("pty writer");
        let opened = Self {
            session,
            transcript,
            writer,
            _master: pty.master,
            _daemon: daemon,
        };
        assert!(
            wait_for(&opened.transcript, "ready (freeform)"),
            "the session never reached the entry prompt; transcript:\n{}",
            opened.snapshot()
        );
        opened
    }

    /// Type one line at the terminal.
    fn type_line(&mut self, line: &str) {
        self.writer
            .write_all(format!("{line}\r").as_bytes())
            .expect("type at the pty");
        self.writer.flush().ok();
    }

    /// Type `bytes` at the terminal exactly as given — no trailing return.
    ///
    /// [`Self::type_line`]'s other half, for REQ-621's AC-10 leg: what that
    /// leg is about is bytes reaching the kernel's line buffer *while the row
    /// is animating*, so it needs to choose when the return goes in rather than
    /// have one appended for it.
    fn type_raw(&mut self, bytes: &str) {
        self.writer
            .write_all(bytes.as_bytes())
            .expect("type at the pty");
        self.writer.flush().ok();
    }

    fn wait_until(&self, ready: impl Fn(&str) -> bool) -> bool {
        wait_until(&self.transcript, ready)
    }

    fn wait_for(&self, marker: &str) -> bool {
        wait_for(&self.transcript, marker)
    }

    fn snapshot(&self) -> String {
        snapshot(&self.transcript)
    }

    /// Kill the daemon this session is talking to, and reap it (REQ-621 AC-7).
    ///
    /// The `Drop` below kills it again and ignores the failure, so the leg does
    /// not have to hand ownership anywhere: what BR-12 is about is the
    /// *client's* control flow when the socket goes away mid-turn, and this is
    /// the only way to take the socket away.
    fn kill_daemon(&mut self) {
        let _ = self._daemon.child.kill();
        let _ = self._daemon.child.wait();
    }

    /// Run `teton cost` against this session's daemon and return its stdout.
    ///
    /// The producer as the oracle (LESSON-544): what a turn cost is the
    /// daemon's own figure, read back through the daemon's own report, rather
    /// than a price written into a test that would keep passing after the
    /// ledger changed its mind.
    fn cost_report(&self) -> String {
        let out = std::process::Command::new(teton_bin())
            .arg("cost")
            .env("XDG_RUNTIME_DIR", &self._daemon.runtime_dir)
            .env("XDG_DATA_HOME", self._daemon.root.join("d"))
            .env("TETON_CONFIG", self._daemon.root.join("config.toml"))
            .env("TETON_REPO_ROOT", &self._daemon.root)
            .output()
            .expect("run `teton cost` against the fixture daemon");
        String::from_utf8_lossy(&out.stdout).into_owned()
    }
}

impl Drop for RenderedSession {
    fn drop(&mut self) {
        let _ = self.session.kill();
        let _ = self.session.wait();
    }
}

/// **REQ-592 AC-12 — at a fixed terminal width, the bytes a scripted turn puts
/// on screen are wrapped rows, a transposed table, and SGR-styled spans.**
///
/// Written here rather than claimed as covered by `render.rs`'s unit tests. Those
/// prove the transform; this proves the *wiring* — that a terminal session builds
/// a surface with a renderer on it, at the width `TIOCGWINSZ` reports, and that
/// the bytes reaching the pty master are the rendered ones.
///
/// The table is deliberately the last thing in the reply, which couples this
/// test to the flush: a buffered table run is laid out when the run **ends**, and
/// a run that ends with the turn ends at `end_block()` (ADR-3). So the wait below
/// is a wait on the whole reply having been rendered, flush included.
#[test]
fn a_rendered_turn_wraps_its_rows_transposes_its_table_and_styles_its_spans() {
    // The two spellings of the paragraph are the same sentence. Without this the
    // rejoin below could pass against a fixture that had drifted, asserting that
    // the renderer reproduced a sentence nobody sent it.
    assert_eq!(
        AUDIT_SOURCE.replace("**", "").replace('`', ""),
        AUDIT_PARAGRAPH,
        "the marked-up and plain spellings of the audit paragraph must match"
    );
    // The precondition the wrap assertions rest on: the sentence does not fit.
    assert!(
        AUDIT_PARAGRAPH.chars().count() > RENDERED_COLS as usize,
        "the paragraph must not fit the window, or the test proves nothing"
    );

    let reply = format!(
        "## Audit findings\n\n{AUDIT_SOURCE}\n\n\
         | Finding | Detail |\n| --- | --- |\n\
         | unsafe deserialization | {AUDIT_DETAIL} |"
    );
    let mut session = RenderedSession::open(RENDERED_COLS, &[&reply], &[]);
    session.type_line("audit the parser");

    // The value's last word, and a single word is the one thing a wrap never
    // splits (`wrap_ranges`' whole-and-over-wide rule), so this marker survives
    // whatever the break positions turn out to be.
    assert!(
        session.wait_for("queue"),
        "the scripted reply never reached the screen; transcript:\n{}",
        session.snapshot()
    );

    let seen = session.snapshot();
    let rows = display_rows(&seen);

    // (1) BR-3: the paragraph is broken by the CLI, into rows that fit.
    assert!(
        !seen.contains(AUDIT_PARAGRAPH),
        "the paragraph reached the terminal in one piece, so nothing wrapped it \
         and the terminal is left to hard-wrap it mid-word — which is the defect \
         REQ-592 exists to remove; transcript:\n{seen}"
    );
    let paragraph = rendered_block(&rows, "The audit run by");
    assert!(
        paragraph.len() > 1,
        "the paragraph occupies one row, so it was not wrapped; rows:\n{paragraph:#?}"
    );
    for row in &paragraph {
        // ASCII throughout, so a character is a column here. The CJK case is
        // `markdown.rs`'s to prove, and it proves it with `display_width`
        // (ADR-5) rather than through a pty.
        assert!(
            row.chars().count() <= RENDERED_COLS as usize,
            "a rendered row is past the terminal's edge, so the terminal wraps it \
             again: {row:?} is {} columns in a {RENDERED_COLS}-column window",
            row.chars().count()
        );
    }
    assert_eq!(
        paragraph.join(" "),
        AUDIT_PARAGRAPH,
        "the wrapped rows must rejoin to exactly the sentence the model wrote — \
         no word lost at a break and none duplicated; transcript:\n{seen}"
    );

    // (2) The heading's markers are consumed and its text is not.
    assert!(
        !seen.contains("## Audit findings"),
        "the heading's `##` reached the screen, so nothing classified it; \
         transcript:\n{seen}"
    );
    assert!(
        rows.iter().any(|row| row == "Audit findings"),
        "the heading's own text must survive the markers being removed; \
         transcript:\n{seen}"
    );

    // (3) BR-4: a table too wide to align is transposed into labelled blocks.
    assert!(
        rows.iter()
            .any(|row| row == "Finding: unsafe deserialization"),
        "a table needing 122 columns in a {RENDERED_COLS}-column window must be \
         transposed — one line per column, carrying that column's header and \
         this row's value; transcript:\n{seen}"
    );
    // The two things it must not be instead, named separately because they fail
    // for different reasons. The raw source rows are `layout_table`'s
    // degrade-don't-truncate floor (ADR-2) and reaching it here would mean the
    // transposition refused; an aligned row would mean it was never tried.
    assert!(
        !seen.contains("| unsafe deserialization |"),
        "the table run reached the screen as its own source rows, which is the \
         last-resort floor rather than a layout; transcript:\n{seen}"
    );
    assert!(
        !rows
            .iter()
            .any(|row| row.contains("unsafe deserialization") && row.contains("the parser")),
        "the table was drawn aligned — one row carrying both cells — at a width \
         that cannot hold it, so the terminal hard-wraps it mid-cell; \
         transcript:\n{seen}"
    );
    // And the transposed value is itself wrapped under BR-3, onto continuation
    // rows carrying the fixed two-column indent rather than the label's width.
    assert!(
        rows.iter()
            .any(|row| row.starts_with("  ") && row.trim().ends_with("untrusted queue")),
        "the transposed value must wrap onto indented continuation rows; \
         transcript:\n{seen}"
    );

    // (4) BR-5: the styling is authored by the surface, from its own table, over
    // text that was defused first — and the markers that asked for it are gone.
    assert!(
        seen.contains("\x1b[1maudit\x1b[0m"),
        "`**audit**` must reach the screen as a bold run with its markers \
         consumed; transcript:\n{seen}"
    );
    assert!(
        seen.contains("\x1b[36mcargo audit\x1b[0m"),
        "an inline code span must reach the screen cyan — a second style, so the \
         SGR is being drawn from `inline_sgr`'s table rather than being one \
         attribute applied to everything; transcript:\n{seen}"
    );
    assert!(
        !seen.contains("**audit**") && !seen.contains("`cargo audit`"),
        "the inline markers must not survive alongside the styling they asked \
         for; transcript:\n{seen}"
    );
}

/// **REQ-592 AC-8 (pty leg) — `NO_COLOR` in the child's environment leaves the
/// rows wrapped and the transcript free of styling.**
///
/// The claim is scoped to **SGR** escapes, and that scoping is the honest part
/// rather than a weakening. The entry frame writes `\x1b[2A` and `\x1b[J` on
/// every redraw whatever the colour setting is: that is geometry, it is how the
/// frame stays put while output scrolls above it, and `NO_COLOR` says nothing
/// about it. A "no `\x1b` anywhere" assertion would therefore be false against a
/// perfectly behaved session — see `sgr_escapes`.
///
/// What makes the emptiness meaningful is the other two halves. The rows are
/// still wrapped and the markers are still consumed, so the renderer is *live*:
/// `NO_COLOR` turned the styling off without turning the layout off, which is
/// the property AC-8 is actually about. Without them, this test would pass
/// against a build where `with_markdown` was never reached at all.
#[test]
fn a_no_color_session_wraps_its_rows_and_authors_no_styling() {
    let mut session = RenderedSession::open(RENDERED_COLS, &[AUDIT_SOURCE], &[("NO_COLOR", "1")]);
    session.type_line("audit the parser");

    // The paragraph's last word: a single word, never split by a break, and the
    // tail of a reply that carries no trailing newline — so waiting on it is
    // also a wait on `end_block()` having released the held row.
    assert!(
        session.wait_for("rows."),
        "the scripted reply never reached the screen; transcript:\n{}",
        session.snapshot()
    );

    let seen = session.snapshot();
    let rows = display_rows(&seen);

    // (1) The layout still happened.
    assert!(
        !seen.contains(AUDIT_PARAGRAPH),
        "the paragraph reached the terminal in one piece: `NO_COLOR` turned the \
         layout off as well as the styling; transcript:\n{seen}"
    );
    let paragraph = rendered_block(&rows, "The audit run by");
    assert!(
        paragraph.len() > 1,
        "the paragraph occupies one row, so it was not wrapped; rows:\n{paragraph:#?}"
    );
    assert_eq!(
        paragraph.join(" "),
        AUDIT_PARAGRAPH,
        "the wrapped rows must rejoin to exactly the sentence the model wrote; \
         transcript:\n{seen}"
    );

    // (2) The renderer really is the thing that laid them out — an inert surface
    // would have passed the markers straight through.
    assert!(
        !seen.contains("**audit**") && !seen.contains("`cargo audit`"),
        "the inline markers survived, so no renderer ran and (1) proves nothing \
         about colour; transcript:\n{seen}"
    );

    // (3) And it authored no styling at all. Not "no bold" — no SGR of any kind,
    // which is a property of the uncoloured code path never reaching
    // `inline_sgr` rather than of a table that happened to return nothing.
    let styling = sgr_escapes(&seen);
    assert!(
        styling.is_empty(),
        "AC-8: a `NO_COLOR` session must author no styling escape anywhere; \
         found {styling:?}; transcript:\n{seen:?}"
    );

    // (4) The negative above is about colour and not about a session that
    // emitted nothing: the frame's cursor motion is still there.
    assert!(
        seen.contains('\x1b'),
        "no escape of any kind reached the terminal, so (3) is vacuous — this is \
         not a real terminal session; transcript:\n{seen:?}"
    );
}

/// **REQ-592 AC-10 (pty leg) — a reply whose final chunk carries no trailing
/// newline has its last row on screen, above the entry frame that follows.**
///
/// ## Why this is a separate test from the hand-off one
///
/// `a_reply_reciting_the_cli_earns_the_hand_off_line_at_a_terminal` pins a reply
/// with no trailing newline and asserts its tail lands before the hand-off line,
/// and TASK-280 recorded it as AC-10's terminal leg. Re-verified at TASK-283 now
/// that TASK-281 has wired the renderer in, and it is **not** that leg: the
/// hand-off is a `line()`, and `line()` calls `emit_pending()` before it claims
/// its row (BR-8). So that test's tail is released by the hand-off's own write,
/// and it passes byte-for-byte against a pump that never calls `end_block()` at
/// all. It exercises the markdown path — the reply is buffered and re-emitted by
/// the renderer rather than streamed — but it cannot see the flush.
///
/// This leg removes that particular flusher: the reply recites no CLI, so
/// `hand_off_after_turn` writes nothing, and the entry frame that follows goes
/// straight to stdout through the `Prompter`, which never touches a `Surface`
/// (ADR-4).
///
/// ## What no pty leg can isolate, stated rather than implied
///
/// It still does not isolate `end_block()`, and TASK-283 found that by mutation
/// rather than by reading: **deleting both `end_block()` calls from `client.rs`
/// leaves this whole file green.** `main.rs`'s Ok arm ends every non-verbose turn
/// with `ctx.surface.line(LineKind::Info, "")` — a blank row so the next entry
/// frame starts clean, and a line that predates this REQ — and `line()` emits the
/// pending buffer before it claims its row (BR-8). Every reachable arm of the
/// turn loop writes through a `Surface` after the call returns, so the tail is
/// released on all of them whether or not the pump flushes.
///
/// That is not a defect and it does not weaken AC-10, whose claim is about what
/// the reader sees: this test fails if *nothing* releases the tail, which is the
/// property. It does mean the flush half of `end_block()` has no terminal
/// witness — the same observation ADR-4 already made when it withdrew the
/// prompt-path call site ("`line()` already emits the pending buffer"), reaching
/// one call site further than the ADR took it. The half that **does** have one
/// is the fence bit, and `a_turn_boundary_closes_an_unclosed_fence_at_a_terminal`
/// below is its leg.
///
/// Mutation for *this* test: make `Surface::end_block` and `Surface::line` both
/// stop calling `emit_pending`, and the last row never arrives — the wait below
/// spends its whole window while the head row is on screen the entire time.
#[test]
fn a_reply_without_a_trailing_newline_shows_its_last_row_above_the_frame() {
    const HEAD: &str = "the review is finished and nothing is outstanding";
    const TAIL: &str = "this last row carries no trailing newline";
    // The entry frame's top rule, which is the terminal's width in box-drawing
    // characters — the first bytes of the frame drawn after the turn.
    let frame = "\u{2500}".repeat(RENDERED_COLS as usize);

    // Both rows fit, so "the last row" is one row and the assertions below are
    // about the flush rather than about the wrap (which AC-12's leg owns).
    assert!(
        HEAD.chars().count() <= RENDERED_COLS as usize
            && TAIL.chars().count() <= RENDERED_COLS as usize,
        "both rows must fit the window, or this leg is testing the wrap instead"
    );

    // `ScriptedFileEngine` trims each block, so the reply's final chunk carries
    // no trailing newline by construction — which is the common shape of a model
    // reply and the one a streaming renderer holds.
    let reply = format!("{HEAD}\n{TAIL}");
    let mut session = RenderedSession::open(RENDERED_COLS, &[&reply], &[]);
    session.type_line("is anything left?");

    // (1) The precondition: the turn ran and the *complete* line was rendered as
    // it streamed. Without this, (2) failing could mean the turn never happened.
    assert!(
        session.wait_for(HEAD),
        "the scripted reply never reached the screen; transcript:\n{}",
        session.snapshot()
    );

    // (2) AC-10 itself, and (3) its ordering half, as one condition: the tail is
    // on screen *and* a frame was drawn after it. Two `wait_for` calls could not
    // say this — the frame is in the transcript from startup, so a second
    // `contains` would return on a state that has not happened yet.
    assert!(
        wait_until(&session.transcript, |seen| {
            seen.find(TAIL)
                .is_some_and(|at| seen[at + TAIL.len()..].contains(&frame))
        }),
        "AC-10: the reply's last row must be on screen, above the entry frame \
         that follows the turn. A tail still held is a tail the reader never \
         sees; transcript:\n{}",
        session.snapshot()
    );

    let seen = session.snapshot();

    // (4) The fixture is the shape it claims to be. A hand-off line would be a
    // *second* thing writing through the surface between the reply and the
    // frame, and the ordering above would then hold for a reason that has
    // nothing to do with the turn ending — which is how the hand-off test came
    // to look like AC-10's leg without being it.
    assert!(
        !seen.contains("/provider setup"),
        "this fixture must draw no hand-off line, or the ordering above is about \
         the hand-off's own write rather than the turn's end; transcript:\n{seen}"
    );
}

/// **REQ-592 — a turn boundary closes a fence the reply left open, and the pty
/// is the only place that can see it happen.**
///
/// The companion to the leg above, and the reason `end_block()` is a verb at all
/// rather than a line in `hand_off_after_turn`. Its second half — "then the fence
/// bit is cleared" — is the one no other path in `render.rs` performs: inside a
/// fence, "this line is not markup" is exactly what the renderer is supposed to
/// believe, so only a caller that knows the block is over can say otherwise.
///
/// A reply that opens a ``` ``` ``` and never closes it therefore leaves the bit
/// set, and a bit that survives the turn makes **every line of every subsequent
/// turn** render verbatim — no wrap, no styling, for the rest of the session.
/// That is a whole-session defect produced by one malformed reply, and it is
/// invisible to `cli_e2e` (no renderer at all) and to `render.rs`'s unit tests
/// (which call `end_block()` themselves, so they can only pin what the verb does,
/// never that the pump reaches it between two real turns).
///
/// Mutation: delete either `end_block()` call from `client.rs`'s pump and the
/// second turn's paragraph arrives byte-for-byte as the model wrote it, markers
/// and all, on one over-wide row.
#[test]
fn a_turn_boundary_closes_an_unclosed_fence_at_a_terminal() {
    // Opened, one line of content, never closed — the shape a reply takes when a
    // model runs out of budget mid-block.
    const UNCLOSED: &str = "```rust\nlet parsed = 1;";
    const FENCED_ROW: &str = "let parsed = 1;";

    let mut session = RenderedSession::open(RENDERED_COLS, &[UNCLOSED, AUDIT_SOURCE], &[]);
    session.type_line("show me the parser");

    // (1) The precondition: the fence really did open. Its content is on screen
    // verbatim, which is what BR-6 promises and what proves the bit was set —
    // without it the paragraph below could wrap for the trivial reason that no
    // fence was ever recognized.
    assert!(
        session.wait_for(FENCED_ROW),
        "the fenced reply never reached the screen; transcript:\n{}",
        session.snapshot()
    );

    session.type_line("now describe the audit");
    // The paragraph's last word, which is present in both outcomes — verbatim or
    // wrapped — so this wait cannot itself decide the assertion below.
    assert!(
        session.wait_for("rows."),
        "the second turn never produced its reply; transcript:\n{}",
        session.snapshot()
    );

    let seen = session.snapshot();

    // (2) The claim: the fence did not survive the turn boundary.
    assert!(
        !seen.contains(AUDIT_SOURCE),
        "the second turn's paragraph was rendered verbatim, so the fence the \
         first reply left open is still set: `end_block()` never cleared it, and \
         every line of every remaining turn of this session renders as code; \
         transcript:\n{seen}"
    );

    // (3) And it is genuinely being laid out again, not merely different.
    let paragraph = rendered_block(&display_rows(&seen), "The audit run by");
    assert!(
        paragraph.len() > 1,
        "the paragraph occupies one row after the fence closed; rows:\n{paragraph:#?}"
    );
    assert_eq!(
        paragraph.join(" "),
        AUDIT_PARAGRAPH,
        "the turn after a fenced one must lay out exactly as any other turn does; \
         transcript:\n{seen}"
    );
}

/// A `teton` session under a pty against `daemon`, ready at the entry prompt.
/// Returns the child, its transcript, and the pty writer (REQ-611 tests).
fn transcript_session(
    daemon: &TestDaemon,
) -> (
    Box<dyn portable_pty::Child + Send + Sync>,
    Transcript,
    Box<dyn Write + Send>,
) {
    let pty = native_pty_system()
        .openpty(PtySize {
            rows: 40,
            cols: 160,
            pixel_width: 0,
            pixel_height: 0,
        })
        .expect("openpty");
    let mut cmd = CommandBuilder::new(teton_bin());
    cmd.env("XDG_RUNTIME_DIR", &daemon.runtime_dir);
    cmd.env("XDG_DATA_HOME", daemon.root.join("d"));
    cmd.env("TETON_CONFIG", daemon.root.join("config.toml"));
    cmd.env("TETON_REPO_ROOT", &daemon.root);
    let session = pty.slave.spawn_command(cmd).expect("spawn teton under pty");
    drop(pty.slave);
    let transcript = spawn_reader(pty.master.try_clone_reader().expect("pty reader"));
    let writer = pty.master.take_writer().expect("pty writer");
    assert!(
        wait_for(&transcript, "ready (freeform)"),
        "the session never reached the entry prompt; transcript:\n{}",
        snapshot(&transcript)
    );
    (session, transcript, writer)
}

fn count_lines_containing(text: &str, needle: &str) -> usize {
    text.lines().filter(|line| line.contains(needle)).count()
}

/// Wait until `text` holds at least `n` lines containing `needle`, bounded.
fn wait_for_count(transcript: &Transcript, needle: &str, n: usize) -> bool {
    let deadline = Instant::now() + Duration::from_secs(15);
    while Instant::now() < deadline {
        if count_lines_containing(&snapshot(transcript), needle) >= n {
            return true;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    false
}

/// REQ-611 AC-3 (CLI half): `/transcript on` prints the handler's one line
/// (naming the file) and the `transcript_state` notice arrives exactly once.
///
/// **Mutation (run 2026-09-03):** suppressing the `transcript_state` render
/// arm in `session_ui` (line composed, never drawn) reddened "the
/// transcript_state notice must render"; restored.
#[test]
fn transcript_on_prints_one_line_and_one_state_notice() {
    let daemon = TestDaemon::spawn(&daemon_bin());
    let (mut session, transcript, mut writer) = transcript_session(&daemon);

    writer
        .write_all(b"/transcript on\r")
        .expect("type the command");
    writer.flush().ok();
    assert!(
        wait_for(&transcript, "recording to"),
        "`/transcript on` must name the file it records to; transcript:\n{}",
        snapshot(&transcript)
    );
    let landed = wait_for_count(&transcript, "transcript: on", 2);
    let text = snapshot(&transcript);
    let _ = session.kill();
    let _ = session.wait();

    assert!(
        landed,
        "the transcript_state notice must render; transcript:\n{text}"
    );
    assert_eq!(
        count_lines_containing(&text, "recording to"),
        1,
        "the handler prints one line; transcript:\n{text}"
    );
    assert_eq!(
        text.lines()
            .filter(|line| line.trim_end().ends_with("transcript: on"))
            .count(),
        1,
        "the state notice arrives once, not twice; transcript:\n{text}"
    );
    assert!(
        text.contains("/transcripts/") && text.contains(".jsonl"),
        "the path under the data directory is shown to the asker (BR-15); transcript:\n{text}"
    );
}

/// REQ-611 AC-4 (CLI half): `off` stops, `on` again resumes the same file.
///
/// **Mutation (run 2026-09-03):** making `off` print the `recording to` form
/// reddened the `stopped` count; restored.
#[test]
fn transcript_off_then_on_prints_the_resume() {
    let daemon = TestDaemon::spawn(&daemon_bin());
    let (mut session, transcript, mut writer) = transcript_session(&daemon);

    writer.write_all(b"/transcript on\r").expect("type on");
    writer.flush().ok();
    assert!(
        wait_for(&transcript, "recording to"),
        "on; transcript:\n{}",
        snapshot(&transcript)
    );
    let first = snapshot(&transcript);
    let path = first
        .lines()
        .find_map(|line| line.split("recording to ").nth(1))
        .map(|p| p.trim().to_owned())
        .expect("the first `on` names a path");

    writer.write_all(b"/transcript off\r").expect("type off");
    writer.flush().ok();
    assert!(
        wait_for(&transcript, "stopped"),
        "off; transcript:\n{}",
        snapshot(&transcript)
    );

    writer
        .write_all(b"/transcript on\r")
        .expect("type on again");
    writer.flush().ok();
    let resumed = wait_for_count(&transcript, "recording to", 2);
    let text = snapshot(&transcript);
    let _ = session.kill();
    let _ = session.wait();

    assert!(resumed, "the second `on` prints again; transcript:\n{text}");
    let second = text
        .lines()
        .filter_map(|line| line.split("recording to ").nth(1))
        .nth(1)
        .map(|p| p.trim().to_owned())
        .expect("the second `on` names a path");
    assert_eq!(
        second, path,
        "the second `on` resumes the SAME file (AC-4); transcript:\n{text}"
    );
    assert_eq!(
        count_lines_containing(&text, "stopped"),
        1,
        "one stop; transcript:\n{text}"
    );
    assert_eq!(
        count_lines_containing(&text, "recording to"),
        2,
        "two starts; transcript:\n{text}"
    );
}

/// REQ-611 AC-5 (CLI half): bare `/transcript` reports the state, the path,
/// the record count, and — after the daemon refuses a directory that is wider
/// than owner-only — the degraded reason.
///
/// **Mutation (run 2026-09-03):** dropping the `degraded:` suffix from the
/// render reddened the final assertion; restored.
#[test]
fn bare_transcript_prints_status_with_path_and_degraded_reason() {
    use std::os::unix::fs::PermissionsExt;

    let daemon = TestDaemon::spawn(&daemon_bin());
    // A pre-existing, group-readable transcript directory: the daemon refuses
    // it at `on` (BR-9 / AC-11) and the session is degraded from then on.
    let dir = daemon.root.join("d").join("teton").join("transcripts");
    std::fs::create_dir_all(&dir).expect("plant the directory");
    std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o755)).expect("widen it");

    let (mut session, transcript, mut writer) = transcript_session(&daemon);

    writer.write_all(b"/transcript\r").expect("type bare");
    writer.flush().ok();
    assert!(
        wait_for(&transcript, "transcript: off"),
        "bare `/transcript` reports the state; transcript:\n{}",
        snapshot(&transcript)
    );

    writer.write_all(b"/transcript on\r").expect("type on");
    writer.flush().ok();
    assert!(
        wait_for(&transcript, "degraded:"),
        "a refused directory is reported as degraded; transcript:\n{}",
        snapshot(&transcript)
    );

    writer.write_all(b"/transcript\r").expect("type bare again");
    writer.flush().ok();
    let reported = wait_for_count(&transcript, "degraded:", 2);
    let text = snapshot(&transcript);
    let _ = session.kill();
    let _ = session.wait();

    assert!(
        reported,
        "bare `/transcript` reports the degraded reason (AC-5); transcript:\n{text}"
    );
    assert!(
        count_lines_containing(&text, "transcript: off") >= 2,
        "the state stays off on a degraded session (BR-6); transcript:\n{text}"
    );
}

/// REQ-611 AC-14: with the transcript on, the REQ-572 key step (typed
/// echo-off, declined at the confirm) leaves no trace of the planted key in
/// any `*.jsonl` under the transcript directory — while the transcript itself
/// is non-empty, so the sweep is over a real file and not an absence.
///
/// **Mutation (run 2026-09-03):** planting `PLANTED_KEY` into a throwaway
/// `decoy.jsonl` in the directory before the sweep reddened the "never
/// reaches" assertion, proving the sweep fires; restored.
#[test]
fn the_fixture_key_never_reaches_the_transcript_directory() {
    let daemon_path = daemon_bin();
    // Every tier bound to the scripted local tier, so the session is servable
    // and — the part this test needs — the daemon reports a local model, which
    // is what makes the `search` tier (the only branch that asks for a key)
    // offerable at all.
    let tiers: String = ["reflex", "scan", "build", "think"]
        .iter()
        .map(|t| format!("[[tiers]]\ntier = \"{t}\"\nprovider_id = \"local\"\n\n"))
        .collect();
    let config = format!("[[providers]]\nid = \"local\"\nkind = \"local\"\n\n{tiers}");
    let daemon = TestDaemon::spawn_with(&daemon_path, &config, &["a scripted reply."]);
    let config_path = daemon.root.join("config.toml");
    let (mut session, transcript, mut writer) = {
        let pty = native_pty_system()
            .openpty(PtySize {
                rows: 40,
                cols: 120,
                pixel_width: 0,
                pixel_height: 0,
            })
            .expect("openpty");
        let mut cmd = CommandBuilder::new(teton_bin());
        cmd.env("XDG_RUNTIME_DIR", &daemon.runtime_dir);
        cmd.env("XDG_DATA_HOME", daemon.root.join("d"));
        cmd.env("TETON_CONFIG", &config_path);
        cmd.env("TETON_REPO_ROOT", &daemon.root);
        let session = pty.slave.spawn_command(cmd).expect("spawn teton under pty");
        drop(pty.slave);
        let transcript = spawn_reader(pty.master.try_clone_reader().expect("pty reader"));
        let writer = pty.master.take_writer().expect("pty writer");
        assert!(
            wait_for(&transcript, "ready (freeform)"),
            "the session never reached the entry prompt; transcript:\n{}",
            snapshot(&transcript)
        );
        (session, transcript, writer)
    };

    let step = |writer: &mut Box<dyn Write + Send>, text: &str, until: &str| {
        writer
            .write_all(text.as_bytes())
            .expect("type into the pty");
        writer.flush().ok();
        assert!(
            wait_for(&transcript, until),
            "the walk never reached {until:?}; transcript:\n{}",
            snapshot(&transcript)
        );
    };

    step(&mut writer, "/transcript on\r", "recording to");
    step(&mut writer, "/web setup\r", "tier [1-3");
    step(&mut writer, "3\r", "search endpoint");
    step(
        &mut writer,
        "https://api.search.brave.com/res/v1/web/search\r",
        "does this backend need an API key?",
    );
    step(
        &mut writer,
        &format!("{ECHO_WITNESS}\r"),
        "auth header template",
    );
    step(&mut writer, "\r", "API key (not shown");
    step(
        &mut writer,
        &format!("{PLANTED_KEY}\r"),
        "write this to your config?",
    );
    writer.write_all(b"n\r").expect("decline the confirm");
    writer.flush().ok();
    assert!(
        wait_for(&transcript, "no key was stored"),
        "the decline must land; transcript:\n{}",
        snapshot(&transcript)
    );
    step(&mut writer, "/transcript off\r", "stopped");
    let _ = session.kill();
    let _ = session.wait();

    let dir = daemon.root.join("d").join("teton").join("transcripts");
    let files: Vec<std::path::PathBuf> = std::fs::read_dir(&dir)
        .unwrap_or_else(|e| panic!("the transcript directory exists after `/transcript on`: {e}"))
        .flatten()
        .map(|e| e.path())
        .filter(|p| p.extension().and_then(|x| x.to_str()) == Some("jsonl"))
        .collect();
    assert!(
        !files.is_empty(),
        "the session wrote a transcript under {}",
        dir.display()
    );
    for file in &files {
        let text = std::fs::read_to_string(file).expect("the transcript is readable");
        assert!(
            text.contains("transcript_opened"),
            "the sweep runs over a real transcript: {}",
            file.display()
        );
        assert!(
            !text.contains(PLANTED_KEY) && !text.contains("PLANTED-DO-NOT-ECHO"),
            "the planted key never reaches the transcript (AC-14): {}",
            file.display()
        );
    }
}

/// REQ-611 AC-5 (usage leg): an argument that is neither `on` nor `off` prints
/// the usage line and sends nothing.
///
/// **Mutation (run 2026-09-03):** treating an unknown argument as `Status`
/// reddened the assertion (a status line printed instead); restored.
#[test]
fn transcript_unknown_argument_prints_the_usage_line() {
    let daemon = TestDaemon::spawn(&daemon_bin());
    let (mut session, transcript, mut writer) = transcript_session(&daemon);
    writer
        .write_all(b"/transcript maybe\r")
        .expect("type the command");
    writer.flush().ok();
    let printed = wait_for(&transcript, "unknown transcript argument `maybe`");
    let text = snapshot(&transcript);
    let _ = session.kill();
    let _ = session.wait();
    assert!(
        printed,
        "the usage line names the argument; transcript:\n{text}"
    );
    assert!(
        !text.contains("recording to") && !text.contains("transcript: on"),
        "nothing was switched or reported; transcript:\n{text}"
    );
}

// ---------------------------------------------------------------------------
// REQ-612 AC-3 — the truncation notice, at a terminal
// ---------------------------------------------------------------------------
//
// The piped suite owns the *content* of this line (`cli_e2e`'s
// `a_truncated_or_withheld_notes_file_is_announced_and_doctor_advises`), and it
// owns it against a surface that draws no frame. What a pipe cannot show is
// that the line survives the terminal surface: the notice arrives on the event
// channel while the session sits at its entry prompt, and REQ-556's rule is
// that such a line is drawn *above* the frame with nothing typed. A notice that
// reached a pipe and not a pty would be a line every interactive user misses,
// which is the whole audience BR-3's "nothing is clamped in silence" is for.

/// **REQ-612 AC-3 / BR-3, the terminal half.** A session in a repository whose
/// `TETON.md` is over the 8 KiB cap prints the truncation notice at a real
/// terminal, with `/verbose` off, naming both figures and the remedy.
///
/// ## The arithmetic is the fixture's, and it is exact
///
/// 129 lines of 64 bytes is 8,256 on disk; the cut takes the last line boundary
/// at or under 8,192, which is line 128 exactly — so the notice must read
/// **8,256** and **8,192**, and a renderer that reported the block's length
/// (frame included) or the cap instead of what was kept fails on the digits
/// rather than on a substring. Both figures are asserted, because either alone
/// is satisfiable by a build that reported the same number twice.
///
/// ## Why the switch is toggled rather than the launch awaited
///
/// `repo_context_state` is published only when the state is *news* (ADR-3), and
/// a client that attached after `session/create` may never have seen the load.
/// `/context off` then `/context on` makes the announcement follow a change this
/// test made, which is what `cli_e2e`'s piped twin does and for the same reason
/// — a test written against the launch ordering would be flaky rather than
/// wrong.
///
/// **Mutation (run 2026-09-03):** putting `session_ui::format_repo_context`'s
/// `Truncated` arm behind the `verbose` gate leaves the session quiet and this
/// test red on its notice wait, which is BR-3's claim exactly.
#[test]
fn the_truncated_notes_notice_reaches_the_terminal() {
    let daemon_path = daemon_bin();
    let daemon = TestDaemon::spawn(&daemon_path);

    // A project root — only a `project`-kind root is read (BR-1), so without
    // the marker file this session would be asserting the `absent` path.
    let project = daemon.root.join("big");
    std::fs::create_dir_all(&project).unwrap();
    std::fs::write(project.join("Cargo.toml"), "[package]\nname = \"big\"\n").unwrap();

    let line = format!(
        "{:<63}\n",
        "The crates live under crates/; the cap will bite below this."
    );
    assert_eq!(
        line.len(),
        64,
        "this fixture's two figures are derived from 64-byte lines"
    );
    let notes = line.repeat(129);
    assert_eq!(notes.len(), 8_256, "129 whole lines of 64 bytes");
    std::fs::write(project.join("TETON.md"), &notes).unwrap();

    // Wide enough that the notice is one row: the claim is about the bytes the
    // surface drew, and a wrapped line would make `contains` a test of the
    // terminal's width.
    let pty = native_pty_system()
        .openpty(PtySize {
            rows: 40,
            cols: 200,
            pixel_width: 0,
            pixel_height: 0,
        })
        .expect("openpty");
    let mut cmd = CommandBuilder::new(teton_bin());
    cmd.args(["--cwd", project.to_str().unwrap()]);
    cmd.env("XDG_RUNTIME_DIR", &daemon.runtime_dir);
    cmd.env("XDG_DATA_HOME", daemon.root.join("d"));
    cmd.env("TETON_CONFIG", daemon.root.join("config.toml"));
    cmd.env("TETON_REPO_ROOT", &daemon.root);
    let mut session = pty.slave.spawn_command(cmd).expect("spawn teton under pty");
    drop(pty.slave);
    let transcript = spawn_reader(pty.master.try_clone_reader().expect("pty reader"));
    let mut writer = pty.master.take_writer().expect("pty writer");

    assert!(
        wait_for(&transcript, "ready (freeform)"),
        "the session never reached the entry prompt; transcript:\n{}",
        snapshot(&transcript)
    );

    writer
        .write_all(b"/context off\r")
        .expect("type /context off");
    writer.flush().ok();
    let switched_off = wait_for(&transcript, "nothing was opened");
    writer
        .write_all(b"/context on\r")
        .expect("type /context on");
    writer.flush().ok();
    let announced = wait_for(&transcript, "trim the file or move detail below the fold");
    let seen = snapshot(&transcript);
    let _ = session.kill();
    let _ = session.wait();

    assert!(
        switched_off,
        "`/context off` never reported the switch, so the `on` below announced \
         nothing; transcript:\n{seen}\ndaemon log:\n{}",
        std::fs::read_to_string(daemon.root.join("tetond.log")).unwrap_or_default()
    );
    assert!(
        announced,
        "the truncation notice never reached the terminal; transcript:\n{seen}\n\
         daemon log:\n{}",
        std::fs::read_to_string(daemon.root.join("tetond.log")).unwrap_or_default()
    );
    assert!(
        seen.contains("context: TETON.md is 8,256 bytes"),
        "the notice must name the file and its size on disk; transcript:\n{seen}"
    );
    assert!(
        seen.contains("the first 8,192 are resident"),
        "the notice must name what the model actually has — the bytes kept, not \
         the cap and not the block's own length; transcript:\n{seen}"
    );
    assert!(
        !seen.contains("route ["),
        "this session is quiet: a `/verbose` route line would mean the notice \
         above proves nothing about BR-3's ungated announcement; transcript:\n{seen}"
    );
}

// ---------------------------------------------------------------------------
// REQ-621 — the live activity row at a real terminal (TASK-413)
// ---------------------------------------------------------------------------
//
// ## Why every leg below is here, and can be nowhere else
//
// `Surface::has_live_rows` answers `true` for exactly one constructor —
// `PlainSurface::with_markdown`, the one `main.rs` reaches for when stdout is a
// terminal — and the pump reads that answer once per call, into `RowState::live`.
// Answered `false` the pump keeps the blocking receive it has always had and
// never enters its tick arm at all, so a piped run cannot emit a frame it did
// not emit before. That is how BR-6 holds by construction, and it is also what
// makes `cli_e2e` **structurally blind** to every claim below rather than merely
// uninterested in it.
//
// The two halves that are testable without a terminal are tested without one:
// `activity.rs` pins the frame text against a table of literals, and
// `client.rs` pins the pump's withdraw/dispatch/redraw discipline against a
// `RecordingSurface`. What is left — that a real session wakes on its own clock
// with the daemon silent, at the width `TIOCGWINSZ` reports, and takes the row
// back before anything durable prints or the turn ends — is a fact only a
// process observed from outside can produce (BR-7, LESSON-481).
//
// ## How a turn is held open, and why that is a fixture and not a delay
//
// The row exists only *while a turn is working*, so a leg that looks at it needs
// a turn that is still working when it looks. `@delay-ms <n>` as a scripted
// block's first line supplies exactly that (ADR-621-6): the fixture engine
// sleeps that long before its first token and strips the line, so nothing
// test-visible about the reply changes but its timing. It rides
// `TETON_TEST_SEAMS`, the master switch a release build refuses to start under,
// and it is part of the script grammar of an engine that exists only for tests.
// REQ-556 left its dots-advancing leg uncovered rather than invent a
// *production* delay to make it possible; this is the other trade, and BUG-191
// is what claiming pty coverage without the leg looks like.
//
// ## What these legs deliberately do not prove
//
// **The `preparing` frame.** BR-2 has the row say `preparing turn` until
// `route_decided` names something, and no leg here sees that frame. The daemon
// publishes its first `route_decided` well inside one `FRAME_INTERVAL` of the
// prompt going on the wire — the classifier's reflex route arrives first, then
// the turn's own — so the row's *first* paint is already an `awaiting_model`
// sentence, drawn from the message arm rather than from a tick. Nothing in the
// fixture can hold the daemon quiet for that first 120 ms: the only seam that
// could is `Connection`'s own receive, which is reachable from a unit test and
// not from a process. The frame is pinned there instead —
// `client.rs::the_pump_ticks_while_the_daemon_is_silent` asserts the literal
// `⠋ preparing turn · 0s · turn 0s` over a `RecordingSurface` — and what is
// observed here is the
// property AC-1 is actually about: a row is on screen before the first reply
// byte, naming a phase and a model the daemon reported, with its counter
// advancing.
//
// **A non-zero cost so far.** No scripted tier can be priced, and that is by
// design rather than by omission: the local model is absent from the bundled
// price table, so a local call is recorded *unpriced* and never assigned a
// guessed cost (REQ-564 BR-9). Its `cost_recorded` rows therefore carry
// `usd_micros = 0`, and BR-3's "shown once it is non-zero" correctly shows
// nothing. The running-tool leg asserts that against the daemon's **own**
// figure rather than against a literal, so a build that started pricing local
// calls turns the leg red instead of leaving it vacuous (LESSON-544); the
// non-zero rendering is `activity.rs`'s frame table's to prove.
//
// **A mid-stream stall (OQ-2).** BR-11's streaming clause needs reply bytes to
// stop for longer than the quiet bound *after* some have arrived, and the
// directive holds a block before its first token rather than between two of
// them. `activity.rs` covers it (`Streaming` past the bound renders `receiving
// the reply` with the annotation); the pty leg would need a second directive,
// which is a fixture change this task did not make.

/// Every glyph the activity row can open with — [`common::ACTIVITY_GLYPHS`],
/// under the name the legs below read it by.
///
/// It lives in `tests/common` because `cli_e2e` needs the same list for the
/// opposite claim (BR-6: none of them reaches a pipe), and because a list
/// imported from `activity.rs` would agree with whatever `activity.rs` said
/// (LESSON-569). Nothing else this binary prints draws braille, so a search for
/// one of these characters is a search for an activity row — which is what lets
/// the residue claims below be made over a whole screen.
const ROW_GLYPHS: [char; 11] = common::ACTIVITY_GLYPHS;

/// The glyph a stalled row shows in place of a spinner frame (ADR-621-5).
const STALLED_GLYPH: char = '⠿';

/// The two config lines every leg that needs a tool to run unattended carries.
///
/// `full` is the level at which a `shell` call runs without a question standing
/// between the turn and the row — at the default level the row correctly steps
/// aside for the permission prompt (BR-1), which is a different leg's subject.
///
/// `generate = "never"` is there because the first turn of a session inside a
/// project *offers* to write a `TETON.md` (REQ-613), and at `full` the
/// permission gate grants that offer without asking — so a fixture that only
/// meant to allow `sleep 3` would also have the daemon draft a file into
/// whichever directory `cargo test` happens to run from. Suppressing the offer
/// is the narrow fix; the level is what the leg is actually varying.
const FULL_AND_NO_CONTEXT_OFFER: &str =
    "[permissions]\ndefault_level = \"full\"\n\n[context]\ngenerate = \"never\"\n\n";

/// The bytes a repaint of a live row opens with (`PlainSurface::repaint_row_above`).
///
/// The row's *animation*, specifically: a first draw goes through `line()` and a
/// withdraw through `\x1b[{n}A\r\x1b[K`, and only a repaint saves the cursor
/// first. Counting these is therefore counting ticks that painted, which is
/// what the mutation recorded on the leg below removes.
const REPAINT_OPEN: &str = "\x1b[s\x1b[1A\r\x1b[K";

/// A repaint's first two escapes: save the cursor, then step up a row.
///
/// A prefix of [`REPAINT_OPEN`] rather than the whole of it, because the claim
/// it serves is "no repaint was *attempted* past this point" (AC-10, half 2).
/// Counting the full sequence would miss a build that saved and stepped up and
/// then erased differently, and stepping up onto the line the user is typing on
/// is the intrusion, whatever is written next.
const REPAINT_MEASURE: &str = "\x1b[s\x1b[1A";

/// The bytes that take a live row back (`PlainSurface::withdraw_row_above`).
///
/// No cursor save/restore pair, unlike the repaint: the cursor is meant to end
/// up on the row it cleared, so whatever prints next lands where the row was
/// and the scrollback closes over it without a gap (BR-5).
const WITHDRAW: &str = "\x1b[1A\r\x1b[K";

/// Every activity row `transcript` carries, in the order they were painted.
///
/// A row is found by its opening glyph and read to the first byte that is not
/// part of it — an escape (the trailing `\x1b[0m` of a `line()` draw, or the
/// `\x1b[u` of a repaint) or the end of the line. So this reads the row's
/// *text*, whichever verb drew it, which is what lets one helper serve the
/// first-draw, repaint and stall legs alike.
fn activity_rows(transcript: &str) -> Vec<String> {
    let mut rows = Vec::new();
    for (at, c) in transcript.char_indices() {
        if !ROW_GLYPHS.contains(&c) {
            continue;
        }
        let tail = &transcript[at..];
        let end = tail.find(['\x1b', '\r', '\n']).unwrap_or(tail.len());
        rows.push(tail[..end].to_owned());
    }
    rows
}

/// The clocks a row is showing — everything from its first ` · ` on.
///
/// `⠋ waiting on local local (build) · 2s · turn 2s` yields `2s · turn 2s`.
/// Split off the sentence rather than parsed into numbers: what the legs below
/// assert is that the figures *changed*, and comparing the clause as written
/// says that without a second parser deciding what a second is.
fn clocks(row: &str) -> &str {
    row.split_once(" · ").map_or("", |(_, clocks)| clocks)
}

/// The distinct clock clauses in `rows`, in the order they first appeared.
fn distinct_clocks(rows: &[String]) -> Vec<&str> {
    let mut seen: Vec<&str> = Vec::new();
    for row in rows {
        let clocks = clocks(row);
        if !seen.contains(&clocks) {
            seen.push(clocks);
        }
    }
    seen
}

/// Assert that replaying `transcript` through a terminal leaves no activity row
/// on screen (BR-5, BR-12).
///
/// This is the assertion that cannot be made on the stream. A withdrawn row is
/// still in the bytes — `\x1b[1A\r\x1b[K` tells a terminal to draw over
/// characters, it does not unsend them — so a `contains` finds the whole
/// animation whether or not the last frame was taken back. Replaying is what
/// tells the two apart, and `common::rendered_screen` is the replay.
fn assert_no_row_on_screen(transcript: &str, whose: &str) {
    let screen = common::rendered_screen(transcript);
    let left_behind: Vec<&String> = screen
        .iter()
        .filter(|row| row.chars().any(|c| ROW_GLYPHS.contains(&c)))
        .collect();
    assert!(
        left_behind.is_empty(),
        "{whose}: the turn left an activity row on screen — BR-5 has the row \
         withdrawn rather than blanked, so the scrollback after a turn is what \
         it would have been without the feature. Rows still showing:\n\
         {left_behind:#?}\nWhole screen:\n{screen:#?}"
    );
}

/// **REQ-621 AC-1 / BR-1 / BR-3 — the row is on screen before the first reply
/// byte, it counts, and the arriving text takes it back.**
///
/// The silent lead-in is the stretch this REQ exists for: between Enter and the
/// first streamed byte the old session showed an unmoving cursor, and a user
/// cannot tell that from a hang. `@delay-ms 3000` makes that stretch three
/// seconds long on purpose, which is long enough for the counter to pass
/// through several values and for the assertions to be about ordering rather
/// than about timing.
///
/// Four claims, and the third is the one with teeth:
///
/// 1. a row is drawn **before** the reply's first byte — positionally, not by a
///    pair of `contains` that would both be true from the moment the second
///    arrived;
/// 2. its sentence names what the daemon reported: `waiting on`, the provider,
///    and the model out of `route_decided` (BR-2 — see the section header on why
///    the `preparing` frame is not observable here);
/// 3. the counter shows **at least two distinct** values and the row is
///    repainted in place while it does, so the animation is running off the
///    client's own clock during a stretch in which the daemon says nothing
///    (BR-4);
/// 4. nothing activity-shaped appears from the first reply byte to the end of
///    the turn, and the reply's first byte lands on the withdrawn row's own line
///    (BR-1: the arriving text *is* the liveness signal, so the row steps
///    aside).
///
/// # What breaks this test
///
/// The mutation below was **applied to `client.rs`, built, and observed
/// failing**, not reasoned about (AC-11, LESSON-441):
///
/// | Mutation | Fails |
/// |---|---|
/// | the pump's `Wake::Tick` arm advances `row.tick` but does not call `paint_row` | **all five** of REQ-621's pty legs — 5 red of 28 here — and **none** of `cli_e2e`'s 85 (2026-09-10) |
///
/// This leg is the one that fails on the *property*. Claim (3) is what catches
/// it: a pump that never paints on a tick leaves only the frames the *message*
/// arm drew, and the daemon says nothing for three seconds, so the clock never
/// turns over. When the mutation was run this claim was still a single
/// evaluation after the reply and it failed immediately, reporting "3 rows, all
/// reading `0s · turn 0s`"; it is now the polled condition above, which under
/// the same mutation exhausts its window and quotes the same unmoving rows.
/// Claims (1), (2) and (4) stay green under the mutation, and that is right:
/// the row is still drawn and still withdrawn by the message path, whose
/// discipline this mutation does not touch. The other four legs redden on their
/// non-vacuity guards ("the row never animated, so this leg is not asking …"),
/// which is what those guards are for.
///
/// The piped suite staying green is not a gap in it. `cli_e2e` cannot reach the
/// tick arm at all — with no live rows the pump keeps its blocking receive — so
/// a mutation of that arm is invisible to it by construction, which is BR-6's
/// own claim arriving as evidence rather than as an argument.
#[test]
fn the_row_appears_before_the_first_byte_and_withdraws_when_text_streams() {
    const REPLY: &str = "The delayed reply arrives at last.";
    let mut session = RenderedSession::open(100, &[&format!("@delay-ms 3000\n{REPLY}")], &[]);
    session.type_line("hold the turn open");

    // (3), polled **before** the turn's own marker is awaited, so the condition
    // the leg waits on is the condition it asserts (LESSON-450). Evaluated once
    // after the reply had landed, this claim was a snapshot of a window that
    // had already closed: a run in which the clock had not yet turned over when
    // the reply arrived reported a flake rather than waiting the extra frame it
    // needed. The window is the lead-in either way — before the reply arrives
    // the whole transcript is the lead-in, and after it the split is the same
    // one the assertions below read.
    assert!(
        session.wait_until(|seen| {
            let lead_in = seen.split_once(REPLY).map_or(seen, |(lead_in, _)| lead_in);
            distinct_clocks(&activity_rows(lead_in)).len() >= 2
        }),
        "the row's counter never moved across a three-second silence. Either \
         the pump is not waking on its own clock (BR-4) or the frame is not \
         reading the tick it is handed (BR-3); rows: {:#?}\ntranscript:\n{}",
        activity_rows(&session.snapshot()),
        session.snapshot()
    );
    assert!(
        session.wait_for(REPLY),
        "the scripted reply never reached the screen, so the turn under test \
         never ran; transcript:\n{}",
        session.snapshot()
    );
    // The turn's own end: the entry frame is redrawn after it, and claim (4) is
    // about the whole of the turn rather than about the reply's arrival.
    assert!(
        session.wait_for("ready (freeform)")
            && session.wait_until(|seen| seen.ends_with("\x1b[0m ")),
        "the session never got back to its entry prompt; transcript:\n{}",
        session.snapshot()
    );

    let seen = session.snapshot();
    let reply_at = seen
        .find(REPLY)
        .unwrap_or_else(|| panic!("the reply is not in the transcript:\n{seen}"));
    let (lead_in, streaming) = seen.split_at(reply_at);
    let rows = activity_rows(lead_in);

    // (1) A row, before the first byte of the reply.
    assert!(
        !rows.is_empty(),
        "no activity row was drawn in the three seconds before the first reply \
         byte — the whole of BR-1's silent lead-in showed nothing, which is the \
         defect this REQ exists to remove; transcript:\n{seen}"
    );

    // (2) The sentence is the daemon's, in every part of it. `local` twice is
    // this fixture's own reading of `route_decided` — the provider id and the
    // model the local tier reports are both `local`, and `route_clause` prints
    // each only when the event carries one — and the parenthesised band is the
    // tier that decision went through.
    //
    // Two bands appear, and that is the rule working rather than a wobble: the
    // classifier's own reflex route is published on this session before the
    // turn's, so the row names `(reflex)` for as long as that is the last thing
    // the daemon said and moves to `(build)` when the turn's route arrives. A
    // row that named the turn's tier before the daemon had chosen it would be
    // exactly the invention BR-2 forbids, so the assertion is "whatever was
    // last reported", with the turn's own route pinned as the one it ends on.
    for row in &rows {
        assert!(
            row.contains("waiting on local local ("),
            "every lead-in row must name the phase and the route the daemon \
             reported — the provider and model out of `route_decided`, never a \
             name the client composed (BR-2); row: {row:?}\ntranscript:\n{seen}"
        );
    }
    assert!(
        rows.last().is_some_and(|row| row.contains("(build)")),
        "the last row before the reply must name the tier the *turn* resolved \
         through, which is the last route the daemon reported before it started \
         streaming; rows: {rows:#?}\ntranscript:\n{seen}"
    );

    // (3), second half: the row was repainted **in place** while the clock
    // advanced. The advance itself was the polled condition above.
    assert!(
        lead_in.matches(REPAINT_OPEN).count() >= 2,
        "the row was drawn but never repainted in place, so each frame appended \
         another line instead of replacing the last — which is the scrollback \
         BR-5 forbids. {} repaints in the lead-in; transcript:\n{seen}",
        lead_in.matches(REPAINT_OPEN).count()
    );

    // (4) The reply's first byte lands on the row's own line, and nothing
    // activity-shaped follows it for the rest of the turn.
    assert!(
        lead_in.ends_with(WITHDRAW),
        "the reply did not begin on the row's own line: the bytes immediately \
         before it should be the withdraw, so the row leaves no gap and no \
         residue behind it (BR-5). Instead: {:?}",
        &lead_in[lead_in.len().saturating_sub(24)..]
    );
    assert!(
        activity_rows(streaming).is_empty(),
        "an activity row was painted while the reply was streaming, or after the \
         turn ended — during `streaming` the arriving text is the liveness \
         signal and the row is withdrawn (BR-1); rows: {:#?}\ntranscript:\n{seen}",
        activity_rows(streaming)
    );
    assert_no_row_on_screen(&seen, "the silent lead-in");
}

/// **REQ-621 AC-2 / AC-5 / BR-3 — a running tool shows its title, its elapsed
/// seconds and its cost so far, beneath its own durable line.**
///
/// The second silent stretch: a forty-second test suite used to be one
/// `[running]` line and then a cursor that did not move. Here it is a scripted
/// `shell` call running `sleep 3`, which is a **real** tool rather than a seam
/// — the row's sentence has to come from the title the daemon composed for
/// `tool_call`, and the phase has to be driven by the daemon's own publisher on
/// both edges (AC-5, LESSON-544).
///
/// Four claims:
///
/// 1. the durable `[running]` line prints where it always did and the row
///    appears **beneath** it, not in place of it (BR-10: no second renderer);
/// 2. the row says `running` and the daemon's title, and its counter advances
///    while the tool runs;
/// 3. on completion the `[done]` line prints where the row was and the row comes
///    back in `awaiting_model` — the model is composing its next step, which is
///    the phase after a tool result by construction;
/// 4. the cost clause is the daemon's own figure. See the section header: this
///    fixture's calls are unpriced by design, so the daemon reports `$0.000000`
///    and the row correctly carries no cost clause. The oracle is `teton cost`
///    rather than a literal, so a build that priced local calls fails here
///    instead of passing vacuously.
///
/// # The 2026-09-10 flake, and why the sleep is still three seconds
///
/// This leg failed once in a full run of the suite and passed in isolation and
/// on every full run after it. The failing output was not kept; of its claims,
/// (2) is the only one time can move, and (2) is what a starved event
/// forwarder produces. The daemon dispatched `Tool::run` **inline** on its
/// async worker, so a `shell` call held that worker for as long as `sleep 3`
/// ran — and the `tool_call` publish immediately before it had woken this
/// connection's forwarder into that same worker's LIFO slot, which no other
/// worker steals (tokio-rs/tokio#4941). When the scheduling fell that way, the
/// client received `tool_call` and `tool_call_update` together as the tool
/// finished: one row, painted by the `tool_call` arm and withdrawn by the next
/// message, reading one clause. That is BUG-226, and it is the silent stretch
/// this leg exists to catch — the fix moved the dispatch behind
/// `block_in_place_if_multithread` (observed on tokio 1.53 with a standalone
/// probe: the forwarder waited out the whole block in 8 of 8 trials inline,
/// and 0 of 8 through the helper).
///
/// Claim (2) is polled rather than read once, which is right for the reason
/// its comment gives — but polling cannot reopen a window that closed with
/// the tool. Rows are painted only while the tool runs, and a starved
/// forwarder leaves one of them however long the poll waits, so under BUG-226
/// the poll exhausts its window and quotes that one row. Neither the tool's
/// length nor the `>= 2` claim changed: a longer sleep or a looser count would
/// hide a starved forwarder, which compresses the window to nothing however
/// long the tool runs, and `>= 2` over three seconds is ~25 paints of margin
/// on the client's side. This leg catches the starvation only when tokio's
/// scheduling hands the forwarder the slot, so the deterministic half is
/// `tetond`'s `the_turn_path_takes_no_blocking_wait`, which holds the dispatch
/// inside the helper by source. Claim (2)'s message lists the rows in the
/// tool's window, so a one-row window and a frozen counter across many rows
/// are told apart on the spot.
#[test]
fn a_running_tool_shows_its_title_elapsed_and_cost_so_far_beneath_its_running_line() {
    const REPLY: &str = "The tool turn is done.";
    const RUNNING: &str = " - shell: sleep 3 [running]";
    const DONE: &str = " - shell: sleep 3 [done]";
    let mut session = RenderedSession::open_with_config(
        100,
        &[
            r#"{"tool": "shell", "arguments": {"command": "sleep 3"}}"#,
            REPLY,
        ],
        &[],
        // `full`, so nothing stands between the turn and the tool. At the
        // default level a `shell` call is a question, the row steps aside for
        // it (BR-1), and this leg would be about the permission prompt instead
        // — which is `permission_levels_change_what_a_session_asks_about`'s
        // subject, over a pipe, where it belongs.
        &local_tier_config(FULL_AND_NO_CONTEXT_OFFER),
    );
    session.type_line("run the slow tool");

    // (2)'s counter claim, polled **before** the turn's own marker is awaited,
    // so the condition the leg waits on is the condition it asserts
    // (LESSON-450). Evaluated once after the reply had landed this flaked: the
    // window it read was already closed, so a run whose clock had not turned
    // over inside it failed instead of waiting the one frame it needed. The
    // window is the tool's own — from its `[running]` line to its `[done]`, or
    // to the end of what has arrived while it is still running.
    let tool_window = |seen: &str| -> Vec<String> {
        let Some(from) = seen.find(RUNNING) else {
            return Vec::new();
        };
        let window = &seen[from..];
        let window = window.split_once(DONE).map_or(window, |(open, _)| open);
        activity_rows(window)
    };
    assert!(
        session.wait_until(|seen| distinct_clocks(&tool_window(seen)).len() >= 2),
        "the row's counter never moved while a three-second tool ran — a tool's \
         elapsed counter is the only signal there is while it runs, because the \
         daemon publishes nothing (BR-4). Two readings, told apart by the rows \
         in the tool's window: one row, or two, means `[running]` and `[done]` \
         reached the client together, because the daemon parked the worker its \
         event forwarder was queued on (BUG-226); many rows all reading alike \
         mean the counter is not on the client's own clock. Rows: {:#?}\n\
         transcript:\n{}",
        tool_window(&session.snapshot()),
        session.snapshot()
    );
    assert!(
        session.wait_for(REPLY),
        "the tool turn never finished; transcript:\n{}",
        session.snapshot()
    );

    let seen = session.snapshot();
    let running_at = seen
        .find(RUNNING)
        .unwrap_or_else(|| panic!("the tool never announced itself:\n{seen}"));
    let done_at = seen
        .find(DONE)
        .unwrap_or_else(|| panic!("the tool never reported completion:\n{seen}"));
    let reply_at = seen
        .find(REPLY)
        .unwrap_or_else(|| panic!("the reply is not in the transcript:\n{seen}"));

    // (1) The row is beneath the `[running]` line, and the line is untouched.
    assert!(
        running_at < done_at && done_at < reply_at,
        "the tool's two durable lines and the reply must land in that order — \
         {running_at}, {done_at}, {reply_at}; transcript:\n{seen}"
    );
    let while_running = &seen[running_at..done_at];
    let rows = activity_rows(while_running);
    assert!(
        !rows.is_empty(),
        "a tool ran for three seconds and the row showed nothing beneath its \
         `[running]` line, which is the second silent stretch this REQ is about \
         (BR-1); transcript:\n{seen}"
    );

    // (2) The daemon's title, and a counter that moves.
    for row in &rows {
        assert!(
            row.starts_with(|c| ROW_GLYPHS.contains(&c)) && row.contains("running shell: sleep 3"),
            "the row must say what is running, in the title the daemon composed \
             for `tool_call` rather than in a second reading of the tool's \
             arguments (BR-2, LESSON-456); row: {row:?}\ntranscript:\n{seen}"
        );
    }

    // (3) `[done]` lands where the row was, and the row comes back in
    // `awaiting_model` — the model composing its next step.
    assert!(
        seen[..done_at].ends_with(WITHDRAW),
        "the `[done]` line did not land on the row's own line: a durable line \
         prints where the row was and the row returns beneath it (ADR-621-3); \
         before it: {:?}",
        &seen[done_at.saturating_sub(24)..done_at]
    );
    let after_done = activity_rows(&seen[done_at..reply_at]);
    // `any`, not `all`, and the emptiness checked first. The claim is that the
    // row **moves to** the model phase once the tool result is in, and the
    // post-tool window is a stretch of the turn in which the daemon may
    // legitimately say something else: any notice-shaped event landing in it —
    // a route re-decided, a cost row, a compaction — changes the sentence of
    // the frames after it, and an `all` would read that as the transition
    // having failed. So the assertion is on the transition, and the frames
    // either side of an event are the daemon's to name (BR-2).
    //
    // `all` was also *vacuously true* over an empty window, which is the state
    // that matters here — no row came back at all — so the emptiness check
    // goes first and says that in its own words rather than leaving it to a
    // quantifier.
    assert!(
        !after_done.is_empty(),
        "the row did not come back after the tool finished, so the stretch \
         while the model composes its next step is silent again (BR-1); \
         transcript:\n{seen}"
    );
    assert!(
        after_done
            .iter()
            .any(|row| row.contains("waiting on local")),
        "after a tool result the row must move to the model phase — there is no \
         `model request issued` event and by construction the next thing the \
         daemon can send is a chunk, a tool call or the result (ADR-621-2); \
         rows: {after_done:#?}\ntranscript:\n{seen}"
    );

    // (4) The cost clause is the daemon's own figure, whatever that figure is.
    let report = session.cost_report();
    let total = report
        .lines()
        .find_map(|line| line.strip_prefix("total: "))
        .and_then(|tail| tail.split_whitespace().next())
        .unwrap_or_else(|| panic!("`teton cost` printed no total:\n{report}"))
        .to_owned();
    let with_cost: Vec<&String> = rows.iter().filter(|row| row.contains('$')).collect();
    if total == "$0.000000" {
        assert!(
            with_cost.is_empty(),
            "the daemon recorded no spend for this turn ({total} — this \
             fixture's model is unpriced, REQ-564 BR-9) and BR-3 shows the \
             figure only once it is non-zero, so no row may carry one: \
             {with_cost:#?}\ncost report:\n{report}"
        );
    } else {
        assert!(
            !with_cost.is_empty() && with_cost.iter().all(|row| row.contains(&total)),
            "the daemon recorded {total} for this turn, so every row drawn \
             after it must carry that exact amount — the row and the cost meter \
             read one accumulator and cannot print two figures (BR-3): \
             {with_cost:#?}\ncost report:\n{report}"
        );
    }

    assert_no_row_on_screen(&seen, "the running tool");
}

/// **REQ-621 AC-6 / BR-11 — a daemon that has gone quiet is named as quiet; a
/// tool that is simply slow is not.**
///
/// Both legs are here because they are one rule. A wedged daemon has to look
/// different from a slow model, and the way the first draft of BR-11 said that
/// — replace the phase with `stalled` — would have relabelled every
/// forty-second test suite as a stall at fifteen seconds, which is the noise
/// that makes a real one easy to miss (ADR-621-5, LESSON-628). So the annotation
/// is added to the phase the daemon **last reported** and `tool_running` is
/// exempt, and a leg that asserted only the first half would pass against the
/// draft this REQ corrected.
///
/// Leg A — `@delay-ms 16500`, 1.5 s past the quiet bound: the row keeps saying
/// `waiting on`, appends how long the daemon has been silent, stops its spinner
/// on a constant glyph across every stalled frame, and drops the annotation
/// the moment real text arrives.
///
/// Leg B — a `shell` call running `sleep 16`, a second past the same bound: the
/// row counts and says nothing about silence at any point, because the daemon
/// publishes nothing while a tool runs and the tool's own elapsed counter is
/// the honest signal.
///
/// The leg costs about thirty-three seconds of wall clock, once, in a suite
/// whose window is sixty per wait. That is the accepted price of a bound that
/// can only be crossed by waiting (ADR-621-6): every assertion is still on
/// state reached, never on a sleep (LESSON-450).
#[test]
fn a_silent_daemon_earns_the_stall_annotation_and_a_long_tool_does_not() {
    // ---- Leg A: the daemon goes quiet before the first byte ----
    const STALLED_REPLY: &str = "The stalled reply arrives.";
    let mut quiet =
        RenderedSession::open(100, &[&format!("@delay-ms 16500\n{STALLED_REPLY}")], &[]);
    quiet.type_line("go quiet on me");

    assert!(
        quiet.wait_until(|seen| seen.contains("no word from the daemon for")),
        "the daemon said nothing for over fifteen seconds and the row never \
         said so — a wedged daemon must not look like a slow model (BR-11); \
         transcript:\n{}",
        quiet.snapshot()
    );
    assert!(
        quiet.wait_for(STALLED_REPLY),
        "the delayed reply never arrived, so the annotation was never cleared \
         by a real event; transcript:\n{}",
        quiet.snapshot()
    );

    let seen = quiet.snapshot();
    let reply_at = seen
        .find(STALLED_REPLY)
        .unwrap_or_else(|| panic!("the reply is not in the transcript:\n{seen}"));
    let stalled: Vec<String> = activity_rows(&seen[..reply_at])
        .into_iter()
        .filter(|row| row.contains("no word from the daemon for"))
        .collect();

    assert!(
        stalled.len() >= 2,
        "fewer than two stalled frames, so nothing here can say whether the \
         spinner stopped: {stalled:#?}\ntranscript:\n{seen}"
    );
    for row in &stalled {
        // The phase, still. This is ADR-621-5's whole correction: the row says
        // what the daemon last reported *and* how long ago, never `stalled`
        // instead of the phase.
        assert!(
            row.contains("waiting on local"),
            "a stalled row must keep naming the phase the daemon last reported \
             and merely add the silence to it (BR-11, ADR-621-5); row: \
             {row:?}\ntranscript:\n{seen}"
        );
        assert!(
            row.contains("no word from the daemon for 1"),
            "the annotation must say how long the daemon has been silent, and \
             past a fifteen-second bound that figure opens with a `1`; row: \
             {row:?}\ntranscript:\n{seen}"
        );
        assert!(
            row.starts_with(STALLED_GLYPH),
            "a stalled row's spinner must be stopped on the one full-cell glyph \
             — a stopped row has to read as stopped rather than as a spinner \
             between frames (BR-11); row: {row:?}\ntranscript:\n{seen}"
        );
    }
    // And the clock kept moving while the spinner did not: a stopped row is
    // still a counting row, which is what tells "the daemon is quiet" from "the
    // client is wedged too".
    assert!(
        distinct_clocks(&stalled).len() >= 2,
        "the stalled row stopped counting as well as spinning: {:?} — the \
         annotation keeps counting until a real event arrives (BR-11); \
         transcript:\n{seen}",
        distinct_clocks(&stalled)
    );
    // Cleared by the event, not by a timer.
    assert!(
        !seen[reply_at..].contains("no word from the daemon"),
        "the stall annotation outlived the event that refuted it; \
         transcript:\n{seen}"
    );
    assert_no_row_on_screen(&seen, "the stalled turn");
    drop(quiet);

    // ---- Leg B: a tool runs a second past the same bound ----
    const TOOL_REPLY: &str = "The long tool turn is done.";
    let mut slow = RenderedSession::open_with_config(
        100,
        &[
            r#"{"tool": "shell", "arguments": {"command": "sleep 16"}}"#,
            TOOL_REPLY,
        ],
        &[],
        &local_tier_config(FULL_AND_NO_CONTEXT_OFFER),
    );
    slow.type_line("run the long tool");

    assert!(
        slow.wait_for(TOOL_REPLY),
        "the long tool turn never finished; transcript:\n{}",
        slow.snapshot()
    );

    let seen = slow.snapshot();
    let rows = activity_rows(&seen);
    let running: Vec<&String> = rows
        .iter()
        .filter(|row| row.contains("running shell: sleep 16"))
        .collect();
    // Non-vacuity first: the negative claim below means nothing unless the row
    // was up for the whole of a window it could have annotated.
    assert!(
        running
            .iter()
            .any(|row| clocks(row).starts_with("16s") || clocks(row).starts_with("15s")),
        "the tool row never reached the quiet bound, so this leg never asked \
         the question it exists to ask: {running:#?}\ntranscript:\n{seen}"
    );
    assert!(
        !seen.contains("no word from the daemon"),
        "a tool that ran a second past the quiet bound was reported as a stall. \
         `tool_running` is exempt: the daemon publishes nothing while a tool \
         runs, so silence there is the expected state, and annotating it is the \
         noise that makes a real stall easy to miss (BR-11, ADR-621-5); \
         transcript:\n{seen}"
    );
    assert!(
        !running.iter().any(|row| row.starts_with(STALLED_GLYPH)),
        "the long tool's spinner stopped, which is the stall reading arriving \
         by the other half of the same rule: {running:#?}\ntranscript:\n{seen}"
    );
    assert_no_row_on_screen(&seen, "the long tool");
}

/// **REQ-621 AC-7 / BR-5 / BR-12 — every way a turn can end takes the row with
/// it.**
///
/// Three exits, and none of them depends on the daemon sending a final event.
/// That independence is the point: BR-12 is a rule about the paths nobody
/// anticipated, and the close-out is on the client's own control flow — the
/// `P::ENDS_TURN` branch of `Connection::call`, which runs on the `Ok`, on the
/// RPC error, and on every transport `?` inside the pump (ADR-621-4).
///
/// * **A normal result.** The turn is held open long enough for the row to be
///   animating when the reply arrives, so the withdraw has something to withdraw.
/// * **An RPC error.** A tier bound to a provider nothing is listening for: the
///   route is decided and published — so the row is on screen — and then the
///   call fails and `session/prompt` answers with an error. `prompt failed:`
///   lands where the row was.
/// * **The daemon killed mid-turn.** Killed while the row is repainting, which
///   is the disconnect path: `recv_timeout` reports `Disconnected`, the pump
///   returns the transport error, and the close-out still runs.
///
/// Each leg asserts on the **screen**, not on the transcript. A withdrawn row is
/// still in the byte stream — the escape tells a terminal to draw over
/// characters, it does not unsend them — so a `contains` over a transcript finds
/// the whole animation whichever way the turn ended, and would pass against a
/// build that never withdrew anything. `common::rendered_screen` replays the
/// bytes and the claim is made on what is left (see `assert_no_row_on_screen`).
#[test]
fn every_exit_erases_the_row() {
    // ---- Leg 1: a normal result ----
    const REPLY: &str = "The ordinary turn ended.";
    let mut ok = RenderedSession::open(100, &[&format!("@delay-ms 1500\n{REPLY}")], &[]);
    ok.type_line("end normally");
    assert!(
        ok.wait_for(REPLY),
        "the ordinary turn never ended; transcript:\n{}",
        ok.snapshot()
    );
    assert!(
        ok.wait_until(|seen| seen.ends_with("\x1b[0m ")),
        "the session never got back to its entry prompt; transcript:\n{}",
        ok.snapshot()
    );
    let seen = ok.snapshot();
    // Non-vacuity: there was a row to erase. Without this the leg would pass
    // against a build that drew nothing at all.
    assert!(
        seen.contains(REPAINT_OPEN),
        "the row never animated, so this leg is not asking whether the normal \
         exit erased one; transcript:\n{seen}"
    );
    assert_no_row_on_screen(&seen, "a normal result");
    drop(ok);

    // ---- Leg 2: an RPC error ----
    let port = closed_port();
    let mut failed = RenderedSession::open_with_config(
        100,
        &["never reached"],
        &[],
        // `reflex` stays local so the classifier duty is served; the turn's own
        // tiers point at a port nothing is listening on. Every tier names
        // exactly one provider — the daemon refuses to start on a duplicate
        // binding — so this is a whole config rather than an appended table.
        &format!(
            "[[providers]]\nid = \"local\"\nkind = \"local\"\n\n\
             [[providers]]\nid = \"gone\"\nkind = \"openai-compatible\"\n\
             endpoint = \"http://127.0.0.1:{port}/v1/chat/completions\"\n\
             model = \"gone-model\"\n\n\
             [[tiers]]\ntier = \"reflex\"\nprovider_id = \"local\"\n\n\
             [[tiers]]\ntier = \"scan\"\nprovider_id = \"gone\"\n\n\
             [[tiers]]\ntier = \"build\"\nprovider_id = \"gone\"\n\n\
             [[tiers]]\ntier = \"think\"\nprovider_id = \"gone\"\n\n"
        ),
    );
    failed.type_line("route me nowhere");
    // `main::render_turn_failure`'s line, with the `error: ` prefix
    // `LineKind::Error` gives it: the whole row is the marker, because the claim
    // below is that the row was taken back *immediately* before this line's
    // first byte.
    const FAILURE: &str = "error: prompt failed:";
    assert!(
        failed.wait_for(FAILURE),
        "the turn never failed, so this leg never reached the RPC-error exit; \
         transcript:\n{}",
        failed.snapshot()
    );
    let seen = failed.snapshot();
    let failure_at = seen
        .find(FAILURE)
        .unwrap_or_else(|| panic!("the failure line is not in the transcript:\n{seen}"));
    // Non-vacuity, and it is a different shape from leg 1's: a dial to a closed
    // port fails in milliseconds, so the row here is drawn from the *message*
    // arm on `route_decided` and never ticks. What matters is that it was on
    // screen when the error arrived.
    let before = activity_rows(&seen[..failure_at]);
    assert!(
        before
            .last()
            .is_some_and(|row| row.contains("waiting on gone gone-model")),
        "no row was on screen when the turn failed, so this leg is not asking \
         whether the error path erased one — and the last one there must name \
         the provider and model `route_decided` reported for the turn, the \
         unreachable one: {before:#?}\ntranscript:\n{seen}"
    );
    assert!(
        seen[..failure_at].ends_with(WITHDRAW),
        "the failure line did not land on the row's own line; before it: {:?}",
        &seen[failure_at.saturating_sub(24)..failure_at]
    );
    assert_no_row_on_screen(&seen, "an RPC error");
    drop(failed);

    // ---- Leg 3: the daemon killed mid-turn ----
    let mut killed = RenderedSession::open(100, &["@delay-ms 20000\nnever arrives"], &[]);
    killed.type_line("hold the turn open");
    // Killed on a state the row itself reached — three repaints, so the row is
    // demonstrably animating — rather than after an interval chosen by hand
    // (LESSON-450).
    assert!(
        killed.wait_until(|seen| seen.matches(REPAINT_OPEN).count() >= 3),
        "the row never animated, so there is nothing for the disconnect to \
         erase; transcript:\n{}",
        killed.snapshot()
    );
    killed.kill_daemon();
    // The whole row `main` writes when `call` returns through its own transport
    // `?`, opening prefix included, for the reason the failure marker above
    // carries one.
    const CLOSED: &str = "teton: connection to the daemon closed";
    assert!(
        killed.wait_for(CLOSED),
        "the client never noticed the daemon was gone; transcript:\n{}",
        killed.snapshot()
    );
    let seen = killed.snapshot();
    let closed_at = seen
        .find(CLOSED)
        .unwrap_or_else(|| panic!("the disconnect line is not in the transcript:\n{seen}"));
    assert!(
        seen[..closed_at].ends_with(WITHDRAW),
        "the disconnect line did not land on the row's own line, so a killed \
         daemon leaves the row above the message that explains it; before it: \
         {:?}",
        &seen[closed_at.saturating_sub(24)..closed_at]
    );
    assert_no_row_on_screen(&seen, "a daemon killed mid-turn");
}

/// **REQ-621 AC-10 / BR-9 — bytes typed while the row is animating arrive
/// intact, and no repaint blanks them.**
///
/// The row is redrawn eight times a second in the line above the cursor and the
/// terminal echoes typed characters at the cursor, so this is a claim about two
/// writers sharing a screen. BR-9 scopes it precisely: what must survive is
/// **delivery** — the kernel's line buffer is untouched by anything the row
/// does — and what must not happen is a repaint reaching the line the user is
/// typing on.
///
/// **Intactness is asserted through the client, not through a model.** The
/// carried line is a slash command the session has no row for, so the client
/// quotes it straight back — ``unknown command: `/the-carried-line` `` — which
/// makes the claim about bytes the client read out of the line buffer. A
/// prompt would have proved only that *some* line arrived.
///
/// Two halves, because the return key is what separates them, and they make
/// **opposite** claims about the same counter:
///
/// 1. **Characters during the animation, return after the turn.** The
///    type-ahead case, and the one where every claim holds at once: the echo
///    survives verbatim beside an animation that goes on running over it — the
///    repaints keep coming, because characters alone submit nothing — the line
///    is delivered intact, and the turn still leaves no row on screen.
/// 2. **The whole line, return included, during the animation.** Delivery still
///    holds, and the row *gives up*: from the echoed newline on, not one
///    repaint byte reaches the terminal, and the echoed line is intact on the
///    replayed screen.
///
/// # Half 2 is a claim about the row stopping, and the earlier wording was wrong
///
/// The first pass of this leg said the echoed characters were merely "visually
/// displaced" and asserted delivery alone. That understated it. Once the
/// terminal has echoed a newline the cursor has dropped a row under bookkeeping
/// a canonical-mode client cannot see — no read of ours, no `\n` of ours,
/// nothing on the wire — so the row is *two* rows above the cursor and every
/// offset it owns is short by one. The next repaint would therefore have
/// rewritten the line holding the characters the user just typed, which is the
/// blanking BR-9 forbids in as many words, and the closing withdraw would have
/// erased that line outright.
///
/// So the pump abandons the row the moment a submitted line is waiting on
/// stdin (`RowState::abandon`, from the check at the top of the pump loop, which
/// runs before every read and write of the row): no further repaint, no
/// withdraw, and the last frame stays where it was. That frame is a bounded,
/// recorded exception to BR-5 — one row per turn, in a case the user created by
/// typing over the row — and it is the only outcome here that damages nothing
/// the user typed.
///
/// Half 2 asserts all three parts of that: delivery, **no repaint attempted**
/// past the newline, and the echo intact on the screen. The last one is a claim
/// the byte stream cannot make — a repaint that erased the echo would leave the
/// echo *and* the erasure in the transcript — which is why it is made on
/// `common::rendered_screen`'s replay. `assert_no_row_on_screen` is deliberately
/// **not** applied to half 2: the abandoned frame is the recorded exception
/// above, and asserting its absence would be asserting the damage the exception
/// exists to avoid.
///
/// # What breaks half 2
///
/// The two halves falsify each other, which is what makes the negative claim
/// worth making. Half 1 counts [`REPAINT_MEASURE`] **after** its echo and
/// requires two or more; half 2 counts it after its echoed newline and requires
/// zero. Both counters run over the same stretch of the same script — the
/// difference between them is one `\r` — so a build that abandoned rows it
/// should have kept reddens half 1, and a build that repainted over a submitted
/// line reddens half 2. Half 2 also carries the non-vacuity: at least two
/// repaints *before* the newline, without which its zero would be the zero of a
/// row that never animated.
///
/// The boundary itself was **mutated and observed failing**:
///
/// | Mutation | Fails |
/// |---|---|
/// | half 2's counter reads the window *before* the newline instead of after it | the zero-repaint claim, `left: 2` against `right: 0` |
///
/// So the newline is where the repaints stop, to the byte: two of them arrive
/// in the stretch the user was typing over and none in the stretch after they
/// pressed Enter, over one script and one window boundary apart.
#[test]
fn typed_bytes_survive_the_animation() {
    const CARRIED: &str = "/the-carried-line";
    const QUOTED: &str = "unknown command: `/the-carried-line`";
    const FIRST: &str = "The first reply lands.";

    // ---- Half 1: characters during the animation, return after the turn ----
    let mut ahead = RenderedSession::open(100, &[&format!("@delay-ms 3000\n{FIRST}")], &[]);
    ahead.type_line("hold the turn open");
    // Typed on a state the row reached, not after an interval: two repaints
    // means the animation is demonstrably running when these bytes arrive
    // (LESSON-450).
    assert!(
        ahead.wait_until(|seen| seen.matches(REPAINT_OPEN).count() >= 2),
        "the row never animated, so nothing here would be typed *during* an \
         animation; transcript:\n{}",
        ahead.snapshot()
    );
    ahead.type_raw(CARRIED);

    assert!(
        ahead.wait_for(FIRST),
        "the first turn never finished; transcript:\n{}",
        ahead.snapshot()
    );
    let seen = ahead.snapshot();
    let first_at = seen
        .find(FIRST)
        .unwrap_or_else(|| panic!("the first reply is not in the transcript:\n{seen}"));
    // Not blanked: the echo is in the stream in one piece, and it is there
    // *before* the turn ended — so it is an echo from during the animation
    // rather than the entry prompt's own.
    let echo_at = seen[..first_at].find(CARRIED).unwrap_or_else(|| {
        panic!(
            "the characters typed during the animation were never echoed, or a \
             repaint blanked them as they arrived — a repaint claims the row \
             *above* the cursor and must leave the line being typed on alone \
             (BR-9, ADR-621-3); transcript:\n{seen}"
        )
    });
    // And immediately after a cursor restore, which is where a terminal puts an
    // echo while a repaint owns the row above: proof the two writers did not
    // collide, rather than proof they were both present.
    assert!(
        seen[..echo_at].ends_with("\x1b[u"),
        "the echo did not land at the restored cursor; before it: {:?}",
        &seen[echo_at.saturating_sub(16)..echo_at]
    );
    // And the animation went on running over them. Characters alone submit
    // nothing — stdin is in canonical mode, so the kernel makes no byte
    // readable until Enter — so the pump's abandon check correctly does not
    // fire and BR-9's "a repaint never blanks characters the user has echoed"
    // has to hold with repaints still arriving. This is half 2's counterpart:
    // the same counter over the same stretch of the same script, and the
    // opposite answer.
    assert!(
        seen[echo_at..].matches(REPAINT_MEASURE).count() >= 2,
        "the row stopped repainting when characters were typed at it. Nothing \
         was submitted — there is no newline in the line buffer — so the row is \
         still the pump's to animate, and abandoning it here would take away \
         the liveness signal for the rest of a three-second silence (BR-9, \
         BR-4); repaints after the echo: {}\ntranscript:\n{seen}",
        seen[echo_at..].matches(REPAINT_MEASURE).count()
    );
    // The row's cadence delayed nothing: the return goes in after the turn is
    // over and the buffered characters are still there to be read.
    ahead.type_raw("\r");
    assert!(
        ahead.wait_for(QUOTED),
        "the characters typed during the animation never reached the entry loop \
         — the row must neither consume nor delay input (BR-9); transcript:\n{}",
        ahead.snapshot()
    );
    let seen = ahead.snapshot();
    assert!(
        seen.contains(QUOTED),
        "the carried line reached the entry loop altered: it must arrive as the \
         next prompt's text byte for byte (BR-9); transcript:\n{seen}"
    );
    assert_no_row_on_screen(&seen, "typing ahead during the animation");
    drop(ahead);

    // ---- Half 2: the whole line, return included, mid-animation ----
    let mut submitted = RenderedSession::open(100, &[&format!("@delay-ms 3000\n{FIRST}")], &[]);
    submitted.type_line("hold the turn open");
    assert!(
        submitted.wait_until(|seen| seen.matches(REPAINT_OPEN).count() >= 2),
        "the row never animated; transcript:\n{}",
        submitted.snapshot()
    );
    submitted.type_raw(&format!("{CARRIED}\r"));

    assert!(
        submitted.wait_for(FIRST),
        "the first turn never finished; transcript:\n{}",
        submitted.snapshot()
    );
    assert!(
        submitted.wait_for(QUOTED),
        "a line submitted while the row was animating never reached the entry \
         loop, or reached it altered — the row's cadence adds no latency to \
         input and the kernel's line buffer is untouched by anything it draws \
         (BR-9); transcript:\n{}",
        submitted.snapshot()
    );

    let seen = submitted.snapshot();
    // The echo of the submitted line, and the newline the terminal echoed after
    // it. `CARRIED` appears twice in the transcript — once as this echo, once
    // inside the client's own quote of it — and the echo is the first, because
    // the quote is written after the line has been read.
    let echo_at = seen
        .find(CARRIED)
        .unwrap_or_else(|| panic!("the submitted line was never echoed; transcript:\n{seen}"));
    let enter_at = seen[echo_at..]
        .find('\n')
        .map(|offset| echo_at + offset + 1)
        .unwrap_or_else(|| {
            panic!("the submitted line's echo carries no newline; transcript:\n{seen}")
        });

    // Non-vacuity, before the negative: the row was demonstrably repainting up
    // to the moment the line went in.
    let before = seen[..enter_at].matches(REPAINT_MEASURE).count();
    assert!(
        before >= 2,
        "the row never repainted before the line was submitted, so the zero \
         below is the zero of an animation that was not running; \
         transcript:\n{seen}"
    );

    // (1) Not one repaint attempted past the echoed newline. From that byte on
    // the row is two rows above the cursor, so a repaint would rewrite the line
    // holding what the user just typed — the blanking BR-9 forbids — and the
    // pump gives the row up instead.
    let after = seen[enter_at..].matches(REPAINT_MEASURE).count();
    assert_eq!(
        after, 0,
        "the pump went on repainting after the user submitted a line. The \
         echoed newline dropped the cursor a row without anything this client \
         could observe, so every offset the row owns is short by one and the \
         next repaint lands on the line the user typed (BR-9); {before} \
         repaints before the newline and {after} after it; transcript:\n{seen}"
    );

    // (2) And the echo survived, on the **screen** rather than in the stream: a
    // repaint that erased it would leave both the echo and the erasure in the
    // transcript, so only the replay can tell the two apart.
    let screen = common::rendered_screen(&seen);
    assert!(
        screen.iter().any(|row| row.contains(CARRIED)),
        "the line the user typed is not on the screen: something drew over it \
         after the terminal echoed it, which is the one outcome worse than a \
         stale row (BR-9); screen:\n{screen:#?}\ntranscript:\n{seen}"
    );
}

/// The two config lines a leg that needs a `shell` call to **ask** carries.
///
/// [`FULL_AND_NO_CONTEXT_OFFER`]'s opposite, and the default: `guarded` is the
/// level a session starts at, and at it a `shell` call is a question standing
/// between the turn and the row. Written out rather than left implicit so the
/// leg below varies exactly one thing from the running-tool leg it is the
/// counterpart of.
///
/// `generate = "never"` is here for the same reason it is there — the first
/// turn of a session inside a project offers to write a `TETON.md` (REQ-613),
/// and at `guarded` that offer is a *second* permission prompt, which would put
/// a question the leg is not about in front of the one it is.
const GUARDED_AND_NO_CONTEXT_OFFER: &str =
    "[permissions]\ndefault_level = \"guarded\"\n\n[context]\ngenerate = \"never\"\n\n";

/// **REQ-621 BR-1 / AC-11 — the row steps aside for a permission prompt and
/// comes back the instant it is answered.**
///
/// The clause of BR-1 that had no pty leg. `awaiting_permission` is the one
/// phase that is *not* idle and still draws nothing: the prompt owns the
/// terminal and its own question is the indication (BR-9, BR-10), and a spinner
/// repainting the line above a question the user is reading is the second
/// renderer BR-10 forbids. Until this leg the claim was pinned only at
/// renderer-unit level, which is the shape BUG-191 is about — AC-11 requires a
/// real terminal for every TTY claim in the list.
///
/// # What the daemon actually does, and why the window starts where it does
///
/// The order on screen is **`[running]` line, then the question**. The daemon
/// publishes `tool_call` (`in_progress`) before it consults the permission
/// gate, so the client learns a tool has started and moves to `tool_running`
/// *first*, and only then does the request arrive and take the row back. That
/// is the producer's own sequence and this leg asserts it as such rather than
/// asserting the order somebody would have designed (LESSON-544, LESSON-628):
/// the window the negative claim is made over therefore opens at the
/// **question's first byte**, not at the tool's, because a row above the
/// question is the row correctly showing the phase the daemon had reported at
/// the time.
///
/// Four claims:
///
/// 1. the question is put at all — the level, not the fixture, decides that,
///    and without it the rest is a negative claim about a turn that never asked;
/// 2. no activity row appears from the question's first byte until the answer
///    goes in — positionally, over a snapshot taken **while the question was
///    standing**, so this is not a `contains` that would be satisfied by the
///    animation either side of it;
/// 3. the row returns after the answer, in `tool_running`, with the title the
///    daemon composed — BR-1's "the instant the next silent phase begins", and
///    the non-vacuity for claim (2): a phase that can draw a row, and did not
///    while the question stood;
/// 4. the turn completes and the screen carries no residue (BR-5).
///
/// # What breaks this test
///
/// Claim (3) is the one with teeth and it fails on the property. A pump that
/// stopped painting on its own clock leaves it with nothing: `sleep 2` publishes
/// nothing while it runs, so every row in the post-answer window is a tick's.
/// The mutation recorded on
/// `the_row_appears_before_the_first_byte_and_withdraws_when_text_streams` — the
/// `Wake::Tick` arm advancing `row.tick` without calling `paint_row` — reddens
/// this leg at claim (3) for that reason.
///
/// Claim (2) is a negative, so its teeth are in the window boundary, and that
/// boundary was **mutated and observed failing** too:
///
/// | Mutation | Fails |
/// |---|---|
/// | the window opens at the `[running]` line instead of the question's first byte | claim (2), reporting the one row it then swallows — `⠋ running shell: sleep 2 · 0s · turn 0s` |
///
/// Which is the evidence the window is the right one rather than a wide net: a
/// row *is* drawn in the stretch between the tool announcing itself and the
/// question being put, the client takes it back immediately before the
/// question's first byte, and the leg's negative claim is about the stretch
/// after that and nothing else.
#[test]
fn the_row_steps_aside_for_a_permission_prompt_and_returns_after_the_answer() {
    const REPLY: &str = "The asked-for tool turn is done.";
    const RUNNING: &str = " - shell: sleep 2 [running]";
    const ASKED: &str = "permission requested: shell";
    // The prompt's own option row, verbatim: `shell` offers no `[p]ermanently`,
    // so this is the four-way form (REQ-563 BR-4).
    const OPTIONS: &str = "allow shell? [y]es / [n]o / [a]llow-always / [d]eny-always:";
    // `LineKind::Prompt`'s prefix, which is what makes the request line's
    // *first* byte findable — the claim below is about what the row does
    // immediately before the whole line, not before its text.
    const PROMPT_MARKER: &str = "? ";
    let mut session = RenderedSession::open_with_config(
        100,
        &[
            r#"{"tool": "shell", "arguments": {"command": "sleep 2"}}"#,
            REPLY,
        ],
        &[],
        &local_tier_config(GUARDED_AND_NO_CONTEXT_OFFER),
    );
    session.type_line("ask me first");

    // (1) The question, and the option row with it. Waited on rather than
    // inferred: a prompt that never came times out here instead of being read
    // out of a partial transcript.
    assert!(
        session.wait_for(OPTIONS),
        "the default level never asked about a `shell` call, so this leg never \
         reached the phase it is about; transcript:\n{}",
        session.snapshot()
    );

    // (2) Snapshotted **while the question is standing** and before a byte of
    // the answer goes in, so the window below cannot contain anything from
    // after it.
    let asking = session.snapshot();
    let asked_at = asking
        .find(ASKED)
        .unwrap_or_else(|| panic!("the request line is not in the transcript:\n{asking}"));
    let during = activity_rows(&asking[asked_at..]);
    assert!(
        during.is_empty(),
        "an activity row was painted while a permission question was standing. \
         During `awaiting_permission` the row is withdrawn: the prompt owns the \
         terminal and its own question is the indication, and a spinner \
         repainting the line above it is the second renderer BR-10 forbids \
         (BR-1); rows: {during:#?}\ntranscript:\n{asking}"
    );
    // The daemon's own order, asserted rather than assumed: the tool announced
    // itself first and the question came after it, which is why the window
    // above opens at the question and not at the tool.
    assert!(
        asking
            .find(RUNNING)
            .is_some_and(|running_at| running_at < asked_at),
        "the daemon is expected to publish `tool_call` before it consults the \
         permission gate, so the `[running]` line precedes the question — if \
         that changed, the window this leg makes its negative claim over is the \
         wrong window; transcript:\n{asking}"
    );
    // And the row was taken back rather than left above the question: the
    // question's first byte lands on the row's own line, so the prompt closes
    // over it with no gap and no residue (BR-5).
    let row_at = asked_at - PROMPT_MARKER.len();
    assert!(
        asking[..asked_at].ends_with(PROMPT_MARKER) && asking[..row_at].ends_with(WITHDRAW),
        "the question did not land on the row's own line, so the row was left \
         above a prompt the user is reading instead of withdrawn (BR-1, BR-5); \
         before it: {:?}",
        &asking[row_at.saturating_sub(24)..asked_at]
    );

    // The answer.
    session.type_line("y");
    assert!(
        session.wait_for(REPLY),
        "the turn never completed after the permission was granted; \
         transcript:\n{}",
        session.snapshot()
    );

    // (3) The row is back, in the phase the daemon reported, with the daemon's
    // own title — and the window is everything after the snapshot taken while
    // the question stood, so this is the row *returning* rather than the row
    // that was up before it.
    let seen = session.snapshot();
    let returned = activity_rows(&seen[asking.len()..]);
    assert!(
        returned
            .iter()
            .any(|row| row.contains("running shell: sleep 2")),
        "the row did not come back once the permission was answered. BR-1 has \
         it return the instant the next silent phase begins, and the two \
         seconds a granted `sleep 2` then spends are exactly the stretch this \
         REQ exists for; rows: {returned:#?}\ntranscript:\n{seen}"
    );

    // (4) And the turn left nothing behind.
    assert_no_row_on_screen(&seen, "a permission prompt mid-turn");
}
