//! The weights install pipeline: preflight → download → verify → atomic rename
//! (REQ-547 BR-6, BR-7, BR-9, BR-16).
//!
//! [`crate::model_consent`] decides *whether* to fetch. This module is the
//! *how*, and it is written around two promises a caller is entitled to make
//! without looking inside:
//!
//! 1. **Nothing is fetched when the disk is short.** The free-space check runs
//!    before the transport is touched at all, so "refuses before any bytes are
//!    fetched" (AC-6) is a property of the control flow — a recording
//!    [`RangeFetcher`] behind a refused install records zero calls.
//! 2. **The final path only ever holds verified bytes.** The download lands on a
//!    `.part` path and is renamed into place only after the library's SHA-256
//!    check passed (BR-9). `rename(2)` within one directory is atomic, so an
//!    interrupt — a dropped connection, a killed daemon, a panic — leaves the
//!    partial file and *nothing* at the final path. There is no window in which
//!    a half-written artifact is loadable.
//!
//! ## Why the preflight margin is not defined here
//!
//! It is [`crate::model_consent::DISK_WORKING_MARGIN_BYTES`], consumed through
//! [`required_disk_bytes`]. The proposal quotes that figure to the user *before*
//! they answer; this preflight enforces it *after*. Two constants would let the
//! daemon advertise one requirement and enforce another, which is the failure
//! mode the shared constant exists to make impossible.
//!
//! A resumed download subtracts the bytes already on disk from that requirement:
//! refusing to finish a nearly-complete 18 GiB transfer because the *total*
//! no longer fits would be a bug, not a safeguard.
//!
//! ## Why the install state is not a size check
//!
//! `size == catalog size` says a file of the right length is present, not that
//! it is the catalog's artifact. So a successful install writes a **receipt**
//! beside the weights recording the digest that was verified plus the size and
//! mtime of the file that was verified. [`WeightsInstall::status`] reports
//! `verified` only when the receipt still describes the file on disk; when it
//! does not — no receipt, an older build, or something replaced the file — it
//! falls back to re-digesting rather than guessing. The expensive answer is
//! reached only on suspicion, and never skipped in favour of a hopeful one.
//!
//! ## Why the precise failure cause comes from the fetcher
//!
//! [`teton_inference::download::DownloadError`] is deliberately coarse: it has
//! one variant per *orchestration* behaviour, not one per cause. A 429 and a
//! dead host both arrive as `Transport`. [`crate::download::HttpRangeFetcher`]
//! retains the typed cause, so this pipeline reads it back through [`FetchCause`]
//! and reports rate-limiting as rate-limiting (AC-12) instead of flattening it
//! into a generic transport failure — or, worse, into corruption.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use serde::{Deserialize, Serialize};

use teton_inference::catalog::ModelEntry;
use teton_inference::download::{DownloadError, Downloader, RangeFetcher};
use teton_inference::hash::sha256_file;
use teton_inference::lifecycle::LifecycleEvent;

use teton_protocol::events::{Event, ModelLifecycle, ModelLifecycleStage};
use teton_protocol::methods::InstallStatus;

use crate::broadcast::EventBus;
use crate::download::{FetchError, HttpRangeFetcher};
use crate::model_consent::{required_disk_bytes, InstallError, WeightsInstaller};

/// Bytes between download-progress reports.
///
/// The library reports every chunk, which for an 18 GiB artifact is millions of
/// callbacks. The event bus evicts slow subscribers, so an unthrottled stream
/// would not inform a client — it would disconnect one. A report every 32 MiB is
/// a visible tick on any transfer worth showing progress for; the first and last
/// reports are always emitted regardless of stride.
pub const PROGRESS_STRIDE_BYTES: u64 = 32 * 1024 * 1024;

/// Extension of the in-progress download beside the installed weights.
const PARTIAL_SUFFIX: &str = "gguf.part";

/// Extension of the verification receipt beside the installed weights.
const RECEIPT_SUFFIX: &str = "gguf.verified";

// ---------------------------------------------------------------------------
// Seams
// ---------------------------------------------------------------------------

/// Measures free space on the volume holding a path (BR-7).
///
/// A seam rather than a direct syscall because AC-6's claim is about behaviour
/// on a *full* disk, and a test that can only run on a genuinely full volume is
/// a test that never runs.
pub trait FreeSpace: Send + Sync {
    /// Bytes available under `path`, or `None` when the platform could not
    /// answer.
    ///
    /// `None` means "unknown", never "zero": an unanswerable `statvfs` must not
    /// become a refusal to install.
    fn available_bytes(&self, path: &Path) -> Option<u64>;
}

/// The host filesystem, via `statvfs(3)`.
#[derive(Debug, Default, Clone, Copy)]
pub struct HostFreeSpace;

impl FreeSpace for HostFreeSpace {
    fn available_bytes(&self, path: &Path) -> Option<u64> {
        use std::os::unix::ffi::OsStrExt;

        let c_path = std::ffi::CString::new(path.as_os_str().as_bytes()).ok()?;
        let mut stat: libc::statvfs = unsafe { std::mem::zeroed() };
        // SAFETY: `c_path` is a NUL-terminated C string that outlives the call,
        // and `stat` is a live, correctly-typed, zero-initialized out-parameter.
        let rc = unsafe { libc::statvfs(c_path.as_ptr(), &mut stat) };
        if rc != 0 {
            return None;
        }
        // `f_bavail` is the blocks available to an unprivileged writer, which is
        // the number that governs whether *this daemon* can write — `f_bfree`
        // would include the root-reserved slack we cannot use.
        let frsize = widen(stat.f_frsize)?;
        let block = if frsize > 0 {
            frsize
        } else {
            widen(stat.f_bsize)?
        };
        Some(widen(stat.f_bavail)?.saturating_mul(block))
    }
}

