//! REQ-579 acceptance: the guided `/provider setup` flow, driven over a socket
//! against a **spawned daemon that owns a config file** (TASK-157).
//!
//! ## Why this suite exists beside the ones that already cover the flow
//!
//! Three instruments were already pointed at REQ-579, and each is blind to what
//! is asserted here:
//!
//! * `runtime::tests::provider_setup_flow` (TASK-153/154) drives
//!   plan/preview/commit against a `DaemonRuntime` whose `config_path` a test
//!   set by hand — in process, with no wire, no dispatch gate, and no second
//!   connection to be foreign.
//! * `server.rs`'s own unit tests (TASK-154) drive the three gates against a
//!   bare `Daemon::new()`, whose `config_path` is `None` — so a commit that got
//!   past every gate could only ever answer `CONFIG_REJECTED`, and "a refused
//!   commit wrote nothing" is unfalsifiable there because there is nothing to
//!   write to.
//! * `provider_setup_contracts.rs` (TASK-156) pins the catalog projection.
//!
//! What is left, and what this file is, is the **process**: a real `teton-code`
//! with `TETON_CONFIG` in its own environment, so the config path is per-daemon
//! rather than a process-global the rest of a test binary would share. That is
//! what makes "the commit wrote these bytes and the *same session* then routed
//! to the new provider with no restart" assertable at all (LESSON-523: at least
//! one real registration through the real seam).
//!
//! ## AC → test map
//!
//! | AC / BR | Test |
//! |---------|------|
//! | AC-2 (plan → preview → commit → live routing, no restart), AC-4 (the config bytes hold only `keychain://`), BR-9, BR-15 | [`the_committed_provider_routes_the_next_decision_in_the_same_session`] |
//! | BR-9 (a commit is bound to the preview the user saw), AC-7 (a refusal writes nothing) | [`a_commit_whose_digest_went_stale_is_refused_and_the_file_is_untouched`] |
//! | AC-10 (a foreign connection is refused and the session's user is told), AC-7 | [`a_commit_from_a_connection_that_did_not_open_the_session_is_refused_and_the_session_is_told`] |
//! | AC-12 / BR-14 (replacing an existing provider is explicit and surgical), AC-11's comment-preservation half | [`a_replacement_is_previewed_as_one_and_leaves_every_other_byte_alone`] |
//! | AC-11 (the presence gate, through the REQ-575 seam), AC-7 | [`a_presence_refused_commit_writes_nothing_and_swaps_nothing`] |
//! | BR-10 (`applied: false` is a truthful answer, not a write) | [`a_commit_of_a_candidate_the_config_already_holds_writes_nothing`] |
//!
//! ### Not asserted here, and where each one is
//!
//! * **AC-9** (piped degradation) — `crates/teton/tests/cli_e2e.rs`
//!   (`a_piped_provider_setup_prints_the_recipe_and_asks_nothing`). It is a
//!   claim about the *client* binary's stdin, which this side of the wire
//!   cannot observe.
//! * **AC-4's keychain half, AC-6, AC-7's per-prompt legs, AC-8's undo** — the
//!   secret's whole lifecycle is the client's: no key is entered on this side of
//!   the wire at all, and the daemon only ever sees a `key_ref`. Those are
//!   `provider_setup_ui`'s own suite, against a fake keychain.
//! * **AC-10's model-tool-call half** — structural, and pinned as such by
//!   `no_tool_can_commit_a_provider_setup_and_no_harness_source_names_it`: tool
//!   dispatch holds a `ToolContext`, not a `DaemonRuntime`, so there is no
//!   channel for a model to reach this method through. A test here could only
//!   re-run the *connection* gate against a different fixture.
//!
//! ## Every "unchanged" is a claim about the file, never about an RPC read-back
//!
//! LESSON-519. Each refusal below is proven by the config file's own **bytes
//! and inode** — `write_config_atomically` lands every write by `rename`, so a
//! committed write always installs a new inode, and byte equality alone cannot
//! tell "nothing was written" from "the same bytes were written again". No test
//! here reads its own writes back through the RPC that made them.
//!
//! ## Falsification (LESSON-479)
//!
//! Every refusal is paired, on the **same daemon and the same candidate**, with
//! an observation of that candidate landing: the stale-digest test commits
//! against a fresh digest afterwards, the foreign-caller test hands the same
//! candidate to the session's own connection, and the presence-refused test's
//! counterpart is the happy path below — same fixture, same candidate, presence
//! seam accepting. A refusal that could never have succeeded measures nothing.

use std::path::Path;
use std::time::Duration;

use serde_json::{json, Value};

#[path = "e2e/harness.rs"]
mod harness;

use harness::{tier_block, Client, Daemon, DaemonOptions, Workspace};

