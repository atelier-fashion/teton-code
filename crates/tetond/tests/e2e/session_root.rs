//! REQ-583 over the real socket: the session root a session stands on, and
//! `/cd` moving it (TASK-178).
//!
//! Both tests spawn the real `teton-code` binary and drive it as a protocol
//! client; the turn provider is a localhost mock that captures every request
//! body, so "what a `read` handed the model" — a file's contents under the old
//! root, a BR-2 refusal naming the new one — is settled on **bytes that
//! crossed a transport**, not on a status flag.
//!
//! - AC-9's daemon half: `session/create` answers with the derived root (kind,
//!   display, project name) for a cwd'd session and for one on the daemon's
//!   fallback root, and refuses a bad cwd **naming the path**.
//! - AC-10's daemon half, at **every** permission level (`PermissionLevel::ALL`,
//!   `plan` included — LESSON-524's "exposure is not callability"):
//!   `session/set_cwd` moves a live session's jail, clears its conversation,
//!   and puts `context_cleared` **then** `session_root_changed` on the wire
//!   ahead of its own answer; a `read` that worked under the old root is then
//!   refused in the BR-2 shape; a move to a path that does not exist is
//!   refused naming it and leaves root and conversation alone; a connection
//!   that never attached cannot move the session at all.
//!
//! The CLI halves — `--cwd`, the banner line, the `/cd` command and the
//! transcript lines the two events render into — are TASK-179's, asserted in
//! `crates/teton/tests/cli_e2e.rs`. The environment block is asserted **here,
//! on the wire**: the provider body of every turn carries `Session root:
//! <display>` for the root the session stood on when that turn ran — the old
//! display before the move, the new one after it (BR-1/AC-10).

use std::path::Path;
use std::time::Duration;

use serde_json::json;
use teton_protocol::jsonrpc::error_code;
use teton_protocol::permissions::PermissionLevel;

use crate::harness::{
    openai_turn, remote_provider_block, tier_block, Client, Daemon, DaemonOptions, MockProvider,
    MockResponse, Workspace,
};

const GIB: u64 = 1024 * 1024 * 1024;

/// A deterministic above-floor hardware profile with **no** local engine, so
/// every turn here goes to the mock provider and nothing is served off-script.
fn probe_16gb() -> DaemonOptions {
    DaemonOptions::default()
        .env("TETON_PROBE_RAM_BYTES", (16 * GIB).to_string())
        .env("TETON_PROBE_DISK_BYTES", "500000000000")
        .env("TETON_PROBE_GPU", "apple-silicon")
}

/// A scripted turn whose one tool call is a `read` of `path`.
fn read_turn(path: &Path) -> MockResponse {
    let arguments = json!({ "path": path }).to_string();
    MockResponse::ok(openai_turn(
        "Reading it.",
        Some(("c1", "read", &arguments)),
        120,
        20,
    ))
}

/// The turn's closing reply.
fn done() -> MockResponse {
    MockResponse::ok(openai_turn("Done.", None, 10, 5))
}

/// The captured bodies that are **agent turns** — those carrying the system
/// head — as text, in the order the provider received them.
fn remote_turns(provider: &MockProvider) -> Vec<String> {
    provider
        .requests()
        .into_iter()
        .map(|body| String::from_utf8_lossy(&body).into_owned())
        .filter(|body| body.contains("You are Teton Code"))
        .collect()
}

/// The `display` the daemon spells for `path` — the same pure rule the daemon's
/// probe applies (home-relative, bounded), evaluated here so an assertion names
/// the path it means rather than re-implementing the spelling.
fn display_of(path: &Path) -> String {
    use teton_core::session_root::{bounded_field, display_for, DISPLAY_MAX_CHARS};
    let home = std::env::var_os("HOME").map(std::path::PathBuf::from);
    bounded_field(&display_for(path, home.as_deref()), DISPLAY_MAX_CHARS)
}

/// The position of the first event named `name` at or after `from` in the
/// client's log, or `None`.
fn event_index_from(client: &Client, from: usize, name: &str) -> Option<usize> {
    client.events()[from..]
        .iter()
        .position(|e| e["event"].as_str() == Some(name))
        .map(|i| from + i)
}