/// Widen a platform-sized `statvfs` field to `u64`.
///
/// The field widths differ by platform — `fsblkcnt_t` is 32-bit on macOS and
/// 64-bit on Linux — so neither a cast nor a `From` conversion reads correctly
/// on both: one is lossy on one platform, redundant on the other, and a lint
/// error on whichever it is redundant on. A generic fallible conversion is
/// exactly right on each, and `None` (rather than a guessed number) keeps a
/// value that will not fit inside this module's "unknown is not zero" contract.
fn widen<T: TryInto<u64>>(value: T) -> Option<u64> {
    value.try_into().ok()
}

/// A fixed free-space answer, for tests and for a caller that already knows.
#[derive(Debug, Clone, Copy)]
pub struct FixedFreeSpace(pub Option<u64>);

impl FreeSpace for FixedFreeSpace {
    fn available_bytes(&self, _path: &Path) -> Option<u64> {
        self.0
    }
}

/// A transport that retains the precise, typed cause of its last failure.
///
/// The [`RangeFetcher`] seam can only hand back a coarse [`DownloadError`]; this
/// is how the pipeline recovers what actually happened (AC-12).
pub trait FetchCause: Send + Sync {
    /// The classified cause of the most recent failed fetch, if the last fetch
    /// failed.
    fn last_cause(&self) -> Option<FetchError>;
}

impl FetchCause for HttpRangeFetcher {
    fn last_cause(&self) -> Option<FetchError> {
        self.last_error()
    }
}

// ---------------------------------------------------------------------------
// Progress
// ---------------------------------------------------------------------------

/// A step in the install pipeline, reported as it happens (AC-2).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum InstallStep {
    /// Preflight passed and the transfer is starting. `downloaded_bytes` is
    /// non-zero when a previous run's `.part` file is being resumed.
    DownloadStarted {
        /// Bytes already on disk from an earlier attempt.
        downloaded_bytes: u64,
        /// Bytes the finished artifact will hold.
        total_bytes: u64,
    },
    /// Bytes durably written so far. Throttled to [`PROGRESS_STRIDE_BYTES`].
    Downloading {
        /// Bytes durably written.
        downloaded_bytes: u64,
        /// Bytes the finished artifact will hold.
        total_bytes: u64,
    },
    /// Every byte is on disk and the SHA-256 check is running (BR-6). Emitted
    /// again per retry when a mismatch forces a clean re-fetch.
    Verifying {
        /// Bytes being hashed.
        total_bytes: u64,
    },
    /// The verified artifact was renamed into place (BR-9). Only ever emitted
    /// after a digest match.
    Installed {
        /// Bytes now installed.
        total_bytes: u64,
    },
}

/// Receives [`InstallStep`]s from a running install.
pub trait InstallProgress: Send + Sync {
    /// Report one step of `model_name`'s install.
    fn report(&self, model_name: &str, step: &InstallStep);
}

/// Discards every step. The default for an installer built without one.
#[derive(Debug, Default, Clone, Copy)]
pub struct SilentProgress;

impl InstallProgress for SilentProgress {
    fn report(&self, _model_name: &str, _step: &InstallStep) {}
}

/// Publishes install progress as `model_lifecycle` events (AC-2).
///
/// The wire vocabulary ([`ModelLifecycleStage`]) has a `download` stage and no
/// `verifying`/`installing` stage, so those two steps are projected onto the
/// transfer's completion tick rather than inventing a stage the clients of this
/// milestone cannot render. The install's *completion* already has a wire
/// signal: the consent gate publishes `ready` once the installer returns, and
/// [`InstallStep::Installed`] would only duplicate it.
pub struct LifecycleProgress {
    events: Arc<EventBus>,
}

impl LifecycleProgress {
    /// Publish onto `events`.
    #[must_use]
    pub fn new(events: Arc<EventBus>) -> Self {
        Self { events }
    }
}

impl InstallProgress for LifecycleProgress {
    fn report(&self, model_name: &str, step: &InstallStep) {
        let stage = match step {
            InstallStep::DownloadStarted {
                downloaded_bytes,
                total_bytes,
            }
            | InstallStep::Downloading {
                downloaded_bytes,
                total_bytes,
            } => ModelLifecycleStage::Download {
                downloaded_bytes: *downloaded_bytes,
                total_bytes: Some(*total_bytes),
            },
            InstallStep::Verifying { total_bytes } => ModelLifecycleStage::Download {
                downloaded_bytes: *total_bytes,
                total_bytes: Some(*total_bytes),
            },
            // The gate's `ready` is the install-complete signal; a second event
            // here would say the same thing twice.
            InstallStep::Installed { .. } => return,
        };
        self.events.publish(
            None,
            Event::ModelLifecycle(ModelLifecycle {
                model_id: model_name.to_owned(),
                stage,
            }),
        );
    }
}

// ---------------------------------------------------------------------------
// The pipeline
// ---------------------------------------------------------------------------

