//! One end-to-end test per REQ-547 acceptance criterion (AC-1..AC-10, AC-12).
//!
//! Every test here spawns the **real** `tetond` binary with **no**
//! `TETON_LOCAL_SCRIPT`. That absence is the point: a scripted local engine
//! downloads nothing, so the daemon skips the consent gate entirely
//! (`DaemonRuntime::first_run_consent_applies`) — which meant that until this
//! file existed, no process-level test exercised the gate at all. Here the
//! engine is genuinely absent, the daemon genuinely proposes, and a client
//! genuinely answers over the socket.
//!
//! ## What is real and what is mocked
//!
//! | Piece | Here |
//! |---|---|
//! | daemon, protocol, consent gate, install pipeline, HTTP download client | **real** |
//! | the model host | [`MockHf`] on localhost (no network to huggingface.co) |
//! | the artifact | a ~64–128 KiB fixture whose `sha256` is **computed from the bytes served** |
//! | hardware, free disk, retry delays | env seams (`TETON_PROBE_*`, `TETON_DISK_FREE_BYTES`, `TETON_DOWNLOAD_RETRY_BASE_MS`) |
//!
//! The digest being computed rather than pinned is what keeps the verify path
//! honest: point the host at different bytes (AC-7) and the check fails for the
//! real reason, not because a constant says so.
//!
//! ## What this file deliberately does **not** claim
//!
//! That inference runs on the installed weights. No GGUF here is a model, and
//! `--features llama` is not built in CI. "A real end-to-end install of a real
//! catalog model, benchmarked on a developer machine" is AC-13, a manual gate
//! with a human sign-off (`docs/manual-verification.md`, LESSON-433). Nothing in
//! this file may be read as evidence for it.

use std::net::TcpListener;
use std::path::Path;
use std::process::Command;
use std::time::{Duration, Instant};

use serde_json::{json, Value};

use teton_inference::catalog::{Catalog, HfSource};

use crate::harness::{
    assert_no_boundary_bytes, file_map, fixture_catalog_toml, fixture_models, local_model_block,
    openai_turn, Client, Daemon, DaemonOptions, HfArtifact, HfTreeFile, MockHf, MockHfConfig,
    MockProvider, Workspace,
};

const GIB: u64 = 1024 * 1024 * 1024;

/// How long a test waits for the daemon to publish its proposal.
const PROPOSAL_WINDOW: Duration = Duration::from_secs(10);

/// How long a test waits for an install to reach a terminal state.
const INSTALL_WINDOW: Duration = Duration::from_secs(20);

// ---------------------------------------------------------------------------
// Fixture wiring
// ---------------------------------------------------------------------------

/// The env a consent test spawns the daemon with: a deterministic 16 GiB
/// Apple-Silicon machine, the fixture catalog, and a retry ladder whose delays
/// (only) are shortened.
///
/// Note what is *not* here: `TETON_LOCAL_SCRIPT`. With it the daemon would have
/// an engine, `first_run_consent_applies` would be false, and none of these
/// tests would test anything.
fn consent_env(catalog: &Path) -> DaemonOptions {
    DaemonOptions::default()
        .env("TETON_PROBE_RAM_BYTES", (16 * GIB).to_string())
        .env("TETON_PROBE_DISK_BYTES", (500 * GIB).to_string())
        .env("TETON_PROBE_GPU", "apple-silicon")
        .env("TETON_CATALOG", catalog.display().to_string())
        .env("TETON_DOWNLOAD_RETRY_BASE_MS", "1")
}

/// The proposed entry's name, or `None` when the machine had nothing to offer.
fn proposed_name(proposal: &Value) -> Option<&str> {
    proposal["proposed"]["entry"]["name"].as_str()
}

/// The `request_id` a proposal must be answered with.
fn request_id(proposal: &Value) -> String {
    proposal["request_id"]
        .as_str()
        .unwrap_or_else(|| panic!("proposal carries no request_id: {proposal}"))
        .to_owned()
}

/// Every `model_lifecycle` reason text this client has seen, joined.
fn lifecycle_reasons(client: &Client) -> String {
    client
        .events_named("model_lifecycle")
        .iter()
        .filter_map(|e| e["stage"]["reason"].as_str())
        .collect::<Vec<_>>()
        .join(" | ")
}

/// Wait for the install to reach a terminal state and return
/// `(failure reasons, model/status)`.
///
/// Terminal is *either* outcome — a `model_lifecycle` reason (the daemon's
/// "this did not happen, and here is why") or verified weights. Waiting only for
/// the failure would make a test that expects one burn its whole timeout when
/// the code under test wrongly succeeds, and report the timeout instead of the
/// wrong success.
fn await_install_outcome(client: &mut Client, window: Duration) -> (String, Value) {
    let deadline = Instant::now() + window;
    loop {
        let reasons = lifecycle_reasons(client);
        let status = client.model_status();
        let settled = !reasons.is_empty()
            || status["install"]["status"].as_str() == Some("verified")
            || Instant::now() >= deadline;
        if settled {
            return (reasons, status);
        }
        client.drain_events(Duration::from_millis(25));
    }
}

/// A TCP port with nothing listening on it: bound to learn a free number, then
/// released. The honest stand-in for "the model host cannot be reached" (AC-10)
/// — a refused connection, not a slow one.
fn closed_port() -> u16 {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind to find a free port");
    let port = listener.local_addr().unwrap().port();
    drop(listener);
    port
}

// ===========================================================================
// AC-1 — the proposal is legible, and NOTHING is fetched before an answer.
// ===========================================================================

