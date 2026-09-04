//! REQ-599 step 4: the local engine and the consent gate's install path.
//!
//! Probing the machine, deciding a model, building the installer and the engine
//! loader, and the slot the loaded engine lands in — `EngineSlot`,
//! `StagedEngines`, `LocalEngine`, and the two `LocalEngineLoader`
//! implementations behind their feature gates.
//!
//! Contiguous in the pre-split file (1,068 lines, items spanning 987 of them),
//! which is why it came before the provider slice: the census measured
//! `provider` scattered across 10,366 lines and this cluster across 987. ADR-4
//! said cheapest seam first, and cheapest is measured, not assumed.
//!
//! **On BR-7 and which tests came along.** Three test functions exercise this
//! code as their subject and live here: `template_fallback_line` and
//! `seam_policy` came with the original split, and
//! `only_a_scripted_engine_exempts_the_local_tier_from_the_consent_gate`
//! followed in REQ-602 TASK-304 — its subject is the scripted-engine exemption
//! defined in this module, not the gate that consults it.
//!
//! Several other tests construct an `EngineSlot` or a `StagedEngines` as a
//! *fixture* while testing something else; those stayed with the code they are
//! about. BR-7 asks that tests not be left "pointing at a module they no longer
//! describe", which is a claim about what a test describes, not about which
//! symbols it happens to name. The eight `mod.rs` tests naming `scripted` or
//! `local_tier` are that case: their subject is `Runtime`'s own tier handling,
//! and a scripted engine is how they arrange it.

use super::*;

/// The installer the consent gate hands a decided model to.
///
/// The download client is credential-free and redirect-following (D-2, TASK-002).
/// If it cannot be built at all, the daemon still runs — it just cannot install
/// weights, and says so rather than reporting them as merely absent.
///
/// Three wires matter here and each is load-bearing:
/// - `base_url` is the `[local_model] base_url` override reaching the *fetch*
///   (BR-16). The catalog's `download_url` implements the rewrite, but a
///   configured mirror that never reaches the installer redirects nothing.
/// - the fetcher is handed over twice — once as the transport, once as the
///   [`FetchCause`] the pipeline reads the precise failure back from, so a 429
///   is reported as rate-limiting rather than as a generic transport failure
///   (AC-12).
/// - `events` makes install progress observable as `model_lifecycle` (AC-2).
pub(super) fn build_installer(
    base_dir: &Path,
    base_url: Option<String>,
    events: &Arc<EventBus>,
) -> Arc<dyn WeightsInstaller> {
    match HttpRangeFetcher::with_policy(download_retry_policy()) {
        Ok(fetcher) => {
            let fetcher = Arc::new(fetcher);
            let cause: Arc<dyn FetchCause> = fetcher.clone();
            let mut install = WeightsInstall::new(
                fetcher,
                base_dir.join(teton_protocol::weights::WEIGHTS_DIR),
                base_url,
            )
            .with_cause(cause)
            .with_progress(Arc::new(LifecycleProgress::new(Arc::clone(events))));
            // AC-6's claim is about behaviour on a full volume, which no CI
            // machine will provide on demand. DECISION 3 + M-8: a test seam,
            // honoured only in a debug build with the master switch, and it may
            // only ever *lower* the measured free space — a seam that could raise
            // it would be a way to disable BR-7, so `CapFreeSpace` takes the
            // minimum of the real measurement and the ceiling.
            if let Some(ceiling) = env_u64("TETON_DISK_FREE_BYTES").filter(|_| test_seams_enabled())
            {
                install = install.with_free_space(Arc::new(CapFreeSpace {
                    inner: Arc::new(HostFreeSpace),
                    ceiling,
                }));
            }
            Arc::new(install)
        }
        Err(_) => Arc::new(NoInstaller),
    }
}

/// The download retry ladder, with only its *delays* overridable (BR-16).
///
/// The attempt count, the doubling and the jitter stay production values: a test
/// that shortened the ladder itself would be exercising a different policy than
/// the one that ships. Shortening the base delay changes how long the same ladder
/// takes, not what it does.
pub(super) fn download_retry_policy() -> RetryPolicy {
    let default = RetryPolicy::default();
    // DECISION 3: a test seam, honoured only in a debug build with the master
    // switch — never in a shipped daemon.
    match env_u64("TETON_DOWNLOAD_RETRY_BASE_MS").filter(|_| test_seams_enabled()) {
        Some(base_ms) => RetryPolicy {
            base_delay: Duration::from_millis(base_ms),
            max_delay: Duration::from_millis(base_ms.saturating_mul(8)),
            ..default
        },
        None => default,
    }
}

/// What the seam master switch means for this build (DECISION 3 / E-6).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum SeamPolicy {
    /// A debug build with the switch on: the seams are honoured.
    Honour,
    /// Nobody asked for them.
    Ignore,
    /// The switch was set in a build that cannot honour it. **Refuse loudly.**
    /// Ignoring it silently is the dangerous answer: whoever set it believes the
    /// daemon is under test control — mocked catalog, simulated hardware, capped
    /// free space — and would read the resulting run as a test result while the
    /// daemon quietly used the real catalog, the real machine, and the real
    /// network. A refusal is a fixable mistake; a silent one is a wrong answer.
    Refuse,
}

/// The policy for a build kind and the raw `TETON_TEST_SEAMS` value.
///
/// Pure so the release-build refusal is testable from a debug-build test — the
/// branch that matters is the one this binary cannot otherwise reach.
pub(super) fn seam_policy(debug_build: bool, switch: Option<&str>) -> SeamPolicy {
    match (debug_build, switch) {
        (true, Some("1")) => SeamPolicy::Honour,
        // Only the value a debug build would have honoured is a refusal; an
        // explicit `TETON_TEST_SEAMS=0` is someone turning them off, which a
        // release build is entitled to simply agree with.
        (false, Some("1")) => SeamPolicy::Refuse,
        _ => SeamPolicy::Ignore,
    }
}