/// The production [`WeightsInstaller`]: disk preflight, resumable verified
/// download, atomic install, and receipt-backed state reporting.
pub struct WeightsInstall {
    fetcher: Arc<dyn RangeFetcher + Send + Sync>,
    cause: Option<Arc<dyn FetchCause>>,
    weights_dir: PathBuf,
    base_url: Option<String>,
    free_space: Arc<dyn FreeSpace>,
    progress: Arc<dyn InstallProgress>,
    progress_stride: u64,
}

impl WeightsInstall {
    /// An installer fetching through `fetcher` into `weights_dir`, applying the
    /// configured catalog base-URL override (BR-16) to every entry.
    ///
    /// Defaults to the host's free-space measurement and no progress reporting;
    /// [`Self::with_progress`] and [`Self::with_cause`] add what the daemon
    /// wires in.
    #[must_use]
    pub fn new(
        fetcher: Arc<dyn RangeFetcher + Send + Sync>,
        weights_dir: PathBuf,
        base_url: Option<String>,
    ) -> Self {
        Self {
            fetcher,
            cause: None,
            weights_dir,
            base_url,
            free_space: Arc::new(HostFreeSpace),
            progress: Arc::new(SilentProgress),
            progress_stride: PROGRESS_STRIDE_BYTES,
        }
    }

    /// Measure free space through `free_space` instead of the host filesystem.
    #[must_use]
    pub fn with_free_space(mut self, free_space: Arc<dyn FreeSpace>) -> Self {
        self.free_space = free_space;
        self
    }

    /// Report every [`InstallStep`] to `progress`.
    #[must_use]
    pub fn with_progress(mut self, progress: Arc<dyn InstallProgress>) -> Self {
        self.progress = progress;
        self
    }

    /// Report download progress every `bytes` rather than every
    /// [`PROGRESS_STRIDE_BYTES`].
    ///
    /// Exists so the throttle can be exercised against a fixture-sized artifact:
    /// a test that had to move 32 MiB to see a second progress tick would be a
    /// test nobody runs.
    #[must_use]
    pub fn with_progress_stride(mut self, bytes: u64) -> Self {
        self.progress_stride = bytes.max(1);
        self
    }

    /// Read the precise failure cause back from `cause` when a download fails
    /// (AC-12). Without it, failures are classified from the library's coarse
    /// error alone.
    #[must_use]
    pub fn with_cause(mut self, cause: Arc<dyn FetchCause>) -> Self {
        self.cause = Some(cause);
        self
    }

    /// The installed weights path for `entry`. Local display only — this never
    /// crosses the protocol boundary (BR-11).
    #[must_use]
    pub fn installed_path(&self, entry: &ModelEntry) -> PathBuf {
        self.weights_dir.join(format!("{}.gguf", entry.name))
    }

    /// The in-progress download path for `entry` (resumable, never loadable).
    #[must_use]
    pub fn partial_path(&self, entry: &ModelEntry) -> PathBuf {
        self.weights_dir
            .join(format!("{}.{PARTIAL_SUFFIX}", entry.name))
    }

    /// The verification receipt path for `entry`.
    fn receipt_path(&self, entry: &ModelEntry) -> PathBuf {
        self.weights_dir
            .join(format!("{}.{RECEIPT_SUFFIX}", entry.name))
    }

    /// The install state of `entry`, always re-digesting the installed file
    /// rather than trusting its receipt.
    ///
    /// [`WeightsInstaller::status`] is the cheap read the daemon performs on
    /// every start; this is the expensive one, for a caller that wants the
    /// question settled from the bytes themselves — the receipt's one blind
    /// spot is a same-size replacement that also preserved the mtime.
    #[must_use]
    pub fn deep_status(&self, entry: &ModelEntry) -> InstallStatus {
        deep_status_at(
            &self.installed_path(entry),
            &self.partial_path(entry),
            entry,
        )
    }

    /// Free disk this install still needs: the advertised requirement less the
    /// bytes a previous attempt already wrote (BR-7).
    fn shortfall_bytes(&self, entry: &ModelEntry) -> u64 {
        required_disk_bytes(entry).saturating_sub(current_len(&self.partial_path(entry)))
    }

    /// Refuse the install when the volume cannot hold it (BR-7 / AC-6).
    ///
    /// Called before the fetcher is touched. An unknown free-space figure is not
    /// a refusal: a `statvfs` this platform will not answer says nothing about
    /// whether the artifact fits, and failing closed on it would make an
    /// unmeasurable volume an uninstallable one.
    fn preflight(&self, entry: &ModelEntry) -> Result<(), InstallError> {
        let required_bytes = self.shortfall_bytes(entry);
        let Some(available_bytes) = self.free_space.available_bytes(&self.weights_dir) else {
            return Ok(());
        };
        if available_bytes < required_bytes {
            return Err(InstallError::InsufficientDisk {
                required_bytes,
                available_bytes,
            });
        }
        Ok(())
    }

    /// Classify a failed download, preferring the transport's typed cause.
    ///
    /// A digest failure is decided first and unconditionally: it is a statement
    /// about the *bytes*, and the last transport error (if any) is stale by then
    /// — a resumed download that hiccuped once and then delivered corrupt bytes
    /// is corrupt, not flaky.
    fn classify(&self, error: &DownloadError) -> InstallError {
        if matches!(
            error,
            DownloadError::Checksum { .. } | DownloadError::Oversized { .. }
        ) {
            return InstallError::Corrupt;
        }
        match self.cause.as_ref().and_then(|cause| cause.last_cause()) {
            Some(cause) if cause.is_rate_limited() => InstallError::RateLimited {
                detail: cause.to_string(),
            },
            Some(cause) => InstallError::Network {
                detail: cause.to_string(),
            },
            None => classify_download_error(error),
        }
    }