const PROVIDER_SETUP_INVALID: i64 = teton_protocol::jsonrpc::error_code::PROVIDER_SETUP_INVALID;
const NOT_ATTACHED: i64 = teton_protocol::jsonrpc::error_code::NOT_ATTACHED;
const ATTESTATION_FAILED: i64 = teton_protocol::jsonrpc::error_code::ATTESTATION_FAILED;

/// The deterministic machine every daemon here is spawned onto, so a probe of
/// the real host cannot pick a model and spend the test's budget loading it.
fn probe() -> DaemonOptions {
    DaemonOptions::default()
        .env("TETON_PROBE_RAM_BYTES", (16u64 << 30).to_string())
        .env("TETON_PROBE_DISK_BYTES", (500u64 << 30).to_string())
        .env("TETON_PROBE_GPU", "apple-silicon")
}

/// The scripted local engine every daemon here gets.
///
/// No test in this suite runs a turn — the flow performs no egress (BR-13) and
/// the routing claim is read off the daemon's own resolver, not off a provider
/// capture. The script is here for what it buys at *startup*: a scripted local
/// tier downloads nothing, so the first-run consent gate is exempt and no
/// proposal stands between a client and the session it just opened.
const NO_TURNS: &str = "unused — no test in this suite runs a turn.";

/// A fresh machine: one local provider, every tier bound to it, and a comment of
/// the user's at the top.
///
/// `think` is bound **away** from the candidate deliberately — the happy path's
/// routing assertion is a change or it is nothing, and a fixture that already
/// routed `think` at `kimi` would make it a coincidence.
fn fresh_config() -> String {
    let mut config = String::from("# my teton config, written by hand\n\n");
    config.push_str("[[providers]]\nid = \"local\"\nkind = \"local\"\n\n");
    for tier in ["reflex", "scan", "build", "think"] {
        config.push_str(&tier_block(tier, "local"));
    }
    config.push_str("[privacy]\nredact = false\n\n");
    config
}

/// A machine that already registers `kimi`, so a re-registration has something
/// of the user's to preserve: a comment above the row, a hand-authored
/// `[providers.capabilities]` on it, and a neighbouring provider with both.
///
/// The in-crate twin of this fixture (`SEEDED_WITH_KIMI`) proves the property of
/// a derivation; this one proves it of **a file a real daemon wrote**.
const SEEDED_WITH_KIMI: &str = "\
# my teton config, written by hand
effort = \"high\"

# the cheap one, for reading
[[providers]]
id = \"deepseek\"
kind = \"openai-compatible\"
endpoint = \"https://api.deepseek.com/chat/completions\"
model = \"deepseek-v4-pro\"
auth_ref = \"keychain://teton/deepseek\"
[providers.capabilities]
max_context = 128000

# the one I registered last month, before the price changed
[[providers]]
id = \"kimi\"
kind = \"openai-compatible\"
endpoint = \"https://api.moonshot.ai/v1/chat/completions\"
model = \"kimi-k2\"
auth_ref = \"keychain://teton/kimi\"
[providers.capabilities]
max_context = 200000

[[providers]]
id = \"local\"
kind = \"local\"

[[tiers]]
tier = \"scan\"
provider_id = \"deepseek\"
";

/// The REQ's own worked example, as the client would send it: the recipe's
/// endpoint, a keychain **reference** (never a value, BR-2), and `think`.
///
/// One spelling for preview and commit, so a test cannot preview one candidate
/// and commit another — BR-9 is exactly the claim that those are the same thing.
fn kimi(model: &str) -> Value {
    json!({
        "id": "kimi",
        "kind": "openai-compatible",
        "endpoint": "https://api.moonshot.ai/v1/chat/completions",
        "model": model,
        "key_ref": "keychain://teton/kimi",
        "bindings": [{ "tier": "think", "provider_id": "kimi" }],
    })
}

/// `provider/setup_plan` for `session`.
fn plan(client: &mut Client, session: &str) -> Value {
    client.call("provider/setup_plan", json!({ "session_id": session }))
}

/// `provider/setup_preview` for `session`.
fn preview(client: &mut Client, session: &str, model: &str) -> Value {
    client.call(
        "provider/setup_preview",
        json!({ "session_id": session, "candidate": kimi(model) }),
    )
}

/// `provider/setup_commit` for `session`, optionally digest-bound.
fn commit(client: &mut Client, session: &str, model: &str, digest: Option<&str>) -> Value {
    let mut params = json!({ "session_id": session, "candidate": kimi(model) });
    if let Some(digest) = digest {
        params["expect_digest"] = json!(digest);
    }
    client.call("provider/setup_commit", params)
}

/// The `digest` a preview answered with, or a failure that shows the response.
fn digest_of(preview: &Value) -> String {
    preview["result"]["digest"]
        .as_str()
        .unwrap_or_else(|| panic!("the preview must carry a digest: {preview}"))
        .to_owned()
}