/// Whether the test seams (`TETON_CATALOG`, `TETON_DISK_FREE_BYTES`,
/// `TETON_DOWNLOAD_RETRY_BASE_MS`, `TETON_PROBE_*`, `TETON_FAKE_ENGINE_LOADER`)
/// may be honoured (DECISION 3).
///
/// A **debug build with `TETON_TEST_SEAMS=1`** and nothing else. A release build
/// refuses regardless of the switch — the seams are how the acceptance suite
/// stands the daemon up against mocks, never an operator feature, so a shipped
/// binary must not honour them even if the environment sets them — and it refuses
/// *loudly* (E-6) rather than pretending it never saw the request.
///
/// # Panics
/// Panics when `TETON_TEST_SEAMS=1` is set in a release build.
pub fn test_seams_enabled() -> bool {
    match seam_policy(
        cfg!(debug_assertions),
        std::env::var("TETON_TEST_SEAMS").ok().as_deref(),
    ) {
        SeamPolicy::Honour => true,
        SeamPolicy::Ignore => false,
        SeamPolicy::Refuse => panic!(
            "teton-code: TETON_TEST_SEAMS=1 is set, but this is a release build, which cannot \
             honour the test seams (TETON_CATALOG, TETON_DISK_FREE_BYTES, \
             TETON_DOWNLOAD_RETRY_BASE_MS, TETON_PROBE_*, TETON_FAKE_ENGINE_LOADER). Refusing \
             to start rather than run as a production daemon while the environment believes \
             it is under test control. Unset TETON_TEST_SEAMS, or use a debug build."
        ),
    }
}

/// The model catalog this daemon proposes from, and whether it is a non-bundled
/// override.
///
/// `TETON_CATALOG` is a **test seam** (DECISION 3): it is honoured only when
/// [`test_seams_enabled`] is true. In a release build, or without the master
/// switch, it is ignored and its use is logged — a shipped daemon always proposes
/// from the catalog it was released with, never one an environment variable
/// swapped in. When an override IS honoured, a prominent warning is printed and
/// the returned flag drives the proposal's `fetch_notice`, so the consent screen
/// says the entries are not the shipped catalog.
///
/// An override that does not parse or does not validate falls back to the bundled
/// catalog with a warning rather than aborting startup: a mistyped path must not
/// brick a daemon, and a *silently* substituted catalog would not be a correct
/// answer, which is why the fallback is announced.
pub(super) fn load_catalog() -> (Catalog, bool) {
    let Some(path) = std::env::var_os("TETON_CATALOG") else {
        return (Catalog::bundled(), false);
    };
    if !test_seams_enabled() {
        eprintln!(
            "tetond: ignoring TETON_CATALOG — it is a test seam honoured only in a debug build \
             with TETON_TEST_SEAMS=1, not an operator feature. Using the bundled catalog."
        );
        return (Catalog::bundled(), false);
    }
    let parsed = std::fs::read_to_string(&path)
        .ok()
        .and_then(|text| Catalog::from_toml(&text).ok())
        .filter(|catalog| catalog.validate().is_ok());
    match parsed {
        Some(catalog) => {
            eprintln!(
                "tetond: WARNING — proposing from an override model catalog (TETON_CATALOG). \
                 This is a test seam, not the shipped catalog; the consent prompt will say so."
            );
            (catalog, true)
        }
        None => {
            eprintln!(
                "tetond: TETON_CATALOG did not name a readable, valid catalog; \
                 using the bundled catalog"
            );
            (Catalog::bundled(), false)
        }
    }
}

/// The result of the startup hardware probe (REQ-544 BR-9 / AC-8).
///
/// Facts only. What the *client* is told about them is
/// [`startup_lifecycle`]'s job, because the honest answer depends on state this
/// function cannot see — whether a decision has been made, whether weights are
/// on disk, and whether anything in this build can load them.
pub(super) struct ProbeResult {
    /// The local model id in force after any step-down, or `None` when disabled.
    pub(super) model: Option<String>,
    /// The model the probe itself picked, before a simulated step-down moved off
    /// it. What the `probed` stage names, because that is what was probed.
    pub(super) probed_model: Option<String>,
    /// Whether the local tier is disabled (below floor / resource-starved).
    pub(super) disabled: bool,
    /// Why the local tier is disabled, when it is — the probe's own sentence.
    pub(super) disabled_reason: Option<String>,
    /// Detected system RAM, as quoted in the `probed` stage.
    pub(super) ram_bytes: u64,
    /// Whether the machine cleared the local-tier RAM floor.
    pub(super) above_floor: bool,
    /// The `TETON_PROBE_FORCE_SLOW_BENCH` simulation, when it was asked for.
    pub(super) forced_bench: Option<ForcedBench>,
}

/// A benchmark ladder the operator explicitly asked to have *simulated*
/// (`TETON_PROBE_FORCE_SLOW_BENCH`), so REQ-544's auto-step-down duty is
/// exercisable end to end without a real model.
///
/// It is the one place a `benchmark` stage is published without a measurement,
/// and it exists only when that env flag is set: a daemon nobody asked to
/// simulate anything never emits one.
pub(super) struct ForcedBench {
    /// The model whose simulated benchmark missed the latency duty.
    pub(super) from_model: String,
    /// The smaller model it stepped down to, or `None` when nothing smaller
    /// clears the duty and the tier is disabled instead.
    pub(super) to_model: Option<String>,
}

/// Run the first-run hardware probe against `profile`.
///
/// The profile and catalog are passed in rather than resolved here so the probe
/// and the REQ-547 consent gate describe the *same* machine and the *same*
/// catalog — re-detecting would let the two disagree.
pub(super) fn probe_local_tier(
    profile: &HardwareProfile,
    catalog: &Catalog,
    pinned: Option<&str>,
) -> ProbeResult {
    let decision = decide(profile, catalog, pinned);
    let above_floor = profile.ram_bytes >= 8 * GIB;

    match decision {
        TierDecision::Disabled { reason } => ProbeResult {
            model: None,
            probed_model: None,
            disabled: true,
            disabled_reason: Some(reason),
            ram_bytes: profile.ram_bytes,
            above_floor,
            forced_bench: None,
        },
        TierDecision::Selected { model, .. } => {
            // A forced-slow micro-benchmark trips the BR-8 latency duty and
            // auto-steps-down to the next smaller catalog model (AC-8). It
            // publishes `benchmark` and `stepped_down` stages for measurements
            // that never happened, so it is a test seam like the rest (E-6) and
            // is honoured only under the master switch: a shipped daemon must not
            // be able to be told to narrate work it did not do.
            if env_flag("TETON_PROBE_FORCE_SLOW_BENCH") && test_seams_enabled() {
                let to_model = step_down_target(catalog, &model);
                return ProbeResult {
                    model: to_model.clone(),
                    probed_model: Some(model.clone()),
                    disabled: to_model.is_none(),
                    disabled_reason: to_model.is_none().then(|| {
                        "no smaller model clears the latency duty; remote-only".to_owned()
                    }),
                    ram_bytes: profile.ram_bytes,
                    above_floor,
                    forced_bench: Some(ForcedBench {
                        from_model: model,
                        to_model,
                    }),
                };
            }

            ProbeResult {
                model: Some(model.clone()),
                probed_model: Some(model),
                disabled: false,
                disabled_reason: None,
                ram_bytes: profile.ram_bytes,
                above_floor,
                forced_bench: None,
            }
        }
    }
}