    /// Run the transfer, reporting throttled progress and the verify transition.
    fn transfer(&self, target: &ModelEntry, partial: &Path) -> Result<(), InstallError> {
        let total = target.size_bytes;
        let mut last_reported = current_len(partial);
        let mut verifying_reported = false;

        let result = Downloader::new(&*self.fetcher).fetch(target, partial, &mut |event| {
            let LifecycleEvent::Download {
                downloaded_bytes, ..
            } = event
            else {
                return;
            };
            if downloaded_bytes < last_reported {
                // A digest mismatch discarded the file and the library is
                // re-fetching from scratch. Without this the stride would hold
                // progress silent until it climbed back past the old high-water
                // mark — the transfer would look hung exactly when it is redoing
                // the most work.
                last_reported = 0;
            }
            let complete = downloaded_bytes >= total;
            if !complete && downloaded_bytes.saturating_sub(last_reported) < self.progress_stride {
                return;
            }
            last_reported = downloaded_bytes;
            self.progress.report(
                &target.name,
                &InstallStep::Downloading {
                    downloaded_bytes,
                    total_bytes: total,
                },
            );
            if complete {
                // The library hashes the moment the file reaches its expected
                // size, so this is the verify transition, not a guess at it.
                verifying_reported = true;
                self.progress
                    .report(&target.name, &InstallStep::Verifying { total_bytes: total });
            }
        });

        match result {
            Ok(()) => {
                // A `.part` that was already complete on arrival goes straight to
                // hashing without a single progress callback.
                if !verifying_reported {
                    self.progress
                        .report(&target.name, &InstallStep::Verifying { total_bytes: total });
                }
                Ok(())
            }
            Err(err) => Err(self.classify(&err)),
        }
    }
}

impl WeightsInstaller for WeightsInstall {
    fn install(&self, entry: &ModelEntry) -> Result<(), InstallError> {
        std::fs::create_dir_all(&self.weights_dir).map_err(|err| InstallError::Io {
            detail: err.kind().to_string(),
        })?;

        // Already installed and still attested: nothing to fetch, and no reason
        // to hold the install to a disk requirement it has already met.
        if self.status(entry) == InstallStatus::Verified {
            self.progress.report(
                &entry.name,
                &InstallStep::Installed {
                    total_bytes: entry.size_bytes,
                },
            );
            return Ok(());
        }

        // BR-7 / AC-6: before the fetcher exists in this call's world.
        self.preflight(entry)?;

        let partial = self.partial_path(entry);
        self.progress.report(
            &entry.name,
            &InstallStep::DownloadStarted {
                downloaded_bytes: current_len(&partial),
                total_bytes: entry.size_bytes,
            },
        );

        // BR-16: fetch the mirrored URL when a base override is configured. The
        // repo/revision/file path — and therefore the pinned digest's meaning —
        // is preserved by `download_url`.
        let mut target = entry.clone();
        target.url = entry.download_url(self.base_url.as_deref());

        self.transfer(&target, &partial)?;

        // BR-9: the file reaches its final name only after the library verified
        // its SHA-256, and `rename` within one directory is atomic — there is no
        // instant at which a partial artifact is visible at the loadable path.
        let installed = self.installed_path(entry);
        std::fs::rename(&partial, &installed).map_err(|err| InstallError::Io {
            detail: err.kind().to_string(),
        })?;
        write_receipt(&self.receipt_path(entry), &installed, entry);
        self.progress.report(
            &entry.name,
            &InstallStep::Installed {
                total_bytes: entry.size_bytes,
            },
        );
        Ok(())
    }

    fn status(&self, entry: &ModelEntry) -> InstallStatus {
        install_status_at(
            &self.installed_path(entry),
            &self.partial_path(entry),
            &self.receipt_path(entry),
            entry,
        )
    }
}

// ---------------------------------------------------------------------------
// Install state (spec entity `InstallState`)
// ---------------------------------------------------------------------------

/// What a successful install attests about the file it left behind.
///
/// Size and mtime together answer "is this still the file we verified?" without
/// re-reading gigabytes; the digest answers "verified against *what*?", so a
/// catalog whose entry changed underneath an installed file is caught rather
/// than inherited.
#[derive(Debug, Clone, Serialize, Deserialize)]
struct VerificationReceipt {
    /// The digest that was verified — compared against the catalog's.
    sha256: String,
    /// Size of the file at the moment it was verified.
    size_bytes: u64,
    /// Modification time of the file at the moment it was verified, in
    /// nanoseconds since the epoch. `None` when the platform would not report
    /// one.
    ///
    /// Nanoseconds rather than seconds because the check has to notice a
    /// replacement that happened moments after the install, not merely one that
    /// happened on a later day.
    modified_ns: Option<u64>,
}

