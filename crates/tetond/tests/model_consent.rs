//! The first-run consent gate's behavioural suite (REQ-547 TASK-004).
//!
//! The claim this file exists to make is AC-1: **zero download requests are
//! issued before a decision**. It is asserted, not argued — a recording
//! [`RangeFetcher`] double is installed behind the *production*
//! [`FetcherInstaller`], which is the only route the gate has to the network, and
//! the assertion is made at the moment the proposal is observed and again after
//! the answer (so a double that is simply never wired in cannot pass).
//!
//! The rest of the file covers the decision's other load-bearing properties:
//!
//! - a session keeps working remote-only while a proposal is outstanding (BR-1 /
//!   D-3 — the gate withholds the *tier*, never the session),
//! - a decline persists and a later daemon start does not re-prompt (AC-4/BR-10),
//! - `auto_accept` completes with no proposal at all (AC-5/BR-5),
//! - an offline accept produces a clear network error, installs nothing, and is
//!   **not** recorded as declined — a later run re-prompts (AC-10/BR-12),
//! - an override installs the chosen entry, and an above-RAM-floor pick needs a
//!   second confirmation (AC-3/BR-3),
//! - the proposal payload carries no path, URL, digest, or credential (BR-11),
//! - the `model/confirm` round-trip runs over a real socket without deadlocking
//!   the reader loop (the `permission/respond` ordering, reused).

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use serde_json::{json, Value};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::unix::{OwnedReadHalf, OwnedWriteHalf};
use tokio::net::UnixStream;
use tokio::time::timeout;

use teton_core::config::LocalModelConfig;
use teton_core::entities::{ModelSelection, SelectionSource};
use teton_core::policy::ProviderHealth;
use teton_core::ToolCallTier;

use teton_inference::catalog::{Catalog, ModelEntry, TierBand};
use teton_inference::download::{DownloadError, RangeFetcher};
use teton_inference::probe::{GpuClass, HardwareProfile, GIB};

use teton_protocol::events::{Event, ModelLifecycleStage};
use teton_protocol::methods::{InstallStatus, ModelConfirmOutcome, ModelConfirmParams};
use teton_protocol::RequestId;

use teton_providers::CapabilityProfile;

use tetond::broadcast::{EventBus, Subscription};
use tetond::install::{FixedFreeSpace, FreeSpace, WeightsInstall};
use tetond::model_consent::{
    ConsentOutcome, InstallError, ModelConsentGate, PendingModelDecisions,
};
use tetond::router::Router;
use tetond::runtime::DaemonRuntime;
use tetond::selection_store::SelectionStore;
use tetond::{server, Daemon};

// ---------------------------------------------------------------------------
// Fixtures
// ---------------------------------------------------------------------------

/// A commit-SHA-shaped revision; the catalog pins immutable revisions (BR-15).
const REVISION: &str = "0123456789abcdef0123456789abcdef01234567";

/// The three artifacts the double serves, with their real SHA-256 digests, so a
/// successful install goes through the library's genuine verification path
/// rather than a stubbed one.
const SMALL_BODY: &[u8] = b"small-model-weights";
const SMALL_SHA: &str = "407111f5472012789dd06a10f915361ab7e0ecd540add9570bc314f4916308c8";
const ALT_BODY: &[u8] = b"alt-model-weights";
const ALT_SHA: &str = "e052079f4ba15abec75ba85c5e1df3218d3bfffe5c6f6cc2e726685f743bd590";
const BIG_BODY: &[u8] = b"big-model-weights";
const BIG_SHA: &str = "ee5901ecfa6c93f94013dc1e4fc5a98d549100f2803ea5048c93b12923078307";

fn entry(
    name: &str,
    body: &[u8],
    sha256: &str,
    ram_floor_bytes: u64,
    band: TierBand,
) -> ModelEntry {
    ModelEntry {
        name: name.to_owned(),
        url: format!("https://models.test.invalid/Org/{name}/resolve/{REVISION}/{name}.gguf"),
        revision: REVISION.to_owned(),
        sha256: sha256.to_owned(),
        size_bytes: body.len() as u64,
        ram_floor_bytes,
        band,
    }
}

/// A three-entry catalog: the probe's pick, a same-band alternative, and one
/// entry above this machine's RAM floor (the BR-3 double-confirmation case).
fn test_catalog() -> Catalog {
    Catalog {
        version: 1,
        models: vec![
            entry("small-fit", SMALL_BODY, SMALL_SHA, 8 * GIB, TierBand::Small),
            entry("alt-fit", ALT_BODY, ALT_SHA, 8 * GIB, TierBand::Small),
            entry("oversized", BIG_BODY, BIG_SHA, 64 * GIB, TierBand::Large),
        ],
    }
}

/// A 16 GiB Apple Silicon machine: the small band, with room for every artifact.
fn machine() -> HardwareProfile {
    HardwareProfile {
        ram_bytes: 16 * GIB,
        free_disk_bytes: 400 * GIB,
        gpu: GpuClass::AppleSilicon,
    }
}

// ---------------------------------------------------------------------------
// The recording fetcher — the instrument AC-1 is measured with
// ---------------------------------------------------------------------------

/// A [`RangeFetcher`] that records **every** call it receives.
///
/// This is the whole apparatus behind AC-1. It sits where the real HTTP client
/// sits — inside the production [`FetcherInstaller`] — so "zero calls" means
/// "the daemon issued no download request", not "the test did not look".
struct RecordingFetcher {
    calls: Mutex<Vec<String>>,
    bodies: HashMap<String, Vec<u8>>,
    offline: bool,
}

impl RecordingFetcher {
    /// A fetcher that serves the test catalog's artifacts.
    fn serving() -> Self {
        let mut bodies = HashMap::new();
        bodies.insert("small-fit".to_owned(), SMALL_BODY.to_vec());
        bodies.insert("alt-fit".to_owned(), ALT_BODY.to_vec());
        bodies.insert("oversized".to_owned(), BIG_BODY.to_vec());
        Self {
            calls: Mutex::new(Vec::new()),
            bodies,
            offline: false,
        }
    }

    /// A fetcher with no network: every request fails as a transport error, the
    /// same shape the real client reports when the host cannot be reached.
    fn offline() -> Self {
        Self {
            calls: Mutex::new(Vec::new()),
            bodies: HashMap::new(),
            offline: true,
        }
    }

    fn call_count(&self) -> usize {
        self.calls.lock().unwrap().len()
    }

    fn calls(&self) -> Vec<String> {
        self.calls.lock().unwrap().clone()
    }
}

impl RangeFetcher for RecordingFetcher {
    fn fetch(
        &self,
        url: &str,
        offset: u64,
        sink: &mut dyn FnMut(&[u8]) -> Result<(), DownloadError>,
    ) -> Result<u64, DownloadError> {
        self.calls.lock().unwrap().push(url.to_owned());
        if self.offline {
            return Err(DownloadError::Transport(
                "could not reach the model host".to_owned(),
            ));
        }
        let body = self
            .bodies
            .iter()
            .find(|(name, _)| url.contains(name.as_str()))
            .map(|(_, body)| body.clone())
            .ok_or_else(|| DownloadError::Io(std::io::Error::other("no such artifact")))?;
        let start = usize::try_from(offset)
            .unwrap_or(usize::MAX)
            .min(body.len());
        if start < body.len() {
            sink(&body[start..])?;
        }
        Ok(body.len() as u64)
    }
}

// ---------------------------------------------------------------------------
// Harness
// ---------------------------------------------------------------------------