/// What a `think`-tier turn would route to right now, asked of the daemon's own
/// **resolver** rather than of the `[[tiers]]` table.
///
/// `config/get`'s `routing` rows are read off `CategoryResolution` — the same
/// value `route_decided` is built from, and the one `teton policy show` renders
/// — and `review` is a `think` category. Reading the `[[tiers]]` row instead
/// would prove the bytes landed and say nothing about the daemon having noticed
/// (BR-10/BR-15, AC-2).
fn think_provider(client: &mut Client) -> Option<String> {
    let snapshot = client.config_get();
    snapshot["routing"]
        .as_array()
        .unwrap_or_else(|| panic!("the snapshot carries a routing table: {snapshot}"))
        .iter()
        .find(|row| row["category"].as_str() == Some("review"))
        .unwrap_or_else(|| panic!("the routing table names every category: {snapshot}"))
        .get("provider_id")
        .and_then(Value::as_str)
        .map(str::to_owned)
}

/// Whether the daemon's live config holds a provider with this id — asked of
/// `provider/setup_plan`, which reports the `[[providers]]` rows in memory.
///
/// Used only to show that a **refused** commit swapped nothing. It is never used
/// as evidence that a write landed; that is always read off the file.
fn plan_knows_provider(client: &mut Client, session: &str, id: &str) -> bool {
    let planned = plan(client, session);
    planned["result"]["existing"]
        .as_array()
        .unwrap_or_else(|| panic!("the plan reports the registered providers: {planned}"))
        .iter()
        .any(|existing| existing["id"].as_str() == Some(id))
}

/// The file's inode — a write's fingerprint, however the bytes come out.
///
/// `write_config_atomically` lands every write by `rename`, so a committed write
/// always installs a new inode. Byte equality alone cannot tell "nothing was
/// written" from "the same bytes were written again", and the claim every
/// refusal here makes is the first one (LESSON-519).
fn inode(path: &Path) -> u64 {
    use std::os::unix::fs::MetadataExt as _;
    std::fs::metadata(path)
        .expect("the config file exists")
        .ino()
}

// ---------------------------------------------------------------------------
// (1) AC-2 / AC-4 / BR-9 / BR-15 — the whole flow, over the socket
// ---------------------------------------------------------------------------