/// The install state implied by what is on disk (absent/partial/verified/corrupt).
///
/// `verified` is never reported on a hopeful signal. A wrong-sized file is
/// `corrupt` outright; a right-sized one is `verified` only when a receipt still
/// describes it, and otherwise is re-digested — the honest answer, paid for only
/// when the cheap attestation is missing or stale.
fn install_status_at(
    installed: &Path,
    partial: &Path,
    receipt: &Path,
    entry: &ModelEntry,
) -> InstallStatus {
    let Ok(meta) = std::fs::metadata(installed) else {
        return if partial.exists() {
            InstallStatus::Partial
        } else {
            InstallStatus::Absent
        };
    };
    if meta.len() != entry.size_bytes {
        // Truncated, or something outside the daemon replaced it. Either way the
        // bytes are not the catalog's bytes (AC-7).
        return InstallStatus::Corrupt;
    }
    if receipt_describes(receipt, &meta, entry) {
        return InstallStatus::Verified;
    }
    let status = deep_status_at(installed, partial, entry);
    if status == InstallStatus::Verified {
        // Attest what was just proven, so the next read is cheap again.
        write_receipt(receipt, installed, entry);
    }
    status
}

/// The install state read from the bytes themselves, with no receipt involved.
fn deep_status_at(installed: &Path, partial: &Path, entry: &ModelEntry) -> InstallStatus {
    let Ok(meta) = std::fs::metadata(installed) else {
        return if partial.exists() {
            InstallStatus::Partial
        } else {
            InstallStatus::Absent
        };
    };
    if meta.len() != entry.size_bytes {
        return InstallStatus::Corrupt;
    }
    match sha256_file(installed) {
        Ok(digest) if digest == entry.sha256 => InstallStatus::Verified,
        _ => InstallStatus::Corrupt,
    }
}

/// Whether `receipt` still describes the file `meta` came from.
fn receipt_describes(receipt: &Path, meta: &std::fs::Metadata, entry: &ModelEntry) -> bool {
    let Ok(text) = std::fs::read_to_string(receipt) else {
        return false;
    };
    let Ok(receipt) = serde_json::from_str::<VerificationReceipt>(&text) else {
        return false;
    };
    receipt.sha256 == entry.sha256
        && receipt.size_bytes == meta.len()
        && receipt.modified_ns == modified_ns(meta)
}

/// Write the receipt for a just-verified `installed` file. Best-effort: a
/// receipt that cannot be written costs a re-digest on the next read, never
/// correctness.
fn write_receipt(receipt: &Path, installed: &Path, entry: &ModelEntry) {
    let Ok(meta) = std::fs::metadata(installed) else {
        return;
    };
    let record = VerificationReceipt {
        sha256: entry.sha256.clone(),
        size_bytes: meta.len(),
        modified_ns: modified_ns(&meta),
    };
    if let Ok(text) = serde_json::to_string(&record) {
        let _ = std::fs::write(receipt, text);
    }
}

/// `meta`'s modification time in nanoseconds since the epoch, when reportable.
fn modified_ns(meta: &std::fs::Metadata) -> Option<u64> {
    meta.modified()
        .ok()?
        .duration_since(std::time::UNIX_EPOCH)
        .ok()
        .map(|since| u64::try_from(since.as_nanos()).unwrap_or(u64::MAX))
}

/// Current length of `path`, or `0` when it does not exist.
fn current_len(path: &Path) -> u64 {
    std::fs::metadata(path).map(|meta| meta.len()).unwrap_or(0)
}

