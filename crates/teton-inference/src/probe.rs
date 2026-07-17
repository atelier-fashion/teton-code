//! Hardware probe and the OQ-3 tier decision table (BR-9).
//!
//! [`HardwareProfile::detect`] reads the machine's RAM, free disk, and GPU
//! class; [`decide`] maps a profile plus the catalog to a concrete model choice
//! (or the cleanly-disabled tier). The decision is a pure function of its inputs
//! so it can be table-tested against simulated profiles — the runtime detection
//! is deliberately factored out so tests never depend on the host machine.
//!
//! ## Decision table (OQ-3, reconciled with AC-8)
//!
//! | Total RAM        | Band  | Default pick     |
//! |------------------|-------|------------------|
//! | `< 8 GiB`        | —     | disabled (remote-only) |
//! | `8..=16 GiB`     | small | ≤3B  (AC-8: a 16 GiB machine selects ≤3B) |
//! | `16 < r <= 32`   | mid   | 7B               |
//! | `> 32 GiB`       | large | 30B-A3B (optional) |
//!
//! The upper bounds at 16 and 32 GiB are **inclusive** so that AC-8's explicit
//! "16 GiB → ≤3B" holds and a 32 GiB machine lands in the 7B band. Within a
//! band, [`decide`] picks the largest model that fits both RAM and free disk,
//! stepping down to a smaller model (or to disabled) when disk is tight.

use crate::catalog::{Catalog, ModelEntry, TierBand};
use crate::lifecycle::LifecycleEvent;

/// One binary gibibyte.
pub const GIB: u64 = 1024 * 1024 * 1024;
/// The hardware floor: below this the local tier is disabled entirely.
pub const FLOOR_RAM_BYTES: u64 = 8 * GIB;
/// Inclusive upper bound of the small band.
pub const SMALL_MAX_RAM_BYTES: u64 = 16 * GIB;
/// Inclusive upper bound of the mid band.
pub const MID_MAX_RAM_BYTES: u64 = 32 * GIB;

/// The GPU acceleration class of the machine.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum GpuClass {
    /// Apple Silicon unified memory + Metal (the MVP first-class target).
    AppleSilicon,
    /// An NVIDIA CUDA GPU.
    Cuda,
    /// No supported accelerator; CPU inference only.
    Cpu,
}

/// A snapshot of the host hardware relevant to model selection.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct HardwareProfile {
    /// Total physical RAM in bytes.
    pub ram_bytes: u64,
    /// Free disk space (for the model download) in bytes.
    pub free_disk_bytes: u64,
    /// GPU acceleration class.
    pub gpu: GpuClass,
}

impl HardwareProfile {
    /// Best-effort detection of the host profile.
    ///
    /// Uses platform facilities (`sysctl hw.memsize` / `/proc/meminfo` for RAM,
    /// `df` for free disk) with no extra dependency. Detection is not the tested
    /// surface — [`decide`] is — so any detection failure surfaces as an error
    /// the daemon can degrade on rather than a panic.
    ///
    /// # Errors
    /// Returns an [`std::io::Error`] if RAM or disk could not be determined.
    pub fn detect() -> std::io::Result<Self> {
        Ok(Self {
            ram_bytes: detect_ram_bytes()?,
            free_disk_bytes: detect_free_disk_bytes(".")?,
            gpu: detect_gpu_class(),
        })
    }
}

/// The band a machine with `ram_bytes` qualifies for, or `None` below the floor.
#[must_use]
pub fn band_for_ram(ram_bytes: u64) -> Option<TierBand> {
    if ram_bytes < FLOOR_RAM_BYTES {
        None
    } else if ram_bytes <= SMALL_MAX_RAM_BYTES {
        Some(TierBand::Small)
    } else if ram_bytes <= MID_MAX_RAM_BYTES {
        Some(TierBand::Mid)
    } else {
        Some(TierBand::Large)
    }
}

/// The outcome of the probe decision.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TierDecision {
    /// A model was selected.
    Selected {
        /// The chosen model's catalog name.
        model: String,
        /// The hardware band the machine qualified for.
        band: TierBand,
        /// Whether this came from a user pin (overriding the probe) rather than
        /// the decision table.
        pinned: bool,
    },
    /// The local tier is disabled; sessions run remote-only.
    Disabled {
        /// User-facing reason.
        reason: String,
    },
}

impl TierDecision {
    /// The selected model name, or `None` when disabled.
    #[must_use]
    pub fn model(&self) -> Option<&str> {
        match self {
            TierDecision::Selected { model, .. } => Some(model),
            TierDecision::Disabled { .. } => None,
        }
    }

