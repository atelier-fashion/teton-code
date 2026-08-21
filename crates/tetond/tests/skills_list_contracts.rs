//! REQ-585 TASK-203 — the registry has a lifetime, and `skills/list` reports it
//! (BR-1, BR-3, AC-14; ADR-1/ADR-2).
//!
//! Every test here drives the **real daemon** over a real Unix socket: two
//! clients, the handshake, `session/create`, `session/set_cwd`, `skills/list`.
//! A registry the daemon owns is only as good as the answer that crosses the
//! wire (LESSON-484 — a gate a client enforces is a rendering choice), so the
//! assertions are made against the JSON frames the daemon actually sent rather
//! than against structs built in-process.
//!
//! ## Why this binary owns `HOME`
//!
//! Two of the four discovery roots are `~/.claude/skills` and
//! `~/.claude/commands`, and the daemon reads `HOME` per probe. Left at the
//! developer's own home the suite would be asserting against whatever skills
//! that machine happens to have — and on the machine this feature was written
//! for, `~/.claude/skills` is a symlink into a checked-out toolkit with twenty
//! of them. So the binary points `HOME` at a fixture home once, before any
//! daemon exists ([`fixture_home`]), and every test's user rows are then a fact
//! about this file rather than about the runner.
//!
//! ## How an absence is asserted
//!
//! Never by a timer. The negative claims here — "the second turn listed
//! nothing", "the row from the old root is gone" — are each bounded by a
//! positive control in the same test: the `/cd` that follows *does* move the
//! recorder's count, and the row that replaced the old one *is* present. A
//! passing negative therefore cannot be discovery merely being broken
//! (LESSON-479).

use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::Duration;

use serde_json::{json, Value};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::unix::{OwnedReadHalf, OwnedWriteHalf};
use tokio::net::UnixStream;
use tokio::time::timeout;

use teton_protocol::{PROTOCOL_VERSION_MAX, PROTOCOL_VERSION_MIN};
use tetond::skills::{DirLister, Entry, ListError, ReadError, RealFs};
use tetond::{server, Daemon};

// ---------------------------------------------------------------------------
// fixtures
// ---------------------------------------------------------------------------

/// A throwaway directory tree, removed on drop.
struct Tree {
    root: PathBuf,
}

impl Tree {
    /// A fresh tree under `/tmp` with a short name: a daemon socket is bound
    /// beneath these paths and `sun_len` caps them at ~104 bytes.
    fn new(tag: &str) -> Self {
        static SEQ: AtomicUsize = AtomicUsize::new(0);
        let seq = SEQ.fetch_add(1, Ordering::SeqCst);
        let root =
            PathBuf::from("/tmp").join(format!("tsl{tag}{:x}{seq:x}", std::process::id() & 0xffff));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).unwrap();
        Self { root }
    }

    /// Write `contents` at `rel`, creating every parent directory.
    fn write(&self, rel: &str, contents: &str) {
        let path = self.root.join(rel);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, contents).unwrap();
    }

    fn path(&self) -> &Path {
        &self.root
    }
}

impl Drop for Tree {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.root);
    }
}

/// A minimal well-formed skill file.
fn skill_file(name: &str, description: &str, hint: &str) -> String {
    format!("---\nname: {name}\ndescription: {description}\nargument-hint: {hint}\n---\n\nBody of {name}.\n")
}

/// The `HOME` every daemon in this binary discovers its **user** skills under.
///
/// Set once, before any daemon is constructed, and never changed: each test
/// calls this first, so the write happens while every other test is still
/// blocked inside the `OnceLock` initializer rather than beside a live read.
///
/// The tree is deliberately not dropped — it has to outlive every test in the
/// binary — so it is re-created from scratch on each run (the pid in the name
/// plus the `remove_dir_all` in [`Tree::new`]) rather than cleaned up at the
/// end of one.
fn fixture_home() -> &'static Path {
    static HOME: OnceLock<Tree> = OnceLock::new();
    HOME.get_or_init(|| {
        let home = Tree::new("h");
        home.write(
            ".claude/skills/hometool/SKILL.md",
            &skill_file("hometool", "the user skill", "[flag]"),
        );
        // The `commands/` shape, with no frontmatter at all — the common case
        // for `.claude/commands/*.md`, and a row with no description.
        home.write(".claude/commands/homecmd.md", "Say something.\n");
        // Found and not registered: an opening delimiter with no closing one.
        // Named on the wire rather than dropped in silence (BR-1).
        home.write(".claude/skills/broken/SKILL.md", "---\nname: broken\n");
        std::env::set_var("HOME", home.path());
        home
    })
    .path()
}

