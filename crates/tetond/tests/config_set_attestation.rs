//! REQ-576 AC-1/AC-2 over the real socket: a presence-refused `config/set` writes
//! nothing and swaps nothing (proven by the bytes on disk and the live config —
//! not inferred from the error code, LESSON-519), while an **attested** one does
//! apply. The two are matched pairs per variant on purpose: the accept test
//! proves the exact payload is persistable, which is what makes the refuse test's
//! byte-identical assertion non-vacuous (a payload that could never write would
//! leave the file unchanged regardless of the gate).
//!
//! `config/set` became the fourth REQ-570 BR-10(b) daemon-wide commitment
//! (TASK-140). Its refusal is covered in-process by the shared
//! `only_a_daemon_wide_commitment_demands_presence` harness; what that harness
//! cannot supply is a **real config file** to inspect. This suite spawns a real
//! `teton-code` with a config on disk and either a **present-but-refusing**
//! verifier (`TETON_PRESENCE_ACCEPT=fail` → `AlwaysFailsVerifier`) or an
//! **accepting** one (`=1` → `AcceptingVerifier`) — the REQ-575 seams, which ride
//! the `TETON_TEST_SEAMS` master switch a release build refuses — for the two
//! egress/privacy-critical variants: `RegisterProvider` (an egress endpoint) and
//! `SetPrivacyBoundary` (the privacy boundary itself).

use serde_json::{json, Value};

#[path = "e2e/harness.rs"]
mod harness;

use harness::{tier_block, Daemon, DaemonOptions, Workspace};

const GIB: u64 = 1024 * 1024 * 1024;

/// A deterministic machine so a probe cannot pick a model and spend the budget,
/// with the presence seam set to `mode` (`"fail"` or `"1"`).
fn probe_with_presence(mode: &str) -> DaemonOptions {
    DaemonOptions::default()
        .env("TETON_PROBE_RAM_BYTES", (16 * GIB).to_string())
        .env("TETON_PROBE_DISK_BYTES", (500 * GIB).to_string())
        .env("TETON_PROBE_GPU", "apple-silicon")
        .env("TETON_TEST_SEAMS", "1")
        .env("TETON_PRESENCE_ACCEPT", mode)
}

/// A minimal valid config with one local provider bound to every tier — enough to
/// start the daemon, and deliberately WITHOUT the provider/boundary the tests add,
/// so a successful `config/set` visibly changes the file and a refused one does
/// not.
fn base_config() -> String {
    let mut c = String::new();
    c.push_str("[[providers]]\nid = \"local\"\nkind = \"local\"\n\n");
    for tier in ["reflex", "scan", "build", "think"] {
        c.push_str(&tier_block(tier, "local"));
    }
    c.push_str("[privacy]\nredact = false\n\n");
    c
}

/// A `RegisterProvider` that actually persists (kind/model/auth_ref all valid and
/// validation-surviving — mirrors the known-good payload in
/// `event_response_ordering.rs`), aimed at a token an attacker would want: a new
/// egress endpoint.
fn register_attacker_egress() -> Value {
    json!({ "update": {
        "op": "register_provider",
        "id": "attacker-egress",
        "kind": "openai-compatible",
        "endpoint": "http://127.0.0.1:9/v1/chat/completions",
        "model": "exfil-model",
        "auth_ref": "env:TETON_REQ576_TEST_CREDENTIAL_ABSENT",
    }})
}

/// A `SetPrivacyBoundary` that persists — the variant the spec singles out as
/// directly mutating the privacy promise.
fn set_secret_boundary() -> Value {
    json!({ "update": {
        "op": "set_privacy_boundary",
        "path_glob": "secrets/**",
        "mode": "local_only",
    }})
}

const ATTESTATION_FAILED: i64 = teton_protocol::jsonrpc::error_code::ATTESTATION_FAILED;

// ---------------------------------------------------------------------------
// RegisterProvider — refuse / accept pair
// ---------------------------------------------------------------------------

/// **AC-1 (RegisterProvider refused): writes nothing, swaps nothing.** Proven by
/// on-disk bytes (before == after) and the live `config/get` snapshot, not the
/// error code. Non-vacuous *because* `an_attested_register_provider_writes` below
/// proves the same payload DOES write when presence is satisfied.
#[test]
fn a_presence_refused_register_provider_writes_nothing() {
    let ws = Workspace::new("configset-register-refused");
    ws.write_config(&base_config());
    let daemon = Daemon::spawn(&ws, probe_with_presence("fail"));
    let mut client = daemon.connect();
    let before = std::fs::read(&ws.config_path).expect("config exists");

    let refused = client.call("config/set", register_attacker_egress());
    assert_eq!(
        refused["error"]["code"].as_i64(),
        Some(ATTESTATION_FAILED),
        "config/set RegisterProvider must be refused at the presence gate: \
         {refused}\ndaemon log:\n{}",
        daemon.log()
    );
    assert_eq!(
        std::fs::read(&ws.config_path).ok().as_deref(),
        Some(before.as_slice()),
        "AC-1: a refused RegisterProvider must leave config.toml byte-identical"
    );
    let snapshot = client.call("config/get", json!({}));
    assert!(
        !serde_json::to_string(&snapshot)
            .unwrap()
            .contains("attacker-egress"),
        "AC-1: the refused provider must not appear in the running config: {snapshot}"
    );
}

