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
//! | REQ-619 AC-1: a typed **user** skill leaves under the builtins, and pins nothing | [`a_typed_user_skill_leaves_under_the_builtins`] |
//!
//! Each remote-touching test also asserts the suite-wide BR-1 egress capture
//! stayed clean.
//!
//! **REQ-619 flipped one row.** BUG-214's own fixture — a typed user skill —
//! no longer pins at all: a skill file discovered under `~/.claude` mints a
//! `~`-scoped identity, and one matching none of the thirteen builtin globs
//! leaves like a project skill's. The row above says so, and the announcement
//! claims BUG-214 bought — the liftable cause, the ordering, the `/shell allow`
//! remedy — stay where a pin is still genuinely taken: the opaque-`shell` tests
//! here, and `skill_provenance`'s AC-5 and AC-13, which drive them through an
//! opaque **preamble**.
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
//! **REQ-620 adds the redirect pair.** The grammar now lifts a redirect to
//! `/dev/null` and a descriptor duplication before it refuses anything, so the
//! commands a *model* writes stop pinning every session on their first call.
//! Two claims say what that did and did not change:
//!
//! | Claim | Test |
//! |---|---|
//! | AC-10 / BR-9: after a cleared command the next turn routes to the provider, its reason names no pin, and the pin RPC and doctor's config both report an unpinned remote session | [`a_cleared_shell_call_leaves_doctor_and_the_route_on_the_provider`] |
//! | BR-8 / AC-3: the same redirect on `cat secrets/prod.env` still names the file, pins `boundary_hit` permanently, and `/shell allow` does not lift it | [`shell_allow_does_not_lift_a_boundary_hit_behind_a_redirect`] |
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

/// A string that exists only inside the model's `shell` command, so "the pin
/// says nothing the command said" can be asserted rather than reasoned about
/// (LESSON-624's egress-capture posture, applied to an event).
const COMMAND_MARKER: &str = "ZQX9MARKER";

