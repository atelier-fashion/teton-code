//! REQ-576 AC-1 over the real socket: a presence-refused `config/set` writes
//! nothing and swaps nothing, proven by the bytes on disk and the live config —
//! not inferred from the error code (LESSON-519).
//!
//! `config/set` became the fourth REQ-570 BR-10(b) daemon-wide commitment
//! (TASK-140). Its refusal/degradation are covered in-process by the shared
//! `only_a_daemon_wide_commitment_demands_presence` harness; what that harness
//! cannot supply is a **real config file** to inspect after a refusal, because
//! its fixture is config-less. This suite spawns a real `teton-code` with a
//! config on disk and a **present-but-refusing** verifier
//! (`TETON_PRESENCE_ACCEPT=fail`, the REQ-575 seam → `AlwaysFailsVerifier`), then
//! reads the evidence back from the world for the two egress/privacy-critical
//! variants: `RegisterProvider` (an egress endpoint) and `SetPrivacyBoundary`
//! (the privacy boundary itself).

use serde_json::json;

#[path = "e2e/harness.rs"]
mod harness;

use harness::{tier_block, Daemon, DaemonOptions, Workspace};

const GIB: u64 = 1024 * 1024 * 1024;

/// A deterministic machine so a probe cannot pick a model and spend the budget.
/// The `fail` seam installs `AlwaysFailsVerifier` (present, always refuses) — the
/// only way a spawned binary reaches a present-but-refusing verifier. It rides
/// the `TETON_TEST_SEAMS` master switch, so a release build refuses it.
fn probe_refusing() -> DaemonOptions {
    DaemonOptions::default()
        .env("TETON_PROBE_RAM_BYTES", (16 * GIB).to_string())
        .env("TETON_PROBE_DISK_BYTES", "500000000000")
        .env("TETON_PROBE_GPU", "apple-silicon")
        .env("TETON_TEST_SEAMS", "1")
        .env("TETON_PRESENCE_ACCEPT", "fail")
}

/// A minimal valid config with one local provider bound to every tier — enough to
/// start the daemon, and deliberately WITHOUT the provider/boundary the tests try
/// to add, so a successful `config/set` would visibly change the file.
fn base_config() -> String {
    let mut c = String::new();
    c.push_str("[[providers]]\nid = \"local\"\nkind = \"local\"\n\n");
    for tier in ["reflex", "scan", "build", "think"] {
        c.push_str(&tier_block(tier, "local"));
    }
    c.push_str("[privacy]\nredact = false\n\n");
    c
}

const ATTESTATION_FAILED: i64 = teton_protocol::jsonrpc::error_code::ATTESTATION_FAILED;

/// **AC-1 (RegisterProvider): a presence-refused provider registration writes
/// nothing.** The refusal is proven by the on-disk bytes (before == after) and
/// the live config snapshot (the provider is absent), not by the error code.
#[test]
fn a_presence_refused_register_provider_writes_nothing() {
    let ws = Workspace::new("configset-register");
    ws.write_config(&base_config());
    let daemon = Daemon::spawn(&ws, probe_refusing());
    let mut client = daemon.connect();

    let before = std::fs::read(&ws.config_path).expect("config exists");

    let refused = client.call(
        "config/set",
        json!({ "update": {
            "op": "register_provider",
            "id": "attacker-egress",
            "kind": "openai",
            "endpoint": "http://127.0.0.1:9/x",
            "model": "whatever",
        }}),
    );
    assert_eq!(
        refused["error"]["code"].as_i64(),
        Some(ATTESTATION_FAILED),
        "config/set RegisterProvider must be refused at the BR-10(b) presence gate: \
         {refused}\ndaemon log:\n{}",
        daemon.log()
    );

    // (1) Inspected on disk, not inferred: byte-identical.
    assert_eq!(
        std::fs::read(&ws.config_path).ok().as_deref(),
        Some(before.as_slice()),
        "AC-1: a refused RegisterProvider must leave config.toml byte-identical"
    );

    // (2) Inspected in the live daemon: the provider was not swapped in.
    let snapshot = client.call("config/get", json!({}));
    assert!(
        !serde_json::to_string(&snapshot)
            .unwrap()
            .contains("attacker-egress"),
        "AC-1: the refused provider must not appear in the running config: {snapshot}"
    );
}

/// **AC-1 (SetPrivacyBoundary): a presence-refused privacy-boundary change writes
/// nothing.** The variant the spec singles out as directly mutating the privacy
/// promise; same inspect-don't-infer evidence.
#[test]
fn a_presence_refused_privacy_boundary_writes_nothing() {
    let ws = Workspace::new("configset-privacy");
    ws.write_config(&base_config());
    let daemon = Daemon::spawn(&ws, probe_refusing());
    let mut client = daemon.connect();

    let before = std::fs::read(&ws.config_path).expect("config exists");

    let refused = client.call(
        "config/set",
        json!({ "update": {
            "op": "set_privacy_boundary",
            "path_glob": "secrets/**",
            "mode": "local_only",
        }}),
    );
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