/// The startup `model_lifecycle` sequence replayed to every attaching client.
///
/// **Every stage here is a claim about something that actually happened.** The
/// sequence this replaced announced `download …`, `benchmark …` and `local model
/// … ready` on every attach — before the user had answered the proposal, and on
/// a machine with no weights at all. In a daemon whose thesis is legibility that
/// is worse than saying nothing: a client cannot distinguish a real readiness
/// from a decorative one, so the honest states have to be nameable.
///
/// What this daemon can truthfully say at startup:
///
/// | State | Stage |
/// |---|---|
/// | the probe ran | `probed` (always) |
/// | below the floor / no fitting entry | `disabled`, with the probe's reason |
/// | a proposal is open, or weights are missing | `awaiting_decision` |
/// | accepted, download/install in flight | `disabled`, saying it is running |
/// | the tier was declined (BR-4) | `disabled`, saying so |
/// | weights installed and verified, load in flight or failed | `disabled`, saying which |
/// | weights installed, nothing in this build can load them | `disabled`, saying so |
/// | an engine is loaded and serving | `ready` |
///
/// Nothing here claims a download: the only `download` stages that reach a
/// client come from [`crate::install::LifecycleProgress`], which publishes bytes
/// as they actually move.
pub(super) fn startup_lifecycle(
    probe: &ProbeResult,
    serving_model: Option<String>,
    loader_present: bool,
    load_failure: Option<String>,
    consent: &ModelConsentGate,
) -> Vec<ModelLifecycle> {
    let model_id = probe
        .model
        .clone()
        .unwrap_or_else(|| LOCAL_TIER_ID.to_owned());
    let mut lifecycle = vec![ModelLifecycle {
        // The model the *probe* chose, which a simulated step-down may since have
        // moved off.
        model_id: probe
            .probed_model
            .clone()
            .unwrap_or_else(|| LOCAL_TIER_ID.to_owned()),
        stage: ModelLifecycleStage::Probed {
            ram_bytes: probe.ram_bytes,
            above_floor: probe.above_floor,
        },
    }];

    // The explicitly-requested simulation, and only when requested.
    if let Some(bench) = &probe.forced_bench {
        lifecycle.push(ModelLifecycle {
            model_id: bench.from_model.clone(),
            stage: ModelLifecycleStage::Benchmark {
                first_token_ms: 2_500,
                tokens_per_sec: 2.0,
            },
        });
        if let Some(to_model) = &bench.to_model {
            lifecycle.push(ModelLifecycle {
                model_id: bench.from_model.clone(),
                stage: ModelLifecycleStage::SteppedDown {
                    from_model: bench.from_model.clone(),
                    to_model: to_model.clone(),
                    reason: "benchmark exceeded the 1s first-token latency duty".to_owned(),
                },
            });
            lifecycle.push(ModelLifecycle {
                model_id: to_model.clone(),
                stage: ModelLifecycleStage::Benchmark {
                    first_token_ms: 600,
                    tokens_per_sec: 30.0,
                },
            });
        }
    }

    if probe.disabled {
        lifecycle.push(ModelLifecycle {
            model_id,
            stage: ModelLifecycleStage::Disabled {
                reason: probe
                    .disabled_reason
                    .clone()
                    .unwrap_or_else(|| "the local tier is unavailable on this machine".to_owned()),
            },
        });
        return lifecycle;
    }

    // An engine is loaded, the tier will serve, and the caller named the model
    // the slot actually holds: `ready` is a fact, not a hope — about that
    // model, not the probe's boot-time pick, which a `model/set` may since
    // have moved off. An engine that is live but *gated* arrives here as
    // `None` and falls through to the consent-state branches, which describe
    // the outstanding decision truthfully.
    if let Some(serving) = serving_model {
        lifecycle.push(ModelLifecycle {
            model_id: serving,
            stage: ModelLifecycleStage::Ready,
        });
        return lifecycle;
    }

    let selection = consent.current_selection();
    let declined = selection
        .as_ref()
        .is_some_and(|selection| selection.declined_local);
    let installing = selection
        .as_ref()
        .and_then(|selection| selection.model_name.as_deref())
        .is_some_and(|name| consent.install_in_flight(name));
    let stage = if declined {
        // BR-4: a settled, deliberate absence. Not a failure and not a prompt.
        ModelLifecycleStage::Disabled {
            reason: "the local tier was declined; sessions run remote-only. \
                     `teton model set <name>` changes that."
                .to_owned(),
        }
    } else if installing && consent.consent_required() {
        // Accepted, bytes in flight: `consent_required()` stays true until the
        // weights verify, so a client attaching mid-download must not be told
        // the proposal is still unanswered. Read *with* the verify state rather
        // than ahead of it (REQ-580): the load phase takes the same claim
        // (`activate_engine`), and installed-verified-and-claimed is the load
        // below, not a download that is not happening — the same precedence
        // `DaemonRuntime::local_tier_state` reads by. The stage is the
        // `disabled`-with-reason shape the in-flight *load* below already
        // uses; the live byte counts arrive separately as `download` events
        // from the installer's own progress stream.
        ModelLifecycleStage::Disabled {
            reason: installing_local_model_reason(&model_id),
        }
    } else if consent.consent_required() {
        // BR-1: proposed and unanswered, or answered but the weights are gone.
        // Nothing has been fetched, measured, or loaded, and the sequence says so.
        ModelLifecycleStage::AwaitingDecision {
            reason: "proposed for this machine — nothing is downloaded, benchmarked, or loaded \
                     until you answer; sessions run remote-only until then."
                .to_owned(),
        }
    } else if loader_present {
        // Decided, downloaded, verified, and this build CAN load the weights —
        // but the engine is not live yet. Either the startup load (deep verify →
        // load → benchmark) is still in flight, or it already failed and left
        // its reason behind. Both are "not serving right now", and each is
        // reported as itself rather than as the loaderless build's untruth.
        match load_failure {
            Some(reason) => ModelLifecycleStage::Disabled { reason },
            None => ModelLifecycleStage::Disabled {
                reason: loading_local_engine_reason(&model_id),
            },
        }
    } else {
        // Decided, downloaded, verified — and unloadable, because nothing in this
        // build constructs a local engine from installed weights (closing that
        // gap is the `llama` feature, absent from this build). Saying `ready`
        // here would be the exact untruth this function exists to stop. The
        // reason is shared with the consent gate's install-time event (M-1) so
        // the two can never drift apart.
        ModelLifecycleStage::Disabled {
            reason: no_local_engine_reason(&model_id),
        }
    };
    lifecycle.push(ModelLifecycle { model_id, stage });
    lifecycle
}