/// A [`DirLister`] that answers exactly as the real filesystem does and
/// remembers every path it was asked about (the TASK-195 seam, `Sync` because
/// the daemon shares it across connection tasks).
///
/// This is the only way the cost claim can be made at all: "discovery was paid
/// once" and "discovery was paid on every turn" produce *the same registry*, so
/// the difference is only visible in what was opened.
#[derive(Default)]
struct RecordingFs {
    inner: RealFs,
    listed: Mutex<Vec<PathBuf>>,
    read: Mutex<Vec<PathBuf>>,
}

impl DirLister for RecordingFs {
    fn list(&self, dir: &Path) -> Result<Vec<Entry>, ListError> {
        self.listed.lock().unwrap().push(dir.to_path_buf());
        self.inner.list(dir)
    }

    fn read(&self, file: &Path) -> Result<String, ReadError> {
        self.read.lock().unwrap().push(file.to_path_buf());
        self.inner.read(file)
    }
}

impl RecordingFs {
    fn listed(&self) -> Vec<PathBuf> {
        self.listed.lock().unwrap().clone()
    }

    fn read_paths(&self) -> Vec<PathBuf> {
        self.read.lock().unwrap().clone()
    }
}

/// The counter, not the timestamp, guarantees uniqueness: `SystemTime::now()`
/// can return the same value for two calls within one clock tick.
fn temp_socket(tag: &str) -> PathBuf {
    static NEXT: AtomicUsize = AtomicUsize::new(0);
    std::env::temp_dir().join(format!(
        "teton-{tag}-{}-{}.sock",
        std::process::id(),
        NEXT.fetch_add(1, Ordering::Relaxed),
    ))
}

/// A consent window a test can outlast, for the one test that needs a second
/// client genuinely attached to somebody else's session.
const TEST_CONSENT_WINDOW: Duration = Duration::from_millis(200);

// ---------------------------------------------------------------------------
// a minimal JSON-RPC client
// ---------------------------------------------------------------------------

/// A newline-delimited JSON-RPC client over the daemon socket.
///
/// Deliberately its own small thing rather than a shared harness: the tests
/// here need to interleave one client's request with another client's event,
/// and single-threaded framing is what makes "B saw this after A did that" a
/// fact about ordering rather than about scheduling.
struct TestClient {
    reader: BufReader<OwnedReadHalf>,
    writer: OwnedWriteHalf,
    next_id: i64,
}

impl TestClient {
    async fn connect(path: &Path) -> Self {
        let stream = UnixStream::connect(path).await.unwrap();
        let (read_half, write_half) = stream.into_split();
        Self {
            reader: BufReader::new(read_half),
            writer: write_half,
            next_id: 1,
        }
    }

    async fn send(&mut self, method: &str, params: Value) -> i64 {
        let id = self.next_id;
        self.next_id += 1;
        let mut text = serde_json::to_string(&json!({
            "jsonrpc": "2.0",
            "id": id,
            "method": method,
            "params": params,
        }))
        .unwrap();
        text.push('\n');
        self.writer.write_all(text.as_bytes()).await.unwrap();
        self.writer.flush().await.unwrap();
        id
    }

    async fn read_line(&mut self) -> Value {
        let mut line = String::new();
        let read = timeout(Duration::from_secs(5), self.reader.read_line(&mut line))
            .await
            .expect("timed out waiting for a frame")
            .unwrap();
        assert!(read > 0, "connection closed unexpectedly");
        serde_json::from_str(&line).unwrap()
    }

