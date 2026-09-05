//! REQ-619 — **proportionate skill provenance**, end to end through the daemon.
//!
//! Two rules written when most machines had no privacy boundary became far
//! stricter than intended the moment REQ-597 put thirteen builtin `local-only`
//! globs permanently in force: a skill's `` !`cmd` `` preamble marked the whole
//! expansion unknown the instant *any* command spawned (REQ-585 BR-7), and a
//! skill under `~/.claude` had no identity at all, so **every** user-authored
//! skill pinned **every** repo-rooted session on **every** machine (BUG-214).
//! REQ-619 moves both onto the model REQ-614 gave `shell`: prove what can be
//! proved, fail closed on the rest.
//!
//! Every claim here is an **egress-capture** claim — asserted on the bytes a
//! mock provider received, or on their **absence counted** — driven through the
//! real `tetond` binary over the socket, because the in-process seams were
//! green while the daemon disagreed (LESSON-649, LESSON-550). The leak marker
//! lives only in `secrets/prod.env` (LESSON-624); the skill bodies here carry
//! `USER-SKILL-BODY-*` markers, which are ordinary prose and are *supposed* to
//! reach the wire on the claims that say they do.
//!
//! | AC | Claim | Test |
//! |---|---|---|
//! | AC-1 | a preamble-free user skill leaves, and the session stays remote | [`a_user_skill_with_no_preambles_leaves_under_the_builtins`] |
//! | AC-2 | the model's `skill` door reaches the same answer | [`a_model_invoked_user_skill_leaves_too`] |
//! | AC-3 | `cat`/`ls` preambles leave, carrying their output | [`rooted_preambles_leave_with_their_output`] |
//! | AC-4 | a boundary-reading preamble pins permanently; nothing later leaves | [`a_boundary_reading_preamble_pins_permanently_and_nothing_later_leaves`] |
//! | AC-5, BR-8 | an opaque preamble pins liftably; `/shell allow` restores routing | [`an_opaque_preamble_pins_liftably_and_shell_allow_restores_routing`] |
//! | AC-6 | the exit-code side channel is closed by the *verdict* | [`the_exit_code_channel_is_closed_by_the_verdict`] |
//! | AC-7 | a glob over the skills directory refuses the skill **by name** | [`a_user_glob_naming_the_skills_directory_refuses_the_skill_by_name`] |
//! | AC-8 | the `~`-scoped id survives the carry and a second client's attach | [`a_user_skills_identity_survives_compaction_and_reattach`] |
//! | AC-9 | a `read` of the skill file is still refused by the jail | [`a_read_of_a_user_skill_file_is_still_refused_by_the_jail`] |
//! | AC-10, BR-9 | with no boundaries at all, an opaque preamble is sent | [`with_no_boundaries_an_opaque_preamble_is_sent_and_nothing_pins`] |
//! | AC-11 | project skills gain the classification and nothing else | [`a_project_skill_leaves_with_a_rooted_preamble_and_is_refused_with_a_boundary_one`] |
//! | AC-12 | `skill_invoked` carries each command's reach — and no output | [`skill_invoked_carries_each_commands_reach_and_nothing_more`] |
//! | AC-13 | BUG-214's own shape: the `sh` alone pins, liftably | [`the_bug_214_shape_pins_liftably_from_the_sh_alone`] |
//! | verify C1 | an opaque **and** boundary-reading expansion keeps the file across `/shell allow` | [`a_model_invoked_skill_with_an_opaque_and_a_boundary_preamble_keeps_the_file_after_a_lift`] |
//! | verify C2 | an out-of-root touch is not cancelled by an in-root file beside it | [`a_preamble_touching_a_boundary_outside_the_root_beside_an_in_root_file_is_refused`] |
//! | verify m1 | the same pair on the **typed** door, pinning BUG-216's reported path | [`a_preamble_that_is_both_opaque_and_boundary_reading_pins_and_survives_the_lift`] |
//!
//! # What AC-8 proves, and what it does not
//!
//! AC-8 names two round trips, "compaction and a client re-attach". This file
//! proves the **carry-and-reattach** half in full: the expansion is committed on
//! one connection, a boundary glob over the skills directory is added
//! mid-session through `config/set`, a **second** client attaches to the live
//! session and prompts, and the carried block is refused **naming the skill
//! file** — which is only possible if the `~`-scoped id survived the seed, the
//! conversation carry and the attach. That is the discriminating half: a build
//! that lost the id would refuse against `<unknown-provenance>` instead, and one
//! that dropped the block would not refuse at all.
//!
//! The **compaction** half is not proved here and is not cheap to prove here:
//! the daemon has no `session/compact` RPC, compaction is driven by window
//! pressure inside the turn assembler, and forcing it from a client would mean
//! sizing a fixture against a provider window — the shape LESSON-640 warns
//! about, where the arithmetic has to be asserted before the interesting
//! assertion means anything. The replay/compaction seam is asserted in process
//! instead (ADR-619-2's "three seams, three tests" — the seed, the union and
//! replay each carry `boundary_touch` and the id), and this file states plainly
//! that it did not re-prove it end to end.
//!
//! # Mutation record (run 2026-09-05, both reverted, suite re-run green)
//!
//! **Mutation 1 — no preamble classification.** Return an `Unknown` verdict
//! from `skills::dynamic::preamble_verdict` before it calls `classify`, which
//! is the pre-REQ-619 answer for every preamble. **6 red of 13:**
//!
//! - [`rooted_preambles_leave_with_their_output`] — `["unknown", "unknown"]`
//!   where `["rooted", "rooted"]` was expected;
//! - [`a_boundary_reading_preamble_pins_permanently_and_nothing_later_leaves`] —
//!   the block names `<unknown-provenance>` instead of `secrets/prod.env`, so
//!   the user is told a boundary was not crossed when one was;
//! - [`the_exit_code_channel_is_closed_by_the_verdict`] — the same, one step
//!   earlier: the verdict is `unknown`, not `boundary_touch`;
//! - [`a_project_skill_leaves_with_a_rooted_preamble_and_is_refused_with_a_boundary_one`]
//!   — the rooted leg's request never leaves (0, not 1);
//! - [`skill_invoked_carries_each_commands_reach_and_nothing_more`] —
//!   `["unknown", "unknown"]` for `["rooted", "unknown"]`;
//! - [`the_bug_214_shape_pins_liftably_from_the_sh_alone`] —
//!   `["unknown", "unknown", "unknown"]`: the `sh` no longer stands out, which
//!   is BUG-214's own symptom.
//!
//! **7 green under it, and that is the finding**: every claim whose subject is
//! the *identity* — AC-1, AC-2, AC-7, AC-8, AC-9 — plus AC-5, whose preamble is
//! opaque either way, and AC-10, which configures no boundary for a verdict to
//! matter against. The two rules this REQ changes are genuinely separable, and
//! the suite separates them.
//!
//! **Mutation 2 — no user-skill identity.** Make
//! `skills::discovery::provenance_of_with_home` answer `None` for
//! `SkillSource::User`, which is the pre-REQ-619 daemon. **7 red of 13:** the
//! five predicted — [`a_user_skill_with_no_preambles_leaves_under_the_builtins`],
//! [`a_model_invoked_user_skill_leaves_too`],
//! [`rooted_preambles_leave_with_their_output`],
//! [`a_user_glob_naming_the_skills_directory_refuses_the_skill_by_name`] and
//! [`a_user_skills_identity_survives_compaction_and_reattach`] — **and two that
//! were predicted green**:
//! [`a_boundary_reading_preamble_pins_permanently_and_nothing_later_leaves`] and
//! [`the_exit_code_channel_is_closed_by_the_verdict`], both of which reported
//! `<unknown-provenance>` / `unknown_shell` where they expect
//! `secrets/prod.env` / `boundary_hit`.
//!
//! That miss is worth writing down, because the reasoning behind the prediction
//! was wrong in an instructive way. `ExpansionProvenance::into_tool_provenance`
//! ranks `boundary_touch` above `unknown`, so "a boundary touch outranks the
//! unknown" looked like it settled the question. It does not: an **in-root**
//! boundary path never sets `boundary_touch` at all — it mints an id and lands
//! in `sources` (ADR-619-4's table) — so with the identity gone the expansion is
//! `sources ∪ {secrets/prod.env}` **with `unknown` set**, and `unknown` is what
//! the block reports. The consequence for a user is real and is the reason the
//! two ACs belong in this list: without the identity, a session that read a
//! protected file is pinned *liftably*, and `/shell allow` would release it.
//!
//! **6 green under it**: AC-5 and AC-13 (already `unknown_shell`, so nothing
//! moves), AC-9 (the jail never consulted the identity — BR-4's whole point),
//! AC-10 (no boundaries), AC-11 (a **project** skill's identity is untouched,
//! which is the control saying the two scopes are separate) and AC-12 (the
//! `reach` words are the classifier's, not the minter's).