/// One consent gate wired exactly as the daemon wires it, with the fetcher and
/// the state directory held aside so a test can assert on them.
struct Harness {
    dir: PathBuf,
    bus: Arc<EventBus>,
    pending: Arc<PendingModelDecisions>,
    store: Arc<SelectionStore>,
    fetcher: Arc<RecordingFetcher>,
    weights_dir: PathBuf,
    gate: Arc<ModelConsentGate>,
}

impl Harness {
    fn new(tag: &str, fetcher: RecordingFetcher, config: LocalModelConfig) -> Self {
        let dir = temp_dir(tag);
        Self::in_dir(dir, fetcher, config)
    }

    /// A harness on a machine other than the default 16 GiB one.
    fn on(tag: &str, profile: HardwareProfile, fetcher: RecordingFetcher) -> Self {
        Self::build(
            temp_dir(tag),
            profile,
            fetcher,
            LocalModelConfig::default(),
            true,
        )
    }

    /// A harness over an existing state directory — how "a later daemon start"
    /// is modelled: same directory, brand-new gate, store, and event bus.
    fn in_dir(dir: PathBuf, fetcher: RecordingFetcher, config: LocalModelConfig) -> Self {
        Self::build(dir, machine(), fetcher, config, true)
    }

    /// A harness modelling the production **no-engine** build: the install can
    /// succeed but nothing can load the weights (M-1).
    fn no_engine(tag: &str, fetcher: RecordingFetcher) -> Self {
        Self::build(
            temp_dir(tag),
            machine(),
            fetcher,
            LocalModelConfig::default(),
            false,
        )
    }

    fn build(
        dir: PathBuf,
        profile: HardwareProfile,
        fetcher: RecordingFetcher,
        config: LocalModelConfig,
        engine_present: bool,
    ) -> Self {
        let bus = Arc::new(EventBus::new());
        let pending = Arc::new(PendingModelDecisions::new());
        let store = Arc::new(SelectionStore::open(&dir));
        let fetcher = Arc::new(fetcher);
        let weights_dir = dir.join("models");
        // A fixed free-space answer rather than the host's: these tests are about
        // the *decision*, and a preflight that consulted the real volume would
        // make them fail on a full disk for reasons unrelated to consent. The
        // preflight's own behaviour is asserted in `install_pipeline.rs`.
        let installer = Arc::new(
            WeightsInstall::new(
                Arc::clone(&fetcher) as Arc<dyn RangeFetcher + Send + Sync>,
                weights_dir.clone(),
                None,
            )
            .with_free_space(Arc::new(FixedFreeSpace(Some(u64::MAX))) as Arc<dyn FreeSpace>),
        );
        let gate = Arc::new(
            ModelConsentGate::new(
                profile,
                test_catalog(),
                config,
                Arc::clone(&bus),
                Arc::clone(&pending),
                Arc::clone(&store),
                installer,
            )
            // Most tests here are about the *decision*, and they assert the tier
            // reaches `ready` — which is only honest on a machine that can load
            // the weights. So they model one: engine present (M-1). The no-engine
            // `disabled`-on-success path has its own coverage
            // (`a_successful_install_on_a_no_engine_build_is_disabled_not_ready`).
            .with_local_engine(engine_present),
        );
        Self {
            dir,
            bus,
            pending,
            store,
            fetcher,
            weights_dir,
            gate,
        }
    }

    fn subscribe(&self) -> Subscription {
        self.bus.subscribe(32)
    }

    fn installed(&self, name: &str) -> PathBuf {
        self.weights_dir.join(format!("{name}.gguf"))
    }