/// The load-bearing test of the whole REQ: zero bytes of model data move until a
/// human answers, and the machine's reasoning is legible to the client that has
/// to answer.
///
/// It also carries architecture D-3, which no other test proves end to end: a
/// prompt turn completes against a live provider **while the proposal is still
/// outstanding**. The gate withholds the tier, never the session.
///
/// The legibility half is asserted through `model/status` + `model/list` rather
/// than through the `model_selection_proposed` event, because that is the only
/// path a real client has — see
/// [`ac1_proposal_event_reaches_an_attached_client`], which is ignored and says
/// why.
#[test]
fn ac1_nothing_downloads_before_the_answer_and_the_machine_is_legible() {
    let models = fixture_models();
    let hf = MockHf::serving(&models);
    let provider = MockProvider::always(openai_turn("Remote answer.", None, 12, 4));

    let ws = Workspace::new("c-ac1");
    let catalog = ws.write_catalog(&fixture_catalog_toml(&models));
    ws.write_config(&format!(
        "{}[[providers]]\nid = \"remote\"\nkind = \"openai-compatible\"\nendpoint = \"{}\"\n",
        local_model_block(&hf.base_url(), false),
        provider.openai_endpoint(),
    ));

    let daemon = Daemon::spawn(&ws, consent_env(&catalog));
    let mut client = daemon.connect();

    // The gate ran and is waiting: a proposal is outstanding and unanswered.
    let id = client.await_pending_proposal(PROPOSAL_WINDOW);

    // --- BR-2: the hardware reasoning the client renders ---
    let list = client.model_list();
    let probe = &list["probe"];
    assert_eq!(probe["total_ram_bytes"].as_u64(), Some(16 * GIB), "{probe}");
    assert_eq!(
        probe["free_disk_bytes"].as_u64(),
        Some(500 * GIB),
        "{probe}"
    );
    assert_eq!(
        probe["gpu_class"].as_str(),
        Some("apple_silicon"),
        "{probe}"
    );
    assert_eq!(probe["chosen_band"].as_str(), Some("small"), "{probe}");
    let reason = probe["reason"].as_str().unwrap_or_default();
    assert!(
        reason.contains("RAM") && reason.contains("free disk") && reason.contains("band"),
        "the probe reason must explain the pick in plain language; got {reason:?}"
    );

    // --- BR-2/BR-3: every selectable entry, with its size and its RAM floor ---
    let rows = list["models"].as_array().expect("model rows");
    for model in &models {
        let row = rows
            .iter()
            .find(|r| r["entry"]["name"].as_str() == Some(model.name))
            .unwrap_or_else(|| panic!("{} missing from model/list: {list}", model.name));
        assert_eq!(
            row["entry"]["size_bytes"].as_u64(),
            Some(model.payload.len() as u64),
            "{row}"
        );
        assert_eq!(
            row["entry"]["ram_floor_bytes"].as_u64(),
            Some(model.ram_floor_bytes),
            "{row}"
        );
        assert!(
            row["fits_ram"].is_boolean() && row["fits_disk"].is_boolean(),
            "{row}"
        );
    }

    // --- AC-1: zero bytes, asserted against the host that would have served them ---
    assert_eq!(
        hf.artifact_request_count(),
        0,
        "AC-1 VIOLATION: the model host was asked for artifact bytes before the \
         user answered. Requests: {:?}",
        hf.requests()
    );

    // --- D-3: the session works remote-only while the proposal is outstanding ---
    let session = client.create_session("freeform", None);
    let turn = client.prompt(&session, "say hello");
    assert_eq!(
        turn["result"]["stop_reason"].as_str(),
        Some("end_turn"),
        "a session must complete remote-only while the local tier awaits a \
         decision (D-3); got {turn}"
    );
    assert!(
        provider.request_count() >= 1,
        "the turn must actually have gone to the remote provider"
    );
    assert_eq!(
        hf.artifact_request_count(),
        0,
        "AC-1 VIOLATION: running a session must not fetch model weights either"
    );
    let status = client.model_status();
    assert!(
        status["selection"].is_null(),
        "nothing may be recorded before an answer; got {status}"
    );

    // --- and only now, after the answer, do bytes move ---
    client.confirm_model(&id, json!({ "outcome": "accept" }));
    let installed = client.wait_for_install_status("verified", INSTALL_WINDOW);
    assert_eq!(
        installed["install"]["status"].as_str(),
        Some("verified"),
        "{installed}"
    );
    assert!(
        hf.artifact_request_count() >= 1,
        "after the answer the artifact must actually be fetched"
    );
    assert_eq!(
        installed["selection"]["model_name"].as_str(),
        Some("tiny-small"),
        "accepting must install the entry the probe picked; got {installed}"
    );

    assert_no_boundary_bytes();
}

/// **KNOWN GAP — the `model_selection_proposed` event is unreachable in the
/// current wiring, so this test is ignored rather than deleted or faked.**
///
/// `tetond`'s `main` spawns the consent flow *before* calling `server::serve`,
/// so the proposal is published before the daemon accepts its first connection.
/// No client can be subscribed in time; the event has no receiver, ever. Every
/// shipped client therefore recovers the prompt from
/// `model/status.pending_request_id` plus `model/list` (`teton`'s
/// `resolve_outstanding`) — which carries the probe and every catalog entry, but
/// **cannot name which entry the daemon proposed**, nor its size and RAM floor
/// as *the proposal*. That is the half of AC-1 this suite cannot claim.
///
/// The payload itself is correct and unit-tested (`model_consent::build_proposal`,
/// and the protocol round-trip in `teton-protocol::events`); what is missing is
/// delivery. Fixing it needs a daemon-side replay of the outstanding proposal to
/// an attaching client *and* client-side de-duplication against the
/// `model/status` path, or the prompt would be raised twice — a change to
/// TASK-004/TASK-007's design, not something to smuggle in under an acceptance
/// suite. Un-ignore this the day that lands.
#[test]
#[ignore = "the daemon publishes model_selection_proposed before it accepts \
            connections, so no client can receive it (REQ-547 gap; see the doc \
            comment)"]