    /// Whether the local tier is disabled.
    #[must_use]
    pub fn is_disabled(&self) -> bool {
        matches!(self, TierDecision::Disabled { .. })
    }
}

/// Decide the local-model tier for `profile` against `catalog`.
///
/// A user pin (`pinned`) that names a known catalog model always wins, per BR-9
/// ("User-pinned model choice in config always overrides the probe"). An unknown
/// pin is ignored and the probe proceeds. Otherwise the machine's RAM picks a
/// band, and the largest model in that band (or below) that fits RAM *and* free
/// disk is chosen; if nothing fits, the tier is disabled.
#[must_use]
pub fn decide(profile: &HardwareProfile, catalog: &Catalog, pinned: Option<&str>) -> TierDecision {
    // BR-9: an explicit, valid pin overrides the probe unconditionally.
    if let Some(name) = pinned {
        if let Some(model) = catalog.get(name) {
            return TierDecision::Selected {
                model: model.name.clone(),
                band: model.band,
                pinned: true,
            };
        }
    }

    let Some(band) = band_for_ram(profile.ram_bytes) else {
        return TierDecision::Disabled {
            reason: format!(
                "{:.1} GiB of RAM is below the {} GiB local-tier floor; running remote-only",
                profile.ram_bytes as f64 / GIB as f64,
                FLOOR_RAM_BYTES / GIB
            ),
        };
    };

    // Candidates: this band and below, largest first.
    let mut candidates: Vec<&ModelEntry> =
        catalog.models.iter().filter(|m| m.band <= band).collect();
    candidates.sort_by_key(|m| std::cmp::Reverse((m.ram_floor_bytes, m.size_bytes)));

    for model in candidates {
        if model.fits(profile) {
            return TierDecision::Selected {
                model: model.name.clone(),
                band,
                pinned: false,
            };
        }
    }

    TierDecision::Disabled {
        reason: "no catalog model fits available RAM and disk; running remote-only".to_owned(),
    }
}

/// [`decide`], but also emits a `Probed` [`LifecycleEvent`] describing the
/// result — the first-run probe surface for the `model_lifecycle` stream (BR-9).
pub fn probe(
    profile: &HardwareProfile,
    catalog: &Catalog,
    pinned: Option<&str>,
    on_event: &mut dyn FnMut(LifecycleEvent),
) -> TierDecision {
    let decision = decide(profile, catalog, pinned);
    on_event(LifecycleEvent::Probed {
        model_id: decision.model().unwrap_or("none").to_owned(),
        ram_bytes: profile.ram_bytes,
        above_floor: band_for_ram(profile.ram_bytes).is_some(),
    });
    decision
}

// ---------------------------------------------------------------------------
// Platform detection (not the tested surface; best-effort, degrades to Err).
// ---------------------------------------------------------------------------

#[cfg(target_os = "macos")]
fn detect_ram_bytes() -> std::io::Result<u64> {
    let out = std::process::Command::new("sysctl")
        .args(["-n", "hw.memsize"])
        .output()?;
    parse_first_u64(&String::from_utf8_lossy(&out.stdout)).ok_or_else(|| {
        std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            "could not parse hw.memsize",
        )
    })
}

#[cfg(target_os = "linux")]
fn detect_ram_bytes() -> std::io::Result<u64> {
    let meminfo = std::fs::read_to_string("/proc/meminfo")?;
    for line in meminfo.lines() {
        if let Some(rest) = line.strip_prefix("MemTotal:") {
            if let Some(kb) = parse_first_u64(rest) {
                return Ok(kb.saturating_mul(1024));
            }
        }
    }
    Err(std::io::Error::new(
        std::io::ErrorKind::InvalidData,
        "MemTotal not found in /proc/meminfo",
    ))
}

#[cfg(not(any(target_os = "macos", target_os = "linux")))]
fn detect_ram_bytes() -> std::io::Result<u64> {
    Err(std::io::Error::new(
        std::io::ErrorKind::Unsupported,
        "RAM detection is not implemented on this platform",
    ))
}

/// Parse free disk bytes for the filesystem holding `path`, via `df -k`.
fn detect_free_disk_bytes(path: &str) -> std::io::Result<u64> {
    let out = std::process::Command::new("df")
        .args(["-k", path])
        .output()?;
    let text = String::from_utf8_lossy(&out.stdout);
    // The data row is the last non-empty line; "Available" is the 4th column of
    // the standard `df -k` layout (Filesystem, 1K-blocks, Used, Available, ...).
    let last = text
        .lines()
        .rfind(|l| !l.trim().is_empty())
        .unwrap_or_default();
    let available_kb = last
        .split_whitespace()
        .nth(3)
        .and_then(|s| s.parse::<u64>().ok())
        .ok_or_else(|| {
            std::io::Error::new(std::io::ErrorKind::InvalidData, "could not parse df output")
        })?;
    Ok(available_kb.saturating_mul(1024))
}

