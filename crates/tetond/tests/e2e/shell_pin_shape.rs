//! REQ-614 TASK-397 / BUG-214 — the pin's **cause** and its **announcement**,
//! end to end, through the prompt turn.
//!
//! Every test here spawns the real `tetond` binary and drives it over the
//! socket, because the defect this file exists for was invisible to every
//! in-process test the REQ shipped with: `taint.rs` constructs a
//! `TaintingPrivacySink` by hand and proves it records `unknown_shell` and
//! publishes `session_pinned`, while the daemon's prompt turn handed
//! `Egress::new` the bare `EventBus` and never built that sink at all. The only
//! marker on a prompt turn was the backstop arm in `run_prompt_turn`, which
//! cannot see the block's path and so recorded **every** turn-path pin as
//! permanent `boundary_hit` — and published nothing. The 2026-09-05 session
//! (`sess-sphx3g1a`, a typed `/analyze` with no `shell` call anywhere) was
//! pinned for life on its first remote send, `/shell allow` was refused naming
//! a boundary nothing had crossed, and the client printed no pin line.
//!
//! So the claims are made from the outside, on what a client received:
//!
//! | Claim | Test |
//! |---|---|
//! | AC-3 (pin half): an opaque `shell` result pins with `unknown_shell`, liftable, remedy `/shell allow` | [`an_opaque_shell_result_pins_with_unknown_shell_and_says_so`] |
//! | AC-11 (daemon half): `session_pinned` is published once, after the block and **before** the pinned local route | [`an_opaque_shell_result_pins_with_unknown_shell_and_says_so`] |
//! | BR-5: `/shell allow` lifts an `unknown_shell` pin once, and a second lift is a no-op | [`an_opaque_shell_result_pins_with_unknown_shell_and_says_so`] |
//! | AC-1 (benign path): a `Rooted` result pins nothing and the next send leaves | [`a_rooted_shell_result_pins_nothing_and_the_next_send_leaves`] |
//! | BUG-214's shape: a typed user skill pins on its first send, liftably, and says so | [`a_typed_user_skill_pins_liftably_and_is_announced`] |
//!
//! Each remote-touching test also asserts the suite-wide BR-1 egress capture
//! stayed clean.
//!
//! **AC-3's routing half — BUG-215.** The prompt *after* `/shell allow` must
//! leave the machine. Probed on 2026-09-05 and it did not: `RoutePin` honored
//! the lift (the turn was routed remote) but the choke point re-inspected the
//! whole context, the unknown-provenance block was still in it, and the send
//! was blocked a second time and rerouted local — every turn, for the life of
//! the session. Two more claims pin the fix:
//!
//! | Claim | Test |
//! |---|---|
//! | AC-3 (routing half): after `/shell allow` the next prompt's request **leaves**, with no second block | [`after_shell_allow_the_next_prompt_leaves_the_machine`] |
//! | BR-3 after a lift: a boundary read **escalates** the pin to permanent, is announced, and `/shell allow` is refused | [`a_boundary_read_after_a_lift_escalates_the_pin_and_nothing_later_leaves`] |
//!
//! Mutation record (run 2026-09-05): dropping `.with_unknown_lift(..)` from the
//! prompt turn's `Egress::new` reddens the two lift claims above and
//! `taint.rs`'s source scan, and nothing else here; swapping the sink's
//! `mark_escalating` back to `mark` reddens the escalation claim alone. The
//! BUG-214 claims stay green under both, which is the separation the two bugs
//! should have.

use std::time::Duration;

use serde_json::{json, Value};

use crate::harness::{
    assert_no_boundary_bytes, openai_turn, Client, Daemon, DaemonOptions, MockProvider,
    MockResponse, Workspace,
};

const GIB: u64 = 1024 * 1024 * 1024;

/// The content-free sentinel an unknown-provenance block is refused against
/// (`tetond::egress::provenance::UNKNOWN_PROVENANCE_PATH`), spelled here so
/// this binary does not link the daemon crate for one constant.
const UNKNOWN_PROVENANCE_PATH: &str = "<unknown-provenance>";