fn ac1_proposal_event_reaches_an_attached_client() {
    let models = fixture_models();
    let hf = MockHf::serving(&models);

    let ws = Workspace::new("c-ac1e");
    let catalog = ws.write_catalog(&fixture_catalog_toml(&models));
    ws.write_config(&local_model_block(&hf.base_url(), false));

    let daemon = Daemon::spawn(&ws, consent_env(&catalog));
    let mut client = daemon.connect();
    let proposal = client.await_proposal(PROPOSAL_WINDOW);

    let small = &models[0];
    assert_eq!(proposed_name(&proposal), Some(small.name), "{proposal}");
    let entry = &proposal["proposed"]["entry"];
    assert_eq!(
        entry["size_bytes"].as_u64(),
        Some(small.payload.len() as u64),
        "{entry}"
    );
    assert_eq!(
        entry["ram_floor_bytes"].as_u64(),
        Some(small.ram_floor_bytes),
        "{entry}"
    );
    assert_eq!(
        proposal["proposed"]["required_disk_bytes"].as_u64(),
        Some(small.payload.len() as u64 + GIB),
        "the proposal must quote the disk the install needs, margin included"
    );
    let alternatives: Vec<&str> = proposal["alternatives"]
        .as_array()
        .expect("alternatives array")
        .iter()
        .filter_map(|a| a["name"].as_str())
        .collect();
    assert!(
        alternatives.contains(&"tiny-mid") && alternatives.contains(&"tiny-large"),
        "every other catalog entry must be selectable; got {alternatives:?}"
    );
    assert_eq!(
        request_id(&proposal),
        client.await_pending_proposal(PROPOSAL_WINDOW)
    );
}

// ===========================================================================
// AC-2 — accept → download → verify → atomic install → ready, with progress.
// ===========================================================================

/// Accepting reaches installed, verified weights, and the client can watch it
/// happen on the `model_lifecycle` stream.
///
/// The last clause of AC-2 — "reaches a working local session" — is **not**
/// asserted here and cannot be: these bytes are not a model and `--features
/// llama` is not built in CI. That claim is AC-13's, and it is signed off by a
/// human in `docs/manual-verification.md`, not by this test.
#[test]
fn ac2_accepting_downloads_verifies_and_installs_atomically() {
    let models = fixture_models();
    let hf = MockHf::serving(&models);

    let ws = Workspace::new("c-ac2");
    let catalog = ws.write_catalog(&fixture_catalog_toml(&models));
    ws.write_config(&local_model_block(&hf.base_url(), false));

    let daemon = Daemon::spawn(&ws, consent_env(&catalog));
    let mut client = daemon.connect();
    let id = client.await_pending_proposal(PROPOSAL_WINDOW);
    client.confirm_model(&id, json!({ "outcome": "accept" }));

    let status = client.wait_for_install_status("verified", INSTALL_WINDOW);
    assert_eq!(
        status["install"]["status"].as_str(),
        Some("verified"),
        "{status}"
    );
    assert_eq!(
        status["selection"]["model_name"].as_str(),
        Some("tiny-small"),
        "{status}"
    );
    assert_eq!(
        status["selection"]["source"].as_str(),
        Some("probe"),
        "{status}"
    );

    // BR-9: the bytes at the final path are the catalog's bytes, and no partial
    // file was left behind claiming to be them.
    let installed = ws.weights_dir().join("tiny-small.gguf");
    let on_disk = std::fs::read(&installed).expect("verified weights are installed");
    assert_eq!(
        on_disk, models[0].payload,
        "the installed file must be exactly the artifact that was verified"
    );
    assert!(
        !ws.weights_dir().join("tiny-small.gguf.part").exists(),
        "the partial download must be gone once it is installed"
    );

    // AC-2: progress is rendered from `model_lifecycle`, not inferred.
    client.drain_events(Duration::from_millis(200));
    let downloads: Vec<&Value> = client
        .events_named("model_lifecycle")
        .into_iter()
        .filter(|e| e["stage"]["stage"].as_str() == Some("download"))
        .collect();
    assert!(
        downloads
            .iter()
            .any(|e| e["stage"]["downloaded_bytes"].as_u64() == Some(0)),
        "the transfer's start must be observable; saw {downloads:?}"
    );
    assert!(
        downloads.iter().any(
            |e| e["stage"]["downloaded_bytes"].as_u64() == Some(models[0].payload.len() as u64)
        ),
        "the transfer's completion must be observable; saw {downloads:?}"
    );
    assert!(
        client
            .events_named("model_lifecycle")
            .iter()
            .any(|e| e["stage"]["stage"].as_str() == Some("ready")),
        "the install must announce `ready`; saw {:?}",
        client.events_named("model_lifecycle")
    );
}

// ===========================================================================
// AC-3 — overriding installs the chosen entry; above the RAM floor needs two.
// ===========================================================================

