//! The first-run consent gate for the local model tier (REQ-547 BR-1..BR-5,
//! BR-10..BR-12).
//!
//! The daemon probes the hardware, **proposes** a model, and waits. Only after an
//! explicit decision does a single byte of model data get fetched. That ordering
//! is the whole requirement: REQ-544 downloaded 1–18 GB autonomously, and this
//! module is what makes the download conditional on a human answer.
//!
//! ## The round-trip is the permission round-trip
//!
//! [`PendingModelDecisions`] is [`crate::harness::PendingPermissions`] with a
//! different payload, deliberately — same registration-before-publish ordering,
//! same `oneshot` resolve seam, same "the server's reader loop stays free while
//! the flow awaits" property. REQ-544's review established that ordering is
//! deadlock-free; reusing the shape rather than inventing a second mechanism is
//! what keeps that result applicable (architecture D-3).
//!
//! ## What the gate gates
//!
//! The **tier**, never the session (D-3). While a proposal is outstanding the
//! local tier is simply unavailable and sessions run remote-only, so a user who
//! ignores the prompt has a working tool rather than a dead one (BR-1).
//!
//! ## Where the fetcher enters
//!
//! [`ModelConsentGate`] holds the [`WeightsInstaller`] — the only thing in this
//! flow that can touch the network — and calls it in exactly one place,
//! [`ModelConsentGate::commit`], which is reachable only from a decided outcome.
//! That is what makes AC-1 testable rather than assertable: a recording
//! [`teton_inference::download::RangeFetcher`] double placed behind the
//! *production* installer ([`crate::install::WeightsInstall`]) must show zero
//! calls until an answer arrives.
//!
//! ## Failure is not a decline
//!
//! An accept that cannot reach the network records the user's *decision* (they
//! did decide) and installs nothing. It is never recorded as declined, and
//! because the weights are then missing, BR-10's missing-weights clause makes the
//! next start re-propose — which is precisely AC-10's "a later run with
//! connectivity re-prompts and succeeds". One mechanism covers both a failed
//! install and a crash mid-download.

use std::collections::HashSet;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use tokio::sync::oneshot;

use teton_core::config::LocalModelConfig;
use teton_core::entities::{ModelSelection, SelectionSource};

use teton_inference::catalog::{url_host, Catalog, ModelEntry, TierBand};
use teton_inference::probe::{band_for_ram, decide, GpuClass, HardwareProfile, TierDecision, GIB};

use teton_protocol::events::{
    CatalogEntryView, CatalogProvenance, ChosenBand, Event, FetchNotice, GpuClass as WireGpuClass,
    ModelLifecycle, ModelLifecycleStage, ModelSelectionDecided, ModelSelectionProposed,
    ProbeReportView, ProposedModel, SelectionSource as WireSelectionSource,
    TierBand as WireTierBand,
};
use teton_protocol::methods::{
    InstallStateView, InstallStatus, ModelConfirmOutcome, ModelListEntry, ModelSelectionView,
};
use teton_protocol::RequestId;

use crate::broadcast::EventBus;
use crate::selection_store::{now_ms, SelectionStore, SelectionStoreError};

/// Free disk required *above* a model's download size before any bytes are
/// fetched (BR-7).
///
/// The download lands in a temporary file that is renamed into place, so at peak
/// exactly one copy exists; the margin covers filesystem overhead and leaves the
/// machine somewhere to breathe rather than filling the volume to the last block.
/// A named constant rather than an inline number because the *proposal* quotes it
/// to the user (`required_disk_bytes`) and the preflight check enforces it — two
/// call sites that must never disagree about what "enough room" means.
pub const DISK_WORKING_MARGIN_BYTES: u64 = GIB;

/// Free disk the install of `entry` needs: its download size plus the working
/// margin (BR-7). Quoted to the user in the proposal before anything is fetched.
#[must_use]
pub fn required_disk_bytes(entry: &ModelEntry) -> u64 {
    entry.size_bytes.saturating_add(DISK_WORKING_MARGIN_BYTES)
}

// ---------------------------------------------------------------------------
// The pending-decision registry (mirrors `PendingPermissions`)
// ---------------------------------------------------------------------------

/// One outstanding proposal: the payload a client must be able to *render*, and
/// the channel its answer comes back on.
///
/// The two are held together deliberately. Keeping only the `request_id` here
/// would let a client answer a prompt it could not describe — which is how the
/// proposal came to be undeliverable *and* unnameable in the first place.
#[derive(Debug)]
struct OpenProposal {
    /// The full proposal, exactly as broadcast.
    proposal: ModelSelectionProposed,
    /// Where the deciding client's answer is delivered.
    answer: oneshot::Sender<ModelConfirmOutcome>,
}

/// The registry of outstanding model proposals, keyed by request id.
///
/// The consent flow registers a waiter and awaits it; a client's `model/confirm`
/// calls [`Self::resolve`]. Registration happens **before** the proposal is
/// published, so a client that answers the instant it sees the event always finds
/// a waiter — the same ordering [`crate::harness::PendingPermissions`] relies on.
///
/// It is also the daemon's *retrieval* path: because the whole proposal is
/// registered, `model/status` can hand a late-attaching client the same payload
/// the event carried ([`Self::outstanding`]), so delivery does not depend on
/// having been attached at the instant of the broadcast.
#[derive(Debug, Default)]
pub struct PendingModelDecisions {
    waiters: Mutex<Vec<OpenProposal>>,
}

impl PendingModelDecisions {
    /// An empty registry.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Register a waiter for `proposal` and return the receiver the consent flow
    /// awaits. The proposal is retained so `model/status` can serve it.
    fn register(&self, proposal: ModelSelectionProposed) -> oneshot::Receiver<ModelConfirmOutcome> {
        let (tx, rx) = oneshot::channel();
        self.waiters
            .lock()
            .expect("pending model decisions mutex poisoned")
            .push(OpenProposal {
                proposal,
                answer: tx,
            });
        rx
    }

    /// Deliver a client's answer to the waiting consent flow. Returns `true` if a
    /// waiter was present, so a late or duplicate `model/confirm` is a no-op
    /// rather than an error (the same idempotence `permission/respond` has).
    pub fn resolve(&self, id: &RequestId, outcome: ModelConfirmOutcome) -> bool {
        let sender = {
            let mut waiters = self
                .waiters
                .lock()
                .expect("pending model decisions mutex poisoned");
            waiters
                .iter()
                .position(|open| &open.proposal.request_id == id)
                .map(|index| waiters.swap_remove(index).answer)
        };
        match sender {
            Some(tx) => tx.send(outcome).is_ok(),
            None => false,
        }
    }

    /// Cancel the outstanding proposal (if any), dropping its answer channel so
    /// the parked consent flow observes the decision was made elsewhere (M-4).
    ///
    /// This is how a `model/set` supersedes a first-run proposal: dropping the
    /// [`oneshot::Sender`] makes the flow's `rx.await` resolve to `Err(RecvError)`,
    /// which [`ModelConsentGate::resolve`] treats as "abandon — an explicit
    /// decision is now on record", rather than letting a later `Accept` overwrite
    /// the user's `model/set` choice. A stale `model/confirm` that arrives after
    /// the cancel finds no waiter and is a harmless no-op, exactly like a duplicate
    /// answer.
    ///
    /// Returns the cancelled proposal (at most one is ever outstanding), and the
    /// caller is expected to *use* it: [`ModelConsentGate::set_model`] announces
    /// its own decision under the cancelled `request_id`, so a client parked on
    /// that prompt is told what became of it instead of rendering a question whose
    /// waiter no longer exists (E-8).
    pub fn cancel(&self) -> Option<ModelSelectionProposed> {
        let mut waiters = self
            .waiters
            .lock()
            .expect("pending model decisions mutex poisoned");
        let cancelled = waiters.first().map(|open| open.proposal.clone());
        // Clearing drops every `OpenProposal` — and with it every answer sender —
        // so any parked flow wakes with `Err`.
        waiters.clear();
        cancelled
    }