/// **AC-2 (RegisterProvider attested): applies through the full gate/RPC path.**
/// This is the accepting-path proof (config/set lands when presence is satisfied)
/// AND the non-vacuity anchor for the refuse test above — the identical payload
/// changes the bytes and appears in the live config.
#[test]
fn an_attested_register_provider_writes() {
    let ws = Workspace::new("configset-register-attested");
    ws.write_config(&base_config());
    let daemon = Daemon::spawn(&ws, probe_with_presence("1"));
    let mut client = daemon.connect();
    let before = std::fs::read(&ws.config_path).expect("config exists");

    let applied = client.call("config/set", register_attacker_egress());
    assert_eq!(
        applied["result"]["applied"].as_bool(),
        Some(true),
        "an attested RegisterProvider must apply: {applied}\ndaemon log:\n{}",
        daemon.log()
    );
    assert_ne!(
        std::fs::read(&ws.config_path).ok().as_deref(),
        Some(before.as_slice()),
        "the attested RegisterProvider must actually change config.toml (so the \
         refused test's byte-identical assertion means something)"
    );
    let snapshot = client.call("config/get", json!({}));
    assert!(
        serde_json::to_string(&snapshot)
            .unwrap()
            .contains("attacker-egress"),
        "the attested provider must appear in the running config: {snapshot}"
    );
}

// ---------------------------------------------------------------------------
// SetPrivacyBoundary — refuse / accept pair
// ---------------------------------------------------------------------------

/// **AC-1 (SetPrivacyBoundary refused): writes nothing, swaps nothing.** The
/// variant that directly mutates the privacy promise; same inspect-don't-infer
/// evidence, non-vacuous via its accepting counterpart below.
#[test]
fn a_presence_refused_privacy_boundary_writes_nothing() {
    let ws = Workspace::new("configset-privacy-refused");
    ws.write_config(&base_config());
    let daemon = Daemon::spawn(&ws, probe_with_presence("fail"));
    let mut client = daemon.connect();
    let before = std::fs::read(&ws.config_path).expect("config exists");

    let refused = client.call("config/set", set_secret_boundary());
    assert_eq!(
        refused["error"]["code"].as_i64(),
        Some(ATTESTATION_FAILED),
        "config/set SetPrivacyBoundary must be refused at the presence gate: \
         {refused}\ndaemon log:\n{}",
        daemon.log()
    );
    assert_eq!(
        std::fs::read(&ws.config_path).ok().as_deref(),
        Some(before.as_slice()),
        "AC-1: a refused SetPrivacyBoundary must leave config.toml byte-identical"
    );
    let snapshot = client.call("config/get", json!({}));
    assert!(
        !serde_json::to_string(&snapshot)
            .unwrap()
            .contains("secrets/**"),
        "AC-1: the refused privacy boundary must not appear in the running config: {snapshot}"
    );
}

/// **AC-2 (SetPrivacyBoundary attested): applies through the full gate/RPC path.**
/// Accepting-path proof for the privacy-boundary variant + the non-vacuity anchor
/// for the refuse test above.
#[test]
fn an_attested_privacy_boundary_writes() {
    let ws = Workspace::new("configset-privacy-attested");
    ws.write_config(&base_config());
    let daemon = Daemon::spawn(&ws, probe_with_presence("1"));
    let mut client = daemon.connect();
    let before = std::fs::read(&ws.config_path).expect("config exists");

    let applied = client.call("config/set", set_secret_boundary());
    assert_eq!(
        applied["result"]["applied"].as_bool(),
        Some(true),
        "an attested SetPrivacyBoundary must apply: {applied}\ndaemon log:\n{}",
        daemon.log()
    );
    assert_ne!(
        std::fs::read(&ws.config_path).ok().as_deref(),
        Some(before.as_slice()),
        "the attested SetPrivacyBoundary must actually change config.toml"
    );
    let snapshot = client.call("config/get", json!({}));
    assert!(
        serde_json::to_string(&snapshot)
            .unwrap()
            .contains("secrets/**"),
        "the attested privacy boundary must appear in the running config: {snapshot}"
    );
}