#[test]
fn ac3_override_installs_the_chosen_entry_and_warns_above_the_ram_floor() {
    let models = fixture_models();

    // --- half one: a plain override installs the *chosen* entry ---
    {
        let hf = MockHf::serving(&models);
        let ws = Workspace::new("c-ac3a");
        let catalog = ws.write_catalog(&fixture_catalog_toml(&models));
        ws.write_config(&local_model_block(&hf.base_url(), false));

        let daemon = Daemon::spawn(&ws, consent_env(&catalog));
        let mut client = daemon.connect();
        let id = client.await_pending_proposal(PROPOSAL_WINDOW);

        client.confirm_model(&id, json!({ "outcome": "choose", "name": "tiny-mid" }));
        let status = client.wait_for_install_status("verified", INSTALL_WINDOW);
        assert_eq!(
            status["selection"]["model_name"].as_str(),
            Some("tiny-mid"),
            "{status}"
        );
        assert_eq!(
            status["selection"]["source"].as_str(),
            Some("user_override"),
            "{status}"
        );

        let fetched = hf.requests();
        assert!(
            fetched.iter().any(|p| p.contains("tiny-mid-q4_k_m.gguf")),
            "the chosen entry must be the one fetched; got {fetched:?}"
        );
        assert!(
            !fetched.iter().any(|p| p.contains("tiny-small-q4_k_m.gguf")),
            "the *proposed* entry must not be fetched once it was overridden; got {fetched:?}"
        );
        assert!(ws.weights_dir().join("tiny-mid.gguf").exists());
        assert!(!ws.weights_dir().join("tiny-small.gguf").exists());
    }

    // --- half two: above the RAM floor is refused until confirmed twice (BR-3) ---
    {
        let hf = MockHf::serving(&models);
        let ws = Workspace::new("c-ac3b");
        let catalog = ws.write_catalog(&fixture_catalog_toml(&models));
        ws.write_config(&local_model_block(&hf.base_url(), false));

        let daemon = Daemon::spawn(&ws, consent_env(&catalog));
        let mut client = daemon.connect();
        let id = client.await_pending_proposal(PROPOSAL_WINDOW);

        // `tiny-large` needs 21 GiB on a 16 GiB machine.
        let refused =
            client.confirm_model(&id, json!({ "outcome": "choose", "name": "tiny-large" }));
        let message = refused["error"]["message"].as_str().unwrap_or_default();
        assert!(
            message.contains("RAM") && message.contains("confirmation"),
            "an above-RAM-floor pick must be refused with an explicit warning; got {refused}"
        );
        assert_eq!(
            hf.artifact_request_count(),
            0,
            "a refused choice must fetch nothing"
        );
        assert!(
            client.model_status()["pending_request_id"].is_string(),
            "a refused choice must leave the proposal open to answer again"
        );

        // The second confirmation is the user's call, and it is honoured.
        let accepted = client.confirm_model(
            &id,
            json!({
                "outcome": "choose",
                "name": "tiny-large",
                "confirmed_above_ram_floor": true,
            }),
        );
        assert!(accepted.get("result").is_some(), "{accepted}");
        let status = client.wait_for_install_status("verified", INSTALL_WINDOW);
        assert_eq!(
            status["selection"]["model_name"].as_str(),
            Some("tiny-large"),
            "{status}"
        );
    }
}

// ===========================================================================
// AC-4 — declining is persisted and never re-litigated.
// ===========================================================================

#[test]
fn ac4_declining_is_remote_only_persisted_and_never_re_prompted() {
    let models = fixture_models();
    let hf = MockHf::serving(&models);

    let ws = Workspace::new("c-ac4");
    let catalog = ws.write_catalog(&fixture_catalog_toml(&models));
    ws.write_config(&local_model_block(&hf.base_url(), false));

    {
        let daemon = Daemon::spawn(&ws, consent_env(&catalog));
        let mut client = daemon.connect();
        let id = client.await_pending_proposal(PROPOSAL_WINDOW);
        client.confirm_model(&id, json!({ "outcome": "decline" }));

        let decided = client
            .wait_for_event("model_selection_decided", Duration::from_secs(5))
            .expect("declining must be announced");
        assert_eq!(decided["declined_local"].as_bool(), Some(true), "{decided}");

        let status = client.model_status();
        assert_eq!(
            status["selection"]["declined_local"].as_bool(),
            Some(true),
            "{status}"
        );
        assert_eq!(
            hf.artifact_request_count(),
            0,
            "declining must fetch nothing"
        );
    }

    // A second daemon over the same state directory: the question is settled.
    let daemon = Daemon::spawn(&ws, consent_env(&catalog));
    let mut client = daemon.connect();
    client.drain_events(Duration::from_millis(500));
    assert!(
        !client.saw_event("model_selection_proposed"),
        "BR-4 VIOLATION: a declined machine was re-prompted on the next start"
    );
    let status = client.model_status();
    assert_eq!(
        status["selection"]["declined_local"].as_bool(),
        Some(true),
        "the decline must survive a restart; got {status}"
    );
    assert!(
        status["pending_request_id"].is_null(),
        "no proposal may be outstanding on a declined machine; got {status}"
    );
    assert_eq!(hf.artifact_request_count(), 0);
}

// ===========================================================================
// AC-5 — auto-accept completes a first run with no prompt and no input.
// ===========================================================================