    /// The proposal currently awaiting an answer, if any — **in full**.
    ///
    /// At most one proposal is ever outstanding: the decision is machine-wide,
    /// not per-session, so there is nothing to disambiguate. This is what lets a
    /// client that attached *after* the broadcast render and answer the open
    /// prompt through `model/status` instead of waiting forever for an event it
    /// already missed — and because it returns the whole payload, that client can
    /// name the proposed entry, its download size, and its RAM floor (BR-2)
    /// rather than describing the band and guessing.
    #[must_use]
    pub fn outstanding(&self) -> Option<ModelSelectionProposed> {
        self.waiters
            .lock()
            .expect("pending model decisions mutex poisoned")
            .first()
            .map(|open| open.proposal.clone())
    }

    /// Number of proposals currently awaiting an answer.
    #[must_use]
    pub fn pending_count(&self) -> usize {
        self.waiters
            .lock()
            .expect("pending model decisions mutex poisoned")
            .len()
    }
}

// ---------------------------------------------------------------------------
// Install seam
// ---------------------------------------------------------------------------

/// A failure while installing model weights.
///
/// Every message is actionable and **content-free**: no URL, no digest, no
/// filesystem path (BR-11). The distinction between the variants is load-bearing
/// — "the network is down" and "the artifact was corrupt" call for different user
/// actions, and AC-10/AC-12 require they never be collapsed into one another.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum InstallError {
    /// The model host could not be reached, or the transfer failed. Nothing was
    /// installed and the decision stands — retrying once online completes it.
    #[error(
        "could not download the model weights: {detail}. Nothing was installed; \
         reconnect and start the daemon again to retry."
    )]
    Network {
        /// The classified transport cause, from the download client.
        detail: String,
    },
    /// The model host is rate-limiting downloads (BR-16 / AC-12).
    ///
    /// Deliberately **not** a [`InstallError::Network`]: "wait and retry" and
    /// "your connection is broken" are different instructions, and AC-12
    /// requires a 429 never read as a corrupt download either.
    #[error(
        "the model host is rate-limiting downloads: {detail}. \
         Nothing was installed; try again shortly."
    )]
    RateLimited {
        /// The classified transport cause, from the download client.
        detail: String,
    },
    /// The artifact failed its SHA-256 check and was discarded (BR-6/BR-9).
    #[error(
        "the downloaded model weights failed their integrity check and were discarded. \
         Nothing was installed; run the download again."
    )]
    Corrupt,
    /// The volume has less free space than the artifact plus its working margin
    /// (BR-7 / AC-6). Raised by the preflight, **before** any bytes are fetched.
    #[error(
        "not enough free disk space for the model weights: {} needed, {} available. \
         Free up space and start the daemon again.",
        gib(*required_bytes),
        gib(*available_bytes)
    )]
    InsufficientDisk {
        /// Free space the install needs: the artifact's size plus
        /// [`DISK_WORKING_MARGIN_BYTES`], less anything already downloaded.
        required_bytes: u64,
        /// Free space the volume actually reported.
        available_bytes: u64,
    },
    /// A local filesystem failure while writing or installing the weights.
    #[error("could not write the model weights to the daemon state directory: {detail}")]
    Io {
        /// The failure kind. Never a path.
        detail: String,
    },
    /// The weights directory is not exclusively owned by this daemon (M-11/M-12).
    ///
    /// The temp-dir fallback base is the hazard: a world-writable base lets another
    /// user pre-create the directory, or plant a symlink at the predictable `.part`
    /// path, before the daemon writes. Refusing is the safe answer — a shared base
    /// is not a place to land weights the engine will later load.
    #[error(
        "refusing to install model weights: the daemon's weights directory is not \
         exclusively owned by this user, so another user could tamper with the \
         download. Check the permissions on the daemon state directory."
    )]
    UntrustedWeightsDir,
    /// This daemon has no download client, so no install can be attempted.
    #[error("this daemon has no model-download client; no weights were installed")]
    Unavailable,
}

/// Downloads, verifies, and installs a catalog entry's weights.
///
/// The seam between the consent gate (which decides *whether* to fetch) and the
/// install pipeline (which decides *how*). The production implementation is
/// [`crate::install::WeightsInstall`]; the gate knows only this trait, which is
/// what let TASK-005 replace the whole pipeline underneath it without touching a
/// line of the decision flow.
pub trait WeightsInstaller: Send + Sync {
    /// Fetch, verify, and install `entry`'s weights.
    ///
    /// Must leave **no file at the final path** unless verification passed
    /// (BR-9): a caller is entitled to treat "the final path exists" as "these
    /// weights are the catalog's weights".
    ///
    /// # Errors
    /// Returns an [`InstallError`] classifying the failure.
    fn install(&self, entry: &ModelEntry) -> Result<(), InstallError>;

    /// The on-disk state of `entry`'s weights — the cheap, receipt-backed read
    /// the daemon performs on every attach (Group B: paid for once).
    fn status(&self, entry: &ModelEntry) -> InstallStatus;

    /// The on-disk state of `entry`'s weights read from the *bytes themselves*,
    /// never from the receipt (M-10).
    ///
    /// The receipt is a pay-once cache keyed on size+mtime; mtime is forgeable,
    /// so it is a cache, not an attestation. This is the read the tier-un-gating
    /// path uses — the security-relevant moment — so the tier cannot open on a
    /// forged receipt. The default delegates to [`status`](Self::status), which is
    /// correct for installers that keep no cache (they read the bytes regardless).
    fn deep_status(&self, entry: &ModelEntry) -> InstallStatus {
        self.status(entry)
    }
}

/// An installer for a daemon with no download client.
///
/// Every install refuses and nothing is ever present. Used by the minimal runtime
/// (which runs no prompt turns) and as the fallback when the HTTP client cannot
/// be initialized — a daemon that cannot download must say so, not pretend the
/// weights are absent-but-coming.
#[derive(Debug, Default, Clone, Copy)]
pub struct NoInstaller;

impl WeightsInstaller for NoInstaller {
    fn install(&self, _entry: &ModelEntry) -> Result<(), InstallError> {
        Err(InstallError::Unavailable)
    }

    fn status(&self, _entry: &ModelEntry) -> InstallStatus {
        InstallStatus::Absent
    }
}

// ---------------------------------------------------------------------------
// Proposal assembly (BR-2 legibility, BR-11 payload hygiene)
// ---------------------------------------------------------------------------

/// Project the inference crate's GPU class onto the wire form.
///
/// A total match rather than a string map, so adding a class to either side is a
/// compile error instead of a silent `cpu`.
#[must_use]
fn wire_gpu(gpu: GpuClass) -> WireGpuClass {
    match gpu {
        GpuClass::AppleSilicon => WireGpuClass::AppleSilicon,
        GpuClass::Cuda => WireGpuClass::Cuda,
        GpuClass::Cpu => WireGpuClass::Cpu,
    }
}

/// Project a catalog band onto the wire form.
#[must_use]
fn wire_band(band: TierBand) -> WireTierBand {
    match band {
        TierBand::Small => WireTierBand::Small,
        TierBand::Mid => WireTierBand::Mid,
        TierBand::Large => WireTierBand::Large,
    }
}