    async fn read_response(&mut self, id: i64) -> Value {
        loop {
            let frame = self.read_line().await;
            if frame.get("id").and_then(Value::as_i64) == Some(id) {
                return frame;
            }
        }
    }

    /// Send `method` and read frames until its response.
    async fn call(&mut self, method: &str, params: Value) -> Value {
        let id = self.send(method, params).await;
        self.read_response(id).await
    }

    /// Read frames until the named event notification arrives.
    async fn read_event(&mut self, name: &str) -> Value {
        loop {
            let frame = self.read_line().await;
            if frame.get("method").and_then(Value::as_str) != Some("event") {
                continue;
            }
            if frame["params"]["event"].as_str() == Some(name) {
                return frame["params"].clone();
            }
        }
    }

    async fn handshake(&mut self) {
        let answer = self
            .call(
                "handshake",
                json!({
                    "client_kind": "cli",
                    "client_name": "skills-test-client",
                    "client_version": "0.1.0",
                    "protocol_min": PROTOCOL_VERSION_MIN,
                    "protocol_max": PROTOCOL_VERSION_MAX,
                    "monitor": false,
                }),
            )
            .await;
        assert!(answer.get("result").is_some(), "handshake failed: {answer}");
    }

    /// Create a freeform session rooted at `cwd`.
    async fn create_session_at(&mut self, cwd: &Path) -> String {
        let created = self
            .call("session/create", json!({"mode": "freeform", "cwd": cwd}))
            .await;
        created["result"]["session_id"]
            .as_str()
            .unwrap_or_else(|| panic!("session/create failed: {created}"))
            .to_owned()
    }

    async fn skills_list(&mut self, session: &str) -> Value {
        let listed = self
            .call("skills/list", json!({"session_id": session}))
            .await;
        assert!(
            listed.get("result").is_some(),
            "skills/list failed: {listed}"
        );
        listed["result"].clone()
    }
}

/// The `(name, source)` pairs a `skills/list` result registered.
fn rows(result: &Value) -> Vec<(String, String)> {
    result["skills"]
        .as_array()
        .expect("a result carries a skills array")
        .iter()
        .map(|view| {
            (
                view["name"].as_str().unwrap().to_owned(),
                view["source"].as_str().unwrap().to_owned(),
            )
        })
        .collect()
}

/// The names of the rows from one source.
fn names_from(result: &Value, source: &str) -> Vec<String> {
    rows(result)
        .into_iter()
        .filter(|(_, from)| from == source)
        .map(|(name, _)| name)
        .collect()
}

/// The user rows every test in this binary sees, from [`fixture_home`].
fn expected_user_rows() -> Vec<String> {
    vec!["homecmd".to_owned(), "hometool".to_owned()]
}

// ---------------------------------------------------------------------------
// the query
// ---------------------------------------------------------------------------

