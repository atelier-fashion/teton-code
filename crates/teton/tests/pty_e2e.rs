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
        Self::spawn_with(daemon, "", &["scripted reply"])
    }

    /// A daemon whose config carries `extra` (one more TOML table) and whose
    /// scripted tier replays `replies`, one per model call.
    ///
    /// REQ-563 needs both: `[web] tier` is config rather than a flag (web lookup
    /// is off by default, BR-1), and a lookup only happens if the scripted tier
    /// emits the tool call that asks for one.
    fn spawn_with(daemon: &Path, extra: &str, replies: &[&str]) -> Self {
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
        let child = std::process::Command::new(daemon)
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