/// The hardware profile to probe: env overrides when present, else detected.
///
/// DECISION 3 / E-6: the overrides are test seams like every other, honoured only
/// under [`test_seams_enabled`]. They were the three ungated ones, and they were
/// the worst three to leave open: `ram_bytes` feeds [`validate_choice`], so a
/// `TETON_PROBE_RAM_BYTES` large enough would make every catalog entry look like
/// it fits and suppress BR-3's above-the-floor confirmation outright — while the
/// "hardware" figures the consent screen shows the user came from the environment
/// rather than the machine. A shipped daemon describes the machine it is on.
///
/// [`validate_choice`]: crate::model_consent::validate_choice
pub(super) fn probe_profile() -> HardwareProfile {
    let seams = test_seams_enabled();
    let ram = env_u64("TETON_PROBE_RAM_BYTES").filter(|_| seams);
    let disk = env_u64("TETON_PROBE_DISK_BYTES").filter(|_| seams);
    let gpu = std::env::var("TETON_PROBE_GPU").ok().filter(|_| seams);
    if !seams
        && (std::env::var_os("TETON_PROBE_RAM_BYTES").is_some()
            || std::env::var_os("TETON_PROBE_DISK_BYTES").is_some()
            || std::env::var_os("TETON_PROBE_GPU").is_some())
    {
        eprintln!(
            "tetond: ignoring TETON_PROBE_RAM_BYTES/_DISK_BYTES/_GPU — they are test seams \
             honoured only in a debug build with TETON_TEST_SEAMS=1, not operator overrides. \
             Probing the real machine."
        );
    }
    if ram.is_some() || disk.is_some() || gpu.is_some() {
        return HardwareProfile {
            ram_bytes: ram.unwrap_or(16 * GIB),
            free_disk_bytes: disk.unwrap_or(500_000 * 1_000_000),
            gpu: match gpu.as_deref() {
                Some("apple-silicon") => GpuClass::AppleSilicon,
                Some("cuda") => GpuClass::Cuda,
                _ => GpuClass::Cpu,
            },
        };
    }
    HardwareProfile::detect().unwrap_or(HardwareProfile {
        ram_bytes: 16 * GIB,
        free_disk_bytes: 500_000 * 1_000_000,
        gpu: GpuClass::Cpu,
    })
}

/// The next-smaller catalog model to step down to (by descending download size).
pub(super) fn step_down_target(catalog: &Catalog, current: &str) -> Option<String> {
    let current_entry = catalog.get(current)?;
    catalog
        .models
        .iter()
        .filter(|e| e.size_bytes < current_entry.size_bytes)
        .max_by_key(|e| e.size_bytes)
        .map(|e| e.name.clone())
}

/// Whether the local tier starts out **withheld** pending a decision (BR-1 / E-5).
///
/// Two inputs, one rule: the tier is withheld while a consent decision is
/// outstanding, and the *only* exemption is a scripted engine — canned replies
/// from a file, which download nothing, so there is nothing to consent to.
///
/// Named and separated because the expression used to be
/// `engine.is_none() && consent.consent_required()`, which is the same thing only
/// while the scripted engine is the *sole* engine this build can construct. A
/// real weights-loading engine is not an exemption; it is precisely the case the
/// gate exists for, and the old spelling would have opened the tier for it
/// unconditionally — while `first_run_consent_applies()`, keyed the same way,
/// stopped the consent flow (and its deep verification) from ever running.
pub(super) fn local_tier_gated(scripted_engine: bool, consent_required: bool) -> bool {
    consent_required && !scripted_engine
}

/// The daemon's one engine slot, shared between the runtime's serving path and
/// the consent flow's post-verify loader.
///
/// A scripted engine occupies it from construction; a real weights engine
/// arrives whenever the loader finishes — possibly minutes into the run, after
/// an accepted install. The slot also remembers a failed load's reason, so the
/// lifecycle replay can tell an attaching client what actually happened rather
/// than guessing between "still loading" and "failed".
/// A live engine tagged with the model id it serves.
pub(super) type TaggedEngine = (String, Arc<Mutex<dyn Engine>>, ChatFormat);

pub(super) struct EngineSlot {
    /// The live engine, tagged with the model it serves. The tag is what lets a
    /// superseded flow evict **its own** engine without ever being able to evict
    /// a successor's ([`Self::remove_if`]), and what lets the lifecycle replay
    /// name the model actually loaded rather than the probe's boot-time pick.
    pub(super) engine: Mutex<Option<TaggedEngine>>,
    pub(super) load_failure: Mutex<Option<String>>,
}

impl EngineSlot {
    /// An empty slot.
    pub(super) fn empty() -> Arc<Self> {
        Arc::new(Self {
            engine: Mutex::new(None),
            load_failure: Mutex::new(None),
        })
    }

    /// Make `engine` the live engine serving `model_id`, clearing any recorded
    /// load failure.
    ///
    /// The engine's [`ChatFormat`] is read HERE, in this sync context, and
    /// stored beside the handle: at install time the engine is not yet shared
    /// (nothing else can hold its mutex), so the lock is uncontended by
    /// construction. Async turn paths then read the format from the slot
    /// instead of the engine — locking the serving mutex for metadata on the
    /// async path would park a tokio worker behind an in-flight completion
    /// (LESSON-448, REQ-554 verify).
    pub(super) fn install(&self, model_id: String, engine: Arc<Mutex<dyn Engine>>) {
        let format = engine
            .lock()
            .expect("engine mutex poisoned at install")
            .chat_format();
        *self
            .load_failure
            .lock()
            .expect("load-failure mutex poisoned") = None;
        *self.engine.lock().expect("engine slot mutex poisoned") = Some((model_id, engine, format));
    }

    /// The live engine and the [`ChatFormat`] it was installed with, if any —
    /// the lock-free-for-metadata surface the async turn path uses.
    pub(super) fn get_with_format(&self) -> Option<(Arc<Mutex<dyn Engine>>, ChatFormat)> {
        self.engine
            .lock()
            .expect("engine slot mutex poisoned")
            .as_ref()
            .map(|(_, engine, format)| (Arc::clone(engine), *format))
    }

    /// The model the live engine serves, if one is live.
    pub(super) fn model(&self) -> Option<String> {
        self.engine
            .lock()
            .expect("engine slot mutex poisoned")
            .as_ref()
            .map(|(id, _, _)| id.clone())
    }

    /// Whether an engine is live.
    pub(super) fn present(&self) -> bool {
        self.engine
            .lock()
            .expect("engine slot mutex poisoned")
            .is_some()
    }