/// Project the persisted decision's source onto the wire form.
#[must_use]
fn wire_source(source: SelectionSource) -> WireSelectionSource {
    match source {
        SelectionSource::Probe => WireSelectionSource::Probe,
        SelectionSource::UserOverride => WireSelectionSource::UserOverride,
        SelectionSource::ConfigPin => WireSelectionSource::ConfigPin,
        SelectionSource::AutoAccept => WireSelectionSource::AutoAccept,
    }
}

/// The client-facing projection of a catalog entry.
///
/// Drops the full `url` and the `sha256` — download mechanics the user is not
/// choosing between, and the shape BR-11 keeps off the wire — but keeps the
/// entry's *provenance* (H-2): the publisher/repo, the host, and the short
/// revision, so the consent screen shows where the bytes come from, not only the
/// model's name. These are public facts, never a credential, a full URL, or a
/// path.
#[must_use]
pub fn entry_view(entry: &ModelEntry) -> CatalogEntryView {
    CatalogEntryView {
        name: entry.name.clone(),
        band: wire_band(entry.band),
        size_bytes: entry.size_bytes,
        ram_floor_bytes: entry.ram_floor_bytes,
        provenance: provenance_view(entry),
    }
}

/// The non-sensitive provenance triple for `entry` (H-2).
///
/// `repo` and `revision` come from the pinned `resolve` URL; `host` is the URL's
/// authority. The revision is abbreviated to 7 hex for display — the full 40-hex
/// pin stays daemon-side. A catalog that reaches this point has passed
/// [`ModelEntry::validate`], so the URL parses and the host is `huggingface.co`;
/// the fallbacks exist only so a projection can never panic on a malformed entry.
#[must_use]
fn provenance_view(entry: &ModelEntry) -> CatalogProvenance {
    let source = entry.source();
    CatalogProvenance {
        repo: source.map(|s| s.repo.to_owned()).unwrap_or_default(),
        host: url_host(&entry.url).unwrap_or_default().to_owned(),
        revision: short_revision(&entry.revision),
    }
}

/// The first 7 hex of a commit revision, for display (e.g. `f74adce`).
#[must_use]
fn short_revision(revision: &str) -> String {
    revision.get(..7).unwrap_or(revision).to_owned()
}

/// The client-facing projection of a persisted decision.
#[must_use]
pub fn selection_view(selection: &ModelSelection) -> ModelSelectionView {
    ModelSelectionView {
        model_name: selection.model_name.clone(),
        source: wire_source(selection.source),
        declined_local: selection.declined_local,
        decided_at_ms: selection.decided_at_ms,
    }
}

/// Render `bytes` as a human GiB figure for a user-facing sentence.
fn gib(bytes: u64) -> String {
    format!("{:.1} GiB", bytes as f64 / GIB as f64)
}

/// A plain-language phrase for the detected accelerator.
fn gpu_phrase(gpu: GpuClass) -> &'static str {
    match gpu {
        GpuClass::AppleSilicon => "Apple Silicon acceleration",
        GpuClass::Cuda => "a CUDA GPU",
        GpuClass::Cpu => "no supported GPU (CPU inference)",
    }
}

/// A plain-language name for a band.
fn band_phrase(band: TierBand) -> &'static str {
    match band {
        TierBand::Small => "small",
        TierBand::Mid => "mid",
        TierBand::Large => "large",
    }
}

/// The probe's reasoning as the user sees it (BR-2).
///
/// A bare model name is explicitly not sufficient, so the detected hardware and a
/// sentence explaining the band travel with every proposal. Machine facts only —
/// no path, no credential, no file content (BR-11).
#[must_use]
pub fn probe_view(profile: &HardwareProfile, decision: &TierDecision) -> ProbeReportView {
    let machine_band = band_for_ram(profile.ram_bytes);
    let reason = match decision {
        TierDecision::Disabled { reason } => reason.clone(),
        TierDecision::Selected { model, band, .. } => format!(
            "{} of RAM, {} free disk and {} put this machine in the {} band; \
             {model} is the largest catalog model that fits.",
            gib(profile.ram_bytes),
            gib(profile.free_disk_bytes),
            gpu_phrase(profile.gpu),
            // The *machine's* band, which is what the sentence is about. It can
            // differ from the selected model's band when a config pin overrode
            // the probe (REQ-544 BR-9); the model's own band is the honest
            // fallback for a machine that has no band of its own.
            machine_band.map_or(band_phrase(*band), band_phrase),
        ),
    };
    ProbeReportView {
        total_ram_bytes: profile.ram_bytes,
        free_disk_bytes: profile.free_disk_bytes,
        gpu_class: wire_gpu(profile.gpu),
        chosen_band: ChosenBand::from(machine_band.map(wire_band)),
        reason,
    }
}

/// Assemble the proposal the daemon broadcasts before fetching anything (BR-1).
///
/// `alternatives` is every *other* catalog entry in catalog order, including ones
/// above this machine's RAM floor — BR-3 says the user may pick them, so hiding
/// them would be the wrong kind of protection; the client flags them instead.
///
/// `base_url` and `catalog_overridden` produce the [`FetchNotice`] (H-2): when a
/// `[local_model] base_url` mirror or a non-bundled catalog is in force, the fetch
/// is redirected away from the provenance host each entry shows, and the user must
/// be told before answering — a redirect they cannot see is where consent means
/// least.
#[must_use]
pub fn build_proposal(
    request_id: RequestId,
    profile: &HardwareProfile,
    catalog: &Catalog,
    decision: &TierDecision,
    base_url: Option<&str>,
    catalog_overridden: bool,
) -> ModelSelectionProposed {
    let proposed_name = decision.model().map(str::to_owned);
    let proposed = proposed_name
        .as_deref()
        .and_then(|name| catalog.get(name))
        .map(|entry| ProposedModel {
            entry: entry_view(entry),
            required_disk_bytes: required_disk_bytes(entry),
        });

    let alternatives = catalog
        .models
        .iter()
        .filter(|entry| Some(entry.name.as_str()) != proposed_name.as_deref())
        .map(entry_view)
        .collect();

    ModelSelectionProposed {
        request_id,
        probe: probe_view(profile, decision),
        proposed,
        alternatives,
        fetch_notice: fetch_notice(base_url, catalog_overridden),
    }
}

/// The [`FetchNotice`] for a proposal, or `None` when nothing redirects the fetch.
///
/// A mirror contributes only its bare host — never the base URL's scheme, path,
/// or userinfo — so the notice is legible (H-2) without leaking a URL (BR-11). A
/// `base_url` with no parseable host still raises the notice with `mirror_host:
/// None`, because the fetch is redirected either way and silence would be the
/// wrong answer.
#[must_use]
fn fetch_notice(base_url: Option<&str>, catalog_overridden: bool) -> Option<FetchNotice> {
    let mirror = base_url
        .map(str::trim)
        .filter(|b| !b.is_empty())
        .map(|b| url_host(b).map(str::to_owned));
    match (mirror, catalog_overridden) {
        (None, false) => None,
        (mirror, override_catalog) => Some(FetchNotice {
            mirror_host: mirror.flatten(),
            override_catalog,
        }),
    }
}

/// The catalog as `model/list` reports it, with each entry's fit for this machine.
///
/// Fits are computed daemon-side so every client renders the same verdict rather
/// than each re-deriving the working margin and disagreeing about it.
#[must_use]
pub fn list_entries(profile: &HardwareProfile, catalog: &Catalog) -> Vec<ModelListEntry> {
    catalog
        .models
        .iter()
        .map(|entry| ModelListEntry {
            entry: entry_view(entry),
            fits_ram: entry.ram_floor_bytes <= profile.ram_bytes,
            fits_disk: required_disk_bytes(entry) <= profile.free_disk_bytes,
        })
        .collect()
}