#[test]
fn ac5_auto_accept_completes_a_first_run_unattended() {
    let models = fixture_models();
    let hf = MockHf::serving(&models);

    let ws = Workspace::new("c-ac5");
    let catalog = ws.write_catalog(&fixture_catalog_toml(&models));
    ws.write_config(&local_model_block(&hf.base_url(), true));

    let daemon = Daemon::spawn(&ws, consent_env(&catalog));
    let mut client = daemon.connect();

    // No answer is ever sent on this connection.
    let status = client.wait_for_install_status("verified", INSTALL_WINDOW);
    assert_eq!(
        status["install"]["status"].as_str(),
        Some("verified"),
        "the unattended path must reach installed weights with no input; got {status}"
    );
    assert_eq!(
        status["selection"]["source"].as_str(),
        Some("auto_accept"),
        "the decision must be recorded as auto-accepted, not as a user answer; got {status}"
    );
    assert!(
        status["pending_request_id"].is_null(),
        "auto-accept must leave no prompt outstanding; got {status}"
    );

    client.drain_events(Duration::from_millis(200));
    assert!(
        !client.saw_event("model_selection_proposed"),
        "BR-5 VIOLATION: auto-accept published a proposal there was nobody to answer"
    );
}

// ===========================================================================
// AC-6 — insufficient disk refuses BEFORE any bytes are fetched.
// ===========================================================================

#[test]
fn ac6_insufficient_disk_refuses_before_fetching_naming_both_figures() {
    let models = fixture_models();
    let hf = MockHf::serving(&models);

    let ws = Workspace::new("c-ac6");
    let catalog = ws.write_catalog(&fixture_catalog_toml(&models));
    ws.write_config(&local_model_block(&hf.base_url(), false));

    let daemon = Daemon::spawn(
        &ws,
        // The volume the installer is about to write to has 4 KiB free.
        consent_env(&catalog).env("TETON_DISK_FREE_BYTES", "4096"),
    );
    let mut client = daemon.connect();
    let id = client.await_pending_proposal(PROPOSAL_WINDOW);
    client.confirm_model(&id, json!({ "outcome": "accept" }));

    let (reason, _status) = await_install_outcome(&mut client, INSTALL_WINDOW);
    assert!(
        reason.contains("not enough free disk space"),
        "the refusal must say what went wrong; got {reason:?}"
    );
    assert!(
        reason.contains("needed") && reason.contains("available"),
        "AC-6 requires the refusal to name required *and* available space; got {reason:?}"
    );
    assert_eq!(
        hf.artifact_request_count(),
        0,
        "AC-6 VIOLATION: bytes were fetched despite an impossible disk requirement"
    );
    assert!(
        !ws.weights_dir().join("tiny-small.gguf").exists(),
        "a refused install must leave nothing installed"
    );
}

// ===========================================================================
// AC-7 — a corrupt artifact is discarded and never installed.
// ===========================================================================

/// The host serves the right *number* of bytes and the wrong bytes, so only the
/// SHA-256 can tell — which is exactly the failure BR-6 exists for.
#[test]
fn ac7_a_corrupt_download_is_discarded_and_never_installed() {
    let models = fixture_models();
    let hf = MockHf::corrupting(&models);

    let ws = Workspace::new("c-ac7");
    let catalog = ws.write_catalog(&fixture_catalog_toml(&models));
    ws.write_config(&local_model_block(&hf.base_url(), false));

    let daemon = Daemon::spawn(&ws, consent_env(&catalog));
    let mut client = daemon.connect();
    let id = client.await_pending_proposal(PROPOSAL_WINDOW);
    client.confirm_model(&id, json!({ "outcome": "accept" }));

    let (reason, status) = await_install_outcome(&mut client, INSTALL_WINDOW);
    assert!(
        hf.artifact_request_count() >= 1,
        "the test is only meaningful if the artifact was actually fetched"
    );

    // BR-9 first, because it is the claim: the loadable path never held bytes
    // that failed their digest, and the install state never said otherwise.
    let installed = ws.weights_dir().join("tiny-small.gguf");
    assert!(
        !installed.exists(),
        "AC-7 VIOLATION: an artifact that failed its digest was installed at {}",
        installed.display()
    );
    assert_ne!(
        status["install"]["status"].as_str(),
        Some("verified"),
        "AC-7 VIOLATION: install state reported `verified` for weights that never verified; \
         got {status}"
    );
    assert!(
        reason.contains("integrity check") && reason.contains("discarded"),
        "a corrupt artifact must surface as corruption, in its own words; got {reason:?}"
    );

    // The AC's parenthetical, directly: a truncated file at the loadable path
    // reports `corrupt`, never `verified`.
    std::fs::create_dir_all(ws.weights_dir()).unwrap();
    std::fs::write(&installed, &models[0].payload[..1024]).unwrap();
    let truncated = client.model_status();
    assert_eq!(
        truncated["install"]["status"].as_str(),
        Some("corrupt"),
        "AC-7 VIOLATION: a truncated artifact must never read as verified; got {truncated}"
    );
}

// ===========================================================================
// AC-8 — the catalog-integrity check actually detects a dishonest catalog.
// ===========================================================================