/// **AC-2: plan → preview → commit, and the very next routing decision of the
/// same session resolves to the new provider with no restart.**
///
/// This is the criterion no other suite can reach. It needs a daemon with a
/// config file *and* a live router *and* one client holding one session across
/// the write — which is a process, not a fixture.
///
/// The legs, in order, each one a precondition of the next:
///
/// 1. the plan reports the shipped catalog, says `kimi` is not registered, and
///    reports what `think` points at today (non-vacuity: the machine really is
///    in the state the flow is for);
/// 2. the preview renders the rows a commit would write, names the **dial**
///    host, and the file is byte-identical afterwards — a preview is not a
///    write (BR-3's single commit point);
/// 3. the commit applies, and the file **hashes to the digest the user
///    confirmed**. That is the whole assertion about the write: a substring
///    check would pass for a write that also mangled something the preview
///    never showed;
/// 4. AC-4: the document carries the keychain *reference* and nothing that could
///    be a key;
/// 5. `provider_setup_completed` arrives at **this** client, scoped to **this**
///    session (BR-15) — asserted off the socket, never out of a log;
/// 6. and `think` now resolves to `kimi`, in the daemon that wrote it, with no
///    restart — the leg that fails on an implementation that wrote correctly and
///    forgot the swap.
#[test]
fn the_committed_provider_routes_the_next_decision_in_the_same_session() {
    let ws = Workspace::new("provider-setup-happy");
    ws.write_config(&fresh_config());
    let script = ws.write_script(NO_TURNS);
    let daemon = Daemon::spawn(&ws, probe().script(script));
    let mut client = daemon.connect();
    let session = client.create_session("freeform", None);

    let before_bytes = std::fs::read(&ws.config_path).expect("the fixture config exists");

    // (1) The machine is genuinely in the state the flow exists for.
    let planned = plan(&mut client, &session);
    let catalog = planned["result"]["catalog"]
        .as_array()
        .unwrap_or_else(|| panic!("the plan must serve the shipped catalog: {planned}"));
    assert!(
        catalog
            .iter()
            .any(|entry| entry["id_suggestion"].as_str() == Some("kimi")),
        "the vendor this test registers must be one the daemon offers: {planned}"
    );
    assert!(
        !plan_knows_provider(&mut client, &session, "kimi"),
        "the fixture must start with `kimi` unregistered, or nothing below is evidence"
    );
    assert_eq!(
        think_provider(&mut client),
        Some("local".to_owned()),
        "the fixture must start with `think` routed somewhere else"
    );

    // (2) A preview is not a write.
    let previewed = preview(&mut client, &session, "kimi-k3");
    let toml = previewed["result"]["toml"]
        .as_str()
        .unwrap_or_else(|| panic!("the preview must render the candidate rows: {previewed}"));
    for needle in [
        "[[providers]]",
        "id = \"kimi\"",
        "model = \"kimi-k3\"",
        "auth_ref = \"keychain://teton/kimi\"",
        "[[tiers]]",
        "tier = \"think\"",
        "provider_id = \"kimi\"",
    ] {
        assert!(
            toml.contains(needle),
            "the preview must be the bytes a commit would write ({needle:?}): {toml:?}"
        );
    }
    assert_eq!(
        previewed["result"]["dial_host"].as_str(),
        Some("api.moonshot.ai"),
        "the host at the confirm step comes from the parser that dials — a host, \
         never the path or the query (BR-5, LESSON-528/529): {previewed}"
    );
    assert!(
        previewed["result"]["replaces"].is_null(),
        "a fresh registration replaces nothing: {previewed}"
    );
    assert_eq!(
        std::fs::read(&ws.config_path).ok().as_deref(),
        Some(before_bytes.as_slice()),
        "BR-3: the commit is the single write point — a preview writes nothing"
    );

    // (3) The commit, bound to the digest the user confirmed.
    let digest = digest_of(&previewed);
    let committed = commit(&mut client, &session, "kimi-k3", Some(&digest));
    assert_eq!(
        committed["result"]["applied"].as_bool(),
        Some(true),
        "the commit must apply: {committed}\ndaemon log:\n{}",
        daemon.log()
    );
    assert_eq!(committed["result"]["provider_id"].as_str(), Some("kimi"));
    assert_eq!(
        committed["result"]["bindings"][0]["tier"].as_str(),
        Some("think"),
        "the bindings reported are what landed: {committed}"
    );

    let written = std::fs::read_to_string(&ws.config_path).expect("the config was written");
    assert_eq!(
        teton_inference::sha256_hex(written.as_bytes()),
        digest,
        "BR-9: the file is not the document the user confirmed:\n{written}"
    );
    for section in toml.split("\n\n") {
        assert!(
            written.contains(section.trim_end()),
            "the preview showed bytes the write did not leave:\n{section}\n\
             document:\n{written}"
        );
    }
    assert!(
        written.contains("[[tiers]]\ntier = \"think\"\nprovider_id = \"kimi\""),
        "the routing answer must land as a `[[tiers]]` row of its own:\n{written}"
    );

    // (4) AC-4: a reference, and nothing a key could be hiding in.
    assert!(
        written.contains("auth_ref = \"keychain://teton/kimi\""),
        "AC-4: the config carries only the keychain reference:\n{written}"
    );
    assert!(
        !written.contains("key_ref"),
        "AC-4: the wire field name has no business in the document:\n{written}"
    );

    // (5) BR-15: the completion reached *this* client, scoped to *this* session.
    let completion = client
        .wait_for_event("provider_setup_completed", Duration::from_secs(2))
        .unwrap_or_else(|| {
            panic!(
                "BR-15: the commit must announce itself; daemon log:\n{}",
                daemon.log()
            )
        });
    assert_eq!(
        completion["session_id"].as_str(),
        Some(session.as_str()),
        "the completion belongs to the committing session: {completion}"
    );
    assert_eq!(completion["provider_id"].as_str(), Some("kimi"));
    assert_eq!(completion["kind"].as_str(), Some("openai-compatible"));
    assert_eq!(completion["model"].as_str(), Some("kimi-k3"));
    assert_eq!(completion["bindings"][0]["tier"].as_str(), Some("think"));
    assert_eq!(
        completion["bindings"][0]["provider_id"].as_str(),
        Some("kimi")
    );

    // (6) Live, in this daemon, with no restart: a `think`-tier routing decision
    // now resolves to the provider the flow just registered.
    assert_eq!(
        think_provider(&mut client),
        Some("kimi".to_owned()),
        "AC-2: the committed provider must serve the next routing decision of the \
         same session, with no restart. daemon log:\n{}",
        daemon.log()
    );
    let snapshot = client.config_get();
    let think_row = snapshot["tiers"]
        .as_array()
        .and_then(|rows| {
            rows.iter()
                .find(|row| row["tier"].as_str() == Some("think"))
        })
        .unwrap_or_else(|| panic!("the snapshot reports every tier: {snapshot}"));
    assert_eq!(think_row["provider_id"].as_str(), Some("kimi"));
    assert_eq!(
        think_row["source"].as_str(),
        Some("configured"),
        "the binding is a row the flow wrote, not an inherited fill: {think_row}"
    );
    // And the tiers nobody named are exactly where they were.
    for tier in ["reflex", "scan", "build"] {
        let row = snapshot["tiers"]
            .as_array()
            .and_then(|rows| rows.iter().find(|row| row["tier"].as_str() == Some(tier)))
            .unwrap_or_else(|| panic!("the snapshot reports every tier: {snapshot}"));
        assert_eq!(
            row["provider_id"].as_str(),
            Some("local"),
            "registering `kimi` was collateral for the `{tier}` binding: {row}"
        );
    }
}