    /// Record why a load attempt left the slot empty.
    ///
    /// Single writer: [`DaemonRuntime::apply_consent_outcome`], on an
    /// `EngineLoadFailed` outcome. Recording at the outcome rather than inside
    /// the loader covers every failure shape the same way — a load error, a
    /// failed duty, and a loader that panicked (whose own recording code never
    /// ran) — so the replay can never claim "still loading" for a load that
    /// terminally failed.
    pub(super) fn record_load_failure(&self, reason: String) {
        *self
            .load_failure
            .lock()
            .expect("load-failure mutex poisoned") = Some(reason);
    }

    /// The recorded reason the last load attempt failed, if one did.
    pub(super) fn load_failure(&self) -> Option<String> {
        self.load_failure
            .lock()
            .expect("load-failure mutex poisoned")
            .clone()
    }
}

/// The staging bay every [`crate::model_consent::LocalEngineLoader`] in this
/// module shares: loaded-and-measured engines keyed by model, in front of the
/// daemon's one serving slot.
///
/// Staging is per-model so concurrent flows for different models can never
/// clobber each other's staged engines, and [`Self::commit`] is the ONLY path
/// from "staged" to "serving" — it goes through [`EngineSlot::install`] on the
/// runtime's real slot. Shared between the real [`LlamaEngineLoader`] and the
/// seam's [`FakeEngineLoader`] so `ready`'s tier-opening fact
/// ([`EngineSlot::present`]) is established by the same code in production and
/// in the acceptance suite — a seam with its own private commit path would
/// leave the production one exercised only in a dogfood run.
pub(super) struct StagedEngines {
    pub(super) slot: Arc<EngineSlot>,
    /// Loaded-and-measured engines awaiting the gate's commit/abandon verdict,
    /// each with the template-fallback reason its loader captured (`None` for a
    /// recognized template — and for test doubles, which are flat by design,
    /// not degraded).
    pub(super) staged: Mutex<HashMap<String, StagedEntry>>,
}