/// A 16 GiB Apple-Silicon probe **with** a local script, so the daemon has a
/// local tier to reroute a blocked remote turn onto (REQ-544 M-1).
fn probe_16gb_with_local(script: std::path::PathBuf) -> DaemonOptions {
    DaemonOptions::default()
        .env("TETON_PROBE_RAM_BYTES", (16 * GIB).to_string())
        .env("TETON_PROBE_DISK_BYTES", "500000000000")
        .env("TETON_PROBE_GPU", "apple-silicon")
        .script(script)
}

fn provider_block(id: &str, kind: &str, endpoint: &str, model: &str) -> String {
    format!(
        "[[providers]]\nid = \"{id}\"\nkind = \"{kind}\"\nendpoint = \"{endpoint}\"\n\
         model = \"{model}\"\n\n"
    )
}

fn tier_block(tier: &str, provider: &str) -> String {
    format!("[[tiers]]\ntier = \"{tier}\"\nprovider_id = \"{provider}\"\n\n")
}

fn boundary_block(glob: &str, mode: &str) -> String {
    format!("[[boundaries]]\npath_glob = \"{glob}\"\nmode = \"{mode}\"\n\n")
}

/// A local-engine script of plain end-of-turn replies — enough for every
/// reroute and pinned turn a test here drives.
fn local_done_script() -> String {
    [
        "Rerouted locally; done.",
        "Still local; done.",
        "Local again; done.",
    ]
    .join("\n---\n")
}

fn count_route_decided_to(client: &Client, provider: &str) -> usize {
    client
        .events_named("route_decided")
        .iter()
        .filter(|e| e["provider_id"].as_str() == Some(provider))
        .count()
}

/// The one configuration every test here shares: `build` routed to a remote
/// mock, one `local-only` boundary the tests never touch (the point is that
/// they do not have to — REQ-597 makes *some* boundary always present), and a
/// scripted local tier to be pinned onto.
fn config_for(provider: &MockProvider) -> String {
    let mut config = String::new();
    config.push_str(&provider_block(
        "deepseek",
        "openai-compatible",
        &provider.openai_endpoint(),
        "deepseek-chat",
    ));
    config.push_str(&tier_block("build", "deepseek"));
    config.push_str(&boundary_block("secrets/**", "local-only"));
    config
}

/// Index of the first event named `name` for which `pred` holds, searching
/// from `from`.
fn index_of(
    client: &Client,
    from: usize,
    name: &str,
    pred: impl Fn(&Value) -> bool,
) -> Option<usize> {
    client.event_index_from(from, |e| e["event"].as_str() == Some(name) && pred(e))
}