// ---------------------------------------------------------------------------
// (2) BR-9 / AC-7 — a preview the document has outgrown
// ---------------------------------------------------------------------------

/// **A commit whose digest no longer matches is refused, and the refusal has no
/// write path** (BR-9, AC-7, LESSON-519).
///
/// The drift is a comment appended at a key this flow never touches, which is
/// the point: the digest covers the **whole document**, because the whole
/// document is what a commit writes — everything the flow does not collect rides
/// along from the file. A digest over the rendered rows alone would call this
/// document unchanged and write away the user's edit.
///
/// "Nothing was written" is asserted against the file's own **bytes and inode**,
/// and "nothing was swapped" against the daemon's live config: a refusal that
/// had already swapped would leave a daemon routing to a provider its config
/// file has never heard of.
///
/// Falsified in place: the same candidate, re-previewed against the document as
/// it now is, commits. What was refused is the staleness, not the candidate.
#[test]
fn a_commit_whose_digest_went_stale_is_refused_and_the_file_is_untouched() {
    let ws = Workspace::new("provider-setup-stale");
    ws.write_config(&fresh_config());
    let script = ws.write_script(NO_TURNS);
    let daemon = Daemon::spawn(&ws, probe().script(script));
    let mut client = daemon.connect();
    let session = client.create_session("freeform", None);

    let stale = digest_of(&preview(&mut client, &session, "kimi-k3"));

    // The hand edit that lands while the user is still reading the preview.
    let drifted = format!(
        "{}\n# added while the user was still reading the preview\n",
        fresh_config()
    );
    std::fs::write(&ws.config_path, &drifted).expect("hand-edit the config");
    let before_inode = inode(&ws.config_path);

    let refused = commit(&mut client, &session, "kimi-k3", Some(&stale));
    assert_eq!(
        refused["error"]["code"].as_i64(),
        Some(PROVIDER_SETUP_INVALID),
        "a document that moved under the preview must be refused under this REQ's \
         own code: {refused}\ndaemon log:\n{}",
        daemon.log()
    );
    let message = refused["error"]["message"].as_str().unwrap_or_default();
    assert!(
        !message.contains(&stale),
        "the refusal echoes no digest and no document: {message}"
    );

    assert_eq!(
        std::fs::read_to_string(&ws.config_path).expect("read"),
        drifted,
        "AC-7: a refused commit must leave the config byte-identical"
    );
    assert_eq!(
        before_inode,
        inode(&ws.config_path),
        "a refused commit rewrote the file — byte equality alone cannot tell that \
         from a write of identical bytes (LESSON-519)"
    );
    assert!(
        !plan_knows_provider(&mut client, &session, "kimi"),
        "a refused commit swapped the candidate into memory anyway"
    );
    assert!(
        client.events_named("provider_setup_completed").is_empty(),
        "a refused commit announced a completion: {:?}",
        client.events_named("provider_setup_completed")
    );

    // Falsification: the same candidate against a digest taken over the document
    // as it now is lands, so what was refused above is the staleness.
    let fresh = digest_of(&preview(&mut client, &session, "kimi-k3"));
    let committed = commit(&mut client, &session, "kimi-k3", Some(&fresh));
    assert_eq!(
        committed["result"]["applied"].as_bool(),
        Some(true),
        "a fresh digest must commit: {committed}\ndaemon log:\n{}",
        daemon.log()
    );
    let written = std::fs::read_to_string(&ws.config_path).expect("read");
    assert!(
        written.contains("# added while the user was still reading the preview"),
        "and the edit the refusal protected survives the commit that followed:\n{written}"
    );
}

// ---------------------------------------------------------------------------
// (3) AC-10 / BR-12 — a connection that did not open the session
// ---------------------------------------------------------------------------