use std::time::Duration;

use serde_json::{json, Value};

use crate::harness::{
    assert_no_boundary_bytes, openai_turn, Client, Daemon, DaemonOptions, MockProvider,
    MockResponse, Workspace,
};

const GIB: u64 = 1024 * 1024 * 1024;

/// The content-free sentinel an unknown-provenance block is refused against
/// (`tetond::egress::provenance::UNKNOWN_PROVENANCE_PATH`), spelled here so this
/// binary does not link the daemon crate for one constant.
const UNKNOWN_PROVENANCE_PATH: &str = "<unknown-provenance>";

/// A line of the fixture repository's `README.md`, verbatim.
///
/// The instrument for AC-3: a `cat README.md` preamble that reached the wire
/// put *these bytes* there. Asserting on the file's own content rather than on
/// a marker planted for the test is what makes the claim "the expansion carries
/// the output" rather than "the daemon echoed something".
const README_LINE: &str = "A tiny fixture repo the Teton Code acceptance suite drives a session";

/// A 16 GiB Apple-Silicon probe **with** a local script, so a pinned turn has a
/// local tier to be rerouted onto (REQ-544 M-1).
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

fn boundary_block(glob: &str) -> String {
    format!("[[boundaries]]\npath_glob = \"{glob}\"\nmode = \"local-only\"\n\n")
}

/// A local-engine script of plain end-of-turn replies — enough for every reroute
/// and pinned turn here.
fn local_done_script() -> String {
    [
        "Rerouted locally; done.",
        "Still local; done.",
        "Local again; done.",
        "Local once more; done.",
    ]
    .join("\n---\n")
}

/// The configuration every test here shares unless it says otherwise: `build`
/// routed to the remote mock, the **thirteen builtin globs left in force**
/// (REQ-597 — the posture the REQ is about), and one `secrets/**` row so the
/// fixture repository's boundary file is protected by a glob the tests can name.
///
/// `extra_globs` is what a claim adds on top — AC-7 and AC-8's
/// `**/.claude/skills/**`, and nothing else.
fn config_for(provider: &MockProvider, extra_globs: &[&str]) -> String {
    let mut config = String::new();
    config.push_str(&provider_block(
        "deepseek",
        "openai-compatible",
        &provider.openai_endpoint(),
        "deepseek-chat",
    ));
    config.push_str(&tier_block("build", "deepseek"));
    config.push_str(&boundary_block("secrets/**"));
    for glob in extra_globs {
        config.push_str(&boundary_block(glob));
    }
    config
}

fn count_route_decided_to(client: &Client, provider: &str) -> usize {
    client
        .events_named("route_decided")
        .iter()
        .filter(|e| e["provider_id"].as_str() == Some(provider))
        .count()
}

/// Every captured request body of `provider`, as UTF-8 text.
fn bodies(provider: &MockProvider) -> Vec<String> {
    provider
        .requests()
        .iter()
        .map(|b| String::from_utf8_lossy(b).into_owned())
        .collect()
}

/// Whether any captured request body contains `needle`.
fn any_body_contains(provider: &MockProvider, needle: &str) -> bool {
    bodies(provider).iter().any(|b| b.contains(needle))
}

/// A mock that answers every request with a plain end-of-turn reply, after
/// serving `scripted` in order.
fn mock(scripted: Vec<MockResponse>) -> MockProvider {
    MockProvider::start(
        scripted,
        MockResponse::ok(openai_turn("Remote; done.", None, 10, 5)),
    )
}

/// The whole standing fixture for a **user**-skill claim: a workspace, a config,
/// a scripted local tier, a daemon with the fixture HOME, and a live session.
///
/// Returned as a tuple rather than a struct so the workspace and the daemon stay
/// bound in the caller (dropping either tears down the temp tree and the
/// process, and a `_ws` binding is the readable way to say so).
fn user_skill_fixture(
    tag: &str,
    provider: &MockProvider,
    extra_globs: &[&str],
    skills: &[(&str, &str)],
) -> (Workspace, Daemon, Client, String) {
    let ws = Workspace::new(tag);
    ws.write_config(&config_for(provider, extra_globs));
    let script = ws.write_script(&local_done_script());
    let home = ws.root.join("home");
    for (name, body) in skills {
        assert_eq!(
            ws.user_skill(name, body),
            home,
            "one fixture home per workspace"
        );
    }
    let daemon = Daemon::spawn(
        &ws,
        probe_16gb_with_local(script).env("HOME", home.display().to_string()),
    );
    let mut client = daemon.connect();
    let session = client.create_session("structured", Some("implement"));
    (ws, daemon, client, session)
}

/// Index of the first event named `name` for which `pred` holds, from `from`.
fn index_of(
    client: &Client,
    from: usize,
    name: &str,
    pred: impl Fn(&Value) -> bool,
) -> Option<usize> {
    client.event_index_from(from, |e| e["event"].as_str() == Some(name) && pred(e))
}

/// The single `skill_invoked` record this client saw.
fn one_invocation(client: &Client) -> &Value {
    let seen = client.events_named("skill_invoked");
    assert_eq!(
        seen.len(),
        1,
        "expected exactly one skill_invoked among {:?}",
        client.event_names()
    );
    seen[0]
}

/// The `reach` word of each command in a `skill_invoked` record, in order.
fn reaches(invocation: &Value) -> Vec<&str> {
    invocation["outcomes"]
        .as_array()
        .unwrap_or_else(|| panic!("skill_invoked carries an outcome list: {invocation}"))
        .iter()
        .map(|o| {
            o["reach"]
                .as_str()
                .unwrap_or_else(|| panic!("every outcome carries a reach: {invocation}"))
        })
        .collect()
}

// ---------------------------------------------------------------------------
// AC-1 / AC-2 — the identity, on both doors
// ---------------------------------------------------------------------------

/// **AC-1 (BR-3).** A user skill with no preambles and a body that names no
/// file: the typed turn's request **leaves**, carrying the expansion; nothing is
/// blocked, nothing is pinned, and a second prompt on the same session routes
/// remote and sends.
///
/// This is the claim BUG-214 is the absence of. Before REQ-619 this very
/// fixture — the shipped builtin globs, a skill file matching none of them,
/// no `shell` call, no preamble, nothing read — was refused on its first send
/// and the session was pinned for life. `shell_pin_shape` asserted that
/// behaviour and now asserts this one.
///
/// The marker is ordinary prose in the body: it is *supposed* to reach the wire,
/// which is what makes the presence assertion a statement about the expansion
/// rather than about the prompt (LESSON-624 — the leak marker stays in
/// `secrets/prod.env`, where nothing here reads it).
#[test]
fn a_user_skill_with_no_preambles_leaves_under_the_builtins() {
    let provider = mock(Vec::new());
    let (_ws, _daemon, mut client, session) = user_skill_fixture(
        "sp-ac1",
        &provider,
        &[],
        &[("probe", "Describe the repository. USER-SKILL-BODY-AC1\n")],
    );

    let turn = client.skill(&session, "probe", "");
    assert_eq!(
        turn["result"]["stop_reason"].as_str(),
        Some("end_turn"),
        "the skill turn must complete: {turn}"
    );
    client.drain_events(Duration::from_millis(300));

    assert!(
        client.saw_event("skill_invoked"),
        "fixture: the typed user skill must have expanded: {:?}",
        client.event_names()
    );
    assert_eq!(
        provider.request_count(),
        1,
        "the skill turn's request leaves the machine: {:?}",
        client.event_names()
    );
    assert!(
        any_body_contains(&provider, "USER-SKILL-BODY-AC1"),
        "…carrying the expansion itself, not an empty turn"
    );
    assert!(
        client.events_named("privacy_block").is_empty(),
        "a user skill whose file matches no glob is not refused: {:?}",
        client.event_names()
    );
    assert!(
        client.events_named("session_pinned").is_empty(),
        "BUG-214: and it does not pin the session: {:?}",
        client.event_names()
    );

    // The session is not merely un-refused once — it is un-pinned, so the next
    // ordinary prompt is routed remote and its bytes leave too.
    let second = client.prompt(&session, "Now summarize what you described.");
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
    assert_eq!(provider.request_count(), 2, "…and its request leaves");
    assert!(client.events_named("session_pinned").is_empty());

    assert_no_boundary_bytes();
}