/// **AC-3's pin half, AC-11's daemon half, BR-5's lift — and BUG-214.**
///
/// The remote model runs `sh -c 'echo opaque'`. `sh` is an opaque verb, so
/// REQ-614's classifier answers `Unknown` and the result enters context with
/// unknown provenance; the loop's next remote send is blocked at egress. What
/// this test pins is what the daemon *recorded and said* about that:
///
/// 1. the block names the unknown-provenance sentinel, not a path;
/// 2. exactly one `session_pinned` follows it, with cause `unknown_shell`,
///    `liftable: true`, remedy `/shell allow`, and the local budget;
/// 3. it lands **before** the `route_decided` that moves the turn local — a
///    pin announced below the slow turns it explains is the same failure as
///    not announcing it, one step subtler;
/// 4. `shell/override` reports the pin liftable and lifts it, publishing
///    `session_pin_lifted`; a second lift is acknowledged and publishes nothing.
///
/// Before BUG-214 this test failed at step 2 twice over: no `session_pinned`
/// arrived at all, and `shell/override` answered `lifted_now: false, cause:
/// boundary_hit` for a session that had read no boundary file.
#[test]
fn an_opaque_shell_result_pins_with_unknown_shell_and_says_so() {
    let provider = MockProvider::start(
        vec![MockResponse::ok(openai_turn(
            "Running a quick check.",
            Some(("c1", "shell", r#"{"command":"sh -c 'echo opaque'"}"#)),
            120,
            20,
        ))],
        MockResponse::ok(openai_turn("Should never be reached.", None, 10, 5)),
    );
    let ws = Workspace::new("pin-opaque");
    ws.write_config(&config_for(&provider));
    let script = ws.write_script(&local_done_script());
    let daemon = Daemon::spawn(&ws, probe_16gb_with_local(script));
    let mut client = daemon.connect();
    let session = client.create_session("structured", Some("implement"));

    let first = client.prompt(
        &session,
        "Run a quick shell check and tell me what you see.",
    );
    assert_eq!(
        first["result"]["stop_reason"].as_str(),
        Some("end_turn"),
        "the rerouted-to-local turn should complete cleanly: {first}"
    );
    client.drain_events(Duration::from_millis(300));

    // 1. One block, against the sentinel — the classifier could not prove the
    //    command's reach, and there is no path to name.
    let blocks = client.events_named("privacy_block");
    assert_eq!(
        blocks.len(),
        1,
        "expected exactly one privacy_block: {blocks:?}"
    );
    assert_eq!(blocks[0]["provider_id"].as_str(), Some("deepseek"));
    assert_eq!(
        blocks[0]["path"].as_str(),
        Some(UNKNOWN_PROVENANCE_PATH),
        "an opaque shell result is refused against the content-free sentinel"
    );
    assert_eq!(
        count_route_decided_to(&client, "deepseek"),
        1,
        "the blocked remote provider must not be retried"
    );

    // 2. The announcement, with the liftable cause (BUG-214: this arrived
    //    never, and the recorded cause was `boundary_hit`).
    let pinned = client.events_named("session_pinned");
    assert_eq!(
        pinned.len(),
        1,
        "a pinned session announces itself exactly once; got {pinned:?} among {:?}",
        client.event_names()
    );
    let pinned = pinned[0];
    assert_eq!(
        pinned["cause"].as_str(),
        Some("unknown_shell"),
        "an opaque shell result pins with the liftable cause, not `boundary_hit`: {pinned}"
    );
    assert_eq!(pinned["liftable"].as_bool(), Some(true), "{pinned}");
    assert_eq!(
        pinned["remedy"]["kind"].as_str(),
        Some("command"),
        "{pinned}"
    );
    assert_eq!(
        pinned["remedy"]["command"].as_str(),
        Some("/shell allow"),
        "{pinned}"
    );
    assert!(
        pinned["budget_tokens"].is_u64(),
        "the announcement names what the session dropped to: {pinned}"
    );

    // 3. Ordering (AC-11): the pin precedes the local route it explains. The
    //    sink announces the pin *as* it records it and only then forwards the
    //    block, so `session_pinned` lands one event ahead of `privacy_block`;
    //    the local route is anchored at the block so the `title` duty's own
    //    earlier local route cannot stand in for the reroute.
    let block_at = index_of(&client, 0, "privacy_block", |_| true).expect("the block");
    let pinned_at = index_of(&client, 0, "session_pinned", |_| true).expect("the pin");
    let local_at = index_of(&client, block_at, "route_decided", |e| {
        e["provider_id"].as_str() == Some("local")
    })
    .expect("the reroute after the block");
    assert!(
        pinned_at < local_at,
        "AC-11: `session_pinned` (#{pinned_at}) must precede the pinned local route \
         (#{local_at}): {:?}",
        client.event_names()
    );
    assert!(
        pinned_at + 1 == block_at,
        "the pin is announced with the block that caused it, not later: pin #{pinned_at}, \
         block #{block_at}: {:?}",
        client.event_names()
    );

    // 4. The lift. `shell/override` is the client RPC `/shell allow` sends.
    let lifted = client.call("shell/override", json!({ "session_id": session }));
    assert_eq!(
        lifted["result"]["was_pinned"].as_bool(),
        Some(true),
        "{lifted}"
    );
    assert_eq!(
        lifted["result"]["lifted_now"].as_bool(),
        Some(true),
        "an `unknown_shell` pin lifts on the first `/shell allow`: {lifted}"
    );
    assert_eq!(
        lifted["result"]["cause"].as_str(),
        Some("unknown_shell"),
        "{lifted}"
    );
    client.drain_events(Duration::from_millis(300));
    let lifted_events = client.events_named("session_pin_lifted");
    assert_eq!(
        lifted_events.len(),
        1,
        "the lift is announced once: {lifted_events:?}"
    );
    assert!(
        lifted_events[0]["turns_pinned"].is_u64(),
        "the lift names what the pin cost: {:?}",
        lifted_events[0]
    );

    // BR-5's no-op clause: a second lift is acknowledged and publishes nothing.
    let again = client.call("shell/override", json!({ "session_id": session }));
    assert_eq!(
        again["result"]["was_pinned"].as_bool(),
        Some(true),
        "{again}"
    );
    assert_eq!(
        again["result"]["lifted_now"].as_bool(),
        Some(false),
        "a second `/shell allow` in a lifted session lifts nothing: {again}"
    );
    client.drain_events(Duration::from_millis(200));
    assert_eq!(
        client.events_named("session_pin_lifted").len(),
        1,
        "a second lift must not re-announce"
    );

    assert_no_boundary_bytes();
}

/// **AC-1 — the benign path.** `ls -la` names no file and reads nothing the
/// classifier cannot see, so REQ-614 proves it `Rooted`: the result enters
/// context pinned to nothing, the loop's next send **leaves**, no block is
/// published, and the session is not pinned — a second prompt routes remote.
///
/// This is the control that keeps the test above honest: without it, a daemon
/// that pinned every session on its first shell result would still pass it.
#[test]
fn a_rooted_shell_result_pins_nothing_and_the_next_send_leaves() {
    let provider = MockProvider::start(
        vec![MockResponse::ok(openai_turn(
            "Listing the tree.",
            Some(("c1", "shell", r#"{"command":"ls -la"}"#)),
            120,
            20,
        ))],
        MockResponse::ok(openai_turn("Listed; done.", None, 10, 5)),
    );
    let ws = Workspace::new("pin-rooted");
    ws.write_config(&config_for(&provider));
    let script = ws.write_script(&local_done_script());
    let daemon = Daemon::spawn(&ws, probe_16gb_with_local(script));
    let mut client = daemon.connect();
    let session = client.create_session("structured", Some("implement"));

    let first = client.prompt(&session, "List the repository root.");
    assert_eq!(
        first["result"]["stop_reason"].as_str(),
        Some("end_turn"),
        "{first}"
    );
    client.drain_events(Duration::from_millis(300));

    assert_eq!(
        provider.request_count(),
        2,
        "the tool-call turn and the send carrying its `Rooted` result both reach \
         the provider"
    );
    assert!(
        client.events_named("privacy_block").is_empty(),
        "a `Rooted` result is not blocked: {:?}",
        client.event_names()
    );
    assert!(
        client.events_named("session_pinned").is_empty(),
        "a `Rooted` result pins nothing: {:?}",
        client.event_names()
    );

    let second = client.prompt(&session, "Now summarize what you listed.");
    assert_eq!(
        second["result"]["stop_reason"].as_str(),
        Some("end_turn"),
        "{second}"
    );
    client.drain_events(Duration::from_millis(300));
    assert_eq!(
        count_route_decided_to(&client, "deepseek"),
        2,
        "an unpinned session's second prompt routes remote"
    );
    assert_eq!(
        provider.request_count(),
        3,
        "…and its request leaves the machine"
    );
    assert!(client.events_named("session_pinned").is_empty());

    assert_no_boundary_bytes();
}

/// **BUG-214's own shape.** A typed skill installed under `~/.claude/skills`
/// has no repo-relative identity (REQ-587 ADR-9), so its expansion is seeded
/// unknown and the turn's *first* remote send is blocked — no `shell` tool,
/// no preamble, nothing read. That is by design and unchanged here. What the
/// bug changed is what the daemon recorded and said about it: a pin with the
/// liftable cause, announced.
///
/// The fixture HOME is the daemon's own — `DaemonOptions::env("HOME", …)` —
/// so the skill really is discovered as a **user** skill, on a root the
/// session's repo is not under.
#[test]
fn a_typed_user_skill_pins_liftably_and_is_announced() {
    let provider = MockProvider::start(
        Vec::new(),
        MockResponse::ok(openai_turn("Should never be reached.", None, 10, 5)),
    );
    let ws = Workspace::new("pin-skill");
    ws.write_config(&config_for(&provider));
    let script = ws.write_script(&local_done_script());

    let home = ws.root.join("home");
    let skill_dir = home.join(".claude").join("skills").join("probe");
    std::fs::create_dir_all(&skill_dir).unwrap();
    std::fs::write(
        skill_dir.join("SKILL.md"),
        "---\ndescription: a user skill with no commands\n---\n\nDescribe the repository.\n",
    )
    .unwrap();

    let daemon = Daemon::spawn(
        &ws,
        probe_16gb_with_local(script).env("HOME", home.display().to_string()),
    );
    let mut client = daemon.connect();
    let session = client.create_session("structured", Some("implement"));

    let turn = client.call(
        "session/prompt",
        json!({
            "session_id": session,
            "prompt": [],
            "skill": { "name": "probe", "raw_arguments": "" },
        }),
    );
    assert_eq!(
        turn["result"]["stop_reason"].as_str(),
        Some("end_turn"),
        "the skill turn is served locally after the block: {turn}"
    );
    client.drain_events(Duration::from_millis(300));

    assert!(
        client.saw_event("skill_invoked"),
        "fixture: the typed skill must have expanded: {:?}",
        client.event_names()
    );
    let blocks = client.events_named("privacy_block");
    assert_eq!(blocks.len(), 1, "one block on the first send: {blocks:?}");
    assert_eq!(
        blocks[0]["path"].as_str(),
        Some(UNKNOWN_PROVENANCE_PATH),
        "a user skill's expansion is refused against the sentinel: {:?}",
        blocks[0]
    );
    assert_eq!(
        provider.request_count(),
        0,
        "nothing of the skill turn reached the provider"
    );

    let pinned = client.events_named("session_pinned");
    assert_eq!(
        pinned.len(),
        1,
        "BUG-214: the pin is announced; got {pinned:?} among {:?}",
        client.event_names()
    );
    assert_eq!(
        pinned[0]["cause"].as_str(),
        Some("unknown_shell"),
        "BUG-214: the cause read off the sentinel is the liftable one: {:?}",
        pinned[0]
    );
    assert_eq!(pinned[0]["liftable"].as_bool(), Some(true));

    let lifted = client.call("shell/override", json!({ "session_id": session }));
    assert_eq!(
        lifted["result"]["lifted_now"].as_bool(),
        Some(true),
        "BUG-214: `/shell allow` was refused here with `boundary_hit`: {lifted}"
    );

    assert_no_boundary_bytes();
}

/// Pin a session liftably with one opaque command, then lift it. Returns the
/// live fixture with the session in the lifted state and the mock having
/// served exactly one request.
///
/// `scripted_after` is what the mock answers **after** the pinning turn — the
/// prompt that follows the lift is the first thing to reach it.
fn pinned_and_lifted(
    tag: &str,
    scripted_after: Vec<MockResponse>,
) -> (MockProvider, Workspace, Daemon, Client, String) {
    let mut scripted = vec![MockResponse::ok(openai_turn(
        "Running a quick check.",
        Some(("c1", "shell", r#"{"command":"sh -c 'echo opaque'"}"#)),
        120,
        20,
    ))];
    scripted.extend(scripted_after);
    let provider = MockProvider::start(
        scripted,
        MockResponse::ok(openai_turn("Remote; done.", None, 10, 5)),
    );
    let ws = Workspace::new(tag);
    ws.write_config(&config_for(&provider));
    let script = ws.write_script(&local_done_script());
    let daemon = Daemon::spawn(&ws, probe_16gb_with_local(script));
    let mut client = daemon.connect();
    let session = client.create_session("structured", Some("implement"));

    let first = client.prompt(&session, "Run a quick shell check.");
    assert_eq!(
        first["result"]["stop_reason"].as_str(),
        Some("end_turn"),
        "{first}"
    );
    client.drain_events(Duration::from_millis(300));
    assert_eq!(
        client.events_named("session_pinned")[0]["cause"].as_str(),
        Some("unknown_shell"),
        "fixture: pinned liftably"
    );
    let lifted = client.call("shell/override", json!({ "session_id": session }));
    assert_eq!(
        lifted["result"]["lifted_now"].as_bool(),
        Some(true),
        "fixture: lifted: {lifted}"
    );
    client.drain_events(Duration::from_millis(200));
    assert_eq!(
        provider.request_count(),
        1,
        "fixture: only the pinning turn reached the mock"
    );
    (provider, ws, daemon, client, session)
}

/// **AC-3's routing half (BUG-215).** After `/shell allow`, the next prompt is
/// not merely *routed* remote — its request **leaves**, and no second block is
/// published. The unknown-provenance `shell` result is still in the carried
/// conversation; what the lift asserts is that the daemon may stop treating
/// it as opaque, and the choke point now reads that assertion through the
/// same `RoutePin` the route does.
///
/// Before BUG-215 this test failed at the request count: the second prompt
/// produced a second `privacy_block` against `<unknown-provenance>` and was
/// served locally, with the mock still at one request.
#[test]
fn after_shell_allow_the_next_prompt_leaves_the_machine() {
    let (provider, _ws, _daemon, mut client, session) = pinned_and_lifted("pin-lift", Vec::new());

    let second = client.prompt(&session, "Now say hello.");
    assert_eq!(
        second["result"]["stop_reason"].as_str(),
        Some("end_turn"),
        "{second}"
    );
    client.drain_events(Duration::from_millis(300));

    assert_eq!(
        count_route_decided_to(&client, "deepseek"),
        2,
        "the lifted session is routed remote"
    );
    assert_eq!(
        provider.request_count(),
        2,
        "…and its request leaves: {:?}",
        client.event_names()
    );
    assert_eq!(
        client.events_named("privacy_block").len(),
        1,
        "the carried opaque result is not blocked again after the lift: {:?}",
        client.event_names()
    );
    assert_eq!(
        client.events_named("session_pinned").len(),
        1,
        "nothing re-pinned the lifted session"
    );

    assert_no_boundary_bytes();
}

/// **BR-3 after a lift (BUG-215).** The lift releases *opacity*, never a
/// boundary. A lifted session whose model then reads `secrets/prod.env` is
/// blocked naming that file, **escalated** to the permanent cause — a second
/// `session_pinned`, `boundary_hit`, no remedy — refused a further
/// `/shell allow`, and served locally from then on: nothing later leaves.
///
/// The control is the test above: the same lifted session with no boundary
/// read does leave. Together they say the lift is exactly as wide as the
/// user's assertion and no wider.
#[test]
fn a_boundary_read_after_a_lift_escalates_the_pin_and_nothing_later_leaves() {
    let (provider, _ws, _daemon, mut client, session) = pinned_and_lifted(
        "pin-escalate",
        vec![MockResponse::ok(openai_turn(
            "Reading the production config.",
            Some(("c2", "shell", r#"{"command":"cat secrets/prod.env"}"#)),
            120,
            20,
        ))],
    );

    // The prompt after the lift reaches the mock (the lift held), the model
    // reads the boundary file, and the send carrying it is blocked.
    let second = client.prompt(&session, "Read the production configuration.");
    assert_eq!(
        second["result"]["stop_reason"].as_str(),
        Some("end_turn"),
        "{second}"
    );
    client.drain_events(Duration::from_millis(300));
    assert_eq!(provider.request_count(), 2, "the lifted prompt itself left");

    let blocks = client.events_named("privacy_block");
    assert_eq!(blocks.len(), 2, "one block per pin: {blocks:?}");
    assert_eq!(
        blocks[1]["path"].as_str(),
        Some("secrets/prod.env"),
        "the boundary file is what is named, not the sentinel: {:?}",
        blocks[1]
    );

    // Escalation: announced again, permanently, with no remedy.
    let pinned = client.events_named("session_pinned");
    assert_eq!(
        pinned.len(),
        2,
        "the escalation is a transition and is announced: {pinned:?}"
    );
    assert_eq!(
        pinned[1]["cause"].as_str(),
        Some("boundary_hit"),
        "{:?}",
        pinned[1]
    );
    assert_eq!(pinned[1]["liftable"].as_bool(), Some(false));
    assert_eq!(pinned[1]["remedy"]["kind"].as_str(), Some("none"));

    let refused = client.call("shell/override", json!({ "session_id": session }));
    assert_eq!(
        refused["result"]["was_pinned"].as_bool(),
        Some(true),
        "{refused}"
    );
    assert_eq!(
        refused["result"]["lifted_now"].as_bool(),
        Some(false),
        "no command lifts a boundary hit: {refused}"
    );
    assert_eq!(refused["result"]["cause"].as_str(), Some("boundary_hit"));

    // And the session is local for good: a third prompt neither routes nor
    // sends remote.
    let third = client.prompt(&session, "Summarize what you read.");
    assert_eq!(
        third["result"]["stop_reason"].as_str(),
        Some("end_turn"),
        "{third}"
    );
    client.drain_events(Duration::from_millis(300));
    assert_eq!(
        count_route_decided_to(&client, "deepseek"),
        2,
        "a permanently pinned session is not routed remote again"
    );
    assert_eq!(provider.request_count(), 2, "nothing later leaves");

    assert_no_boundary_bytes();
}