    fn cleanup(&self) {
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

fn temp_dir(tag: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!(
        "teton-consent-{tag}-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

/// Wait for the next `model_selection_proposed` on `sub`, or fail the test.
async fn next_proposal(sub: &mut Subscription) -> teton_protocol::events::ModelSelectionProposed {
    loop {
        let envelope = timeout(Duration::from_secs(5), sub.recv())
            .await
            .expect("timed out waiting for a proposal")
            .expect("event bus closed");
        if let Event::ModelSelectionProposed(proposal) = envelope.event {
            return proposal;
        }
    }
}

/// Everything published on `sub`, stopping once the stream goes quiet.
///
/// A short quiet window rather than a fixed count: the assertions these feed are
/// about an event that must be **absent**, so the drain has to be willing to wait
/// long enough for a stray proposal to show up if one were going to.
async fn drain(sub: &mut Subscription) -> Vec<Event> {
    let mut events = Vec::new();
    while let Ok(Some(envelope)) = timeout(Duration::from_millis(100), sub.recv()).await {
        events.push(envelope.event);
    }
    events
}

fn native() -> CapabilityProfile {
    CapabilityProfile {
        tool_call_tier: ToolCallTier::Native,
        parallel_calls: true,
        max_context: 200_000,
    }
}

// ---------------------------------------------------------------------------
// AC-1 — the central guarantee
// ---------------------------------------------------------------------------

#[tokio::test]
async fn zero_download_requests_are_issued_before_a_decision() {
    let h = Harness::new(
        "ac1",
        RecordingFetcher::serving(),
        LocalModelConfig::default(),
    );
    let mut sub = h.subscribe();

    // Nothing has run yet.
    assert_eq!(h.fetcher.call_count(), 0);

    let resolve = h.gate.resolve();
    let answer = async {
        let proposal = next_proposal(&mut sub).await;

        // THE assertion: the daemon has told the user what it wants to install
        // and how big it is, and has issued **no** download request (AC-1/BR-1).
        assert_eq!(
            h.fetcher.call_count(),
            0,
            "a download request was issued before the user answered: {:?}",
            h.fetcher.calls()
        );
        assert_eq!(
            proposal.proposed.as_ref().unwrap().entry.name,
            "small-fit",
            "the probe's pick should be proposed"
        );
        assert!(!h.installed("small-fit").exists());

        // Wait a beat and re-check: the gate is awaiting an answer, not racing
        // ahead of one.
        tokio::time::sleep(Duration::from_millis(20)).await;
        assert_eq!(h.fetcher.call_count(), 0);

        assert!(h
            .pending
            .resolve(&proposal.request_id, ModelConfirmOutcome::Accept));
    };

    let (outcome, ()) = tokio::join!(resolve, answer);

    // The anti-fake half: the double IS the production download path, so an
    // accepted proposal must drive it. A fetcher that is never called would
    // otherwise satisfy "zero calls" trivially.
    assert!(
        h.fetcher.call_count() >= 1,
        "the accepted install never reached the fetcher — the zero-call assertion above would be vacuous"
    );
    assert!(
        matches!(outcome, ConsentOutcome::Ready { .. }),
        "{outcome:?}"
    );
    assert!(h.installed("small-fit").exists());
    h.cleanup();
}

#[tokio::test]
async fn a_declined_proposal_never_fetches_a_single_byte() {
    let h = Harness::new(
        "decline-nofetch",
        RecordingFetcher::serving(),
        LocalModelConfig::default(),
    );
    let mut sub = h.subscribe();

    let resolve = h.gate.resolve();
    let answer = async {
        let proposal = next_proposal(&mut sub).await;
        assert_eq!(h.fetcher.call_count(), 0);
        h.pending
            .resolve(&proposal.request_id, ModelConfirmOutcome::Decline);
    };
    let (outcome, ()) = tokio::join!(resolve, answer);

    assert_eq!(outcome, ConsentOutcome::Declined);
    assert_eq!(
        h.fetcher.call_count(),
        0,
        "declining must not fetch anything, ever: {:?}",
        h.fetcher.calls()
    );
    assert!(!h.weights_dir.exists());
    h.cleanup();
}

// ---------------------------------------------------------------------------
// BR-1 / D-3 — the gate withholds the tier, not the session
// ---------------------------------------------------------------------------

#[tokio::test]
async fn an_undecided_tier_is_withheld_while_the_session_still_routes_remote_only() {
    let h = Harness::new(
        "d3",
        RecordingFetcher::serving(),
        LocalModelConfig::default(),
    );
    let runtime = Arc::new(DaemonRuntime::with_consent(Arc::clone(&h.gate)));

    // Undecided: the local tier is unavailable even though it is capable.
    assert!(
        !runtime.local_tier_available(),
        "an undecided tier must not serve turns"
    );

    // ...and a turn resolved against that state still finds a provider: the
    // session runs remote-only rather than blocking on the answer (BR-1).
    let router = Router::new(Vec::new(), "remote", "local")
        .with_provider("remote", "remote-model", native(), ProviderHealth::Healthy)
        .with_local_available(runtime.local_tier_available());
    let route = router.resolve_freeform("write a function that parses this config file");
    assert!(
        route.selected(),
        "an undecided local tier must not strand the session: {}",
        route.reason
    );
    assert_eq!(route.provider_id.as_ref().unwrap().0, "remote");

    // An answer that installs opens the gate for every later turn.
    let mut sub = h.subscribe();
    let drive = {
        let runtime = Arc::clone(&runtime);
        async move { runtime.run_model_consent().await }
    };
    let answer = async {
        let proposal = next_proposal(&mut sub).await;
        h.pending
            .resolve(&proposal.request_id, ModelConfirmOutcome::Accept);
    };
    let (outcome, ()) = tokio::join!(drive, answer);
    assert!(outcome.local_tier_ready());
    assert!(
        runtime.local_tier_available(),
        "a decided-and-installed tier must be usable"
    );
    h.cleanup();
}

#[tokio::test]
async fn a_declined_tier_stays_withheld() {
    let h = Harness::new(
        "d3-declined",
        RecordingFetcher::serving(),
        LocalModelConfig::default(),
    );
    let runtime = Arc::new(DaemonRuntime::with_consent(Arc::clone(&h.gate)));
    let mut sub = h.subscribe();

    let drive = {
        let runtime = Arc::clone(&runtime);
        async move { runtime.run_model_consent().await }
    };
    let answer = async {
        let proposal = next_proposal(&mut sub).await;
        h.pending
            .resolve(&proposal.request_id, ModelConfirmOutcome::Decline);
    };
    let (outcome, ()) = tokio::join!(drive, answer);

    assert_eq!(outcome, ConsentOutcome::Declined);
    assert!(!runtime.local_tier_available());
    h.cleanup();
}

// ---------------------------------------------------------------------------
// AC-4 / BR-10 — a decision is not re-litigated
// ---------------------------------------------------------------------------

#[tokio::test]
async fn declining_persists_and_a_later_daemon_start_does_not_re_prompt() {
    let first = Harness::new(
        "ac4",
        RecordingFetcher::serving(),
        LocalModelConfig::default(),
    );
    let mut sub = first.subscribe();

    let resolve = first.gate.resolve();
    let answer = async {
        let proposal = next_proposal(&mut sub).await;
        first
            .pending
            .resolve(&proposal.request_id, ModelConfirmOutcome::Decline);
    };
    let (outcome, ()) = tokio::join!(resolve, answer);
    assert_eq!(outcome, ConsentOutcome::Declined);

    // The decision is on disk, and it is a decline rather than an absence.
    let recorded = SelectionStore::open(&first.dir).current().unwrap();
    assert!(recorded.declined_local);
    assert_eq!(recorded.model_name, None);

    // A *later daemon start*: same state directory, everything else new.
    let second = Harness::in_dir(
        first.dir.clone(),
        RecordingFetcher::serving(),
        LocalModelConfig::default(),
    );
    let mut sub2 = second.subscribe();
    let outcome = second.gate.resolve().await;

    assert_eq!(outcome, ConsentOutcome::Declined);
    assert_eq!(second.pending.pending_count(), 0, "a decline re-prompted");
    let events = drain(&mut sub2).await;
    assert!(
        !events
            .iter()
            .any(|e| matches!(e, Event::ModelSelectionProposed(_))),
        "AC-4: a recorded decline must never produce another proposal: {events:?}"
    );
    assert_eq!(second.fetcher.call_count(), 0);
    second.cleanup();
}

#[tokio::test]
async fn an_installed_decision_is_not_re_litigated_but_missing_weights_re_prompt() {
    // A decision is recorded and the weights are present: settled (BR-10).
    let h = Harness::new(
        "br10",
        RecordingFetcher::serving(),
        LocalModelConfig::default(),
    );
    h.store
        .record(&ModelSelection::accepted(
            "small-fit",
            SelectionSource::Probe,
            1,
        ))
        .unwrap();
    std::fs::create_dir_all(&h.weights_dir).unwrap();
    std::fs::write(h.installed("small-fit"), SMALL_BODY).unwrap();

    let mut sub = h.subscribe();
    let outcome = h.gate.resolve().await;
    assert!(
        matches!(outcome, ConsentOutcome::Ready { .. }),
        "{outcome:?}"
    );
    assert!(!drain(&mut sub)
        .await
        .iter()
        .any(|e| matches!(e, Event::ModelSelectionProposed(_))));
    assert_eq!(
        h.fetcher.call_count(),
        0,
        "a settled decision re-downloaded"
    );

    // Remove the weights: BR-10's one sanctioned re-prompt.
    std::fs::remove_file(h.installed("small-fit")).unwrap();
    let reopened = Harness::in_dir(
        h.dir.clone(),
        RecordingFetcher::serving(),
        LocalModelConfig::default(),
    );
    let mut sub2 = reopened.subscribe();
    let resolve = reopened.gate.resolve();
    let answer = async {
        let proposal = next_proposal(&mut sub2).await;
        reopened
            .pending
            .resolve(&proposal.request_id, ModelConfirmOutcome::Accept);
    };
    let (outcome, ()) = tokio::join!(resolve, answer);
    assert!(
        matches!(outcome, ConsentOutcome::Ready { .. }),
        "{outcome:?}"
    );
    reopened.cleanup();
}

// ---------------------------------------------------------------------------
// AC-5 / BR-5 — the unattended path
// ---------------------------------------------------------------------------

#[tokio::test]
async fn auto_accept_completes_the_flow_with_no_proposal_emitted() {
    let h = Harness::new(
        "ac5",
        RecordingFetcher::serving(),
        LocalModelConfig {
            auto_accept: true,
            ..LocalModelConfig::default()
        },
    );
    let mut sub = h.subscribe();

    // No concurrent answerer: this must complete on its own or hang the test.
    let outcome = timeout(Duration::from_secs(5), h.gate.resolve())
        .await
        .expect("auto-accept must not wait for an answer");

    assert!(
        matches!(outcome, ConsentOutcome::Ready { .. }),
        "{outcome:?}"
    );
    assert!(h.installed("small-fit").exists());

    let events = drain(&mut sub).await;
    assert!(
        !events
            .iter()
            .any(|e| matches!(e, Event::ModelSelectionProposed(_))),
        "AC-5: auto-accept must emit no proposal: {events:?}"
    );
    // It still announces the decision, so an attached client learns why the tier
    // is in the state it is in.
    let decided = events
        .iter()
        .find_map(|e| match e {
            Event::ModelSelectionDecided(d) => Some(d.clone()),
            _ => None,
        })
        .expect("auto-accept must announce its decision");
    assert_eq!(decided.model_name.as_deref(), Some("small-fit"));
    assert!(!decided.declined_local);
    assert_eq!(
        decided.source,
        teton_protocol::events::SelectionSource::AutoAccept
    );
    assert_eq!(decided.request_id, None, "no prompt was shown");
    h.cleanup();
}

#[tokio::test]
async fn a_config_pin_proposes_the_pinned_entry_and_downloads_nothing_until_answered() {
    // C-1 (REQ-547 review): a `[local_model] pinned` key changes WHICH model is
    // proposed — it never bypasses consent. The pinned entry is the proposal's
    // pick, and ZERO download requests are issued until the user answers, exactly
    // like the unpinned path. This is the failure this REQ exists to close: an
    // existing REQ-544 user who pinned a large model must not get an unprompted
    // multi-gigabyte fetch on first REQ-547 start.
    let h = Harness::new(
        "pin",
        RecordingFetcher::serving(),
        LocalModelConfig {
            pinned: Some("alt-fit".to_owned()),
            ..LocalModelConfig::default()
        },
    );
    let mut sub = h.subscribe();

    // Nothing has run yet.
    assert_eq!(h.fetcher.call_count(), 0);

    let resolve = h.gate.resolve();
    let answer = async {
        let proposal = next_proposal(&mut sub).await;

        // The pin drives the proposal's pick — but nothing has been fetched.
        assert_eq!(
            proposal.proposed.as_ref().unwrap().entry.name,
            "alt-fit",
            "the pin should name the proposed entry"
        );
        assert_eq!(
            h.fetcher.call_count(),
            0,
            "a pin must not trigger a download before the user answers: {:?}",
            h.fetcher.calls()
        );
        assert!(!h.installed("alt-fit").exists());

        // Wait a beat and re-check: the gate is awaiting an answer, not racing
        // ahead of one just because a pin is configured.
        tokio::time::sleep(Duration::from_millis(20)).await;
        assert_eq!(h.fetcher.call_count(), 0);

        assert!(h
            .pending
            .resolve(&proposal.request_id, ModelConfirmOutcome::Accept));
    };
    let (outcome, ()) = tokio::join!(resolve, answer);

    // Answered: now — and only now — the pinned entry installs.
    assert!(
        matches!(outcome, ConsentOutcome::Ready { .. }),
        "{outcome:?}"
    );
    assert!(h.installed("alt-fit").exists());
    assert!(!h.installed("small-fit").exists());
    assert!(
        h.fetcher.calls().iter().all(|url| url.contains("alt-fit")),
        "the daemon fetched something other than the pinned entry: {:?}",
        h.fetcher.calls()
    );
    h.cleanup();
}

// ---------------------------------------------------------------------------
// AC-10 / BR-12 — offline accept is a failure, not a decline
// ---------------------------------------------------------------------------

#[tokio::test]
async fn an_offline_accept_errors_clearly_installs_nothing_and_is_not_recorded_as_declined() {
    let h = Harness::new(
        "ac10",
        RecordingFetcher::offline(),
        LocalModelConfig::default(),
    );
    let mut sub = h.subscribe();

    let resolve = h.gate.resolve();
    let answer = async {
        let proposal = next_proposal(&mut sub).await;
        assert_eq!(h.fetcher.call_count(), 0);
        h.pending
            .resolve(&proposal.request_id, ModelConfirmOutcome::Accept);
    };
    let (outcome, ()) = tokio::join!(resolve, answer);

    // A clear, actionable network error — distinct from a corrupt download.
    let ConsentOutcome::InstallFailed { model_name, error } = outcome else {
        panic!("expected an install failure, got {outcome:?}");
    };
    assert_eq!(model_name, "small-fit");
    assert!(
        matches!(error, InstallError::Network { .. }),
        "an unreachable host must not be reported as corruption: {error:?}"
    );
    let message = error.to_string();
    assert!(message.contains("Nothing was installed"), "{message}");
    // BR-11: the error text names no filesystem path.
    assert!(
        !message.contains(h.dir.to_str().unwrap()),
        "the error leaked the state directory: {message}"
    );

    // No partial install: nothing at the final path, and the install state is
    // never `verified` (AC-7's invariant, asserted here for the offline case).
    assert!(!h.installed("small-fit").exists());
    let runtime = Arc::new(DaemonRuntime::with_consent(Arc::clone(&h.gate)));
    let status = runtime.model_status();
    assert_ne!(
        status.install.as_ref().unwrap().status,
        InstallStatus::Verified
    );

    // The decision is recorded, and it is NOT a decline (BR-12).
    let recorded = SelectionStore::open(&h.dir).current().unwrap();
    assert!(
        !recorded.declined_local,
        "a failed install must never be written down as a decline"
    );
    assert_eq!(recorded.model_name.as_deref(), Some("small-fit"));

    // A later run — now with connectivity — re-prompts and succeeds (AC-10).
    let later = Harness::in_dir(
        h.dir.clone(),
        RecordingFetcher::serving(),
        LocalModelConfig::default(),
    );
    let mut sub2 = later.subscribe();
    let resolve = later.gate.resolve();
    let answer = async {
        let proposal = next_proposal(&mut sub2).await;
        later
            .pending
            .resolve(&proposal.request_id, ModelConfirmOutcome::Accept);
    };
    let (outcome, ()) = tokio::join!(resolve, answer);
    assert!(
        matches!(outcome, ConsentOutcome::Ready { .. }),
        "{outcome:?}"
    );
    assert!(later.installed("small-fit").exists());
    later.cleanup();
}

// ---------------------------------------------------------------------------
// AC-2 / AC-3 backend — accept, override, and the RAM-floor double confirmation
// ---------------------------------------------------------------------------

#[tokio::test]
async fn choosing_an_alternative_installs_that_entry_instead_of_the_proposed_one() {
    let h = Harness::new(
        "ac3",
        RecordingFetcher::serving(),
        LocalModelConfig::default(),
    );
    let mut sub = h.subscribe();

    let resolve = h.gate.resolve();
    let answer = async {
        let proposal = next_proposal(&mut sub).await;
        assert_eq!(proposal.proposed.as_ref().unwrap().entry.name, "small-fit");
        assert!(proposal
            .alternatives
            .iter()
            .any(|entry| entry.name == "alt-fit"));
        h.pending.resolve(
            &proposal.request_id,
            ModelConfirmOutcome::Choose {
                name: "alt-fit".to_owned(),
                confirmed_above_ram_floor: false,
            },
        );
    };
    let (outcome, ()) = tokio::join!(resolve, answer);

    assert!(
        matches!(outcome, ConsentOutcome::Ready { .. }),
        "{outcome:?}"
    );
    assert!(h.installed("alt-fit").exists());
    assert!(
        !h.installed("small-fit").exists(),
        "the proposed entry must not be installed when an alternative was chosen"
    );
    assert!(
        h.fetcher.calls().iter().all(|url| url.contains("alt-fit")),
        "the daemon fetched something other than the chosen entry: {:?}",
        h.fetcher.calls()
    );
    h.cleanup();
}

#[tokio::test]
async fn an_above_ram_floor_choice_needs_a_second_confirmation() {
    let h = Harness::new(
        "br3",
        RecordingFetcher::serving(),
        LocalModelConfig::default(),
    );
    let runtime = Arc::new(DaemonRuntime::with_consent(Arc::clone(&h.gate)));

    // Unconfirmed: refused at the RPC boundary, and the proposal is untouched.
    let refused = runtime.confirm_model(ModelConfirmParams {
        request_id: RequestId::from("model-0"),
        outcome: ModelConfirmOutcome::Choose {
            name: "oversized".to_owned(),
            confirmed_above_ram_floor: false,
        },
    });
    let err = refused.expect_err("an above-RAM-floor pick must not be applied unconfirmed");
    assert!(err.message.contains("64.0 GiB"), "{}", err.message);
    assert!(err.message.contains("16.0 GiB"), "{}", err.message);
    assert_eq!(h.fetcher.call_count(), 0);

    // An unknown name is refused the same way.
    let err = runtime
        .confirm_model(ModelConfirmParams {
            request_id: RequestId::from("model-0"),
            outcome: ModelConfirmOutcome::Choose {
                name: "no-such-model".to_owned(),
                confirmed_above_ram_floor: true,
            },
        })
        .expect_err("an unknown catalog name must be refused");
    assert!(err.message.contains("no-such-model"), "{}", err.message);

    // Confirmed: permitted — the user's machine is the user's call (BR-3).
    let mut sub = h.subscribe();
    let resolve = h.gate.resolve();
    let answer = async {
        let proposal = next_proposal(&mut sub).await;
        h.pending.resolve(
            &proposal.request_id,
            ModelConfirmOutcome::Choose {
                name: "oversized".to_owned(),
                confirmed_above_ram_floor: true,
            },
        );
    };
    let (outcome, ()) = tokio::join!(resolve, answer);
    assert!(
        matches!(outcome, ConsentOutcome::Ready { .. }),
        "{outcome:?}"
    );
    assert!(h.installed("oversized").exists());
    h.cleanup();
}

#[tokio::test]
async fn the_gate_refuses_an_invalid_choice_without_fetching_anything() {
    // Defence in depth: even if a caller bypassed `model/confirm`'s validation,
    // the gate itself refuses and never reaches the fetcher.
    let h = Harness::new(
        "refuse",
        RecordingFetcher::serving(),
        LocalModelConfig::default(),
    );
    let mut sub = h.subscribe();

    let resolve = h.gate.resolve();
    let answer = async {
        let proposal = next_proposal(&mut sub).await;
        h.pending.resolve(
            &proposal.request_id,
            ModelConfirmOutcome::Choose {
                name: "oversized".to_owned(),
                confirmed_above_ram_floor: false,
            },
        );
    };
    let (outcome, ()) = tokio::join!(resolve, answer);

    assert!(
        matches!(outcome, ConsentOutcome::Refused { .. }),
        "{outcome:?}"
    );
    assert_eq!(h.fetcher.call_count(), 0);
    assert!(
        SelectionStore::open(&h.dir).current().is_none(),
        "a refused answer must record nothing"
    );
    h.cleanup();
}

#[tokio::test]
async fn an_answer_for_an_unknown_proposal_is_ignored_and_the_real_one_still_resolves() {
    let h = Harness::new(
        "stale-answer",
        RecordingFetcher::serving(),
        LocalModelConfig::default(),
    );
    let mut sub = h.subscribe();

    let resolve = h.gate.resolve();
    let answer = async {
        let proposal = next_proposal(&mut sub).await;

        // A stale or fabricated id finds no waiter and changes nothing — the
        // same idempotence `permission/respond` has.
        assert!(!h.pending.resolve(
            &RequestId::from("model-does-not-exist"),
            ModelConfirmOutcome::Accept
        ));
        assert_eq!(h.pending.pending_count(), 1);
        assert_eq!(h.fetcher.call_count(), 0);

        h.pending
            .resolve(&proposal.request_id, ModelConfirmOutcome::Decline);
    };
    let (outcome, ()) = tokio::join!(resolve, answer);
    assert_eq!(outcome, ConsentOutcome::Declined);
    assert_eq!(h.fetcher.call_count(), 0);
    h.cleanup();
}

#[tokio::test]
async fn a_below_floor_machine_proposes_nothing_and_accepting_is_refused() {
    // 4 GiB: below the local-tier floor entirely. The proposal still goes out —
    // BR-3 lets the user override to any entry — but there is nothing to
    // "accept", and guessing would be exactly the autonomous download this REQ
    // exists to stop.
    let h = Harness::on(
        "below-floor",
        HardwareProfile {
            ram_bytes: 4 * GIB,
            free_disk_bytes: 400 * GIB,
            gpu: GpuClass::Cpu,
        },
        RecordingFetcher::serving(),
    );
    let mut sub = h.subscribe();

    let resolve = h.gate.resolve();
    let answer = async {
        let proposal = next_proposal(&mut sub).await;
        assert!(proposal.proposed.is_none());
        assert_eq!(
            proposal.probe.chosen_band,
            teton_protocol::events::ChosenBand::None
        );
        assert_eq!(
            proposal.alternatives.len(),
            3,
            "every entry stays selectable (BR-3)"
        );
        h.pending
            .resolve(&proposal.request_id, ModelConfirmOutcome::Accept);
    };
    let (outcome, ()) = tokio::join!(resolve, answer);

    assert!(
        matches!(outcome, ConsentOutcome::Refused { .. }),
        "{outcome:?}"
    );
    assert_eq!(h.fetcher.call_count(), 0);
    h.cleanup();
}

#[tokio::test]
async fn installing_with_no_recorded_decision_is_undecided_and_fetches_nothing() {
    let h = Harness::new(
        "undecided",
        RecordingFetcher::serving(),
        LocalModelConfig::default(),
    );
    assert_eq!(h.gate.install_recorded().await, ConsentOutcome::Undecided);
    assert_eq!(h.fetcher.call_count(), 0);

    // A decline is not something to install, either.
    h.store.record(&ModelSelection::declined(3)).unwrap();
    assert_eq!(h.gate.install_recorded().await, ConsentOutcome::Declined);
    assert_eq!(h.fetcher.call_count(), 0);
    h.cleanup();
}

// ---------------------------------------------------------------------------
// M-1 — a successful install on a no-engine build is honest about it
// ---------------------------------------------------------------------------

/// The install succeeds — the bytes on disk are the catalog's bytes — but the
/// production build has no engine to load them, so the daemon publishes the
/// no-engine `disabled`, not `ready`. Saying `ready` here would be the exact
/// untruth `startup_lifecycle` refuses to tell; the gate refuses it too (M-1).
#[tokio::test]
async fn a_successful_install_on_a_no_engine_build_is_disabled_not_ready() {
    let h = Harness::no_engine("no-engine", RecordingFetcher::serving());
    let mut sub = h.subscribe();

    h.gate
        .set_model("small-fit", false)
        .expect("small-fit fits");
    let outcome = h.gate.install_recorded().await;

    // Not Ready, and — crucially — not InstallFailed: the install *did* succeed.
    assert_eq!(
        outcome,
        ConsentOutcome::InstalledNoEngine {
            model_name: "small-fit".to_owned(),
        },
        "{outcome:?}"
    );
    assert!(!outcome.local_tier_ready());
    assert_eq!(
        h.gate
            .current_install()
            .expect("a model is selected")
            .status,
        InstallStatus::Verified,
        "the weights really are installed and verified"
    );

    // The lifecycle event a client sees is the honest no-engine `disabled`.
    let stages: Vec<ModelLifecycleStage> = drain(&mut sub)
        .await
        .into_iter()
        .filter_map(|e| match e {
            Event::ModelLifecycle(lifecycle) => Some(lifecycle.stage),
            _ => None,
        })
        .collect();
    assert!(
        stages.iter().any(|stage| matches!(
            stage,
            ModelLifecycleStage::Disabled { reason }
                if reason.contains("no local inference engine")
        )),
        "expected the no-engine disabled stage; got {stages:?}"
    );
    assert!(
        !stages
            .iter()
            .any(|stage| matches!(stage, ModelLifecycleStage::Ready)),
        "a no-engine build must not publish `ready`; got {stages:?}"
    );
    h.cleanup();
}

// ---------------------------------------------------------------------------
// M-2 — concurrent installs of one entry serialize on the shared `.part`
// ---------------------------------------------------------------------------

/// Two installs of the same entry race for one shared `.part`. If both ran they
/// would append into it concurrently, interleaving bytes so the digest fails and
/// the user is told "corrupt" when nothing upstream was wrong. The M-2 guard lets
/// exactly one touch it; the other no-ops.
#[tokio::test]
async fn two_concurrent_installs_of_one_entry_do_not_both_fetch_or_corrupt() {
    let h = Harness::new(
        "concurrent-install",
        RecordingFetcher::serving(),
        LocalModelConfig::default(),
    );
    h.gate
        .set_model("small-fit", false)
        .expect("small-fit fits");

    // Both target the recorded entry, concurrently.
    let (a, b) = tokio::join!(h.gate.install_recorded(), h.gate.install_recorded());
    let outcomes = [a, b];

    // Exactly one fetched — the shared `.part` was written by one install only.
    assert_eq!(
        h.fetcher.call_count(),
        1,
        "both installs fetched, racing on the shared .part: {:?}",
        h.fetcher.calls()
    );
    assert_eq!(
        outcomes.iter().filter(|o| o.local_tier_ready()).count(),
        1,
        "exactly one install should reach Ready: {outcomes:?}"
    );
    assert!(
        outcomes
            .iter()
            .any(|o| matches!(o, ConsentOutcome::AlreadyInstalling { .. })),
        "the deferred install must report AlreadyInstalling: {outcomes:?}"
    );

    // And the weights on disk are exactly the artifact, uncorrupted.
    assert_eq!(
        h.gate
            .current_install()
            .expect("a model is selected")
            .status,
        InstallStatus::Verified
    );
    assert_eq!(std::fs::read(h.installed("small-fit")).unwrap(), SMALL_BODY);

    // The claim is released once the installs finish — a later install is free
    // to run rather than being wedged by a stale in-flight marker.
    assert!(!h.gate.install_in_flight("small-fit"));
    h.cleanup();
}

// ---------------------------------------------------------------------------
// BR-11 — payload hygiene
// ---------------------------------------------------------------------------

#[tokio::test]
async fn the_proposal_payload_carries_no_path_url_digest_or_credential() {
    let h = Harness::new(
        "br11",
        RecordingFetcher::serving(),
        LocalModelConfig::default(),
    );
    let mut sub = h.subscribe();

    let resolve = h.gate.resolve();
    let inspect = async {
        let proposal = next_proposal(&mut sub).await;
        let json = serde_json::to_string(&proposal).unwrap();

        for forbidden in [
            "http",
            REVISION,
            SMALL_SHA,
            ".gguf",
            h.dir.to_str().unwrap(),
            "/",
        ] {
            assert!(
                !json.contains(forbidden),
                "BR-11: the proposal leaked {forbidden:?}: {json}"
            );
        }
        // What it *does* carry: the hardware reasoning the user needs (BR-2).
        assert!(json.contains("total_ram_bytes"));
        assert!(json.contains("required_disk_bytes"));
        assert!(json.contains("reason"));

        h.pending
            .resolve(&proposal.request_id, ModelConfirmOutcome::Decline);
    };
    let (_outcome, ()) = tokio::join!(resolve, inspect);
    h.cleanup();
}

// ---------------------------------------------------------------------------
// AC-9 — model/list, model/set, model/status
// ---------------------------------------------------------------------------

#[tokio::test]
async fn model_list_reports_each_entry_s_fit_and_the_current_selection() {
    let h = Harness::new(
        "list",
        RecordingFetcher::serving(),
        LocalModelConfig::default(),
    );
    let runtime = Arc::new(DaemonRuntime::with_consent(Arc::clone(&h.gate)));

    let listed = runtime.model_list();
    assert_eq!(listed.probe.total_ram_bytes, 16 * GIB);
    assert_eq!(listed.models.len(), 3);
    assert!(listed.selection.is_none(), "nothing is decided yet");

    let oversized = listed
        .models
        .iter()
        .find(|row| row.entry.name == "oversized")
        .unwrap();
    assert!(!oversized.fits_ram, "64 GiB floor on a 16 GiB machine");
    assert!(oversized.fits_disk);

    h.store
        .record(&ModelSelection::accepted(
            "alt-fit",
            SelectionSource::UserOverride,
            5,
        ))
        .unwrap();
    let listed = runtime.model_list();
    assert_eq!(
        listed.selection.unwrap().model_name.as_deref(),
        Some("alt-fit")
    );
    h.cleanup();
}

#[tokio::test]
async fn model_set_changes_the_selection_and_refuses_an_unconfirmed_oversized_pick() {
    let h = Harness::new(
        "set",
        RecordingFetcher::serving(),
        LocalModelConfig::default(),
    );
    let runtime = Arc::new(DaemonRuntime::with_consent(Arc::clone(&h.gate)));

    let err = runtime
        .set_model("oversized", false)
        .expect_err("BR-3 applies to `model/set` too");
    assert!(err.message.contains("64.0 GiB"), "{}", err.message);
    assert!(SelectionStore::open(&h.dir).current().is_none());

    let result = runtime.set_model("alt-fit", false).unwrap();
    assert_eq!(result.selection.model_name.as_deref(), Some("alt-fit"));
    assert!(!result.selection.declined_local);
    assert_eq!(
        result.selection.source,
        teton_protocol::events::SelectionSource::UserOverride
    );
    // Persisted, so it survives the daemon (D-4).
    assert_eq!(
        SelectionStore::open(&h.dir)
            .current()
            .unwrap()
            .model_name
            .as_deref(),
        Some("alt-fit")
    );

    // The install is a separate step, so the RPC answers immediately.
    let outcome = runtime.install_selected_model().await;
    assert!(
        matches!(outcome, ConsentOutcome::Ready { .. }),
        "{outcome:?}"
    );
    assert!(h.installed("alt-fit").exists());
    h.cleanup();
}

#[tokio::test]
async fn model_status_exposes_the_open_proposal_so_a_late_client_can_answer() {
    let h = Harness::new(
        "status",
        RecordingFetcher::serving(),
        LocalModelConfig::default(),
    );
    let runtime = Arc::new(DaemonRuntime::with_consent(Arc::clone(&h.gate)));

    assert!(runtime.model_status().pending_proposal.is_none());

    let mut sub = h.subscribe();
    let resolve = h.gate.resolve();
    let late_client = async {
        let proposal = next_proposal(&mut sub).await;
        // A client that attached after the broadcast finds the open prompt here
        // rather than waiting forever for an event it already missed.
        let status = runtime.model_status();
        // Not just "a prompt is open" — the whole proposal, so the client can
        // render the pick by name with its size and RAM floor (BR-2).
        let pending = status
            .pending_proposal
            .expect("the open proposal is retrievable from model/status");
        assert_eq!(
            pending, proposal,
            "the retrieved payload IS the broadcast one"
        );
        let proposed = pending.proposed.expect("this machine has a pick");
        assert_eq!(proposed.entry.name, "small-fit");
        assert!(proposed.entry.size_bytes > 0 && proposed.entry.ram_floor_bytes > 0);
        assert!(status.selection.is_none());
        h.pending
            .resolve(&proposal.request_id, ModelConfirmOutcome::Decline);
    };
    let (_outcome, ()) = tokio::join!(resolve, late_client);

    let status = runtime.model_status();
    assert!(status.pending_proposal.is_none());
    assert!(status.selection.unwrap().declined_local);
    assert!(status.install.is_none(), "a decline installs nothing");
    h.cleanup();
}

// ---------------------------------------------------------------------------
// M-4 — a `model/set` supersedes an outstanding proposal
// ---------------------------------------------------------------------------

#[tokio::test]
async fn a_model_set_during_an_outstanding_proposal_supersedes_it_and_a_late_accept_is_ignored() {
    // M-4 / BR-10: a `model/set` while a first-run proposal is outstanding is an
    // explicit decision. It cancels the proposal, so (a) the parked consent flow
    // abandons rather than overwriting the choice, (b) a late `Accept` for the old
    // proposal finds no waiter and installs nothing, and (c) the set choice is
    // what stands and is not re-prompted on a later start.
    let h = Harness::new(
        "m4-set-cancels",
        RecordingFetcher::serving(),
        LocalModelConfig::default(),
    );
    let runtime = Arc::new(DaemonRuntime::with_consent(Arc::clone(&h.gate)));
    let mut sub = h.subscribe();

    let drive = {
        let runtime = Arc::clone(&runtime);
        async move { runtime.run_model_consent().await }
    };
    let interject = async {
        let proposal = next_proposal(&mut sub).await;
        assert_eq!(proposal.proposed.as_ref().unwrap().entry.name, "small-fit");
        assert_eq!(h.pending.pending_count(), 1);

        // A `model/set` for a DIFFERENT model lands mid-proposal.
        let set = runtime
            .set_model("alt-fit", false)
            .expect("the set is valid");
        assert_eq!(set.selection.model_name.as_deref(), Some("alt-fit"));

        // It cancelled the proposal: no waiter remains, so a late Accept for the
        // old proposal is a no-op and cannot install small-fit over the set choice.
        assert_eq!(
            h.pending.pending_count(),
            0,
            "the set must cancel the outstanding proposal"
        );
        assert!(
            !h.pending
                .resolve(&proposal.request_id, ModelConfirmOutcome::Accept),
            "a late Accept for the superseded proposal must find no waiter"
        );
    };
    let (outcome, ()) = tokio::join!(drive, interject);

    // The parked flow abandoned as superseded: it recorded nothing of its own.
    assert_eq!(
        outcome,
        ConsentOutcome::Superseded,
        "the parked flow must abandon, not overwrite: {outcome:?}"
    );
    let recorded = SelectionStore::open(&h.dir).current().unwrap();
    assert_eq!(
        recorded.model_name.as_deref(),
        Some("alt-fit"),
        "the explicit set choice must stand"
    );
    assert_eq!(recorded.source, SelectionSource::UserOverride);
    assert!(!recorded.declined_local);

    // A later start is NOT re-prompted: with the weights in place the set choice
    // is settled (BR-10). (The set-path drives its own install; this asserts the
    // decision, not the tier gate.)
    let later = Harness::in_dir(
        h.dir.clone(),
        RecordingFetcher::serving(),
        LocalModelConfig::default(),
    );
    std::fs::create_dir_all(&later.weights_dir).unwrap();
    std::fs::write(later.installed("alt-fit"), ALT_BODY).unwrap();
    let mut sub2 = later.subscribe();
    let outcome = later.gate.resolve().await;
    assert!(
        matches!(outcome, ConsentOutcome::Ready { .. }),
        "a settled set choice must not re-prompt: {outcome:?}"
    );
    assert_eq!(later.pending.pending_count(), 0);
    assert!(!drain(&mut sub2)
        .await
        .iter()
        .any(|e| matches!(e, Event::ModelSelectionProposed(_))));
    later.cleanup();
    h.cleanup();
}

// ---------------------------------------------------------------------------
// M-6 — a decision that cannot be persisted is surfaced, not swallowed
// ---------------------------------------------------------------------------

#[tokio::test]
async fn a_decline_whose_persistence_fails_is_surfaced_and_does_not_vanish_in_memory() {
    // M-6 / BR-4: if the decline cannot be written to disk, it must not silently
    // vanish — the daemon surfaces it (a diagnostic on stderr) and still honours
    // it for this process, so the user is not re-prompted mid-session. The disk
    // failure is modelled by a store whose parent path is a FILE, so the atomic
    // write can never create its directory.
    let base = temp_dir("decline-persist-fail");
    let wall = base.join("wall");
    std::fs::write(&wall, b"not a directory").unwrap();
    let unwritable = wall.join("state"); // parent (`wall`) is a file
    let store = Arc::new(SelectionStore::open(&unwritable));

    // Sanity: this store genuinely cannot persist.
    assert!(
        store.record(&ModelSelection::declined(1)).is_err(),
        "the failing store must actually fail to persist"
    );
    store.clear(); // reset the in-memory view to undecided for the gate below

    let bus = Arc::new(EventBus::new());
    let pending = Arc::new(PendingModelDecisions::new());
    let installer = Arc::new(
        WeightsInstall::new(
            Arc::new(RecordingFetcher::serving()) as Arc<dyn RangeFetcher + Send + Sync>,
            base.join("models"),
            None,
        )
        .with_free_space(Arc::new(FixedFreeSpace(Some(u64::MAX))) as Arc<dyn FreeSpace>),
    );
    let gate = Arc::new(ModelConsentGate::new(
        machine(),
        test_catalog(),
        LocalModelConfig::default(),
        Arc::clone(&bus),
        Arc::clone(&pending),
        Arc::clone(&store),
        installer,
    ));
    let mut sub = bus.subscribe(32);

    let resolve = gate.resolve();
    let answer = async {
        let proposal = next_proposal(&mut sub).await;
        pending.resolve(&proposal.request_id, ModelConfirmOutcome::Decline);
    };
    let (outcome, ()) = tokio::join!(resolve, answer);

    // The decline still takes effect for this daemon run: the outcome is Declined
    // and the in-memory decision is the decline, not an absence (BR-4). The
    // persistence failure was surfaced, not swallowed — the old `let _ = record`
    // would have discarded it silently.
    assert_eq!(outcome, ConsentOutcome::Declined);
    let current = store
        .current()
        .expect("the decline is held in memory even though the disk write failed");
    assert!(
        current.declined_local,
        "the decline must not vanish in memory"
    );

    let _ = std::fs::remove_dir_all(&base);
}

// ---------------------------------------------------------------------------
// Minor — `accept` with no proposed model is pre-validated at the RPC boundary
// ---------------------------------------------------------------------------

#[tokio::test]
async fn confirm_model_pre_validates_an_accept_with_no_proposed_model() {
    // An `accept` for a proposal that offered no model is rejected up front as
    // INVALID_PARAMS with the proposal left OPEN — rather than resolving the
    // waiter and failing inside the flow as `NothingToAccept`, which would
    // permanently consume the user's one chance to answer and leave the tier dead.
    let h = Harness::on(
        "accept-prevalidate",
        HardwareProfile {
            ram_bytes: 4 * GIB,
            free_disk_bytes: 400 * GIB,
            gpu: GpuClass::Cpu,
        },
        RecordingFetcher::serving(),
    );
    let runtime = Arc::new(DaemonRuntime::with_consent(Arc::clone(&h.gate)));
    let mut sub = h.subscribe();

    let resolve = h.gate.resolve();
    let probe = async {
        let proposal = next_proposal(&mut sub).await;
        assert!(
            proposal.proposed.is_none(),
            "a below-floor machine proposes nothing to accept"
        );

        // The accept is rejected without consuming the waiter.
        let err = runtime
            .confirm_model(ModelConfirmParams {
                request_id: proposal.request_id.clone(),
                outcome: ModelConfirmOutcome::Accept,
            })
            .expect_err("accept with no proposed model must be INVALID_PARAMS");
        assert!(
            err.message.contains("no fitting catalog model"),
            "{}",
            err.message
        );
        assert_eq!(
            h.pending.pending_count(),
            1,
            "the proposal must stay open so the user can still answer"
        );
        assert_eq!(h.fetcher.call_count(), 0);

        // The user can still answer for real — here, by declining.
        h.pending
            .resolve(&proposal.request_id, ModelConfirmOutcome::Decline);
    };
    let (outcome, ()) = tokio::join!(resolve, probe);
    assert_eq!(outcome, ConsentOutcome::Declined);
    h.cleanup();
}

// ---------------------------------------------------------------------------
// The round-trip over a real socket (the `permission/respond` ordering)
// ---------------------------------------------------------------------------

/// A minimal in-test JSON-RPC client over the daemon socket.
struct TestClient {
    reader: BufReader<OwnedReadHalf>,
    writer: OwnedWriteHalf,
}

impl TestClient {
    async fn connect(path: &Path) -> Self {
        let stream = UnixStream::connect(path).await.unwrap();
        let (read_half, write_half) = stream.into_split();
        Self {
            reader: BufReader::new(read_half),
            writer: write_half,
        }
    }

    async fn send(&mut self, id: i64, method: &str, params: Value) {
        let message = json!({"jsonrpc": "2.0", "id": id, "method": method, "params": params});
        let mut text = serde_json::to_string(&message).unwrap();
        text.push('\n');
        self.writer.write_all(text.as_bytes()).await.unwrap();
        self.writer.flush().await.unwrap();
    }

    async fn read_line(&mut self) -> Value {
        let mut line = String::new();
        let n = timeout(Duration::from_secs(5), self.reader.read_line(&mut line))
            .await
            .expect("timed out waiting for a line")
            .unwrap();
        assert!(n > 0, "connection closed unexpectedly");
        serde_json::from_str(&line).unwrap()
    }

    async fn read_response(&mut self, id: i64) -> Value {
        loop {
            let value = self.read_line().await;
            if value.get("id").and_then(Value::as_i64) == Some(id) {
                return value;
            }
        }
    }

    async fn read_event(&mut self, name: &str) -> Value {
        loop {
            let value = self.read_line().await;
            if value.get("method").and_then(Value::as_str) == Some("event")
                && value["params"]["event"].as_str() == Some(name)
            {
                return value;
            }
        }
    }

    async fn handshake(&mut self, id: i64) {
        self.send(
            id,
            "handshake",
            json!({
                "client_kind": "cli",
                "client_name": "consent-test",
                "client_version": "0.1.0",
                "protocol_min": 1,
                "protocol_max": 1,
            }),
        )
        .await;
        assert!(self.read_response(id).await.get("result").is_some());
    }
}

fn temp_socket(tag: &str) -> PathBuf {
    std::env::temp_dir().join(format!(
        "teton-{tag}-{}-{}.sock",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ))
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_reader_loop_serves_sessions_while_a_proposal_is_outstanding() {
    let h = Harness::new(
        "socket",
        RecordingFetcher::serving(),
        LocalModelConfig::default(),
    );
    let runtime = Arc::new(DaemonRuntime::with_consent(Arc::clone(&h.gate)));
    let path = temp_socket("consent");
    let listener = server::bind_listener(&path).unwrap();
    let daemon = Arc::new(Daemon::with_runtime(
        Arc::clone(&h.bus),
        Arc::clone(&runtime),
    ));
    let server_task = tokio::spawn(server::serve(listener, daemon));

    let mut client = TestClient::connect(&path).await;
    client.handshake(1).await;

    // The flow starts only after the client is subscribed, so it sees the
    // proposal. It runs on its own task — exactly as `main` spawns it.
    let consent_task = {
        let runtime = Arc::clone(&runtime);
        tokio::spawn(async move { runtime.run_model_consent().await })
    };

    let event = client.read_event("model_selection_proposed").await;
    let request_id = event["params"]["request_id"].as_str().unwrap().to_owned();
    assert_eq!(
        event["params"]["proposed"]["entry"]["name"]
            .as_str()
            .unwrap(),
        "small-fit"
    );
    assert_eq!(
        h.fetcher.call_count(),
        0,
        "AC-1 over the wire: nothing downloads before the client answers"
    );

    // The reader loop is NOT blocked on the outstanding proposal: ordinary
    // session work is served while the consent flow awaits (D-3).
    client
        .send(2, "session/create", json!({"mode": "freeform"}))
        .await;
    let created = client.read_response(2).await;
    assert!(created["result"]["session_id"].is_string());

    client.send(3, "session/list", json!({})).await;
    assert_eq!(
        client.read_response(3).await["result"]["sessions"]
            .as_array()
            .unwrap()
            .len(),
        1
    );

    // `model/status` surfaces the open prompt to a client that wants to answer.
    client.send(4, "model/status", json!({})).await;
    let status = client.read_response(4).await;
    assert_eq!(
        status["result"]["pending_proposal"]["request_id"]
            .as_str()
            .unwrap(),
        request_id
    );

    // A bad answer is an RPC error and leaves the proposal open (BR-3): the
    // message names the shortfall, so it is clearly the RAM-floor refusal rather
    // than a parse failure sharing the same code.
    client
        .send(
            5,
            "model/confirm",
            json!({
                "request_id": request_id,
                "outcome": {"outcome": "choose", "name": "oversized"},
            }),
        )
        .await;
    let refused = client.read_response(5).await;
    assert_eq!(refused["error"]["code"].as_i64().unwrap(), -32602);
    let message = refused["error"]["message"].as_str().unwrap();
    assert!(message.contains("64.0 GiB"), "{message}");
    assert_eq!(h.fetcher.call_count(), 0);

    // An `outcome` this build does not understand is an error, never "accept".
    client
        .send(
            6,
            "model/confirm",
            json!({"request_id": request_id, "outcome": {"outcome": "yolo"}}),
        )
        .await;
    assert_eq!(
        client.read_response(6).await["error"]["code"]
            .as_i64()
            .unwrap(),
        -32602
    );
    assert_eq!(h.fetcher.call_count(), 0);

    // The real answer resolves the waiter the flow is parked on.
    client
        .send(
            7,
            "model/confirm",
            json!({"request_id": request_id, "outcome": {"outcome": "accept"}}),
        )
        .await;
    let accepted = client.read_response(7).await;
    assert!(accepted.get("result").is_some(), "{accepted}");

    let outcome = timeout(Duration::from_secs(10), consent_task)
        .await
        .expect("the consent flow never completed — the round-trip deadlocked")
        .unwrap();
    assert!(outcome.local_tier_ready(), "{outcome:?}");
    assert!(h.installed("small-fit").exists());

    server_task.abort();
    let _ = std::fs::remove_file(&path);
    h.cleanup();
}