/// **AC-10: a commit from a second connection that never opened the session is
/// refused, writes nothing, and the session's own user is told.**
///
/// The refusal code is the existing `NOT_ATTACHED` — the one `web/setup_*` gives
/// a foreign caller — and the *announcement* is this REQ's own
/// `provider_setup_rejected_nonuser`. An RPC error travels back to the caller
/// and nowhere else (LESSON-505), so without the event the person whose session
/// was reached for would never learn of it.
///
/// The intruder learns the session id the way any same-UID peer would: this test
/// hands it over, which is strictly more generous than `session/list` would be.
///
/// Falsified in place, on the same daemon and with the same candidate: the
/// session's own connection commits it immediately afterwards and the bytes
/// change. So "the file was untouched" is a fact about the gate and not about a
/// candidate that could never have landed.
#[test]
fn a_commit_from_a_connection_that_did_not_open_the_session_is_refused_and_the_session_is_told() {
    let ws = Workspace::new("provider-setup-foreign");
    ws.write_config(&fresh_config());
    let script = ws.write_script(NO_TURNS);
    let daemon = Daemon::spawn(&ws, probe().script(script));
    let mut owner = daemon.connect();
    let session = owner.create_session("freeform", None);
    let before_bytes = std::fs::read(&ws.config_path).expect("the fixture config exists");
    let before_inode = inode(&ws.config_path);

    // A second connection to the same daemon. It never opened this session and
    // never attached to it.
    let mut intruder = daemon.connect();
    let refused = commit(&mut intruder, &session, "kimi-k3", None);
    assert_eq!(
        refused["error"]["code"].as_i64(),
        Some(NOT_ATTACHED),
        "a connection without session access must not commit that session's \
         configuration: {refused}\ndaemon log:\n{}",
        daemon.log()
    );

    assert_eq!(
        std::fs::read(&ws.config_path).ok().as_deref(),
        Some(before_bytes.as_slice()),
        "AC-10: a refused foreign commit must leave the config byte-identical"
    );
    assert_eq!(
        before_inode,
        inode(&ws.config_path),
        "AC-10: a refused foreign commit must not rewrite the file at all"
    );

    // BR-12: the notice goes to the **session's own** connection.
    let notice = owner
        .wait_for_event("provider_setup_rejected_nonuser", Duration::from_secs(2))
        .unwrap_or_else(|| {
            panic!(
                "BR-12: the session's user must be told; daemon log:\n{}",
                daemon.log()
            )
        });
    assert_eq!(
        notice["session_id"].as_str(),
        Some(session.as_str()),
        "the notice belongs to the session that was reached for: {notice}"
    );
    assert_eq!(
        notice["method"].as_str(),
        Some("provider/setup_commit"),
        "and it names the method: {notice}"
    );
    // It names a method and nothing else — no caller identity, and no echo of
    // the candidate it tried to register (BR-2, BR-12).
    let mut keys: Vec<&String> = notice
        .as_object()
        .unwrap_or_else(|| panic!("the envelope is an object: {notice}"))
        .keys()
        .collect();
    keys.sort();
    assert_eq!(
        keys,
        ["event", "method", "seq", "session_id"],
        "the rejection carries nothing else: {notice}"
    );
    intruder.drain_events(Duration::from_millis(200));
    assert!(
        !intruder.saw_event("provider_setup_rejected_nonuser"),
        "the notice must not be handed back to the caller it is about: {:?}",
        intruder.events()
    );

    // Falsification: the same candidate, from the session's own connection.
    let committed = commit(&mut owner, &session, "kimi-k3", None);
    assert_eq!(
        committed["result"]["applied"].as_bool(),
        Some(true),
        "the session's own client must reach the runtime: {committed}\n\
         daemon log:\n{}",
        daemon.log()
    );
    assert_ne!(
        std::fs::read(&ws.config_path).ok().as_deref(),
        Some(before_bytes.as_slice()),
        "non-vacuity: the candidate the gate refused is one that does write"
    );
}

// ---------------------------------------------------------------------------
// (4) AC-12 / BR-14 — replacing an existing provider
// ---------------------------------------------------------------------------