/// **The whole answer, over the socket**: every registered row with its source,
/// the file-authored fields, and the skipped entry named rather than dropped.
///
/// The skipped entry is the load-bearing half. BR-1's entity table says a skill
/// path is never shown as an absolute one — `/Users/jane/.claude/skills/broken/
/// SKILL.md` carries a username into a transcript and, through `/help`, into a
/// screenshot — so what crosses the wire is `~/…`. And it carries its **name**,
/// derived on the side that owns BR-2's naming rule rather than re-derived by
/// every client that needs to answer "you typed `/broken`, here is why it is
/// not there".
#[tokio::test]
async fn skills_list_reports_the_sessions_registry_with_sources_and_skips() {
    let home = fixture_home();
    let repo = Tree::new("r");
    repo.write(
        ".claude/skills/alpha/SKILL.md",
        &skill_file("alpha", "the project skill", "[path]"),
    );
    let socket = temp_socket("skills-list");
    let listener = server::bind_listener(&socket).unwrap();
    let server_task = tokio::spawn(server::serve(listener, Arc::new(Daemon::new())));

    let mut client = TestClient::connect(&socket).await;
    client.handshake().await;
    let session = client.create_session_at(repo.path()).await;
    let listed = client.skills_list(&session).await;

    assert_eq!(
        rows(&listed),
        vec![
            ("alpha".to_owned(), "project".to_owned()),
            ("homecmd".to_owned(), "user".to_owned()),
            ("hometool".to_owned(), "user".to_owned()),
        ],
        "every root's rows, ordered by the daemon rather than by the filesystem: {listed}"
    );

    let alpha = &listed["skills"][0];
    assert_eq!(alpha["description"].as_str(), Some("the project skill"));
    assert_eq!(alpha["argument_hint"].as_str(), Some("[path]"));
    assert!(
        alpha.get("shadowed").is_none(),
        "nothing owns this name, so no key at all: {alpha}"
    );
    let homecmd = &listed["skills"][1];
    assert!(
        homecmd.get("description").is_none() && homecmd.get("argument_hint").is_none(),
        "a `commands/` file with no frontmatter declares neither, and an absence \
         writes no key: {homecmd}"
    );

    assert_eq!(
        listed["skipped"].as_array().map(Vec::len),
        Some(1),
        "the malformed user skill is counted and named: {listed}"
    );
    let broken = &listed["skipped"][0];
    assert_eq!(broken["name"].as_str(), Some("broken"));
    assert_eq!(
        broken["path"].as_str(),
        Some("~/.claude/skills/broken/SKILL.md"),
        "a skipped path crosses the wire home-relative, never absolute: {broken}"
    );
    assert_eq!(broken["reason"].as_str(), Some("malformed frontmatter"));
    assert!(
        home.join(".claude/skills/broken/SKILL.md").exists(),
        "the fixture must really be there, or the assertions above are about \
         a missing file"
    );

    server_task.abort();
    let _ = std::fs::remove_file(&socket);
}

/// **REQ-587 BR-3 — both invocation flags cross the wire.**
///
/// The client marks a model-only row `(model-only)` in `/help`, and it can only
/// do that from these two keys. A `skills_list_result` that dropped either one
/// leaves that mark inert with nothing red anywhere: the roster the daemon
/// builds for the model is built from the *registry*, not from this result, so
/// every daemon-side test would still pass while `/help` quietly stopped
/// telling anyone which skills they cannot type.
///
/// The **absences** are asserted too, because both keys are additive
/// (`skip_serializing_if`) and an absent key is a value: `model_invocable` is
/// absent for a hidden skill and `user_invocable` absent for an ordinary one,
/// which is what keeps an ordinary row's bytes what REQ-585 wrote.
///
/// **Mutation**: hard-code either field in `skills_list_result` — the shape
/// TASK-210's placeholder had — and one of the four `Some(…)` assertions here
/// fails.
#[tokio::test]
async fn both_invocation_flags_reach_the_client() {
    let _home = fixture_home();
    let repo = Tree::new("flags");
    repo.write(
        ".claude/skills/alpha/SKILL.md",
        &skill_file("alpha", "invocable by both", "[path]"),
    );
    // Hidden from the model, still the user's.
    repo.write(
        ".claude/commands/beta.md",
        "---\ndisable-model-invocation: true\n---\nBody of beta.\n",
    );
    // Model-only: the state that exists only because the two questions are
    // asked separately.
    repo.write(
        ".claude/commands/delta.md",
        "---\nuser-invocable: false\n---\nBody of delta.\n",
    );
    // A flag whose *value* is not a boolean literal: the file still registers,
    // the safe reading applies, and the key is named as ignored rather than
    // silently obeyed.
    repo.write(
        ".claude/commands/typo.md",
        "---\nuser-invocable: yes\n---\nBody of typo.\n",
    );

    let socket = temp_socket("skills-flags");
    let listener = server::bind_listener(&socket).unwrap();
    let server_task = tokio::spawn(server::serve(listener, Arc::new(Daemon::new())));

    let mut client = TestClient::connect(&socket).await;
    client.handshake().await;
    let session = client.create_session_at(repo.path()).await;
    let listed = client.skills_list(&session).await;

    let row = |name: &str| -> Value {
        listed["skills"]
            .as_array()
            .expect("a result carries a skills array")
            .iter()
            .find(|view| view["name"].as_str() == Some(name))
            .unwrap_or_else(|| panic!("`{name}` must be listed: {listed}"))
            .clone()
    };

    let alpha = row("alpha");
    assert_eq!(
        alpha["model_invocable"].as_bool(),
        Some(true),
        "an ordinary skill is the model's: {alpha}"
    );
    assert!(
        alpha.get("user_invocable").is_none(),
        "…and its `user_invocable` is the absent default, so an ordinary row's \
         bytes are the ones REQ-585 wrote: {alpha}"
    );

    let beta = row("beta");
    assert!(
        beta.get("model_invocable").is_none(),
        "`disable-model-invocation: true` writes no key, because absent already \
         means not model-invocable: {beta}"
    );
    assert!(
        beta.get("user_invocable").is_none(),
        "and the flag says nothing about the user: {beta}"
    );

    let delta = row("delta");
    assert_eq!(
        delta["user_invocable"].as_bool(),
        Some(false),
        "the model-only state has to reach the client or `/help` cannot mark \
         it: {delta}"
    );
    assert_eq!(
        delta["model_invocable"].as_bool(),
        Some(true),
        "…and a skill the user may not type is still the model's — the whole \
         point of the third state: {delta}"
    );

    let typo = row("typo");
    assert!(
        typo.get("user_invocable").is_none(),
        "a value that is not a boolean leaves the user's `/name` alone: {typo}"
    );

    server_task.abort();
    let _ = std::fs::remove_file(&socket);
}

