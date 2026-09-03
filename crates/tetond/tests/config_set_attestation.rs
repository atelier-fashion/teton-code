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
//!
//! ## REQ-589 AC-20 — and the posture the remedy really has
//!
//! REQ-589's over-budget offer writes a going-forward remedy through this same
//! method (ADR-4), so its two payload shapes join the pairs above, and AC-20
//! asks a further question this file is the right place for: does the offer's
//! wording claim an attestation the running build does not perform? The answer
//! turned out to be sharper than the AC anticipated — the remedy's durable write
//! did not pass the daemon-wide commitment gates at all (ADR-18 item 3). REQ-591
//! D-1 closed that, and the last section now pins **where** the gate went rather
//! than recording that there was none. See the section header below.

use std::sync::Arc;

use serde_json::{json, Value};

use teton_core::config::Config;
use teton_protocol::events::BudgetBound;
use teton_protocol::methods::{ConfigUpdate, ProviderConfig, SkillSource};
use teton_protocol::{ProviderId, ProviderKind};
use tetond::broadcast::EventBus;
use tetond::harness::budget::{
    self, BudgetInputs, OverBudgetOffer, PriorWindowRejection, SkillStage,
};
use tetond::harness::context::Fit;
use tetond::harness::turn_loop::HarnessConfig;
use tetond::runtime::DaemonRuntime;

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

// ---------------------------------------------------------------------------
// REQ-589 AC-20 — the attestation posture the over-budget remedy actually has
// ---------------------------------------------------------------------------
//
// BR-8 says the durable remedy "inherits `config/set`'s existing posture
// exactly", and AC-20 says the offer's wording must match what the **running
// build** performs — never claiming a verified human on a build that verifies
// nobody. Both are claims about a mechanism, so both are tested against the
// mechanism rather than against the prose.
//
// Three legs, and the third is the one that matters:
//
// 1. `config/set` over the wire, carrying the remedy's OWN two payload shapes,
//    refused under `TETON_PRESENCE_ACCEPT=fail` and applied under `=1`. The
//    field-wise `RegisterProvider` (REQ-586's window merge) and `SetTierBinding`
//    are the two writes BR-9's rebind performs, and neither had a pair here.
// 2. The offer's words, from the one composer (ADR-16), asserted to claim no
//    attestation — under both presence configurations, with what the build did
//    in each recorded beside the assertion.
// 3. **The remedy's durable write does not go through that method at all**, and
//    therefore does not pass either daemon-wide commitment gate. See
//    [`the_remedys_durable_write_does_not_pass_the_daemon_wide_commitment_gates`].

/// The reported machine, as a config the daemon will start on: a local provider
/// bound to every tier, and one remote registered with a model and **no**
/// declared window — the `LocalEngine` bound whose BR-7 remedy is the two-part
/// rebind, and the only shape in which both of the remedy's writes are legal.
fn rebind_config() -> String {
    let mut c = String::new();
    c.push_str("[[providers]]\nid = \"local\"\nkind = \"local\"\n\n");
    c.push_str(
        "[[providers]]\nid = \"kimi\"\nkind = \"openai-compatible\"\n\
         endpoint = \"http://127.0.0.1:9/v1/chat/completions\"\n\
         model = \"kimi-k3\"\n\
         auth_ref = \"env:TETON_REQ589_TEST_CREDENTIAL_ABSENT\"\n\n",
    );
    for tier in ["reflex", "scan", "build", "think"] {
        c.push_str(&tier_block(tier, "local"));
    }
    c.push_str("[privacy]\nredact = false\n\n");
    c
}

/// BR-9's **first** write on the wire: `kimi` re-registered field-wise with the
/// catalogued window declared (ADR-5's order, REQ-586 ADR-7's merge).
fn declare_kimis_window() -> Value {
    json!({ "update": {
        "op": "register_provider",
        "id": "kimi",
        "kind": "openai-compatible",
        "endpoint": "http://127.0.0.1:9/v1/chat/completions",
        "model": "kimi-k3",
        "auth_ref": "env:TETON_REQ589_TEST_CREDENTIAL_ABSENT",
        "max_context": 1_000_000,
    }})
}