fn detect_gpu_class() -> GpuClass {
    if cfg!(all(target_os = "macos", target_arch = "aarch64")) {
        GpuClass::AppleSilicon
    } else {
        // CUDA detection would require probing the driver; default to CPU and let
        // the daemon override if it detects a GPU by other means.
        GpuClass::Cpu
    }
}

/// Parse the first whitespace-delimited unsigned integer out of `s`.
#[cfg(any(target_os = "macos", target_os = "linux"))]
fn parse_first_u64(s: &str) -> Option<u64> {
    s.split_whitespace().find_map(|t| t.parse::<u64>().ok())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::catalog::Catalog;

    fn profile(ram_gib: u64, disk_gb: u64) -> HardwareProfile {
        HardwareProfile {
            ram_bytes: ram_gib * GIB,
            free_disk_bytes: disk_gb * 1_000_000_000,
            gpu: GpuClass::AppleSilicon,
        }
    }

    #[test]
    fn band_boundaries_follow_the_table() {
        assert_eq!(band_for_ram(4 * GIB), None);
        assert_eq!(band_for_ram(FLOOR_RAM_BYTES - 1), None);
        assert_eq!(band_for_ram(8 * GIB), Some(TierBand::Small));
        // AC-8: a 16 GiB machine is still in the small band.
        assert_eq!(band_for_ram(16 * GIB), Some(TierBand::Small));
        assert_eq!(band_for_ram(16 * GIB + 1), Some(TierBand::Mid));
        assert_eq!(band_for_ram(32 * GIB), Some(TierBand::Mid));
        assert_eq!(band_for_ram(32 * GIB + 1), Some(TierBand::Large));
    }

    #[test]
    fn probe_emits_probed_event_with_ram_and_floor() {
        let catalog = Catalog::bundled();
        let mut events = Vec::new();
        let decision = probe(&profile(16, 500), &catalog, None, &mut |e| events.push(e));
        assert_eq!(decision.model(), Some("qwen2.5-coder-3b"));
        assert_eq!(events.len(), 1);
        match &events[0] {
            LifecycleEvent::Probed {
                model_id,
                ram_bytes,
                above_floor,
            } => {
                assert_eq!(model_id, "qwen2.5-coder-3b");
                assert_eq!(*ram_bytes, 16 * GIB);
                assert!(*above_floor);
            }
            other => panic!("expected Probed, got {other:?}"),
        }
    }

    #[test]
    fn below_floor_probe_emits_disabled_marker() {
        let catalog = Catalog::bundled();
        let mut events = Vec::new();
        let decision = probe(&profile(4, 500), &catalog, None, &mut |e| events.push(e));
        assert!(decision.is_disabled());
        match &events[0] {
            LifecycleEvent::Probed {
                model_id,
                above_floor,
                ..
            } => {
                assert_eq!(model_id, "none");
                assert!(!above_floor);
            }
            other => panic!("expected Probed, got {other:?}"),
        }
    }

    #[test]
    fn valid_pin_overrides_the_probe() {
        let catalog = Catalog::bundled();
        // A 16 GiB machine would pick the 3b; a pin forces the 7b.
        let decision = decide(&profile(16, 500), &catalog, Some("qwen2.5-coder-7b"));
        assert_eq!(
            decision,
            TierDecision::Selected {
                model: "qwen2.5-coder-7b".to_owned(),
                band: TierBand::Mid,
                pinned: true,
            }
        );
    }

    #[test]
    fn unknown_pin_falls_back_to_the_probe() {
        let catalog = Catalog::bundled();
        let decision = decide(&profile(16, 500), &catalog, Some("does-not-exist"));
        assert_eq!(decision.model(), Some("qwen2.5-coder-3b"));
        assert!(matches!(
            decision,
            TierDecision::Selected { pinned: false, .. }
        ));
    }

    #[test]
    fn detect_does_not_panic_and_is_self_consistent() {
        // Live detection is environment-dependent; we only assert it never
        // panics and that a successful result is internally consistent. This
        // keeps the test deterministic regardless of the host or sandbox.
        if let Ok(p) = HardwareProfile::detect() {
            assert!(p.ram_bytes > 0);
        }
    }
}