/// **REQ-620 BR-6 / AC-6, at the wire.** The pin names the syntax class that
/// refused the command, and carries no byte of the command itself.
///
/// The model runs `ls 'ZQX9MARKER'`. The quote is unmodelled, so the classifier
/// refuses it as `unknown`, the result enters context opaque, the next remote
/// send is blocked, and the session pins. What is new is the pin's `reason`:
/// before REQ-620 the user was told `cause: unknown_shell` and nothing about
/// which byte to change — the 2026-09-09 session's whole complaint.
///
/// Three claims:
///
/// 1. `session_pinned.reason` is the **quote** class's sentence, verbatim.
/// 2. `session_pinned` for the pin still carries everything BUG-214 bought —
///    the liftable cause, the remedy, the budget — so the field is additive at
///    the wire as well as in the type.
/// 3. The marker appears in **no privacy event**: not on `session_pinned`, not
///    on `privacy_block`, not on `provenance_rejected`. The two events that do
///    carry it are the ones whose whole job is to show the user what the model
///    asked to run — `session_update`'s `tool_call` title (`describe_call`,
///    REQ-611 BR-4) and the `permission_request` the user answers, neither of
///    which could do its job without quoting the command. The assertion names
///    the permitted carriers rather than excluding a list, so a *new* event
///    quoting the command fails here rather than slipping past.
///
/// **Mutation (run 2026-09-09, red, reverted):** have the sink pass `None`
/// instead of the block's reason for an `unknown_shell` cause — claim 1 reds,
/// alone in this binary.
///
/// Claim 3 has no mutation and is recorded as a **ratchet** rather than as one
/// that was run (LESSON-569: do not claim a mutation you did not run). The leak
/// it forbids is unconstructible today — the reason is a `&'static str` from a
/// closed set for its whole journey and is owned only at the wire seam — so the
/// only way to fail it is to widen that type, which is exactly the change this
/// assertion exists to catch.
#[test]
fn a_pin_carries_the_class_that_refused_and_no_command_bytes() {
    const QUOTE_CLASS: &str = "the command uses a quoted string this classifier does not model";

    let provider = MockProvider::start(
        vec![MockResponse::ok(openai_turn(
            "Listing one thing.",
            Some(("c1", "shell", r#"{"command":"ls 'ZQX9MARKER'"}"#)),
            120,
            20,
        ))],
        MockResponse::ok(openai_turn("Should never be reached.", None, 10, 5)),
    );
    let ws = Workspace::new("pin-class");
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

    // 1 + 2. The announcement, with the class beside the cause.
    let pinned = client.events_named("session_pinned");
    assert_eq!(
        pinned.len(),
        1,
        "a pinned session announces itself once: {:?}",
        client.event_names()
    );
    let pinned = pinned[0];
    assert_eq!(pinned["cause"].as_str(), Some("unknown_shell"), "{pinned}");
    assert_eq!(
        pinned["reason"].as_str(),
        Some(QUOTE_CLASS),
        "the pin names the class that refused the command: {pinned}"
    );
    assert_eq!(pinned["liftable"].as_bool(), Some(true), "{pinned}");
    assert_eq!(
        pinned["remedy"]["command"].as_str(),
        Some("/shell allow"),
        "{pinned}"
    );
    assert!(pinned["budget_tokens"].is_u64(), "{pinned}");
    assert_eq!(
        client.events_named("privacy_block")[0]["path"].as_str(),
        Some(UNKNOWN_PROVENANCE_PATH),
        "fixture: the pin came from the unknown-provenance sentinel"
    );

    // 3. Where the command's bytes went, exhaustively. Two surfaces exist to
    //    show the user what the model asked to run; nothing else may quote it.
    const MAY_QUOTE_THE_COMMAND: &[&str] = &["session_update/tool_call", "permission_request/-"];
    let carriers: Vec<String> = client
        .events()
        .iter()
        .filter(|e| e.to_string().contains(COMMAND_MARKER))
        .map(|e| {
            format!(
                "{}/{}",
                e["event"].as_str().unwrap_or("?"),
                e["update"]["kind"].as_str().unwrap_or("-")
            )
        })
        .collect();
    assert!(
        carriers
            .iter()
            .all(|c| MAY_QUOTE_THE_COMMAND.contains(&c.as_str())),
        "only the surfaces that show the user what was asked may quote the \
         command; these also did: {carriers:?}"
    );
    assert!(
        !carriers.is_empty(),
        "fixture: the marker must have reached the daemon at all — an empty \
         carrier list would make this assertion vacuous"
    );
    //    And the privacy chain by name, so the check above cannot go quiet if
    //    the event vocabulary is renamed out from under it.
    for name in ["session_pinned", "privacy_block", "provenance_rejected"] {
        for event in client.events_named(name) {
            assert!(
                !event.to_string().contains(COMMAND_MARKER),
                "`{name}` must carry no byte of the command: {event}"
            );
        }
    }

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

/// **REQ-619 AC-1 — the claim this test used to make, inverted.**
///
/// It was `a_typed_user_skill_pins_liftably_and_is_announced`, and its subject
/// was BUG-214's fixture: a typed skill under `~/.claude/skills` had no
/// repo-relative identity (REQ-587 ADR-9), so its expansion was seeded
/// `unknown`, the turn's *first* remote send was blocked — no `shell` call, no
/// preamble, nothing read — and the session was pinned. The bug was in what the
/// daemon *recorded and said* about that pin, and this file's other tests are
/// the fix for it.
///
/// REQ-619 BR-3 removes the pin itself. A user skill mints a `~`-scoped
/// identity, egress matches it against the boundary globs like any other id,
/// and a file matching none of the thirteen builtins **leaves**. So the fixture
/// is unchanged and the assertions are inverted: one request, no block, no pin.
/// The announcement claims BUG-214 bought stay on the opaque-shell tests above,
/// where a pin is still taken — which is why this test was flipped rather than
/// deleted.
///
/// The fixture HOME is the daemon's own — `DaemonOptions::env("HOME", …)` — so
/// the skill really is discovered as a **user** skill, on a root the session's
/// repository is not under. The boundary-glob half of BR-3 (a user glob over
/// the skills directory refuses the same skill by name) lives in
/// `skill_provenance::a_user_glob_naming_the_skills_directory_refuses_the_skill_by_name`.
#[test]
fn a_typed_user_skill_leaves_under_the_builtins() {
    let provider = MockProvider::start(
        Vec::new(),
        MockResponse::ok(openai_turn("Described; done.", None, 10, 5)),
    );
    let ws = Workspace::new("pin-skill");
    ws.write_config(&config_for(&provider));
    let script = ws.write_script(&local_done_script());

    let home = ws.user_skill(
        "probe",
        "Describe the repository. USER-SKILL-BODY-PINSHAPE\n",
    );

    let daemon = Daemon::spawn(
        &ws,
        probe_16gb_with_local(script).env("HOME", home.display().to_string()),
    );
    let mut client = daemon.connect();
    let session = client.create_session("structured", Some("implement"));

    let turn = client.skill(&session, "probe", "");
    assert_eq!(
        turn["result"]["stop_reason"].as_str(),
        Some("end_turn"),
        "the skill turn completes: {turn}"
    );
    client.drain_events(Duration::from_millis(300));

    assert!(
        client.saw_event("skill_invoked"),
        "fixture: the typed skill must have expanded: {:?}",
        client.event_names()
    );
    assert_eq!(
        provider.request_count(),
        1,
        "REQ-619 BR-3: the expansion carries a `~`-scoped identity that matches \
         no builtin glob, so the send leaves: {:?}",
        client.event_names()
    );
    assert!(
        client.events_named("privacy_block").is_empty(),
        "nothing is refused: {:?}",
        client.event_names()
    );
    assert!(
        client.events_named("session_pinned").is_empty(),
        "BUG-214's pin is gone, not merely announced: {:?}",
        client.event_names()
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
    // REQ-620 BR-6's must-not-fire half: a pin whose cause is a *path* carries
    // no class sentence — the block beside it names the file, and the earlier
    // liftable pin's class does not survive the escalation.
    assert!(
        pinned[1].get("reason").is_none(),
        "a boundary_hit pin says nothing about shell syntax: {:?}",
        pinned[1]
    );

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

// ---------------------------------------------------------------------------
// REQ-620 — a cleared command, and a boundary read the redirect did not hide
// ---------------------------------------------------------------------------

/// The 2026-09-09 shape, reduced to the two redirect forms the transcript
/// tripped on, either side of a `&&` the grammar must still read as a
/// separator.
const CLEARED_COMMAND: &str = r#"{"command":"ls src 2>/dev/null && echo ok 2>&1"}"#;

/// The same redirect on a command that reads the fixture's boundary file.
const BOUNDARY_COMMAND: &str = r#"{"command":"cat secrets/prod.env 2>/dev/null"}"#;

/// The `route_decided` events naming `provider`, in the order the client
/// received them.
fn routes_to<'a>(client: &'a Client, provider: &str) -> Vec<&'a Value> {
    client
        .events_named("route_decided")
        .into_iter()
        .filter(|e| e["provider_id"].as_str() == Some(provider))
        .collect()
}

/// **BR-8's second half, and AC-3 at the wire.** A boundary read wearing
/// `2>/dev/null` pins **permanently**, and `/shell allow` does not lift it.
///
/// The redirect is the only thing REQ-620 changed about this command, and what
/// it changed is nothing: the path still resolves, `privacy_block` still names
/// `secrets/prod.env`, `session_pinned` still carries `boundary_hit` with
/// `liftable: false` and no remedy — and, per BR-6's must-not-fire half, no
/// `reason`, because the cause is a path the block already named.
///
/// This is the must-not-fire twin of
/// [`a_cleared_shell_call_leaves_doctor_and_the_route_on_the_provider`] below:
/// the two commands differ in their path and in nothing else, and they land on
/// opposite sides. Without it, a strip that had swallowed the whole command
/// would pass every claim that test makes.
///
/// **Inversion (run 2026-09-09, red, restored):** with
/// `shell_syntax::strip_null_redirects` made a no-op this test reds at the
/// `privacy_block` path, which comes back `<unknown-provenance>`, and the pin
/// becomes the liftable `unknown_shell` — so `/shell allow` would have lifted
/// it. Note what the mutation does **not** do: nothing leaves under it either,
/// because a strip that lifts nothing can only make the grammar more
/// suspicious. The red is the *report* degrading and the pin becoming liftable,
/// not a leak — which is why the hole-opening direction for BR-8 is the
/// *widening* mutation (lift any word carrying `>` or `<`), recorded with the
/// recogniser in `harness::tools::shell_syntax`.
#[test]
fn shell_allow_does_not_lift_a_boundary_hit_behind_a_redirect() {
    let provider = MockProvider::start(
        vec![MockResponse::ok(openai_turn(
            "Reading the production config.",
            Some(("c1", "shell", BOUNDARY_COMMAND)),
            120,
            20,
        ))],
        MockResponse::ok(openai_turn("Should never be reached.", None, 10, 5)),
    );
    let ws = Workspace::new("pin-redirect-boundary");
    ws.write_config(&config_for(&provider));
    let script = ws.write_script(&local_done_script());
    let daemon = Daemon::spawn(&ws, probe_16gb_with_local(script));
    let mut client = daemon.connect();
    let session = client.create_session("structured", Some("implement"));

    let first = client.prompt(&session, "Read the production configuration.");
    assert_eq!(
        first["result"]["stop_reason"].as_str(),
        Some("end_turn"),
        "{first}"
    );
    client.drain_events(Duration::from_millis(300));

    // The block names the file, not the content-free sentinel: the redirect did
    // not stop the path from resolving.
    let blocks = client.events_named("privacy_block");
    assert_eq!(blocks.len(), 1, "one block: {blocks:?}");
    assert_eq!(
        blocks[0]["path"].as_str(),
        Some("secrets/prod.env"),
        "BR-8: the redirect did not hide the read — the block names the file, \
         not `{UNKNOWN_PROVENANCE_PATH}`: {:?}",
        blocks[0]
    );

    // The pin: permanent, no remedy, and no syntax class.
    let pinned = client.events_named("session_pinned");
    assert_eq!(
        pinned.len(),
        1,
        "the pin is announced once: {:?}",
        client.event_names()
    );
    let pinned = pinned[0];
    assert_eq!(pinned["cause"].as_str(), Some("boundary_hit"), "{pinned}");
    assert_eq!(pinned["liftable"].as_bool(), Some(false), "{pinned}");
    assert_eq!(pinned["remedy"]["kind"].as_str(), Some("none"), "{pinned}");
    assert!(
        pinned.get("reason").is_none(),
        "BR-6: a pin whose cause is a path says nothing about shell syntax: {pinned}"
    );

    // BR-8: `/shell allow` is refused, naming the cause.
    let refused = client.call("shell/override", json!({ "session_id": session }));
    assert_eq!(
        refused["result"]["was_pinned"].as_bool(),
        Some(true),
        "{refused}"
    );
    assert_eq!(
        refused["result"]["lifted_now"].as_bool(),
        Some(false),
        "BR-8: no command lifts a boundary hit, redirect or not: {refused}"
    );
    assert_eq!(
        refused["result"]["cause"].as_str(),
        Some("boundary_hit"),
        "{refused}"
    );
    client.drain_events(Duration::from_millis(200));
    assert!(
        client.events_named("session_pin_lifted").is_empty(),
        "a refused lift announces nothing: {:?}",
        client.event_names()
    );

    // And the session stays local: the prompt after the refused lift is neither
    // routed nor sent remote.
    let second = client.prompt(&session, "Summarize what you read.");
    assert_eq!(
        second["result"]["stop_reason"].as_str(),
        Some("end_turn"),
        "{second}"
    );
    client.drain_events(Duration::from_millis(300));
    assert_eq!(
        count_route_decided_to(&client, "deepseek"),
        1,
        "a permanently pinned session is not routed remote again: {:?}",
        client.event_names()
    );
    assert_eq!(
        provider.request_count(),
        1,
        "…and nothing later leaves: only the tool-call turn ever reached the mock"
    );

    assert_no_boundary_bytes();
}

/// **AC-10, and BR-9's routing half.** After a `shell` call the grammar clears,
/// the session's next turn routes to the **provider**, its `route_decided`
/// reason mentions no pin, and the two surfaces a user would check for one —
/// the pin RPC behind `/shell allow`, and the config `teton doctor` renders —
/// report an unpinned session on the remote provider.
///
/// ## Why the assertion is `config/get` and `shell/override`, not a spawned `teton doctor`
///
/// AC-10 names `teton doctor`. Two facts about that command decide where its
/// claim can honestly be made:
///
/// 1. **Doctor has no session line.** Its report is daemon-scoped —
///    `doctor_header`, the attach line, then `render_config` over the
///    `config/get` snapshot, the transcript and repo-context postures, and the
///    trailer. There is no pin line and no per-session routing line in it, so
///    "doctor's session line shows the provider" describes a surface that does
///    not exist. What doctor *does* show about a provider is
///    `config/get`'s `providers` rows, verbatim — asserted below through the
///    same RPC doctor makes.
/// 2. **The CLI binary is not linkable from here.** `CARGO_BIN_EXE_*` is
///    defined only for binaries of the *package under test*; `teton` lives in
///    another crate, so this suite can spawn `teton-code` and not `teton`. A
///    doctor run belongs to `crates/teton`'s own CLI end-to-end suite.
///
/// So the daemon half of AC-10 is asserted here, on what a client received, and
/// the CLI half is the rendering of these same two answers.
///
/// **Inversion (run 2026-09-09, red, restored):** with
/// `shell_syntax::strip_null_redirects` made a no-op this test reds at the
/// first `privacy_block` assertion — the residue still carries `>`, the command
/// is refused whole on the redirect class, the session pins `unknown_shell`,
/// and every claim below about the second turn's route and request follows it
/// down. Both REQ-620 tests in this file red under that mutation and the six
/// REQ-614/BUG-214/BUG-215/REQ-619 tests above stay green, which is the
/// separation the widening should have.
#[test]
fn a_cleared_shell_call_leaves_doctor_and_the_route_on_the_provider() {
    let provider = MockProvider::start(
        vec![MockResponse::ok(openai_turn(
            "Listing the sources.",
            Some(("c1", "shell", CLEARED_COMMAND)),
            120,
            20,
        ))],
        MockResponse::ok(openai_turn("Listed; done.", None, 10, 5)),
    );
    let ws = Workspace::new("pin-redirect-clear");
    ws.write_config(&config_for(&provider));
    let script = ws.write_script(&local_done_script());
    let daemon = Daemon::spawn(&ws, probe_16gb_with_local(script));
    let mut client = daemon.connect();
    let session = client.create_session("structured", Some("implement"));

    let first = client.prompt(&session, "List the sources and tell me what you see.");
    assert_eq!(
        first["result"]["stop_reason"].as_str(),
        Some("end_turn"),
        "{first}"
    );
    client.drain_events(Duration::from_millis(300));

    assert!(
        client.events_named("privacy_block").is_empty(),
        "AC-4: a command whose only unmodelled bytes are null redirects is not \
         refused: {:?}",
        client.event_names()
    );
    assert!(
        client.events_named("session_pinned").is_empty(),
        "AC-4: and nothing pins: {:?}",
        client.event_names()
    );
    assert_eq!(
        provider.request_count(),
        2,
        "the tool-call turn and the send carrying its result both reach the provider"
    );

    // The turn *after* the cleared command: routed remote, and its request
    // leaves.
    let second = client.prompt(&session, "Now summarize what you listed.");
    assert_eq!(
        second["result"]["stop_reason"].as_str(),
        Some("end_turn"),
        "{second}"
    );
    client.drain_events(Duration::from_millis(300));

    let remote = routes_to(&client, "deepseek");
    assert_eq!(
        remote.len(),
        2,
        "AC-10: the turn after the cleared command routes to the provider: {:?}",
        client.event_names()
    );
    assert_eq!(
        provider.request_count(),
        3,
        "…and its request leaves the machine"
    );
    // AC-10's reason half. Both sentences the daemon composes for a pinned
    // route contain "pinned to the local tier", so the substring is what
    // separates a clean route from a pinned one.
    let reason = remote[1]["reason"].as_str().unwrap_or_default();
    assert!(
        !reason.contains("pin"),
        "AC-10: the route after a cleared command explains itself without a \
         pin: {reason:?}"
    );
    assert!(
        client.events_named("session_pinned").is_empty(),
        "AC-10: still nothing pinned after the second turn: {:?}",
        client.event_names()
    );

    // The pin-facing RPC — what `/shell allow` calls and what the CLI renders
    // when a user asks whether this session is pinned.
    let pin_state = client.call("shell/override", json!({ "session_id": session }));
    assert_eq!(
        pin_state["result"]["was_pinned"].as_bool(),
        Some(false),
        "AC-10: nothing to lift, because nothing pinned: {pin_state}"
    );
    assert!(
        pin_state["result"]["cause"].is_null(),
        "…and no cause to name: {pin_state}"
    );

    // The doctor half: the providers doctor renders come off this snapshot.
    let snapshot = client.call("config/get", json!({}));
    let providers = snapshot["result"]["snapshot"]["providers"]
        .as_array()
        .expect("the snapshot lists providers");
    let deepseek = providers
        .iter()
        .find(|p| p["id"].as_str() == Some("deepseek"))
        .unwrap_or_else(|| panic!("doctor's provider list must name the remote route: {snapshot}"));
    assert_eq!(
        deepseek["model"].as_str(),
        Some("deepseek-chat"),
        "AC-10: doctor reports the remote provider this session is served by: {deepseek}"
    );

    assert_no_boundary_bytes();
}