/// BR-9's **second** write on the wire: the `build` tier moved to `kimi`. The
/// `config/set` variant that had no refuse/accept pair in this file at all.
fn bind_build_to_kimi() -> Value {
    json!({ "update": {
        "op": "set_tier_binding",
        "tier": "build",
        "provider_id": "kimi",
    }})
}

/// **AC-20 / LESSON-519 (the remedy's payloads, refused): writes nothing.**
///
/// Proven by the bytes on disk and the live `config/get`, for **both** of
/// BR-9's writes on one fixture. Non-vacuous because
/// [`an_attested_remedy_pair_applies_over_the_wire`] proves these exact
/// payloads persist when presence is satisfied.
#[test]
fn a_presence_refused_remedy_pair_writes_nothing() {
    let ws = Workspace::new("req589-remedy-refused");
    ws.write_config(&rebind_config());
    let daemon = Daemon::spawn(&ws, probe_with_presence("fail"));
    let mut client = daemon.connect();
    let before = std::fs::read(&ws.config_path).expect("config exists");

    for (what, payload) in [
        ("the window declaration", declare_kimis_window()),
        ("the tier binding", bind_build_to_kimi()),
    ] {
        let refused = client.call("config/set", payload);
        assert_eq!(
            refused["error"]["code"].as_i64(),
            Some(ATTESTATION_FAILED),
            "{what} must be refused at the presence gate: {refused}\ndaemon log:\n{}",
            daemon.log()
        );
        assert_eq!(
            std::fs::read(&ws.config_path).ok().as_deref(),
            Some(before.as_slice()),
            "a refused {what} must leave config.toml byte-identical"
        );
    }

    let snapshot = serde_json::to_string(&client.call("config/get", json!({}))).unwrap();
    assert!(
        !snapshot.contains("1000000"),
        "AC-20: the refused window must not appear in the running config: {snapshot}"
    );
    assert!(
        !snapshot.contains(r#""tier":"build","provider_id":"kimi""#),
        "AC-20: the refused binding must not appear in the running config: {snapshot}"
    );
}

/// **AC-20 (the remedy's payloads, attested): both halves apply, in ADR-5's
/// order.**
///
/// The accepting-path proof and the non-vacuity anchor for the refusal above:
/// the identical payloads change the bytes and reach the live config.
#[test]
fn an_attested_remedy_pair_applies_over_the_wire() {
    let ws = Workspace::new("req589-remedy-attested");
    ws.write_config(&rebind_config());
    let daemon = Daemon::spawn(&ws, probe_with_presence("1"));
    let mut client = daemon.connect();
    let before = std::fs::read(&ws.config_path).expect("config exists");

    // FIRST — the window (ADR-5). A failure here stops the rebind, which is
    // what makes the tier binding below unreachable with `max_context = 0`.
    let declared = client.call("config/set", declare_kimis_window());
    assert_eq!(
        declared["result"]["applied"].as_bool(),
        Some(true),
        "an attested window declaration must apply: {declared}\ndaemon log:\n{}",
        daemon.log()
    );
    // SECOND — the binding.
    let bound = client.call("config/set", bind_build_to_kimi());
    assert_eq!(
        bound["result"]["applied"].as_bool(),
        Some(true),
        "an attested tier binding must apply: {bound}\ndaemon log:\n{}",
        daemon.log()
    );

    let after = std::fs::read_to_string(&ws.config_path).expect("config exists");
    assert_ne!(
        after.as_bytes(),
        before.as_slice(),
        "the attested pair must actually change config.toml (so the refused test's \
         byte-identical assertion means something)"
    );
    assert!(
        after.contains("max_context = 1000000"),
        "the declared window must reach the document:\n{after}"
    );
    let snapshot = serde_json::to_string(&client.call("config/get", json!({}))).unwrap();
    assert!(
        snapshot.contains("1000000"),
        "the declared window must reach the running config: {snapshot}"
    );
}

/// The offer as a real route produces it, composed from the **one** home
/// (ADR-16): a `Window`-bound remote route whose declared window the expansion
/// exceeds, which is the arm that carries a `RaiseWindow` remedy and therefore
/// the arm with the most words to get wrong.
fn a_window_bound_offer() -> OverBudgetOffer {
    const WINDOW: u32 = 128_000;
    let inputs = BudgetInputs {
        window: WINDOW,
        cap: 0,
        // ADR-1's reservation: the `max_tokens` the adapters send.
        reservation: HarnessConfig::default().gen_params.max_tokens,
        is_local: false,
        redact_scan: false,
        provider_id: Some("kimi"),
    };
    let budget = budget::derive(inputs);
    assert_eq!(
        budget.bound,
        BudgetBound::Window,
        "the fixture must be the bound whose remedy is a window write"
    );
    let measured = Fit {
        tokens: budget.budget_tokens * 2,
        bytes: budget.budget_bytes * 2,
        fits: false,
    };
    OverBudgetOffer::new(
        "analyze",
        SkillStage::Body,
        measured,
        &budget,
        WINDOW,
        budget::proposed_window(Some("kimi-k3"), inputs, measured),
        None,
    )
}

/// **AC-20 — the offer claims no attestation, and this build performs none for
/// it.**
///
/// The offer's words have exactly one home (ADR-16: the daemon words it, the
/// client renders it verbatim), so guarding that home guards every surface they
/// reach. What is guarded is narrow and checkable: the question and all four
/// option labels may not claim a verified human, an attestation, or a presence
/// check — because on a shipped build there is none (ASSUME-C).
///
/// **REQ-591 D-1 changed what the build does and not what it says.** The remedy
/// now passes the commitment gate on a presence build, so a claim of
/// attestation would be *true there* — and still false on every shipped build,
/// where the mechanism degrades to allow. That is exactly why the rule is worded
/// as an absence: the only sentence that is honest under both postures is one
/// that claims nothing.
///
/// **Run under both presence configurations**, with what the build actually did
/// recorded beside the assertion. The wording is identical in both, which is
/// the point rather than an accident.
///
/// Non-vacuous by construction: the fixture is asserted to carry the remedy
/// pair, so the labels under test exist and name a concrete write. An offer
/// with no remedy would trivially claim no attestation.
#[test]
fn the_offer_claims_no_attestation_under_either_presence_configuration() {
    /// Vocabulary that would assert a human was verified. Matched
    /// case-insensitively against the question and every option label.
    const FORBIDDEN: &[&str] = &[
        "attest",
        "presence",
        "verified human",
        "verified you",
        "confirmed you",
        "touch id",
        "biometric",
        "authenticated",
    ];

    let offer = a_window_bound_offer();
    let question = offer.question(SkillSource::User, PriorWindowRejection::None);
    let labels = offer.option_labels();
    let remedy = labels
        .remedy
        .as_ref()
        .expect("the fixture must carry BR-7's remedy, or this test guards nothing");
    assert!(
        remedy
            .proceed_and_remedy
            .contains("capabilities.max_context")
            && remedy.remedy_only.contains("capabilities.max_context"),
        "the remedy labels must name the concrete write, or there is no claim here to \
         check: {remedy:?}"
    );

    let spoken = [
        question.as_str(),
        labels.proceed_once.as_str(),
        labels.decline.as_str(),
        remedy.proceed_and_remedy.as_str(),
        remedy.remedy_only.as_str(),
    ];

    for mode in ["1", "fail"] {
        let ws = Workspace::new("req589-attestation-wording");
        ws.write_config(&rebind_config());
        let daemon = Daemon::spawn(&ws, probe_with_presence(mode));
        let mut client = daemon.connect();

        // What this build actually performs for a `config/set`, established
        // rather than assumed — the fact the wording has to match.
        let performed = client.call("config/set", declare_kimis_window());
        let refused_here = performed["error"]["code"].as_i64() == Some(ATTESTATION_FAILED);
        assert_eq!(
            refused_here,
            mode == "fail",
            "the seam must actually change what the build does, or `both configurations` \
             is one configuration twice: {performed}\ndaemon log:\n{}",
            daemon.log()
        );

        for line in spoken {
            let lowered = line.to_lowercase();
            for claim in FORBIDDEN {
                assert!(
                    !lowered.contains(claim),
                    "AC-20: the offer says `{claim}` while this build \
                     {} — the wording must not claim an attestation the running build does \
                     not perform:\n{line}",
                    if refused_here {
                        "verifies a human for `config/set` but not for the remedy write"
                    } else {
                        "verifies nobody"
                    }
                );
            }
        }
    }
}

/// **Where the remedy's commitment gate sits, pinned so it cannot move
/// silently (REQ-591 D-1; was ADR-18 item 3).**
///
/// ADR-4 routes every remedy through `config/set` and said it inherits that
/// method's posture "verbatim". It inherited `DaemonRuntime::apply_config_update`
/// — validation, `reject_unusable_binding`, the atomic document-preserving
/// persist, the identical refusals — and **not** `refuse_daemon_wide`
/// (REQ-570 BR-10(a)) or `refuse_unattested_commitment` (BR-10(b)), which wrap
/// that body in `server.rs::handle_config_set`. This test recorded that gap as
/// a fact rather than a footnote; D-1 closed it, and the test now records
/// **where** it was closed, which is the thing a later edit can get wrong.
///
/// The gate went to the remedy's own door — `apply_over_budget_remedy`, through
/// the `CommitmentAttestation` seam the acknowledgment's durable row also
/// consults — and deliberately **not** into `apply_config_update`. Pushing it
/// down would put a second presence check underneath `config/set`, which has
/// already run one, and would make the seam a test drives directly stop being
/// the seam production uses.
///
/// **So the two legs pin the two ends of that decision.** (a) The wire still
/// refuses and still writes nothing — `config/set`'s own gate, untouched.
/// (b) The shared body is still reachable and still writes, because the body is
/// not where the check belongs.
///
/// The remedy's own paired witness — refused write versus attested write, on
/// the same fixture, driven through a real over-budget turn — is
/// `skill_over_budget_recovery::the_remedy_is_written_only_where_a_verified_human_stands_behind_it`.
/// This test is about placement; that one is about behaviour.
#[test]
fn the_remedys_gate_sits_at_its_own_door_and_not_in_the_shared_config_body() {
    assert!(
        std::env::var_os("TETON_CONFIG").is_none(),
        "the in-process half relies on `from_env` resolving `base_dir/config.toml`; the \
         spawned harness sets TETON_CONFIG on the child only"
    );

    // (a) The wire, under a present-but-refusing verifier: refused, nothing
    // written. The gate is real and this is it working.
    let refused_ws = Workspace::new("req589-gap-wire");
    refused_ws.write_config(&rebind_config());
    let daemon = Daemon::spawn(&refused_ws, probe_with_presence("fail"));
    let mut client = daemon.connect();
    let before = std::fs::read(&refused_ws.config_path).expect("config exists");
    let refused = client.call("config/set", declare_kimis_window());
    assert_eq!(
        refused["error"]["code"].as_i64(),
        Some(ATTESTATION_FAILED),
        "the RPC surface attests: {refused}\ndaemon log:\n{}",
        daemon.log()
    );
    assert_eq!(
        std::fs::read(&refused_ws.config_path).ok().as_deref(),
        Some(before.as_slice()),
        "and writes nothing when it refuses"
    );

    // (b) The shared body, same payload, same fixture shape: applied. This is
    // `config/set`'s own persist path, reached without `config/set`'s handler —
    // and it must stay reachable, because the remedy's gate is one level up. A
    // failure here means the check was pushed into the body, which double-gates
    // the RPC above and moves the seam out from under every test that drives it.
    // Read back off the file and re-parsed, not inferred from the return
    // (LESSON-519).
    let seam_ws = Workspace::new("req589-gap-seam");
    seam_ws.write_config(&rebind_config());
    let events = Arc::new(EventBus::new());
    let runtime = DaemonRuntime::from_env(&seam_ws.root, &events).expect("the daemon starts");
    runtime
        .apply_config_update(ConfigUpdate::RegisterProvider(ProviderConfig {
            id: ProviderId::from("kimi"),
            kind: ProviderKind::OpenaiCompatible,
            endpoint: Some("http://127.0.0.1:9/v1/chat/completions".to_owned()),
            model: Some("kimi-k3".to_owned()),
            auth_ref: Some("env:TETON_REQ589_TEST_CREDENTIAL_ABSENT".to_owned()),
            max_context: Some(1_000_000),
            context_budget_cap: None,
            allow_cleartext: None,
            floored_budget: None,
        }))
        .expect(
            "the shared config body has no presence gate of its own, by design (REQ-591 \
             D-1). If this starts erroring, the check moved down into it — re-read the \
             doc above before changing this expectation",
        );

    let document = std::fs::read_to_string(&seam_ws.config_path).expect("config exists");
    assert!(
        document.contains("max_context = 1000000"),
        "the shared body wrote the window the gated RPC refused — which is the \
         placement claim, not a hole:\n{document}"
    );
    assert_eq!(
        Config::load(&document)
            .expect("and the document still parses")
            .providers
            .iter()
            .find(|p| p.id == "kimi")
            .expect("the provider survives a field-wise write")
            .capabilities
            .max_context,
        1_000_000,
        "…and the production loader agrees with the bytes"
    );
}

// ---------------------------------------------------------------------------
// REQ-611 BR-16 / AC-6 — SetTranscriptEnabled, the durable transcript default
// ---------------------------------------------------------------------------

/// The durable transcript default, as `config/set` carries it (REQ-611 ADR-5).
///
/// A payload that **would** persist if the gate were bypassed (LESSON-520):
/// `base_config` names no `[transcript]` table at all, and `enabled = true` is
/// not the shipped default, so an applied write necessarily adds bytes and a
/// refused one necessarily leaves the file alone. A payload that merely
/// restated the default would leave the file identical either way and the
/// refusal leg would prove nothing.
fn enable_transcripts() -> Value {
    json!({ "update": { "op": "set_transcript_enabled", "enabled": true } })
}

/// **REQ-611 BR-16 / AC-6: the durable transcript switch writes on an attested
/// seam and writes nothing on a refused one.**
///
/// Both legs in one test, on the same fixture and the same payload, because
/// each is what makes the other mean something: the accepting leg proves the
/// payload is persistable, which is what stops the refusing leg's
/// byte-identical assertion from being satisfied by a write that could never
/// have happened (LESSON-520, and the shape the two pairs above already take).
///
/// The evidence is the **bytes and a re-parse**, never the error code
/// (LESSON-519): `config.toml` is read back and handed to the production
/// `Config::load`, so what is asserted is that the daemon's own loader agrees a
/// durable transcript default is now in force — not that a string appeared in a
/// file.
///
/// ADR-5's inheritance claim is what is really under test. `SetTranscriptEnabled`
/// adds no gate of its own and gets no exemption; it is refused here **because**
/// `config/set` refuses, which is the whole argument for putting the durable
/// switch on this method rather than a new one.
///
/// **Mutation (run, red):** delete the `refuse_unattested_commitment` call from
/// `handle_config_set` — the one gate line, per LESSON-520's instruction for
/// AC-6. The refusing leg then applies the update, and this test fails on the
/// `ATTESTATION_FAILED` assertion and again on the byte-identical one, naming
/// the `[transcript]` table that appeared. The accepting leg stays green, which
/// is the right split: it is the payload that is being shown persistable, and a
/// deleted gate does not stop a legitimate write. Restored.
/// **Mutation (LESSON-520, run 2026-09-03):** deleting the
/// `refuse_unattested_commitment` line from `handle_config_set` turned this
/// test red at the refuse leg — `ADR-5: the transcript default inherits
/// config/set's presence gate with no exemption` with `left: None,
/// right: Some(-32015)` and `"applied":true` in the response — so the
/// assertion is load-bearing and the payload would persist if the gate were
/// bypassed. Restored; green again.
#[test]
fn set_transcript_enabled_writes_on_accept_and_nothing_on_refuse() {
    // --- refused: nothing on disk, nothing in the running config ---
    let refused_ws = Workspace::new("configset-transcript-refused");
    refused_ws.write_config(&base_config());
    let refused_daemon = Daemon::spawn(&refused_ws, probe_with_presence("fail"));
    let mut refused_client = refused_daemon.connect();
    let before = std::fs::read(&refused_ws.config_path).expect("config exists");
    assert!(
        !String::from_utf8_lossy(&before).contains("transcript"),
        "the fixture must start with no [transcript] table, or 'nothing was \
         written' is unfalsifiable"
    );

    let refused = refused_client.call("config/set", enable_transcripts());
    assert_eq!(
        refused["error"]["code"].as_i64(),
        Some(ATTESTATION_FAILED),
        "ADR-5: the transcript default inherits config/set's presence gate with no \
         exemption: {refused}\ndaemon log:\n{}",
        refused_daemon.log()
    );
    assert_eq!(
        std::fs::read(&refused_ws.config_path).ok().as_deref(),
        Some(before.as_slice()),
        "BR-16: a refused SetTranscriptEnabled must leave config.toml byte-identical"
    );
    assert!(
        !Config::load(&String::from_utf8_lossy(
            &std::fs::read(&refused_ws.config_path).expect("config exists")
        ))
        .expect("the untouched document still loads")
        .transcript
        .enabled,
        "BR-16: and the production loader agrees nothing was recorded"
    );
    let refused_snapshot = refused_client.call("config/get", json!({}));
    assert_eq!(
        refused_snapshot["result"]["snapshot"]["transcript"]["enabled"].as_bool(),
        Some(false),
        "the refused default must not appear in the running config: {refused_snapshot}"
    );
    drop(refused_client);
    drop(refused_daemon);

    // --- attested: the same payload lands, and the loader reads it back ---
    let ws = Workspace::new("configset-transcript-attested");
    ws.write_config(&base_config());
    let daemon = Daemon::spawn(&ws, probe_with_presence("1"));
    let mut client = daemon.connect();
    let before = std::fs::read(&ws.config_path).expect("config exists");

    let applied = client.call("config/set", enable_transcripts());
    assert_eq!(
        applied["result"]["applied"].as_bool(),
        Some(true),
        "an attested SetTranscriptEnabled must apply: {applied}\ndaemon log:\n{}",
        daemon.log()
    );
    assert_ne!(
        std::fs::read(&ws.config_path).ok().as_deref(),
        Some(before.as_slice()),
        "the attested write must actually change config.toml (so the refused leg's \
         byte-identical assertion means something)"
    );
    let document = std::fs::read_to_string(&ws.config_path).expect("config exists");
    let reloaded = Config::load(&document).expect("the written document must load");
    assert!(
        reloaded.transcript.enabled,
        "AC-6: read back and RE-PARSED, not matched as a string:\n{document}"
    );
    let snapshot = client.call("config/get", json!({}));
    assert_eq!(
        snapshot["result"]["snapshot"]["transcript"]["enabled"].as_bool(),
        Some(true),
        "and the live config swapped too: {snapshot}"
    );
}