/// A daemon over a workspace whose fixture repo is the session root under test
/// (it holds a `Cargo.toml`, so it is a `project` root), a second directory
/// `other/` beside it to move to (holding only `marker.txt`), and a mock
/// provider scripted by `script` — a closure, because the reads it scripts
/// name paths inside the workspace, which has to exist first.
fn spawn(
    tag: &str,
    script: impl FnOnce(&Workspace) -> Vec<MockResponse>,
) -> (Workspace, MockProvider, Daemon) {
    let ws = Workspace::new(tag);
    let other = ws.root.join("other");
    std::fs::create_dir_all(&other).unwrap();
    std::fs::write(other.join("marker.txt"), "still standing here\n").unwrap();
    let provider = MockProvider::start(script(&ws), done());
    let mut config = String::new();
    config.push_str(&remote_provider_block(
        "frontier",
        &provider.openai_endpoint(),
        "claude-opus-4",
    ));
    // The turn tier only: no `default_provider` and no category rows, so no
    // duty is bound anywhere and the script is consumed by turns alone.
    config.push_str(&tier_block("build", "frontier"));
    ws.write_config(&config);
    let daemon = Daemon::spawn(&ws, probe_16gb());
    (ws, provider, daemon)
}

// ===========================================================================
// AC-9 (daemon half) — session/create answers with the root, refuses naming
// the path
// ===========================================================================

/// **AC-9 / BR-6, the daemon's half.** `session/create` with a cwd answers with
/// the root the daemon derived — kind `project` for the fixture repo, its
/// display and project name — and with the fallback root (`TETON_REPO_ROOT`,
/// the same directory) for a client that sent none. A cwd that does not exist,
/// or is relative, is refused `INVALID_PARAMS` with the path in the message,
/// and no session is created.
#[test]
fn session_create_returns_the_root_and_refuses_a_bad_cwd_naming_it() {
    let (ws, _provider, daemon) = spawn("root-create", |_| Vec::new());
    let mut client = daemon.connect();

    let created = client.create_session_at("structured", Some("implement"), &ws.repo);
    let root = &created["result"]["root"];
    assert_eq!(
        root["kind"].as_str(),
        Some("project"),
        "the fixture repo holds a Cargo.toml, so it is a project root: {created}"
    );
    assert_eq!(
        root["display"].as_str(),
        Some(display_of(&ws.repo).as_str()),
        "the display is the daemon's spelling of the path it was sent: {created}"
    );
    assert_eq!(
        root["project_name"].as_str(),
        Some("repo"),
        "a project root names its directory: {created}"
    );

    // No cwd: the daemon's own fallback root — the value the turn jails to —
    // which this daemon was spawned with as the same fixture repo.
    let bare = client.call("session/create", json!({ "mode": "freeform" }));
    assert_eq!(
        bare["result"]["root"]["display"].as_str(),
        Some(display_of(&ws.repo).as_str()),
        "a session that sent no cwd is told the daemon's fallback root: {bare}"
    );
    assert_eq!(bare["result"]["root"]["kind"].as_str(), Some("project"));

    let before = client.session_ids().len();
    let missing = client.create_session_at("structured", Some("implement"), Path::new("/nope"));
    assert_eq!(
        missing["error"]["code"].as_i64(),
        Some(error_code::INVALID_PARAMS),
        "a cwd that does not exist is refused: {missing}"
    );
    let message = missing["error"]["message"].as_str().unwrap_or_default();
    assert_eq!(
        message, "path `/nope` does not exist or is not a directory",
        "the refusal is the validator's one root-neutral sentence, naming the path (BR-6)"
    );
    let relative = client.call(
        "session/create",
        json!({ "mode": "freeform", "cwd": "relative/dir" }),
    );
    let message = relative["error"]["message"].as_str().unwrap_or_default();
    assert_eq!(
        relative["error"]["code"].as_i64(),
        Some(error_code::INVALID_PARAMS)
    );
    assert_eq!(
        message, "path `relative/dir` must be an absolute path",
        "the refusal is the validator's one root-neutral sentence, naming the path (BR-6)"
    );
    assert_eq!(
        client.session_ids().len(),
        before,
        "a refused create must leave no session behind — never a session that \
         starts and then fails on every tool"
    );
}

// ===========================================================================
// AC-10 (daemon half) — session/set_cwd at every permission level
// ===========================================================================