// ---------------------------------------------------------------------------
// Answer validation (BR-3)
// ---------------------------------------------------------------------------

/// Why the daemon refuses to act on a client's model choice.
///
/// `UnknownModel` and `AboveRamFloor` are *recoverable at the RPC boundary*: the
/// client fixes the name, or re-sends with the second confirmation, and the
/// outstanding proposal is left open — `model/confirm` validates them *before*
/// resolving the waiter, so a bad answer never costs the user their one chance to
/// answer (BR-3). `NothingToAccept` is different in kind: it means an `accept`
/// was sent for a proposal that offered no model at all, which `model/confirm`
/// now also rejects up front (an `INVALID_PARAMS` with the proposal left open)
/// for the same reason — reached inside the gate it *would* consume the waiter,
/// and guessing which model "accept" meant would be exactly the autonomous
/// download this REQ exists to stop.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum ChoiceRefusal {
    /// The named entry is not in this daemon's catalog.
    #[error("no catalog model is named '{name}'; run `teton model list` to see the choices")]
    UnknownModel {
        /// The name the client sent.
        name: String,
    },
    /// The entry needs more RAM than this machine has, and the client has not
    /// sent the second confirmation BR-3 requires.
    #[error(
        "'{name}' needs {needed} of RAM and this machine has {available}. \
         It is your machine and your call, but this cannot happen by accident: \
         re-send the choice with the above-RAM-floor confirmation to proceed."
    )]
    AboveRamFloor {
        /// The entry the client chose.
        name: String,
        /// Its RAM floor, rendered.
        needed: String,
        /// The machine's RAM, rendered.
        available: String,
    },
    /// The client accepted a proposal that offered no model — this machine has
    /// no fitting catalog entry. Guessing which model "accept" meant would be
    /// exactly the autonomous download this REQ exists to stop.
    #[error(
        "this machine has no fitting catalog model to accept. \
         Choose one explicitly (`teton model list`), or decline the local tier."
    )]
    NothingToAccept,
}

/// Resolve a client-chosen catalog name against the catalog and the machine.
///
/// The BR-3 double-confirmation lives here rather than in each client, so a new
/// client cannot forget it: an entry above the machine's RAM floor is refused
/// until `confirmed_above_ram_floor` is set.
///
/// # Errors
/// Returns a [`ChoiceRefusal`] for an unknown name or an unconfirmed
/// above-RAM-floor pick. Nothing is recorded and nothing is fetched either way.
pub fn validate_choice<'a>(
    catalog: &'a Catalog,
    profile: &HardwareProfile,
    name: &str,
    confirmed_above_ram_floor: bool,
) -> Result<&'a ModelEntry, ChoiceRefusal> {
    let entry = catalog
        .get(name)
        .ok_or_else(|| ChoiceRefusal::UnknownModel {
            name: name.to_owned(),
        })?;
    if entry.ram_floor_bytes > profile.ram_bytes && !confirmed_above_ram_floor {
        return Err(ChoiceRefusal::AboveRamFloor {
            name: entry.name.clone(),
            needed: gib(entry.ram_floor_bytes),
            available: gib(profile.ram_bytes),
        });
    }
    Ok(entry)
}

// ---------------------------------------------------------------------------
// The gate
// ---------------------------------------------------------------------------

/// The outcome of resolving the local tier's consent state.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ConsentOutcome {
    /// A model is decided, its weights are installed and verified, **and** this
    /// build has a local engine that can load them: the local tier may run.
    Ready {
        /// The decision in force.
        selection: ModelSelection,
    },
    /// A model is decided and its weights are installed and verified, but this
    /// build has no local inference engine to load them (M-1).
    ///
    /// The install genuinely succeeded — the bytes on disk are the catalog's
    /// bytes — so this is emphatically **not** an [`InstallFailed`](Self::InstallFailed).
    /// But the tier cannot serve, so it must not be reported as
    /// [`Ready`](Self::Ready): that is the same untruth `startup_lifecycle`
    /// refuses to tell, and the lifecycle event this drives is `disabled`, not
    /// `ready`. The tier stays withheld; sessions run remote-only.
    InstalledNoEngine {
        /// The model whose weights are installed.
        model_name: String,
    },
    /// An install for this entry was already in flight, so this attempt did
    /// nothing (M-2).
    ///
    /// The shared `.part` is not this task's to touch: two installs of the same
    /// entry appending to one partial file would interleave bytes and fail the
    /// digest. The in-flight install is the authority on the tier gate, so this
    /// no-op leaves the gate exactly as it found it — like [`Superseded`](Self::Superseded).
    AlreadyInstalling {
        /// The model whose install is already running.
        model_name: String,
    },
    /// The user declined the local tier (BR-4): remote-only, never re-proposed.
    Declined,
    /// No catalog entry fits this machine and no override was made. The local
    /// tier is absent; sessions run remote-only.
    Unavailable {
        /// User-facing reason (the probe's own sentence).
        reason: String,
    },
    /// A decision was made and recorded, but the install failed (BR-12 / AC-10).
    ///
    /// **Not** a decline. The weights are missing, so BR-10's missing-weights
    /// clause re-proposes on the next start.
    InstallFailed {
        /// The model the user decided on.
        model_name: String,
        /// Why the install failed.
        error: InstallError,
    },
    /// The client's answer was refused (BR-3): nothing recorded, nothing fetched.
    Refused {
        /// Why it was refused.
        refusal: ChoiceRefusal,
    },
    /// The proposal went unanswered — the client detached, or the daemon is
    /// shutting down. The tier stays unavailable and the next start re-proposes
    /// (BR-1: absent a decision, sessions proceed remote-only).
    Undecided,
    /// The outstanding proposal was superseded by an explicit `model/set` while
    /// this flow was parked (M-4). The `model/set` recorded the decision and
    /// drives its own install, so this flow abandons without recording anything
    /// and — crucially — without touching the tier gate, leaving that to the
    /// authoritative `model/set` install path.
    Superseded,
}

impl ConsentOutcome {
    /// Whether the local tier may be used as a result of this outcome.
    #[must_use]
    pub fn local_tier_ready(&self) -> bool {
        matches!(self, ConsentOutcome::Ready { .. })
    }
}

/// The first-run consent authority for the local model tier.
///
/// Owns everything the decision needs — the probe, the catalog, the user's config
/// inputs, the decision store, the pending-answer registry, and the installer —
/// so the flow is one `await` a test can drive with `tokio::join!` exactly like
/// the permission gate.
pub struct ModelConsentGate {
    profile: HardwareProfile,
    catalog: Catalog,
    config: LocalModelConfig,
    events: Arc<EventBus>,
    pending: Arc<PendingModelDecisions>,
    store: Arc<SelectionStore>,
    installer: Arc<dyn WeightsInstaller>,
    counter: AtomicU64,
    /// Whether this build has a local inference engine that can load installed
    /// weights (M-1). Gates the `Ready` publish onto the SAME signal
    /// `startup_lifecycle` uses (`engine.is_some()`): a successful install on a
    /// build with no engine publishes `disabled`, not a `ready` it cannot honour.
    /// Defaults to `false` — the honest default for the production build, which
    /// ships no GGUF loader.
    local_engine_present: bool,
    /// Whether the catalog is a non-bundled override (`TETON_CATALOG`, H-2).
    /// Feeds the proposal's [`FetchNotice`] so the consent screen says the
    /// entries are not from the shipped catalog. Defaults to `false`.
    catalog_overridden: bool,
    /// Model names whose install is in flight (M-2). Claimed under the lock
    /// before an install begins and released when it ends, so two installs of
    /// the same entry can never both open the shared `.part` — the second finds
    /// the name claimed and no-ops rather than interleaving bytes into it.
    installing: Arc<Mutex<HashSet<String>>>,
}