/// **BR-3 / LESSON-517: what reaches the client is already bounded and
/// neutralized.**
///
/// A description is file bytes, written by whoever owns the repo the session
/// stands in — which is not always the person reading the screen. The client
/// defuses again at the terminal (`Surface::line`), but that is the *second*
/// end: the phase-2 VS Code client has no such function, so a 4,000-character
/// description with a right-to-left override in it has to be harmless as
/// protocol.
///
/// **Mutation**: drop `bounded_field` from `skills_list_result` and the wire
/// carries all 4,000 characters, the bidi override and the zero-width space —
/// three separate assertions, each of which fails on its own.
#[tokio::test]
async fn a_loud_description_reaches_the_client_bounded_and_neutralized() {
    let _home = fixture_home();
    let repo = Tree::new("loud");
    // One line, because the frontmatter grammar is line-based — the characters
    // planted here are the ones that survive that and still ruin a terminal: a
    // right-to-left override, a zero-width space, a line separator, a carriage
    // return and a BEL.
    let loud = format!(
        "start\u{202E}\u{200B}\u{2028}\r\u{7}{}end",
        "x".repeat(4_000)
    );
    repo.write(
        ".claude/skills/loud/SKILL.md",
        &format!("---\ndescription: {loud}\nargument-hint: {loud}\n---\n\nbody\n"),
    );
    let socket = temp_socket("skills-bound");
    let listener = server::bind_listener(&socket).unwrap();
    let server_task = tokio::spawn(server::serve(listener, Arc::new(Daemon::new())));

    let mut client = TestClient::connect(&socket).await;
    client.handshake().await;
    let session = client.create_session_at(repo.path()).await;
    let listed = client.skills_list(&session).await;

    let loud_row = listed["skills"]
        .as_array()
        .unwrap()
        .iter()
        .find(|view| view["name"] == "loud")
        .unwrap_or_else(|| panic!("the loud skill must register: {listed}"));
    let description = loud_row["description"].as_str().expect("a description");
    let hint = loud_row["argument_hint"].as_str().expect("a hint");

    assert!(
        description.chars().count() <= 200,
        "a 4,000-character description must arrive bounded, not whole: {} chars",
        description.chars().count()
    );
    assert!(
        hint.chars().count() <= 80,
        "and the hint shares the row, so it is bounded too: {} chars",
        hint.chars().count()
    );
    for (field, text) in [("description", description), ("argument_hint", hint)] {
        assert!(
            text.contains('…'),
            "{field} must say it was cut rather than end mid-word: {text}"
        );
        assert!(
            !text.chars().any(char::is_control),
            "{field} must carry no control character: {text:?}"
        );
        for hidden in ['\u{202E}', '\u{200B}', '\u{2028}'] {
            assert!(
                !text.contains(hidden),
                "{field} must carry no hidden or bidi character ({hidden:?}): {text:?}"
            );
        }
    }

    // Non-vacuity: the file really did hold all of it, so the bounding above is
    // the daemon's doing and not the fixture's.
    let written =
        std::fs::read_to_string(repo.path().join(".claude/skills/loud/SKILL.md")).unwrap();
    assert!(written.contains(&loud) && loud.chars().count() > 4_000);

    server_task.abort();
    let _ = std::fs::remove_file(&socket);
}