/// Map the library's coarse [`DownloadError`] onto the user-facing
/// classification, with no typed transport cause available.
///
/// `Checksum` is the one failure that means "the bytes were wrong" (BR-6); every
/// transport-shaped failure is a network problem the user can retry.
fn classify_download_error(error: &DownloadError) -> InstallError {
    match error {
        DownloadError::Checksum { .. } | DownloadError::Oversized { .. } => InstallError::Corrupt,
        DownloadError::Transport(detail) => InstallError::Network {
            detail: detail.clone(),
        },
        DownloadError::Stalled { .. } => InstallError::Network {
            detail: "the transfer stopped making progress".to_owned(),
        },
        // The fetcher reports a permanent HTTP failure as `Io` carrying its own
        // classified message (TASK-002 D-2); a genuine filesystem failure lands
        // here too. Both are reported with the same content-free text.
        DownloadError::Io(err) => InstallError::Network {
            detail: err.to_string(),
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;
    use teton_inference::catalog::TierBand;
    use teton_inference::hash::sha256_hex;

    const REVISION: &str = "0123456789abcdef0123456789abcdef01234567";
    /// A stand-in artifact and its real SHA-256, so every verification in this
    /// module runs the library's genuine digest path rather than a stub.
    const BODY: &[u8] = b"weights";
    const BODY_SHA: &str = "9a129038d9a00aed0cf6a7ea059ca50a813449061ab87848cf1a13eafdf33b2c";

    fn entry() -> ModelEntry {
        ModelEntry {
            name: "m".to_owned(),
            url: format!("https://models.test.invalid/Org/Repo/resolve/{REVISION}/m.gguf"),
            revision: REVISION.to_owned(),
            sha256: BODY_SHA.to_owned(),
            size_bytes: BODY.len() as u64,
            ram_floor_bytes: 0,
            band: TierBand::Small,
        }
    }

    fn temp_dir(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "teton-install-{tag}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    /// A progress sink that keeps every step it is handed.
    #[derive(Default)]
    struct Recorder(Mutex<Vec<InstallStep>>);

    impl InstallProgress for Recorder {
        fn report(&self, _model_name: &str, step: &InstallStep) {
            self.0.lock().unwrap().push(step.clone());
        }
    }

    #[test]
    fn the_shortfall_is_the_shared_requirement_less_the_resumable_bytes() {
        let dir = temp_dir("shortfall");
        let entry = entry();
        let install = WeightsInstall::new(Arc::new(NoFetcher), dir.clone(), None);
        assert_eq!(install.shortfall_bytes(&entry), required_disk_bytes(&entry));

        std::fs::write(install.partial_path(&entry), b"we").unwrap();
        assert_eq!(
            install.shortfall_bytes(&entry),
            required_disk_bytes(&entry) - 2
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn preflight_refuses_naming_required_and_available() {
        let dir = temp_dir("preflight");
        let entry = entry();
        let install = WeightsInstall::new(Arc::new(NoFetcher), dir.clone(), None)
            .with_free_space(Arc::new(FixedFreeSpace(Some(4096))));

        let err = install.preflight(&entry).unwrap_err();
        match &err {
            InstallError::InsufficientDisk {
                required_bytes,
                available_bytes,
            } => {
                assert_eq!(*required_bytes, required_disk_bytes(&entry));
                assert_eq!(*available_bytes, 4096);
            }
            other => panic!("expected InsufficientDisk, got {other:?}"),
        }
        // AC-6: the message names both figures.
        let rendered = err.to_string();
        assert!(rendered.contains("1.0 GiB"), "message: {rendered}");
        assert!(rendered.contains("0.0 GiB"), "message: {rendered}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn an_unmeasurable_volume_does_not_refuse() {
        let dir = temp_dir("unmeasurable");
        let install = WeightsInstall::new(Arc::new(NoFetcher), dir.clone(), None)
            .with_free_space(Arc::new(FixedFreeSpace(None)));
        assert!(install.preflight(&entry()).is_ok());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn the_host_reports_free_space_for_a_real_directory() {
        // Not an assertion about *how much* — only that the syscall path answers
        // for a path that exists, which is what the daemon depends on.
        let dir = temp_dir("statvfs");
        assert!(HostFreeSpace.available_bytes(&dir).is_some());
        assert!(HostFreeSpace
            .available_bytes(Path::new("/definitely/not/here"))
            .is_none());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn install_state_is_absent_partial_verified_or_corrupt() {
        let dir = temp_dir("status");
        let body = BODY.to_vec();
        let model = entry();

        let installed = dir.join("m.gguf");
        let partial = dir.join("m.gguf.part");
        let receipt = dir.join("m.gguf.verified");

        assert_eq!(
            install_status_at(&installed, &partial, &receipt, &model),
            InstallStatus::Absent
        );
        std::fs::write(&partial, b"we").unwrap();
        assert_eq!(
            install_status_at(&installed, &partial, &receipt, &model),
            InstallStatus::Partial
        );
        // Right name, wrong length: never `verified` (AC-7).
        std::fs::write(&installed, b"we").unwrap();
        assert_eq!(
            install_status_at(&installed, &partial, &receipt, &model),
            InstallStatus::Corrupt
        );
        // Right length and right bytes, with no receipt: re-digested, verified,
        // and attested for next time.
        std::fs::write(&installed, &body).unwrap();
        assert_eq!(
            install_status_at(&installed, &partial, &receipt, &model),
            InstallStatus::Verified
        );
        assert!(receipt.exists(), "a verified read writes its receipt");
        // Same length, different bytes, receipt now stale: the re-digest catches
        // what a size check never would. The pause is so the replacement lands
        // on a different mtime than the receipt recorded — without it the test
        // would be asserting nanosecond clock resolution, not the check.
        std::thread::sleep(std::time::Duration::from_millis(20));
        std::fs::write(&installed, b"WEIGHTS").unwrap();
        assert_eq!(
            install_status_at(&installed, &partial, &receipt, &model),
            InstallStatus::Corrupt
        );
        // And the receipt-free read agrees, from the bytes alone.
        assert_eq!(
            deep_status_at(&installed, &partial, &model),
            InstallStatus::Corrupt
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_receipt_for_a_different_catalog_digest_is_not_trusted() {
        let dir = temp_dir("receipt-drift");
        let body = BODY.to_vec();
        let model = entry();

        let installed = dir.join("m.gguf");
        let partial = dir.join("m.gguf.part");
        let receipt = dir.join("m.gguf.verified");
        std::fs::write(&installed, &body).unwrap();
        write_receipt(&receipt, &installed, &model);
        assert_eq!(
            install_status_at(&installed, &partial, &receipt, &model),
            InstallStatus::Verified
        );

        // The catalog now pins a different digest for the same size: the old
        // receipt must not carry the old file into the new entry's identity.
        let mut moved = model.clone();
        moved.sha256 = "f".repeat(64);
        assert_eq!(
            install_status_at(&installed, &partial, &receipt, &moved),
            InstallStatus::Corrupt
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_checksum_failure_is_corrupt_and_a_transport_failure_is_network() {
        assert_eq!(
            classify_download_error(&DownloadError::Checksum {
                expected: "a".to_owned(),
                actual: "b".to_owned(),
                attempts: 3,
            }),
            InstallError::Corrupt
        );
        assert_eq!(
            classify_download_error(&DownloadError::Oversized {
                expected: 1,
                actual: 2,
            }),
            InstallError::Corrupt
        );
        assert!(matches!(
            classify_download_error(&DownloadError::Transport("offline".to_owned())),
            InstallError::Network { .. }
        ));
        assert!(matches!(
            classify_download_error(&DownloadError::Stalled { attempts: 6 }),
            InstallError::Network { .. }
        ));
    }

    #[test]
    fn a_rate_limited_host_is_never_reported_as_corruption() {
        let dir = temp_dir("ratelimit");
        let install = WeightsInstall::new(Arc::new(NoFetcher), dir.clone(), None).with_cause(
            Arc::new(FixedCause(Some(FetchError::RateLimited {
                status: 429,
                attempts: 4,
            }))),
        );

        // AC-12: the coarse library error says "transport"; the typed cause says
        // which transport failure, and the two must not be collapsed.
        let classified = install.classify(&DownloadError::Transport("nope".to_owned()));
        match &classified {
            InstallError::RateLimited { detail } => {
                assert!(detail.contains("rate-limited"), "detail: {detail}");
            }
            other => panic!("expected RateLimited, got {other:?}"),
        }

        // A digest failure is decided on the bytes, whatever the transport last
        // complained about.
        assert_eq!(
            install.classify(&DownloadError::Checksum {
                expected: "a".to_owned(),
                actual: "b".to_owned(),
                attempts: 3,
            }),
            InstallError::Corrupt
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn an_unavailable_host_is_a_network_failure_with_the_precise_cause() {
        let dir = temp_dir("unavailable");
        let install = WeightsInstall::new(Arc::new(NoFetcher), dir.clone(), None).with_cause(
            Arc::new(FixedCause(Some(FetchError::Unavailable {
                status: 503,
                attempts: 4,
            }))),
        );
        match install.classify(&DownloadError::Transport("nope".to_owned())) {
            InstallError::Network { detail } => {
                assert!(detail.contains("unavailable"), "detail: {detail}");
                assert!(detail.contains("503"), "detail: {detail}");
            }
            other => panic!("expected Network, got {other:?}"),
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn lifecycle_progress_projects_steps_onto_download_stages() {
        let bus = Arc::new(EventBus::new());
        let mut sub = bus.subscribe(8);
        let progress = LifecycleProgress::new(Arc::clone(&bus));

        progress.report(
            "m",
            &InstallStep::DownloadStarted {
                downloaded_bytes: 0,
                total_bytes: 10,
            },
        );
        progress.report("m", &InstallStep::Verifying { total_bytes: 10 });
        // Deliberately silent: the gate's `ready` already says this.
        progress.report("m", &InstallStep::Installed { total_bytes: 10 });
        // A third, distinguishable event proves the `Installed` step published
        // nothing rather than merely publishing late.
        progress.report(
            "m",
            &InstallStep::Downloading {
                downloaded_bytes: 7,
                total_bytes: 10,
            },
        );

        let mut stages = Vec::new();
        for _ in 0..3 {
            let envelope = sub.recv().await.expect("event bus closed");
            if let Event::ModelLifecycle(lifecycle) = envelope.event {
                stages.push(lifecycle.stage);
            }
        }
        assert_eq!(
            stages,
            vec![
                ModelLifecycleStage::Download {
                    downloaded_bytes: 0,
                    total_bytes: Some(10),
                },
                ModelLifecycleStage::Download {
                    downloaded_bytes: 10,
                    total_bytes: Some(10),
                },
                ModelLifecycleStage::Download {
                    downloaded_bytes: 7,
                    total_bytes: Some(10),
                },
            ]
        );
    }

    #[test]
    fn a_recorder_sees_every_step_of_a_successful_install() {
        let dir = temp_dir("steps");
        let body = BODY.to_vec();
        let model = entry();

        let recorder = Arc::new(Recorder::default());
        let install = WeightsInstall::new(Arc::new(WholeFetcher(body.clone())), dir.clone(), None)
            .with_free_space(Arc::new(FixedFreeSpace(Some(u64::MAX))))
            .with_progress(Arc::clone(&recorder) as Arc<dyn InstallProgress>);

        install.install(&model).expect("install succeeds");

        let steps = recorder.0.lock().unwrap().clone();
        assert!(matches!(
            steps.first(),
            Some(InstallStep::DownloadStarted {
                downloaded_bytes: 0,
                ..
            })
        ));
        assert!(steps
            .iter()
            .any(|s| matches!(s, InstallStep::Downloading { .. })));
        assert!(steps
            .iter()
            .any(|s| matches!(s, InstallStep::Verifying { .. })));
        assert!(matches!(steps.last(), Some(InstallStep::Installed { .. })));
        // And the artifact is where a loader would look for it, attested.
        assert_eq!(install.status(&model), InstallStatus::Verified);
        assert_eq!(install.deep_status(&model), InstallStatus::Verified);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn an_already_installed_model_is_not_fetched_again() {
        let dir = temp_dir("idempotent");
        let model = entry();
        // `NoFetcher` panics if touched: the claim is that a verified install is
        // recognized without a single byte of network traffic.
        let install = WeightsInstall::new(Arc::new(NoFetcher), dir.clone(), None);
        std::fs::write(install.installed_path(&model), BODY).unwrap();

        install.install(&model).expect("already installed");
        assert_eq!(install.status(&model), InstallStatus::Verified);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn progress_is_throttled_to_the_stride_rather_than_reported_per_chunk() {
        let dir = temp_dir("throttle");
        let data: Vec<u8> = (0u8..=250).cycle().take(4096).collect();
        let mut model = entry();
        model.sha256 = sha256_hex(&data);
        model.size_bytes = data.len() as u64;

        let recorder = Arc::new(Recorder::default());
        WeightsInstall::new(
            Arc::new(ChunkedFetcher {
                data: data.clone(),
                chunk: 256,
            }),
            dir.clone(),
            None,
        )
        .with_free_space(Arc::new(FixedFreeSpace(Some(u64::MAX))))
        .with_progress(Arc::clone(&recorder) as Arc<dyn InstallProgress>)
        .with_progress_stride(1024)
        .install(&model)
        .expect("install succeeds");

        let reported: Vec<u64> = recorder
            .0
            .lock()
            .unwrap()
            .iter()
            .filter_map(|step| match step {
                InstallStep::Downloading {
                    downloaded_bytes, ..
                } => Some(*downloaded_bytes),
                _ => None,
            })
            .collect();
        // 16 chunks arrived; a stride of 1024 over 4096 bytes means four ticks.
        assert_eq!(reported, vec![1024, 2048, 3072, 4096], "{reported:?}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_discarded_attempt_restarts_progress_instead_of_going_silent() {
        let dir = temp_dir("retry-progress");
        let good: Vec<u8> = (0u8..=250).cycle().take(4096).collect();
        let corrupt: Vec<u8> = good.iter().map(|b| b ^ 0xFF).collect();
        let mut model = entry();
        model.sha256 = sha256_hex(&good);
        model.size_bytes = good.len() as u64;

        let recorder = Arc::new(Recorder::default());
        WeightsInstall::new(
            Arc::new(CorruptThenGoodFetcher {
                good: good.clone(),
                corrupt,
                served: Mutex::new(0),
                chunk: 256,
            }),
            dir.clone(),
            None,
        )
        .with_free_space(Arc::new(FixedFreeSpace(Some(u64::MAX))))
        .with_progress(Arc::clone(&recorder) as Arc<dyn InstallProgress>)
        .with_progress_stride(1024)
        .install(&model)
        .expect("the second attempt verifies");

        let steps = recorder.0.lock().unwrap().clone();
        // Both attempts reached the hashing step.
        assert_eq!(
            steps
                .iter()
                .filter(|step| matches!(step, InstallStep::Verifying { .. }))
                .count(),
            2,
            "{steps:?}"
        );
        // And the re-fetch reported from the start rather than waiting to climb
        // back past the discarded attempt's high-water mark.
        let ticks: Vec<u64> = steps
            .iter()
            .filter_map(|step| match step {
                InstallStep::Downloading {
                    downloaded_bytes, ..
                } => Some(*downloaded_bytes),
                _ => None,
            })
            .collect();
        assert_eq!(
            ticks,
            vec![1024, 2048, 3072, 4096, 1024, 2048, 3072, 4096],
            "{ticks:?}"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    // --- fetcher doubles -------------------------------------------------

    /// Fails every call. Used where the test's claim is that it is never called.
    struct NoFetcher;
    impl RangeFetcher for NoFetcher {
        fn fetch(
            &self,
            _url: &str,
            _offset: u64,
            _sink: &mut dyn FnMut(&[u8]) -> Result<(), DownloadError>,
        ) -> Result<u64, DownloadError> {
            panic!("the fetcher must not be called");
        }
    }

    /// Streams a whole body from the requested offset.
    struct WholeFetcher(Vec<u8>);
    impl RangeFetcher for WholeFetcher {
        fn fetch(
            &self,
            _url: &str,
            offset: u64,
            sink: &mut dyn FnMut(&[u8]) -> Result<(), DownloadError>,
        ) -> Result<u64, DownloadError> {
            let start = usize::try_from(offset)
                .unwrap_or(usize::MAX)
                .min(self.0.len());
            if start < self.0.len() {
                sink(&self.0[start..])?;
            }
            Ok(self.0.len() as u64)
        }
    }

    /// Streams a body in fixed-size chunks, so the sink — and therefore the
    /// progress throttle — is exercised many times per transfer.
    struct ChunkedFetcher {
        data: Vec<u8>,
        chunk: usize,
    }
    impl RangeFetcher for ChunkedFetcher {
        fn fetch(
            &self,
            _url: &str,
            offset: u64,
            sink: &mut dyn FnMut(&[u8]) -> Result<(), DownloadError>,
        ) -> Result<u64, DownloadError> {
            let start = usize::try_from(offset)
                .unwrap_or(usize::MAX)
                .min(self.data.len());
            for piece in self.data[start..].chunks(self.chunk) {
                sink(piece)?;
            }
            Ok(self.data.len() as u64)
        }
    }

    /// Serves corrupt bytes on the first full attempt and good bytes after —
    /// the discard-and-refetch path BR-6 requires.
    struct CorruptThenGoodFetcher {
        good: Vec<u8>,
        corrupt: Vec<u8>,
        served: Mutex<u32>,
        chunk: usize,
    }
    impl RangeFetcher for CorruptThenGoodFetcher {
        fn fetch(
            &self,
            _url: &str,
            offset: u64,
            sink: &mut dyn FnMut(&[u8]) -> Result<(), DownloadError>,
        ) -> Result<u64, DownloadError> {
            let first = {
                let mut served = self.served.lock().unwrap();
                *served += 1;
                *served == 1
            };
            let data = if first { &self.corrupt } else { &self.good };
            let start = usize::try_from(offset)
                .unwrap_or(usize::MAX)
                .min(data.len());
            for piece in data[start..].chunks(self.chunk) {
                sink(piece)?;
            }
            Ok(data.len() as u64)
        }
    }

    /// A fixed typed cause, standing in for the real client's `last_error`.
    struct FixedCause(Option<FetchError>);
    impl FetchCause for FixedCause {
        fn last_cause(&self) -> Option<FetchError> {
            self.0.clone()
        }
    }
}