/// **AC-12: re-registering an existing id is previewed as a replacement, and the
/// commit edits that one row and nothing else** (BR-14, BUG-155's class).
///
/// Two claims, and the second is the one only a real file can carry. The preview
/// states the replacement in a **typed** field — the plan's `existing` list can
/// be several answers old by the time a candidate is built, so the surface that
/// knows whether the write replaces something is the daemon that built the
/// candidate config, not a client re-deriving it.
///
/// Then: the write is a matched edit. Every block quoted below is taken out of
/// the seed rather than retyped, so a change to the fixture cannot quietly
/// weaken what is compared — the neighbouring provider's whole block, the
/// replaced row's own comment, and the hand-authored `[providers.capabilities]`
/// on the row the edit lands on (REQ-574; AC-11's comment-preservation half).
#[test]
fn a_replacement_is_previewed_as_one_and_leaves_every_other_byte_alone() {
    let ws = Workspace::new("provider-setup-replace");
    ws.write_config(SEEDED_WITH_KIMI);
    let script = ws.write_script(NO_TURNS);
    let daemon = Daemon::spawn(&ws, probe().script(script));
    let mut client = daemon.connect();
    let session = client.create_session("freeform", None);

    let previewed = preview(&mut client, &session, "kimi-k3");
    assert_eq!(
        previewed["result"]["replaces"]["id"].as_str(),
        Some("kimi"),
        "BR-14: a replace is stated, never silent: {previewed}"
    );
    assert_eq!(
        previewed["result"]["replaces"]["model"].as_str(),
        Some("kimi-k2"),
        "AC-12: and it says what it is replacing — the fixture is only meaningful \
         if this replaces something: {previewed}"
    );
    assert_eq!(
        previewed["result"]["replaces"]["kind"].as_str(),
        Some("openai-compatible"),
        "{previewed}"
    );

    let digest = digest_of(&previewed);
    let committed = commit(&mut client, &session, "kimi-k3", Some(&digest));
    assert_eq!(
        committed["result"]["applied"].as_bool(),
        Some(true),
        "the replacement must apply: {committed}\ndaemon log:\n{}",
        daemon.log()
    );

    let written = std::fs::read_to_string(&ws.config_path).expect("the config was written");
    for surviving in [
        "# my teton config, written by hand",
        "effort = \"high\"",
        "# the cheap one, for reading",
        "id = \"deepseek\"",
        "model = \"deepseek-v4-pro\"",
        "auth_ref = \"keychain://teton/deepseek\"",
        "max_context = 128000",
        // The replaced row's own comment and its hand-authored capability table:
        // a replace is a matched edit, not a rewrite.
        "# the one I registered last month, before the price changed",
        "max_context = 200000",
        "[[tiers]]\ntier = \"scan\"\nprovider_id = \"deepseek\"",
    ] {
        assert!(
            written.contains(surviving),
            "registering `kimi` was collateral for `{surviving}`:\n{written}"
        );
    }
    // The whole neighbouring provider block, byte for byte, rather than key by
    // key — quoted out of the seed so the fixture and the assertion cannot drift.
    let neighbour = SEEDED_WITH_KIMI
        .split_once("# the cheap one, for reading")
        .and_then(|(_, rest)| rest.split_once("\n\n"))
        .map(|(block, _)| block)
        .expect("the seed carries a neighbour block");
    assert!(
        written.contains(neighbour),
        "the neighbouring provider's block was not left alone:\n{written}"
    );

    // What did change: the one row, once.
    assert_eq!(
        written.matches("\nid = \"kimi\"").count(),
        1,
        "the replace inserted a second `kimi` row instead of editing the one \
         that was there (LESSON-522):\n{written}"
    );
    assert!(written.contains("model = \"kimi-k3\""), "{written}");
    assert!(!written.contains("model = \"kimi-k2\""), "{written}");
    assert!(
        written.contains("[[tiers]]\ntier = \"think\"\nprovider_id = \"kimi\""),
        "and the tier the answers named is now a row of its own:\n{written}"
    );
    assert_eq!(
        think_provider(&mut client),
        Some("kimi".to_owned()),
        "daemon log:\n{}",
        daemon.log()
    );
}

// ---------------------------------------------------------------------------
// (5) AC-11 / BR-12 — the presence gate
// ---------------------------------------------------------------------------

/// **AC-11: a presence-refused commit is refused, and the proof is the bytes on
/// disk and the live config — not the error code.**
///
/// The seam is the one REQ-575 built and `web_setup_flow.rs` uses verbatim:
/// `TETON_TEST_SEAMS=1` + `TETON_PRESENCE_ACCEPT=fail` installs
/// `AlwaysFailsVerifier` (see `attest::seam_verifier`), which is the only way to
/// reach a **present-but-refusing** mechanism in a separate process. It is a
/// *runtime* seam and not a `--features presence` one, which is why this test
/// needs no build guard and prints no skip: the `presence` feature compiles the
/// real macOS FFI verifier, while the seam above overrides whatever verifier the
/// build shipped. The master switch keeps it out of the artifact users run — a
/// release build refuses to start with `TETON_TEST_SEAMS` set (DECISION 3, E-6).
///
/// The evidence AC-11 asks for is read back from the world:
///
///   1. the config file is **byte-identical**, with its original inode, and
///   2. the daemon's live config still does not hold `kimi`, so nothing was
///      swapped in ahead of the write.
///
/// The error code is asserted too, but it is the weakest of the three claims.
/// The non-vacuity anchor is [`the_committed_provider_routes_the_next_decision_in_the_same_session`]:
/// the same fixture and the same candidate, on a daemon whose presence seam
/// accepts, writes.
#[test]
fn a_presence_refused_commit_writes_nothing_and_swaps_nothing() {
    let ws = Workspace::new("provider-setup-presence");
    ws.write_config(&fresh_config());
    let script = ws.write_script(NO_TURNS);
    let daemon = Daemon::spawn(
        &ws,
        probe()
            .script(script)
            .env("TETON_TEST_SEAMS", "1")
            .env("TETON_PRESENCE_ACCEPT", "fail"),
    );
    let mut client = daemon.connect();
    let session = client.create_session("freeform", None);

    let before_bytes = std::fs::read(&ws.config_path).expect("the fixture config exists");
    let before_inode = inode(&ws.config_path);
    // A preview is not a commitment, so it is not behind the presence gate — and
    // its success is what makes the refusal below the gate's answer rather than
    // a candidate this daemon would have refused anyway.
    let digest = digest_of(&preview(&mut client, &session, "kimi-k3"));

    let refused = commit(&mut client, &session, "kimi-k3", Some(&digest));
    assert_eq!(
        refused["error"]["code"].as_i64(),
        Some(ATTESTATION_FAILED),
        "AC-11: the commit must be refused at the BR-10(b) presence gate: {refused}\n\
         daemon log:\n{}",
        daemon.log()
    );

    // (1) Inspected on disk, not inferred.
    assert_eq!(
        std::fs::read(&ws.config_path).ok().as_deref(),
        Some(before_bytes.as_slice()),
        "AC-11: a presence-refused commit must leave config.toml byte-identical"
    );
    assert_eq!(
        before_inode,
        inode(&ws.config_path),
        "AC-11: and must not rewrite it with identical bytes either"
    );

    // (2) Inspected in the live daemon, not inferred.
    assert!(
        !plan_knows_provider(&mut client, &session, "kimi"),
        "AC-11: a presence-refused commit must not live-swap the in-memory config"
    );
    assert_eq!(
        think_provider(&mut client),
        Some("local".to_owned()),
        "and routing is exactly where it was"
    );
    assert!(
        client.events_named("provider_setup_completed").is_empty(),
        "a presence-refused commit announced a completion: {:?}",
        client.events_named("provider_setup_completed")
    );
}