impl ModelConsentGate {
    /// A gate over the given machine, catalog, and user config, publishing to
    /// `events`, awaiting answers on `pending`, recording into `store`, and
    /// installing through `installer`.
    #[must_use]
    pub fn new(
        profile: HardwareProfile,
        catalog: Catalog,
        config: LocalModelConfig,
        events: Arc<EventBus>,
        pending: Arc<PendingModelDecisions>,
        store: Arc<SelectionStore>,
        installer: Arc<dyn WeightsInstaller>,
    ) -> Self {
        Self {
            profile,
            catalog,
            config,
            events,
            pending,
            store,
            installer,
            counter: AtomicU64::new(0),
            local_engine_present: false,
            catalog_overridden: false,
            installing: Arc::new(Mutex::new(HashSet::new())),
        }
    }

    /// Declare that the catalog is a non-bundled override (`TETON_CATALOG`, H-2).
    ///
    /// Threaded into the proposal's [`FetchNotice`] so the consent screen states
    /// the entries are not the shipped catalog — a redirected source the user
    /// cannot otherwise see. Left `false` (the constructor default), the bundled
    /// catalog is assumed.
    #[must_use]
    pub fn with_catalog_override(mut self, overridden: bool) -> Self {
        self.catalog_overridden = overridden;
        self
    }

    /// Declare whether this build has a local inference engine that can load
    /// installed weights (M-1).
    ///
    /// This is the SAME signal `startup_lifecycle` reads (`engine.is_some()`),
    /// threaded into the gate so a completed install publishes `ready` only when
    /// the tier can actually serve. Left `false` (the constructor default), a
    /// successful install publishes `disabled` with the no-engine reason — the
    /// truth for the production build, which has no GGUF loader.
    #[must_use]
    pub fn with_local_engine(mut self, present: bool) -> Self {
        self.local_engine_present = present;
        self
    }

    /// Whether an install for `model_name` is currently in flight (M-2). Lets a
    /// caller skip spawning a duplicate install task rather than spawning one
    /// that would immediately no-op.
    #[must_use]
    pub fn install_in_flight(&self, model_name: &str) -> bool {
        self.installing
            .lock()
            .expect("install-in-flight mutex poisoned")
            .contains(model_name)
    }

    /// The machine this gate proposes for.
    #[must_use]
    pub fn profile(&self) -> &HardwareProfile {
        &self.profile
    }

    /// The catalog this gate proposes from.
    #[must_use]
    pub fn catalog(&self) -> &Catalog {
        &self.catalog
    }

    /// The probe's decision for this machine, honouring a `[local_model] pinned`
    /// key (REQ-544 BR-9: a pin overrides the probe's pick).
    ///
    /// The single place the decision is computed, so the proposal, `model/list`,
    /// and the probe reasoning can never describe different pictures of the same
    /// machine.
    #[must_use]
    pub fn probe_decision(&self) -> TierDecision {
        decide(&self.profile, &self.catalog, self.config.pinned.as_deref())
    }

    /// The registry a `model/confirm` handler resolves against.
    #[must_use]
    pub fn pending(&self) -> &Arc<PendingModelDecisions> {
        &self.pending
    }

    /// The recorded decision, or `None` when this machine has not decided.
    #[must_use]
    pub fn current_selection(&self) -> Option<ModelSelection> {
        self.store.current()
    }

    /// The install state of the currently selected model, for `model/status`.
    ///
    /// `None` when nothing is selected, when the tier was declined, or when the
    /// recorded model is no longer in the catalog.
    #[must_use]
    pub fn current_install(&self) -> Option<InstallStateView> {
        let selection = self.store.current()?;
        let name = selection.model_name?;
        let entry = self.catalog.get(&name)?;
        Some(InstallStateView {
            model_name: name,
            status: self.installer.status(entry),
        })
    }

    /// Whether a decision must still be obtained before the local tier can run.
    ///
    /// The BR-10 state read: a decline is final, an installed model is settled,
    /// and only missing or corrupt weights re-open the question.
    #[must_use]
    pub fn consent_required(&self) -> bool {
        match self.store.current() {
            None => true,
            Some(selection) if selection.declined_local => false,
            Some(selection) => match selection.model_name.as_deref() {
                Some(name) => match self.catalog.get(name) {
                    Some(entry) => self.installer.status(entry) != InstallStatus::Verified,
                    // The recorded model left the catalog: re-propose rather than
                    // silently running with a model this build cannot describe.
                    None => true,
                },
                None => true,
            },
        }
    }