/// A staged engine and the template-fallback reason captured at load time.
pub(super) type StagedEntry = (Arc<Mutex<dyn Engine>>, Option<&'static str>);

/// The user-visible template-downgrade report (REQ-554 BR-2/AC-3), as a pure
/// function so its shape is pinned by a default-build unit test even though
/// the emitting path is `llama`-gated. Carries the model and the CAUSE
/// (LESSON-456 — a downgrade report that names no reason tells the user
/// something happened but not what to do about it); never prompt content.
pub(super) fn template_fallback_line(model_name: &str, reason: &str) -> String {
    format!("tetond: model {model_name}: {reason}; using flat transcript rendering")
}

impl StagedEngines {
    /// An empty staging bay in front of `slot`.
    pub(super) fn new(slot: Arc<EngineSlot>) -> Self {
        Self {
            slot,
            staged: Mutex::new(HashMap::new()),
        }
    }

    /// Hold `engine` as `model_name`'s staged engine — measured, not serving —
    /// with the loader-captured template-fallback reason, if any.
    pub(super) fn stage(
        &self,
        model_name: &str,
        engine: Arc<Mutex<dyn Engine>>,
        template_note: Option<&'static str>,
    ) {
        self.staged
            .lock()
            .expect("staged map poisoned")
            .insert(model_name.to_owned(), (engine, template_note));
    }

    /// Make `model_name`'s staged engine live in the serving slot. A no-op when
    /// nothing is staged under that name.
    ///
    /// The template-downgrade report is emitted HERE, not at stage time
    /// (REQ-554 verify): a staged engine can still be abandoned by the
    /// authority re-check (LESSON-445), and a report for an engine that never
    /// serves would be false. Commit is the moment the downgrade becomes true
    /// of the serving tier — once per engine that actually goes live.
    pub(super) fn commit(&self, model_name: &str) {
        let staged = self
            .staged
            .lock()
            .expect("staged map poisoned")
            .remove(model_name);
        if let Some((engine, template_note)) = staged {
            if let Some(reason) = template_note {
                eprintln!("{}", template_fallback_line(model_name, reason));
            }
            self.slot.install(model_name.to_owned(), engine);
        }
    }

    /// Discard `model_name`'s staged engine, if any — never anything live.
    pub(super) fn abandon(&self, model_name: &str) {
        self.staged
            .lock()
            .expect("staged map poisoned")
            .remove(model_name);
    }
}

/// The explanation for a tier whose accepted install is still in flight: the
/// answer exists, the bytes are moving, and the tier opens on its own once
/// they verify and load. Distinct from the unanswered-proposal sentence on
/// purpose — telling a user who just said yes that they "have not answered"
/// reads as their accept having been lost. Names the model but no path
/// (BR-11).
pub(super) fn installing_local_model_reason(model_id: &str) -> String {
    format!(
        "{model_id} was accepted and its download/install is running now — \
         the local tier opens when it completes; `teton model status` shows \
         progress."
    )
}

/// The replay-time explanation for verified weights whose load has not finished:
/// the startup flow (deep verify → load → benchmark) is still in flight. Names
/// the model but no path (BR-11).
pub(super) fn loading_local_engine_reason(model_id: &str) -> String {
    format!(
        "{model_id}'s weights are installed and verified; the daemon is loading and \
         benchmarking them now — the local tier opens when that completes."
    )
}

/// A constructed local engine, and what kind of engine it is (E-5).
///
/// The kind travels with the engine because the consent flow's one exemption is
/// about the *kind* — a scripted engine downloads nothing — and inferring it from
/// "an engine exists" silently becomes wrong the day a real GGUF loader lands.
pub(super) struct LocalEngine {
    /// The model id the engine serves (the slot's tag).
    pub(super) model_id: String,
    /// The engine the router will call.
    pub(super) engine: Arc<Mutex<dyn Engine>>,
    /// Whether it replays canned replies from `TETON_LOCAL_SCRIPT` rather than
    /// loading weights the daemon would have had to download.
    pub(super) scripted: bool,
}

/// Build the local engine when a scripted engine is configured and the probe did
/// not disable the tier.
///
/// A real weights-loading engine is deliberately NOT constructed here: it enters
/// through the consent flow's post-verify loader (`build_engine_loader`), so its
/// bytes are digest-verified before the GGUF parser ever sees them — and so the
/// consent flow and its deep verification stay switched on for it (E-5).
pub(super) fn build_local_engine(probe: &ProbeResult) -> Option<LocalEngine> {
    if probe.disabled {
        return None;
    }
    let script = std::env::var_os("TETON_LOCAL_SCRIPT")?;
    let model_id = probe
        .model
        .clone()
        .unwrap_or_else(|| "scripted-local".to_owned());
    let engine = ScriptedFileEngine::from_file(model_id.clone(), Path::new(&script)).ok()?;
    Some(LocalEngine {
        model_id,
        engine: Arc::new(Mutex::new(engine)) as Arc<Mutex<dyn Engine>>,
        scripted: true,
    })
}

/// Build the weights loader this build carries, or `None` when it carries none.
///
/// The `llama` feature is what makes verified installed bytes loadable at all;
/// without it there is nothing to construct, and the consent gate's loaderless
/// default keeps publishing the honest `disabled` after an install. A scripted
/// tier also gets no loader: its engine is already live, and the consent flow —
/// the only caller of a loader — does not apply to it (E-5). Neither condition
/// feeds a gate: the gate stays keyed on `scripted_engine` and the consent
/// state alone (LESSON-443).
#[cfg(feature = "llama")]
pub(super) fn build_engine_loader(
    slot: &Arc<EngineSlot>,
    profile: &HardwareProfile,
    base_dir: &Path,
    scripted_engine: bool,
) -> Option<Arc<dyn crate::model_consent::LocalEngineLoader>> {
    if scripted_engine {
        return None;
    }
    Some(Arc::new(LlamaEngineLoader {
        staged: StagedEngines::new(Arc::clone(slot)),
        base_dir: base_dir.to_owned(),
        gpu: profile.gpu,
    }))
}

/// The loaderless build: no `llama` feature, nothing can load a GGUF.
#[cfg(not(feature = "llama"))]
pub(super) fn build_engine_loader(
    _slot: &Arc<EngineSlot>,
    _profile: &HardwareProfile,
    _base_dir: &Path,
    _scripted_engine: bool,
) -> Option<Arc<dyn crate::model_consent::LocalEngineLoader>> {
    None
}

/// The measurement [`FakeEngineLoader`] reports, fixed so the acceptance suite
/// can assert the published `benchmark` stage carries **this loader's** figures
/// — not a real measurement, not a default — while sitting safely inside the
/// BR-8 duty so the flow reaches `ready`.
pub(super) const FAKE_LOADER_FIRST_TOKEN_MS: u32 = 42;
/// See [`FAKE_LOADER_FIRST_TOKEN_MS`].
pub(super) const FAKE_LOADER_TOKENS_PER_SEC: f32 = 512.5;

/// The `TETON_FAKE_ENGINE_LOADER` seam's loader: a [`MockEngine`] behind the
/// same [`StagedEngines`] stage → re-check → commit path as the real loader,
/// against the runtime's real serving slot.
///
/// What it fakes is deliberately minimal — the GGUF parse and the measurement.
/// Everything downstream is the production machinery: the gate's supersede
/// re-check, the staged-not-live discipline, [`EngineSlot::install`], and
/// `ready` opening the tier on the slot's own fact. That is the point of the
/// seam: the cross-process suite can otherwise never watch an accepted install
/// proceed past `verified`, because the default build carries no loader and a
/// scripted engine skips the consent flow entirely.
pub(super) struct FakeEngineLoader {
    pub(super) staged: StagedEngines,
}

impl crate::model_consent::LocalEngineLoader for FakeEngineLoader {
    fn load(&self, model_name: &str) -> Result<crate::model_consent::EngineLoadReport, String> {
        // REQ-580: a real load takes tens of seconds; this one takes none, and a
        // suite that wants a prompt to land *inside* the load has no window to
        // land it in. The delay is opt-in, honest work on the blocking pool
        // (this is where `activate_engine` runs the loader), and gated by the
        // fake loader's own gate — nothing constructs this type outside it.
        if let Some(ms) = env_u64("TETON_FAKE_ENGINE_LOADER_DELAY_MS") {
            std::thread::sleep(Duration::from_millis(ms));
        }
        let benchmark = BenchmarkResult {
            first_token_ms: FAKE_LOADER_FIRST_TOKEN_MS,
            tokens_per_sec: FAKE_LOADER_TOKENS_PER_SEC,
        };
        // The judgement is the real duty applied to the fake figures, so the
        // gate downstream sees the same shape a real loader hands it.
        let duty = DutySpec::default().evaluate(&benchmark);
        if duty.is_pass() {
            // No template note: a test double is flat by design, not degraded —
            // the downgrade report is for real models only (REQ-554 AC-3).
            self.staged.stage(
                model_name,
                Arc::new(Mutex::new(MockEngine::new(model_name))) as Arc<Mutex<dyn Engine>>,
                None,
            );
        }
        Ok(crate::model_consent::EngineLoadReport { benchmark, duty })
    }

    fn commit(&self, model_name: &str) {
        self.staged.commit(model_name);
    }

    fn abandon(&self, model_name: &str) {
        self.staged.abandon(model_name);
    }
}

/// Build the `TETON_FAKE_ENGINE_LOADER` stand-in loader when the seam is set
/// and honoured, or `None` to fall through to the loader the build carries.
///
/// A **gated test seam** (DECISION 3), honoured only under
/// [`test_seams_enabled`]: a fabricated "engine loaded and passed its
/// benchmark" is exactly the class of fiction the master switch exists to
/// fence off, so a release build refuses the master switch outright and a
/// build without the switch declines this request loudly rather than
/// silently. A scripted tier gets no loader here for the same reason it gets
/// no real one: its engine is already live and the consent flow — the only
/// caller of a loader — does not apply to it (E-5).
pub(super) fn fake_engine_loader(
    slot: &Arc<EngineSlot>,
    scripted_engine: bool,
) -> Option<Arc<dyn crate::model_consent::LocalEngineLoader>> {
    if !env_flag("TETON_FAKE_ENGINE_LOADER") {
        return None;
    }
    if !test_seams_enabled() {
        eprintln!(
            "tetond: ignoring TETON_FAKE_ENGINE_LOADER — it is a test seam honoured only in a \
             debug build with TETON_TEST_SEAMS=1, not an operator feature. The daemon keeps \
             whatever weights loader this build actually carries."
        );
        return None;
    }
    if scripted_engine {
        return None;
    }
    Some(Arc::new(FakeEngineLoader {
        staged: StagedEngines::new(Arc::clone(slot)),
    }))
}

/// Generation context window for the local tier's engine, in **BPE tokens**.
///
/// **This constant is the source, not the consequence (REQ-590).** It used to
/// be sized *to cover* a separately-chosen harness budget — 4,096 whitespace
/// words times a worst-case ~4× BPE expansion, plus generation headroom. That
/// dependency now runs the other way: the harness budget is derived **from**
/// this number, so a reader who arrives here looking for the ~4× of margin the
/// old wording promised will not find it.
///
/// # 32,768, raised from 16,384
///
/// At 16,384 the local byte budget was a fixed 32,768 B (REQ-590 ADR-9), and a
/// `/analyze` turn whose dynamic context came to **55 KB** was refused against
/// it while the window would have held it comfortably at real BPE density. A
/// repository audit is a baseline task for the local tier, so the window is
/// doubled and **both** halves of the local pair now derive from it — the byte
/// half's fixed constant existed only because the window-derived figure was
/// *smaller* at 16,384, and at 32,768 it is 1.94× the constant instead (see
/// [`derive`](crate::harness::budget::derive)'s local arm).
///
/// Every catalogued model holds this natively: Qwen2.5-Coder's 1.5B/3B/7B
/// weights carry a 32,768 position table and Qwen3-Coder-30B-A3B a 262,144 one,
/// so no RoPE scaling is asked for. What it costs is KV: roughly double the
/// per-context cache (≈3 GiB per context on the 30B-A3B Q4_K at fp16 KV, ≈1.75
/// GiB on the 7B, ≈1.1 GiB on the 3B, ≈0.9 GiB on the 1.5B), and — because
/// prefill is super-linear (REQ-590 TASK-275 measured 4.35× the time for 2.5×
/// the tokens) — a turn that genuinely *fills* the window waits longer for its
/// first token. The budget is a ceiling: a turn pays only for what it sends.
///
/// # What derives from it
///
/// * The local route's **word** budget, in
///   [`derive`](crate::harness::budget::derive)'s local arm: `32,768 − 1,024
///   reserved for the reply = 31,744 usable`, then the same 3/2 rule every
///   window-derived route runs, giving **21,162 whitespace words** (the 2/3 is
///   integer division; the one token it leaves is truncation, not headroom).
///   That is saturating and carries no deliberate slack (ADR-6): any content
///   denser than 1.5 real BPE tokens per whitespace word overruns the engine at
///   full budget, with the engine's own typed `context_length_exceeded` as the
///   only catch.
/// * The local route's **byte** budget, on the same arm: `31,744 × 2 B/token =
///   63,488 B`. Both halves now bridge to exactly the usable window, so a
///   prompt saturated in either currency claims what the engine has, not more.
/// * [`COMPACT_OUTPUT_MAX_BYTES`](crate::harness::compact::COMPACT_OUTPUT_MAX_BYTES)
///   and [`COMPACT_PROMPT_BUDGET_BYTES`](crate::harness::compact::COMPACT_PROMPT_BUDGET_BYTES)
///   — the `compact` duty's output ceiling and its own prompt bound.
/// * [`REDACT_CHUNK_MAX_BYTES`](crate::egress::redact::REDACT_CHUNK_MAX_BYTES)
///   and `REDACT_PROMPT_BUDGET_BYTES`, on the same chain — and through the
///   chunk cap, the scan's total cap and every `[privacy] redact = true`
///   route's byte budget.
///
/// # So lowering it is not a local edit (BR-8, OQ-2)
///
/// Every number above moves with it, and the rules that keep those moves safe
/// live in `harness/budget.rs`, not here:
///
/// * **BR-8** — `MIN_BUDGET_*` may never raise the local pair above what this
///   window holds. The floor's "only ever raises" property is safe against a
///   provider's *declaration*, which the provider can refuse in words a user
///   reads, and is not safe against this **allocation**, where a budget above
///   the window buys nothing a turn can spend. Latent at 32,768; live the
///   moment this constant falls. `budget::Floor::HeldToTheEngine` is where it
///   is enforced and tested at a synthetic window.
/// * **OQ-2, open.** At 4,096 tokens `window_pair` gives 6,144 bytes, below
///   the harness's own ~6 KB system prompt plus the 1 KiB truncation floor,
///   i.e. a tier that cannot serve anything — and since both halves derive
///   again there is no constant to fall back on. `HeldToTheEngine` refuses to
///   raise it, by design. Read `derive`'s local arm and OQ-2 together before
///   changing this line downward.
///
/// The harness bounds its side in this window's currency as well as in words:
/// the assembled context and the summarizer's input are capped in **bytes**
/// (`HarnessConfig::context_budget_bytes`), so pathological content (a minified
/// single-line file) is clamped or mechanically truncated instead of reaching
/// the engine over-window. What that history cost is worth keeping: a folded
/// `read` of a real file killed the first dogfooded turn with an opaque "local
/// engine could not serve the turn", and that failure now carries the engine's
/// own over-window sentence (BUG-146). The typed backend error is the backstop,
/// never the expected path.
///
/// ## Not feature-gated, because other consumers derive from it
///
/// `LlamaEngine::load` is the only *caller*, and it exists only under
/// `--features tetond/llama`. But [`REDACT_CHUNK_MAX_BYTES`](crate::egress::redact::REDACT_CHUNK_MAX_BYTES)
/// is **derived** from this number in every build (REQ-562, LESSON-446): the
/// scan's per-chunk cap and this window are two descriptions of one budget,
/// and they were once picked independently — 64 KiB against a window that
/// refused anything over 30,720 bytes — so payloads in the ~30–64 KiB band
/// passed the cap and then failed as an opaque engine error, blocking with
/// the wrong reason. One number, one place; the scan's total cap
/// (`REDACT_INPUT_MAX_BYTES`) is in turn a stated multiple of the chunk cap.
/// ## Why this is a *default* since REQ-616 (ADR-616-1)
///
/// The name gained its suffix when the local window became a runtime fact. The
/// constant used to do two jobs, and only the first was about a real engine:
/// the `n_ctx` handed to `LlamaEngine::load`, and the compile-time basis for
/// the two derived byte caps and [`derive`](crate::harness::budget::derive)'s
/// local arm — the second evaluated in **every** build, including every CI
/// build, none of which compile llama.cpp.
///
/// Deleting the constant in favour of a runtime lookup would have left that
/// second job with no value to compute from in exactly the builds that do all
/// the testing, and every assertion derived from it either rewritten or
/// vacuous (LESSON-569, LESSON-598). So it keeps the second job and says what
/// it now is: the window used when **no real engine is loaded**, which is the
/// `MockEngine` path and all of CI. A loaded engine's window travels as
/// `BudgetInputs::local_window`.
///
/// The property that follows is the one that made REQ-616 tractable in a single
/// pass: **CI's window is still 32,768, so every existing assertion on
/// 21,162 / 63,488 / 184,265 keeps its number**, and the 262,144 path is
/// exercised by tests that pass an explicit window.
pub(crate) const LOCAL_ENGINE_N_CTX_DEFAULT: u32 = 32_768;

/// The real weights loader: llama.cpp behind the [`Engine`] trait (AC-2).
///
/// Called by the consent gate only after digest verification, on the blocking
/// pool. Loads the GGUF from the shared install path convention, runs the BR-8
/// micro-benchmark, and **stages** the duty-passing engine per model; the gate
/// makes it live (`commit`) only after its post-load supersede re-check, or
/// discards it (`abandon`). Staging is a per-model map so concurrent flows for
/// different models can never clobber each other's staged engines, and only a
/// committed flow ever touches the serving slot.
#[cfg(feature = "llama")]
pub(super) struct LlamaEngineLoader {
    staged: StagedEngines,
    base_dir: PathBuf,
    gpu: GpuClass,
}

/// Strip any rendering of `path` out of a third-party error message (BR-11).
///
/// llama-cpp-2's load errors can echo the path they were given (e.g. its
/// non-UTF-8 `PathToStrError` displays the full `PathBuf`), and this message is
/// published on the event bus and memoized for replay — a resolved weights path
/// must never ride either. Both the plain and the `Debug`-quoted renderings are
/// scrubbed.
#[cfg(feature = "llama")]
pub(super) fn without_path(message: &str, path: &Path) -> String {
    message
        .replace(&format!("{path:?}"), "<weights file>")
        .replace(&path.display().to_string(), "<weights file>")
}

#[cfg(feature = "llama")]
impl crate::model_consent::LocalEngineLoader for LlamaEngineLoader {
    fn load(&self, model_name: &str) -> Result<crate::model_consent::EngineLoadReport, String> {
        use teton_inference::{default_prompts, run_benchmark, DutySpec, LlamaEngine};

        let path = teton_protocol::weights::weights_path(&self.base_dir, model_name);
        // Offload every layer on a GPU-classed machine (Metal / CUDA); CPU-only
        // machines run all layers on the CPU.
        let gpu_layers = match self.gpu {
            GpuClass::AppleSilicon | GpuClass::Cuda => u32::MAX,
            GpuClass::Cpu => 0,
        };
        let engine = LlamaEngine::load(model_name, &path, gpu_layers, LOCAL_ENGINE_N_CTX_DEFAULT)
            .map_err(|e| {
            format!(
                "{model_name}'s weights could not be loaded: {}",
                without_path(&e.to_string(), &path)
            )
        })?;

        let benchmark = run_benchmark(&engine, &default_prompts(), &GenParams::default())
            .map_err(|e| format!("{model_name} loaded but failed its benchmark: {e}"))?;
        let duty = DutySpec::default().evaluate(&benchmark);

        // A passing engine is STAGED, not made live: the gate re-checks the
        // recorded decision after this returns and only then commits. A failing
        // one is dropped here (unmapping the weights); the failure memo is
        // recorded by `apply_consent_outcome` from the outcome this becomes.
        if duty.is_pass() {
            // REQ-554 BR-2/AC-3: a model whose GGUF carries no template this
            // build recognizes serves on the flat transcript rendering, and
            // that downgrade is reported — once, naming the model — never
            // silently (LESSON-447: a best-effort fallback must fail loudly, or
            // the tier quietly runs on the format that produced BUG-147).
            //
            // The reason is CAPTURED here — the last point the loader holds the
            // concrete `LlamaEngine` — but the report itself is emitted at
            // `commit` (REQ-554 verify): a staged engine can still be abandoned
            // by the authority re-check (LESSON-445), and a downgrade report
            // for an engine that never serves would be false. Test doubles
            // stage with no note (flat by design, not degraded); scripted
            // engines reach no loader at all (E-5).
            let template_note = engine.template_fallback_reason();
            self.staged.stage(
                model_name,
                Arc::new(Mutex::new(engine)) as Arc<Mutex<dyn Engine>>,
                template_note,
            );
        }
        Ok(crate::model_consent::EngineLoadReport { benchmark, duty })
    }

    fn commit(&self, model_name: &str) {
        self.staged.commit(model_name);
    }

    fn abandon(&self, model_name: &str) {
        self.staged.abandon(model_name);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // REQ-602 TASK-304: moved here with its subject. It calls `local_tier_gated`
    // four times and nothing else — the module's own header already records
    // which tests deliberately stayed in `mod.rs` as fixtures; this one was not
    // one of them, it was simply left behind (BR-7).

    #[test]
    fn the_template_fallback_line_names_the_model_and_the_cause() {
        // REQ-554 BR-2/AC-3: the downgrade report's shape is pinned here even
        // though its emitting path is `llama`-gated — and it carries the CAUSE,
        // not a fixed sentence (LESSON-456). No prompt content ever rides it.
        let line = template_fallback_line(
            "qwen3-coder-30b-a3b",
            "no chat template in the GGUF metadata",
        );
        assert_eq!(
            line,
            "tetond: model qwen3-coder-30b-a3b: no chat template in the GGUF \
         metadata; using flat transcript rendering"
        );
    }

    /// DECISION 3 / E-6: the master switch is a debug-build affordance, and a
    /// release build asked to honour it must **refuse**, not quietly ignore it.
    #[test]
    fn the_seam_master_switch_is_debug_only_and_refuses_loudly_in_a_release_build() {
        assert_eq!(seam_policy(true, Some("1")), SeamPolicy::Honour);
        assert_eq!(seam_policy(true, None), SeamPolicy::Ignore);
        assert_eq!(seam_policy(true, Some("0")), SeamPolicy::Ignore);
        assert_eq!(seam_policy(true, Some("yes")), SeamPolicy::Ignore);
        // The branch a debug-build test cannot otherwise reach: whoever set this
        // believes the daemon is running against mocks, simulated hardware and a
        // capped volume. Ignoring them silently means they read a production run
        // as a test result.
        assert_eq!(seam_policy(false, Some("1")), SeamPolicy::Refuse);
        // Turning the seams off explicitly is not a mistake to refuse over.
        assert_eq!(seam_policy(false, Some("0")), SeamPolicy::Ignore);
        assert_eq!(seam_policy(false, None), SeamPolicy::Ignore);
    }

    /// E-5: the consent gate must not switch itself off the moment a real engine
    /// appears — which is exactly when downloading weights starts to mean
    /// something.
    #[test]
    fn only_a_scripted_engine_exempts_the_local_tier_from_the_consent_gate() {
        // The ordinary first run on a production build: withheld until answered.
        assert!(local_tier_gated(false, true));
        // Decided and installed: open.
        assert!(!local_tier_gated(false, false));
        // A `TETON_LOCAL_SCRIPT` engine fetches nothing, so it is never gated.
        assert!(!local_tier_gated(true, true));
        assert!(!local_tier_gated(true, false));
        // And the regression this pins: a build that HAS a weights-loading engine
        // (`scripted_engine == false`) and an outstanding decision is withheld.
        // The old `engine.is_none() && …` spelling made that case un-gated.
        assert!(
            local_tier_gated(false, true),
            "a real engine must not un-gate the tier before the user has decided"
        );
    }
}
