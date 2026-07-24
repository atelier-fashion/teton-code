//! The install pipeline's behavioural suite (REQ-547 TASK-005).
//!
//! Four claims, each asserted against the *production* [`WeightsInstall`] with
//! only its transport and its free-space reading replaced:
//!
//! - **AC-6** — an install that cannot fit on the volume refuses *before any
//!   bytes are fetched*. The instrument is a recording fetcher that must show
//!   zero calls: "refuses first" is otherwise indistinguishable from "refuses
//!   eventually", and only the first is what BR-7 asks for.
//! - **AC-7 / BR-9** — the install is atomic. An interrupt mid-transfer leaves
//!   the partial file and nothing at the final path, and the *observation is
//!   made from inside the transfer* rather than inferred after it: the fetcher
//!   inspects the weights directory while it streams.
//! - **BR-6** — a mismatched digest is discarded and never installed, and the
//!   reported [`InstallStatus`] is never `verified` for a truncated artifact.
//! - **AC-2 / AC-12 / BR-16** — progress is observable on `model_lifecycle`,
//!   a configured base URL redirects the fetch to the mirror, and a rate-limited
//!   host reads as rate-limiting rather than as corruption.
//!
//! The resume path is exercised end-to-end here (an interrupted install followed
//! by a second install that continues from the bytes already on disk) rather than
//! re-tested in the abstract — `teton_inference` already owns the resume
//! contract; what this file checks is that the install shell preserves it.

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use tokio::time::timeout;

use teton_inference::catalog::{ModelEntry, TierBand};
use teton_inference::download::{DownloadError, RangeFetcher};
use teton_inference::hash::sha256_hex;

use teton_protocol::events::{Event, ModelLifecycleStage};
use teton_protocol::methods::InstallStatus;

use tetond::broadcast::{EventBus, Subscription};
use tetond::download::FetchError;
use tetond::install::{
    FetchCause, FixedFreeSpace, FreeSpace, InstallProgress, InstallStep, LifecycleProgress,
    WeightsInstall,
};
use tetond::model_consent::{
    required_disk_bytes, InstallError, WeightsInstaller, DISK_WORKING_MARGIN_BYTES,
};

// ---------------------------------------------------------------------------
// Fixtures
// ---------------------------------------------------------------------------

/// A commit-SHA-shaped revision; the catalog pins immutable revisions (BR-15).
const REVISION: &str = "0123456789abcdef0123456789abcdef01234567";

/// The artifact the doubles serve. Long enough that a half-delivered transfer is
/// unambiguously partial, short enough to stay a fixture.
fn body() -> Vec<u8> {
    (0u8..=250).cycle().take(4096).collect()
}

fn entry(body: &[u8]) -> ModelEntry {
    ModelEntry {
        name: "small-fit".to_owned(),
        url: format!("https://models.test.invalid/Org/Repo/resolve/{REVISION}/small-fit.gguf"),
        revision: REVISION.to_owned(),
        sha256: sha256_hex(body),
        size_bytes: body.len() as u64,
        ram_floor_bytes: 8 * 1024 * 1024 * 1024,
        band: TierBand::Small,
    }
}

fn temp_dir(tag: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!(
        "teton-install-pipeline-{tag}-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

/// Room for anything.
fn ample() -> Arc<dyn FreeSpace> {
    Arc::new(FixedFreeSpace(Some(u64::MAX)))
}

// ---------------------------------------------------------------------------
// Doubles
// ---------------------------------------------------------------------------

/// What the fetcher does when it is called.
#[derive(Clone)]
enum Plan {
    /// Stream the whole artifact from the requested offset.
    Whole(Vec<u8>),
    /// Stream `cutoff` bytes then fail permanently — a killed transfer.
    Interrupt {
        /// The artifact being served.
        data: Vec<u8>,
        /// Bytes to deliver before the failure.
        cutoff: usize,
    },
    /// Stream bytes of the right length whose digest is wrong.
    Corrupt(Vec<u8>),
    /// Fail every call at the transport, delivering nothing.
    Fail,
}

/// One recorded call, which is how "zero bytes were fetched" is measured.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Call {
    url: String,
    offset: u64,
}

/// What the directory looked like from *inside* a running transfer.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct MidFlight {
    installed_exists: bool,
    partial_exists: bool,
}