    /// Resolve the local tier's consent state, proposing and awaiting an answer
    /// when — and only when — one is needed.
    ///
    /// **No download happens on any path that has not reached a decision.** The
    /// installer is reachable only through [`Self::commit`], and every branch
    /// above the `await` either returns or falls through to the proposal.
    pub async fn resolve(&self) -> ConsentOutcome {
        // BR-10: a recorded decision is not re-litigated.
        if let Some(selection) = self.store.current() {
            if selection.declined_local {
                // AC-4: a decline is final. No proposal, no prompt, ever again.
                return ConsentOutcome::Declined;
            }
            if let Some(entry) = selection
                .model_name
                .as_deref()
                .and_then(|name| self.catalog.get(name))
            {
                // M-10: un-gating the tier is the security-relevant moment, so it
                // is decided by re-digesting the bytes (`deep_status`), not by
                // trusting the install receipt — which is a pay-once *cache* keyed
                // on size+mtime, and mtime is forgeable. `resolve` runs once at
                // startup, so this pays the hash at most once per boot; the cheap
                // receipt-backed `status` still serves the non-security-critical
                // attach/status path (Group B). The tier must not open on a
                // forgeable receipt alone.
                //
                // M-3 (E-3): that digest reads multiple gigabytes *synchronously*,
                // and `resolve` is an `async fn` on a tokio worker — so it runs on
                // the blocking pool, exactly like the install itself. Doing it
                // inline would park a runtime thread for the whole hash on every
                // start, which is the defect `status`'s receipt cache exists to
                // avoid on the attach path.
                let installer = Arc::clone(&self.installer);
                let target = entry.clone();
                let verified = matches!(
                    tokio::task::spawn_blocking(move || installer.deep_status(&target)).await,
                    Ok(InstallStatus::Verified)
                );
                if verified {
                    // M-1 (E-5): verified weights are not a running tier. Whether
                    // the tier may serve depends on something the bytes cannot
                    // supply — an engine to load them — so this reports exactly
                    // what `report_install_success` reports for the same state,
                    // rather than a `Ready` the daemon could not honour.
                    return if self.local_engine_present {
                        ConsentOutcome::Ready { selection }
                    } else {
                        ConsentOutcome::InstalledNoEngine {
                            model_name: entry.name.clone(),
                        }
                    };
                }
            }
            // Missing, corrupt, or no-longer-catalogued weights: BR-10's one
            // sanctioned re-prompt. This is also the path an offline accept takes
            // on its next start (AC-10) — the decision was recorded, the bytes
            // never arrived.
        }

        let decision = self.probe_decision();

        // C-1 (REQ-547 review): a `[local_model] pinned` key is NOT consent to a
        // download. It already feeds `probe_decision()` above (REQ-544 BR-9: a
        // pin overrides the probe's pick), so the pinned model is simply the one
        // the proposal below NAMES — and the user still answers that proposal
        // before a single byte is fetched. There is deliberately no early
        // `commit` for a pin here: a pin changes *which* model is proposed, never
        // *whether* consent is required (BR-1). Silently committing a pin was the
        // one path that let an existing REQ-544 `pinned` key trigger an unprompted
        // multi-gigabyte fetch on first REQ-547 start.

        // BR-5 / AC-5: the explicit opt-in unattended path. Note what is *absent*
        // here — no `ModelSelectionProposed` is published, because there is no one
        // to answer it; the flow still emits `model_selection_decided` so an
        // attached client learns why the tier is in the state it is in.
        if self.config.auto_accept {
            return match self.catalog.get(decision.model().unwrap_or_default()) {
                Some(entry) => self.commit(entry, SelectionSource::AutoAccept, None).await,
                None => ConsentOutcome::Unavailable {
                    reason: disabled_reason(&decision),
                },
            };
        }

        // --- the gate itself ---
        //
        // Register the waiter *before* publishing, so a client answering the
        // instant it sees the event always finds a waiter; then await. Nothing
        // above this point has touched the installer, and nothing below it runs
        // until an answer arrives (BR-1 / AC-1).
        //
        // Registration takes the **whole** proposal, not just its id: the event
        // is broadcast once and never replayed, and this flow is spawned beside
        // `serve` (D-3) so it may publish before the daemon accepts its first
        // connection. The registry is therefore the retrieval path a client of
        // any attach timing reads through `model/status` — see
        // [`PendingModelDecisions::outstanding`].
        let request_id = RequestId::from(format!(
            "model-{}",
            self.counter.fetch_add(1, Ordering::SeqCst)
        ));
        let proposal = build_proposal(
            request_id.clone(),
            &self.profile,
            &self.catalog,
            &decision,
            self.config.base_url.as_deref(),
            self.catalog_overridden,
        );
        let proposed_name = proposal
            .proposed
            .as_ref()
            .map(|proposed| proposed.entry.name.clone());
        // M-4: snapshot the recorded decision as it stands the instant before we
        // publish and park. A concurrent `model/set` records an explicit decision
        // and cancels this proposal; if the recorded decision has changed by the
        // time we wake, this answer is stale and must not overwrite the newer,
        // explicit choice.
        let decision_before = self.store.current();
        let rx = self.pending.register(proposal.clone());
        self.events
            .publish(None, Event::ModelSelectionProposed(proposal));

        let Ok(outcome) = rx.await else {
            // No answer arrived on this channel. Either the client detached / the
            // daemon is shutting down (D-3: the tier stays unavailable, the
            // session is untouched, the next start re-proposes), OR a concurrent
            // `model/set` cancelled this proposal (M-4). Distinguish by whether the
            // recorded decision changed under us: if it did, that `model/set` is
            // authoritative and drives its own install — abandon this flow without
            // touching the tier gate.
            return if self.store.current() != decision_before {
                ConsentOutcome::Superseded
            } else {
                ConsentOutcome::Undecided
            };
        };

        // M-4: an answer *did* arrive, but a `model/set` may still have raced in
        // and recorded an explicit decision while we were parked. Honour that
        // decision rather than overwriting it with this now-stale answer.
        if self.store.current() != decision_before {
            return ConsentOutcome::Superseded;
        }

        match outcome {
            ModelConfirmOutcome::Decline => {
                let selection = ModelSelection::declined(now_ms());
                // M-6 / BR-4: a decline that cannot be persisted must not vanish
                // silently — the user would be re-prompted forever with no signal.
                // Surface it; the in-memory record still holds for this process.
                if let Err(err) = self.store.record(&selection) {
                    report_persist_failure("declined-local decision", &err);
                }
                self.announce(&selection, Some(request_id));
                ConsentOutcome::Declined
            }
            ModelConfirmOutcome::Accept => {
                let Some(name) = proposed_name.as_deref() else {
                    return ConsentOutcome::Refused {
                        refusal: ChoiceRefusal::NothingToAccept,
                    };
                };
                // BR-3 (E-1): an `accept` is the cheapest answer in the system —
                // one keystroke, and the client's question defaults to yes. It
                // must therefore be held to the SAME floor check as a `choose`,
                // because the proposal can name an entry above this machine's
                // RAM: `probe::decide` honours a `[local_model] pinned` key
                // unconditionally (REQ-544 BR-9), and since C-1 that pin reaches
                // the user as the *proposed* model rather than a silent commit.
                // Without this, a pin naming the 30B entry on a 16 GiB machine
                // turned one Enter into an 18 GB fetch and BR-3's second
                // confirmation was never asked for. `accept` carries no second
                // confirmation by construction, so it is validated with
                // `confirmed_above_ram_floor: false`; the client re-sends the
                // same entry as a `choose` once the user has confirmed twice.
                match validate_choice(&self.catalog, &self.profile, name, false) {
                    Ok(entry) => {
                        // The pin is what chose this model, so the record says so
                        // rather than crediting the probe it overrode.
                        let source = match &decision {
                            TierDecision::Selected { pinned: true, .. } => {
                                SelectionSource::ConfigPin
                            }
                            _ => SelectionSource::Probe,
                        };
                        self.commit(entry, source, Some(request_id)).await
                    }
                    Err(refusal) => ConsentOutcome::Refused { refusal },
                }
            }
            ModelConfirmOutcome::Choose {
                name,
                confirmed_above_ram_floor,
            } => {
                // Defence in depth: `model/confirm` already validates so the
                // client gets an RPC error it can correct, but the gate refuses
                // an invalid choice on its own rather than trusting the caller.
                match validate_choice(
                    &self.catalog,
                    &self.profile,
                    &name,
                    confirmed_above_ram_floor,
                ) {
                    Ok(entry) => {
                        self.commit(entry, SelectionSource::UserOverride, Some(request_id))
                            .await
                    }
                    Err(refusal) => ConsentOutcome::Refused { refusal },
                }
            }
        }
    }

    /// Record `name` as the selection in force without proposing (`model/set`).
    ///
    /// This is a user-only action (spec Permissions table) and is BR-10's other
    /// sanctioned way to re-open a settled question. It records and announces the
    /// decision; installing the newly chosen weights is [`Self::install_recorded`].
    ///
    /// # Errors
    /// Returns a [`ChoiceRefusal`] for an unknown name or an unconfirmed
    /// above-RAM-floor pick (BR-3). Nothing is recorded in that case.
    pub fn set_model(
        &self,
        name: &str,
        confirmed_above_ram_floor: bool,
    ) -> Result<ModelSelection, ChoiceRefusal> {
        let entry = validate_choice(
            &self.catalog,
            &self.profile,
            name,
            confirmed_above_ram_floor,
        )?;
        // M-4 / BR-10: a `model/set` is an explicit decision that supersedes any
        // outstanding first-run proposal. Cancel it *before* recording, so a
        // late `Accept` for the old proposal finds no waiter and cannot overwrite
        // this choice with a different model.
        let cancelled = self.pending.cancel();
        let selection =
            ModelSelection::accepted(entry.name.clone(), SelectionSource::UserOverride, now_ms());
        // M-6: a decision the daemon could not persist must not vanish silently —
        // surface it (the message names no path, BR-11). The in-memory record is
        // still updated, so the choice holds for this process either way.
        if let Err(err) = self.store.record(&selection) {
            report_persist_failure("model/set selection", &err);
        }
        // E-8: when this superseded an open proposal, the decided event carries
        // that proposal's `request_id`. A client parked on it is otherwise never
        // told what became of its prompt — it would keep rendering a question
        // whose waiter no longer exists, and a `model/confirm` answering it would
        // be silently dropped. Naming the cancelled id is what lets the client
        // correlate "the thing I asked" with "the decision that was actually
        // made".
        self.announce(&selection, cancelled.map(|open| open.request_id));
        Ok(selection)
    }