/// **AC-2 (BR-6).** The same skill, reached through the model's `skill` tool
/// rather than typed: the tool result enters the loop, the follow-up send
/// **leaves**, and nothing pins.
///
/// One rule, two doors. REQ-587 BR-10 made the model's door stricter than a
/// `read` for exactly the user-skill case; this asserts it is not any more. The
/// discriminator against AC-1 is the request count: two, because the turn that
/// *called* the tool also left, and the second body is the one carrying the
/// expansion.
#[test]
fn a_model_invoked_user_skill_leaves_too() {
    let provider = mock(vec![MockResponse::ok(openai_turn(
        "Reaching for the probe skill.",
        Some(("c1", "skill", r#"{"name":"probe"}"#)),
        120,
        20,
    ))]);
    let (_ws, _daemon, mut client, session) = user_skill_fixture(
        "sp-ac2",
        &provider,
        &[],
        &[("probe", "Describe the repository. USER-SKILL-BODY-AC2\n")],
    );

    let turn = client.prompt(&session, "Use whichever skill fits, then answer.");
    assert_eq!(
        turn["result"]["stop_reason"].as_str(),
        Some("end_turn"),
        "{turn}"
    );
    client.drain_events(Duration::from_millis(300));

    assert!(
        client.saw_event("skill_invoked"),
        "fixture: the model's `skill` call must have expanded something: {:?}",
        client.event_names()
    );
    assert_eq!(
        provider.request_count(),
        2,
        "the tool-call turn and the send carrying the expansion both leave: {:?}",
        client.event_names()
    );
    assert!(
        any_body_contains(&provider, "USER-SKILL-BODY-AC2"),
        "the expansion reached the wire"
    );
    assert!(
        client.events_named("privacy_block").is_empty(),
        "{:?}",
        client.event_names()
    );
    assert!(
        client.events_named("session_pinned").is_empty(),
        "the model's door pins no more than the typed one: {:?}",
        client.event_names()
    );

    assert_no_boundary_bytes();
}

// ---------------------------------------------------------------------------
// AC-3 / AC-4 / AC-5 / AC-6 — the preamble verdict
// ---------------------------------------------------------------------------

/// **AC-3 (BR-1, BR-2).** Two preambles the classifier can prove — `cat
/// README.md` and `ls -la` of the session root — leave, and the expansion
/// carries their output.
///
/// Before REQ-619 the *spawn* was the whole rule, so this turn was refused and
/// the session pinned; since REQ-614 the same two commands typed by the model
/// through `shell` were provably rooted, and the disagreement between the two
/// paths is what this asserts is gone. The presence assertion is on `README.md`'s
/// own bytes rather than on a planted marker, so it cannot pass by the daemon
/// echoing the command back.
#[test]
fn rooted_preambles_leave_with_their_output() {
    let provider = mock(Vec::new());
    let (_ws, _daemon, mut client, session) = user_skill_fixture(
        "sp-ac3",
        &provider,
        &[],
        &[(
            "probe",
            "USER-SKILL-BODY-AC3\nOne: !`cat README.md`\nTwo: !`ls -la`\n",
        )],
    );

    let turn = client.skill(&session, "probe", "");
    assert_eq!(
        turn["result"]["stop_reason"].as_str(),
        Some("end_turn"),
        "{turn}"
    );
    client.drain_events(Duration::from_millis(300));

    assert_eq!(
        reaches(one_invocation(&client)),
        vec!["rooted", "rooted"],
        "fixture: both preambles must have been classified rooted"
    );
    assert_eq!(
        provider.request_count(),
        1,
        "a rooted preamble does not hold the send: {:?}",
        client.event_names()
    );
    assert!(
        any_body_contains(&provider, README_LINE),
        "the expansion carries the preamble's output, not a placeholder: {:?}",
        bodies(&provider).first().map(|b| b.len())
    );
    assert!(
        client.events_named("privacy_block").is_empty(),
        "{:?}",
        client.event_names()
    );
    assert!(
        client.events_named("session_pinned").is_empty(),
        "REQ-585 BR-7 pinned here; REQ-619 does not: {:?}",
        client.event_names()
    );

    assert_no_boundary_bytes();
}

/// **AC-4 (BR-2).** A preamble that names a boundary file is refused **naming
/// that file**, pins the session permanently, is refused a `/shell allow`, and
/// nothing later leaves the machine.
///
/// The command genuinely runs — the verdict is taken before the spawn but does
/// not cancel it — so the secret really is in the expansion the choke point
/// refuses. That is the point: the absence is asserted by counting captured
/// requests (LESSON-550) and by the suite-wide capture assertion, not by
/// arranging for the bytes never to exist.
#[test]
fn a_boundary_reading_preamble_pins_permanently_and_nothing_later_leaves() {
    let provider = mock(Vec::new());
    let (_ws, _daemon, mut client, session) = user_skill_fixture(
        "sp-ac4",
        &provider,
        &[],
        &[(
            "probe",
            "USER-SKILL-BODY-AC4\nEnv: !`cat secrets/prod.env`\n",
        )],
    );

    let turn = client.skill(&session, "probe", "");
    assert_eq!(
        turn["result"]["stop_reason"].as_str(),
        Some("end_turn"),
        "the refused turn is served locally: {turn}"
    );
    client.drain_events(Duration::from_millis(300));

    let blocks = client.events_named("privacy_block");
    assert_eq!(blocks.len(), 1, "one block: {blocks:?}");
    assert_eq!(
        blocks[0]["path"].as_str(),
        Some("secrets/prod.env"),
        "an in-root boundary path mints an id, so the block names the file \
         rather than a sentinel: {:?}",
        blocks[0]
    );

    let pinned = client.events_named("session_pinned");
    assert_eq!(pinned.len(), 1, "announced once: {pinned:?}");
    assert_eq!(
        pinned[0]["cause"].as_str(),
        Some("boundary_hit"),
        "a boundary touch is the permanent cause: {:?}",
        pinned[0]
    );
    assert_eq!(pinned[0]["liftable"].as_bool(), Some(false));

    let refused = client.call("shell/override", json!({ "session_id": session }));
    assert_eq!(refused["result"]["was_pinned"].as_bool(), Some(true));
    assert_eq!(
        refused["result"]["lifted_now"].as_bool(),
        Some(false),
        "no command lifts a boundary hit: {refused}"
    );
    assert_eq!(refused["result"]["cause"].as_str(), Some("boundary_hit"));

    // Nothing later leaves — counted, because an absence asserted any other way
    // is an absence that comes back (LESSON-550).
    assert_eq!(provider.request_count(), 0, "the skill turn sent nothing");
    // The pinning turn was itself routed remote before the choke point refused
    // it, so the claim is that the count does not **grow**: a permanently
    // pinned session is not routed remote again.
    let routed_when_pinned = count_route_decided_to(&client, "deepseek");
    assert_eq!(
        routed_when_pinned, 1,
        "fixture: the refused turn was the remote one"
    );
    let later = client.prompt(&session, "Summarize what you found.");
    assert_eq!(
        later["result"]["stop_reason"].as_str(),
        Some("end_turn"),
        "{later}"
    );
    client.drain_events(Duration::from_millis(300));
    assert_eq!(
        count_route_decided_to(&client, "deepseek"),
        routed_when_pinned,
        "a permanently pinned session is not routed remote again"
    );
    assert_eq!(provider.request_count(), 0, "and nothing later leaves");

    assert_no_boundary_bytes();
}

/// **AC-5, BR-8 (BR-2, BUG-215).** A preamble the classifier cannot prove —
/// `sh -c 'echo x'`, an opaque verb *and* a quoted line — is refused against the
/// content-free sentinel, pins the session **liftably** with the `/shell allow`
/// remedy, and after the lift the next prompt's request leaves with no second
/// block.
///
/// The announcement claims are REQ-614 BR-7's and BUG-214's, unchanged by this
/// REQ (ADR-619-6): no new cause, no new event, no new command.
#[test]
fn an_opaque_preamble_pins_liftably_and_shell_allow_restores_routing() {
    let provider = mock(Vec::new());
    let (_ws, _daemon, mut client, session) = user_skill_fixture(
        "sp-ac5",
        &provider,
        &[],
        &[("probe", "USER-SKILL-BODY-AC5\nOut: !`sh -c 'echo x'`\n")],
    );

    let turn = client.skill(&session, "probe", "");
    assert_eq!(
        turn["result"]["stop_reason"].as_str(),
        Some("end_turn"),
        "{turn}"
    );
    client.drain_events(Duration::from_millis(300));

    assert_eq!(
        reaches(one_invocation(&client)),
        vec!["unknown"],
        "fixture: `sh -c` is opaque"
    );
    let blocks = client.events_named("privacy_block");
    assert_eq!(blocks.len(), 1, "{blocks:?}");
    assert_eq!(
        blocks[0]["path"].as_str(),
        Some(UNKNOWN_PROVENANCE_PATH),
        "nothing was named, so there is no path to name: {:?}",
        blocks[0]
    );
    assert_eq!(provider.request_count(), 0, "the pinned turn sent nothing");

    let pinned = client.events_named("session_pinned");
    assert_eq!(pinned.len(), 1, "{pinned:?}");
    assert_eq!(
        pinned[0]["cause"].as_str(),
        Some("unknown_shell"),
        "an unprovable preamble pins liftably: {:?}",
        pinned[0]
    );
    assert_eq!(pinned[0]["liftable"].as_bool(), Some(true));
    assert_eq!(
        pinned[0]["remedy"]["command"].as_str(),
        Some("/shell allow"),
        "BR-8: the remedy is REQ-614's, verbatim: {:?}",
        pinned[0]
    );

    let lifted = client.call("shell/override", json!({ "session_id": session }));
    assert_eq!(
        lifted["result"]["lifted_now"].as_bool(),
        Some(true),
        "{lifted}"
    );

    let after = client.prompt(&session, "Now say hello.");
    assert_eq!(
        after["result"]["stop_reason"].as_str(),
        Some("end_turn"),
        "{after}"
    );
    client.drain_events(Duration::from_millis(300));
    assert_eq!(
        provider.request_count(),
        1,
        "BUG-215: after the lift the next prompt's request leaves: {:?}",
        client.event_names()
    );
    assert_eq!(
        client.events_named("privacy_block").len(),
        1,
        "the carried expansion is not blocked a second time: {:?}",
        client.event_names()
    );

    assert_no_boundary_bytes();
}

/// **AC-6 (BR-1, BR-2).** The exit-code side channel REQ-585's verify found —
/// `` !`grep -q <pattern> secrets/prod.env && exit 1 || exit 2` `` — is closed by
/// the **verdict**, not by asking whether anything printed.
///
/// A content-reading verb given a boundary path is `boundary_touch` before it
/// spawns, so the turn is refused whatever the command chose to exit with, and
/// the placeholder the fold writes into the prompt (`exited 1` / `exited 2`)
/// never reaches the mock. The absence is the request count: **zero**.
///
/// The pattern is `nothing-here`, deliberately not the file's own marker
/// (LESSON-624): a grep pattern is echoed into the conversation legitimately,
/// so the sentinel written there would fail the suite-wide capture assertion on
/// a daemon that was behaving perfectly.
#[test]
fn the_exit_code_channel_is_closed_by_the_verdict() {
    let provider = mock(Vec::new());
    let (_ws, _daemon, mut client, session) = user_skill_fixture(
        "sp-ac6",
        &provider,
        &[],
        &[(
            "probe",
            "USER-SKILL-BODY-AC6\n\
             Probe: !`grep -q nothing-here secrets/prod.env && exit 1 || exit 2`\n",
        )],
    );

    let turn = client.skill(&session, "probe", "");
    assert_eq!(
        turn["result"]["stop_reason"].as_str(),
        Some("end_turn"),
        "{turn}"
    );
    client.drain_events(Duration::from_millis(300));

    assert_eq!(
        reaches(one_invocation(&client)),
        vec!["boundary_touch"],
        "the verdict is taken from the path argument, not from the exit status"
    );
    let pinned = client.events_named("session_pinned");
    assert_eq!(pinned.len(), 1, "{pinned:?}");
    assert_eq!(
        pinned[0]["cause"].as_str(),
        Some("boundary_hit"),
        "the exit code decided nothing: {:?}",
        pinned[0]
    );
    assert_eq!(
        client.events_named("privacy_block")[0]["path"].as_str(),
        Some("secrets/prod.env"),
    );
    assert_eq!(
        provider.request_count(),
        0,
        "neither the placeholder nor the exit status reached the wire: {:?}",
        client.event_names()
    );

    assert_no_boundary_bytes();
}

// ---------------------------------------------------------------------------
// AC-7 / AC-8 / AC-9 — the identity's edges
// ---------------------------------------------------------------------------

/// **AC-7 (BR-3, BR-4).** A user who writes a boundary glob over their own
/// skills directory gets what they asked for: the skill is refused **naming its
/// own file**, `~/.claude/skills/probe/SKILL.md`, and the session pins
/// permanently — the same treatment a `read` of a protected project file gets.
///
/// The control is the second daemon: the same skill, the same body, the same
/// builtin set, with the one glob absent — and it leaves. Without it this would
/// pass on a build that refused every user skill, which is the build REQ-619
/// exists to retire.
#[test]
fn a_user_glob_naming_the_skills_directory_refuses_the_skill_by_name() {
    // Leg one: the glob is configured, and the file is named.
    let guarded = mock(Vec::new());
    let (_gws, _gdaemon, mut gclient, gsession) = user_skill_fixture(
        "sp-ac7g",
        &guarded,
        &["**/.claude/skills/**"],
        &[("probe", "USER-SKILL-BODY-AC7\n")],
    );
    let turn = gclient.skill(&gsession, "probe", "");
    assert_eq!(
        turn["result"]["stop_reason"].as_str(),
        Some("end_turn"),
        "{turn}"
    );
    gclient.drain_events(Duration::from_millis(300));

    let blocks = gclient.events_named("privacy_block");
    assert_eq!(blocks.len(), 1, "{blocks:?}");
    assert_eq!(
        blocks[0]["path"].as_str(),
        Some("~/.claude/skills/probe/SKILL.md"),
        "the `~`-scoped id is what the glob matched and what the block names: {:?}",
        blocks[0]
    );
    assert_eq!(
        gclient.events_named("session_pinned")[0]["cause"].as_str(),
        Some("boundary_hit"),
        "a file that matched a glob pins permanently"
    );
    assert_eq!(guarded.request_count(), 0, "nothing left the machine");

    // Leg two, the control: the identical fixture with the glob absent.
    let open = mock(Vec::new());
    let (_ows, _odaemon, mut oclient, osession) =
        user_skill_fixture("sp-ac7o", &open, &[], &[("probe", "USER-SKILL-BODY-AC7\n")]);
    let turn = oclient.skill(&osession, "probe", "");
    assert_eq!(
        turn["result"]["stop_reason"].as_str(),
        Some("end_turn"),
        "{turn}"
    );
    oclient.drain_events(Duration::from_millis(300));
    assert!(
        oclient.events_named("privacy_block").is_empty(),
        "the same skill with no glob over it is not refused: {:?}",
        oclient.event_names()
    );
    assert_eq!(open.request_count(), 1, "…and its request leaves");
    assert!(any_body_contains(&open, "USER-SKILL-BODY-AC7"));

    assert_no_boundary_bytes();
}

/// **AC-8 (BR-5) — the carry-and-reattach half.**
///
/// The expansion is committed on client A's turn, which leaves. A boundary glob
/// over the skills directory is then added **mid-session** through `config/set`,
/// a **second** client attaches to the live session, and its prompt is refused
/// naming `~/.claude/skills/probe/SKILL.md` — which the daemon can only say if
/// the `~`-scoped id survived the seeded block, the conversation carry and the
/// attach. A build that lost the id on any of those hops would refuse against
/// `<unknown-provenance>`; one that dropped the block would not refuse at all.
///
/// **What this does not prove:** the compaction hop. There is no `session/compact`
/// RPC and compaction is driven by window pressure inside the assembler, so
/// forcing it here would mean sizing a fixture against a provider window —
/// exactly the shape LESSON-640 warns about. The replay seam is asserted in
/// process instead (ADR-619-2's three seams); this file says so rather than
/// implying the round trip was proved end to end.
///
/// A is built `with_auto_consent`: B's attach puts a question to a user, and A —
/// the client that created the session — is the surface it is rendered at
/// (REQ-569 BR-6), answered by A's reader thread while the test blocks on B.
#[test]
fn a_user_skills_identity_survives_compaction_and_reattach() {
    let provider = mock(Vec::new());
    let ws = Workspace::new("sp-ac8");
    ws.write_config(&config_for(&provider, &[]));
    let script = ws.write_script(&local_done_script());
    let home = ws.user_skill("probe", "Describe the repository. USER-SKILL-BODY-AC8\n");
    let daemon = Daemon::spawn(
        &ws,
        probe_16gb_with_local(script).env("HOME", home.display().to_string()),
    );

    let mut a = daemon.connect().with_auto_consent();
    let mut b = daemon.connect();
    let session = a.create_session("structured", Some("implement"));

    // A's skill turn leaves: the block is committed to the conversation with a
    // `~`-scoped id that no configured glob matches yet.
    let turn = a.skill(&session, "probe", "");
    assert_eq!(
        turn["result"]["stop_reason"].as_str(),
        Some("end_turn"),
        "{turn}"
    );
    a.drain_events(Duration::from_millis(300));
    assert_eq!(provider.request_count(), 1, "fixture: A's turn left");
    assert!(
        a.events_named("privacy_block").is_empty(),
        "fixture: nothing was refused yet: {:?}",
        a.event_names()
    );

    // The user adds the glob mid-session.
    let set = a.call(
        "config/set",
        json!({ "update": {
            "op": "set_privacy_boundary",
            "path_glob": "**/.claude/skills/**",
            "mode": "local_only",
        }}),
    );
    assert_eq!(
        set["result"]["applied"].as_bool(),
        Some(true),
        "the boundary must actually be written: {set}"
    );

    // A second client joins the live session and prompts. The carried block is
    // now under a glob, and the refusal names the file.
    let attached = b.call("session/attach", json!({ "session_id": session.clone() }));
    assert_eq!(
        attached["result"]["session"]["session_id"].as_str(),
        Some(session.as_str()),
        "{attached}"
    );
    let carried = b.prompt(&session, "Now summarize what you described.");
    assert_eq!(
        carried["result"]["stop_reason"].as_str(),
        Some("end_turn"),
        "{carried}"
    );
    b.drain_events(Duration::from_millis(300));

    let blocks = b.events_named("privacy_block");
    assert_eq!(
        blocks.len(),
        1,
        "the carried expansion is refused on B's prompt: {:?}",
        b.event_names()
    );
    assert_eq!(
        blocks[0]["path"].as_str(),
        Some("~/.claude/skills/probe/SKILL.md"),
        "BR-5: the id rode the carry and the attach intact — a lost one would \
         read `{UNKNOWN_PROVENANCE_PATH}`: {:?}",
        blocks[0]
    );
    assert_eq!(
        provider.request_count(),
        1,
        "and B's prompt did not leave: {:?}",
        b.event_names()
    );

    assert_no_boundary_bytes();
}

/// **AC-9 (BR-4).** The identity exists for files **discovery listed**, and
/// widens nothing else: a `read` of `~/.claude/skills/probe/SKILL.md` from a
/// repo-rooted session is refused by the jail exactly as it was before REQ-619.
///
/// The model is scripted to reach for the absolute path — the skill is really
/// installed and really discovered, so this is the widening a reader would
/// expect the REQ to have caused. The assertion is on what the **model** was
/// handed: the follow-up request body carries the jail's refusal, and the skill
/// file's own bytes are nowhere on the wire.
#[test]
fn a_read_of_a_user_skill_file_is_still_refused_by_the_jail() {
    // The workspace comes first, because the path the model is scripted to name
    // is the fixture's own.
    let ws = Workspace::new("sp-ac9");
    let home = ws.user_skill("probe", "USER-SKILL-BODY-AC9\n");
    let skill_file = home.join(".claude/skills/probe/SKILL.md");
    let provider = mock(vec![MockResponse::ok(openai_turn(
        "Opening the skill file directly.",
        Some((
            "c1",
            "read",
            &json!({ "path": skill_file.display().to_string() }).to_string(),
        )),
        120,
        20,
    ))]);
    ws.write_config(&config_for(&provider, &[]));
    let script = ws.write_script(&local_done_script());
    let daemon = Daemon::spawn(
        &ws,
        probe_16gb_with_local(script).env("HOME", home.display().to_string()),
    );

    let mut client = daemon.connect();
    let session = client.create_session("structured", Some("implement"));
    let turn = client.prompt(&session, "Open that skill file and tell me what it says.");
    assert_eq!(
        turn["result"]["stop_reason"].as_str(),
        Some("end_turn"),
        "{turn}"
    );
    client.drain_events(Duration::from_millis(300));

    assert_eq!(
        provider.request_count(),
        2,
        "the tool-call turn and the follow-up carrying its refusal: {:?}",
        client.event_names()
    );
    assert!(
        any_body_contains(&provider, "is outside the session root"),
        "the model is handed the jail's refusal, not the file: {:?}",
        bodies(&provider).last().map(String::len)
    );
    assert!(
        !any_body_contains(&provider, "USER-SKILL-BODY-AC9"),
        "the file's bytes must not have been read"
    );
    assert!(
        !any_body_contains(&provider, "~/.claude/skills"),
        "no `~`-scoped id reaches the wire: the identity is a provenance value, \
         not a path the jail resolves"
    );

    assert_no_boundary_bytes();
}

// ---------------------------------------------------------------------------
// AC-10 / AC-11 — the unchanged neighbours
// ---------------------------------------------------------------------------

/// **AC-10, BR-9.** With `disable_default_boundaries = true` and no user rows
/// there is nothing to protect, so a skill with an opaque `sh` preamble is sent
/// and nothing pins — exactly as before REQ-597, and exactly as REQ-614 BR-9
/// promises for the `shell` tool.
///
/// The classifier short-circuits to `Unknown` on an empty boundary set, so this
/// is also the claim that an `Unknown` verdict on a machine with no boundaries
/// costs nothing: the fold sets `unknown`, and the choke point has no glob to
/// fail closed against.
#[test]
fn with_no_boundaries_an_opaque_preamble_is_sent_and_nothing_pins() {
    let provider = mock(Vec::new());
    let ws = Workspace::new("sp-ac10");
    let mut config = String::new();
    config.push_str(&provider_block(
        "deepseek",
        "openai-compatible",
        &provider.openai_endpoint(),
        "deepseek-chat",
    ));
    config.push_str(&tier_block("build", "deepseek"));
    // No `[[boundaries]]` rows, and the builtins off: the whole of BR-9.
    config.push_str("[privacy]\ndisable_default_boundaries = true\n");
    ws.write_config(&config);
    let script = ws.write_script(&local_done_script());
    let home = ws.user_skill("probe", "USER-SKILL-BODY-AC10\nOut: !`sh -c 'echo x'`\n");
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
        "{turn}"
    );
    client.drain_events(Duration::from_millis(300));

    assert_eq!(
        provider.request_count(),
        1,
        "with no boundary configured the send is not held: {:?}",
        client.event_names()
    );
    assert!(any_body_contains(&provider, "USER-SKILL-BODY-AC10"));
    assert!(
        client.events_named("privacy_block").is_empty(),
        "{:?}",
        client.event_names()
    );
    assert!(
        client.events_named("session_pinned").is_empty(),
        "BR-9: the no-boundary machine is unchanged: {:?}",
        client.event_names()
    );
}

/// **AC-11 (BR-10).** A **project** skill is unchanged except that its preambles
/// gain BR-1's classification: with `cat README.md` it leaves, and with `cat
/// secrets/prod.env` it is refused naming that file.
///
/// Two sessions on one daemon, so the only difference between the legs is the
/// preamble. The pin is session-scoped, which is what lets the second leg's
/// refusal be read as a statement about its own skill rather than about a
/// session the first leg had already spoiled.
#[test]
fn a_project_skill_leaves_with_a_rooted_preamble_and_is_refused_with_a_boundary_one() {
    let provider = mock(Vec::new());
    let ws = Workspace::new("sp-ac11");
    ws.write_config(&config_for(&provider, &[]));
    let script = ws.write_script(&local_done_script());
    for (name, body) in [
        ("rooted", "PROJECT-SKILL-BODY-AC11\nOne: !`cat README.md`\n"),
        (
            "touching",
            "PROJECT-SKILL-BODY-AC11\nEnv: !`cat secrets/prod.env`\n",
        ),
    ] {
        let dir = ws.repo.join(".claude").join("skills").join(name);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(
            dir.join("SKILL.md"),
            format!("---\ndescription: the {name} project fixture\n---\n\n{body}"),
        )
        .unwrap();
    }
    let daemon = Daemon::spawn(&ws, probe_16gb_with_local(script));
    let mut client = daemon.connect();

    // Leg one: the rooted preamble. The repository's trust acknowledgment is
    // asked once and auto-approved by the harness client, exactly as a user
    // answering it would (REQ-591).
    let rooted = client.create_session("structured", Some("implement"));
    let turn = client.skill(&rooted, "rooted", "");
    assert_eq!(
        turn["result"]["stop_reason"].as_str(),
        Some("end_turn"),
        "{turn}"
    );
    client.drain_events(Duration::from_millis(300));
    assert_eq!(
        provider.request_count(),
        1,
        "a project skill with a provable preamble leaves: {:?}",
        client.event_names()
    );
    assert!(
        any_body_contains(&provider, README_LINE),
        "…carrying the preamble's output"
    );
    assert!(
        client.events_named("privacy_block").is_empty(),
        "{:?}",
        client.event_names()
    );
    assert!(client.events_named("session_pinned").is_empty());

    // Leg two: the boundary-naming preamble, on a fresh session of the same
    // daemon.
    let touching = client.create_session("structured", Some("implement"));
    let turn = client.skill(&touching, "touching", "");
    assert_eq!(
        turn["result"]["stop_reason"].as_str(),
        Some("end_turn"),
        "{turn}"
    );
    client.drain_events(Duration::from_millis(300));

    let blocks = client.events_named("privacy_block");
    assert_eq!(blocks.len(), 1, "{blocks:?}");
    assert_eq!(
        blocks[0]["path"].as_str(),
        Some("secrets/prod.env"),
        "the refusal names the boundary file: {:?}",
        blocks[0]
    );
    assert_eq!(
        client.events_named("session_pinned")[0]["cause"].as_str(),
        Some("boundary_hit")
    );
    assert_eq!(
        provider.request_count(),
        1,
        "the second leg sent nothing: {:?}",
        client.event_names()
    );

    assert_no_boundary_bytes();
}

// ---------------------------------------------------------------------------
// AC-12 / AC-13 — the record, and BUG-214's own shape
// ---------------------------------------------------------------------------

/// **AC-12 (BR-7).** Each command's verdict rides the `skill_invoked` outcome —
/// `reach` and a content-free `reach_reason` — and the record carries nothing
/// else it did not carry before: no output, and no key beside the four
/// documented ones.
///
/// The key check is what makes the "no output" clause structural rather than a
/// promise about the fields somebody happened to look at: an outcome that grew a
/// fifth key would fail here even if its name sounded harmless.
#[test]
fn skill_invoked_carries_each_commands_reach_and_nothing_more() {
    let provider = mock(Vec::new());
    let (_ws, _daemon, mut client, session) = user_skill_fixture(
        "sp-ac12",
        &provider,
        &[],
        &[(
            "probe",
            "USER-SKILL-BODY-AC12\nOne: !`cat README.md`\nTwo: !`sh -c 'echo x'`\n",
        )],
    );

    let turn = client.skill(&session, "probe", "");
    assert_eq!(
        turn["result"]["stop_reason"].as_str(),
        Some("end_turn"),
        "{turn}"
    );
    client.drain_events(Duration::from_millis(300));

    let invocation = one_invocation(&client);
    let outcomes = invocation["outcomes"].as_array().unwrap();
    assert_eq!(outcomes.len(), 2, "one entry per preamble: {invocation}");
    assert_eq!(reaches(invocation), vec!["rooted", "unknown"]);

    for outcome in outcomes {
        let keys: Vec<&str> = outcome
            .as_object()
            .unwrap()
            .keys()
            .map(String::as_str)
            .collect();
        for key in &keys {
            assert!(
                matches!(*key, "command" | "outcome" | "reach" | "reach_reason"),
                "an outcome carries only the documented keys — `{key}` is new, and \
                 a new key on this record is how a command's output reaches a \
                 transcript: {outcome}"
            );
        }
        assert!(
            outcome["reach_reason"]
                .as_str()
                .is_some_and(|r| !r.is_empty()),
            "every classified command says why, in the classifier's own words: {outcome}"
        );
    }

    // The command is echoed as it always was, bounded and after substitution;
    // its *output* is not.
    assert_eq!(outcomes[0]["command"].as_str(), Some("cat README.md"));
    assert_eq!(outcomes[1]["command"].as_str(), Some("sh -c 'echo x'"));
    let record = invocation.to_string();
    assert!(
        !record.contains(README_LINE),
        "the record must not carry a byte of the command's output: {record}"
    );

    assert_no_boundary_bytes();
}

/// **AC-13 — BUG-214's own shape, end to end.** A user skill whose preambles are
/// `sh <script>`, `cat <in-root>`, `cat <in-root>` pins **from the `sh` alone**:
/// the two `cat`s are `rooted` and contribute their sources, the `sh` is
/// `unknown` and pins liftably, the session is announced **once**, and after
/// `/shell allow` the next prompt's request leaves.
///
/// This is the 2026-09-05 `/analyze` session in miniature. Under REQ-585 BR-7
/// all three commands were the same fact — something spawned — and the pin was
/// recorded as a permanent `boundary_hit` nothing had crossed. Under REQ-619 the
/// three are three verdicts, only one of them pins, and the pin it takes is the
/// one a user can lift.
#[test]
fn the_bug_214_shape_pins_liftably_from_the_sh_alone() {
    let provider = mock(Vec::new());
    let ws = Workspace::new("sp-ac13");
    ws.write_config(&config_for(&provider, &[]));
    let script = ws.write_script(&local_done_script());
    // The `sh <script>` the toolkit's own partials use, and an ordinary in-root
    // note file beside it. Neither matches any builtin glob; the `sh` is opaque
    // because of its **verb**, which is the whole claim.
    std::fs::write(ws.repo.join("partial.sh"), "echo bug-214-shape\n").unwrap();
    std::fs::write(ws.repo.join(".adlc-notes.md"), "ADLC-NOTES-AC13\n").unwrap();
    let home = ws.user_skill(
        "analyze",
        "USER-SKILL-BODY-AC13\n\
         Ethos: !`sh partial.sh`\n\
         Notes: !`cat .adlc-notes.md`\n\
         Readme: !`cat README.md`\n",
    );
    let daemon = Daemon::spawn(
        &ws,
        probe_16gb_with_local(script).env("HOME", home.display().to_string()),
    );
    let mut client = daemon.connect();
    let session = client.create_session("structured", Some("implement"));

    let turn = client.skill(&session, "analyze", "");
    assert_eq!(
        turn["result"]["stop_reason"].as_str(),
        Some("end_turn"),
        "{turn}"
    );
    client.drain_events(Duration::from_millis(300));

    assert_eq!(
        reaches(one_invocation(&client)),
        vec!["unknown", "rooted", "rooted"],
        "one command pinned this session and the other two did not, and the \
         record says which"
    );

    let blocks = client.events_named("privacy_block");
    assert_eq!(blocks.len(), 1, "{blocks:?}");
    assert_eq!(
        blocks[0]["path"].as_str(),
        Some(UNKNOWN_PROVENANCE_PATH),
        "{:?}",
        blocks[0]
    );
    let pinned = client.events_named("session_pinned");
    assert_eq!(
        pinned.len(),
        1,
        "BUG-214: announced exactly once, not once per command: {pinned:?}"
    );
    assert_eq!(
        pinned[0]["cause"].as_str(),
        Some("unknown_shell"),
        "BUG-214 recorded `boundary_hit` for a session that had crossed nothing: {:?}",
        pinned[0]
    );
    assert_eq!(pinned[0]["liftable"].as_bool(), Some(true));
    let pinned_at = index_of(&client, 0, "session_pinned", |_| true).expect("the pin");
    let block_at = index_of(&client, 0, "privacy_block", |_| true).expect("the block");
    assert!(
        pinned_at < block_at,
        "the pin is announced with the block that caused it: {:?}",
        client.event_names()
    );
    assert_eq!(provider.request_count(), 0, "the pinned turn sent nothing");

    let lifted = client.call("shell/override", json!({ "session_id": session }));
    assert_eq!(
        lifted["result"]["lifted_now"].as_bool(),
        Some(true),
        "BUG-214: `/shell allow` was refused here with `boundary_hit`: {lifted}"
    );
    let after = client.prompt(&session, "Now go on with the analysis.");
    assert_eq!(
        after["result"]["stop_reason"].as_str(),
        Some("end_turn"),
        "{after}"
    );
    client.drain_events(Duration::from_millis(300));
    assert_eq!(
        provider.request_count(),
        1,
        "after the lift the next prompt leaves: {:?}",
        client.event_names()
    );

    assert_no_boundary_bytes();
}

// ---------------------------------------------------------------------------
// REQ-619 verify — the two shapes one expansion's fold could not express
// ---------------------------------------------------------------------------

/// **REQ-619 verify, C1.** A **model-invoked** skill whose preambles are one
/// opaque command and one in-root boundary read is *both* things at once, and
/// the tool provenance has to carry both: the first send is refused against the
/// content-free sentinel, `/shell allow` lifts the opacity, and the very next
/// send is refused **again** — now naming `secrets/prod.env`, with the cause
/// escalated to the permanent `boundary_hit`. The mock's request count does not
/// move across the lift.
///
/// # The leak this pins
///
/// `ExpansionProvenance::into_tool_provenance` used to hand-write the mapping
/// and answer `ToolProvenance::Unknown` for that pair, dropping the source set.
/// Refusing the send made the build look correct — and it was correct right up
/// to the lift, which is the point where dropping an id stops being
/// conservative: `/shell allow` clears the opacity, a cleared `Unknown` over an
/// **empty** source set is a *clean* provenance, and the conversation carrying
/// `secrets/prod.env`'s bytes was then sent. `ToolProvenance::UnknownWith` keeps
/// the id for the glob to match after the lift, which is the whole of what the
/// lift is supposed to mean (BUG-215, REQ-614 BR-4).
///
/// The **typed** path never had this: `runtime::turn` pushes the fold's three
/// fields onto the user block separately, so its sources survived. One rule,
/// two doors, and only one of them lost the file — BR-6's own concern.
///
/// # Mutation
///
/// Ran with `into_tool_provenance` reverted to the hand-written match
/// (`self.unknown => ToolProvenance::Unknown`): **9 red of 16** in this file.
/// This test is the one that fails on its *own* subject — `the lifted send is
/// refused a second time`, `left: 1, right: 2`: there is no second block,
/// because after the lift the provenance was clean. The other eight fail on
/// `BR-1 VIOLATION: boundary secret leaked into captured egress payload #13`,
/// which is [`assert_no_boundary_bytes`] — process-global by construction, so
/// once this test's send leaves with `secrets/prod.env`'s bytes in it every
/// later egress-touching test in the binary reports the leak too. The cascade is
/// the finding, not noise: the mutation does not merely mis-shape a value, it
/// puts a protected file on the wire. Restored: green, 16 of 16.
#[test]
fn a_model_invoked_skill_with_an_opaque_and_a_boundary_preamble_keeps_the_file_after_a_lift() {
    let provider = mock(vec![MockResponse::ok(openai_turn(
        "Reaching for the release skill.",
        Some(("c1", "skill", r#"{"name":"release"}"#)),
        120,
        20,
    ))]);
    let (_ws, _daemon, mut client, session) = user_skill_fixture(
        "sp-c1",
        &provider,
        &[],
        &[(
            "release",
            "USER-SKILL-BODY-C1\n\
             Opaque: !`sh -c 'echo hi'`\n\
             Env: !`cat secrets/prod.env`\n",
        )],
    );

    let turn = client.prompt(&session, "Use whichever skill fits, then answer.");
    assert_eq!(
        turn["result"]["stop_reason"].as_str(),
        Some("end_turn"),
        "the refused turn is served locally: {turn}"
    );
    client.drain_events(Duration::from_millis(300));

    assert_eq!(
        reaches(one_invocation(&client)),
        vec!["unknown", "boundary_touch"],
        "fixture: the expansion is opaque **and** boundary-reading, which is the \
         pair the tool provenance had no vocabulary for"
    );

    // Leg (a): the opacity is what the first refusal reports (the inspector
    // reads `is_unknown` before it walks the sources — BUG-216).
    let blocks = client.events_named("privacy_block");
    assert_eq!(blocks.len(), 1, "one block: {blocks:?}");
    assert_eq!(
        blocks[0]["path"].as_str(),
        Some(UNKNOWN_PROVENANCE_PATH),
        "{:?}",
        blocks[0]
    );
    let pinned = client.events_named("session_pinned");
    assert_eq!(pinned.len(), 1, "{pinned:?}");
    assert_eq!(
        pinned[0]["cause"].as_str(),
        Some("unknown_shell"),
        "{:?}",
        pinned[0]
    );
    assert_eq!(pinned[0]["liftable"].as_bool(), Some(true));

    // The tool-call turn left; the send carrying the expansion did not.
    let sent_before_lift = provider.request_count();
    assert_eq!(
        sent_before_lift,
        1,
        "fixture: the turn that *called* the tool left, and only that one: {:?}",
        client.event_names()
    );

    // Leg (b): the lift is granted — the pin really was the liftable kind.
    let lifted = client.call("shell/override", json!({ "session_id": session }));
    assert_eq!(
        lifted["result"]["lifted_now"].as_bool(),
        Some(true),
        "{lifted}"
    );

    // Leg (c): and the file the expansion proved is still there to refuse it.
    let after = client.prompt(&session, "Now summarize the release checklist.");
    assert_eq!(
        after["result"]["stop_reason"].as_str(),
        Some("end_turn"),
        "{after}"
    );
    client.drain_events(Duration::from_millis(300));

    let blocks = client.events_named("privacy_block");
    assert_eq!(
        blocks.len(),
        2,
        "the lifted send is refused a second time: {:?}",
        client.event_names()
    );
    assert_eq!(
        blocks[1]["path"].as_str(),
        Some("secrets/prod.env"),
        "the lift releases the opacity, not the file: {:?}",
        blocks[1]
    );
    let pinned = client.events_named("session_pinned");
    assert_eq!(
        pinned.last().map(|p| p["cause"].as_str()),
        Some(Some("boundary_hit")),
        "…and the cause escalates to the permanent one: {pinned:?}"
    );
    assert_eq!(
        provider.request_count(),
        sent_before_lift,
        "nothing left the machine across the lift: {:?}",
        client.event_names()
    );

    assert_no_boundary_bytes();
}

/// **REQ-619 verify, C2.** One preamble that names a boundary file **outside**
/// the session root *and* an ordinary in-root file — `` !`cat ~/.ssh/id_rsa
/// README.md` `` — is refused against `<boundary-touch>` with the permanent
/// cause, and the key's bytes never reach the wire.
///
/// # Why the in-root file is the whole test
///
/// Both the shell tool and the skill fold used to read `verdict.sources` being
/// **empty** as the proof that a boundary touch was in-root — which is true for
/// every command that names one path, and false for this one. `README.md` mints
/// an id and lands in `sources`, so the emptiness test read an ordinary note
/// file as evidence that the private key beside it had been nameable, folded the
/// pair to a plain `Sources({README.md})`, and let the expansion — key bytes and
/// all — leave. `Verdict::out_of_root_touch` states the fact rather than
/// inferring it, and the two facts are folded together: the bit is set *and*
/// `README.md` is still named.
///
/// The fixture's `.ssh/id_rsa` holds a distinctive marker rather than anything
/// key-shaped, so the absence assertion below is a search for bytes that exist
/// exactly once on the machine (LESSON-624 — the repo's own leak sentinel stays
/// in `secrets/prod.env`, which nothing here reads).
///
/// # Mutation
///
/// Ran with `run.verdict.out_of_root_touch ||` dropped from `fold_expansion`'s
/// `BoundaryTouch` arm, leaving the old `sources.is_empty()` reading: **1 red of
/// 16** in this file, this test on `an out-of-root touch is not cancelled by an
/// in-root file beside it`, `left: 0, right: 1` — no `privacy_block` at all.
/// One, and that is the finding: every other row in this file names either an
/// in-root path or no path, so the leak lived in the one shape nothing covered.
/// Restored: green, 16 of 16.
#[test]
fn a_preamble_touching_a_boundary_outside_the_root_beside_an_in_root_file_is_refused() {
    /// A line of the fixture home's fake `.ssh/id_rsa`. Not key-shaped on
    /// purpose: what it has to be is unique, so finding it in a request body is
    /// unambiguous.
    const FAKE_KEY_MARKER: &str = "FAKE-KEY-MARKER-AC-C2";

    let provider = mock(Vec::new());
    let ws = Workspace::new("sp-c2");
    ws.write_config(&config_for(&provider, &[]));
    let script = ws.write_script(&local_done_script());

    // The fixture HOME `Workspace::user_skill` plants into, named here because
    // the skill body has to spell the key's path absolutely.
    let home = ws.root.join("home");
    let ssh = home.join(".ssh");
    std::fs::create_dir_all(&ssh).unwrap();
    std::fs::write(
        ssh.join("id_rsa"),
        format!("{FAKE_KEY_MARKER}\nnot-a-real-key\n"),
    )
    .unwrap();

    // One command, two path arguments: the out-of-root key (matched by the
    // builtin `**/.ssh/**`) and an in-root file that mints an ordinary id.
    let key = ssh.join("id_rsa");
    assert_eq!(
        ws.user_skill(
            "probe",
            &format!(
                "USER-SKILL-BODY-C2\nBoth: !`cat {} README.md`\n",
                key.display()
            ),
        ),
        home,
        "fixture: one home per workspace"
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
        "the refused turn is served locally: {turn}"
    );
    client.drain_events(Duration::from_millis(300));

    assert_eq!(
        reaches(one_invocation(&client)),
        vec!["boundary_touch"],
        "fixture: the classifier saw the out-of-root key"
    );

    let blocks = client.events_named("privacy_block");
    assert_eq!(
        blocks.len(),
        1,
        "an out-of-root touch is not cancelled by an in-root file beside it: {:?}",
        client.event_names()
    );
    assert_eq!(
        blocks[0]["path"].as_str(),
        Some("<boundary-touch>"),
        "the key has no identity for the block to name, so the sentinel is the \
         only honest path: {:?}",
        blocks[0]
    );

    let pinned = client.events_named("session_pinned");
    assert_eq!(pinned.len(), 1, "{pinned:?}");
    assert_eq!(
        pinned[0]["cause"].as_str(),
        Some("boundary_hit"),
        "a named protected path pins permanently, however unnameable it is: {:?}",
        pinned[0]
    );
    assert_eq!(pinned[0]["liftable"].as_bool(), Some(false));

    assert_eq!(
        provider.request_count(),
        0,
        "nothing left the machine: {:?}",
        client.event_names()
    );
    assert!(
        !any_body_contains(&provider, FAKE_KEY_MARKER),
        "the key's own bytes must not appear in any captured body"
    );

    assert_no_boundary_bytes();
}

/// **REQ-619 verify, m1 — a pin on BUG-216's reported path.** The same pair as
/// C1 above (`` !`sh probe.sh` `` beside `` !`cat secrets/prod.env` ``) on the
/// **typed** door, asserted at all three moments: what the first refusal names,
/// that the lift is granted, and what the refusal after the lift names.
///
/// # What this test is for
///
/// Not the leak — C1 owns that, and the typed path never lost the id. This one
/// exists because [BUG-216] is a **known, deliberately unfixed** ordering:
/// `egress::inspector` tests `Provenance::is_unknown` before it walks the source
/// set, so a context that is both opaque and boundary-naming reports the opacity
/// first and the file only after `/shell allow` has taken the opacity away. That
/// is not wrong — both refuse, and the user reaches the truth in two steps
/// rather than one — but it is *chosen*, and an unpinned choice is one a later
/// refactor makes silently.
///
/// So legs **(a)** and **(c)** are the pin. Reorder the inspector to walk the
/// sources before the unknown test and both flip: (a) becomes
/// `secrets/prod.env` / `boundary_hit` / not liftable, (b)'s `/shell allow` is
/// then refused outright, and (c) never happens because the session was
/// permanently pinned at the first block. Whoever makes that change is fixing
/// BUG-216 and should rewrite this test to the new order — deliberately, which
/// is the point.
///
/// Leg (b) is not decoration either: it is what proves the first pin was the
/// liftable kind rather than a permanent one that merely reported a sentinel.
///
/// [BUG-216]: the inspector reports opacity before a matched boundary source
#[test]
fn a_preamble_that_is_both_opaque_and_boundary_reading_pins_and_survives_the_lift() {
    let provider = mock(Vec::new());
    let ws = Workspace::new("sp-m1");
    ws.write_config(&config_for(&provider, &[]));
    let script = ws.write_script(&local_done_script());
    // An ordinary in-root script under an opaque verb: `sh` is opaque because of
    // its **verb**, whatever it is handed.
    std::fs::write(ws.repo.join("probe.sh"), "echo probing\n").unwrap();
    let home = ws.user_skill(
        "probe",
        "USER-SKILL-BODY-M1\n\
         Opaque: !`sh probe.sh`\n\
         Env: !`cat secrets/prod.env`\n",
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
        "{turn}"
    );
    client.drain_events(Duration::from_millis(300));

    assert_eq!(
        reaches(one_invocation(&client)),
        vec!["unknown", "boundary_touch"],
        "fixture: one opaque command and one boundary read"
    );

    // (a) The opacity is reported first — BUG-216's ordering, pinned.
    let blocks = client.events_named("privacy_block");
    assert_eq!(blocks.len(), 1, "{blocks:?}");
    assert_eq!(
        blocks[0]["path"].as_str(),
        Some(UNKNOWN_PROVENANCE_PATH),
        "BUG-216: the inspector reads the unknown bit before the sources, so the \
         first refusal names the opacity and not the file: {:?}",
        blocks[0]
    );
    let pinned = client.events_named("session_pinned");
    assert_eq!(pinned.len(), 1, "{pinned:?}");
    assert_eq!(
        pinned[0]["cause"].as_str(),
        Some("unknown_shell"),
        "{:?}",
        pinned[0]
    );
    assert_eq!(pinned[0]["liftable"].as_bool(), Some(true));
    assert_eq!(provider.request_count(), 0, "the pinned turn sent nothing");

    // (b) …and the pin really is the liftable kind.
    let lifted = client.call("shell/override", json!({ "session_id": session }));
    assert_eq!(
        lifted["result"]["lifted_now"].as_bool(),
        Some(true),
        "{lifted}"
    );

    // (c) The lift released the opacity and nothing else: the file the
    // expansion proved refuses the next send, permanently.
    let after = client.prompt(&session, "Now summarize what you found.");
    assert_eq!(
        after["result"]["stop_reason"].as_str(),
        Some("end_turn"),
        "{after}"
    );
    client.drain_events(Duration::from_millis(300));

    let blocks = client.events_named("privacy_block");
    assert_eq!(
        blocks.len(),
        2,
        "the lifted send is refused again: {:?}",
        client.event_names()
    );
    assert_eq!(
        blocks[1]["path"].as_str(),
        Some("secrets/prod.env"),
        "and now the file is named: {:?}",
        blocks[1]
    );
    assert_eq!(
        client
            .events_named("session_pinned")
            .last()
            .map(|p| p["cause"].as_str()),
        Some(Some("boundary_hit")),
        "…with the cause escalated to the permanent one: {:?}",
        client.events_named("session_pinned")
    );
    assert_eq!(
        provider.request_count(),
        0,
        "nothing left the machine at any point: {:?}",
        client.event_names()
    );

    assert_no_boundary_bytes();
}