/// **AC-3, through the probe**: a session whose root *is* `$HOME` reaches
/// `~/.claude/skills` once, not twice.
///
/// The registry is built from the session's **probed** root, which carries the
/// `RootKind` as well as the path. Hard-code `Project` in
/// `rebuild_session_skills` and every user skill registers under both sources —
/// each name shadowing itself, with two permission keys for one file — which is
/// exactly what this asserts is absent.
#[tokio::test]
async fn a_session_rooted_at_home_registers_each_user_skill_once() {
    let home = fixture_home();
    let socket = temp_socket("skills-home");
    let listener = server::bind_listener(&socket).unwrap();
    let server_task = tokio::spawn(server::serve(listener, Arc::new(Daemon::new())));

    let mut client = TestClient::connect(&socket).await;
    client.handshake().await;
    let session = client.create_session_at(home).await;
    let listed = client.skills_list(&session).await;

    assert_eq!(
        names_from(&listed, "user"),
        expected_user_rows(),
        "the user pair, once: {listed}"
    );
    assert!(
        names_from(&listed, "project").is_empty(),
        "a home-kind session has no project pair to discover: {listed}"
    );
    assert!(
        listed["skills"]
            .as_array()
            .unwrap()
            .iter()
            .all(|view| view.get("shadowed").is_none()),
        "and nothing shadows itself: {listed}"
    );

    server_task.abort();
    let _ = std::fs::remove_file(&socket);
}

// ---------------------------------------------------------------------------
// AC-14 — the lifetime
// ---------------------------------------------------------------------------

/// **AC-14: `/cd` re-derives the project skills, leaves the user skills as they
/// were, and the next `skills/list` sees it without a restart.**
///
/// Both halves matter and they fail differently. Skip the rebuild in
/// `handle_session_set_cwd` and the session keeps dispatching a command that
/// lives under a root it has left — the first assertion. Rebuild from the
/// wrong root, or drop the user pair on the way, and the second one goes.
///
/// **Mutation**: deleting the `rebuild_session_skills` call from the
/// `session/set_cwd` handler fails this test on `beta`.
#[tokio::test]
async fn a_cd_re_derives_the_project_skills_and_leaves_the_user_skills_alone() {
    let _home = fixture_home();
    let before = Tree::new("cd1");
    before.write(
        ".claude/skills/alpha/SKILL.md",
        &skill_file("alpha", "the old root's skill", "[path]"),
    );
    let after = Tree::new("cd2");
    after.write(".claude/commands/beta.md", "The new root's command.\n");

    let socket = temp_socket("skills-cd");
    let listener = server::bind_listener(&socket).unwrap();
    let server_task = tokio::spawn(server::serve(listener, Arc::new(Daemon::new())));

    let mut client = TestClient::connect(&socket).await;
    client.handshake().await;
    let session = client.create_session_at(before.path()).await;
    let listed = client.skills_list(&session).await;
    assert_eq!(names_from(&listed, "project"), vec!["alpha".to_owned()]);
    assert_eq!(names_from(&listed, "user"), expected_user_rows());

    let moved = client
        .call(
            "session/set_cwd",
            json!({"session_id": session, "cwd": after.path()}),
        )
        .await;
    assert!(
        moved.get("result").is_some(),
        "session/set_cwd failed: {moved}"
    );

    let listed = client.skills_list(&session).await;
    assert_eq!(
        names_from(&listed, "project"),
        vec!["beta".to_owned()],
        "the new root's commands, and none of the old root's: {listed}"
    );
    assert_eq!(
        names_from(&listed, "user"),
        expected_user_rows(),
        "and the user skills are untouched by a move — they were never derived \
         from the session root: {listed}"
    );

    server_task.abort();
    let _ = std::fs::remove_file(&socket);
}