    /// Install the weights for the decision already in the store, announcing
    /// nothing (the decision was announced when it was made).
    ///
    /// Used after `model/set`, where the decision and the install are two steps
    /// so the RPC can answer immediately while the download proceeds.
    pub async fn install_recorded(&self) -> ConsentOutcome {
        let Some(selection) = self.store.current() else {
            return ConsentOutcome::Undecided;
        };
        if selection.declined_local {
            return ConsentOutcome::Declined;
        }
        let Some(entry) = selection
            .model_name
            .as_deref()
            .and_then(|name| self.catalog.get(name))
            .cloned()
        else {
            return ConsentOutcome::Undecided;
        };
        self.run_install(&entry, selection).await
    }

    /// Record the decision, announce it, and install the weights.
    ///
    /// The **only** path to the installer. Recording happens before the install
    /// so a crash mid-download cannot lose the user's answer; the missing weights
    /// are what make the next start re-propose (BR-10), which is also why a
    /// failed install is never written down as a decline (BR-12).
    async fn commit(
        &self,
        entry: &ModelEntry,
        source: SelectionSource,
        request_id: Option<RequestId>,
    ) -> ConsentOutcome {
        let selection = ModelSelection::accepted(entry.name.clone(), source, now_ms());
        // M-6 / BR-12: recording precedes the install so a crash mid-download
        // cannot lose the answer; a record that could not be written is surfaced
        // rather than discarded (the in-memory record still holds, so the install
        // below still proceeds against this decision).
        if let Err(err) = self.store.record(&selection) {
            report_persist_failure("accepted selection", &err);
        }
        self.announce(&selection, request_id);
        let entry = entry.clone();
        self.run_install(&entry, selection).await
    }

    /// Run the (blocking) installer off the async executor and classify the result.
    async fn run_install(&self, entry: &ModelEntry, selection: ModelSelection) -> ConsentOutcome {
        // M-2: claim this entry before touching its `.part`. If another install
        // of the same entry already holds the claim, this one does nothing — two
        // installs appending to one shared partial file would interleave bytes
        // and fail the digest.
        let Some(guard) = self.claim_install(&entry.name) else {
            return ConsentOutcome::AlreadyInstalling {
                model_name: entry.name.clone(),
            };
        };

        let installer = Arc::clone(&self.installer);
        let target = entry.clone();
        // E-9: the guard is moved INTO the blocking closure, so the claim's
        // lifetime tracks the *work* rather than this future. `spawn_blocking`
        // work is not cancellable: if this future were dropped — the task
        // aborted, the daemon shutting down — a guard held out here would drop
        // immediately and release the claim while the installer thread was still
        // appending to the shared `.part`, which is precisely the interleaving
        // M-2 exists to prevent. Held inside, it is released when the installer
        // returns or unwinds, so a panic still cannot strand the claim.
        let result = tokio::task::spawn_blocking(move || {
            let _guard = guard;
            installer.install(&target)
        })
        .await;

        match result {
            Ok(Ok(())) => self.report_install_success(entry, selection),
            Ok(Err(error)) => self.report_install_failure(entry, error),
            // The blocking task panicked or was cancelled. Report it as an
            // install failure rather than swallowing it: the tier is not ready.
            Err(_) => self.report_install_failure(
                entry,
                InstallError::Io {
                    detail: "the install task did not complete".to_owned(),
                },
            ),
        }
    }

    /// Claim the install slot for `name`, or `None` when one is already claimed.
    ///
    /// The claim is a single entry in [`Self::installing`]; the returned guard
    /// removes it on drop. Holding it across the whole install is what serializes
    /// same-entry installs (M-2).
    fn claim_install(&self, name: &str) -> Option<InFlightGuard> {
        let mut installing = self
            .installing
            .lock()
            .expect("install-in-flight mutex poisoned");
        if installing.insert(name.to_owned()) {
            Some(InFlightGuard {
                installing: Arc::clone(&self.installing),
                name: name.to_owned(),
            })
        } else {
            None
        }
    }

    /// Announce a successful install and return its outcome (M-1).
    ///
    /// The install succeeded — the bytes on disk are the catalog's bytes — but
    /// whether the tier can *run* depends on something the install cannot provide:
    /// a local inference engine to load the GGUF. So the terminal lifecycle stage
    /// is gated on the SAME `engine.is_some()` signal `startup_lifecycle` uses.
    /// With an engine, `ready` is a fact; without one, saying `ready` would be
    /// "the exact untruth this function exists to stop", so the daemon publishes
    /// `disabled` with the no-engine reason instead — matching what a later attach
    /// would independently report.
    ///
    /// It also re-reads the store: a `model/set` is allowed at any moment, so the
    /// decision can have moved *while this install ran*. Un-gating the tier for
    /// weights that are no longer the recorded selection would open it for a model
    /// the daemon is not going to load, and publish a terminal stage for a
    /// decision nobody holds. That is the same "another task is the authority
    /// here" shape as [`ConsentOutcome::Superseded`], so it is reported the same
    /// way — leaving the gate to the `model/set` install that now owns it.
    fn report_install_success(
        &self,
        entry: &ModelEntry,
        selection: ModelSelection,
    ) -> ConsentOutcome {
        let still_selected = self
            .store
            .current()
            .and_then(|current| current.model_name)
            .is_some_and(|name| name == entry.name);
        if !still_selected {
            return ConsentOutcome::Superseded;
        }
        if self.local_engine_present {
            self.events.publish(
                None,
                Event::ModelLifecycle(ModelLifecycle {
                    model_id: entry.name.clone(),
                    stage: ModelLifecycleStage::Ready,
                }),
            );
            ConsentOutcome::Ready { selection }
        } else {
            self.events.publish(
                None,
                Event::ModelLifecycle(ModelLifecycle {
                    model_id: entry.name.clone(),
                    stage: ModelLifecycleStage::Disabled {
                        reason: no_local_engine_reason(&entry.name),
                    },
                }),
            );
            ConsentOutcome::InstalledNoEngine {
                model_name: entry.name.clone(),
            }
        }
    }

    /// Announce a failed install on the lifecycle stream and return the outcome.
    ///
    /// AC-10's "clear network error": the reason is the [`InstallError`]'s own
    /// actionable message, which names no path and no URL (BR-11).
    fn report_install_failure(&self, entry: &ModelEntry, error: InstallError) -> ConsentOutcome {
        self.events.publish(
            None,
            Event::ModelLifecycle(ModelLifecycle {
                model_id: entry.name.clone(),
                stage: ModelLifecycleStage::Disabled {
                    reason: error.to_string(),
                },
            }),
        );
        ConsentOutcome::InstallFailed {
            model_name: entry.name.clone(),
            error,
        }
    }

    /// Broadcast `model_selection_decided` for every decision, including the ones
    /// no human answered, so an attached client always learns why the local tier
    /// is in the state it is in.
    fn announce(&self, selection: &ModelSelection, request_id: Option<RequestId>) {
        self.events.publish(
            None,
            Event::ModelSelectionDecided(ModelSelectionDecided {
                request_id,
                model_name: selection.model_name.clone(),
                declined_local: selection.declined_local,
                source: wire_source(selection.source),
            }),
        );
    }
}

/// Releases an install claim (M-2) when it drops.
///
/// A guard rather than an explicit remove at each `run_install` return so that a
/// panic in the blocking install task — or any early return — cannot leave the
/// entry marked in-flight forever, which would wedge every future install of it.
///
/// It is deliberately owned by the **blocking closure**, not by the `run_install`
/// future (E-9). The claim protects a file the installer thread is writing, and
/// that thread cannot be cancelled — so the claim has to outlive a cancelled
/// future, which it can only do from inside the work it is guarding.
struct InFlightGuard {
    installing: Arc<Mutex<HashSet<String>>>,
    name: String,
}