/// Runs the **real** gate (`tools/refresh-catalog.py --check`) against a mock
/// HuggingFace metadata API, twice: once telling the truth, once drifted.
///
/// Asserting the tool exists proves nothing; asserting it *fails* when upstream
/// disagrees is the claim AC-8 makes. The network run against the real
/// huggingface.co stays where it belongs — TASK-006's dedicated CI job, the only
/// network-touching job in this REQ.
#[test]
fn ac8_the_catalog_integrity_check_passes_on_truth_and_fails_on_drift() {
    if !python3_available() {
        eprintln!("skipping AC-8: python3 is not on PATH");
        return;
    }
    let catalog = Catalog::bundled();

    // --- truthful upstream: the gate verifies ---
    let honest = MockHf::start(MockHfConfig {
        tree: bundled_tree(&catalog, None),
        ..MockHfConfig::default()
    });
    let verified = run_catalog_check(&honest.base_url(), None);
    assert_eq!(
        verified.code,
        Some(0),
        "the gate must verify a catalog that agrees with upstream.\nstdout:\n{}\nstderr:\n{}",
        verified.stdout,
        verified.stderr
    );
    assert!(
        verified.stdout.contains("VERIFIED"),
        "stdout:\n{}",
        verified.stdout
    );

    // --- drifted upstream: the gate reports a MISMATCH naming the field ---
    let drifted_name = catalog.models[0].name.clone();
    let drifted = MockHf::start(MockHfConfig {
        tree: bundled_tree(&catalog, Some(&drifted_name)),
        ..MockHfConfig::default()
    });
    let mismatch = run_catalog_check(&drifted.base_url(), None);
    assert_eq!(
        mismatch.code,
        Some(1),
        "AC-8 VIOLATION: the gate accepted a catalog whose digest disagrees with \
         upstream.\nstdout:\n{}\nstderr:\n{}",
        mismatch.stdout,
        mismatch.stderr
    );
    assert!(
        mismatch.stderr.contains("MISMATCH")
            && mismatch.stderr.contains(&drifted_name)
            && mismatch.stderr.contains("sha256"),
        "the failure must name the entry and the field; stderr:\n{}",
        mismatch.stderr
    );
}

// ===========================================================================
// AC-9 — `model/list`, `model/set`, `model/status` over the wire.
// ===========================================================================

#[test]
fn ac9_model_list_set_and_status_report_and_change_the_selection() {
    let models = fixture_models();
    let hf = MockHf::serving(&models);

    let ws = Workspace::new("c-ac9");
    let catalog = ws.write_catalog(&fixture_catalog_toml(&models));
    ws.write_config(&local_model_block(&hf.base_url(), false));

    let daemon = Daemon::spawn(&ws, consent_env(&catalog));
    let mut client = daemon.connect();
    let id = client.await_pending_proposal(PROPOSAL_WINDOW);
    client.confirm_model(&id, json!({ "outcome": "accept" }));
    client.wait_for_install_status("verified", INSTALL_WINDOW);

    // --- model/list: the catalog, each entry's fit, and the selection ---
    let list = client.model_list();
    let rows = list["models"].as_array().expect("model rows");
    assert_eq!(rows.len(), 3, "{list}");
    let fit = |name: &str| -> (bool, bool) {
        let row = rows
            .iter()
            .find(|r| r["entry"]["name"].as_str() == Some(name))
            .unwrap_or_else(|| panic!("{name} missing from model/list: {list}"));
        (
            row["fits_ram"].as_bool().unwrap(),
            row["fits_disk"].as_bool().unwrap(),
        )
    };
    assert_eq!(fit("tiny-small"), (true, true), "{list}");
    assert_eq!(fit("tiny-mid"), (true, true), "{list}");
    assert!(
        !fit("tiny-large").0,
        "a 21 GiB floor must not fit a 16 GiB machine; {list}"
    );
    assert_eq!(
        list["selection"]["model_name"].as_str(),
        Some("tiny-small"),
        "{list}"
    );
    assert_eq!(
        list["probe"]["total_ram_bytes"].as_u64(),
        Some(16 * GIB),
        "model/list must describe the same machine the proposal did; {list}"
    );

    // --- model/set: change it post-first-run ---
    let set = client.call("model/set", json!({ "name": "tiny-mid" }));
    assert_eq!(
        set["result"]["selection"]["model_name"].as_str(),
        Some("tiny-mid"),
        "{set}"
    );

    // BR-3 still applies after first run.
    let refused = client.call("model/set", json!({ "name": "tiny-large" }));
    assert!(
        refused["error"]["message"]
            .as_str()
            .unwrap_or_default()
            .contains("RAM"),
        "{refused}"
    );
    let unknown = client.call("model/set", json!({ "name": "no-such-model" }));
    assert!(
        unknown["error"]["message"]
            .as_str()
            .unwrap_or_default()
            .contains("no-such-model"),
        "{unknown}"
    );

    // --- model/status: the decision and the weights' state ---
    let status = client.wait_for_install_status("verified", INSTALL_WINDOW);
    assert_eq!(
        status["selection"]["model_name"].as_str(),
        Some("tiny-mid"),
        "{status}"
    );
    assert_eq!(
        status["install"]["model_name"].as_str(),
        Some("tiny-mid"),
        "{status}"
    );
    // BR-11: no filesystem path ever crosses the protocol boundary.
    assert!(
        !status.to_string().contains("/tmp/"),
        "BR-11 VIOLATION: an install path reached a protocol payload: {status}"
    );
}

// ===========================================================================
// AC-10 — an offline accept fails cleanly, is not a decline, and re-prompts.
// ===========================================================================