/// **Two clients, one registry** (REQ-568's topology, REQ-570's consent).
///
/// A session is shared state, and its skills are part of that state: a peer
/// that was granted access to somebody else's session must see the same
/// commands that session dispatches, and must see a `/cd` the other client
/// drove — after its own `skills/list`, because the snapshot is pulled and not
/// pushed (ADR-2: the client refreshes on `session_root_changed`, which the
/// daemon already sends).
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn two_clients_on_one_session_see_one_registry_across_a_cd() {
    let _home = fixture_home();
    let before = Tree::new("mc1");
    before.write(
        ".claude/skills/alpha/SKILL.md",
        &skill_file("alpha", "the old root's skill", "[path]"),
    );
    let after = Tree::new("mc2");
    after.write(".claude/commands/beta.md", "The new root's command.\n");

    let socket = temp_socket("skills-mc");
    let listener = server::bind_listener(&socket).unwrap();
    let daemon = Arc::new(
        Daemon::new()
            .with_consent_timeout(TEST_CONSENT_WINDOW)
            .with_presence_verifier(Box::new(tetond::attest::AcceptingVerifier::default())),
    );
    let server_task = tokio::spawn(server::serve(listener, daemon));

    let mut owner = TestClient::connect(&socket).await;
    owner.handshake().await;
    let session = owner.create_session_at(before.path()).await;

    // A second client, attached the only way a peer can be: by asking, and by
    // the session's own client approving with a verified human behind it.
    let mut peer = TestClient::connect(&socket).await;
    peer.handshake().await;
    let attaching = peer
        .send("session/attach", json!({"session_id": session}))
        .await;
    let asked = owner.read_event("attach_consent_requested").await;
    let request_id = asked["request_id"]
        .as_str()
        .expect("the prompt carries its request id")
        .to_owned();
    let answered = owner
        .call(
            "attach/consent",
            json!({"request_id": request_id, "outcome": {"outcome": "granted"}}),
        )
        .await;
    assert_eq!(
        answered["result"]["resolved"].as_bool(),
        Some(true),
        "the approval must decide the request: {answered}"
    );
    let attached = peer.read_response(attaching).await;
    assert!(
        attached.get("result").is_some(),
        "the peer must be attached: {attached}"
    );

    let owners_view = owner.skills_list(&session).await;
    let peers_view = peer.skills_list(&session).await;
    assert_eq!(
        owners_view, peers_view,
        "one session, one registry — not one per connection"
    );
    assert_eq!(
        names_from(&owners_view, "project"),
        vec!["alpha".to_owned()]
    );

    // The owner moves the root; the peer learns of it the way ADR-2 says it
    // does — an event it already receives, then its own query.
    let moved = owner
        .call(
            "session/set_cwd",
            json!({"session_id": session, "cwd": after.path()}),
        )
        .await;
    assert!(
        moved.get("result").is_some(),
        "session/set_cwd failed: {moved}"
    );
    let announced = peer.read_event("session_root_changed").await;
    assert_eq!(announced["session_id"].as_str(), Some(session.as_str()));

    let peers_view = peer.skills_list(&session).await;
    assert_eq!(
        names_from(&peers_view, "project"),
        vec!["beta".to_owned()],
        "the peer's own refresh must see the move the other client drove: {peers_view}"
    );
    assert_eq!(
        peers_view,
        owner.skills_list(&session).await,
        "and the two are still looking at one registry"
    );

    server_task.abort();
    let _ = std::fs::remove_file(&socket);
}