impl Drop for InFlightGuard {
    fn drop(&mut self) {
        if let Ok(mut installing) = self.installing.lock() {
            installing.remove(&self.name);
        }
    }
}

/// The reason a completed install still leaves the tier disabled: the weights are
/// present and verified, but this build cannot load them (M-1).
///
/// The single source of this sentence, shared with `startup_lifecycle` so the
/// install-time event and a later attach's event cannot drift into two different
/// explanations of the same state. Names the model but no path (BR-11).
#[must_use]
pub fn no_local_engine_reason(model_id: &str) -> String {
    format!(
        "{model_id}'s weights are installed and verified, but this build has no local \
         inference engine to load them; sessions run remote-only."
    )
}

/// Surface a decision-persistence failure on stderr (M-6).
///
/// The consent gate updates its in-memory record before attempting the write, so
/// a failed persist does not lose the decision for the running daemon — but it
/// *would* silently lose it across a restart, re-prompting the user forever with
/// no signal (BR-4). Reporting it is the minimum: the daemon's other
/// fallback conditions warn the same way. The [`SelectionStoreError`] message
/// names no filesystem path (BR-11), so it is safe to log.
fn report_persist_failure(what: &str, err: &SelectionStoreError) {
    eprintln!("tetond: could not persist the {what}: {err}. It holds for this daemon run but may not survive a restart.");
}

/// The probe's own sentence for a machine with no usable local tier.
fn disabled_reason(decision: &TierDecision) -> String {
    match decision {
        TierDecision::Disabled { reason } => reason.clone(),
        TierDecision::Selected { model, .. } => {
            format!("the probe selected '{model}', which is not in this daemon's catalog")
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn required_disk_adds_the_documented_margin() {
        let entry = test_entry("m", 10 * GIB, 8 * GIB, TierBand::Small);
        assert_eq!(
            required_disk_bytes(&entry),
            10 * GIB + DISK_WORKING_MARGIN_BYTES
        );
    }

    #[test]
    fn a_resolved_waiter_is_removed_and_a_duplicate_answer_is_a_noop() {
        let pending = PendingModelDecisions::new();
        let id = RequestId::from("model-0");
        let _rx = pending.register(sample_proposal(id.clone()));
        assert_eq!(pending.pending_count(), 1);
        assert_eq!(
            pending.outstanding().map(|p| p.request_id),
            Some(id.clone())
        );

        assert!(pending.resolve(&id, ModelConfirmOutcome::Accept));
        assert_eq!(pending.pending_count(), 0);
        assert!(
            pending.outstanding().is_none(),
            "an answered proposal is no longer outstanding, so a client that \
             polls afterwards is not re-prompted"
        );
        // Idempotent, exactly like `permission/respond`.
        assert!(!pending.resolve(&id, ModelConfirmOutcome::Accept));
    }

    /// The registry retains the payload a client has to *render*, not just the id
    /// it has to answer with — the difference between "there is a prompt" and
    /// "here is what it proposes" (BR-2).
    #[test]
    fn the_outstanding_proposal_is_retrievable_in_full_and_names_the_pick() {
        let pending = PendingModelDecisions::new();
        let _rx = pending.register(sample_proposal(RequestId::from("model-0")));

        let open = pending.outstanding().expect("a proposal is outstanding");
        let proposed = open.proposed.expect("the small machine gets a pick");
        assert_eq!(proposed.entry.name, "small");
        assert_eq!(proposed.entry.size_bytes, 2 * GIB);
        assert_eq!(proposed.entry.ram_floor_bytes, 8 * GIB);
        assert_eq!(
            proposed.required_disk_bytes,
            2 * GIB + DISK_WORKING_MARGIN_BYTES
        );
        assert!(open.probe.reason.contains("16.0 GiB"), "{:?}", open.probe);
        assert_eq!(
            open.alternatives
                .iter()
                .map(|e| e.name.as_str())
                .collect::<Vec<_>>(),
            vec!["big"],
            "every other entry stays selectable on the retrieved payload (BR-3)"
        );
    }

    #[test]
    fn the_probe_view_explains_the_band_in_plain_language() {
        let profile = HardwareProfile {
            ram_bytes: 16 * GIB,
            free_disk_bytes: 400 * GIB,
            gpu: GpuClass::AppleSilicon,
        };
        let catalog = test_catalog();
        let decision = decide(&profile, &catalog, None);
        let view = probe_view(&profile, &decision);

        assert_eq!(view.total_ram_bytes, 16 * GIB);
        assert_eq!(view.gpu_class, WireGpuClass::AppleSilicon);
        assert_eq!(view.chosen_band, ChosenBand::Small);
        // BR-2: the hardware reasoning, not a bare band name.
        assert!(view.reason.contains("16.0 GiB"), "reason: {}", view.reason);
        assert!(
            view.reason.contains("Apple Silicon"),
            "reason: {}",
            view.reason
        );
    }

    #[test]
    fn an_unknown_choice_and_an_unconfirmed_oversized_choice_are_refused() {
        let profile = small_machine();
        let catalog = test_catalog();

        assert!(matches!(
            validate_choice(&catalog, &profile, "nope", true),
            Err(ChoiceRefusal::UnknownModel { .. })
        ));
        assert!(matches!(
            validate_choice(&catalog, &profile, "big", false),
            Err(ChoiceRefusal::AboveRamFloor { .. })
        ));
        // BR-3: permitted, but only after the second confirmation.
        assert!(validate_choice(&catalog, &profile, "big", true).is_ok());
    }

    #[test]
    fn an_insufficient_disk_refusal_names_both_figures() {
        // AC-6's "naming required vs available" is a property of the message,
        // so it is asserted on the message rather than on the variant.
        let rendered = InstallError::InsufficientDisk {
            required_bytes: 9 * GIB,
            available_bytes: 2 * GIB,
        }
        .to_string();
        assert!(rendered.contains("9.0 GiB"), "message: {rendered}");
        assert!(rendered.contains("2.0 GiB"), "message: {rendered}");
    }

    // --- shared fixtures -------------------------------------------------

    /// The proposal a 16 GiB machine gets from [`test_catalog`].
    fn sample_proposal(request_id: RequestId) -> ModelSelectionProposed {
        let profile = small_machine();
        let catalog = test_catalog();
        let decision = decide(&profile, &catalog, None);
        build_proposal(request_id, &profile, &catalog, &decision, None, false)
    }

    pub(super) fn small_machine() -> HardwareProfile {
        HardwareProfile {
            ram_bytes: 16 * GIB,
            free_disk_bytes: 400 * GIB,
            gpu: GpuClass::AppleSilicon,
        }
    }

    pub(super) fn test_entry(
        name: &str,
        size_bytes: u64,
        ram_floor_bytes: u64,
        band: TierBand,
    ) -> ModelEntry {
        ModelEntry {
            name: name.to_owned(),
            url: format!(
                "https://models.example.com/Org/Repo/resolve/{}/{name}.gguf",
                "a".repeat(40)
            ),
            revision: "a".repeat(40),
            sha256: "b".repeat(64),
            size_bytes,
            ram_floor_bytes,
            band,
        }
    }

    pub(super) fn test_catalog() -> Catalog {
        Catalog {
            version: 1,
            models: vec![
                test_entry("small", 2 * GIB, 8 * GIB, TierBand::Small),
                test_entry("big", 40 * GIB, 64 * GIB, TierBand::Large),
            ],
        }
    }
}