/// A [`RangeFetcher`] that records every call, follows a [`Plan`], and — when
/// asked — inspects the weights directory while it is streaming.
struct TestFetcher {
    plan: Plan,
    calls: Mutex<Vec<Call>>,
    watch: Option<(PathBuf, PathBuf)>,
    seen: Mutex<Vec<MidFlight>>,
}

impl TestFetcher {
    fn new(plan: Plan) -> Self {
        Self {
            plan,
            calls: Mutex::new(Vec::new()),
            watch: None,
            seen: Mutex::new(Vec::new()),
        }
    }

    /// Observe `installed` and `partial` from inside every chunk delivery.
    fn watching(mut self, installed: PathBuf, partial: PathBuf) -> Self {
        self.watch = Some((installed, partial));
        self
    }

    fn calls(&self) -> Vec<Call> {
        self.calls.lock().unwrap().clone()
    }

    fn call_count(&self) -> usize {
        self.calls.lock().unwrap().len()
    }

    fn mid_flight(&self) -> Vec<MidFlight> {
        self.seen.lock().unwrap().clone()
    }

    fn observe(&self) {
        if let Some((installed, partial)) = &self.watch {
            self.seen.lock().unwrap().push(MidFlight {
                installed_exists: installed.exists(),
                partial_exists: partial.exists(),
            });
        }
    }
}

impl RangeFetcher for TestFetcher {
    fn fetch(
        &self,
        url: &str,
        offset: u64,
        sink: &mut dyn FnMut(&[u8]) -> Result<(), DownloadError>,
    ) -> Result<u64, DownloadError> {
        self.calls.lock().unwrap().push(Call {
            url: url.to_owned(),
            offset,
        });
        let start = usize::try_from(offset).unwrap_or(usize::MAX);
        match &self.plan {
            Plan::Whole(data) | Plan::Corrupt(data) => {
                if start < data.len() {
                    sink(&data[start..])?;
                    self.observe();
                }
                Ok(data.len() as u64)
            }
            Plan::Interrupt { data, cutoff } => {
                let end = (*cutoff).min(data.len());
                if start < end {
                    sink(&data[start..end])?;
                    self.observe();
                }
                // A hard stop, not a resumable hiccup: the orchestrator gives up
                // immediately and the caller sees exactly what a killed daemon
                // would leave behind.
                Err(DownloadError::Io(std::io::Error::other(
                    "connection reset by peer",
                )))
            }
            Plan::Fail => Err(DownloadError::Transport(
                "could not reach the model host".to_owned(),
            )),
        }
    }
}

/// A fixed typed cause, standing in for the real client's `last_error` (AC-12).
struct FixedCause(Option<FetchError>);

impl FetchCause for FixedCause {
    fn last_cause(&self) -> Option<FetchError> {
        self.0.clone()
    }
}

/// A progress sink that keeps every step it is handed.
#[derive(Default)]
struct Recorder(Mutex<Vec<InstallStep>>);

impl Recorder {
    fn steps(&self) -> Vec<InstallStep> {
        self.0.lock().unwrap().clone()
    }
}

impl InstallProgress for Recorder {
    fn report(&self, _model_name: &str, step: &InstallStep) {
        self.0.lock().unwrap().push(step.clone());
    }
}

/// Everything published on `sub`, stopping once the stream goes quiet.
async fn drain(sub: &mut Subscription) -> Vec<Event> {
    let mut events = Vec::new();
    while let Ok(Some(envelope)) = timeout(Duration::from_millis(100), sub.recv()).await {
        events.push(envelope.event);
    }
    events
}

/// The download stages on `events`, in order.
fn download_stages(events: &[Event]) -> Vec<(u64, Option<u64>)> {
    events
        .iter()
        .filter_map(|event| match event {
            Event::ModelLifecycle(lifecycle) => match &lifecycle.stage {
                ModelLifecycleStage::Download {
                    downloaded_bytes,
                    total_bytes,
                } => Some((*downloaded_bytes, *total_bytes)),
                _ => None,
            },
            _ => None,
        })
        .collect()
}