// ---------------------------------------------------------------------------
// cost
// ---------------------------------------------------------------------------

/// **Discovery is paid at `session/create` and at `/cd`, and nowhere else.**
///
/// The claim is about what was *opened*, so it is asserted through the
/// recording seam: four listings for the four roots, one read per candidate,
/// and then two prompt turns that add nothing to either count.
///
/// The turns fail — this fixture has no provider, by design — and what that
/// leaves asserted is stated rather than glossed: the two lifecycle points are
/// the only callers of `rebuild_session_skills`, and a third caller anywhere on
/// the prompt path would move these counts. The `/cd` at the end is the
/// positive control that makes the negative mean something: the recorder is
/// live, and it does move when discovery genuinely runs again.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn discovery_is_paid_at_create_and_at_cd_and_never_per_turn() {
    let _home = fixture_home();
    let repo = Tree::new("cost1");
    repo.write(
        ".claude/skills/alpha/SKILL.md",
        &skill_file("alpha", "the project skill", "[path]"),
    );
    let elsewhere = Tree::new("cost2");
    elsewhere.write(".claude/commands/beta.md", "The new root's command.\n");

    let recorder = Arc::new(RecordingFs::default());
    let socket = temp_socket("skills-cost");
    let listener = server::bind_listener(&socket).unwrap();
    let daemon = Arc::new(
        Daemon::new().with_skill_lister(Arc::clone(&recorder) as Arc<dyn DirLister + Send + Sync>),
    );
    let server_task = tokio::spawn(server::serve(listener, daemon));

    let mut client = TestClient::connect(&socket).await;
    client.handshake().await;
    let session = client.create_session_at(repo.path()).await;

    let four_roots: HashSet<PathBuf> = [
        repo.path().join(".claude/skills"),
        repo.path().join(".claude/commands"),
        fixture_home().join(".claude/skills"),
        fixture_home().join(".claude/commands"),
    ]
    .into_iter()
    .collect();
    assert_eq!(
        recorder.listed().into_iter().collect::<HashSet<_>>(),
        four_roots,
        "creating a session lists the four roots — and nothing else"
    );
    let after_create = (recorder.listed().len(), recorder.read_paths().len());
    assert_eq!(after_create.0, 4, "{:?}", recorder.listed());

    // Two turns. Both fail (no provider is configured) and both are answered,
    // which is all this needs: a turn that ran is a turn that would have paid
    // for discovery if the prompt path had a discovery on it.
    for text in ["first turn", "second turn"] {
        let answered = client
            .call(
                "session/prompt",
                json!({"session_id": session, "prompt": [{"type": "text", "text": text}]}),
            )
            .await;
        assert!(
            answered.get("result").is_some() || answered.get("error").is_some(),
            "the turn must be answered one way or the other: {answered}"
        );
    }
    assert_eq!(
        (recorder.listed().len(), recorder.read_paths().len()),
        after_create,
        "a turn must open nothing: the registry is a snapshot, not a live view"
    );

    // The control: a root that moves *does* pay again, so the counts above are
    // a property of the prompt path rather than of a recorder that stopped
    // recording.
    let moved = client
        .call(
            "session/set_cwd",
            json!({"session_id": session, "cwd": elsewhere.path()}),
        )
        .await;
    assert!(
        moved.get("result").is_some(),
        "session/set_cwd failed: {moved}"
    );
    assert_eq!(
        recorder.listed().len(),
        8,
        "the move lists the four roots again: {:?}",
        recorder.listed()
    );
    assert!(
        recorder
            .listed()
            .contains(&elsewhere.path().join(".claude/commands")),
        "and the second four are the roots the session now stands on: {:?}",
        recorder.listed()
    );

    server_task.abort();
    let _ = std::fs::remove_file(&socket);
}