#[test]
fn ac10_an_offline_accept_errors_cleanly_and_the_next_run_succeeds() {
    let models = fixture_models();
    let ws = Workspace::new("c-ac10");
    let catalog = ws.write_catalog(&fixture_catalog_toml(&models));

    // --- run one: nothing is listening where the weights live ---
    ws.write_config(&local_model_block(
        &format!("http://127.0.0.1:{}", closed_port()),
        false,
    ));
    {
        let daemon = Daemon::spawn(&ws, consent_env(&catalog));
        let mut client = daemon.connect();
        let id = client.await_pending_proposal(PROPOSAL_WINDOW);
        client.confirm_model(&id, json!({ "outcome": "accept" }));

        let (reason, status) = await_install_outcome(&mut client, INSTALL_WINDOW);
        assert!(
            reason.contains("could not download the model weights"),
            "an unreachable host must read as a network failure; got {reason:?}"
        );
        assert!(
            reason.contains("Nothing was installed"),
            "the error must say the install did not happen; got {reason:?}"
        );
        assert!(
            !reason.contains("integrity check"),
            "a network failure must never be reported as corruption; got {reason:?}"
        );

        assert_eq!(
            status["selection"]["declined_local"].as_bool(),
            Some(false),
            "AC-10 VIOLATION: a failed install was recorded as a decline; got {status}"
        );
        assert_ne!(
            status["install"]["status"].as_str(),
            Some("verified"),
            "{status}"
        );
        assert!(
            !ws.weights_dir().join("tiny-small.gguf").exists(),
            "a failed download must leave no installed file"
        );
    }

    // --- run two: connectivity is back, so the daemon re-proposes and succeeds ---
    let hf = MockHf::serving(&models);
    ws.write_config(&local_model_block(&hf.base_url(), false));
    let daemon = Daemon::spawn(&ws, consent_env(&catalog));
    let mut client = daemon.connect();
    let id = client.await_pending_proposal(PROPOSAL_WINDOW);
    client.confirm_model(&id, json!({ "outcome": "accept" }));
    let status = client.wait_for_install_status("verified", INSTALL_WINDOW);
    assert_eq!(
        status["install"]["status"].as_str(),
        Some("verified"),
        "the retry once online must complete; got {status}"
    );
    assert_eq!(
        status["selection"]["model_name"].as_str(),
        Some("tiny-small"),
        "BR-12 VIOLATION: missing weights must re-open the question, not be assumed decided; \
         got {status}"
    );
}

// ===========================================================================
// AC-12 — moving refs rejected, base-URL override honoured, 429 backed off.
// ===========================================================================

#[test]
fn ac12_moving_ref_rejected_base_url_mirrored_and_rate_limit_backed_off() {
    let models = fixture_models();

    // --- (a) a moving ref fails the integrity check, actionably (BR-15) ---
    if python3_available() {
        let committed =
            std::fs::read_to_string(repo_root().join("crates/teton-inference/data/models.toml"))
                .expect("the committed catalog is readable");
        let pinned = &Catalog::bundled().models[0].revision.clone();
        let drifted = committed.replacen(
            &format!("revision = \"{pinned}\""),
            "revision = \"main\"",
            1,
        );
        assert_ne!(drifted, committed, "the fixture must actually differ");

        let ws = Workspace::new("c-ac12a");
        let fixture = ws.root.join("moving-ref.toml");
        std::fs::write(&fixture, drifted).unwrap();

        let host = MockHf::start(MockHfConfig::default());
        let result = run_catalog_check(&host.base_url(), Some(&fixture));
        assert_eq!(
            result.code,
            Some(1),
            "AC-12 VIOLATION: a catalog pinning a moving ref passed the integrity \
             check.\nstdout:\n{}\nstderr:\n{}",
            result.stdout,
            result.stderr
        );
        assert!(
            result.stderr.contains("moving ref") && result.stderr.contains("--update"),
            "the refusal must name the hazard and the remedy; stderr:\n{}",
            result.stderr
        );
    } else {
        eprintln!("skipping AC-12(a): python3 is not on PATH");
    }

    // --- (b) the base-URL override redirects the fetch to the mirror (BR-16) ---
    {
        // The mirror answers `302` to a second host, the way HuggingFace hands
        // an LFS artifact to its CDN — so this also exercises the redirect the
        // credential-free download client is allowed to follow (BR-14/D-2).
        let cdn = MockHf::serving(&models);
        let mirror = MockHf::start(MockHfConfig {
            artifact: HfArtifact::RedirectTo(cdn.base_url()),
            files: file_map(&models),
            ..MockHfConfig::default()
        });

        let ws = Workspace::new("c-ac12b");
        let catalog = ws.write_catalog(&fixture_catalog_toml(&models));
        ws.write_config(&local_model_block(&mirror.base_url(), false));

        let daemon = Daemon::spawn(&ws, consent_env(&catalog));
        let mut client = daemon.connect();
        let id = client.await_pending_proposal(PROPOSAL_WINDOW);
        client.confirm_model(&id, json!({ "outcome": "accept" }));
        let status = client.wait_for_install_status("verified", INSTALL_WINDOW);
        assert_eq!(
            status["install"]["status"].as_str(),
            Some("verified"),
            "a mirrored, redirected fetch must complete; got {status}"
        );

        let expected_path = format!(
            "/{}/resolve/{}/{}",
            models[0].repo, models[0].revision, models[0].file
        );
        assert!(
            mirror.requests().contains(&expected_path),
            "the mirror must be asked for the *same* repo/revision/file path — \
             that is what keeps the pinned digest meaningful; got {:?}",
            mirror.requests()
        );
        assert!(
            cdn.requests().contains(&expected_path),
            "the 302 to the CDN host must be followed; got {:?}",
            cdn.requests()
        );
    }

    // --- (c) a 429 is retried with backoff, then reported as rate-limiting ---
    {
        // Two rate-limited answers, then the bytes. `Retry-After` is absent, so
        // the real ladder decides the delay; a 120 ms base makes that delay
        // measurable instead of theoretical.
        let hf = MockHf::start(MockHfConfig {
            files: file_map(&models),
            fail_first: 2,
            fail_status: 429,
            fail_retry_after_secs: None,
            ..MockHfConfig::default()
        });

        let ws = Workspace::new("c-ac12c");
        let catalog = ws.write_catalog(&fixture_catalog_toml(&models));
        ws.write_config(&local_model_block(&hf.base_url(), false));

        let daemon = Daemon::spawn(
            &ws,
            consent_env(&catalog).env("TETON_DOWNLOAD_RETRY_BASE_MS", "120"),
        );
        let mut client = daemon.connect();
        let id = client.await_pending_proposal(PROPOSAL_WINDOW);
        let started = Instant::now();
        client.confirm_model(&id, json!({ "outcome": "accept" }));
        let status = client.wait_for_install_status("verified", INSTALL_WINDOW);
        let elapsed = started.elapsed();

        assert_eq!(
            status["install"]["status"].as_str(),
            Some("verified"),
            "a rate-limited transfer must recover once the host relents; got {status}"
        );
        // Equal jitter samples [delay/2, delay], so two retries at a 120 ms base
        // cost at least 60 + 120 ms.
        assert!(
            elapsed >= Duration::from_millis(150),
            "AC-12 VIOLATION: the 429s were retried with no backoff at all ({elapsed:?})"
        );
        assert!(
            hf.artifact_request_count() >= 3,
            "both rate-limited attempts and the successful one must be visible; got {:?}",
            hf.requests()
        );
    }

    // --- (c continued) a host that never relents reads as rate-limiting ---
    {
        // `Retry-After: 0` is honoured verbatim, so the whole real ladder runs
        // without spending its seconds.
        let hf = MockHf::start(MockHfConfig {
            artifact: HfArtifact::Status {
                code: 429,
                retry_after_secs: Some(0),
            },
            files: file_map(&models),
            ..MockHfConfig::default()
        });

        let ws = Workspace::new("c-ac12d");
        let catalog = ws.write_catalog(&fixture_catalog_toml(&models));
        ws.write_config(&local_model_block(&hf.base_url(), false));

        let daemon = Daemon::spawn(&ws, consent_env(&catalog));
        let mut client = daemon.connect();
        let id = client.await_pending_proposal(PROPOSAL_WINDOW);
        client.confirm_model(&id, json!({ "outcome": "accept" }));

        let (reason, _status) = await_install_outcome(&mut client, INSTALL_WINDOW);
        assert!(
            reason.contains("rate-limiting"),
            "a persistent 429 must be reported as rate-limiting; got {reason:?}"
        );
        assert!(
            !reason.contains("integrity check"),
            "AC-12 VIOLATION: rate-limiting was reported as a corrupt download; got {reason:?}"
        );
        assert!(
            hf.artifact_request_count() > 1,
            "a 429 must be retried, not given up on immediately; got {:?}",
            hf.artifact_request_count()
        );
    }
}

