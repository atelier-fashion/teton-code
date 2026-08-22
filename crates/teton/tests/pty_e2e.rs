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
        extra_env: &[(&str, &Path)],
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
    let daemon = TestDaemon::spawn_with(&daemon_path, &config, &[reply]);

    // Wide enough that the terminal cannot hard-wrap the sentence under test —
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
    cmd.env("XDG_RUNTIME_DIR", &daemon.runtime_dir);
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
    let daemon =
        TestDaemon::spawn_with_env(&daemon_path, "", &["scripted reply"], &[("HOME", &home)]);
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
        &[("HOME", &home)],
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
        &[("HOME", &home)],
    );

    let project = daemon.root.join("proj");
    std::fs::create_dir_all(&project).unwrap();
    std::fs::write(project.join("Cargo.toml"), "[package]\nname = \"proj\"\n").unwrap();
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