/// **AC-10 / BR-7, the daemon's half, at every permission level.**
///
/// One daemon; per level a fresh session at the fixture repo, set to that level
/// through `session/permissions`, then:
///
/// 1. a `read` of `<repo>/README.md` by absolute path hands the model the
///    file (the control: the jail admits it under the old root);
/// 2. `session/set_cwd` to `<root>/other` answers `{root: plain, blocks_dropped
///    > 0}`, and the client saw `context_cleared` **then** `session_root_changed`
///    — carrying the old display as `previous_display` — before the answer;
/// 3. the same `read` now hands the model the BR-2 refusal naming the new
///    root's display, and not the file;
/// 4. `session/set_cwd` to `/nope` is refused `INVALID_PARAMS` naming the
///    path, publishes nothing, and a following relative `read` of
///    `marker.txt` (which exists only under `other`) succeeds — the root did
///    not move — while that turn's request still carries step 3's prompt: the
///    conversation was not cleared either;
/// 5. a second connection that never attached is refused `NOT_ATTACHED` and
///    moves nothing.
///
/// Permission levels gate **tools**, not RPC dispatch, so nothing about `/cd`
/// *should* vary with the level — which is exactly why the whole enumerated
/// set is driven rather than one member (LESSON-524).
#[test]
fn set_cwd_moves_the_jail_clears_and_announces_at_every_permission_level() {
    // Per level: the control read, the read after the move, the read after the
    // refused move — each followed by its closing reply.
    let (ws, provider, daemon) = spawn("root-cd", |ws| {
        let readme = ws.repo.join("README.md");
        let mut scripted = Vec::new();
        for _ in PermissionLevel::ALL {
            scripted.extend([
                read_turn(&readme),
                done(),
                read_turn(&readme),
                done(),
                read_turn(Path::new("marker.txt")),
                done(),
            ]);
        }
        scripted
    });
    let readme = ws.repo.join("README.md");
    let other = ws.root.join("other");

    let old_display = display_of(&ws.repo);
    let new_display = display_of(&other);
    let refusal = format!(
        "path `{}` is outside the session root {new_display}",
        readme.display()
    );
    // BR-1's block, as the provider body carries it (JSON-escaped, so asserted
    // on the label-plus-display prefix rather than the whole line).
    let old_block = format!("Session root: {old_display} (");
    let new_block = format!("Session root: {new_display} (");
    let mut turns_seen = 0usize;

    for level in PermissionLevel::ALL {
        let level_name = level.name();
        let mut client = daemon.connect();

        let created = client.create_session_at("structured", Some("implement"), &ws.repo);
        let session = created["result"]["session_id"]
            .as_str()
            .unwrap_or_else(|| panic!("{level_name}: session/create failed: {created}"))
            .to_owned();
        assert_eq!(
            created["result"]["root"]["kind"].as_str(),
            Some("project"),
            "{level_name}: {created}"
        );
        let set = client.call(
            "session/permissions",
            json!({ "session_id": session, "level": level }),
        );
        assert_eq!(
            set["result"]["level"],
            json!(level),
            "{level_name}: the level must be in force before the moves: {set}"
        );

        // 1. The control: under the old root the file is read.
        let turn = client.prompt(&session, "Read the README.");
        assert_eq!(
            turn["result"]["stop_reason"].as_str(),
            Some("end_turn"),
            "{level_name}: {turn}"
        );
        let turns = remote_turns(&provider);
        assert!(
            turns.len() >= turns_seen + 2,
            "{level_name}: the control turn must reach the provider twice — the \
             tool call and the follow-up carrying its result"
        );
        assert!(
            turns[turns_seen + 1].contains("A tiny fixture repo"),
            "{level_name}: under the old root the model must be handed the file: {}",
            turns[turns_seen + 1]
        );
        for body in &turns[turns_seen..] {
            assert!(
                body.contains(&old_block) && !body.contains(&new_block),
                "{level_name}: before the move every turn's prompt states the old \
                 root `{old_block}`: {body}"
            );
        }
        turns_seen = turns.len();

        // 2. The move: answer, and both events on the wire ahead of it.
        let mark = client.events().len();
        let moved = client.call(
            "session/set_cwd",
            json!({ "session_id": session, "cwd": other }),
        );
        assert_eq!(
            moved["result"]["root"]["kind"].as_str(),
            Some("plain"),
            "{level_name}: `other` holds no marker: {moved}"
        );
        assert_eq!(
            moved["result"]["root"]["display"].as_str(),
            Some(new_display.as_str()),
            "{level_name}: {moved}"
        );
        let dropped = moved["result"]["blocks_dropped"]
            .as_u64()
            .unwrap_or_else(|| panic!("{level_name}: no blocks_dropped: {moved}"));
        assert!(
            dropped > 0,
            "{level_name}: the control turn left blocks for the move to drop: {moved}"
        );
        // Both events were recorded while awaiting the response, so they
        // preceded it on the wire; `context_cleared` first.
        let cleared_at = event_index_from(&client, mark, "context_cleared").unwrap_or_else(|| {
            panic!(
                "{level_name}: no context_cleared before the answer: {:?}",
                &client.events()[mark..]
            )
        });
        let changed_at =
            event_index_from(&client, mark, "session_root_changed").unwrap_or_else(|| {
                panic!(
                    "{level_name}: no session_root_changed before the answer: {:?}",
                    &client.events()[mark..]
                )
            });
        assert!(
            cleared_at < changed_at,
            "{level_name}: context_cleared ({cleared_at}) must precede \
             session_root_changed ({changed_at})"
        );
        let cleared = &client.events()[cleared_at];
        assert_eq!(
            cleared["blocks_dropped"].as_u64(),
            Some(dropped),
            "{level_name}: the event and the answer disagree on the count: {cleared}"
        );
        let changed = &client.events()[changed_at];
        assert_eq!(
            changed["previous_display"].as_str(),
            Some(old_display.as_str()),
            "{level_name}: `previous_display` is the old root's spelling: {changed}"
        );
        assert_eq!(changed["root"]["kind"].as_str(), Some("plain"), "{changed}");
        assert_eq!(
            changed["root"]["display"].as_str(),
            Some(new_display.as_str()),
            "{changed}"
        );
        assert_eq!(
            changed["session_id"].as_str(),
            Some(session.as_str()),
            "{level_name}: the event is scoped to the session it is about: {changed}"
        );

        // 3. The same read is now outside the root, and says so in the BR-2
        // shape — the caller's path, the words, the new display.
        let turn = client.prompt(&session, "Read the README again.");
        assert_eq!(
            turn["result"]["stop_reason"].as_str(),
            Some("end_turn"),
            "{level_name}: a refused tool call is a turn that completes: {turn}"
        );
        let turns = remote_turns(&provider);
        assert!(turns.len() >= turns_seen + 2, "{level_name}");
        let follow_up = &turns[turns_seen + 1];
        assert!(
            follow_up.contains(&refusal),
            "{level_name}: after the move the model must be handed the BR-2 \
             refusal `{refusal}`: {follow_up}"
        );
        assert!(
            !follow_up.contains("A tiny fixture repo"),
            "{level_name}: the file under the old root must not be read: {follow_up}"
        );
        for body in &turns[turns_seen..] {
            assert!(
                body.contains(&new_block) && !body.contains(&old_block),
                "{level_name}: after the move every turn's prompt states the new \
                 root `{new_block}` and not the old: {body}"
            );
        }
        turns_seen = turns.len();

        // 4. A refused move: names the path, publishes nothing, moves nothing,
        // clears nothing.
        let mark = client.events().len();
        let refused = client.call(
            "session/set_cwd",
            json!({ "session_id": session, "cwd": "/nope" }),
        );
        assert_eq!(
            refused["error"]["code"].as_i64(),
            Some(error_code::INVALID_PARAMS),
            "{level_name}: {refused}"
        );
        let message = refused["error"]["message"].as_str().unwrap_or_default();
        assert_eq!(
            message, "path `/nope` does not exist or is not a directory",
            "{level_name}: the refusal is the validator's one sentence, naming the path"
        );
        client.drain_events(Duration::from_millis(100));
        assert!(
            event_index_from(&client, mark, "context_cleared").is_none()
                && event_index_from(&client, mark, "session_root_changed").is_none(),
            "{level_name}: a refused move must announce nothing: {:?}",
            &client.events()[mark..]
        );
        let turn = client.prompt(&session, "Read the marker file.");
        assert_eq!(
            turn["result"]["stop_reason"].as_str(),
            Some("end_turn"),
            "{level_name}: {turn}"
        );
        let turns = remote_turns(&provider);
        assert!(turns.len() >= turns_seen + 2, "{level_name}");
        let follow_up = &turns[turns_seen + 1];
        assert!(
            follow_up.contains("still standing here"),
            "{level_name}: the root did not move — the relative read resolves \
             under `other`: {follow_up}"
        );
        assert!(
            follow_up.contains("Read the README again."),
            "{level_name}: the conversation was not cleared — the refused move's \
             turn still carries the previous prompt: {follow_up}"
        );
        turns_seen = turns.len();

        // 5. A connection that never attached cannot move the session.
        let mut stranger = daemon.connect();
        let refused = stranger.call(
            "session/set_cwd",
            json!({ "session_id": session, "cwd": ws.repo }),
        );
        assert_eq!(
            refused["error"]["code"].as_i64(),
            Some(error_code::NOT_ATTACHED),
            "{level_name}: watching is not driving: {refused}"
        );
        assert!(
            refused.get("result").is_none(),
            "{level_name}: a refused move reports neither a root nor a count: {refused}"
        );
        drop(stranger);
        // The stranger's refusal moved nothing: the session still stands on
        // `other`, and the owner's answer proves it without another turn.
        let list = client.call("session/list", json!({}));
        let row = list["result"]["sessions"]
            .as_array()
            .and_then(|rows| {
                rows.iter()
                    .find(|row| row["session_id"].as_str() == Some(session.as_str()))
            })
            .cloned()
            .unwrap_or_else(|| panic!("{level_name}: the session vanished: {list}"));
        assert_eq!(
            row["cwd"].as_str(),
            Some(other.to_str().unwrap()),
            "{level_name}: the stored path is the moved-to one: {row}"
        );
    }
}