fn installed_path(dir: &Path, entry: &ModelEntry) -> PathBuf {
    dir.join(format!("{}.gguf", entry.name))
}

fn partial_path(dir: &Path, entry: &ModelEntry) -> PathBuf {
    dir.join(format!("{}.gguf.part", entry.name))
}

// ---------------------------------------------------------------------------
// AC-6 — refuse before fetching
// ---------------------------------------------------------------------------

#[test]
fn insufficient_disk_refuses_before_a_single_byte_is_fetched() {
    let dir = temp_dir("ac6");
    let data = body();
    let model = entry(&data);
    let fetcher = Arc::new(TestFetcher::new(Plan::Whole(data.clone())));

    let install = WeightsInstall::new(
        Arc::clone(&fetcher) as Arc<dyn RangeFetcher + Send + Sync>,
        dir.clone(),
        None,
    )
    // One byte short of the requirement: the refusal is about the requirement,
    // not about a wildly empty disk.
    .with_free_space(Arc::new(FixedFreeSpace(Some(
        required_disk_bytes(&model) - 1,
    ))));

    let err = install.install(&model).expect_err("preflight must refuse");

    // The instrument: the transport was never asked for anything.
    assert_eq!(
        fetcher.call_count(),
        0,
        "the preflight fetched bytes before refusing: {:?}",
        fetcher.calls()
    );
    match &err {
        InstallError::InsufficientDisk {
            required_bytes,
            available_bytes,
        } => {
            // The figure the preflight enforces is the figure the proposal
            // advertised — one shared constant, never two.
            assert_eq!(*required_bytes, required_disk_bytes(&model));
            assert_eq!(
                *required_bytes,
                model.size_bytes + DISK_WORKING_MARGIN_BYTES
            );
            assert_eq!(*available_bytes, required_disk_bytes(&model) - 1);
        }
        other => panic!("expected InsufficientDisk, got {other:?}"),
    }
    // AC-6: the message names required and available.
    let rendered = err.to_string();
    assert!(rendered.contains("needed"), "message: {rendered}");
    assert!(rendered.contains("available"), "message: {rendered}");

    // Nothing was created on the way to refusing.
    assert!(!installed_path(&dir, &model).exists());
    assert!(!partial_path(&dir, &model).exists());
    assert_eq!(install.status(&model), InstallStatus::Absent);
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn a_resumable_download_is_not_refused_for_the_bytes_it_already_has() {
    let dir = temp_dir("resume-preflight");
    let data = body();
    let model = entry(&data);
    // A previous run left most of the artifact on disk.
    let already = 3000;
    std::fs::write(partial_path(&dir, &model), &data[..already]).unwrap();

    let fetcher = Arc::new(TestFetcher::new(Plan::Whole(data.clone())));
    let install = WeightsInstall::new(
        Arc::clone(&fetcher) as Arc<dyn RangeFetcher + Send + Sync>,
        dir.clone(),
        None,
    )
    // Exactly the shortfall: enough for what is left, not for the whole entry.
    .with_free_space(Arc::new(FixedFreeSpace(Some(
        required_disk_bytes(&model) - already as u64,
    ))));

    install
        .install(&model)
        .expect("the remaining bytes fit, so the install proceeds");
    assert_eq!(std::fs::read(installed_path(&dir, &model)).unwrap(), data);
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn an_unmeasurable_volume_installs_rather_than_refusing() {
    let dir = temp_dir("unmeasurable");
    let data = body();
    let model = entry(&data);
    let install = WeightsInstall::new(
        Arc::new(TestFetcher::new(Plan::Whole(data.clone()))),
        dir.clone(),
        None,
    )
    .with_free_space(Arc::new(FixedFreeSpace(None)));

    // "Unknown" is not "short": a platform that will not answer `statvfs` must
    // not make every model uninstallable.
    install.install(&model).expect("install proceeds");
    let _ = std::fs::remove_dir_all(&dir);
}

// ---------------------------------------------------------------------------
// AC-7 / BR-9 — atomicity
// ---------------------------------------------------------------------------

#[test]
fn an_interrupted_download_leaves_nothing_at_the_final_path() {
    let dir = temp_dir("atomic");
    let data = body();
    let model = entry(&data);
    let installed = installed_path(&dir, &model);
    let partial = partial_path(&dir, &model);

    let fetcher = Arc::new(
        TestFetcher::new(Plan::Interrupt {
            data: data.clone(),
            cutoff: 1500,
        })
        .watching(installed.clone(), partial.clone()),
    );
    let install = WeightsInstall::new(
        Arc::clone(&fetcher) as Arc<dyn RangeFetcher + Send + Sync>,
        dir.clone(),
        None,
    )
    .with_free_space(ample());

    let err = install
        .install(&model)
        .expect_err("the transfer was killed");
    assert!(
        matches!(err, InstallError::Network { .. }),
        "a killed transfer is a network failure, got {err:?}"
    );

    // The claim, observed from inside the transfer rather than inferred after
    // it: while bytes were arriving, only the temp path existed.
    let seen = fetcher.mid_flight();
    assert!(!seen.is_empty(), "the fetcher never observed the directory");
    for observation in &seen {
        assert!(
            !observation.installed_exists,
            "the final path existed mid-flight: {observation:?}"
        );
        assert!(observation.partial_exists, "no temp path: {observation:?}");
    }

    // And after: the partial survives (that is what resume needs) and the final
    // path was never created.
    assert!(
        !installed.exists(),
        "a partial artifact reached the loadable path"
    );
    assert_eq!(std::fs::read(&partial).unwrap(), data[..1500]);
    // BR-9 as the daemon reports it: partial, never verified.
    assert_eq!(install.status(&model), InstallStatus::Partial);
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn an_interrupted_download_resumes_from_the_bytes_already_written() {
    let dir = temp_dir("resume");
    let data = body();
    let model = entry(&data);
    let installed = installed_path(&dir, &model);

    // First run: killed at 1500 bytes.
    let interrupted = Arc::new(TestFetcher::new(Plan::Interrupt {
        data: data.clone(),
        cutoff: 1500,
    }));
    WeightsInstall::new(
        Arc::clone(&interrupted) as Arc<dyn RangeFetcher + Send + Sync>,
        dir.clone(),
        None,
    )
    .with_free_space(ample())
    .install(&model)
    .expect_err("first run is interrupted");
    assert_eq!(interrupted.calls()[0].offset, 0);

    // Second run: a fresh installer over the same directory, exactly as a
    // restarted daemon would build one.
    let resumed = Arc::new(TestFetcher::new(Plan::Whole(data.clone())));
    let install = WeightsInstall::new(
        Arc::clone(&resumed) as Arc<dyn RangeFetcher + Send + Sync>,
        dir.clone(),
        None,
    )
    .with_free_space(ample());
    install.install(&model).expect("the second run completes");

    // The resume is asserted at the transport: the first request of the second
    // run started past the bytes already on disk, so nothing was re-fetched.
    let calls = resumed.calls();
    assert_eq!(
        calls.len(),
        1,
        "the resumed run made extra requests: {calls:?}"
    );
    assert_eq!(calls[0].offset, 1500);

    assert_eq!(std::fs::read(&installed).unwrap(), data);
    assert!(
        !partial_path(&dir, &model).exists(),
        "the temp path survived a completed install"
    );
    assert_eq!(install.status(&model), InstallStatus::Verified);
    assert_eq!(install.deep_status(&model), InstallStatus::Verified);
    let _ = std::fs::remove_dir_all(&dir);
}

// ---------------------------------------------------------------------------
// BR-6 / AC-7 — a mismatched digest is discarded
// ---------------------------------------------------------------------------

#[test]
fn a_mismatched_digest_is_discarded_and_never_installed() {
    let dir = temp_dir("corrupt");
    let data = body();
    let model = entry(&data);
    // The right number of bytes, the wrong bytes: a size check would call this
    // a successful download.
    let corrupt: Vec<u8> = data.iter().map(|b| b ^ 0xFF).collect();

    let install = WeightsInstall::new(
        Arc::new(TestFetcher::new(Plan::Corrupt(corrupt))),
        dir.clone(),
        None,
    )
    .with_free_space(ample());

    let err = install
        .install(&model)
        .expect_err("the digest does not match");
    assert_eq!(err, InstallError::Corrupt);
    // The error text is actionable and content-free — no URL, no digest (BR-11).
    let rendered = err.to_string();
    assert!(rendered.contains("integrity check"), "message: {rendered}");
    assert!(
        !rendered.contains(&model.sha256),
        "message leaked the digest"
    );

    assert!(!installed_path(&dir, &model).exists());
    assert!(!partial_path(&dir, &model).exists());
    assert_eq!(install.status(&model), InstallStatus::Absent);
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn install_state_never_reports_verified_for_a_truncated_artifact() {
    let dir = temp_dir("truncated");
    let data = body();
    let model = entry(&data);
    let install = WeightsInstall::new(Arc::new(TestFetcher::new(Plan::Fail)), dir.clone(), None);

    // A truncated file sitting at the loadable path, as a killed `cp` or a full
    // disk would leave one. AC-7: never `verified`.
    std::fs::write(installed_path(&dir, &model), &data[..100]).unwrap();
    assert_eq!(install.status(&model), InstallStatus::Corrupt);
    assert_eq!(install.deep_status(&model), InstallStatus::Corrupt);

    // Full length, wrong bytes — the case a size check gets wrong.
    let same_size_wrong_bytes: Vec<u8> = data.iter().map(|b| b ^ 0x01).collect();
    std::fs::write(installed_path(&dir, &model), &same_size_wrong_bytes).unwrap();
    assert_eq!(install.status(&model), InstallStatus::Corrupt);
    assert_eq!(install.deep_status(&model), InstallStatus::Corrupt);
    let _ = std::fs::remove_dir_all(&dir);
}

// ---------------------------------------------------------------------------
// AC-2 — progress
// ---------------------------------------------------------------------------

#[tokio::test]
async fn progress_is_reported_for_download_start_progress_verify_and_install() {
    let dir = temp_dir("progress");
    let data = body();
    let model = entry(&data);

    let recorder = Arc::new(Recorder::default());
    let install = WeightsInstall::new(
        Arc::new(TestFetcher::new(Plan::Whole(data.clone()))),
        dir.clone(),
        None,
    )
    .with_free_space(ample())
    .with_progress(Arc::clone(&recorder) as Arc<dyn InstallProgress>);

    install.install(&model).expect("install succeeds");

    let steps = recorder.steps();
    assert_eq!(
        steps.first(),
        Some(&InstallStep::DownloadStarted {
            downloaded_bytes: 0,
            total_bytes: model.size_bytes,
        })
    );
    assert!(
        steps.iter().any(|step| matches!(
            step,
            InstallStep::Downloading { downloaded_bytes, .. } if *downloaded_bytes == model.size_bytes
        )),
        "no completed-transfer progress step: {steps:?}"
    );
    assert!(
        steps.contains(&InstallStep::Verifying {
            total_bytes: model.size_bytes
        }),
        "no verify step: {steps:?}"
    );
    assert_eq!(
        steps.last(),
        Some(&InstallStep::Installed {
            total_bytes: model.size_bytes,
        }),
        "the install step must come last: {steps:?}"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test]
async fn install_progress_reaches_clients_as_model_lifecycle_events() {
    let dir = temp_dir("lifecycle");
    let data = body();
    let model = entry(&data);
    let bus = Arc::new(EventBus::new());
    let mut sub = bus.subscribe(64);

    let install = WeightsInstall::new(
        Arc::new(TestFetcher::new(Plan::Whole(data.clone()))),
        dir.clone(),
        None,
    )
    .with_free_space(ample())
    .with_progress(Arc::new(LifecycleProgress::new(Arc::clone(&bus))));

    install.install(&model).expect("install succeeds");

    let events = drain(&mut sub).await;
    let stages = download_stages(&events);
    assert_eq!(
        stages.first(),
        Some(&(0, Some(model.size_bytes))),
        "no download-start event: {stages:?}"
    );
    assert_eq!(
        stages.last(),
        Some(&(model.size_bytes, Some(model.size_bytes))),
        "the last progress event does not report a complete transfer: {stages:?}"
    );
    // Every event names the model it is about, and none carries a path (BR-11).
    for event in &events {
        if let Event::ModelLifecycle(lifecycle) = event {
            assert_eq!(lifecycle.model_id, model.name);
        }
        let wire = serde_json::to_string(event).unwrap();
        assert!(
            !wire.contains(&dir.to_string_lossy().to_string()),
            "an event carried the install path: {wire}"
        );
    }
    let _ = std::fs::remove_dir_all(&dir);
}

// ---------------------------------------------------------------------------
// AC-12 / BR-16 — the mirror and the true cause
// ---------------------------------------------------------------------------

#[test]
fn a_configured_base_url_redirects_the_fetch_to_the_mirror() {
    let dir = temp_dir("mirror");
    let data = body();
    let model = entry(&data);

    let fetcher = Arc::new(TestFetcher::new(Plan::Whole(data.clone())));
    let install = WeightsInstall::new(
        Arc::clone(&fetcher) as Arc<dyn RangeFetcher + Send + Sync>,
        dir.clone(),
        Some("https://hf-mirror.test.invalid/".to_owned()),
    )
    .with_free_space(ample());

    install.install(&model).expect("install succeeds");

    // BR-16 is only real if the override reaches the *fetch*. This is the
    // assertion that makes it so: the URL that hit the transport is the mirror's.
    let calls = fetcher.calls();
    assert!(!calls.is_empty(), "nothing was fetched");
    for call in &calls {
        assert!(
            call.url
                .starts_with("https://hf-mirror.test.invalid/Org/Repo/resolve/"),
            "fetched the catalog host instead of the mirror: {}",
            call.url
        );
        // The pinned revision and file survive the rewrite, so the catalog's
        // sha256 still describes what was fetched (BR-15).
        assert!(
            call.url.contains(REVISION),
            "the revision pin was lost: {}",
            call.url
        );
        assert!(
            call.url.ends_with("small-fit.gguf"),
            "the file path was lost: {}",
            call.url
        );
    }
    assert_eq!(std::fs::read(installed_path(&dir, &model)).unwrap(), data);
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn with_no_override_the_catalog_url_is_fetched_unchanged() {
    let dir = temp_dir("no-mirror");
    let data = body();
    let model = entry(&data);

    let fetcher = Arc::new(TestFetcher::new(Plan::Whole(data.clone())));
    WeightsInstall::new(
        Arc::clone(&fetcher) as Arc<dyn RangeFetcher + Send + Sync>,
        dir.clone(),
        None,
    )
    .with_free_space(ample())
    .install(&model)
    .expect("install succeeds");

    assert_eq!(fetcher.calls()[0].url, model.url);
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn a_rate_limited_host_is_reported_as_rate_limiting_not_as_corruption() {
    let dir = temp_dir("429");
    let data = body();
    let model = entry(&data);

    let install = WeightsInstall::new(Arc::new(TestFetcher::new(Plan::Fail)), dir.clone(), None)
        .with_free_space(ample())
        .with_cause(Arc::new(FixedCause(Some(FetchError::RateLimited {
            status: 429,
            attempts: 4,
        }))));

    let err = install
        .install(&model)
        .expect_err("the host refused to serve");
    match &err {
        InstallError::RateLimited { detail } => {
            assert!(detail.contains("429"), "detail: {detail}");
        }
        other => panic!("a 429 must not read as {other:?}"),
    }
    // AC-12's actual requirement: distinct from corruption, and actionable.
    assert_ne!(err, InstallError::Corrupt);
    let rendered = err.to_string();
    assert!(rendered.contains("rate-limiting"), "message: {rendered}");
    assert!(!installed_path(&dir, &model).exists());
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn an_offline_host_is_a_network_failure_carrying_the_precise_class() {
    let dir = temp_dir("offline");
    let data = body();
    let model = entry(&data);

    let install = WeightsInstall::new(Arc::new(TestFetcher::new(Plan::Fail)), dir.clone(), None)
        .with_free_space(ample())
        .with_cause(Arc::new(FixedCause(Some(FetchError::Network {
            class: "connect",
        }))));

    match install.install(&model).expect_err("offline") {
        InstallError::Network { detail } => {
            // The library's coarse error said only "transport"; the typed cause
            // says which transport failure it was.
            assert!(detail.contains("connect"), "detail: {detail}");
        }
        other => panic!("expected Network, got {other:?}"),
    }
    let _ = std::fs::remove_dir_all(&dir);
}