// ---------------------------------------------------------------------------
// (6) BR-10 — a candidate the configuration already holds
// ---------------------------------------------------------------------------

/// **A commit whose candidate is already the configuration writes nothing and
/// says so** — `applied: false` is a truthful answer, not a failure and not an
/// error response (BR-10).
///
/// The claim that needs a real file is the *write*: an implementation that
/// re-wrote identical bytes would satisfy every byte comparison and still have
/// touched the user's file, so the inode is what is asserted (LESSON-519).
///
/// The completion count carries the other half. `provider_setup_completed` fires
/// only on a commit that **applied**, so after two commits of the same candidate
/// there must be exactly one — a client that printed "registered" twice would be
/// telling the user something happened when nothing did.
#[test]
fn a_commit_of_a_candidate_the_config_already_holds_writes_nothing() {
    let ws = Workspace::new("provider-setup-noop");
    ws.write_config(&fresh_config());
    let script = ws.write_script(NO_TURNS);
    let daemon = Daemon::spawn(&ws, probe().script(script));
    let mut client = daemon.connect();
    let session = client.create_session("freeform", None);

    let first = digest_of(&preview(&mut client, &session, "kimi-k3"));
    let committed = commit(&mut client, &session, "kimi-k3", Some(&first));
    assert_eq!(
        committed["result"]["applied"].as_bool(),
        Some(true),
        "the first commit must land: {committed}\ndaemon log:\n{}",
        daemon.log()
    );
    let after_first = std::fs::read(&ws.config_path).expect("the config was written");
    let after_first_inode = inode(&ws.config_path);
    assert!(
        client
            .wait_for_event("provider_setup_completed", Duration::from_secs(2))
            .is_some(),
        "the applied commit must announce itself; daemon log:\n{}",
        daemon.log()
    );

    // The same answers again, previewed against the document as it now is.
    let second = digest_of(&preview(&mut client, &session, "kimi-k3"));
    let again = commit(&mut client, &session, "kimi-k3", Some(&second));
    assert!(
        again["error"].is_null(),
        "a no-op commit is not a failure: {again}"
    );
    assert_eq!(
        again["result"]["applied"].as_bool(),
        Some(false),
        "BR-10: the config already said exactly this, so nothing was applied: {again}"
    );
    assert_eq!(
        again["result"]["provider_id"].as_str(),
        Some("kimi"),
        "the rows in force are still reported: {again}"
    );
    assert_eq!(
        again["result"]["bindings"][0]["tier"].as_str(),
        Some("think"),
        "{again}"
    );

    assert_eq!(
        std::fs::read(&ws.config_path).ok().as_deref(),
        Some(after_first.as_slice()),
        "a no-op commit changed the file"
    );
    assert_eq!(
        after_first_inode,
        inode(&ws.config_path),
        "a no-op commit rewrote the file with identical bytes — `applied: false` \
         and a write are not the same answer (LESSON-519)"
    );

    client.drain_events(Duration::from_millis(300));
    assert_eq!(
        client.events_named("provider_setup_completed").len(),
        1,
        "the completion fires only on a commit that applied: {:?}",
        client.events_named("provider_setup_completed")
    );
}