// ---------------------------------------------------------------------------
// Catalog-integrity gate support (AC-8, AC-12a)
// ---------------------------------------------------------------------------

/// The repository root, so a test can run the repo's own tooling.
fn repo_root() -> std::path::PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("..")
        .join("..")
        .canonicalize()
        .expect("the crate manifest dir resolves to a repository checkout")
}

fn python3_available() -> bool {
    Command::new("python3")
        .arg("--version")
        .output()
        .map(|out| out.status.success())
        .unwrap_or(false)
}

/// The outcome of one `refresh-catalog.py` run.
struct CheckRun {
    code: Option<i32>,
    stdout: String,
    stderr: String,
}

/// Run the real integrity gate against `endpoint`, optionally over a fixture
/// catalog instead of the committed one.
fn run_catalog_check(endpoint: &str, catalog: Option<&Path>) -> CheckRun {
    let mut command = Command::new("python3");
    command
        .arg(repo_root().join("tools/refresh-catalog.py"))
        .arg("--check")
        .env("HF_ENDPOINT", endpoint);
    if let Some(path) = catalog {
        command.arg("--catalog").arg(path);
    }
    let output = command.output().expect("run tools/refresh-catalog.py");
    CheckRun {
        code: output.status.code(),
        stdout: String::from_utf8_lossy(&output.stdout).into_owned(),
        stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
    }
}

/// Mock LFS tree responses for every bundled catalog entry.
///
/// `drift` names the entry whose `oid` should disagree with the catalog — the
/// upstream-changed-under-a-pin scenario the gate exists to catch.
fn bundled_tree(
    catalog: &Catalog,
    drift: Option<&str>,
) -> std::collections::BTreeMap<String, Vec<HfTreeFile>> {
    let mut tree = std::collections::BTreeMap::new();
    for entry in &catalog.models {
        let source = HfSource::parse(&entry.url).expect("every catalog URL is a resolve URL");
        let oid = if drift == Some(entry.name.as_str()) {
            // A different, well-formed digest: the shape of a real drift.
            format!("{}f", &entry.sha256[..entry.sha256.len() - 1])
        } else {
            entry.sha256.clone()
        };
        tree.insert(
            format!("{}@{}", source.repo, source.revision),
            vec![HfTreeFile {
                path: source.file.to_owned(),
                oid,
                size: entry.size_bytes,
            }],
        );
    }
    tree
}
